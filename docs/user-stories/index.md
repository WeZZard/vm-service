# vm-service — User Stories

Consolidated, test-linked user stories for the lease-based VM service. Each story names its **actor profile** (see the profile pages linked below) and the tests that pin it down in `tests/unit/`, `tests/integration/`, and `tests/e2e/`.

## Actor profiles

| Profile | Page | Summary |
|---------|------|---------|
| **Agent** | [agent.md](agent.md) | An AI agent (pi child, Claude Code, scripts) that needs a disposable VM for a bounded task and must hand it back when done. |
| **Orchestrator** | [orchestrator.md](orchestrator.md) | A coordinating layer (pi-subagents workflow, CI job, daemon) that owns the VM lifecycle on behalf of children and guarantees teardown even when a child fails. |
| **Operator** | [operator.md](operator.md) | The human (WeZZard) who installs, inspects, and troubleshoots the service and enforces the host doctrines. |

## Story index

| ID | Actor | Story | Covered by |
|----|-------|-------|------------|
| US1 | Agent | Acquire a fresh VM on demand, by purpose, with a chosen image and TTL | `TestAcquireValidation`, `TestAcquireLifecycle` |
| US2 | Agent | Run commands, push files in, pull artifacts out of the leased VM | `TestRecordOps` |
| US3 | Agent | Credentials arrive automatically as env vars (gateway era) | `TestInjectPack`, `TestGitIdentity` |
| US4 | Agent | Opt out of application credentials (`env: "none"`); lease SSH key still required | `TestAcquireValidation.test_env_none_records_no_pack`, `TestInjectPack.test_empty_pack_injects_nothing_and_succeeds` |
| US5 | Agent | Get a *pristine* clone every time — no state leaks between tasks | `TestAcquireLifecycle.test_full_lifecycle_record_and_tart_calls` |
| US6 | Agent | Long jobs keep the lease alive by heartbeating | `TestTtlGc` |
| US7 | Agent | See the service's capacity before asking (`GET /images`) | `TestImagesSnapshot` |
| US8 | Orchestrator | Host doctrines are enforced mechanically, not by convention | `TestConcurrency`, `TestAcquireLifecycle` |
| US9 | Orchestrator | Teardown is guaranteed even if the orchestrator dies | `TestTtlGc` |
| US10 | Orchestrator | Own the lifecycle: acquire, delegate, collect evidence, release | `examples/pi-subagents/vm-use.workflow.js` (E2E) |
| US11 | Operator | Drive the service from a CLI without hand-crafting HTTP | `TestVmctl` (integration) |
| US12 | Operator | Stable, backward-compatible API naming (`image`/`env`) | `TestHttpApi.test_acquire_old_field_aliases_on_the_wire` (integration), `TestStateMigration.test_lane_alias_migrated` + `test_migration_idempotent` (unit) |
| US13 | Operator | Inspect what the service is doing and where its files live | `TestImagesSnapshot` (unit), `TestSnapshots` (integration) |
| US14 | Operator | A hypervisor-refused boot fails fast with a useful diagnosis | `TestRefusedBoot` (unit) |
| US15 | Operator | Capacity reporting distinguishes our leases from host-wide framework guests | `TestHostGauge` (unit) |
| US16 | Agent / Operator | A fresh lease uses its own host-held SSH key; readiness verifies key-only command and transfer paths; cleanup retains keys until destruction | `test_lease_keys.py`, `test_key_lifecycle.py`; [contract](../lease-ssh-keys.md) |
| US17 | Agent / Operator | Clients can discover acquisition options and distinguish applied settings from readiness. | New-source tests are `test_stock_console.py` and `test_console_http.py`; see the [contract](../acquisition-contract.md). |
| US18 | Agent / Human observer | The source supports stock-Tart guest sharing while viewer closure leaves the VM running; installed/live acceptance remains pending. | Tests cover the controller, worker, guest adapter, installer, and relay consumer; see the [console contract](../console-contract.md) and [acceptance specification](../vnc-acceptance.md). |
| US-N1 | Agent | Bounded resource consumption and predictable defaults | `TestAcquireValidation.test_ttl_bounds` (unit), `TestImagesSnapshot` (unit) |
| US-N2 | Orchestrator | Failure at any provisioning step rolls back cleanly | `TestAcquireLifecycle.test_failure_rolls_back_clone_and_state`, `test_ssh_never_ready_rolls_back` (unit) |
| US-N3 | Orchestrator | Concurrency correctness: races never oversubscribe or double-lease | `TestConcurrency` (unit) |
| US-N4 | Operator | Tests run fast and offline where possible; e2e explicitly real | unit + integration suites: no Tart/SSH/network, seconds; `tests/e2e/`: real Tart/SSH, opt-in |

## Non-goals (deliberately unimplemented)

- No bridged networking, no `-base`/`-seed`/`-work` boots, no credentials written into golden images — refused by design (US8).
- No raising of Apple's host-wide 2-macOS-guest limit (no SIP modification, no framework configuration changes). vm-service enforces its own share and *reports* external consumption (US14–US16); it cannot and will not prevent other Virtualization.framework users from booting guests.
- No task registry management: the AGENTS.md human-readable registry table is maintained by the acquiring agent, not by the service (see README).
- No multi-host scheduling: this service manages *this* Mac's Tart only.
