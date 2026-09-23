# Historical owned-clone startup comparison

> The runtime patch, fixtures, and raw logs referenced below were removed from the current branch. Tart patching is prohibited, including this startup workaround. The stock-runtime failure remains a compatibility finding to address with an unmodified upstream release and supported host. See the [current design](../design.md) and [archive index](README.md).

## Provenance and scope

- The parent supplied these observations after the user authorized an isolated live VNC test. This implementation agent did not start or operate a VM for this comparison.
- The comparison used the same owned diagnostic clone, `pilot-visual-boot-diagnostic`, in the isolated candidate store `/private/var/tmp/vnc-visual-69f51e63`.
- The original patched binary stalled without obtaining a guest IP. Its symbol-only sample at `/tmp/vnc-owned-boot-sample.txt` placed `Run.run()` inside `NSApplication.run()` on the main dispatch queue, without VM startup worker activity.
- The stock `/opt/homebrew/bin/tart` binary also stalled on that clone. Its corresponding parent-retained sample is `/tmp/vnc-stock-boot-sample.txt`.
- The parent then tested the corrected binary from `/tmp/tart-private-vnc.gYNlcv/tart/.build/debug/tart` on the same clone. The parent reported that the VM booted and obtained a guest IP.
- The source correction replaces the headless nested AppKit loop with `await task.value`, preserving the inherited MainActor task and retaining dispatch signal sources across suspension.
- This comparison supports the startup correction across the real VM boundary. It does not establish VNC readiness, listener scope, password interoperability, screenshot correctness, viewer connection, or human visibility.

## Independent harmless regression

- `headless-executor-tests.log` records the directly executed VM-free scheduling reproducer. The nested AppKit path hits its watchdog, while the awaited-task path runs and resumes the MainActor task under both debug and optimized compilation.
- `build-headless-fix.log` records successful full compilation.
- `protocol-tests-headless-fix.log` records the passing private-control protocol and shim tests after the correction.
- The patch SHA-256 for this comparison is `3c9cf5709ee371e120ee3ebc3ff496b85df32a0d1aa53c0b1eca761ae0c7bb1c`.
