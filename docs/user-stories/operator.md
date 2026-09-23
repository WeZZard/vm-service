# Profile: Operator

The human operator (WeZZard) who installs the service, keeps it healthy, troubleshoots failures, and treats the host doctrines as non-negotiable.

## US11 — Drive the service from a CLI

**As an** operator, **I want** a `vmctl` command for every HTTP operation, **so that** I can inspect and manage leases without hand-crafting HTTP.

- `acquire`, `wait --timeout-s`, `list [--json]`, `images [--json]`, `images-show`, `status`, `exec`, `push`, `pull`, `heartbeat`, `release`, `gc`.
- Flags mirror the API: `--image`, `--env`, `--ttl-hours`, `--no-wait`; old `--line`/`--pack` flags still parse (hidden aliases).
- Errors are actionable: `leased`/`limit`/`BASE-MISSING`/`404`/`unreachable` each map to a distinct, human-readable message.

**Tests:** `TestVmctl.test_acquire_old_flag_aliases` and the rest of `TestVmctl` (integration — real `vmctl` subprocesses against the live daemon: acquire with new and old flags, list/images/images-show, exec rc passthrough, gc, actionable error messages).

## US12 — Stable, backward-compatible API naming

**As an** operator, **I want** the `line`→`image` and `pack`/`lane`→`env` rename to be non-breaking, **so that** existing scripts and stored state keep working across the rename.

- `POST /acquire` accepts `image`/`env` and still accepts `line`/`pack`/`lane` as hidden aliases.
- Old state records (`line`/`line_kind`/`pack`/`lane`) migrate to the new shape on load, idempotently.
- User-facing errors use the new vocabulary (`unknown image 'x' (available: macos26, ubuntu2404)`).

**Tests:** `TestHttpApi.test_acquire_old_field_aliases_on_the_wire` (integration), `TestStateMigration.test_lane_alias_migrated` + `test_migration_idempotent` (unit).

## US13 — Inspect what the service is doing

**As an** operator, **I want** visibility into leases, capacity, and files, **so that** I can troubleshoot and verify the doctrines hold.

- `GET /vms`, `GET /vms/<name>`, `GET /images`, `GET /health`.
- State at `~/.local/state/vm-service/` (`state.json`, `state.lock`, `service.log`); per-VM console logs at `/tmp/tart-run-<vm>.log`.
- The install script manages the `com.wezzard.vm-service` LaunchAgent; restart with `launchctl kickstart -k gui/$UID/com.wezzard.vm-service`.

**Tests:** `TestImagesSnapshot` (unit capacity reporting), `TestSnapshots` (integration, live daemon over HTTP), `TestStateMigration` (unit, state file shape).

## US14 — A refused boot fails fast

**As an** operator, **I want** the daemon to detect a guest the hypervisor refused to start within seconds, **so that** a consumed host-wide slot costs seconds of diagnosis, not a 420-second `wait_ip` timeout followed by a vague rollback.

- Apple's 2-macOS-guest limit is host-wide across all Virtualization.framework consumers. If an external tool holds a slot, our `tart run` exits almost immediately — the daemon watches for that early exit and aborts the acquire at once, with an error naming the likely cause (`macOS VM refused to start — the host-wide 2-VM limit may be consumed by another Virtualization.framework user`).
- A guest that boots normally is indistinguishable from the old behavior: no false aborts, `wait_ip` continues as before.
- The failure is a clean rollback (US-N2): VM stopped, deleted, lease record dropped.

**Tests:** `TestRefusedBoot` (unit — Popen stub whose process exits immediately → fail-fast with diagnostic; alive process → no false failure).

## US15 — Capacity distinguishes our leases from host-wide guests

**As an** operator, **I want** the capacity report to separate vm-service's own macOS leases from host-wide Virtualization.framework guests, **so that** I can tell "our two slots are busy" apart from "an external tool took a slot" when the limit trips.

- `GET /images` capacity carries `host_macos_guests` (a pgrep gauge of running `com.apple.Virtualization.VirtualMachine` XPC processes — one per booted macOS guest, regardless of which tool started it) and `foreign_macos_guests` (host total minus our active macOS leases).
- The gauge is **advisory**: it appears in snapshot output and in the limit-tripped 409 message, but never gates acquires. Its snapshot semantics cannot see a foreign boot in flight, so enforcement stays on our own `ACTIVE_STATES` counter (US8).
- QEMU-based tools (e.g. UTM's QEMU backend) do not use Virtualization.framework, do not consume slots, and correctly do not appear in the gauge.

**Tests:** `TestHostGauge` (unit — stubbed process count drives gauge values in snapshot and 409 text).

## US16 — The host-wide limit is a documented shared resource

**As an** operator, **I want** the docs to state plainly that Apple's 2-macOS-guest limit is host-wide across all Virtualization.framework consumers while vm-service enforces only its own share, **so that** future changes (or other tools on this Mac) do not relearn this by a mysterious third-boot failure.

- This page and the README document: the shared-resource semantics, the advisory gauge (US15), the fail-fast diagnosis (US14), the QEMU exception, and that no SIP modification or framework configuration change is ever used to raise the limit (non-goals).

**Tests:** none — documentation contract, pinned by review like the non-goals section.

## US-N4 — Tests run where they can, fast; e2e is explicitly real

**As an** operator, **I want** a test suite with no Tart, SSH, or network dependencies, **so that** I can validate changes in seconds before touching the live daemon.

- Stdlib `unittest` only; no pytest or other dependencies.
- Unit and integration suites stub `tart`, `_ssh`/`_scp`, and `subprocess.Popen` (via a proxy module so `subprocess.run` keeps working) and give every test an isolated temp state dir — no Tart, SSH, or network needed.
- Unit: 44 tests in ~0.1 s. Integration: 35 tests in ~6 s (real daemon process over real HTTP, `vmctl` subprocesses, Tart/SSH stubbed at process level).
- `tests/e2e/` is deliberately NOT stubbed: real Tart clone/boot, real sshpass SSH, real credential injection, run opt-in against the live daemon (`/opt/homebrew/bin/python3 -m unittest discover -s tests/e2e -v`). Every e2e acquire is released in teardown; the macOS-capacity test skips itself when slots are busy.

**Tests:** the unit and integration suites; suite-level properties (speed, isolation, stubbing) are part of the contract.
