# Private Softnet packet source — NOT DEPLOYABLE

Pinned base: openai/softnet `e5fd48cf033ed0ec376710607187d761e60c2374`.
Patch SHA-256: `c1ce073c3b036cb966cd1ded4efc14ba30e84f94a94674cf917c8f23b5524dfa`.

Apply with `git apply --check softnet-control-only.patch` then `git apply` to
that clean base. Seven patched files were compared byte-for-byte with the source
checkout after clean application; reverse applicability also passed.

## Safe source-only verification

The repository `.cargo/config.toml` sets its test runner to **sudo -E**.
Always override it explicitly; never run unrestricted upstream tests (some open vmnet).
The installed stable used here is rustc/cargo 1.98.1; no compiler was installed.

```sh
CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=/usr/bin/env \
  rustup run stable cargo test --offline --locked \
  --features vm-service-control-only --lib proxy::control_only -- --nocapture
rustup run stable cargo check --offline --locked --features vm-service-control-only
rustup run stable cargo check --offline --locked
```

11 pure packet-policy tests pass; 67 other tests were filtered out. Locked public
Cargo dependencies were fetched in the first compilation, then commands ran offline.
Lockfile is unchanged. See minelogue3 `docs/verification/packet-policy-compilation.md`
and its evidence directory for full logs, including the failed initial sudo-runner
attempt and the fixture correction. No privileged helper or VM ran.

## Policy and remaining gates

Private policy now has a validated interface-config input (gateway IP/MAC, netmask,
DHCP pool endpoints), a single bounded/expiring DHCP transaction, exact request/offer/
ACK correlation, subnet/pool/server/client/link checks, malformed/duplicate-option
rejection, owned-address ARP/DAD, and 128 bounded TCP22 return permissions with
120-second host-refresh expiry. ACK/NAK and lease loss/address changes clear return
state. Replies cannot extend expiry. DHCP retransmissions cannot extend the absolute
30-second transaction deadline. Unsupported DHCP extensions and Release/Decline/
Inform fail closed. No generic broadcast exception, IPv6, fragments, VLAN or IP options.

**No actual interface metadata or observer has been supplied. `ControlOnly::new()`
therefore denies every packet.** `with_config` accepts only an internal configuration,
not guest/client JSON, but its constructor checks consistency, not provenance. Wiring
it to a reviewed host-authoritative observer is deliberately left undone; no guessed
network values or gateway-MAC learning fallback. Fixture addresses are synthetic only.

Default feature-off dispatch/startup behavior is unchanged. Private source startup
retains the previous skip-bootpd/no-sudo/no-telemetry-init behavior. No service orchestration
was changed. No helper installation, signing, runtime activation, actual network test,
provider/credential access, global configuration change or capability enablement.

Permissions are directional tuples, not a TCP sequence firewall. Checksums are left to
endpoint stacks (offload semantics require separate review). Runtime identity/attachment
observation, DHCP compatibility, exact kernel enforcement and legacy runtime regression
remain unverified. Source tests do not establish a usable or safe deployed helper.
