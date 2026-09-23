# Guest console adapters and image preparation

## Scope and verification status

- The adapters use an unmodified upstream Tart installation and the lease's existing SSH identity. They do not patch Tart, use private Apple frameworks, or create a graphical session.
- The `console` crate builds fixed SSH commands that run the native `guest-console-agent` binary. The agent artifact and the daemon must remain together in the installed service distribution.
- The daemon uploads the agent binary once per lease to `$HOME/.vm-service-console/agent` with mode `0700`, and then executes that path, so the guest does not need a Python interpreter. The guest still needs the platform prerequisites below.
- The unit tests are offline tests of framing, validation, process arguments, cleanup, and stream handling. They do not establish successful stock-Tart boot, actual x11vnc authentication, macOS firewall isolation, or rendered pixels.
- A probe's `ready` value describes the adapter's current prerequisite checks. It does not mean that a viewer is authenticated or that end-to-end acceptance has passed. The probe includes `acceptance: "unverified"` explicitly.
- This document describes preparation that an image owner or an authorized operator must perform separately. Writing or testing these adapters does not install packages, enable guest services, modify a guest, or change the host firewall.

## Host interface and stream protocol

- `probe_command(kind)` returns a shell command that produces one JSON probe record on stdout. The only accepted kinds are `linux` and `macos`.
- `stream_command(kind)` returns a shell command that accepts a configuration line on stdin and then forwards binary RFB. The command contains source code and fixed arguments, not credentials or caller-supplied executable paths.
- `parse_probe(stdout)` accepts bytes or text and validates the bounded probe record. A result with `ready: false` is an acquisition failure, not permission to open an unverified stream.
- The SSH invocation must be noninteractive and must not allocate a PTY. It must retain the lease identity, host-key verification, environment selection, and existing forwarding restrictions. These functions do not construct or weaken SSH options.
- The caller must preserve stdout and stderr as separate channels. It must not merge diagnostics into the RFB stream or log the initial configuration line.

A successful probe contains the following fields. The `session` object's additional fields depend on the platform and must be carried through unchanged.

```json
{
  "version": 1,
  "kind": "linux",
  "ready": true,
  "backend": "x11vnc-inetd",
  "authentication": "vnc-password",
  "view_only": true,
  "acceptance": "unverified",
  "session": {
    "id": "2",
    "uid": 1000,
    "user": "guest",
    "type": "x11"
  },
  "isolation": {
    "mechanism": "inetd",
    "guest_tcp_listener": false
  },
  "error": null
}
```

- The example session above is abbreviated for explanation and is not a usable session proof. The host must use the complete object returned by the actual probe.
- A failed probe returns `ready: false` and a fixed `error` code. Raw subprocess output and exception text are not returned.
- Linux proofs include the login ID, UID, user name, seat, display, session leader, session start timestamp, boot ID, Xauthority path, and a SHA-256 digest of its contents. They do not contain the X11 cookie.
- macOS proofs include the console session ID, UID, user name, session type, and boot timestamp. The adapter checks both the active IORegistry console session and `/dev/console` ownership.

The first bytes sent to the serving command must be one UTF-8 JSON object followed by a single newline. Construct this object privately in the service, rather than asking a user or model to supply a password:

```json
{
  "version": 1,
  "session": "<the complete session proof returned by the probe>",
  "expires_at": 1790100000
}
```

For a Linux guest the object also carries a `password` field, holding eight characters drawn uniformly from the ASCII letters and digits.

- The JSON must contain exactly the fields above. `kind` is not a JSON field because the command already fixes it. macOS rejects a `password` field.
- `expires_at` is an absolute Unix timestamp in seconds for the guest's hard lifetime ceiling. The ceiling is 720 hours, matching the maximum service TTL. Validation tolerates up to 300 additional seconds for modest host/guest clock skew, but the effective monotonic lifetime is always `min(expires_at - guest_wall_time, 720 * 3600)` seconds. Expired timestamps are rejected.
- The adapter converts the initial ceiling to a monotonic deadline. The independent host worker must enforce the current lease TTL, ordered renewal grants, and controller loss by closing the stream. The guest ceiling does not authorize access beyond the worker's current grant.
- The stream has no in-band guest renewal protocol. Worker renewal grants can extend access only within the guest's original hard ceiling. A new stream beyond that ceiling requires an explicitly authorized and reconciled new attempt.
- Linux requires exactly eight printable, non-space ASCII characters. A leading `#` and the strings `__SKIP__` and `__COMM__` are rejected because x11vnc interprets those values in password files. Random alphanumeric characters avoid those special cases.
- The adapter reads at most 16,384 bytes of configuration and allows ten seconds for that line. It reads the newline without consuming any following RFB bytes.
- After the configuration line, stdin and stdout carry only binary RFB. There is no JSON acknowledgment on stdout before the server's RFB greeting. The host must observe transport progress separately from authentication and displayed pixels.
- The guest rechecks the entire session proof immediately before connecting or starting x11vnc. A changed session is rejected rather than silently attached to a different desktop.
- EOF on SSH stdin, endpoint closure, the deadline, or a failed periodic check ends the managed stream. The host must still supervise the SSH worker and close its input when its controller disappears. An idle but still-open SSH stream is not evidence that the controller remains alive.
- The adapter rechecks the session periodically, including during output backpressure. macOS also rechecks PF and listener identity. A prerequisite inspection is bounded by the remaining deadline.
- The serving command writes only fixed failure codes to stderr. Child x11vnc stderr is discarded, and subprocess inspection diagnostics are discarded. Credentials must also remain absent from host logs, public API responses, portable state, and evidence packages.

## Linux image profile

### Required packages and login state

- The maintained image profile must provide `/usr/bin/loginctl`, `/usr/bin/x11vnc`, and `/usr/bin/xdpyinfo`. On Debian or Ubuntu, the corresponding packages normally include `systemd`, `x11vnc`, and `x11-utils`.
- The image pipeline must install and pin a maintained x11vnc package version. The probe checks that the installed executable documents the required inetd, view-only, password-file, and command-restriction options.
- The lease SSH account must be the same non-root UID and user name as the active local graphical console user. An administrative account connecting to another user's desktop is not accepted.
- Exactly one active, local, seated user session must exist for that UID. Its systemd-logind session type must be `x11`, its state must be `active`, and it must not be a display-manager greeter.
- At least one process belonging to that user and login ID must expose the real `DISPLAY` and `XAUTHORITY` in its session environment. The adapter examines only that UID's processes with a matching `XDG_SESSION_ID`. If several matching processes provide contradictory values, the probe fails.
- If logind supplies a display value, it must match the process environment. If logind leaves the display empty, the verified login's process environment can supply it. The adapter never guesses `:0` or searches for another display to use.
- The Xauthority file must be an absolute path to a regular file owned by the console UID, with no group or other permissions. Symlinks are rejected. The adapter verifies actual access to the existing X server with `xdpyinfo` before reporting readiness.
- A freshly booted guest reaches these conditions after it accepts key-only SSH, not with it. Measured on a `pilot-ubuntu-base` clone (2026-09-22): SSH first answered at boot+121s, the probe reported `active_console_session_required` for a further 9s and `x11_environment_unavailable` for 2s, and reported ready at boot+132s. A single-shot probe at SSH readiness therefore fails on a correct image, which is why preparation carries a bounded wait.
- A Wayland login is unsupported by this adapter. The image owner may maintain an explicit X11 image profile, but the adapter never changes a running session, starts Xvfb or Xvnc, or selects an independent remote-login desktop.

### Authentication and server restrictions

- The adapter starts x11vnc with `-inetd`, `-once`, `-viewonly`, `-norc`, `-noremote`, and `-nocmds`. It supplies the verified display and Xauthority file explicitly.
- A private socket pair connects x11vnc to the SSH stream. Inetd mode does not create a guest VNC TCP listener.
- `-viewonly` denies input at the server, rather than relying on a viewer preference. Selection and clipboard operations are disabled, and file-transfer extensions are not enabled. x11vnc documents that view-only mode also disables its file-transfer operations.
- The adapter uses the documented `-passwdfile rm:/absolute/path` interface. The first line contains the host-generated password; the `rm:` prefix instructs x11vnc to unlink the file after reading it.
- The password file is created with mode `0600` inside a newly created mode `0700` temporary directory. Neither the password nor a command containing it is passed in argv or the environment. `-storepasswd password file` is deliberately not used because it would expose the password in argv.
- Normal stream closure and handled signals terminate and reap the owned x11vnc process group and remove the temporary directory, including an unread password file after startup failure. The adapter does not claim that cleanup can run after SIGKILL or a guest crash. Image maintenance must remove abandoned `vm-console-*` temporary directories after interrupted execution, without touching a live adapter's directory.
- The host must deliver the same password to its supported viewer through a verified private interface. Neither a VNC URL containing the password nor a command-line password is acceptable.

The upstream [x11vnc option reference](https://github.com/LibVNC/x11vnc/blob/master/doc/OPTIONS.md) documents `-inetd`, `-viewonly`, `-passwdfile`, the `rm:` prefix, and password-file control strings. Packaging and real authentication still require acceptance with the pinned guest version.

## macOS image and clone preparation

### Supported Screen Sharing configuration

- The image must contain Apple's standard `ioreg`, `launchctl`, `sysctl`, `ps`, `ifconfig`, `pfctl`, and `lsof` tools at the paths used by the adapter.
- The image must use Apple's built-in Screen Sharing, not Remote Management. The adapter rejects Apple's Remote Management launch condition and a running ARDAgent, and separately checks the Screen Sharing launchd job and listener owner.
- The guest must already have a graphical login for the same non-root user and UID as the SSH account. The adapter cannot create or choose a new login.
- In the guest's supported System Settings interface, use General → Sharing → Screen Sharing and authorize the intended guest account. Keep Remote Management off. Do not enable a third-party VNC password for this adapter.
- Establish guest network filtering before enabling Screen Sharing. Preparation through Settings may require a human or an approved image-management process. An SSH `kickstart` command is not substituted for the supported Settings workflow.
- Apple's Screen Sharing viewer performs guest-account authentication interactively. The service does not inject a server password. Required consent and login prompts remain explicit pending steps.
- The viewer must use Standard sharing of the existing console. It must not select a separate Log In session or a High Performance virtual display. The adapter's login checks do not prove which choice a human makes in the viewer.
- The macOS probe reports `view_only: false`. Observe mode in Apple's viewer is not server-enforced denial of control, and this backend makes no such claim.

### PF policy candidate, version 2

The supported parser recognizes a narrow, inspectable policy instead of attempting to prove arbitrary PF configurations. An administrator can provision it in an authorized disposable clone. No command below is executed automatically by the adapter.

1. Keep Screen Sharing and Remote Management off while preparing the clone. Retain the image's original PF configuration so the operator can roll back changes.
2. Use a dedicated guest profile that does not require PF address translation. The active output of `sudo /sbin/pfctl -s nat` must be empty. Every `nat`, `rdr`, or `binat` rule and every translation-anchor reference is unsupported, including ordinary Apple translation anchors and references to empty or nested anchors. The adapter does not traverse anchors or assume that unreadable children are safe. If removing translation would break a guest dependency, this backend is unavailable for that profile; do not disable unrelated functionality merely to pass the probe. Through the authorized image-management process, preserve applicable normalization and filter configuration, and insert this rule as the **first top-level filter rule**, before every filter anchor or other filter rule:

   ```pf
   block drop in quick on ! lo0 proto tcp from any to any port = 5900
   ```

3. Do not put the rule only inside an anchor. The adapter requires the effective top-level filter output to begin with that rule. A pair of otherwise identical `inet` and `inet6` rules is also accepted if those are the first two filter rules.
4. Remove any filtering bypass for non-loopback interfaces or interface groups. `set skip on lo0` is allowed. The adapter rejects a skipped `en0`, `utun` interface, `all` group, or any other non-loopback interface or group, and requires PF's interface listing to cover the current interface inventory.
5. Validate and load the guest configuration while Screen Sharing remains off. These are guest administrator operations, not host firewall commands:

   ```sh
   sudo /sbin/pfctl -n -f /etc/pf.conf
   sudo /sbin/pfctl -f /etc/pf.conf
   sudo /sbin/pfctl -E
   ```

6. Inspect existing PF states before enabling sharing. Existing non-loopback states that mention TCP port 5900 are rejected by the adapter because they can outlive a rule change. Reconcile those connections under the image owner's normal firewall procedure; the adapter never flushes states automatically.
7. Grant the console user only the fixed read-only inspection commands listed below. Check the candidate sudoers file with `visudo -cf` before installing it through the image pipeline.
8. Enable Screen Sharing through Settings only after verifying the loaded policy. Run the adapter probe through the same account and SSH identity that the service will use.
9. Test direct connection rejection from the host and a neighboring guest for both IPv4 and IPv6, and test the guest-loopback connector separately. Retain evidence from outside the guest. A rule listing or a local successful connection cannot substitute for these tests.

The adapter reads the active kernel configuration, not `/etc/pf.conf` or a provisioning marker. It requires enabled PF, an empty translation ruleset, the first-rule restriction, interface coverage without non-loopback skip flags, and no pre-existing non-loopback state involving port 5900. Translation inspection must succeed; missing permission or unreadable output is not treated as an empty ruleset. This restriction is necessary because an `rdr pass` rule can bypass filter evaluation and expose the loopback service through another external port even when the state table is initially empty. Filter anchors after the initial quick block can remain, but no translation-anchor entry is accepted. The result uses `isolation.policy_checks_passed: true`, not a claim of verified isolation. Its `isolation.external_acceptance` value remains `unverified`; those local checks cannot establish external acceptance by themselves.

For a dedicated guest with no other PF requirements, this minimal candidate has no translation rules or anchors. It is an explicit image policy, not a command to overwrite a general-purpose guest's configuration:

```pf
set skip on lo0
block drop in quick on ! lo0 proto tcp from any to any port = 5900
```

An image owner must review any additional rules, load the candidate while sharing is disabled, and confirm that `pfctl -s nat` still produces empty output. The adapter rejects any nonempty translation output, including unknown syntax, rather than trying to identify only dangerous translations.

The following sudoers example uses `guest` as a placeholder for the actual console account. Colons in command arguments are escaped for sudoers syntax. The command paths and argument order must match the adapter exactly.

```sudoers
Cmnd_Alias VM_CONSOLE_INSPECT = /sbin/pfctl -s info, \
    /sbin/pfctl -s rules, \
    /sbin/pfctl -s nat, \
    /sbin/pfctl -v -s Interfaces, \
    /sbin/pfctl -s states, \
    /usr/sbin/lsof -nP -iTCP\:5900 -sTCP\:LISTEN -Fpcun
guest ALL=(root) NOPASSWD: VM_CONSOLE_INSPECT
```

- The adapter uses `sudo -n` and fails closed if a grant is missing. Do not grant arbitrary `pfctl`, arbitrary shell commands, or unrestricted passwordless sudo to satisfy a failed probe.
- The loopback connector's destination is fixed to `127.0.0.1:5900`. A caller cannot supply a host, port, alternate server, or shell command.
- The adapter verifies the system Screen Sharing job's executable and checks that TCP port 5900 listeners belong to root's launchd or the expected Apple Screen Sharing executable. A different service listening on that port is rejected.
- The connector carries Apple's native RFB authentication unchanged. It does not accept or synthesize a VNC password on macOS.

### Restart and exposure limitations

- A manually prepared clone is not automatically a persistent, accepted image profile. A launchd job that loads PF at boot has no implicit ordering guarantee relative to Screen Sharing, so adding a `RunAtLoad` plist alone does not prove safe startup.
- For the manual preparation profile, keep Screen Sharing disabled in the base image, enable it only after loading and checking PF in the clone, and disable it through Settings before reboot or shutdown. Repeat preparation after boot. Do not reboot a prepared clone with sharing enabled and assume that an eventual successful probe proves there was no startup exposure.
- A reusable image profile must separately establish reliable filtering before sharing becomes reachable and must pass external IPv4 and IPv6 tests after restart. Until that ordering and acceptance are demonstrated, advertise persistent macOS support as unverified rather than weakening the checks.
- Closing the SSH connector revokes that managed connection. It does not stop Apple's built-in Screen Sharing server, revoke the guest account, or prove that some other local client has disconnected. Guest-local access and a guest administrator remain outside the managed stream's revocation boundary.
- NAT, a host-loopback viewer address, a configuration marker, and an enabled Screen Sharing checkbox are not independent proof of guest network isolation.

Apple documents [Screen Sharing configuration](https://support.apple.com/guide/mac-help/turn-screen-sharing-on-or-off-mh11848/mac), [the standard sharing workflow](https://support.apple.com/guide/mac-help/share-the-screen-of-another-mac-mh14066/mac), and [kickstart limitations](https://support.apple.com/guide/remote-desktop/use-the-kickstart-command-line-utility-apd8b1c65bd/mac). The guest's `pf.conf(5)`, `pfctl(8)`, and `sudoers(5)` manuals define the filtering and inspection interfaces.

## Offline tests and remaining acceptance

Run only the adapter unit tests with:

```sh
cargo test -p guest-console-agent
```

- The tests exercise the adapter for the wrong platform where they need to, so that local sharing services cannot be inspected. Platform readiness checks otherwise use fixtures and mocked commands.
- The tests verify configuration framing, binary transparency, bounded buffering, EOF revocation, deadlines, session changes, password-file permissions, startup-failure cleanup, fixed listener selection, and PF rejection cases.
- No unit result substitutes for authenticating against real x11vnc or Apple's Screen Sharing, verifying input denial where claimed, or confirming that the viewer and the agent see the same changing desktop.
- The complete delivery must satisfy [the current design](design.md), [the console contract](console-contract.md), and [the live acceptance specification](vnc-acceptance.md), separately for each platform.
