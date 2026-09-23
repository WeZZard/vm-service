# Historical rejected Tart patch

> Tart patching is prohibited. The patch, fixtures, and raw build/disassembly evidence described below have been removed from the current branch. This document is retained as a historical record, not an installation guide or a permitted design. Its commands must not be used for deployment. See the [current design](../design.md) and [archive index](README.md).

## Pinned source and review status

- The upstream base is `cirruslabs/tart` tag `2.32.1`, commit `8aa377b71ebfd90b2df9803d3e20033f58d6800c`.
- The patch is `patches/tart-private-vnc.patch`. Its SHA-256 is `3c9cf5709ee371e120ee3ebc3ff496b85df32a0d1aa53c0b1eca761ae0c7bb1c`.
- This is a concrete runtime patch for source review. It is not permission to deploy, sign, install, start a VM or VNC server, or open a viewer.
- The new listener restriction deliberately depends on private framework layout. It is not a supported Apple interface or a portable macOS implementation.
- The existing stock VNC implementations and their URL behavior are unchanged. The new private path does not call those implementations or create a URL.
- Production activation still requires review of this material runtime change, review of the service/viewer integration, and separately authorized live acceptance.

## Private control protocol

The runtime option is `tart run <vm> --no-graphics --private-vnc-fd N`. This describes the integration interface; it is not a live-test instruction.

- `N` must be greater than 2 and identify an inherited, connected, anonymous `AF_UNIX` / `SOCK_STREAM` socketpair endpoint. Both local and peer socket addresses must be unnamed. A file, pipe, TCP socket, or named Unix socket is refused.
- The runtime takes ownership of the descriptor, sets `FD_CLOEXEC`, enables nonblocking I/O, and suppresses socket `SIGPIPE`. The controller must close its copy of the runtime endpoint and ensure unrelated processes do not retain either endpoint.
- Each message is UTF-8 NDJSON with `schemaVersion: 1`. A frame may contain at most 8192 bytes, including its newline. Unknown fields and operations are rejected.
- Credentials appear only in the `ready` response on this inherited FD. The service must capture the descriptor privately and redirect runtime stdout/stderr to `DEVNULL`; framework diagnostics are not a supported credential channel.
- Every connection allows one configuration and one terminal revocation. A new configuration cannot revive a revoked session.

### Configure

The controller sends this message before any VNC server is constructed:

```json
{"schemaVersion":1,"op":"configure","lease_id":"opaque-lease-id","runtime_id":"opaque-runtime-id","environment_fingerprint":"legacy","deadline":2000000000}
```

- Each identity field must be a nonempty string of at most 256 UTF-8 bytes with no control characters. An unselected legacy environment uses the literal string `legacy`, not null.
- The deadline is a finite, future Unix timestamp in seconds. Booleans, strings, NaN, infinity, and past deadlines are rejected.
- The runtime echoes the identity; the anonymous channel binds this process to its controller. The service remains responsible for matching that identity to its actual lease, environment, and process. Echoing an identity is not independent lease authentication.
- Configuration must arrive within 10 seconds after control monitoring starts following VM boot. The server must report a nonzero port within 10 seconds after configuration.

The runtime returns `op: "ready"` with `schemaVersion`, all three identity fields, integer `port`, string `password`, and `listener_scope: "loopback-ipv4"`. This response contains no URL and is never printed. It establishes that the guarded constructor/start path ran and the framework reported a port. It does not prove viewer authentication, a displayed desktop, or measured kernel listener scope.

### Renew and revoke

```json
{"schemaVersion":1,"op":"renew","deadline":2000000100}
{"schemaVersion":1,"op":"revoke"}
```

- A successful renewal returns `{"schemaVersion":1,"op":"renewed","deadline":2000000100}`. Renewal cannot revive an already-expired session.
- Explicit revoke returns `op: "revoked"`, `reason: "requested"`, `stop_requested`, and `disconnect_verified: false`, then closes the channel. `stop_requested` is true only when a server existed and the runtime invoked its stop selector.
- EOF, controller loss, malformed or oversized input, read/write failure, expired deadlines, readiness timeout, and listener guard failure are terminal. Responses use fixed reason codes rather than raw framework errors. A broken channel may prevent delivery of its final response.
- Revocation does not cancel the VM run task. The runtime clears its server reference, stops monitoring, closes the inherited descriptor, and never constructs another server for that session.
- The runtime checks both wall-clock expiry and a monotonic deadline calculated at configuration/renewal. Moving the wall clock backwards cannot extend the configured duration. Checks run on the main dispatch queue without depending on service-side guest-command locks.
- Nonblocking output fails closed on a partial write or backpressure rather than holding the VM event loop or queuing credentials indefinitely.
- `revoked` acknowledges a stop request, not verified disconnection of existing clients. The service must separately close its tracked viewer connections. No live disconnection evidence exists yet.

## Listener restriction

The patch adds a separate Objective-C SwiftPM target, `PrivateVNCLoopback`, and a private Swift adapter that has no URL/viewer interface.

Before constructing the server, the adapter checks all of the following:

- The process targets arm64 and `kern.osversion` is exactly `26A428`.
- The Virtualization framework UUID is exactly `418EFC48-09D9-3606-918E-B6F95BAEFFDC`.
- The constructor, `start`, and `stop` IMPs resolve to the same expected Virtualization framework image. The constructor and `start` offsets must be `0x1b99f8` and `0x1ba548` relative to the Mach-O header. Function pointer authentication is stripped only for metadata lookup.
- The inspected instructions at `start + 96` must decode to the C callback at header offset `0x1bcfd4`. The guard checks ADRP/ADD, PACIZA, argument moves, and the branch instruction shape. This is a `dispatch_async_f` function callback, not an Objective-C block.
- `_VZVNCServer` has instance size 184, `_server` offset 48, `_serverDelegate` offset 56, and the inspected C++ unique_ptr type encoding.

After construction, but before VM assignment or `start`, the shim checks the exact class, non-null aligned backend pointer, untouched state at offset `0x18`, initial scope at `0x1c`, initial byte at `0x40`, and null listener at `0x48`. Only then does it set the scope field at `0x1c` from 0 to 1 and verify a volatile readback. Independent source review found that a plain memcpy readback was optimized away; the delivered fix retains a separate load and comparison at `-O2`, which the test suite checks.

The independently inspected framework start routine maps that field to `htonl(INADDR_LOOPBACK)` before configuring its Network.framework local endpoint and creating/starting the listener. A failed guard never falls back to wildcard startup. This restriction does not change firewall rules, install a privileged helper, or wrap an otherwise unrestricted listener with a proxy.

These checks implement the specific source mechanism. They do not replace a future authorized runtime bind/connection test, and they do not establish resistance to a hostile same-process code injector.

## Read-only capability discovery

`tart vm-service-vnc-capabilities` prints JSON containing:

- `schemaVersion: 1` and `private_vnc_protocol: 1` describe the interface.
- `available` reports whether the exact-image and metadata guards pass on the current process.
- `listener_scope` is `loopback-ipv4` when those guards pass, otherwise `unavailable`.
- `constraints` records the pinned build/image, source-review and live-acceptance requirements, unverified stop disconnection, descriptor framing, and absence of an automatic viewer.

The exact discovery invocation bypasses Tart's ordinary Root telemetry and garbage collection before they initialize. It does not construct a VNC server or VM, read VM state, create credentials, or open a viewer. `available: true` is implementation compatibility, not deployment approval or live acceptance.

## Source-only verification

Fetch the pinned source into a new temporary directory and check/apply the patch there:

```sh
git clone --depth 1 --branch 2.32.1 https://github.com/cirruslabs/tart.git "$TMP/tart"
git -C "$TMP/tart" rev-parse HEAD
git -C "$TMP/tart" apply --check /absolute/path/to/patches/tart-private-vnc.patch
git -C "$TMP/tart" apply /absolute/path/to/patches/tart-private-vnc.patch
```

The delivered verification performed a clean application, reverse-application check, and byte-for-byte comparison of all eight patched files against the implementation checkout.

The full executable compiled with the already installed Xcode 27 beta 4 toolchain:

```sh
cd "$TMP/tart"
DEVELOPER_DIR=/Applications/Xcode-27.0-beta-4.app/Contents/Developer \
  xcrun swift build --build-system native --product tart
```

- No Xcode selection, system configuration, signing, installation, or deployment was changed.
- The first default build failed during build-system property-list initialization. A native build using Command Line Tools then failed on missing C++ standard-library headers. Selecting the existing Xcode toolchain for this command resolved those environment failures.
- The first full source compilation exposed collisions with upstream types named `Set` and `Darwin`. The patch now uses `Swift.Set` and unqualified C socket calls; the subsequent complete build passed.
- The upstream `Package.resolved` was unchanged. Dependencies were fetched only for compilation.

Run the harmless tests from vm-service:

```sh
python3 tests/tart-private-vnc/test_protocol.py
```

- The runner extracts the actual added Swift control source from the patch and compiles it with a fake server. It does not import Virtualization into that fixture or open a listener.
- Fifteen unittest methods passed in the final parent run; this includes an added cross-language test connecting the actual Swift control source to the Python manager. They cover readiness, identity, private credential delivery, explicit revoke, EOF, closed-peer loss, renewal/expiry, fragmented and oversized input, invalid schemas and deadlines, reconfiguration, configure/readiness timeouts, invalid descriptors, and preservation of the simulated VM process.
- The Objective-C tests compile the actual shim and metadata-only variants with deliberately incorrect UUID, class size, OS-build, initializer IMP, and callback-target guards. The incorrect variants report unavailable; an unrelated NSObject is rejected before backend access. No VNC instance is allocated by those tests.
- The real built executable's capability command reported compatible metadata on the inspected host. A nonexistent temporary `TART_HOME` remained nonexistent after the probe.
- `tests/tart-private-vnc/evidence/` retains the final build log, unittest log, metadata capability response, layout probe result, optimized shim assembly, and the listener researcher's read-only layout/disassembly evidence. Earlier build attempts and the implementation checkout remain under `/tmp/tart-private-vnc.gYNlcv/` for this review session.

## Headless startup correction after the parent's authorized live attempt

- The parent reported that both this compiled candidate and stock Tart stalled on the same isolated clone. Its symbol-only sample showed `Run.run()` executing `NSApplication.run()` on the main dispatch queue, with no VM startup activity. This is not evidence that the private VNC implementation caused the inherited stall.
- `tests/tart-private-vnc/HeadlessExecutorFixture.swift` reproduces the scheduling problem without a VM, server, viewer, or window. After an async suspension, entering the nested AppKit loop prevents the queued MainActor startup task from executing on the inspected host. The same fixture completes when it awaits the task instead.
- The corrected headless branch awaits `task.value` rather than running a nested AppKit event loop. The async entry point continues to service the main executor. VM startup remains on the inherited MainActor task; no detached task or off-actor Virtualization access was introduced. Signal sources are explicitly retained across suspension. The graphical branch is unchanged.
- `python3 tests/tart-private-vnc/test_headless_executor.py` passes both test methods. The old fixture hits its watchdog and the corrected fixture enters, suspends, and resumes the task under both debug and optimized compilation. The full executable builds, the fourteen protocol methods still pass, and clean application/reverse applicability/byte comparison still pass.
- The new logs are `tests/tart-private-vnc/evidence/headless-executor-tests.log`, `build-headless-fix.log`, and `protocol-tests-headless-fix.log`. Earlier evidence is retained rather than overwritten.
- The parent subsequently reported a successful comparison on the same authorized, owned clone: the old patched binary and stock Tart stalled without an IP, while the corrected headless-await binary booted and obtained a guest IP. This is parent-reported real VM startup evidence, in addition to this agent's directly executed scheduling fixture. The parent is separately retrying service-level VNC acquisition; no VNC readiness or visibility result is implied by the boot comparison.

## Evidence not supplied by this implementation agent

This agent did not start a VM, VNC server, viewer, production service, or privileged helper. The parent's separately authorized live attempts exposed the startup failure and then confirmed the corrected binary could boot the same clone and obtain an IP. That boot comparison is not a successful VNC live acceptance result. Actual listener binding, VNC password interoperability, existing-client disconnection, macOS/Linux guest display correctness, relay screenshot agreement, and human visibility confirmation remain unverified by this agent. The patch must not be treated as a deployed feature.
