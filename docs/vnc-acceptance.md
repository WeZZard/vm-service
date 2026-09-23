# Acceptance specification for stock-Tart viewing

## Status and pass rule

- **This is a test specification, not an implemented runner or a passing result.** The previous runner and runtime patch were removed. The previous Linux two-VM test is historical and cannot establish this design's acceptance.
- A pass requires the supported deployment procedure, the installed service, and the newly built mcp-vm-relay to complete the workflow without a patched Tart binary or a test-only viewer adapter.
- Linux and macOS must pass separately. A missing prerequisite, skipped visual check, unverified cleanup, or unsupported target is not a full pass.
- Live tests require explicit authorization for disposable VMs and any visible viewer. Source review and this documentation change do not authorize those effects.

## Installation and upgrade

- Begin with an unmodified upstream Tart installation and record its version, origin, host OS, service revision, relay revision, viewer version, and guest image identity.
- Run the documented vm-service installation or upgrade procedure and the normal relay build/upgrade. Confirm that required dependencies and persistent service configuration are installed or explicitly diagnosed.
- Verify that no production path points to a `/tmp` build, a private patch, a development checkout of Tart, or an experimental adapter used only by the test.
- Restart the service and relay through their normal procedures and repeat capability checks. Preserve active unrelated leases and ordinary non-VNC behavior.
- Test the previously observed stock-Tart headless startup failure on the affected host combination. A working patched binary is not an acceptable resolution; use a verified upstream release and supported host or report incompatibility.

## Guest readiness and network scope

- Acquire a fresh owned lease with VNC requested and application credentials disabled. Verify exact lease, environment, image, guest identity, console user, and graphical session.
- For Linux X11, verify that the sharing server attaches to the agent's existing display and that its inetd path creates no guest VNC TCP listener.
- For macOS, verify supported sharing provisioning, allowed-user policy, standard shared-console behavior, and any required consent. Do not accept another login session or a virtual display.
- For a guest listener, independently verify loopback connectivity and rejection from the host and a neighboring guest across applicable IPv4 and IPv6 paths. Verify restrictions were effective before the server became reachable and remain effective after restart.
- Inspect the service broker's actual bind scope. A loopback address in a response, URL, or configuration file is not a substitute for inspection.
- Verify authentication, including rejection of an incorrect credential where the backend supports a controlled negative test. Do not print, persist in evidence, or expose real credentials to the evaluator.

## Real relay and visual assertions

- Use the newly built relay to request viewing, then exercise its actual console-open path and normal standard viewer. A manually opened viewer is insufficient for relay integration acceptance.
- In the source guest, render a fresh random code and a randomized arrangement of colored shapes. Retain expected values separately from screenshots and model prompts.
- Capture the actual viewer window or viewer machine's desktop after successful display. A screenshot taken only inside the source guest does not establish VNC rendering.
- Submit the viewer image to a vision-capable vLLM model without the expected answers. Compare its phase, code, shapes, colors, and positions to the independently retained manifest using one documented comparison rule.
- Change the source pattern without reconnecting and repeat the screenshot and comparison. A single static image does not establish live updates.
- Capture the same source content through relay and compare the identity/code with the viewer. A black or different relay image remains a failure of the complete requested workflow even if the VNC picture is correct.
- Retain actual screenshots, raw model responses, comparison decisions, timestamps, and hashes. Record obstructions and unexpected dialogs rather than removing them or hiding them from the report.
- Model evaluation, assistant image inspection, and human confirmation must be reported separately. If human visibility is required, record an actual confirmation after any authentication prompt, not merely process launch.
- The user's two-VM scenario remains a useful additional test: run a real standard viewer in a second disposable VM and capture that VM's screen. It supplements, rather than replaces, the production viewer and relay path. Platform-specific Apple authentication must use a compatible viewer machine rather than being inferred from a Linux client.

## Lifetime, isolation, and failure cases

- Close the viewer normally and confirm that the source guest continues executing agent work. Reopening requires an explicit new attempt.
- Cancel through the console API, verify all managed connections and attempt processes end, and confirm the VM remains usable.
- Let an actual short lease TTL expire during a longer guest operation. Verify existing streams close and new opens fail before VM destruction and without waiting for the operation lock. Do not substitute mocked clocks for this live case.
- Kill or restart the service while viewing. Verify the worker's controller-loss/deadline behavior, SSH child cleanup, and absence of automatic viewer replay or reconnection.
- Test duplicate identifiers, stale console instances, name reuse, wrong selected environments, cancelled-before-open requests, slow launch, and delayed renewal. No case may connect to another lease or resurrect a revoked attempt.
- Verify that closing the managed path cannot be bypassed through an unintended direct guest listener. Do not claim containment of trusted guest administrators who can deliberately create new access paths.
- Exercise preparation and cleanup failures. Preserve uncertain ownership for recovery rather than replaying acquisition or declaring a disappeared record to be proof of destruction.

## Cleanup and reporting

- Release only the test-owned leases, verify the purpose clones are absent from the selected Tart store, reap viewer/SSH workers, and stop only the test-owned service when safe.
- Preserve an existing lease if a test accidentally targets it; never replace or restart it to make acceptance pass.
- The final report must separate installed-source identity, automated tests, real stock boot, authentication, visual comparison, human confirmation, lifetime behavior, and cleanup.
- The overall task remains incomplete if the deployment or either required platform/relay workflow is still unverified. A narrower successful experiment must be named as such.
