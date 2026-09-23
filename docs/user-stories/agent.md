# Profile: Agent

An AI agent or script that needs a disposable VM for a bounded task. Concrete embodiments: a pi child agent driven through `vmctl`, a Claude Code session, a shell script doing build/test work.

The agent's contract with the service is **lease-based**: it asks for a VM, works inside it, transfers evidence out, and gives the VM back. It never touches `tart` and never holds host credentials beyond what the service injects into the guest.

## US1 — Acquire a fresh VM on demand

**As an** agent, **I want** to request a VM by `purpose` (a short slug that becomes the VM name), **so that** I get a machine dedicated to my task.

- `POST /acquire` or `vmctl acquire --purpose <slug>`.
- Choose `image` (`macos26` or `ubuntu2404`), `env` pack, `ttl_hours`, optional `cpu`/`memory_mb`/`disk_gb` overrides.
- Acquisition is synchronous in both readiness modes. The default verifies key-only command access and a checksum-verified transfer round trip. `wait: false` uses a shorter bootstrap deadline and one command probe, but does not skip those readiness requirements.
- Cloning a macOS base can take minutes. The caller must allow a sufficiently long acquisition timeout.
- VM name: `pilot-mac-<purpose>-<rand>` (macOS) or `pilot-<purpose>-<rand>`.
- Purpose must match `[a-z0-9][a-z0-9-]{0,63}`; one active lease per purpose+image.

**Tests:** `TestAcquireValidation` (unit, validation), `TestAcquireLifecycle` (unit, happy path + exact tart call sequence), `TestLifecycleE2E` (e2e, real clone/boot/exec/release).

## US2 — Work inside the leased VM

**As an** agent, **I want** to run commands and move files, **so that** the VM does useful work and its outputs reach the host.

- `POST /vms/<vm>/exec` accepts `argv` or `script`. The script shell follows the image kind: zsh on macOS and bash on Ubuntu. The positive request timeout defaults to 600 seconds and is not clamped to 3600 seconds.
- `push` host→guest, `pull` guest→host (evidence transfer *before* release).
- Operations require the VM to be in `running` state.

**Tests:** `TestRecordOps` (unit), `TestLifecycleE2E.test_push_pull_roundtrip` (e2e, real file transfer).

## US3 — Credentials arrive automatically

**As an** agent, **I want** gateway credentials present as environment variables when my session starts, **so that** `pi` and Claude Code work without any interactive auth inside the guest.

- Default `env: "default"` injects `~/.config/vm-credentials/default/`: `env.extra` (LiteLLM/Anthropic gateway envs, `GH_TOKEN`) plus optional `git-identity`.
- Delivery is verified: scp return codes are checked and the guest `secrets.zsh` must be non-empty, or the injection reports failure.
- The gateway architecture is transparent to the agent — it just sees working API endpoints.

**Tests:** `TestInjectPack` (unit; one test per failure mode: scp rc, script rc, verification probe, SSH drop), `TestGitIdentity` (unit), `TestCredentialInjectionE2E` (e2e; gateway envs reach the login shell, git identity applied, `env: "none"` stays credential-free).

## US4 — Opt out of credentials

**As an** agent, **I want** `env: "none"` for credential-free clones, **so that** I can run tasks that need no network identity (and prove images stay credential-free).

**Tests:** `TestAcquireValidation.test_env_none_records_no_pack`, `TestInjectPack.test_empty_pack_injects_nothing_and_succeeds` (unit), `TestCredentialInjectionE2E.test_none_pack_is_credential_free` (e2e).

## US5 — Pristine clone every time

**As an** agent, **I want** every acquire to be a fresh COW clone of the golden base, **so that** no state leaks between tasks.

- Only purpose clones are ever booted; the golden bases (`pilot-*-base`) are never started through the service.

**Tests:** `TestAcquireLifecycle.test_full_lifecycle_record_and_tart_calls`, `test_macos_base_never_booted_even_for_macos_line` (unit), `TestDoctrinesE2E.test_macos_base_never_booted` (e2e).

## US6 — Keep long jobs alive

**As an** agent, **I want** to heartbeat my lease, **so that** long tasks are not reclaimed mid-flight.

- `POST /vms/<vm>/heartbeat` with `ttl_hours` (0.1–720) resets the TTL and clears any grace/warn state.

**Tests:** `TestTtlGc` (unit: heartbeat resets, bounds, unknown VM).

## US7 — See capacity before asking

**As an** agent, **I want** `GET /images` (or `vmctl images --json`), **so that** I can pick an image that actually has capacity.

**Tests:** `TestImagesSnapshot` (unit), `TestSnapshots` (integration), `TestServiceUp.test_daemon_and_images` (e2e).

## US17 — Discover acquisition options and inspect applied settings

- As an agent, I want to inspect supported acquisition options without creating a VM and distinguish requested settings from provisioning instructions and observed readiness.
- The former reporting implementation was removed. The new source implements [the acquisition contract](../acquisition-contract.md) without a Tart patch; `test_stock_console.py` and `test_console_http.py` verify its defaults, validation, and read-only discovery.
- Future reporting must preserve ordinary resource defaults, nullable legacy fields, selected environments, and existing leases without invented configuration.

## US18 — Let a human watch the same VM

- As a user, I want a normal vm-service deployment and mcp-vm-relay upgrade to let me watch the agent's existing guest desktop through a standard viewer while Tart remains unmodified.
- Closing or cancelling the viewer must leave the VM and agent running. Managed access must expire independently of long guest operations and delayed VM destruction.
- The previous patched-Tart implementation was rejected and removed. The new source implements [the revised guest-sharing design](../design.md), with platform-specific preparation and no hidden runtime patch.
- `test_stock_console.py`, `test_console_worker.py`, `test_guest_console.py`, and `test_console_http.py` cover the controller, real supervised helper with synthetic processes, guest adapters, and public contract. These are not real VM/viewer acceptance.
- [The console contract](../console-contract.md) defines the source interface. [The acceptance specification](../vnc-acceptance.md) still requires the installed relay path, actual viewer screenshots, live lifecycle behavior, and cleanup. macOS user authentication and display selection remain explicit unverified steps until observed.

## US-N1 — Bounded, predictable consumption

- As an agent, I want explicit defaults and limits so that I can plan a bounded task. TTL defaults to 24 hours and is capped at 720 hours, and macOS concurrency is capped at two leases.
- Execution accepts an explicit positive timeout without a shorter 3600-second backend cap. The caller is responsible for selecting a suitable operation budget and continuing lease renewal.

**Tests:** `TestAcquireValidation.test_ttl_bounds` (unit), `TestImagesSnapshot` (unit), `TestVmctl.test_acquire_old_flag_aliases` (integration).
