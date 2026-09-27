# vm-service

Turns this Mac's Tart installation into a **lease-based VM service for agents**.
Agents never touch `tart` directly; they ask the service for a fresh, disposable
VM and give it back when done.

Built on top of `~/Artifacts/Repositories/com.github/WeZZard/pilot-images`
(golden bases `pilot-macos26-base` / `pilot-ubuntu-base`, credential packs).
Enforces the doctrines from pilot-images' CLAUDE.md and the host
AGENTS.md mechanically:

- **Clean clone per task** — every acquire is a fresh COW clone of the golden
  base; every release destroys it (AGENTS.md "MUST create a clean VM instance /
  destroy when done").
- **Max 2 concurrent macOS VMs** (Virtualization.framework limit); Linux unlimited.
- **NAT only** — no bridged networking is ever offered (MAC-whitelist doctrine).
- **Lifecycle-managed images are unbootable through the service** — only
  purpose clones are ever started; `-base`/`-seed`/`-work` are never booted by the service. Explicit work-source acceptance reads and clones the configured stopped work image without changing it.
- **Credential env packs are env-var-only** (gateway era) — `env.extra` carries
  LiteLLM gateway envs (pi) and Anthropic-compatible gateway envs (Claude
  Code) plus GH_TOKEN. Gateway keys are not identities: no lane
  balancing, no busy marking; one env pack serves any number of concurrent VMs.
- **Teardown is part of the job** — leases carry a TTL; past TTL the lease is
  warned, past TTL + 6h grace the VM is stopped, deleted, and unregistered.
  This prevents a repeat of the 2026-08-19 style 25 GB leftovers.

## VNC design status

- **The stock-Tart VNC replacement is deployed as part of the Rust port, but its console workflow is not yet live-accepted.** It uses guest-side sharing over SSH; it does not restore the rejected Tart patch.
- [The revised service design](docs/design.md) includes the normal vm-service deployment and mcp-vm-relay upgrade path in the definition of completion. Source tests alone do not establish a working installed workflow.
- [The acquisition contract](docs/acquisition-contract.md) and [console contract](docs/console-contract.md) describe capability discovery, `acquire --vnc`, and explicit `console-resolve`, `console-open`, and `console-cancel` operations.
- [Installation](docs/vnc-installation.md) persists trusted viewer configuration, and [guest preparation](docs/guest-console-provisioning.md) defines the Linux X11 and macOS sharing prerequisites. A configuration check does not prove a stock-Tart boot or a displayed desktop.
- [The mechanism investigation](docs/live-console-review.md) compares stock Tart, Linux X11 sharing, and macOS Screen Sharing, including unresolved compatibility and provisioning checks.
- [The acceptance specification](docs/vnc-acceptance.md) requires actual viewer screenshots, independent visual evaluation, lifecycle tests, and the installed relay workflow.
- [The verification status](docs/capability-transparency-verification.md) distinguishes the new source tests from live acceptance and [historical patched-runtime evidence](docs/history/README.md).

## User stories

Actor-oriented stories (agent / orchestrator / operator), each linked to the
tests that pin it down: [docs/user-stories/index.md](docs/user-stories/index.md).

## Components

The service is a Rust workspace under `rust/`. The Python implementation it
replaced was removed once the port was verified against it, and remains
recoverable from this repository's history.

| Path | Role |
|------|------|
| `rust/crates/vm-service` | Daemon binary: HTTP API on the configured bind address (loopback port 6240 by default), state in `~/.local/state/vm-service/state.json`, GC loop every 60s |
| `rust/crates/vmctl` | CLI client (same operations as the HTTP API) |
| `rust/crates/vm-service-core` | Lease records, Tart lifecycle, SSH access, env packs, and operations |
| `rust/crates/lease-keys` | Standard OpenSSH per-lease key generation, bootstrap, and safe cleanup |
| `rust/crates/acquisition-options` | Pure option validation, configuration reporting, and discovery descriptions |
| `rust/crates/environment` | Selected-environment profile resolution and startup ownership |
| `rust/crates/control-only` | Offline control-only network contract; acquisition refuses while none is available |
| `rust/crates/application-catalog` | Read-only installed-application catalog and base fingerprints |
| `rust/crates/console` | Trusted viewer configuration, lease and attempt identity, guest preparation, and worker supervision |
| `rust/crates/console-worker` | Independently expiring loopback broker, SSH stream, and standard viewer launch |
| `rust/crates/guest-console-agent` | Guest-side sharing agent, built once per guest operating system |
| `rust/crates/runtime-launcher` | Service-owned guest launcher |
| `rust/crates/vm-service-install` | Install/remove the launchd agent `com.wezzard.vm-service` (RunAtLoad + KeepAlive) |
| `bin/runtime-launcher.c` | C source of the service-owned guest launcher |
| `examples/pi-subagents/vm-use.workflow.js` | Workflow: acquire → child works via vmctl → pull evidence → release in `finally` |

## Selected environments

For an isolated Tart store, service state, image checkout, and HTTP endpoint,
select an explicit JSON profile with `VM_ENVIRONMENT_FILE` or
`vmctl --environment /absolute/profile.json`. The profile overrides individual
ambient configuration variables. `vmctl environment --json` resolves the
configuration offline without creating state directories.

See [Selected environments](docs/selected-environments.md) for the schema,
resolver API, startup ownership, HTTP identity checks, and test scope.
Without a profile, the legacy defaults below remain unchanged.

## Build and install

The daemon is a native executable, so it is launched directly instead of
through an interpreter. The installer installs the executables that sit beside
it, which makes the staging directory the deployed location.

```zsh
# 1. Build the daemon, the client, and one guest agent per guest OS.
cd ~/Artifacts/Repositories/com.github/WeZZard/vm-service/rust
cargo build --release
cargo build --release -p guest-console-agent
CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
  cargo build --release -p guest-console-agent --target aarch64-unknown-linux-musl

# 2. Stage every executable the installer requires as a sibling.
install -d ~/.local/libexec/vm-service
install -m 0755 target/release/vm-service          ~/.local/libexec/vm-service/vm-service
install -m 0755 target/release/vmctl               ~/.local/libexec/vm-service/vmctl
install -m 0755 target/release/console-worker      ~/.local/libexec/vm-service/console-worker
install -m 0755 target/release/vm-service-install  ~/.local/libexec/vm-service/vm-service-install
install -m 0755 target/release/guest-console-agent ~/.local/libexec/vm-service/guest-console-agent
install -m 0755 target/aarch64-unknown-linux-musl/release/guest-console-agent \
  ~/.local/libexec/vm-service/guest-console-agent-linux

# 3. Validate, then install. Loopback only is the fresh-install default.
~/.local/libexec/vm-service/vm-service-install --check
~/.local/libexec/vm-service/vm-service-install install

# LAN-reachable (binds 0.0.0.0; required for Consul-discovered consumers on
# other hosts). A repeated install preserves the bind address it finds.
VM_SERVICE_HOST=0.0.0.0 ~/.local/libexec/vm-service/vm-service-install install

launchctl list | grep vm-service          # health check
curl -s localhost:6240/health
```

- The installer requires `vm-service`, `vmctl`, `console-worker`,
  `guest-console-agent`, and `guest-console-agent-linux` beside itself.
  The daemon resolves its helpers relative to its own executable, and a Linux
  guest cannot execute the macOS agent build.
- The installer refuses to install, restart, or remove while any lease record
  remains in the previous or the proposed state root, including retained failed
  or deleting records. Release the leases first.
- The installer preserves installed environment values such as `VM_SERVICE_HOST`,
  `VM_SERVICE_CONSOLE_CONFIG`, and the selected-environment selector, unless the
  caller supplies an override for that setting.
- Enabling the console for the first time requires an explicit
  `--console-config PATH` argument.
- `--check` and `--dry-run` validate without writing a plist or calling
  `launchctl`.
- `rust/scripts/build-guest-agents.sh` builds both guest agents into
  `target/guest-agents`, but it builds them in the debug profile.
- `vm-service-install remove` unloads the agent and deletes the plist.

Log at `/tmp/com.wezzard.vm-service.{out,err}`, service log at
`~/.local/state/vm-service/service.log`.

## Consul discovery

On machines running the home Consul agent + service-catalog registrar, drop a
description at `~/.config/service-catalog/services/vm-service.json` (see the
service-catalog repo; the file used on this Mac is in this repo at
`services/vm-service.json`). The registrar then keeps this in Consul:

- service `vm-service`, tags `vm tart disposable-infra agents`, address = this
  machine's LAN IP, port 6240, `loopback_only: false` when bound to `0.0.0.0`.
- HTTP health check `GET /health` every 30 s.
- DNS: `dig @<consul> -p 8600 vm-service.service.home.consul A +short`
- HTTP: `GET /v1/health/service/vm-service?passing=true` on the agent (8500).

The API itself has no authentication — LAN exposure assumes the household
trust model (same as the LLM gateway). Keep the loopback-only bind where that
model does not hold.

## HTTP API (agents)

```
GET  /health
GET  /acquisition-capabilities
GET  /vms
GET  /vms/<name>
POST /acquire                {"purpose":"my-task","image":"macos26","env":"default","ttl_hours":4}
POST /vms/<name>/exec        {"argv":["uname","-a"]}  or  {"script":"echo hi"}
POST /vms/<name>/push        {"local_path":"/host/f","remote_path":"/tmp/f"}
POST /vms/<name>/pull        {"remote_path":"/tmp/f","local_path":"/host/f"}
POST /vms/<name>/heartbeat   {"ttl_hours":4}
POST /vms/<name>/release     {"reason":"done"}
POST /gc
```

Errors are JSON `{"error": "..."}` with status 400/409/500.

## Application catalog

`GET /applications` returns the complete validated installed-application catalog
from Git-visible `PILOT_REPO/images/<image>/applications.json` portable observations
paired with host-local associations, or HTTP 503 when unavailable or invalid.

- The state root is `PILOT_IMAGES_STATE_DIR` when it is nonempty, otherwise
  `$XDG_STATE_HOME/pilot-images` when `XDG_STATE_HOME` is nonempty, otherwise
  `~/.local/state/pilot-images`.
- Each association is at `<state-root>/stores/<store-id>/base/<image>.json`.
- The store ID is the full lowercase SHA256 hexadecimal digest of the UTF-8
  canonical absolute Tart VM-store root, without a trailing slash. It identifies
  the store directory, not an individual VM or the pilot-images checkout.
- The daemon uses `$TART_HOME/vms` when TART_HOME is set, or `~/.tart/vms` otherwise. The catalog loader also accepts an explicit `base_root` for fixtures and embedded callers.
- Missing members are reported by exact path in diagnostics. If no inventory is
  usable, the HTTP 503 response also includes those paths.

Portable schema 1 is closed `{schemaVersion,image,
inventory,provenance}`; association schema 2 is closed `{schemaVersion,image,base,
inventorySha256}` and hashes complete portable bytes. Provenance records bounded
extraction mode/evidence identifier and lowercase raw/collector/aliases SHA256.
Missing pairs, digest mismatches and stale bases exclude their inventories;
malformed existing members fail the request even if the mate is missing. Both
members and fingerprint are rechecked against ordinary concurrent replacement.
No legacy migration/rebind. Historical Git-visible initial observations alone
are review-ready, not current-base validity or installation certification.

Trusted startup **executes** actual `images/*/line.conf` with the existing zsh
parser, after exclusive `daemon.lock` ownership and before HTTP construction/GC.
Strict catalog initialization rejects parse failures (including source failure
and timeout), enumeration failures, no lines and invalid image/OS/base
associations. It never uses the permissive lifecycle `discover_lines()` fallback.
Only a complete detached image-to-`{kind, base_vm}` snapshot is published; no
credentials or mutable lifecycle-cache aliases are retained. Known configuration
failure logs a bounded diagnostic and leaves the daemon serving, with catalog
requests returning 503. Unexpected programming errors are not swallowed.

Requests perform **metadata-only** inventory/fingerprint reads, not shell/config
execution, lifecycle operations or inventory generation. An uninitialized/failed
snapshot is not initialized or retried by a request, even with a cold lifecycle
cache. Configuration edits, additions, and removals require a daemon restart.
Portable inventory JSON, host-local association JSON, and base fingerprints
remain live on every request. Lifecycle discovery's
existing permissive behavior is unchanged. Source edits do not restart/deploy the
shared daemon; restart remains an explicit operator action.

### Migration and rollback

- Coordinate the producer and consumer source versions before an operator-approved
  deployment. The consumer has no fallback to `lines/`, `inventories/`, or
  `inventories/local/base/`.
- Move image configuration to `images/<image>/line.conf` and portable observations
  to `images/<image>/applications.json`. Preserve portable bytes exactly because
  the association digest covers the entire file.
- Configure the producer and daemon with the same state-root environment and Tart
  VM-store root. Host-local records are not portable between stores or hosts.
- Use the producer's explicit validation/publication workflow to establish current
  associations. Do not generate or rebind records through catalog search, and do
  not move or modify golden VMs to make an old fingerprint pass.
- Preserve the old checkout and its local association records before migration.
  After an explicitly approved deployment, an operator must restart the daemon
  to load the new configuration snapshot and verify `GET /applications`.
- To roll back, restore the matching old producer and consumer versions together
  with the old configuration and inventory layout. Restore only association
  records that still match the same local base fingerprints. A stale record must
  remain unavailable rather than being silently rebound.
- Source changes and fixture tests do not deploy or restart the shared daemon.

### Execution timeout

- `POST /vms/<name>/exec` accepts a positive `timeout` in seconds and defaults to
  600 seconds. The daemon forwards the requested value to the SSH subprocess
  without imposing a shorter deadline.
- The maximum `timeout` is 4200 seconds. A larger value is rejected before any
  SSH process starts, because a running command holds the lease's operation
  lock, and release and GC wait on that lock. See
  [Lease lifecycle fixes](docs/lifecycle-fixes.md).
- A receiver running a command for up to 3600 seconds must include its capture
  and reporting allowance in this timeout. For example, a 120-second allowance
  requires a 3720-second request timeout. mcp-vm-relay's largest request,
  4080 seconds, is within the maximum.
- The HTTP caller must set a transport timeout longer than the complete receiver
  budget and must continue heartbeating the lease during long operations.

### Fresh-work-clone acceptance

- Acquisition defaults to `source: "base"`. An image maintainer can explicitly request `source: "work"` to test a fresh clone of that image's configured `WORK_VM` before promotion.
- Work-source acquisition requires explicit `env: "none"`. It does not accept an arbitrary VM name or path, and it never boots or modifies the source work image.
- The work image must exist and be stopped. The daemon checks its metadata before reservation and immediately before and after cloning. A changed source fails acquisition and cleans up only the new lease.
- An optional `expected_source_fingerprint` object requires the source metadata to match the maintainer's recorded work association before cloning.
- The returned lease includes `source`, `source_vm`, and `source_fingerprint`. The maintainer must also recheck the source after acceptance and bind the actual clone report to that work association before promotion. File metadata checks are not disk-content attestation.
- The image maintenance lock must remain held by the maintainer throughout work acceptance and promotion. This is not authorization to run builds or restart the production daemon automatically.

```sh
vmctl acquire --purpose acceptance-example --image ubuntu2404 --source work --env none \
  --expected-source-fingerprint /path/to/work-fingerprint.json
```

## CLI

```zsh
vmctl acquire --purpose build-test --image macos26 --ttl-hours 4
vmctl list
vmctl exec pilot-mac-build-test-<id> -- swift --version
vmctl push  <vm> ./artifact.zip /tmp/artifact.zip
vmctl pull  <vm> /tmp/build.log ./build.log
vmctl heartbeat <vm>            # call periodically for long tasks
vmctl release <vm>
```

(Symlink `vmctl` onto PATH, for example as `~/.local/bin/vmctl` →
`~/.local/libexec/vm-service/vmctl`, if you want it unqualified. launchd starts
the daemon, so `vm-service` does not need to be on PATH.)

## acquire semantics

- `purpose` becomes the VM name: `pilot-mac-<purpose>-<rand>` (macOS) or
  `pilot-<purpose>-<rand>` (Ubuntu). One active lease per purpose+image.
- `env` (aliases `pack`/`lane`): `default` (default) injects the env pack
  `~/.config/vm-credentials/default/` (gateway env vars — see
  `pilot-images/profiles/pi/litellm.env.example.md`); `none` for
  clones without application secrets; or an explicit pack name. Every new
  lease still receives its own SSH public key for service access.
- `wait: true` (default) returns only after bootstrap public-key installation,
  fresh key-only command access, a checksum-verified upload/download round trip,
  and any env-pack injection succeed. `wait: false` uses a shorter bootstrap
  deadline and one command probe, but never bypasses key provisioning/readiness.
  Cloning a 200 GB-provisioned macOS base takes minutes; allow a ≥10-minute
  client timeout.
- `ttl_hours` (default 24, cap 720): lease lifetime. Long jobs must heartbeat.
- Failure rolls back: stop, delete, verify absence, then remove the key and lease.
  Failed teardown retains the `releasing` record and credentials for retry.

See [per-lease SSH keys](docs/lease-ssh-keys.md) for bootstrap trust, key-only
OpenSSH options, recovery, and the legacy-lease rollout requirement. The running
shared daemon is not restarted automatically by a source change.

## pi-subagents integration

Register `examples/pi-subagents/vm-use.workflow.js` as a named workflow resource
or paste it as `workflowScript`. It guarantees release even when the child
agent fails, and requires evidence to be pulled to `/var/tmp/` before teardown.

## State & files

- `~/.local/state/vm-service/state.json` — all leases
- `~/.local/state/vm-service/state.lock` — cross-process flock
- `~/.local/state/vm-service/service.log` — daemon log
- `~/.local/state/vm-service/ssh/<vm>/` — private per-lease identity and known_hosts;
  never copy this directory into public reports or guest workspaces
- `/tmp/tart-run-<vm>.log` — per-VM console log (same convention as pilot-images)

## Not implemented here (deliberately)

- **Task registry in AGENTS.md** — the service records leases, but the human
  registry table is maintained by the acquiring agent, as AGENTS.md requires
  ("MUST register your task ... before task started").
- **No bridged networking, no base/seed/work boots, no credential writes into
  golden images** — refused by design, not by convention.

## License

MIT. See [LICENSE](LICENSE).
