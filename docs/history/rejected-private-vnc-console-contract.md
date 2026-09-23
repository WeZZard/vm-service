# Historical rejected private-VNC console contract

> This is the withdrawn implementation's contract, not a current API. Tart patching is prohibited, and the patch, routes, controller, and launcher described below have been removed. Commands and source references are historical only. See the [replacement proposal](../console-contract.md) and [archive index](README.md).

## Status and prerequisites

- The source now implements VNC acquisition, private runtime control, explicit viewer opening, cancellation, and access revocation. It does not enable stock Tart's password-bearing URL output.
- The runtime patch applies to Tart 2.32.1. Its current listener adapter supports only the inspected arm64 macOS build `26A428` and Virtualization framework UUID `418EFC48-09D9-3606-918E-B6F95BAEFFDC`. Other builds fail before allocation or server startup rather than falling back to an unrestricted listener.
- The implementation uses a private Apple framework layout. It is not an Apple-supported VNC API. A macOS update requires review of the new framework before enabling this adapter there.
- The full patched Tart executable has compiled. Automated tests use real private sockets and harmless subprocesses, not VMs or a displayed viewer. Live listener scope, viewer authentication, guest display correctness, and human visibility have not yet been accepted.
- No running service has been changed. Installation, isolated live acceptance, opening the desktop viewer, and production activation remain separate actions.
- The exact patch, build procedure, protocol, and implementation checks are in [the runtime patch notes](rejected-tart-patch.md).

## Responsibilities and supported location

- vm-service owns the VM, the inherited private control channel, and the viewer connection proxy. The password remains in process memory and travels to TurboVNC through stdin.
- The explicit opening operation launches the installed TurboVNC executable **on the service host**. It does not open a viewer on a remote API client's computer.
- This first integration therefore serves the local-host viewing workflow. A remote service can still answer discovery and lease requests, but this API is not a remote viewer or SSH-tunnel launcher.
- The operator configures an absolute trusted TurboVNC 3.1.3 executable path with `VM_SERVICE_VNC_VIEWER` when starting the service. The client cannot submit an executable path, shell command, password, or network address.
- The viewer launcher uses view-only mode and disables automatic reconnect. It does not use macOS `open` or start a custom native viewer.
- Viewer process launch, TCP connection, successful VNC authentication, displayed pixels, and human confirmation are separate facts. The API never turns a process-launch result into a viewing-success claim.

## Discovery and acquisition

- Startup runs the selected executable's read-only `vm-service-vnc-capabilities` command and retains its sanitized result. `GET /acquisition-capabilities` only reads that snapshot; it does not probe Tart on request.
- The `vnc` option's `enabled_support` is `supported` only when the selected runtime reports protocol 1, `available: true`, and `listener_scope: "loopback-ipv4"`. Otherwise it is `unavailable`.
- The descriptor also reports `viewer_configured` and `viewer_location: "service-host"`. A configured path is not proof that the viewer can display a window or authenticate.
- `POST /acquire` accepts `vnc: true`, and `vmctl acquire --vnc` sends it. The default remains false, and ordinary headless boot keeps its previous command.
- A VNC-enabled acquisition reserves a fresh lease, starts the patched runtime with an inherited anonymous socket, validates the private readiness response, and performs the existing SSH and transfer readiness checks.
- The runtime must echo the exact lease, controller-generated runtime identifier, and selected-environment fingerprint. The service rejects wrong identities, invalid ports, malformed frames, and unsupported scope reports.
- An unavailable runtime is rejected before allocation. Failure after cloning invokes the existing acquisition rollback, retaining the lease when VM cleanup cannot be proven.
- Acquisition prepares VNC but never opens the viewer. It does not retrofit, restart, or replace a non-VNC lease.

## Public operations

- Console routes return JSON and use the existing selected-environment fingerprint header policy. They retain the household-trust model; identifiers prevent stale operations but are not caller authentication.
- Every route requires the exact `lease_id`. Opening and cancellation also require `runtime_id` and a caller-generated `attempt_id`.
- Request bodies are closed schemas. Unknown fields, missing fields, and non-string identities are rejected with HTTP 409.

### Resolve console status

```http
POST /vms/<vm>/console/resolve
Content-Type: application/json

{"lease_id":"<lease_id>"}
```

```sh
vmctl console-resolve <vm> --lease-id <lease_id>
```

- Resolution does not create a viewer or connection grant. It returns non-secret status for the existing runtime.
- A response includes `schemaVersion`, `vm`, `lease_id`, `environment_fingerprint`, `runtime_id`, `status`, `reason`, `source`, `observed_at`, `access_expires_at`, `connection_cleanup`, and `runtime_cleanup`.
- `status` is `ready` or `revoked` for a resolved session. A missing controller or legacy/non-VNC lease returns a diagnostic instead of reconstructing a connection from persisted metadata.
- `viewer_connected` and `human_confirmation` remain `unverified`. No response contains the runtime port, proxy port, password, or connection URL.

### Open one viewer attempt

```http
POST /vms/<vm>/console/open
Content-Type: application/json

{"lease_id":"<lease_id>","runtime_id":"<runtime_id>","attempt_id":"watch-1"}
```

```sh
vmctl console-open <vm> --lease-id <lease_id> \
  --runtime-id <runtime_id> --attempt-id watch-1
```

- This is the operation that may open a desktop window. A client must obtain the user's applicable viewing request before calling it.
- Attempt identifiers contain 1 through 120 ASCII letters, digits, dots, underscores, or hyphens and start with a letter or digit.
- An attempt is recorded before process dispatch. Repeating the same identifier returns its current result; it never launches a second viewer.
- Only one attempt may be active per runtime. A new attempt requires the previous viewer to exit or be cancelled. At most 64 attempt identifiers are retained per lease.
- The response adds an `attempt` object with `attempt_id`, `status`, `transport_connected`, `viewer_cleanup`, `viewer_connected`, and `human_confirmation`.
- Attempt statuses distinguish preparation, dispatch, process launch, failure, cancellation, and closure. `transport_connected` reports only a currently open proxy TCP connection, not successful VNC authentication.
- Missing or failed viewer launch leaves the usable VM allocated. A caller can cancel, make an explicit new attempt, or release the lease.

### Cancel an attempt

```http
POST /vms/<vm>/console/cancel
Content-Type: application/json

{"lease_id":"<lease_id>","runtime_id":"<runtime_id>","attempt_id":"watch-1"}
```

```sh
vmctl console-cancel <vm> --lease-id <lease_id> \
  --runtime-id <runtime_id> --attempt-id watch-1
```

- Cancellation closes the attempt's listener and established proxy streams before requesting viewer-process termination. It does not release the VM or stop the agent's guest operation.
- Cancelling an identifier before it opens records a terminal cancellation. A delayed opening request using that identifier does not dispatch a viewer.
- If cancellation follows committed process dispatch, the proxy is closed immediately. A late process result is cleaned up rather than published as a new active viewer.
- Cancellation and failure records cannot be overwritten by a late successful-launch persistence write.
- `viewer_cleanup: "pending"` means process exit has not yet been observed. Cleanup remains owned and retried separately from access revocation.

## Lifetime and failure behavior

- Console access ends at lease TTL, independently of the existing six-hour VM destruction grace. This implementation does not shorten the VM's retention policy.
- Acquisition and heartbeat capture a monotonic deadline inside the same state transaction as the wall-clock expiry. The grant includes a controller identifier and revision; a later clock adjustment does not reconstruct or extend its duration.
- The controller sends short bounded deadlines to Tart. Runtime EOF or controller loss permanently revokes that private server session without stopping the VM.
- A separate local expiry worker closes tracked viewer connections without reading lease state or waiting for a guest operation. Runtime-control I/O and process launch do not own the lock needed for that closure.
- The runtime also checks its own deadline. Heartbeats cannot revive a runtime that has already revoked itself; a renewed VM lease can therefore remain usable for guest work while its console is unavailable.
- Explicit release commits VNC lease teardown and revokes connections before waiting for the per-VM guest-operation lock. Normal non-VNC release behavior remains unchanged.
- Closing the viewer disconnects only its attempt. It neither releases the VM nor stops the VNC runtime, so a later explicit attempt can open another viewer before expiry.
- A service restart does not recover credentials, reopen viewers, or replay uncertain launches. Persisted console metadata becomes revoked, and pending attempts become `uncertain-after-restart`. Existing VMs remain available through their ordinary lease operations.
- The runtime's stop selector is not treated as proof that every undocumented direct client disconnected. The service closes its own tracked streams and reports runtime cleanup as `requested-unconfirmed` unless a runtime acknowledgment was observed.
- VM deletion and connection cleanup remain separate results. Release adds a non-secret `console_cleanup` report when it owned a console session.

## Credential handling and verification limits

- The runtime never prints or constructs a private VNC URL. It returns credentials only through the inherited anonymous Unix socket, and the service suppresses runtime stdout and stderr for VNC-enabled boot.
- The viewer receives a password only through stdin. Its arguments, environment, temporary settings, service state, public errors, and reports do not contain that password.
- Viewer defaults use an isolated temporary home and explicit credential-free settings. This is not a macOS preferences sandbox and does not protect against privileged inspection, crash dumps, a modified executable, or manual user actions.
- The private control channel and live password do not use a filesystem path, so the guest transfer API cannot export them as files. Existing per-lease SSH material remains protected by its existing transfer checks.
- Source compilation and isolated protocol tests do not establish live listener scope, successful VNC authentication, display correctness on either guest family, or agreement with relay screenshots. Those checks still require isolated live acceptance and human confirmation.
