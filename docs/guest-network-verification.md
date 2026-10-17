# Guest network live verification (DC-13)

## Current status

- **With the guest firewall of commit `88aff2d`, an `internet` guest reaches neither the host nor any private address.** Every probe of the gateway timed out, and the lease's isolation check passed.
- **Check 2 by name still fails.** The host's Surge intercepts plain DNS on UDP port 53 to any resolver and answers with `198.18.0.0/15` addresses, which Softnet drops. DNS over TLS to the same public resolvers returns real addresses, and the site then loads by name.
- **The guest firewall also cuts off NAT guests while a Softnet guest runs.** A NAT guest started while a Softnet guest was running joined the same vmnet subnet, `192.168.2.0/24`. The anchor blocks by subnet, so that NAT guest lost the host, including the gateway's DNS and with it internet access by name.
- DC-13 and DC-14 are therefore not yet met on this host. The first two runs below are the record of what stock Softnet and the first anchor allowed.

## Response

- DC-14 adds a pf anchor that drops the connections a Softnet guest opens to the host, public DNS resolvers for the guest, and an isolation check before every `internet` lease is ready. The design is in [the acquisition contract](acquisition-contract.md#guest-network).

## Second run with the guest firewall

- The owner reinstalled the anchor from commit `88aff2d`. Its rules are `pass out quick inet to $softnet_guests keep state`, `pass in quick inet proto udp from $softnet_guests port 68 to any port 67`, and `block drop in quick inet proto { tcp udp } from $softnet_guests to { $softnet_guests self }`, with `softnet_guests = "192.168.2.0/24"`.
- A fresh second instance ran on `127.0.0.1:6260`, built from `88aff2d`. The installed vm-service on port 6240 was not touched and held no leases.
- The `internet` lease `pilot-dc14-internet-1b6a6e` got `192.168.2.9` and was ready after 2 minutes 14 seconds. The service log shows "guest resolvers set to 1.1.1.1, 8.8.8.8" and "isolation verified; 192.168.2.1:51858 on this host did not answer the guest".
- Its record reported `"network": "internet"`, `"guest_dns": ["1.1.1.1", "8.8.8.8"]`, and `"isolation": {"status": "verified", "gateway": "192.168.2.1", "probe_port": 51858, "firewall_subnet": "192.168.2.0/24", "guest_dns": ["1.1.1.1", "8.8.8.8"], "observed_at": 1792239576.68}`.
- The NAT lease `pilot-dc14-nat-cf096c` ran while the `internet` lease was running. It got `192.168.2.10`, on the Softnet subnet, with gateway `192.168.2.1`.
- During each guest's checks, a listener on the host accepted connections on all addresses on port 47811. It recorded no connection from either guest.

| # | Check | Command in the guest | `internet` | NAT, while a Softnet guest runs |
|---|-------|----------------------|------------|---------------------------------|
| 1 | The host reaches the guest | `POST /vms/<vm>/exec` with the check script | rc 0 | rc 0 |
| 1 | The host reaches the guest after 10 minutes idle | A persistent `ssh -o TCPKeepAlive=no -o ServerAliveInterval=0` session, idle from 20:22:43 to 20:32:57, then one command; and `POST /vms/<vm>/exec {"argv":["uname","-n"]}` at 20:33:14 after 10 minutes without requests | The session answered `after-idle: ubuntu`; exec rc 0, `ubuntu` | Not run |
| 2 | An internet site loads by name | `curl -sI -m 15 https://example.com` | **Fails**: `example.com` resolves to `198.18.35.15` through `1.1.1.1` and `8.8.8.8`, and the connection times out | **Fails**: the gateway's resolver is blocked, so the name does not resolve |
| 2 | Plain DNS to public resolvers | `dig @1.1.1.1 example.com`; `dig @8.8.8.8 example.com`; `dig +tcp @9.9.9.9 example.com` | "host unreachable"; `198.18.35.15`; `104.20.23.154 172.66.147.243` | Not run |
| 2 | DNS over HTTPS, as a diagnostic | `curl -sI --doh-url https://1.1.1.1/dns-query https://example.com` | `HTTP/2 200` | Not run |
| 2 | DNS over TLS, as a diagnostic | A temporary resolved drop-in with `DNSOverTLS=yes` and `DNS=1.1.1.1#cloudflare-dns.com 8.8.8.8#dns.google`, then `curl -sI https://example.com`; removed afterwards | `104.20.23.154`, then `HTTP/2 200` | Not run |
| 3 | The gateway on 443, 22, 6240, 80, 53, 6420, 8443 and 47811 | TCP probe `192.168.2.1:<port>` | Timeout on every port | **Timeout on every port** |
| 3 | The gateway's resolver | `dig @192.168.2.1 example.com` | Timed out | **Timed out** |
| 3 | The vm-service isolation probe | The service's probe of `192.168.2.1:51858` | Timeout; verified | Not run for NAT |
| 4 | The host's home-network address | TCP probes `192.168.50.3` on 443, 22 and 6240 | Timeout | **Timeout** |
| 5 | The home router and other private ranges | TCP probes `192.168.50.1:443`, `192.168.50.1:80`, `192.168.50.2:443`, `192.168.50.2:6152`, `10.0.0.1:443`, `172.16.0.1:443` | Timeout | Refused, open, refused, open, open, open |

- The NAT guest's results differ from NAT before the firewall in checks 2, 3 and 4. Before the firewall, a NAT guest alone on `192.168.64.0/24` reached the gateway, the home-network address, and the internet by name.
- A NAT lease during the first firewall run, while no Softnet guest ran, got `192.168.64.2`. That subnet is outside the anchor's, so the current anchor would not match it either; this was not rerun with the current anchor.
- vmnet puts NAT and Softnet guests on one shared subnet when they run at the same time, so a rule keyed on the subnet cannot tell them apart. Whichever guest starts first appears to choose the subnet. A Softnet guest that joins a NAT guest's `192.168.64.0/24` would fail its isolation check, because the anchor covers `192.168.2.0/24`.
- The installed anchor stays in force for every vmnet guest on `192.168.2.0/24`, including NAT leases of the installed vm-service on port 6240.

## First run with the guest firewall

- The owner installed the first anchor on 2026-10-17 at 20:05. Its block rule was `block drop in quick inet proto { tcp udp } from $softnet_guests to (self)`, and its state rule was `pass out quick inet from (self) to $softnet_guests keep state`.
- The installer confirmed that pf was enabled and the anchor was loaded, and the LaunchDaemon's log shows the anchor load exiting 0.
- An `internet` acquisition of `ubuntu2404` on a second instance on port 6260 set the guest resolvers to `1.1.1.1, 8.8.8.8`. The isolation check then failed with: "guest isolation check failed: the guest connected to this host at 192.168.2.1:64427; the pf anchor com.apple/250.vm-service for 192.168.2.0/24 is not in force". The service released the lease and deleted its clone.
- The likely cause is the dynamic address `(self)`. macOS pf has no interface groups, so `(self)` most likely resolves to no address, and neither rule matched. Confirming it needs root, which this session does not have.
- The anchor now names the subnet itself, which holds the gateway address, and pf's static `self`, which `pfctl` expands each time the anchor loads. This version needs a reinstall by root before the next run.
- A NAT lease on the same instance, with the first anchor loaded, gave the same results as the NAT column below. It reached the gateway on 443, 22, 6240, 80, 53, 6420, 8443 and a host listener on port 47811, and it reached the home network.

## Run before the guest firewall

### Setup

- The host is this Mac, with Tart 2.32.1 and Softnet 0.19.0 at `/opt/homebrew/bin/softnet`. That path is a symlink to a root-owned setuid file in Homebrew's Cellar.
- The installed vm-service on port 6240 was left running and unchanged. It held no leases before or after the test.
- A second instance, built from commit `2e0aab5` on branch `guest-internet-only`, ran on `127.0.0.1:6260` with its own state directory, started with `LIVE_PORT=6260 scripts/live-instance.sh start ssh` (DC-5).
- Both leases used the Linux image `ubuntu2404` with `env: none`. Each lease was released after its checks, and the second instance was stopped.
- The host's home-network address is `192.168.50.3` on `en1`, and its default router is `192.168.50.2`.
- The host's DNS resolver is Surge at `198.18.0.2`, which answers names with addresses in `198.18.0.0/15`.

### Procedure

- The leases were acquired with `POST /acquire` and the body `{"purpose":"dc13-internet","image":"ubuntu2404","env":"none","ttl_hours":2,"network":"internet"}`, and the same body without `network` for NAT.
- `GET /acquisition-capabilities` on the second instance reported `{"values":["nat","internet"],"internet":{"available":true,"runtime":"tart-softnet"}}`.
- The `internet` lease ran `tart run pilot-dc13-internet-774794 --no-graphics --net-softnet`, and Tart started `softnet --vm-fd 0 --vm-mac-address 2e:7a:a2:ad:bf:fe`.
- The NAT lease ran `tart run pilot-dc13-nat-0c5b1a --no-graphics`.
- Each check ran in the guest through `POST /vms/<vm>/exec` with a script, so check 1 is the success of that request.
- A TCP probe is `timeout 4 bash -c "exec 3<>/dev/tcp/<address>/<port>"`. "Open" means the handshake completed, "refused" means a reset came back, and "timeout" means no answer within 4 seconds.

### Results

The guest's default gateway was `192.168.2.1` under `internet` and `192.168.64.1` under NAT.

| # | Check | Command in the guest | NAT | `internet` |
|---|-------|----------------------|-----|------------|
| 1 | The host reaches the guest over SSH | `POST /vms/<vm>/exec` with the check script | rc 0 | rc 0 |
| 2a | An internet site loads by name | `curl -sI -m 15 https://example.com` | `HTTP/2 200` | No response; the name resolves to `198.18.35.15`, and the connection times out |
| 2b | An internet site loads through DNS over HTTPS | `curl -sI -m 15 --doh-url https://1.1.1.1/dns-query https://example.com` | `HTTP/2 200` | `HTTP/2 200` |
| 2c | An internet address answers | `curl -sS -m 10 https://1.1.1.1` | Not run | HTTP 301 |
| 3 | The gateway's TCP 443 | TCP probe `<gateway>:443` | Open | **Open** |
| 3 | The gateway's TCP 22 | TCP probe `<gateway>:22` | Open | **Open** |
| 3 | The gateway's TCP 6240 | TCP probe `<gateway>:6240`, then `curl -s -m 5 http://<gateway>:6240/health` | Open; `{"ok": true, "service": "vm-service"}` | **Open; `{"ok": true, "service": "vm-service"}`** |
| 3 | Other gateway ports | TCP probes to 80, 53, 6420 and 8443 | Open | **Open** |
| 4 | The host's home-network address | TCP probes `192.168.50.3` on 443, 22 and 6240 | Open | Timeout |
| 5 | The home router | TCP probes `192.168.50.1:443`, `192.168.50.1:80`, `192.168.50.2:443`, `192.168.50.2:6152` | Refused, open, refused, open | Timeout |
| 5 | Other private ranges | TCP probes `10.0.0.1:443` and `172.16.0.1:443` | Open | Timeout |
| 5 | Surge's DNS answer range | TCP probe `198.18.35.15:443` | Open | Timeout |

- Under NAT, `10.0.0.1:443` and `172.16.0.1:443` report "open" although nothing at those addresses is known to serve. The host's Surge enhanced mode most likely accepts these connections itself. The result shows that the traffic leaves the guest, not that a server answered.
- The gateway's port 6240 is the installed vm-service, which listens on all addresses. A guest on either network can therefore call the installed service's HTTP API.
- The host listens on all addresses on 443 and 80 (Caddy), 6240 (vm-service), 6420 (Node.js), 8443 (zellij), 22 (sshd), and 5000 and 7000 (ControlCenter). Every probed port answered from the `internet` guest through the gateway. Ports 5000 and 7000 were not probed, but the same Softnet rule passes them.

### Why the gateway is reachable

- Softnet is a stateless packet filter. For a packet from the guest, it applies the `--allow` and `--block` rules first, by longest prefix match on the destination.
- Without a matching rule, Softnet passes globally routable destinations, the vmnet bridge's gateway address, and DNS requests to the DNS servers that DHCP gave the guest. Softnet's source states that it passes the gateway address so that SSH to the guest works.
- Softnet drops every other destination, which is why the private ranges and the `198.18.0.0/15` answers time out.
- Tart's `--net-softnet` help text documents the same default: the guest may "send traffic to gateway IP of the vmnet bridge".

### The one way Tart offers to close the gateway

- Tart passes `--net-softnet-block` to Softnet unchanged. Softnet accepts the alias `@host`, which names the bridge's gateway address.
- A scratch clone of `pilot-ubuntu-base`, named `dc13-probe-block`, ran `tart run dc13-probe-block --no-graphics --net-softnet-block=@host`, and Tart started `softnet ... --block @host`.
- The guest received the address `192.168.2.5` from DHCP, as `tart ip` reported.
- The host's `nc -z -G 5 192.168.2.5 22` failed repeatedly for more than four minutes after boot. In the same period, the same probe to the `internet` guest at `192.168.2.4` succeeded.
- The host's SSH connections come from the gateway address, so the block also drops the guest's replies to them. Softnet has no connection tracking that would let replies through.
- The same block would also drop the guest's DNS requests, because the DNS server that DHCP gives the guest is the gateway address.
- The scratch clone was stopped and deleted after the probe.

### What remained after this run

- DC-13 holds only if the host's own services become unreachable from the guest while the host keeps a way to reach the guest. Stock Tart 2.32.1 with Softnet 0.19.0 cannot provide both.
- A host link that does not use the guest's network, such as Tart's guest agent over vsock (DC-3), would allow `--net-softnet-block=@host`. That design would also need a DNS resolver for the guest that is not the gateway.
- A packet filter that tracks connections on the host, such as the repository's unaccepted `patches/softnet-control-only.patch` or a pf rule on the bridge interface, would allow replies while dropping new connections from the guest.
- Name resolution through the host's Surge DNS fails under Softnet on this host. A guest resolver that does not answer with `198.18.0.0/15` addresses, or an allow rule for that range, is needed for names to work. An allow rule for that range would send guest traffic through Surge, which can connect to private addresses on the guest's behalf.
- Binding the host's services to loopback, as the installed vm-service supports, removes them from the gateway address. This step reduces exposure but does not satisfy DC-13 for services that must listen on all addresses.
