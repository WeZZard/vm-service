# Console contract for unmodified Tart

## Status

- **This contract is implemented in working source, not yet deployed or live-accepted.** It is a new guest-sharing implementation, not restoration of the rejected Tart patch.
- The replacement uses unmodified Tart and guest-side console sharing. [The service design](design.md) explains the platform prerequisites and unresolved live macOS and stock-runtime checks.
- No private Tart command, inherited Tart control socket, framework layout, or patched executable is part of this contract. The inherited socket used by the new connection worker is between vm-service and its own Python helper, not Tart.

## Supported location and trust

- The first viewing location is the service host's logged-in graphical session. The service must report this explicitly so a remote relay client does not mistake it for a viewer on its own computer.
- A different viewing host requires its own explicit authenticated transport and launcher design. The API must not improvise a tunnel or expose a raw VNC endpoint to satisfy that request.
- vm-service selects trusted installed adapters and a standard viewer. API clients cannot submit executable paths, shell commands, arbitrary guest ports, or connection URLs.
- The existing household-trust model is unchanged. Lease identifiers and environment fingerprints are stale-request safeguards, not a general API authentication system.

## Discovery and acquisition

- The proposed capability response will identify the stock runtime, image sharing backend, supported desktop-session type, viewer location, and required authentication interaction. Each observation must state whether it is verified, configured only, unavailable, or unknown.
- Discovery reads startup metadata without creating a lease, credential, connection, or viewer. Live image and guest readiness are checked during acquisition rather than inferred from discovery.
- A fresh acquisition with `vnc: true` must use normal stock-Tart boot and existing SSH/transfer readiness before preparing guest sharing. A request with `vnc: false` must leave the ordinary path unchanged.
- Acquisition will create an immutable `lease_id` and a distinct `console_id` when sharing is prepared. A `console_id` identifies the sharing session, not a patched Tart runtime or a guessed guest address.
- Preparation must bind the guest identity, console user, graphical session, selected environment, and lease to the connection adapter. An unexpected display or new desktop is failure, not a fallback.
- Acquisition must report sharing readiness separately from viewer launch, authentication, pixels, and human confirmation. It must not open a viewer automatically.
- Preparation must wait, within a bound, for guest state that is still arriving, and must fail at once on state the guest cannot reach. Key-only SSH readiness does not imply a graphical session: a clone answers commands while its display manager is still logging the console user in. The adapter's own diagnostic codes divide into pending and standing; a pending code is retried until the bound elapses, a standing one is reported immediately, and the reported reason always names the code. Only a code this service recognizes may be named, so an unexpected adapter cannot inject text into a message.
- Preparation failure invokes normal rollback. If destruction or sharing cleanup cannot be confirmed, the service retains ownership and reports the remaining resources.

## Operations

### Resolve

```http
POST /vms/<vm>/console/resolve
Content-Type: application/json

{"lease_id":"<lease_id>"}
```

- Resolution returns the exact lease/environment identity, `console_id`, backend, readiness status, observation source/time, access expiry, and viewer location.
- Resolution does not open a connection, refresh a grant, reconstruct credentials, or revive a lost sharing session.
- A legacy or non-VNC lease returns an explicit unavailable result. It is not restarted, replaced, or retrofitted automatically.
- The response contains no passwords, guest-login secrets, reusable connection URLs, private key paths, or broker ports.

### Open

```http
POST /vms/<vm>/console/open
Content-Type: application/json

{"lease_id":"<lease_id>","console_id":"<console_id>","attempt_id":"watch-1"}
```

- Opening requires an applicable user request and may create a standard viewer window on the declared host. The service checks the lease, environment, console session, and deadline again immediately before dispatch.
- The service records `attempt_id` before dispatch. Repeating an identifier returns its existing result and must not launch another viewer, even after an uncertain response.
- One active attempt per console is sufficient for the initial scope. Limits and request validation must be explicit and covered by tests.
- The service owns the broker, fixed guest adapter, SSH process, and bounded access deadline. It uses the lease's existing SSH key without exposing it to relay.
- Opening reports process launch and transport separately. If a standard viewer needs interactive authentication or macOS consent, the result reports that pending step rather than claiming a displayed desktop.
- A failed or closed viewer does not destroy the VM. A new attempt requires an explicit request after the previous attempt is reconciled.

### Cancel

```http
POST /vms/<vm>/console/cancel
Content-Type: application/json

{"lease_id":"<lease_id>","console_id":"<console_id>","attempt_id":"watch-1"}
```

- Cancellation closes the managed listener and all established streams, then cleans up the SSH adapter and viewer process. It does not release the VM or terminate an unrelated guest command.
- Cancellation before dispatch records a terminal attempt so a delayed open cannot launch it later. Cancellation racing a launch must clean up the late result instead of overwriting the cancelled state.
- A closed window must leave the guest running and available for further agent work. A later explicit viewing attempt may reconnect while the lease remains valid.

## Guest transport and credential boundaries

- The preferred Linux adapter uses a same-session VNC server in inetd mode over SSH, avoiding a guest VNC TCP listener. Its console selection and process lifetime still need verification.
- A built-in guest server, such as macOS Screen Sharing, may require a fixed adapter to its local socket. The service must verify the endpoint and its independent network restrictions rather than assume the SSH stream makes a wildcard server private.
- Ordinary SSH helpers retain their no-forwarding settings. A console-specific path must preserve the lease key policy, known-host identity checks, selected environment, and control-only networking constraints. It must not relax global SSH configuration.
- The local broker must bind only to loopback, have a bounded lifetime, and prevent an unrelated local connection from silently taking over the viewing attempt. RFB authentication or a verified viewer-owned stream is required; an unpredictable port alone is not authentication.
- Secret delivery must use a supported private viewer interface or the standard viewer's explicit authentication UI. The implementation must not use a password in a VNC URL, argv, ordinary environment variable, debug dump, or model response.
- If a backend needs private files or OS credential storage, its permissions, transfer-route exclusions, crash cleanup, and secret lifecycle must be reviewed. The design does not authorize undocumented manipulation of macOS Screen Sharing credentials.

## Expiry, service loss, and cleanup

- The connection worker enforces a monotonic lease deadline independently of guest execution and the VM destruction grace period. Unit tests cover expiry and controller loss; real guest connection expiry still requires live acceptance.
- Heartbeat grants are published only after the persistent state update succeeds, while still serialized against release. A failed release-state write may already have revoked console access; this is intentionally fail-closed, and the lease remains available for an explicit cleanup retry.
- The guest stream also has a 720-hour hard lifetime ceiling with bounded clock-skew validation. It is not the renewable lease authority; the host worker enforces the current shorter grant. Renewing a VM for more than that one stream lifetime does not silently extend the guest stream.
- The service must grant a bounded deadline to an independently supervised connection worker. Renewal must use ordered grants so a delayed older update cannot extend or restore expired access.
- Expiry and cancellation must not acquire locks held by guest execution, state persistence, viewer launch, or a blocked SSH operation before closing streams.
- Controller loss, broker failure, and service restart must close managed connections and stop any attempt-specific guest sharing process. An SSH process is not assumed to terminate just because its parent died.
- Restart must not replay a viewer launch or silently recover stale credentials. An uncertain attempt remains explicitly uncertain until reconciled.
- Stopping the local broker is not proof that a built-in guest server has no other clients. Each backend must separately prove its exposure and revocation boundary; otherwise it cannot claim access was fully revoked.
- Release closes console access before waiting for the ordinary VM-operation lock or deletion. Viewer exit, transport closure, guest-server cleanup, and VM destruction have distinct results and retry ownership.

## Evidence and acceptance

- Every public observation identifies the lease, console, attempt when applicable, source, and time. Unknown observations remain unknown.
- Responses contain top-level console `status` and, when present, a nested `attempt` with its own `attempt_id`, `status`, `transport_connected`, `viewer_cleanup`, and authentication status. A ready console does not mean its latest attempt is still connected.
- `readiness_scope` is `guest-console-preflight`. macOS reports `session_binding: "viewer-selection-unverified"` and `server_enforced_view_only: false`; checking the console account does not determine which display the human selected in Apple's viewer.
- `authentication_mechanism` identifies private stdin or a human guest-account prompt. `authentication`, `pixels`, and `human_confirmation` remain unverified unless a separate source actually observes them; an RFB banner does not upgrade those fields.
- `transport_connected` becomes true only after the worker reads a valid RFB server banner, not merely after starting SSH. It is still not authentication or framebuffer evidence.
- `viewer_cleanup: "local-children-stopped"` refers only to owned local child processes. It does not prove Apple's GUI app or the guest's built-in Screen Sharing daemon exited. Pending launch cleanup remains owned by the worker until late results can be reaped.
- Process launch, a connected TCP stream, successful RFB authentication, rendered pixels, model image evaluation, and human confirmation are different facts.
- The installed relay and normal standard viewer must show two unpredictable source-guest patterns, and actual viewer-side screenshots must match the independent expected manifests.
- [The acceptance specification](vnc-acceptance.md) defines the required deployment, security, lifecycle, and platform checks. The historical patched-runtime run does not satisfy this contract.
