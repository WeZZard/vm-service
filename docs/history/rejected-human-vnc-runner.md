# Historical rejected human-assisted VNC runner

> The runner and its tests were discarded with the patched-Tart implementation. The commands below no longer refer to available files and are not instructions to restore the rejected approach. See the [replacement acceptance specification](../vnc-acceptance.md) and [archive index](README.md).

## Purpose and current status

- `vnc_live.py` tests the real service, patched Tart runtime, disposable guest, and installed TurboVNC viewer together. It does not substitute a fake runtime during live execution.
- The runner is implemented and has offline tests. Adding it does not mean that live VNC acceptance has passed; no live result is supplied by this change.
- The runner is separate from the production-default legacy `test_e2e.py`. It starts and owns a candidate daemon with a selected isolated environment and refuses production port 6240.
- Normal invocation prints a plan. Unittest discovery does not run the live scenario, and `VM_SERVICE_LIVE_E2E=1` does not authorize this viewer test.

## What the scenario checks

- It verifies that the candidate endpoint reports the expected environment fingerprint and private-runtime capability before allocating anything.
- It acquires one fresh VM with `vnc: true` and `env: "none"`, then validates the lease and runtime identities.
- It locates the owned Tart child process and inspects its real listening socket with `lsof`. A wildcard, IPv6, missing, or ambiguous listener fails the test. This observation is separate from the runtime's own scope declaration.
- It explicitly opens TurboVNC, observes the service's transport milestone, and independently observes the service-owned TCP connection to the runtime.
- It creates a random code inside the guest desktop and asks the human to transcribe it from TurboVNC. It then changes the code and asks again. The runner does not show the expected answers in its prompt or write them into the report.
- It asks the human to close the viewer window. It verifies viewer exit, connection closure, and continued guest execution.
- It opens another attempt, cancels it through the API, verifies connection and process cleanup, and checks that querying the cancelled attempt does not reopen it.
- It opens a final attempt, renews the lease to the actual minimum TTL of six minutes, and starts a longer guest command through the service. A separate read-only SSH check using that lease's existing pinned key verifies that the guest command really started; another API command would wait behind the same operation lock.
- It verifies that TTL expiry rejects console access and disconnects the established service-owned stream while the long guest operation is still active. The guest command must subsequently finish successfully.
- It releases the owned lease, checks that the lease record and selected-store clone directory are absent, and stops only the candidate service when cleanup is confirmed.

## What a pass does not mean

- The public console API does not expose an RFB authentication-success or framebuffer event. A connected socket alone is not an authentication assertion.
- Transcribing two guest-only codes supplies human evidence that the trusted viewer displayed live content from this guest. Authentication is inferred from that successful display path, not from a separate protocol observer.
- This runner does not test rejection of an incorrect VNC password, retrieve a private password, or add a password-bearing test endpoint.
- This runner does not compare pi-vm-relay screenshots. It does not establish that a historical capture problem was fixed.
- This runner does not certify undocumented direct clients or convert a runtime stop request into proof of all internal framework cleanup.
- A pass applies only to the selected image and observed host configuration. Run each supported guest family separately.

## Prerequisites

- The host must be compatible with the [version-pinned runtime patch](rejected-tart-patch.md), and the selected profile must point to the built patched Tart executable.
- TurboVNC 3.1.3 must already be installed at a trusted absolute executable path. The runner does not download or install it.
- A human must be present at an interactive terminal on the service host. The viewer opens on that host, not on a remote API client's computer.
- Prepare a [selected environment](../selected-environments.md) with a populated isolated Tart store, the intended image repository, this checkout's `bin/vmctl`, and a new service-state directory. The runner does not import, build, or alter golden images.
- The endpoint must use `127.0.0.1` and an explicit unprivileged port other than 6240. The candidate port must be unused. The other selected-environment isolation and ownership checks remain in force.
- The output directory must be new and outside the image repository and all selected mutable roots. Its parent must already exist.
- The macOS guest must have the lease SSH user logged into its graphical console and have `osascript` available.
- The Linux guest must have `zenity`, `loginctl`, and a local graphical session for the lease SSH user. The fixture obtains the existing X11 display or an unambiguous Wayland socket; it does not create a second desktop session. Missing prerequisites fail the visual test rather than silently skipping it.
- The long-expiry scenario takes at least six and a half minutes after the last viewer opening. Allow additional time for acquisition, two visual confirmations, window closure, and cleanup. Each human response has a bounded timeout.

## Plan without live effects

```sh
python3 tests/e2e/vnc_live.py --plan
```

- This prints the source manifest, prerequisites, intended effects, and verification limits. It does not contact a daemon, create directories, allocate a VM, or open a viewer.

## Run after explicit live and viewer authorization

```sh
python3 tests/e2e/vnc_live.py \
  --execute --allow-vm --allow-viewer --ack-isolated-store \
  --environment /absolute/path/to/isolated-vnc-profile.json \
  --viewer /absolute/path/to/turbovnc/bin/vncviewer \
  --image ubuntu2404 \
  --output /absolute/path/to/new-evidence-directory
```

- These flags authorize the effects of this invocation; they are not a substitute for obtaining permission when an agent runs the command for a user.
- Use `--image macos26` in a separate authorized invocation for macOS guest coverage.
- Do not point the runner at the production profile or replace its endpoint with the running service. It will not attach to an existing daemon.
- The runner returns zero only when all required scenario checks and cleanup pass and the source manifest is unchanged. A skipped visual confirmation is not a pass.

## Evidence and failure recovery

- `report.json` records the source manifest, selected-environment fingerprint, owned identities, allowlisted check results, and cleanup outcome. It does not copy raw API bodies, guest output, process arguments, private state, screenshots, or VNC credentials.
- The candidate's private state remains outside the evidence directory. Never publish that state directory; it can contain lease SSH credentials and service diagnostics.
- An uncertain acquisition is never replayed. Cleanup reconciles by the run's unique purpose and selected environment rather than guessing a VM name.
- A missing lease record after an uncertain request is not proof that no delayed allocation can occur. The runner reports incomplete cleanup and preserves the candidate service for recovery instead of claiming success.
- If release fails or the candidate dies before destruction can be verified, the run fails. The report identifies the recovery purpose, environment, and candidate PID when it is still running. Do not stop that candidate until its owned resources have been reconciled.
- A viewer, guest GUI prerequisite, or host inspection failure is an acceptance failure. The runner does not retry an uncertain viewer launch with a new identifier.

## Offline runner verification

```sh
python3 -m unittest tests.unit.test_vnc_live_runner -v
```

- These tests use mocks and in-memory service results to verify the runner's safety gates, evidence decisions, scenario ordering, and cleanup. They do not start a daemon, VM, SSH connection, or viewer.
- Passing the offline runner tests establishes test-harness behavior, not a live VNC acceptance result.
