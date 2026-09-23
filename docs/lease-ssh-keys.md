# Per-lease SSH keys

## Contract

Each new VM lease owns a fresh Ed25519 SSH keypair generated with OpenSSH
`ssh-keygen`. The public key is installed in the disposable guest account;
the private key never leaves the host. No keys or application credentials are
baked into the golden image. The existing HTTP execution/transfer endpoints
remain the interface used by agents; no new guest daemon or SSH framework is
introduced.

Implementation: the `lease-keys` crate contains the provisioning and
file-permission helpers; the `vm-service` daemon invokes native `ssh` and `scp`
using the lease identity.
There is deliberately no automatic replay of arbitrary guest commands.

## Provisioning and readiness

1. Reserve a lease record with `ssh_auth: lease-key`, `ssh_verified: false`,
   and a snapshot of the image's guest account name.
2. Generate private host key material in `STATE_DIR/ssh/<vm>/` (directory 0700,
   private key 0600), then clone and boot the VM normally.
3. Read the bootstrap account configuration through the existing
   `pilot-images/lines/<image>/line.conf` discovery (`GUEST_USER`, `GUEST_PASS`).
   These are distinct from optional application env packs.
4. Use native OpenSSH `SSH_ASKPASS` for the initial password authentication.
   The password is supplied only in the bootstrap child environment, never
   command-line arguments, the key directory, the lease JSON, or logs. No
   `sshpass` pseudo-terminal is involved. The helper is removed after bootstrap.
5. Idempotently install only this public key in the guest user's
   `~/.ssh/authorized_keys`, preserving unrelated keys and enforcing permissions.
   Bootstrap retries are bounded and reuse the same key; they never include
   application execution or input.
6. Verify a new **key-only** command connection, then independent upload and
   download connections with a random payload and SHA-256 comparison. Delete
   the probe. Optional application env-pack injection also uses the lease key.
7. Only now publish `state: running`, `ssh_verified: true`.

`wait: false` remains accepted but does not bypass authentication or readiness.
It uses a shorter bootstrap deadline and one key-only command probe. This is a
behavioral tightening: an unverified guest is never returned as ready.
`env: none` disables application secrets, not mandatory lease SSH credentials.

## Normal operations

All command/upload/download processes explicitly use the recorded guest account
and lease private key. OpenSSH arguments disable user SSH configuration,
ssh-agent identity selection, password/keyboard-interactive fallback, interactive
prompts and shared control sockets. A new connection authenticates using the
same lease key; no connection pool or multiplexing dependency is required.

Unexpected loss after command submission may still mean **uncertain execution**.
The existing `rc`/`output` response is retained for compatibility; a returned 255
is not a promise that the guest command did not run. Consumers must not replay
non-idempotent operations based on that code. This change hardens authentication,
not exactly-once command delivery or the entire service error protocol.

## Guest host identity

Bootstrap uses **trust on first use** (`accept-new`) into a fresh per-lease
`known_hosts`. Subsequent command and transfer connections require strict matching
against that file, independent of the human's global known_hosts. A changed host
key is not silently accepted. The trust assumption is the local service-owned
Tart/NAT bootstrap path; this is **not** out-of-band authenticated first contact.
Clones may inherit identical image host keys: this policy detects a changed key,
not a cryptographically unique VM identity. Per-instance host-key generation and
trusted fingerprint distribution are separate improvements, not claimed here.

## Recovery and teardown

Per-VM operations are serialized within the daemon, including provisioning and
release; heartbeat/state observation do not wait behind an executing command.
A kernel-held `daemon.lock` enforces one daemon per state directory. Persisted keys and recorded
account identity allow normal operations after a daemon restart; missing or
unsafe credentials fail closed instead of silently regenerating a different key
or falling back to the bootstrap password. Public upload/download endpoints
reject local paths overlapping the private SSH tree (including aliases and
ancestors); recursive transfers and env packs reject symlinks rather than follow
them into host credentials.

Release stops/deletes the clone and independently verifies absence from Tart
before removing private credentials and the lease record. Failed destruction
retains the `releasing` record and key material for retry, still counts against
capacity, and is retried by GC. GC rechecks current TTL/grace atomically after waiting
for a guest operation, so an intervening heartbeat cancels stale reclamation.
Once teardown commits, heartbeat refuses renewal instead of falsely claiming
it saved a releasing VM. Failed acquisition follows the same cleanup rule. Never treat an HTTP release acknowledgement as success before destruction
and key cleanup both complete.

## Rollout

Development/live verification must use an isolated candidate daemon and state
directory. Do not restart the production daemon while other agents own leases.
Legacy pre-key records remain listable, heartbeat-capable and releasable, but
new code refuses their command/transfer operations with an explicit
release/reacquire instruction; it never silently mutates their SSH credentials.
Drain legacy leases before switching the shared daemon to this implementation.

## Verification

Run the offline suites from the repository root:

```sh
python3 -m unittest discover -s tests/unit -v
python3 -m unittest discover -s tests/integration -v
```

Current offline gate: **99 unit tests and 35 integration tests pass** under both
system Python 3.9.6 and the launchd installation's Homebrew Python 3.14.7. Real local `ssh-keygen` plus `ssh -G` also verifies the
produced identity and key-only/strict-host-checking options.

Key-helper tests exercise real local key generation and controlled subprocess
fixtures; lifecycle/HTTP tests substitute the guest transport and are not live
SSH proof. Fresh Ubuntu and macOS key-authentication acceptance **passed** using an
isolated candidate on loopback port 16240. Frozen `vm-service` and `lease_keys.py`
bytes match the implementation after the GC fix. The GC race itself is covered
by offline concurrency tests, not claimed as a live scenario.

Evidence: [`test-evidence/lease-key-live-passed/REPORT.md`](../test-evidence/lease-key-live-passed/REPORT.md)
(host-local gitignored portable package, copied and independently checksum-verified).
Both images verified key-only readiness, 20 independent exec/binary-transfer
cycles, a remote exit-255 command with exactly one counter increment, wrong/missing
identity and host-pin rejection, restored access, restart with unchanged public
fingerprint, and verified VM/key/registry deletion. There are 48 verified binary
roundtrips total (count of `binary-roundtrip` events; 24 per image including
restoration/restart). Integrity formula: SHA256(original bytes) ==
SHA256(downloaded bytes), plus byte equality. No throughput claim.

The wrong-key fixture left the original `.pub` file in place: it establishes
client rejection/no password fallback, not that an unauthorized foreign key
reached server-side matching. Secret scans are heuristic when the configured
password equals public account metadata; the report documents exclusions.

The first attempt is retained unchanged in `test-evidence/lease-key-live-attempt1/`:
its harness falsely matched a bootstrap-password substring in OpenSSH's fixed
"system administrator" advisory. Its VM and keys were cleaned before rerunning.
No production daemon or existing lease was touched. The candidate port is closed,
private scratch deleted, registry rows removed, and independent host Tart checks
show no key-test clones remain. Execution and cleanup passed; human review is
pending. Deployment to the shared service remains separate and requires draining
legacy leases as described above.
