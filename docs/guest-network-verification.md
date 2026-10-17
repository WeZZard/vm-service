# Guest network live verification (DC-13)

## Current status

- **The `internet` network does not yet meet DC-13 on this host.** The guest reaches every TCP port that the host serves on the host's address on the VM network, including the installed vm-service on port 6240.
- The guest cannot reach the host's home-network address or any other private address under `internet`.
- The host still reaches the guest over SSH under `internet`, so acquisition, execution and release work.
- Name-based internet access fails on this host under `internet`, because the host's DNS answers with addresses that Softnet drops. Internet access by address and through DNS over HTTPS works.
- Tart offers one way to close the host's address, `--net-softnet-block=@host`. That option also drops the guest's replies to the host's SSH connections, so the service cannot provision a guest with it. The implementation does not use it.

## Response

- DC-14 closes the gap with a pf anchor that drops the connections a Softnet guest opens to the host's own addresses, public DNS resolvers for the guest, and an isolation check before every `internet` lease is ready. The design is in [the acquisition contract](acquisition-contract.md#guest-network).
- The results below were taken before DC-14 was built. They remain the record of what stock Softnet allows.

## Setup

- The host is this Mac, with Tart 2.32.1 and Softnet 0.19.0 at `/opt/homebrew/bin/softnet`. That path is a symlink to a root-owned setuid file in Homebrew's Cellar.
- The installed vm-service on port 6240 was left running and unchanged. It held no leases before or after the test.
- A second instance, built from commit `2e0aab5` on branch `guest-internet-only`, ran on `127.0.0.1:6260` with its own state directory, started with `LIVE_PORT=6260 scripts/live-instance.sh start ssh` (DC-5).
- Both leases used the Linux image `ubuntu2404` with `env: none`. Each lease was released after its checks, and the second instance was stopped.
- The host's home-network address is `192.168.50.3` on `en1`, and its default router is `192.168.50.2`.
- The host's DNS resolver is Surge at `198.18.0.2`, which answers names with addresses in `198.18.0.0/15`.

## Procedure

- The leases were acquired with `POST /acquire` and the body `{"purpose":"dc13-internet","image":"ubuntu2404","env":"none","ttl_hours":2,"network":"internet"}`, and the same body without `network` for NAT.
- `GET /acquisition-capabilities` on the second instance reported `{"values":["nat","internet"],"internet":{"available":true,"runtime":"tart-softnet"}}`.
- The `internet` lease ran `tart run pilot-dc13-internet-774794 --no-graphics --net-softnet`, and Tart started `softnet --vm-fd 0 --vm-mac-address 2e:7a:a2:ad:bf:fe`.
- The NAT lease ran `tart run pilot-dc13-nat-0c5b1a --no-graphics`.
- Each check ran in the guest through `POST /vms/<vm>/exec` with a script, so check 1 is the success of that request.
- A TCP probe is `timeout 4 bash -c "exec 3<>/dev/tcp/<address>/<port>"`. "Open" means the handshake completed, "refused" means a reset came back, and "timeout" means no answer within 4 seconds.

## Results

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

## Why the gateway is reachable

- Softnet is a stateless packet filter. For a packet from the guest, it applies the `--allow` and `--block` rules first, by longest prefix match on the destination.
- Without a matching rule, Softnet passes globally routable destinations, the vmnet bridge's gateway address, and DNS requests to the DNS servers that DHCP gave the guest. Softnet's source states that it passes the gateway address so that SSH to the guest works.
- Softnet drops every other destination, which is why the private ranges and the `198.18.0.0/15` answers time out.
- Tart's `--net-softnet` help text documents the same default: the guest may "send traffic to gateway IP of the vmnet bridge".

## The one way Tart offers to close the gateway

- Tart passes `--net-softnet-block` to Softnet unchanged. Softnet accepts the alias `@host`, which names the bridge's gateway address.
- A scratch clone of `pilot-ubuntu-base`, named `dc13-probe-block`, ran `tart run dc13-probe-block --no-graphics --net-softnet-block=@host`, and Tart started `softnet ... --block @host`.
- The guest received the address `192.168.2.5` from DHCP, as `tart ip` reported.
- The host's `nc -z -G 5 192.168.2.5 22` failed repeatedly for more than four minutes after boot. In the same period, the same probe to the `internet` guest at `192.168.2.4` succeeded.
- The host's SSH connections come from the gateway address, so the block also drops the guest's replies to them. Softnet has no connection tracking that would let replies through.
- The same block would also drop the guest's DNS requests, because the DNS server that DHCP gives the guest is the gateway address.
- The scratch clone was stopped and deleted after the probe.

## What remains

- DC-13 holds only if the host's own services become unreachable from the guest while the host keeps a way to reach the guest. Stock Tart 2.32.1 with Softnet 0.19.0 cannot provide both.
- A host link that does not use the guest's network, such as Tart's guest agent over vsock (DC-3), would allow `--net-softnet-block=@host`. That design would also need a DNS resolver for the guest that is not the gateway.
- A packet filter that tracks connections on the host, such as the repository's unaccepted `patches/softnet-control-only.patch` or a pf rule on the bridge interface, would allow replies while dropping new connections from the guest.
- Name resolution through the host's Surge DNS fails under Softnet on this host. A guest resolver that does not answer with `198.18.0.0/15` addresses, or an allow rule for that range, is needed for names to work. An allow rule for that range would send guest traffic through Surge, which can connect to private addresses on the guest's behalf.
- Binding the host's services to loopback, as the installed vm-service supports, removes them from the gateway address. This step reduces exposure but does not satisfy DC-13 for services that must listen on all addresses.
