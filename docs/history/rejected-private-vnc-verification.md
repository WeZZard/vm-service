# Historical withdrawn implementation and verification

> All implementation status statements and test counts below concern the discarded approach, not current code or replacement acceptance. Tart patching is prohibited. See [current verification status](../capability-transparency-verification.md) and the [archive index](README.md).

## Latest live evidence

- The user subsequently authorized a two-VM visual test. Two disposable Linux guests successfully exercised VNC delivery and changing randomized desktop patterns after a real headless startup defect was fixed.
- Screenshots were captured from the viewer VM and evaluated by `GLM-5.3-Flash-EXL3` through vLLM without supplying the expected answers. Both patterns matched their source-generated manifests.
- [The two-VM acceptance report](../two-vm-vnc-acceptance.md) records the boot fix, screenshots, exact scope, observed desktop error dialog, and verified cleanup. This used Linux TigerVNC through a private test adapter, not the production macOS TurboVNC application.
- The source-only status and pending-live statements below describe the earlier verification stage. They are not a claim that this later test covered macOS guests, production activation, TTL expiry, or the complete relay workflow.

## Current implementation and verification

- **The VNC source implementation is now present.** It includes the patched Tart runtime, a pre-start loopback restriction, private credentials, explicit service-host viewer opening, cancellation, and expiry independent of VM destruction. [The console contract](rejected-private-vnc-console-contract.md) defines the delivered interface.
- The patched Tart 2.32.1 executable compiled using the installed Xcode 27 beta 4 toolchain. Its current listener adapter accepts only the inspected arm64 build `26A428` and exact Virtualization framework UUID. Other builds remain unavailable.
- The final unit runner reported 247 tests, with one optional collector skip and no failures. The integration runner reported 55 tests with no failures. The separate runtime runner reported 15 passing tests, including the actual Swift control source connected to the Python manager through an inherited socket.
- Final local evidence is retained in `test-evidence/private-vnc/unit-final.log`, `integration-final.log`, and `runtime-final.log`. The runtime patch's build and apply-check evidence is described in [its patch notes](rejected-tart-patch.md).
- Independent Python review found and prompted fixes for release/preparation races, blocked-state expiry, clock-adjustment handling, stale deadline snapshots, late launch persistence, and incomplete viewer cleanup. The final scoped reviewer approved the fixes after 44 console regression tests passed without expected failures.
- Independent listener review checked the private layout, callback decoder, image guards, and ARC ownership against retained disassembly. Its requested volatile readback fix is included and verified in optimized assembly.
- Credentials are tested with synthetic values. Tests exercise malformed private frames, wrong identities, failed launch, restart without replay, actual tracked-stream disconnection, and expiry during blocked state reads, viewer launch, and control I/O.
- **Live acceptance remains outstanding.** No real VNC server, VM, viewer, production service, or image was started or modified for this implementation. Actual listener binding, VNC authentication, macOS/Linux display correctness, screenshot agreement, and human visibility are not established by these results.
- The earlier configuration/discovery commit was pushed as `3a2414d`. The VNC changes are a source delivery; the running service has not been deployed or restarted. pi-vm-relay still needs to integrate the explicit console contract; no claim of an already-working relay workflow is made.

## Historical reporting-only delivery

- The following sections record the earlier partial implementation and investigation. Statements there that VNC was unimplemented or its mechanism unresolved describe that earlier delivery, not the current source.
- At that point, acquisition discovery, centralized resolution, configuration reporting, and new identifiers were implemented, while VNC remained incomplete.
- The user requested implementation of `.plans/capability-transparency.md`. That request did not remove its review gates for material runtime changes or authorize live VM acceptance, viewer interruption, image changes, or production activation.
- Existing documentation edits were preserved. No commit, push, service restart, production request, image change, real VM acquisition, or viewer launch was performed.

## Delivered source changes

- `bin/acquisition.py` resolves resource overrides, initial TTL, and readiness policy without I/O. The same resolved values drive the stored report and provisioning commands.
- `GET /acquisition-capabilities` and `vmctl acquisition-capabilities` return service option definitions and a detached startup image-configuration snapshot. They do not read lease state, invoke Tart, create credentials, or open a viewer.
- The existing networking-only `/capabilities` response is unchanged.
- New leases include `lease_id` and `configuration`. The report distinguishes requested arguments, effective provisioning instructions, default sources, and acknowledgment of the resource-setting command.
- Existing top-level resource fields remain unchanged. Legacy records are not assigned guessed configuration or new identifiers during reload.
- Valid ordinary boot commands remain `tart run <vm> --no-graphics`. Omitted resource settings retain CPU 6, memory 16384 MB, and the inherited source disk size.
- Invalid resource types and nonpositive values fail before reservation. Invalid and non-finite TTL values produce bounded client errors instead of conversion errors or invalid expiration state.
- `--no-wait` help now states the actual synchronous behavior. Both modes still verify SSH and transfers; the bounded mode uses the existing shorter bootstrap and command-probe policy.
- `vnc: true` and `vmctl acquire --vnc` explicitly fail before allocation. No stock VNC flag, guessed endpoint, credential-bearing URL parser, or pretend viewer implementation was added.
- The then-current acquisition contract defined that delivery's API and compatibility limits. The [current acquisition document](../acquisition-contract.md) is now a replacement proposal, not this former implementation's contract.

## Automated verification

- Tests used isolated temporary service state, fake Tart, and stubbed guest SSH. Integration tests started fixture-only loopback HTTP servers and exercised the real CLI. They did not start a hypervisor or contact the production service.
- The unit runner reported 192 tests, with one skip and no failures. The skipped test requires an explicitly selected external inventory collector; no collector path was supplied for this run.
- The integration runner reported 55 tests with no failures.
- Integration output includes existing fixture resource warnings about unclosed subprocess streams and HTTP error objects. These warnings were not hidden or represented as console failures.
- The final logs are `test-evidence/capability-transparency/unit-final.log` and `test-evidence/capability-transparency/integration-final.log`. That directory is Git-ignored local evidence, not a committed delivery artifact.
- Earlier focused and aggregate logs remain in the same directory. Later checks do not overwrite the earlier records.

```sh
env -u VM_ENVIRONMENT_FILE -u VM_ENVIRONMENT_FINGERPRINT \
  -u PILOT_INVENTORY_COLLECTOR \
  python3 -m unittest discover -s tests/unit -v

env -u VM_ENVIRONMENT_FILE -u VM_ENVIRONMENT_FINGERPRINT \
  -u PILOT_INVENTORY_COLLECTOR \
  python3 -m unittest discover -s tests/integration -v
```

### Verified behavior

- Tests compare resolved resource settings with the actual fake-Tart command arguments and verify that image-advertised defaults do not silently replace existing service defaults.
- Tests distinguish successful resource configuration from failed boot or SSH readiness, including failed cleanup that retains a releasing lease.
- Tests exercise both synchronous readiness modes, legacy aliases and response fields, old records without configuration, heartbeat, and reload.
- Tests verify new acquisition identifiers remain stable during a lease and differ on reacquisition. This does not establish stale-request enforcement for future console routes, which do not exist yet.
- Discovery tests guard configuration parsing, subprocess execution, lease-state operations, key creation, and lifecycle functions against accidental invocation.
- Tests cover uninitialized, initialized, and failed startup snapshots. They verify that requests do not retry initialization or refresh edited image configuration.
- Selected-environment tests verify executable and store routing, reject mismatched headers, and confirm that populated discovery uses the selected repository rather than conflicting ambient configuration.
- Synthetic credential markers in startup guest configuration and invalid option values do not appear in new discovery responses, validation errors, or fixture service logs. These tests do not claim credential safety for an unimplemented VNC launcher.
- HTTP and CLI tests verify that requesting unavailable VNC creates no lease or SSH directory.

## Independent code review

- A read-only review found that conditional descriptor fields originally advertised null defaults even though the existing API requires omission. The descriptor now marks those fields with `omit_when_unset`, `nullable: false`, and applicability conditions. A descriptor-to-request test verifies that advertised defaults form a valid ordinary request.
- The review also identified missing coverage for populated selected-environment discovery. An integration test now initializes the selected repository's snapshot while a conflicting ambient repository exists, then guards parsing and Tart calls during repeated discovery.
- Both fixes were included in the final passing test runs. The reviewer found no blocking lifecycle regression in the inspected reporting changes.

## VNC mechanism investigation

- A separate read-only review examined the retained Tart 2.32.1 source and version-pinned standard viewer source. It did not change files or run VMs, viewers, or production services.
- A narrow candidate would modify Tart to deliver connection data over an inherited private socket rather than construct and print a password-bearing URL. This channel does not exist in stock Tart and has not been implemented here.
- TurboVNC 3.1.3 documents and implements `AutoPass`, which reads the password from stdin. That is a possible standard viewer interface, not proof of compatibility, safe persistence behavior, or a complete backend integration.
- The relevant source is [TurboVNC's parameter definition](https://github.com/TurboVNC/turbovnc/blob/3.1.3/java/com/turbovnc/rfb/Params.java#L905-L910) and [authentication implementation](https://github.com/TurboVNC/turbovnc/blob/3.1.3/java/com/turbovnc/vncviewer/CConn.java#L258-L351).
- The unresolved server issue is connection scope. The inspected Tart constructor specifies a port but does not specify a bind address or accept a supplied listening socket. A returned loopback URL does not prove that the underlying server accepts only loopback connections.
- A parent-side read-only Objective-C method inventory examined the installed `_VZVNCServer` class. It listed port-based constructors and stop methods, but no explicit bind-address or supplied-listener method. No server object or VM was created. The output is retained in `test-evidence/capability-transparency/vnc-class-methods.log`.
- That inventory does not prove the server's actual listening behavior or rule out every possible private implementation. It is not permission to start a potentially exposed listener and check afterward.
- The candidate also requires proof that revocation disconnects existing clients, not just that it prevents new connections. Runtime-side deadlines, controller loss, and restart reconciliation remain to be designed and reviewed.
- A loopback proxy does not by itself restrict a separate underlying wildcard listener. No firewall rule, privileged helper, runtime patch, or viewer installation was introduced as an implicit workaround.

## Remaining gates

- Establish a supported or explicitly reviewed runtime mechanism that meets the required connection restrictions and private credential handoff.
- Review the concrete runtime change, viewer integration, and console-access lifetime before implementing them. Keep the existing household-trust model rather than expanding this into a general authentication project.
- Implement lease-bound console preparation, opening, cancellation, duplicate handling, expiry, and cleanup with actual runtime integration.
- Run the console-specific race, credential, process-loss, and restart tests after that implementation exists.
- Obtain separate authorization for isolated live acceptance and opening the human viewer. Verify macOS and Linux guests separately and record real human visibility confirmation.
- Keep the historical relay-capture disagreement separate. Configuration tests and VNC source investigation do not prove that black screenshots were repaired.
- Deploy only after explicit approval, with rollback that revokes new console resources and preserves unrelated leases.
