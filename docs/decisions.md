# Decisions

This list records owner decisions. The linked documents carry the design and the rationale.

## 2026-10-07: session transport for the guest link and the client link

Entries from this date carry identifiers. The design is in [architecture](arch/index.md), the tests in [test layers](arch/test-layers.md), and the acceptance criteria in [acceptance](acceptance/index.md).

- DC-1: Migration cost is not a selection criterion. The owner asked for the best architecture, because the stack is rebuilt with AI agents at near-zero cost.
- DC-2: Every change ships with unit tests and integration tests. Each integration test runs on the layer that owns the behaviour it checks, because the change spans several layers.
- DC-3: The guest link is built behind one interface with two candidates: a guest helper over one long-lived SSH session, and a guest helper over vsock through Tart's guest agent. One conformance suite tests both. Live test results choose the default.
- DC-4: The cross-layer tests and the shared design live in this repository. mcp-vm-relay keeps its own unit tests and its fake-vm-service tests.
- DC-5: Live tests may upgrade Tart to 2.40.1, run a second vm-service instance on another port with its own state directory, add Tart's guest agent to test image variants, and send commands directly to their own test VMs.
- DC-6: Tests cover both situations: the VM link inside one Mac, and a client link that crosses the internet or an intercontinental intranet. The owner proposed WebSocket with heartbeats for the long-distance link; the design adopts it with a resumable session on top.
- DC-7: The owner leaves the technical choices in this work to the agent. The agent decides, writes the reason into the design, and reports its concerns, their consequences and its actions in plain language.
- DC-8: The finish line comes from measurement, not from the owner: the work is done when the transport is the smallest part of the time between actions. The owner sets no numeric target, because nobody knows this system's limits before it is measured.
- DC-9: Network faults are tested in test VMs with network conditioning, macOS VMs included, with the test plan run by an agent.
- DC-10: The remote session listener may accept clients on other machines, under TLS and a client credential (RS-1).
- DC-11: The mcp-vm-relay changes for the new links are merged once their tests and this repository's live tests pass.
- DC-12: The faster link becomes the default when the measurements support it (DC-3, DC-8).

## 2026-09-29: image resource defaults

- An acquisition that omits CPU or memory gets the image line's configured `CPU` and `MEMORY_MB`, as `vmctl images-show` reports them. The hardcoded 6 CPUs and 16384 MB apply only when the line configures no value. The owner called ignoring the configuration a bug. See [acquisition contract](acquisition-contract.md#acquisition-request-and-compatibility).

## 2026-09-27: lease lifecycle fixes

- (Superseded later the same day.) Execution timeouts are capped at 4200 seconds, which is above the largest request that mcp-vm-relay sends. The owner did not approve this cap.
- A heartbeat without `ttl_hours` renews the lease for its own initial TTL. See [V5](lifecycle-fixes.md#v5-a-heartbeat-without-ttl_hours).
- A heartbeat on a `running` lease whose VM is not running fails, and GC reclaims such a lease after two consecutive passes. See [V2](lifecycle-fixes.md#v2-a-running-record-whose-vm-is-gone).
- At startup the daemon releases `pending` and `provisioning` records left by a previous process. See [V1](lifecycle-fixes.md#v1-startup-reconciliation).
- The daemon releases a new lease whose successful acquisition response cannot be delivered. See [V3](lifecycle-fixes.md#v3-an-acquisition-whose-client-has-gone).
- The 6-hour grace period and the macOS slots held by grace leases are unchanged until the owner decides. See [V6](lifecycle-fixes.md#v6-grace-period-and-macos-capacity-owner-decision-pending).

## 2026-09-27: release preempts guest operations

- Guest execution timeouts have no maximum. The 4200-second cap is removed. See [V4](lifecycle-fixes.md#v4-a-long-guest-operation-blocks-release-and-gc).
- A release preempts a running exec, push, or pull on its VM. The operation is killed and returns an error that says it was cancelled by release. See [V4](lifecycle-fixes.md#design-release-preempts-the-operation-lock).
- GC never blocks on one VM. It defers the teardown of a VM whose operation lock is held and reclaims it on a later pass. See [V4](lifecycle-fixes.md#design-gc-does-not-wait-on-a-busy-vm).

## 2026-10-17: guests that reach the internet only

The design is in [the acquisition contract](acquisition-contract.md#guest-network).

- DC-13: A lease may ask for the network `internet`. Its guest runs on Tart's Softnet, reaches the internet, and reaches neither the host nor any private network. NAT stays the default. The agents that run code in guests through mcp-relay must not reach the host's own services, such as an application's page served on the home network, through a guest. Decided by the agent under DC-7.
- DC-14: An `internet` guest is kept off the host by a stateful host packet filter, a pf anchor that drops the connections a Softnet guest opens to the host's own addresses while replies to the host's own connections pass. The guest uses public DNS resolvers, and every `internet` lease is checked before it is ready: a guest that reaches the host through its gateway fails the acquisition. Stock Softnet passes guest traffic to the host's gateway address, which the live test of DC-13 showed. Decided by the agent under DC-7.
- DC-15: The guest firewall blocks each `internet` guest by its own address, never by subnet. Its anchor holds a pf table of the running `internet` guests' addresses, read from one table file on the host that every vm-service instance writes. vmnet puts NAT and Softnet guests on one subnet when they run together, which the live test of DC-14 showed, so a rule keyed on the subnet cut off NAT guests. NAT guests never enter the table. The isolation check waits for the table to reload. Decided by the agent under DC-7.
- DC-16: `internet` guests resolve names over DNS over TLS to public resolvers, because a host may intercept plain DNS; this host's proxy answers UDP port 53 to any resolver with fake `198.18.0.0/15` addresses, which Softnet drops. Linux guests use systemd-resolved; macOS guests, which offer no scriptable encrypted DNS, run vm-service's own forwarder on `127.0.0.1:53`. The isolation check also requires a name to resolve to a public address and HTTPS to it to work. Decided by the agent under DC-7.

## 2026-10-18: a host whose macOS slots another program holds

- DC-18: When the hypervisor refuses a guest because the number of VMs exceeds the system limit, as its `tart run` log says, the acquisition fails with the capacity refusal, `macOS VM limit reached`, after the usual rollback, so mcp-relay waits for capacity (its DC-22) instead of failing. A macOS image's `acquirable` in `GET /images` also counts the host's Virtualization.framework guests less this service's other active guests. The gauge still never gates an acquisition, because it also counts other programs' Linux guests. On 2026-10-18 another program's macOS guest held a slot, `/images` said `macos26` was acquirable, and the acquisition failed at boot with an error the relay did not wait on. Decided by the agent under DC-7.

## 2026-10-17: a reboot keeps the application catalog

- DC-17: The application catalog compares a base's `st_ino`, `st_size` and `st_mtime_ns` with its association, and no longer its `st_dev`. macOS numbers a volume each time it mounts it, so after the reboots of 2026-10-13 and 2026-10-14 every unchanged base reported a new `st_dev` and `/applications` answered 503 with no usable inventories. Associations still record `st_dev`, so documents that pilot-images writes stay valid. Decided by the agent under DC-7.
