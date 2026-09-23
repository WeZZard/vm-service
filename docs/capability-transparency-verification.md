# Stock-Tart console implementation and verification

## Current status

- **The replacement is implemented in working source in vm-service and mcp-vm-relay. It is not yet deployed or live-accepted.** No Tart source, binary, private framework layout, or stock runtime command was patched or replaced.
- New source includes acquisition discovery/reporting, guest console preparation, exact lease/console/attempt operations, an independently supervised connection worker, persistent viewer configuration, installer validation, and the matching relay consumer.
- The ordinary Tart boot remains `run <vm> --no-graphics`. VNC comes from the existing guest desktop through SSH, not Tart's experimental host-side server.
- No production installation, service restart, image provisioning, guest firewall change, new VM acquisition, or real viewer launch was performed during this implementation. Earlier permission and results for the rejected experiment are not substituted for replacement acceptance.

## Implemented source

- `bin/acquisition_options.py` validates options and constructs resolved-configuration and read-only discovery responses. Existing resource defaults and legacy response fields are preserved.
- `bin/console_sessions.py` owns fresh lease/console identities, guest preflight, idempotent attempts, cancellation, renewal publication, and restart refusal. It never recovers credentials or automatically replays a launch.
- `bin/console_worker.py` owns the loopback broker, SSH byte streams, standard viewer subprocess, and independent monotonic expiry. Controller EOF revokes streams without waiting for the guest-operation lock.
- `bin/guest-console-agent.py` checks the current console session and implements Linux x11vnc inetd sharing or a macOS Screen Sharing loopback connector. Its configuration arrives on SSH stdin before binary RFB; the password never enters the SSH command arguments.
- Linux uses a fresh eight-character VNC password per viewing attempt. The server receives it through a private guest file with x11vnc's removal option, and TurboVNC receives it through stdin. The guest adapter also cleans up on ordinary failure/EOF; physical secret erasure after an abrupt system failure is not claimed.
- macOS uses Apple's standard guest-account authentication prompt. The service does not inject the account password. Its report explicitly leaves viewer-selected session binding unverified and does not claim server-enforced view-only access.
- macOS network preflight requires a narrow PF policy: first top-level quick ingress blocking, no non-loopback skipped interfaces, no external existing VNC states, and no translation rules or translation-anchor references. External reachability remains a separate live acceptance check.
- `bin/console_config.py` loads trusted persistent configuration at startup. `bin/install-vm-service.sh` supports check/dry-run modes, preserves installed environment settings, refuses retained leases, and verifies old daemon shutdown before replacing the LaunchAgent.
- The sibling mcp-vm-relay source adds optional VNC acquisition, acquisition-capability discovery, explicit console operations, durable attempt tracking, and nested-response parsing. Its distribution bundles were rebuilt, but the installed extension was not reloaded.

## Automated verification

- The final vm-service unit run reported **270 tests**, with **one optional collector skip** and no failures. The integration run reported **51 tests** without failures. Local logs are `/tmp/stock-console-unit-final.log` and `/tmp/stock-console-integration-final.log`.
- The unit suite includes actual worker processes, sockets, and synthetic guest/viewer processes. Those tests prove local control/stream mechanics, not real SSH authentication, real VNC authentication, guest pixels, or a visible viewer.
- HTTP/CLI tests run a real fixture HTTP server while Tart and guest operations remain fake. They verify inert discovery, fresh identity, unavailable-before-allocation behavior, stale/unknown-field rejection, duplicate cancellation, and non-VNC compatibility.
- Installer tests use temporary homes, fake Tart, and launchctl stubs. They do not install the service or demonstrate real runtime compatibility.
- The relay owner reported a passing build, and the parent independently reran typecheck and the full relay suite: **312 runner-reported tests passed**, without skips or failures. One earlier unchanged owner-lock test timed out; its log was retained, and focused plus full reruns passed. Evidence is in the sibling repository under `test-evidence/console-consumer/nested-report-*` and in `/tmp/relay-stock-console-typecheck-final.log` and `/tmp/relay-stock-console-test-final.log`.
- Relay fixtures are generated from calls to the actual backend report methods, not from a separately invented flat response schema. Tests preserve nested attempt state, macOS limitations, and uncertain outcomes across restoration.
- Existing integration resource warnings remain visible. No live-test result is inferred from these automated results.

## Review corrections

- Review caught a guest hard-deadline mismatch. Controller, worker, and guest now share a 720-hour hard ceiling, while the worker's separately renewable monotonic grant controls the actual lease access lifetime.
- A failed heartbeat state write previously could publish a longer console deadline. Publication now occurs only after persistence under the same transaction locks, with a regression test for failed storage.
- Explicit VNC release revokes access before waiting for a long guest operation. Ordinary non-VNC release retains the old lock/renewal behavior. If release-state persistence fails after revocation, access stays closed and the persisted lease remains for explicit cleanup.
- PF review identified that translation `pass` rules can bypass filter rules. The guest now rejects all translation rules and anchor references instead of claiming arbitrary policy is safe.
- Installation review corrected Apple's current application path and required verified unload/process/ownership termination rather than ignoring launchctl failures.
- Transport is reported only after a valid RFB banner, not SSH subprocess creation. Review then identified Apple's `RFB 003.889` extension; the worker now accepts and forwards it unchanged only for the Apple backend, with a fragmented-banner regression.
- Manual macOS display selection remains an explicit unverified condition. Neither a successful preflight nor an RFB banner is represented as successful authentication, same-session pixels, or human confirmation.

## Deployment and live acceptance still required

- [Installation](vnc-installation.md) and [guest preparation](guest-console-provisioning.md) describe persistent configuration and image prerequisites. They must be exercised on the actual target environment rather than treated as completed provisioning.
- The previous stock Tart 2.32.1 headless stall on the tested prerelease host remains a compatibility finding. This implementation does not repair Tart. A supported unmodified upstream release/host combination must boot successfully before viewing can be accepted.
- Linux and macOS both require real same-console, authentication, network-isolation, cancellation, expiry, service-loss, and cleanup checks. The newly built relay must drive the normal installed viewer path.
- [The acceptance specification](vnc-acceptance.md) requires viewer-side screenshots, independent randomized visual comparisons, and agreement with relay capture. No replacement screenshot or vLLM pass has been obtained yet.
- The implementation checks above preceded the separately requested commit and service restart. A restart without console configuration does not enable viewing. Extension reload, guest preparation, and live acceptance are separate from source tests and service health.

## Historical boundary

- The old task commits `3a2414d`, `11761f7`, and `431c2ee` were removed from `main` through the earlier documentation-only history rewrite. No revert commit or retained published backup branch was used.
- The new implementation was written against the restored baseline and revised design; it does not reuse or reinstate the rejected Tart patch.
- [The historical two-VM report](two-vm-vnc-acceptance.md) records a genuine but rejected-runtime experiment. It remains historical evidence only, not proof of this replacement or the normal upgrade workflow.
- [The historical archive](history/README.md) contains withdrawn documentation. Its commands and test counts are not current installation instructions or replacement acceptance.
