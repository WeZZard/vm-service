# Acquisition options and configuration reporting

## Status

- **The replacement is implemented in working source.** It uses the new pure `acquisition_options.py` module and guest-sharing controller, not the withdrawn patched-Tart implementation. Deployment and actual guest/viewer acceptance remain separate.
- Existing acquisition, selected environments, SSH readiness, resource settings, execution, transfers, heartbeat, and release remain available under their pre-task contracts.
- The replacement must work with unmodified upstream Tart. Guest sharing prerequisites and the normal deployment procedure are part of VNC support, not optional steps after delivery.

## Read-only discovery

- `GET /acquisition-capabilities` and `vmctl acquisition-capabilities` describe supported options without changing the existing networking-only `/capabilities` response.
- The response will include a schema version, option definitions, startup image-configuration metadata, viewer location, and explicit availability reasons.
- Option definitions must distinguish required fields, unconditional defaults, omission, accepted nulls, aliases, and conditional applicability. For example, presence-sensitive fields must not advertise null as an omission substitute.
- A startup image snapshot records configuration, not live capacity, image existence, an authenticated desktop, or a verified guest session.
- VNC availability must distinguish a compatible unmodified runtime, guest-sharing preparation, supported console-session type, configured viewer, and any required user authentication step. An image name alone is insufficient.
- Discovery must not invoke Tart, create a VM, generate credentials, start a guest server, establish SSH, open a listener, or launch a viewer. Failed startup inspection remains unavailable or unknown rather than being retried with side effects on GET.
- The existing selected-environment fingerprint policy remains in force. Discovery must use the selected environment rather than unrelated ambient configuration.

## Acquisition request and compatibility

- The additional boolean `vnc` defaults to false. False preserves ordinary VM acquisition; true requests guest-console preparation on a fresh lease without automatically opening a viewer.
- Existing field names and aliases, resource defaults, selected-store checks, environment-pack behavior, and source-image validation must remain compatible.
- Omitted CPU and memory resolve to the image line's configured `CPU` and `MEMORY_MB` (`line.conf`). A line that does not configure a value falls back to 6 CPUs and 16384 MB. An omitted disk override leaves the cloned disk unchanged; reporting must not invent a measured disk size.
- Resource validation accepts positive integers or null and rejects booleans, nonpositive values, fractional values, and strings before reservation. This tightens previously accidental invalid-input coercion while preserving valid values and defaults.
- TTL validation retains the 0.1–720-hour range and numeric-string compatibility while rejecting non-finite values and invalid types.
- `env: none` suppresses the application credential pack, not required per-lease SSH-key preparation. VNC must not require injecting the default application credentials.
- Acquisition remains synchronous. The existing shorter `wait: false` readiness path is not a background job and must not be described as immediate return before guest access is ready.
- For VNC, the service must verify the actual guest console and adapter prerequisites after existing SSH readiness. A missing graphical session, unsupported Wayland environment, failed macOS sharing policy, or incompatible stock boot produces a specific failure and rollback outcome.
- The service must not silently enable stock experimental VNC, switch desktops, modify a golden image, relax networking policy, or use a patched runtime as fallback.

## Guest network

- `network` is optional. `nat`, the default, keeps today's NAT: the guest reaches the internet, the host and the host's private networks (DC-13).
- `internet` starts the guest with `tart run --net-softnet`. Softnet drops the guest's traffic to the private address ranges, but it passes the guest's traffic to the vmnet bridge's gateway address, which is this host. The guest firewall closes that path (DC-14).
- The guest firewall is a pf anchor, `com.apple/250.vm-service`, loaded under the default `/etc/pf.conf`'s `anchor "com.apple/*"`. It drops every TCP and UDP connection that a guest on the Softnet subnet opens to one of the host's own addresses, except DHCP renewals to the host's bootpd(8). pf keeps state for the host's own connections to the guests, so the replies to the service's SSH still pass.
- The Softnet subnet is chosen by vmnet, because Softnet passes no address to vmnet. On the host where DC-14 was designed it is `192.168.2.0/24`, shared by concurrent Softnet guests and separate from NAT's `192.168.64.0/24`. The anchor is installed for one subnet, `192.168.2.0/24` unless the installer is given `--subnet`.
- `vm-service-guest-firewall install` writes the anchor to `/etc/pf.anchors/com.wezzard.vm-service` and the LaunchDaemon `com.wezzard.vm-service.guest-firewall`, which enables pf with `pfctl -E` and loads the anchor at boot and every 300 seconds. The installer needs root once. `vm-service-guest-firewall remove` unloads and deletes both and leaves pf enabled.
- An `internet` guest gets public DNS resolvers, `VM_SERVICE_GUEST_DNS`, by default `1.1.1.1,8.8.8.8`, because the gateway's resolver is this host. Each resolver must be a public IPv4 address, because Softnet drops IPv6 and private destinations. A Linux guest gets them through a systemd-resolved drop-in with the routing domain `~.`, or `/etc/resolv.conf` without systemd-resolved. A macOS guest gets them through `networksetup -setdnsservers` on the default route's service.
- Before an `internet` lease is ready, the service opens a TCP listener on all of the host's addresses on an ephemeral port. The guest then tries to connect to its default gateway on that port. Only a timeout, with no connection at the listener, passes. A connection, a reset, or a probe that cannot run fails the acquisition, the lease is released, and the reason names the anchor and the install command for the guest's subnet.
- A request for `internet` fails before the reservation and the clone when Softnet is not installed as Tart needs it, when the anchor file is missing, or when `VM_SERVICE_GUEST_DNS` is invalid. Each reason names what is missing. Whether pf has the anchor loaded needs root to see, so the isolation check catches an anchor that is installed but not in force. An `internet` request never falls back to NAT.
- The lease's record and its reporting carry its network. An `internet` lease also carries `guest_dns` and its `isolation` report: the gateway, the probe port, the firewall's subnet and the observation time.
- Discovery reports whether `internet` is available on this host: Softnet is usable and the anchor file is present.
- The control-only network is a separate contract, and stays as it is.
- [The live verification](guest-network-verification.md) records what an `internet` guest reached before the guest firewall existed: every port the host serves on all addresses, through the gateway.

## New lease identity and reporting

- A new acquisition receives an immutable fresh `lease_id`. It prevents accidental name reuse in console operations but does not introduce general caller authentication.
- The `configuration` report separates `requested`, `effective`, `sources`, and `resources_applied`.
- `requested` records normalized inputs rather than raw request bodies. `effective` records the instructions selected for provisioning, not independently measured hardware or readiness.
- Resource origins distinguish service defaults, image configuration, source-image inheritance, and explicit requests. Existing top-level nullable resource fields remain unchanged for older clients.
- `resources_applied` will identify the observation source and time. A successful Tart setting command is command acknowledgment, not hardware readback or proof of subsequent boot.
- VNC reporting will name the guest-sharing backend and console-session type separately from stock Tart version and boot mode. It will contain no password, endpoint credential, or private-key location.
- Current expiration remains in the lease's existing expiration field. Initial requested lifetime must not be rewritten as though every heartbeat were a new acquisition.
- Older leases will not receive invented configuration or identifiers. Ordinary legacy operations remain usable, while new identity-dependent console operations must return unavailable for unsupported old records.

## Acceptance

- Tests must cover default preservation, aliases, explicit omission versus null, invalid values, selected environments, resource-command failure, later boot failure, and incomplete rollback.
- Discovery tests must prohibit unintended I/O and side effects, including on unavailable configurations. New response fields must remain compatible with relay's existing lease parsing.
- [The console contract](console-contract.md) defines the implemented source lifecycle and remaining observation limits. [The acceptance specification](vnc-acceptance.md) requires the actual installation and relay upgrade path to work before end-to-end delivery is declared complete.
- Earlier reporting tests and their former API implementation are preserved only in [historical documentation](history/README.md). The replacement has its own source tests; neither set establishes live viewing acceptance.
