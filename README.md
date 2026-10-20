# machine-service

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
- **NAT or internet-only** — no bridged networking is ever offered (MAC-whitelist doctrine). A lease may ask for `network: "internet"`, which runs its guest on Tart's Softnet with no route to the private networks (DC-13), behind a pf anchor that keeps it off the host by its own address (DC-14, DC-15), resolving names over DNS over TLS (DC-16), and with an isolation check before the lease is ready.
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
- [The revised service design](docs/design.md) includes the normal machine-service deployment and mcp-vm-relay upgrade path in the definition of completion. Source tests alone do not establish a working installed workflow.
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
| `rust/crates/machine-service` | Daemon binary: HTTP API on the configured bind address (loopback port 6240 by default), state in `~/.local/state/machine-service/state.json`, GC loop every 60s |
| `rust/crates/machctl` | CLI client (same operations as the HTTP API) |
| `rust/crates/machine-service-core` | Lease records, Tart lifecycle, SSH access, env packs, and operations |
| `rust/crates/lease-keys` | Standard OpenSSH per-lease key generation, bootstrap, and safe cleanup |
| `rust/crates/acquisition-options` | Pure option validation, configuration reporting, and discovery descriptions |
| `rust/crates/environment` | Selected-environment profile resolution and startup ownership |
| `rust/crates/control-only` | Offline control-only network contract; acquisition refuses while none is available |
| `rust/crates/application-catalog` | Read-only installed-application catalog and base fingerprints |
| `rust/crates/console` | Trusted viewer configuration, lease and attempt identity, guest preparation, and worker supervision |
| `rust/crates/console-worker` | Independently expiring loopback broker, SSH stream, and standard viewer launch |
| `rust/crates/guest-console-agent` | Guest-side sharing agent, built once per guest operating system |
| `rust/crates/runtime-launcher` | Service-owned guest launcher |
| `rust/crates/machine-service-install` | Install/remove the launchd agent `com.wezzard.machine-service` (RunAtLoad + KeepAlive) |
| `bin/runtime-launcher.c` | C source of the service-owned guest launcher |
| `examples/pi-subagents/vm-use.workflow.js` | Workflow: acquire → child works via machctl → pull evidence → release in `finally` |

## Selected environments

For an isolated Tart store, service state, image checkout, and HTTP endpoint,
select an explicit JSON profile with `VM_ENVIRONMENT_FILE` or
`machctl --environment /absolute/profile.json`. The profile overrides individual
ambient configuration variables. `machctl environment --json` resolves the
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
install -d ~/.local/libexec/machine-service
install -m 0755 target/release/machine-service         ~/.local/libexec/machine-service/machine-service
install -m 0755 target/release/machctl                 ~/.local/libexec/machine-service/machctl
install -m 0755 target/release/console-worker          ~/.local/libexec/machine-service/console-worker
install -m 0755 target/release/machine-service-install ~/.local/libexec/machine-service/machine-service-install
install -m 0755 target/release/vm-service-guest-firewall ~/.local/libexec/machine-service/vm-service-guest-firewall
install -m 0755 target/release/vm-service-dot-forwarder ~/.local/libexec/machine-service/vm-service-dot-forwarder
install -m 0755 target/release/guest-console-agent ~/.local/libexec/machine-service/guest-console-agent
install -m 0755 target/aarch64-unknown-linux-musl/release/guest-console-agent \
  ~/.local/libexec/machine-service/guest-console-agent-linux

# 3. Validate, then install. Loopback only is the fresh-install default.
~/.local/libexec/machine-service/machine-service-install --check
~/.local/libexec/machine-service/machine-service-install install

# LAN-reachable (binds 0.0.0.0; required for Consul-discovered consumers on
# other hosts). A repeated install preserves the bind address it finds.
MACHINE_SERVICE_HOST=0.0.0.0 ~/.local/libexec/machine-service/machine-service-install install

launchctl list | grep machine-service     # health check
curl -s localhost:6240/health
```

- The installer requires `machine-service`, `machctl`, `console-worker`,
  `guest-console-agent`, and `guest-console-agent-linux` beside itself.
  The daemon resolves its helpers relative to its own executable, and a Linux
  guest cannot execute the macOS agent build.
- The installer refuses to install, restart, or remove while any lease record
  remains in the previous or the proposed state root, including retained failed
  or deleting records. Release the leases first.
- The installer preserves installed environment values such as `MACHINE_SERVICE_HOST`,
  `MACHINE_SERVICE_CONSOLE_CONFIG`, and the selected-environment selector, unless the
  caller supplies an override for that setting.
- Enabling the console for the first time requires an explicit
  `--console-config PATH` argument.
- `--check` and `--dry-run` validate without writing a plist or calling
  `launchctl`.
- `rust/scripts/build-guest-agents.sh` builds both guest agents into
  `target/guest-agents`, but it builds them in the debug profile.
- `machine-service-install install` links `~/.local/bin/machctl` to the
  `machctl` beside it. An existing link is replaced, and a file that is not a
  link is left in place.
- `machine-service-install remove` unloads the agent, deletes the plist, and
  removes the `machctl` link it made.
- The daemon refuses to start while any `VM_SERVICE_*` variable is set, and
  names each one with its `MACHINE_SERVICE_*` replacement. The installer
  refuses a caller that still exports one.

### Migrating from vm-service

The service was renamed from vm-service, and its CLI from `vmctl` (DC-19,
DC-35). Release every lease first; then `machine-service-install install`
migrates an installed `com.wezzard.vm-service` agent in the same run:

- The old agent's `VM_SERVICE_*` values are written under their
  `MACHINE_SERVICE_*` names into `com.wezzard.machine-service`.
- The old agent's job is unloaded and its plist removed.
- `~/.local/state/vm-service/` moves to `~/.local/state/machine-service/` when
  the new folder does not exist yet. It holds no lease and no lease key,
  because the installer refuses while any lease record exists; the service log
  moves with it.
- `~/.config/vm-service/console.json` is copied to
  `~/.config/machine-service/console.json` when the new file does not exist,
  and the console variable is pointed at the copy.
- The `~/.local/bin/vmctl` link into the old install folder is removed, and no
  `vmctl` link is installed.
- The old install folder `~/.local/libexec/vm-service/` is left in place for
  the operator to delete.

`--check` prints this plan without changing anything.

Log at `/tmp/com.wezzard.machine-service.{out,err}`, service log at
`~/.local/state/machine-service/service.log`.

## Guest firewall for `internet` leases

An `internet` lease needs the guest firewall, a pf anchor whose table lists
the running `internet` guests by address and keeps them off this host
(DC-14, DC-15, [acquisition contract](docs/acquisition-contract.md#guest-network)).
Install it once, as root, from the account that runs machine-service:

```zsh
sudo ~/.local/libexec/machine-service/vm-service-guest-firewall install   # --table PATH for another table file
~/.local/libexec/machine-service/vm-service-guest-firewall status         # no root needed; lists the table's guests
sudo ~/.local/libexec/machine-service/vm-service-guest-firewall remove
```

- `install` creates the table file `/var/db/com.wezzard.vm-service/internet-guests`, owned by the account that ran `sudo`, writes `/etc/pf.anchors/com.wezzard.vm-service` and the LaunchDaemon `com.wezzard.vm-service.guest-firewall`, enables pf, and loads the anchor into `com.apple/250.vm-service`. `print` shows the anchor and the LaunchDaemon without installing them.
- Every machine-service instance on the host writes the same table file. `MACHINE_SERVICE_GUEST_FIREWALL_TABLE` points an instance at another one, which the anchor must read.
- Install the firewall while no `internet` lease is running, because SSH connections opened before the anchor loads have no pf state.
- `MACHINE_SERVICE_GUEST_DNS` sets the resolvers an `internet` guest reaches over DNS over TLS, by default `1.1.1.1#cloudflare-dns.com,8.8.8.8#dns.google`.
- A macOS `internet` guest needs `vm-service-dot-forwarder` beside machine-service, or `MACHINE_SERVICE_DOT_FORWARDER`; the host-native build runs in a macOS guest.
- `MACHINE_SERVICE_GUEST_FIREWALL_ANCHOR` points the service at another anchor file, as the test fixtures do.

## Consul discovery

On machines running the home Consul agent + service-catalog registrar, drop a
description at `~/.config/service-catalog/services/machine-service.json` (see the
service-catalog repo; the file used on this Mac is in this repo at
`services/machine-service.json`). The registrar then keeps this in Consul:

- service `machine-service`, tags `vm tart disposable-infra agents`, address = this
  machine's LAN IP, port 6240, `loopback_only: false` when bound to `0.0.0.0`.
- HTTP health check `GET /health` every 30 s.
- DNS: `dig @<consul> -p 8600 machine-service.service.home.consul A +short`
- HTTP: `GET /v1/health/service/machine-service?passing=true` on the agent (8500).

The API itself has no authentication — LAN exposure assumes the household
trust model (same as the LLM gateway). Keep the loopback-only bind where that
model does not hold.

## HTTP API (agents)

```
GET  /health
GET  /acquisition-capabilities
GET  /images
POST /images/reload
GET  /applications
GET  /machines
GET  /machines/<name>
POST /acquire                {"purpose":"my-task","image":"macos26","env":"default","ttl_hours":4}
POST /machines/<name>/exec        {"argv":["uname","-a"]}  or  {"script":"echo hi"}
POST /machines/<name>/push        {"local_path":"/host/f","remote_path":"/tmp/f"}
POST /machines/<name>/pull        {"remote_path":"/tmp/f","local_path":"/host/f"}
POST /machines/<name>/heartbeat   {"ttl_hours":4}
POST /machines/<name>/release     {"reason":"done"}
POST /gc
```

Errors are JSON `{"error": "..."}` with status 400/409/500.

- `GET /machines` lists the leases under its top-level `machines` key. Each
  lease record keeps its fields, including `vm`.
- `GET /images`, `GET /machines` and `GET /health` carry the image catalog's
  status under `catalog`, and `POST /images/reload` rebuilds the catalog and
  answers with it. See [Image catalog](#image-catalog).
- Every `/vms` and `/vms/<name>/...` path was removed. It answers HTTP 410 Gone
  with an error naming the `/machines` path that replaces it, and a
  `replacement` field carrying that path. No operation runs (DC-37).

## Image catalog

The daemon keeps one immutable snapshot of its image lines, `images/*/line.conf`
in `PILOT_REPO`, and every reader of the lines uses it: `/images`, `/machines`,
`/applications`, `/acquisition-capabilities` and `POST /acquire`. Adding,
editing or removing a line takes effect on the next request, without a restart
([IC](docs/arch/image-catalog.md), DC-34).

- **Trust.** Only the daemon that owns `daemon.lock` **executes** `line.conf`
  files, with the strict zsh parser and its 15-second timeout. It builds the
  first snapshot after taking the lock and before it serves; nothing is
  executed for a request before then.
- **Input identity.** Each request that reads the catalog first lists
  `images/` and, for each folder with a `line.conf`, takes the file's inode,
  size, modification time and SHA-256. This executes nothing. When the
  identity differs from the snapshot's, the request asks for a rebuild and
  waits for it, up to 30 seconds. One rebuild runs at a time, and requests
  that arrive meanwhile wait for that same rebuild.
- **Strict build.** A rebuild fails as a whole on an unreadable `images/`
  folder, no lines, more than 64 lines, a non-regular `line.conf`, a parse or
  source failure or timeout, an invalid image or base name, a kind other than
  `macos` or `linux`, or a `CPU` or `MEMORY_MB` that is not an integer. There
  is no permissive mode and no built-in fallback list. The builder checks the
  identity again after executing the files and starts over if they moved.
- **Atomic swap.** A complete snapshot replaces the old one in one pointer
  swap. A request, and an acquisition from start to end, keeps the snapshot it
  read. Guest passwords stay beside the snapshot in the daemon's memory for
  the lease bootstrap, never in a response or the log.
- **Failure.** A failed rebuild keeps the last good snapshot and writes one
  `ERROR` line to the service log and to stderr per failed input identity.
  Requests do not retry a failed identity; the next change to the files, or
  `POST /images/reload`, does. Without any good snapshot, as after a startup
  with broken files, `/images` and `/applications` answer HTTP 503 with the
  diagnostic and `POST /acquire` answers 409; a selected environment refuses
  to start instead.
- **Status.** `catalog` is `{"generation", "built_at", "status"}`, where
  `status` is `current`, `stale` (the files changed and the snapshot no longer
  matches them) or `unavailable` (no good snapshot yet). After a failed
  rebuild that is not superseded, it adds `error`, `failed_at` and `file`.
  `/health` reports it without waiting for a rebuild.
- **Live inputs.** Whether a base VM exists in the Tart store, and so whether
  an image is acquirable, is read on every request, so promoting a base needs
  no rebuild. Inventories, associations and fingerprints stay live too.
- **Watcher.** A kqueue watcher on `images/`, each line folder and each
  `line.conf` starts a rebuild as soon as a file changes, so the next request
  usually finds the catalog current. It only accelerates: a missed event costs
  one request's wait, never a stale answer.
- **Removed lines.** A lease whose line is removed keeps running; its
  heartbeat, exec and release read the lease record, not the catalog.

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

Requests perform **metadata-only** inventory/fingerprint reads, not shell/config
execution, lifecycle operations or inventory generation. Portable inventory
JSON, host-local association JSON, and base fingerprints remain live on every
request. The image-to-`{kind, base_vm}` associations come from the image
catalog above, and the response keeps its closed shape, which mcp-relay
validates: it carries no catalog status. Without any good catalog it answers HTTP 503 with the diagnostic.

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
  After an explicitly approved deployment, verify `GET /applications`; the
  daemon reads changed image lines on the next request, without a restart.
- To roll back, restore the matching old producer and consumer versions together
  with the old configuration and inventory layout. Restore only association
  records that still match the same local base fingerprints. A stale record must
  remain unavailable rather than being silently rebound.
- Source changes and fixture tests do not deploy or restart the shared daemon.

### Execution timeout

- `POST /machines/<name>/exec` accepts a positive `timeout` in seconds and defaults to
  600 seconds. The daemon forwards the requested value to the SSH subprocess
  without imposing a shorter deadline.
- A receiver running a command for up to 3600 seconds must include its capture
  and reporting allowance in this timeout. For example, a 120-second allowance
  requires a 3720-second request timeout. Longer explicit requests remain valid.
- The HTTP caller must set a transport timeout longer than the complete receiver
  budget and must continue heartbeating the lease during long operations.
- A release of the lease preempts a running command, push, or pull. The daemon
  kills the SSH or SCP subprocess, the request fails with
  `command cancelled by release of <name>` or
  `transfer cancelled by release of <name>`, and teardown proceeds. See
  [Lease lifecycle fixes](docs/lifecycle-fixes.md).

### Fresh-work-clone acceptance

- Acquisition defaults to `source: "base"`. An image maintainer can explicitly request `source: "work"` to test a fresh clone of that image's configured `WORK_VM` before promotion.
- Work-source acquisition requires explicit `env: "none"`. It does not accept an arbitrary VM name or path, and it never boots or modifies the source work image.
- The work image must exist and be stopped. The daemon checks its metadata before reservation and immediately before and after cloning. A changed source fails acquisition and cleans up only the new lease.
- An optional `expected_source_fingerprint` object requires the source metadata to match the maintainer's recorded work association before cloning.
- The returned lease includes `source`, `source_vm`, and `source_fingerprint`. The maintainer must also recheck the source after acceptance and bind the actual clone report to that work association before promotion. File metadata checks are not disk-content attestation.
- The image maintenance lock must remain held by the maintainer throughout work acceptance and promotion. This is not authorization to run builds or restart the production daemon automatically.

```sh
machctl acquire --purpose acceptance-example --image ubuntu2404 --source work --env none \
  --expected-source-fingerprint /path/to/work-fingerprint.json
```

## CLI

```zsh
machctl images                    # images, capacity and the catalog's status
machctl images --reload           # rebuild the catalog now; exit 1 unless current
machctl acquire --purpose build-test --image macos26 --ttl-hours 4
machctl list
machctl exec pilot-mac-build-test-<id> -- swift --version
machctl push  <vm> ./artifact.zip /tmp/artifact.zip
machctl pull  <vm> /tmp/build.log ./build.log
machctl heartbeat <vm>            # call periodically for long tasks
machctl release <vm>
```

`machctl list` and `machctl images` print one warning line above their rows
while the catalog is `stale` or `unavailable`, naming the failing file, the
error and the time of the failure.

(The installer links `~/.local/bin/machctl` to
`~/.local/libexec/machine-service/machctl`. launchd starts the daemon, so
`machine-service` does not need to be on PATH.)

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
- `POST /machines/<name>/heartbeat` renews the TTL from now and clears any grace
  state. With `ttl_hours` it renews for that many hours; without it, the lease
  is renewed for its own initial `ttl_hours`. The recorded initial TTL is not
  changed.
- A heartbeat on a `running` lease whose VM Tart reports stopped or absent
  fails, and the TTL is not renewed. GC releases such a lease after two
  consecutive passes (about two minutes) that find its VM not running.
- Failure rolls back: stop, delete, verify absence, then remove the key and lease.
  Failed teardown retains the `releasing` record and credentials for retry.
- A daemon restart interrupts any acquisition in progress. At startup, before
  the HTTP server binds, the daemon releases every `pending` or `provisioning`
  record left by the previous process and logs each one, so a retry with the
  same purpose is not refused as already leased.
- If the daemon cannot write a successful acquisition response because the
  client's connection was reset, it releases the new lease and logs it. A
  client that closed its connection normally may still receive the bytes into
  its socket buffer; TTL and GC remain the safeguard in that case.

See [per-lease SSH keys](docs/lease-ssh-keys.md) for bootstrap trust, key-only
OpenSSH options, recovery, and the legacy-lease rollout requirement. The running
shared daemon is not restarted automatically by a source change.

## pi-subagents integration

Register `examples/pi-subagents/vm-use.workflow.js` as a named workflow resource
or paste it as `workflowScript`. It guarantees release even when the child
agent fails, and requires evidence to be pulled to `/var/tmp/` before teardown.

## State & files

- `~/.local/state/machine-service/state.json` — all leases
- `~/.local/state/machine-service/state.lock` — cross-process flock
- `~/.local/state/machine-service/service.log` — daemon log
- `~/.local/state/machine-service/ssh/<vm>/` — private per-lease identity and known_hosts;
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
