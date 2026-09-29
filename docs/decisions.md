# Decisions

This list records owner decisions. The linked documents carry the design and the rationale.

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
