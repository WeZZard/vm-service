# Lease lifecycle fixes

## Status

- This note records the decisions and designs for six lease lifecycle failures found in the Rust daemon.
- Each failure has an automated reproducer that was committed before its fix. Items V1 to V5 are fixed in source. Item V6 is documented by an ignored test and waits for the owner's decision.
- The fixes are source changes only. They do not restart, upgrade, or reconfigure the installed daemon.

## V4: bounded execution timeout

- **Failure:** `POST /vms/<vm>/exec` rejected only nonpositive timeouts. The per-VM operation lock is held for the whole SSH call, and release and the single serial GC loop wait on that lock. A very large timeout therefore blocked reclamation of every other lease for as long as the command ran. A timeout near `i64::MAX` also panicked in the subprocess helper after `ssh` had been spawned, because the deadline `Instant` overflowed.
- **Decision:** The daemon rejects a timeout above 4200 seconds with the error `timeout must not exceed 4200 seconds`, before any SSH process starts.
- **Rationale:** mcp-vm-relay accepts guest commands of up to 3,600,000 ms, adds a 180,000 ms receiver allowance, and allows an after-snapshot delay of up to 300,000 ms. Its largest request is therefore `ceil(4,080,000 / 1000) = 4080` seconds. A maximum of 4200 seconds keeps a 120-second margin above that value.
- **Decision:** The subprocess helper computes its deadline with checked arithmetic. A timeout that cannot be represented as a deadline is refused before the child is spawned.
- **Limitation:** A command can still hold its lease's operation lock, and therefore the GC loop, for up to 4200 seconds. The bound limits the delay; it does not make GC independent of guest commands.

## V5: a heartbeat without `ttl_hours`

- **Failure:** A heartbeat without `ttl_hours` cleared the grace deadline and the warning flag but kept the expired TTL. The next GC pass started a new 6-hour grace period, so repeated bare heartbeats kept an expired VM forever. `vmctl heartbeat <vm>` sends no `ttl_hours` unless `--ttl-hours` is given, although its help text says "reset TTL".
- **Decision:** A heartbeat without `ttl_hours` renews the lease for its own initial TTL, which is `configuration.effective.initial_ttl_hours`. A legacy record without that field uses the acquisition default of 24 hours.
- **Rationale:** This behavior matches the `vmctl` help text and the user story "Heartbeats reset both TTL and grace state". The recorded initial TTL is not rewritten, which preserves the acquisition contract.
- A heartbeat with `ttl_hours` behaves as before.

## V2: a running record whose VM is gone

- **Failure:** Heartbeat and GC never checked whether Tart still ran the VM of a `running` record. A VM that crashed or was stopped outside the service kept its record, its macOS slot, and its successful heartbeats until TTL plus grace.
- **Decision:** A heartbeat on a `running` record whose VM Tart does not report as running fails with the error `<vm> is not running on the host (Tart reports it stopped or absent); release it and acquire a new lease`. The TTL is not renewed.
- **Decision:** If `tart list` itself fails, the heartbeat proceeds and the failure is logged. A transient Tart failure must not make a client abandon a healthy lease.
- **Decision:** GC reads `tart list` once per pass. A `running` record whose VM is not running is counted per pass. After two consecutive passes, GC releases the lease through the normal teardown path with the reason `vm-not-running`. A pass that sees the VM running again resets the count.
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
- **Limitation:** Delivery failure can be observed only when the socket write fails. A peer that closed its connection normally may still accept the buffered response bytes, and in that case the lease is kept. TTL and GC remain the safeguard for that case.

## V6: grace period and macOS capacity (owner decision pending)

- **Current behavior:** After its TTL expires, a lease stays in its active state for a 6-hour grace period (`GRACE_HOURS`). During that period it still counts toward `MAX_MACOS_RUNNING`. Two expired macOS leases in grace therefore refuse every new macOS acquisition for up to 6 hours.
- **Status:** The ignored test `grace_leases_hold_macos_slots_for_six_hours` documents this behavior. No behavior change is made until the owner decides whether grace leases should keep their slots and how long grace should last.

## Logging

- The service log records heartbeats, release requests, refused acquisitions, startup reconciliation, liveness observations, and undelivered acquisition responses.
- A refused acquisition caused by the macOS limit logs the VMs that hold the macOS slots, with their states. The HTTP error text is unchanged, so clients that parse it are not affected.
- Log lines use the existing `service.log` format and prefixes (`WARN:`, `GC:`, `LEASE WARN:`).
