# Installing guest-console support

## Scope and current verification boundary

vm-service uses an **unmodified upstream Tart installation** for VM lifecycle operations. It does not compile Tart, patch a binary, inject framework code, or select Tart's experimental host-side VNC server. Console sharing attaches to the existing guest desktop through the lease's authenticated SSH connection.

The installer validates host configuration and runs only `tart --version` against the selected runtime. A version string is not proof of upstream provenance, a successful headless boot, a prepared guest image, an authenticated viewer, or displayed pixels. Install Tart from its upstream distribution and retain its provenance separately. The previously observed stock Tart 2.32.1 headless stall on the tested prerelease macOS host remains a compatibility concern; this installation procedure does not claim to resolve it.

Console viewing is initially disabled. Configuration enables the service-side path, not a claim that every image supports it. Each requested guest must pass acquisition-time session, dependency, and isolation checks. Linux and macOS require separate live acceptance under [the console contract](console-contract.md) and [the acceptance specification](vnc-acceptance.md).

## Host configuration

The service loads configuration once at startup, using this precedence:

1. An explicit `load_config(path)` argument selects that file.
2. `VM_SERVICE_CONSOLE_CONFIG` selects a persistent file for the service.
3. Otherwise, the service reads `~/.config/vm-service/console.json` if it exists.

An absent default file means disabled support. An explicitly selected missing file or any invalid file is an error, not permission to fall back. Discovery uses the startup snapshot; it does not read configuration again, run a viewer, inspect a guest, or create a lease. Restart is required after configuration changes.

The JSON schema contains only these fields:

| Field | Requirement |
|---|---|
| `schemaVersion` | This required field is the integer `1`. |
| `enabled` | This optional Boolean defaults to `false`. |
| `linux_viewer` | This optional string is an absolute path to an installed TurboVNC viewer executable. An omitted field leaves Linux viewing unavailable. |
| `macos_viewer` | This optional string is an absolute path to Apple's Screen Sharing `.app`. When omitted, the standard system application is selected if installed. |

The default macOS viewer is selected from a fixed list of trusted system locations, in this order:

1. The service checks `/System/Applications/Utilities/Screen Sharing.app`.
2. If that application is absent, the service checks `/System/Library/CoreServices/Applications/Screen Sharing.app` for older macOS layouts.

The first installed candidate must pass the normal trust checks. An invalid installed candidate is an error, not permission to skip to another application. An explicit `macos_viewer` overrides this list. The application must contain its executable at `Contents/MacOS/Screen Sharing`. A null value is not a substitute for omitting a field. Unknown fields, duplicate keys, non-Boolean enablement, and unsupported schema versions are rejected.

The configuration must be a regular nonsymlink file owned by the service user, with mode `0600` or `0644`. Every parent directory must be owned by that user or root and must not allow group or world writes. Symlink parent directories are rejected. Configuration in `/tmp` is therefore unsuitable. The service bounds the file size at 65,536 bytes.

Explicit viewer paths must already exist and be trusted. Executables must be executable regular files. Application directories and viewer files must be owned by the service user or root and must not allow group or world writes. Viewer parents have the same trust checks. A final package-manager symlink is accepted only when its original and resolved parent paths satisfy these checks. A group-writable Homebrew directory can therefore cause rejection; do not weaken the validator or change unrelated system permissions merely to pass it. Select a correctly installed, trusted viewer location.

For example, this enables Linux viewing with an administrator-installed TurboVNC executable. Replace the example path with the actual trusted installation path; the installer does not download TurboVNC.

```json
{
  "schemaVersion": 1,
  "enabled": true,
  "linux_viewer": "/opt/TurboVNC/bin/vncviewer"
}
```

The Linux adapter expects TurboVNC's supported private standard-input password interface. An arbitrary VNC client is not an interchangeable dependency. Path validation establishes filesystem trust, not vendor provenance or viewer compatibility. Verify the installed viewer release during live acceptance. The host must also have a logged-in graphical session: the viewer appears on the **service host**, not on an arbitrary remote relay client's computer.

## Check and install

Run the installer binary from the staging directory that holds the service executables beside it, which is `~/.local/libexec/vm-service/vm-service-install` in the example deployment. Its service and helper paths are relative to the binary itself, so it does not depend on a hard-coded developer checkout.

First, create and protect the configuration at its intended permanent location. The installer consumes the existing file; it does not generate credentials, rewrite the file, or modify golden images. Initial enablement requires an explicit `--console-config PATH` argument, including when an enabled file already exists at the default location.

Validate before changing launchd:

```sh
~/.local/libexec/vm-service/vm-service-install --check \
  --console-config "$HOME/.config/vm-service/console.json"
```

`--dry-run` has the same behavior as `--check`. These modes read configuration and service state, validate the presence and executability of the sibling service executables (including `console-worker`, `guest-console-agent`, and `guest-console-agent-linux`), and run `tart --version`. They do not write a plist, create service state directories, call launchctl, boot a VM, launch a viewer, or restart a service. An installed selected-environment profile is resolved read-only. A successful check reports host configuration only.

After the existing leases have been released and a real deployment has been authorized, install with:

```sh
~/.local/libexec/vm-service/vm-service-install \
  --console-config "$HOME/.config/vm-service/console.json"
```

The installer persists the selected path in the LaunchAgent's `EnvironmentVariables` using a plist serializer. Spaces, ampersands, and other XML characters are not interpolated into shell commands or raw XML. The installer does not copy the caller's complete shell environment into launchd.

Existing no-flag operations remain available:

```sh
~/.local/libexec/vm-service/vm-service-install
~/.local/libexec/vm-service/vm-service-install remove
```

A fresh ordinary installation keeps console support disabled unless an enabled configuration is explicitly selected. An upgrade preserves the installed configuration selector, bind address, service port, selected environment, state roots, and other installed environment values unless the caller supplies a service-setting override. The installer launches the service executable directly and does not select an interpreter, so `VM_SERVICE_PYTHON` no longer exists. The fresh-install bind default remains `127.0.0.1`.

The installer checks both the previous and proposed service state roots. It refuses installation, restart, or removal while any lease record remains, including failed or deleting records that may still own resources. It never stops a VM to make installation pass. The final check and restart hold the service state lock to prevent a pending acquisition from being added between them. Before changing the plist, the installer queries the actual launchctl job in both the user's GUI and user domains and verifies that the reported job belongs to the installed plist. Unknown job state or ambiguous ownership is a refusal, not an assumption that the service stopped.

If a job exists, unload must succeed. The installer then confirms job deregistration, checks that its reported process has exited, and verifies that the daemon ownership locks are free. These checks include the previous and proposed service state directories and any selected Tart-store ownership lock. A daemon holding ownership without a registered launchctl job also blocks replacement or removal. The installer does not terminate such a process itself. If unload fails or a job, process, or owner remains, the installed plist is preserved and no replacement service is loaded. Read-only check modes do not call launchctl and therefore do not certify that a later unload will succeed.

Do not manually delete state records or ownership lock files to bypass these guards; release or reconcile the owned resources first.

To disable configuration for a later installation, set `enabled` to `false` in the trusted file and restart only after leases are released. Existing VMs are not retrofitted with console support.

## Linux guest prerequisites

Prepare these dependencies in the image pipeline or an explicitly authorized clone-preparation step, not by silently changing a golden image during installation:

- The guest must have Python 3 for the fixed guest adapter.
- The guest must provide systemd-logind session metadata through `/usr/bin/loginctl`.
- The SSH user must own the active local X11 graphical console session and its valid Xauthority file.
- The guest must provide `/usr/bin/xdpyinfo` to verify access to that existing display.
- The guest must provide `/usr/bin/x11vnc` with the adapter's inetd, password-file, and view-only options.
- The lease's existing SSH identity and transfer readiness must work before console preparation.

The adapter uses x11vnc's inetd mode rather than exposing a guest VNC TCP listener. It must not start Xvnc, create a second desktop, guess another user's display, or fall back from Wayland to a different session. A Wayland image is not supported by this X11 adapter merely because x11vnc is installed.

## macOS guest preparation

Use Apple's supported Screen Sharing settings in the guest and allow the intended guest account. Screen Sharing and Remote Management are different configurations; this backend requires the Screen Sharing service. The guest must have an existing logged-in graphical console belonging to the intended user, Python 3, and the fixed adapter's read-only system inspection tools.

Network isolation is a separate prerequisite. An SSH stream to guest loopback does not secure Screen Sharing's other listeners. The current adapter requires guest PF to be enabled and recognizes a top-level quick ingress block on TCP port 5900 on every interface except `lo0`, covering both IPv4 and IPv6. It rejects bypassing skipped interfaces and existing non-loopback states for that port. Its PF and listener inspection requires narrowly scoped, noninteractive permission for the fixed read-only `pfctl` and `lsof` commands. Image owners must provision and review those permissions; do not grant arbitrary passwordless root access to satisfy a probe.

Provision guest filtering before enabling Screen Sharing. Validate rejection of direct connections from the host and neighboring guests for both address families, while permitting the fixed guest-loopback adapter. Merely seeing a PF rule, using NAT, or connecting the host viewer to loopback does not replace this external acceptance test. The installer does not modify the host firewall or automatically enable guest Screen Sharing.

The standard Screen Sharing application prompts the user for supported guest-account authentication and any required consent. Choose **Standard sharing of the existing display**. Do not substitute a separate Log In session or a High Performance virtual display. This implementation does not provision unattended Screen Sharing credentials, manipulate undocumented password files, or embed a password in a URL, argv, ordinary environment variable, or model response. A pending authentication prompt is not a displayed desktop. Unlike the Linux adapter's server-side view-only mode, the macOS backend does not claim server-enforced denial of control.

## Verification without installation

The unit tests isolate `HOME`, provide a fake Tart that accepts only `--version`, and replace launchctl with a temporary stub. They do not perform a production install or restart.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  tests.unit.test_console_config tests.unit.test_console_install -v
```

These tests cover schema and permissions, snapshot-only availability, invalid prerequisites, XML-safe persistence, ordinary install/remove behavior, upgrade preservation, active-lease refusal, and read-only checking. They do not establish stock-Tart boot compatibility, guest isolation, viewer authentication, displayed pixels, or end-to-end relay acceptance.
