# Unmodified-Tart console investigation

## Conclusion and status

- **Use stock Tart for VM lifecycle and guest-native console sharing over a service-owned SSH command stream.** Linux X11 is the simpler initial backend; macOS needs supported guest-sharing preparation and verified guest-local network restrictions.
- This document records the source research behind the replacement. The adapters and lifecycle are now implemented in working source, as described in [the implementation report](capability-transparency-verification.md). No live guest, viewer, image change, installation, or network-policy change was performed during that implementation; the research is not live acceptance.
- The earlier patched-Tart approach is prohibited and removed from the codebase. Its successful Linux screenshot experiment remains [historical evidence](two-vm-vnc-acceptance.md), not acceptance for this design.
- The desired deliverable includes normal vm-service deployment and mcp-vm-relay upgrade. A working manually assembled test path is insufficient.

## Stock Tart does not require a patch for basic VM access

- The baseline service boots stock Tart with `run <vm> --no-graphics` and separately establishes per-lease SSH access. No local patch is inherently needed for VM management or guest commands.
- In the inspected upstream Tart 2.32.1 source, `ScreenSharingVNC.swift` resolves the guest IP and returns a VNC URL. It does not enable guest Screen Sharing, authorize a viewer, restrict the listener, or verify the correct desktop.
- The separate experimental backend uses private Virtualization.framework APIs and generates a password-bearing URL. The headless run path prints it. Capturing that output in a private pipe could avoid ordinary credential logs without changing Tart, but would not solve the independent listener and revocation problems.
- The loopback address in that URL is not proof of the server's bind scope. The inspected constructor supplies a port, not an explicit address. Closing a downstream proxy cannot stop direct clients of an upstream listener that remains available.
- The normal Tart graphical window has a lifecycle coupled to the VM and is not a substitute for a viewer that can close while the VM continues.
- The previous stock 2.32.1 boot stall on the tested prerelease host remains a compatibility issue. The replacement must demonstrate a supported unmodified runtime/host combination; the deleted headless patch cannot be reused.

## Linux candidate: x11vnc over SSH stdio

- x11vnc documents attachment to an existing X display and use of its Xauthority credentials. Its `-inetd` mode carries RFB over stdin/stdout instead of creating a listening socket.
- Its `-viewonly` mode can enforce observation at the server. The adapter should also disable clipboard and file-transfer features that are not required for watching.
- The candidate runs a fixed, non-PTY SSH command in the actual console user's session. Stdout contains only RFB; diagnostics are separate and sanitized. It does not launch Xvnc, Xvfb, or another desktop.
- Read-only inspection of the Ubuntu image's `guest/45-capture.sh` found intended X11 configuration through `WaylandEnable=false` and desktop autostart for the capture agent. This is configuration evidence, not proof of every installed image artifact.
- Installation, actual display discovery, credential delivery, EOF cleanup, and agreement with the agent's screenshots still need acceptance.
- Wayland is a separate backend decision. wayvnc is not a general GNOME/KDE solution, and GNOME Remote Desktop distinguishes existing-session assistance from independent login/headless modes. No support is inferred from an installed package name.

## macOS candidate: built-in Screen Sharing

- Apple documents sharing the Mac's desktop, selecting allowed users, and optional third-party VNC password access. Screen Sharing and Remote Management cannot be enabled together.
- Apple documents that `kickstart` cannot enable Screen Sharing on macOS 12.1 and later. Administrative SSH access is therefore not a supported unattended provisioning method by itself.
- The initial candidate uses supported image preparation through Settings, or a separately applicable managed Remote Management procedure. Existing capture-agent TCC permissions do not establish Screen Sharing authorization, and this design does not extend direct TCC-database manipulation.
- Apple Screen Sharing's standard authentication prompt can accept the guest account credentials without passing them through the model or service. Prefer the account already running the agent's desktop, select Standard sharing and the shared console, and do not choose a separate Log In session or High Performance virtual display.
- Observe mode is a viewer mode, not proof that the account cannot request control. Server-enforced view-only authorization must be evaluated separately if claimed.
- No supported unattended credential-injection mechanism for Apple's viewer was established by this research. TurboVNC's stdin `AutoPass` is documented, but it must not be assumed to support Apple's account-authentication protocol. Its use would require an explicitly configured and reviewed compatible VNC backend.

### Network restriction is an explicit prerequisite

- This research did not establish a documented loopback-only bind setting for built-in Screen Sharing. A connector to guest loopback does not prevent a server from also listening on the guest's network interfaces.
- The implemented preflight additionally rejects all PF translation rules and translation-anchor references. An `rdr pass` rule can bypass the filter rules, so a first quick block alone is insufficient. The supported narrow policy and inspection privileges are documented in [guest preparation](guest-console-provisioning.md).
- The concrete candidate is guest-local ingress filtering provisioned before sharing becomes available. It must block direct VNC on every guest network interface and both IP families while preserving the guest-loopback path. No host-wide firewall change is proposed.
- Acceptance must verify that guest-loopback access succeeds while direct connections from the host and a neighboring guest fail, including after guest restart. NAT alone does not meet this test.
- Filtering must be part of versioned image/clone preparation and visible capability checks. It cannot be a hidden command performed after declaring the listener safe.
- If the guest policy cannot be prepared and verified reliably through supported mechanisms, macOS remains unavailable. The service must not switch to patched Tart or stock experimental VNC as an implicit fallback.

## SSH transport and lifetime

- An SSH command stream avoids relaxing ordinary SSH port-forwarding settings. Linux can run the inetd VNC server directly; macOS can use a fixed connector to its loopback Screen Sharing endpoint.
- The existing lease key, strict post-bootstrap host-key checks, selected environment, and lack of shared SSH control sockets must be preserved. A dedicated console helper must not change global SSH configuration.
- OpenSSH's forwarding restrictions do not prevent a permitted guest command from opening a socket. This is consistent with the existing arbitrary-command service, not a stronger containment guarantee.
- The service must own every local connection and SSH child. A separate bounded connection worker must close listeners and active streams at cancellation, expiry, controller loss, or release without waiting for the guest-operation lock.
- Key deletion, password rotation, or listener closure alone does not disconnect an established session. Disconnect behavior, process reaping, suspend/resume, and stale renewals all require tests.
- The guarantee concerns service-managed viewing under the household-trust model. A caller already allowed to execute arbitrary guest commands can create another access path; console TTL is not containment of a hostile guest administrator.

## Why this differs from the rejected design

- This design uses public guest-sharing and SSH interfaces instead of adding a private Tart protocol and modifying Apple framework layout.
- It accepts explicit guest image prerequisites and, initially on macOS, a standard user authentication step. These costs must appear in deployment and capability reporting rather than being hidden behind a generic VNC flag.
- It gives up pre-boot/recovery display access. That is acceptable for watching a reproduction after guest readiness, but it is not a general hypervisor-console product.
- It does not yet solve stock-runtime compatibility, guest provisioning, or network isolation merely by documenting them. Those checks are required implementation work before either platform is advertised as supported.

## Primary references

- The pinned Tart sources are [ScreenSharingVNC.swift](https://github.com/cirruslabs/tart/blob/8aa377b71ebfd90b2df9803d3e20033f58d6800c/Sources/tart/VNC/ScreenSharingVNC.swift), [FullFledgedVNC.swift](https://github.com/cirruslabs/tart/blob/8aa377b71ebfd90b2df9803d3e20033f58d6800c/Sources/tart/VNC/FullFledgedVNC.swift), and [Run.swift](https://github.com/cirruslabs/tart/blob/8aa377b71ebfd90b2df9803d3e20033f58d6800c/Sources/tart/Commands/Run.swift).
- [x11vnc options](https://github.com/LibVNC/x11vnc/blob/master/doc/OPTIONS.md) document `-inetd` and `-viewonly`, and [the FAQ](https://github.com/LibVNC/x11vnc/blob/master/doc/FAQ.md) explains existing-display access.
- [wayvnc](https://github.com/any1/wayvnc) and [GNOME Remote Desktop](https://github.com/GNOME/gnome-remote-desktop/blob/master/README.md) document their different desktop/session support. Implementation must pin the actual packaged versions.
- Apple's [Screen Sharing settings](https://support.apple.com/guide/mac-help/turn-screen-sharing-on-or-off-mh11848/mac), [sharing workflow](https://support.apple.com/guide/mac-help/share-the-screen-of-another-mac-mh14066/mac), and [kickstart limitations](https://support.apple.com/guide/remote-desktop/use-the-kickstart-command-line-utility-apd8b1c65bd/mac) define the supported guest preparation and viewing behavior.
- Apple's [managed Remote Management](https://support.apple.com/en-us/102024), [PPPC settings](https://support.apple.com/guide/deployment/privacy-preferences-policy-control-payload-dep38df53c2a/web), and [third-party VNC guidance](https://support.apple.com/guide/remote-desktop/virtual-network-computing-access-and-control-apde0dd523e/mac) explain additional constraints rather than guaranteeing this image is ready.
- [OpenSSH key restrictions](https://man.openbsd.org/sshd.8) and [SSH operation](https://man.openbsd.org/ssh.1) define the transport boundary.
- [TurboVNC 3.1.3 parameters](https://github.com/TurboVNC/turbovnc/blob/3.1.3/java/com/turbovnc/rfb/Params.java) document stdin password input. This does not prove Apple account-authentication compatibility or full viewer acceptance.
