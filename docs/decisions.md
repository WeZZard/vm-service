# Decisions

This list records owner decisions. The linked documents carry the design and the rationale.

## 2026-09-27: lease lifecycle fixes

- Execution timeouts are capped at 4200 seconds, which is above the largest request that mcp-vm-relay sends. See [Lease lifecycle fixes](lifecycle-fixes.md#v4-bounded-execution-timeout).
- A heartbeat without `ttl_hours` renews the lease for its own initial TTL. See [V5](lifecycle-fixes.md#v5-a-heartbeat-without-ttl_hours).
- A heartbeat on a `running` lease whose VM is not running fails, and GC reclaims such a lease after two consecutive passes. See [V2](lifecycle-fixes.md#v2-a-running-record-whose-vm-is-gone).
- At startup the daemon releases `pending` and `provisioning` records left by a previous process. See [V1](lifecycle-fixes.md#v1-startup-reconciliation).
- The daemon releases a new lease whose successful acquisition response cannot be delivered. See [V3](lifecycle-fixes.md#v3-an-acquisition-whose-client-has-gone).
- The 6-hour grace period and the macOS slots held by grace leases are unchanged until the owner decides. See [V6](lifecycle-fixes.md#v6-grace-period-and-macos-capacity-owner-decision-pending).
