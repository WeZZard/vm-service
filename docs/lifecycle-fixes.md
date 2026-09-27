# Lease lifecycle fixes

## Status

- This note records the decisions and designs for six lease lifecycle failures found in the Rust daemon.
- Each failure has an automated reproducer that was committed before its fix. Items V1 to V5 are fixed in source. Item V6 is documented by an ignored test and waits for the owner's decision.
- The first fix for V4, an execution timeout cap, was replaced by the owner's decision of 2026-09-27: release preempts a running guest operation, GC never blocks on one VM, and timeouts have no maximum.
- The fixes are source changes only. They do not restart, upgrade, or reconfigure the installed daemon.

## V4: a long guest operation blocks release and GC

- **Failure:** One per-VM operation lock serializes guest operations. `POST /vms/<vm>/exec` holds it for the whole SSH call, and push and pull hold it for the whole `scp` call. Release took the same lock before tearing the VM down, and the single GC thread released leases one after another. A long command therefore blocked release of its own VM and, through GC, reclamation of every other lease for as long as it ran. A timeout near `i64::MAX` also panicked in the subprocess helper after `ssh` had been spawned, because the deadline `Instant` overflowed.
- **Superseded fix:** Commit `5401dc9` rejected a timeout above 4200 seconds. The owner did not approve the cap and treats the failure as an ordering problem. The cap is removed, and a request may again use any positive timeout.
- **Decision (owner, 2026-09-27):** Guest execution timeouts have no maximum.
- **Decision (owner, 2026-09-27):** A release preempts a running guest operation on its VM.
- **Decision (owner, 2026-09-27):** GC never blocks on one VM.
- **Decision:** The subprocess helper still computes its deadline with checked arithmetic. A timeout that cannot be represented as a deadline is refused before the child is spawned, so the panic does not return.

### Design: release preempts the operation lock

- The per-VM operation lock remains the queue for guest operations on one VM. Exec, push, and pull still run one at a time.
- The service keeps a per-VM cancellation flag beside the operation lock.
- Release first commits the `releasing` state under the state lock. This step is unchanged, and it still honors the expired-only check that GC uses, so a lease renewed before reclamation is not cancelled.
- After the commit succeeds, release sets the VM's cancellation flag, and only then takes the operation lock.
- Exec, push, and pull run their subprocesses inside a cancellation scope for their VM. The subprocess helper checks the flag before it spawns the child and on every 20-millisecond poll. When the flag is set, it kills the child in the same way as at the deadline. The operation lock is therefore free within milliseconds, and teardown proceeds.
- A cancelled exec returns the error `command cancelled by release of <vm>`. A cancelled push or pull returns the error `transfer cancelled by release of <vm>`. The partial output of a cancelled command is discarded.
- No new operation can start after the commit, because exec, push, and pull refuse a record that is not `running`.
- Release no longer takes the operation lock before it commits `releasing`. Heartbeat and the expiry check both run under the state lock, so the commit remains atomic with respect to renewal.
- The flag is never cleared. Each lease has a unique VM name, and a record in the `releasing` state refuses every new operation.

```mermaid
sequenceDiagram
    participant E as exec thread
    participant R as release thread
    participant S as state lock
    participant L as VM operation lock
    E->>L: lock
    E->>E: ssh (polls the flag every 20 ms)
    R->>S: commit state = releasing
    R->>R: set the cancellation flag
    R->>L: lock (waits)
    E->>E: kill ssh on the flag
    E-->>L: unlock; return "command cancelled by release"
    L-->>R: granted
    R->>R: stop and delete the VM
```

### Design: GC does not wait on a busy VM

- For each lease it reclaims, GC commits `releasing` and sets the cancellation flag in the same way as a release.
- GC then tries to take the operation lock without waiting. If another thread holds the lock, GC logs `GC: <vm> is busy with another operation; teardown deferred to the next pass` and continues with the next lease.
- A deferred lease keeps its `releasing` record. The next GC pass reclaims every `releasing` record, so a deferred lease is torn down on a later pass once its lock is free.
- A lock can stay held after a cancellation only by an operation that does not run through the cancellable subprocess helper, such as another thread's teardown or an acquisition that is still provisioning. GC skips that VM and reclaims the others.

### Limitations

- Cancellation kills the local `ssh` or `scp` process. The remote command can continue in the guest until teardown stops the VM, which follows immediately.
- Provisioning during acquisition does not observe the cancellation flag. A release during acquisition waits for provisioning to reach its next state check, as it did before this change. GC does not wait for it.
- Teardown itself (`tart stop` and `tart delete`) is not cancellable.
- The cancellation scope is bound to the thread that runs the operation. A `Host` implementation that runs its subprocess on another thread does not observe the flag. `RealHost` runs the subprocess on the calling thread.

## V5: a heartbeat without `ttl_hours`

- **Failure:** A heartbeat without `ttl_hours` cleared the grace deadline and the warning flag but kept the expired TTL. The next GC pass started a new 6-hour grace period, so repeated bare heartbeats kept an expired VM forever. `vmctl heartbeat <vm>` sends no `ttl_hours` unless `--ttl-hours` is given, although its help text says "reset TTL".
- **Decision:** A heartbeat without `ttl_hours` renews the lease for its own initial TTL, which is `configuration.effective.initial_ttl_hours`. A legacy record without that field uses the acquisition default of 24 hours.
- **Rationale:** This behavior matches the `vmctl` help text and the user story "Heartbeats reset both TTL and grace state". The recorded initial TTL is not rewritten, which preserves the acquisition contract.
- A heartbeat with `ttl_hours` behaves as before.

## V2: a running record whose VM is gone

- **Failure:** Heartbeat and GC never checked whether Tart still ran the VM of a `running` record. A VM that crashed or was stopped outside the service kept its record, its macOS slot, and its successful heartbeats until TTL plus grace.
- **Decision:** A heartbeat on a `running` record whose VM Tart does not report as running fails with the error `<vm> is not running on the host (Tart reports it stopped or absent); release it and acquire a new lease`. The TTL is not renewed.
- **Decision:** If `tart list` itself fails, the heartbeat proceeds and the failure is logged. A transient Tart failure must not make a client abandon a healthy lease.
- **Decision:** GC reads `tart list` once per pass, after every record has passed the selected-environment check, and only when at least one record is `running`. A `running` record whose VM is not running is counted per pass. After two consecutive passes, GC releases the lease through the normal teardown path with the reason `vm-not-running`. A pass that sees the VM running again resets the count.
- **Rationale:** Two passes (about 60 seconds apart) tolerate the race in which a record becomes `running` between the `tart list` snapshot and the state read. The count is held in memory, so the persisted state layout does not change. A daemon restart restarts the count, which delays reclamation by at most two passes.
- GC logs the first observation and the reclamation.

## V1: startup reconciliation

- **Failure:** The daemon started the GC loop and the request loop without examining existing records. A restart or crash during acquisition left a `pending` or `provisioning` record, a clone, and a running `tart run` process. These counted toward `MAX_MACOS_RUNNING` until TTL plus grace (30 hours by default), and a retry with the same purpose was refused as already leased.
- **Decision:** At startup, after the daemon lock is held and before the HTTP server binds, the daemon releases every `pending` and `provisioning` record through the normal teardown path with the reason `acquire-interrupted-by-restart`. It logs each record it reconciles.
- **Rationale:** The kernel-held `daemon.lock` guarantees that one daemon owns a state directory. After a restart, no acquisition from the previous process can still be in flight, so these records are orphans.
- A failed teardown keeps the record in the `releasing` state and the GC loop retries it, as for any other failed release.

## V3: an acquisition whose client has gone

- **Failure:** The daemon ignored the result of writing the HTTP response. When the client disconnected while acquisition was still running, the daemon created a lease that no client knew about. The lease held its slot until TTL plus grace.
- **Decision:** When the daemon cannot deliver a successful `POST /acquire` response, it releases the new lease with the reason `acquire-response-undelivered` and logs the event.
- **Design:** `tiny_http`'s `Request::respond` reports a reset or broken connection as success. The daemon therefore writes each response through the request's raw writer with the same serialization, and it receives the write error.
- **Limitation:** Delivery failure can be observed only when the socket write fails. A peer that closed its connection normally may still accept the buffered response bytes, and in that case the lease is kept. TTL and GC remain the safeguard for that case.

## V6: grace period and macOS capacity (owner decision pending)

- **Current behavior:** After its TTL expires, a lease stays in its active state for a 6-hour grace period (`GRACE_HOURS`). During that period it still counts toward `MAX_MACOS_RUNNING`. Two expired macOS leases in grace therefore refuse every new macOS acquisition for up to 6 hours.
- **Status:** The ignored test `grace_leases_hold_macos_slots_for_six_hours` documents this behavior. No behavior change is made until the owner decides whether grace leases should keep their slots and how long grace should last.

## Logging

- The service log records heartbeats, refused heartbeats, release requests, refused reservations, startup reconciliation, liveness observations, undelivered acquisition responses, guest operations cancelled by release, and GC teardowns deferred because a VM was busy.
- A reservation refused because the purpose is already leased or because the macOS limit is reached is logged. A refusal caused by the macOS limit also logs the VMs that hold the macOS slots, with their states and grace deadlines. Request validation errors are not logged.
- The HTTP error text is unchanged, so clients that parse it are not affected.
- Log lines use the existing `service.log` format and prefixes (`WARN:`, `GC:`, `LEASE WARN:`).
