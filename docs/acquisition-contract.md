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
- The guest firewall is a pf anchor, `com.apple/250.vm-service`, loaded under the default `/etc/pf.conf`'s `anchor "com.apple/*"`. It holds the pf table `<vm_service_internet>`, read from a table file, of the addresses of the running `internet` guests (DC-15).
- The anchor drops every TCP and UDP connection that an address in the table opens to one of the host's own addresses, except DHCP renewals to the host's bootpd(8). pf keeps state for the host's own connections to those addresses, so the replies to the service's SSH still pass.
- The rules name each guest by its address, never by a subnet. vmnet chooses the subnet, because Softnet passes no address to vmnet, and it puts NAT and Softnet guests on one subnet when they run together. A NAT guest never enters the table, whatever subnet it lands on, so it keeps the host, the gateway's resolver and the internet.
- The rules use pf's static `self`, which `pfctl` expands each time the anchor is loaded, because macOS pf has no interface groups and the dynamic `(self)` matches no address.
- The table file is one host-wide path that every vm-service instance on the host writes, by default `/var/db/com.wezzard.vm-service/internet-guests`, set by `VM_SERVICE_GUEST_FIREWALL_TABLE`. Each entry is `<address> # <instance's state directory> <VM>`. An instance edits the file under a lock beside it and replaces it by rename.
- The service adds a guest's address to the table as soon as Tart reports it, before the lease is reported ready. It removes the address once the guest is deleted at release or rollback, and it removes its own entries whose lease is gone at startup and on each GC pass. It leaves other instances' entries alone.
- `vm-service-guest-firewall install` creates the table file, owned by the account that ran `sudo`, and writes the anchor to `/etc/pf.anchors/com.wezzard.vm-service` and the LaunchDaemon `com.wezzard.vm-service.guest-firewall`. The LaunchDaemon enables pf with `pfctl -E` and loads the anchor, with the table, at boot, whenever the table file or its directory changes, and every 300 seconds. The installer needs root once. `vm-service-guest-firewall remove` unloads and deletes the anchor and the LaunchDaemon and leaves pf enabled.
- An `internet` guest resolves names over DNS over TLS to public resolvers, `VM_SERVICE_GUEST_DNS`, by default `1.1.1.1#cloudflare-dns.com,8.8.8.8#dns.google` (DC-16). The gateway's resolver is this host, and a host may intercept plain DNS on port 53 to any resolver. Each resolver must be a public IPv4 address, because Softnet drops IPv6 and private destinations; the name after `#` is the one its certificate is checked against.
- A Linux guest gets the resolvers through a systemd-resolved drop-in with `DNSOverTLS=yes` and the routing domain `~.`, and its interface loses the DHCP resolver. A Linux guest without systemd-resolved fails the acquisition.
- macOS offers no scriptable way to point its resolver at DNS over TLS: encrypted DNS comes from a configuration profile that a person approves in System Settings, or from an app with a network extension. A macOS guest therefore gets `vm-service-dot-forwarder`, a DNS-over-TLS forwarder that vm-service uploads and runs as a LaunchDaemon on `127.0.0.1:53`, and its default route's network service is pointed at `127.0.0.1`. The forwarder checks the resolvers' certificates against Mozilla's roots, built in.
- Before an `internet` lease is ready, the service opens a TCP listener on all of the host's addresses on an ephemeral port, and the guest tries to connect to its default gateway on that port. A connection or a reset is retried for up to 15 seconds while the LaunchDaemon reloads the table. Only a timeout, with no connection at the listener, passes.
- The guest then resolves `example.com` through its system resolver and loads `https://example.com/`. The name must resolve to a public address, and HTTPS must answer with a 2xx or 3xx status.
- Any failure of these steps fails the acquisition, the lease is released, and the reason names what failed; a probe failure names the anchor, the table file and the install command.
- A request for `internet` fails before the reservation and the clone when Softnet is not installed as Tart needs it, when the anchor file is missing, does not read the table file this service writes, or the table file is not writable, when `VM_SERVICE_GUEST_DNS` is invalid, or, for a macOS image, when the forwarder is not staged beside vm-service. Each reason names what is missing. Whether pf has the anchor loaded needs root to see, so the isolation check catches an anchor that is installed but not in force. An `internet` request never falls back to NAT.
- The lease's record and its reporting carry its network. An `internet` lease also carries `guest_dns`, `firewall_table`, `firewall_address`, and its `isolation` report: the guest's address, the gateway, the probe port and attempts, the table file, the resolvers and their transport, the egress result, and the observation time.
- Discovery reports whether `internet` is available on this host: Softnet is usable, and the anchor reads the table file this service writes.
- The control-only network is a separate contract, and stays as it is.
- [The live verification](guest-network-verification.md) records each run: before the guest firewall, with the subnet-keyed anchors of DC-14, and with the table of DC-15.

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
