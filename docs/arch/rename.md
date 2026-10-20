# Rename and migration (MG)

## Why

- The service no longer leases only virtual machines, so it is renamed machine-service (DC-19), and its CLI is renamed `machctl` (DC-35).
- The rename and every caller's switch to the new names land together in one coordinated milestone, with no long compatibility window (DC-33).

## MG-1: Names

| What | Today | After the rename |
| --- | --- | --- |
| Service | vm-service | machine-service |
| Daemon executable | `vm-service` | `machine-service` |
| CLI executable | `vmctl` | `machctl` |
| Installer | `vm-service-install` | `machine-service-install` |
| Rust crates | `vm-service`, `vm-service-core`, `vm-service-install`, `vmctl` | `machine-service`, `machine-service-core`, `machine-service-install`, `machctl` |
| launchd label | `com.wezzard.vm-service` | `com.wezzard.machine-service` |
| Install folder | `~/.local/libexec/vm-service/` | `~/.local/libexec/machine-service/` |
| State folder | `~/.local/state/vm-service/` | `~/.local/state/machine-service/` |
| Configuration folder | `~/.config/vm-service/` | `~/.config/machine-service/` |
| Logs | `/tmp/com.wezzard.vm-service.{out,err}` | `/tmp/com.wezzard.machine-service.{out,err}` |
| Variables | `VM_SERVICE_*` | `MACHINE_SERVICE_*` |
| Selected-environment profile | `vmctlPath`, export `VMCTL`, `schemaVersion` 1 | `machctlPath`, export `MACHCTL`, `schemaVersion` 2 |
| Consul service | `vm-service` | `machine-service` |
| Catalog description | `services/vm-service.json` | `services/machine-service.json` |
| `/health` answer | `"service": "vm-service"` | `"service": "machine-service"` |
| Repository | `WeZZard/vm-service` | `WeZZard/machine-service` |

- The CLI's name, `machctl`, is close to systemd's `machinectl` on purpose: both are the command-line control of a machine manager. macOS has no `machinectl`, so the names never meet on these Macs (DC-35).
- The daemon keeps the service's own name, because the CLI no longer takes it (DC-36).
- These names do not change, because they belong to contracts shared with other projects or to VM-only parts that need root to reinstall:
  - `VM_ENVIRONMENT_FILE`, which pilot-images and mcp-relay also read. The profile it selects is renamed with everything else (DC-41), and `machctl environment --json` keeps the command's shape.
  - `VM_GUEST_SESSION_AGENT`, `VM_GUEST_CONSOLE_AGENT` and `VM_CONSOLE_WORKER`, which name virtual-machine helpers.
  - The guest firewall's names: the pf anchor `com.apple/250.vm-service`, the anchor file `/etc/pf.anchors/com.wezzard.vm-service`, the table file `/var/db/com.wezzard.vm-service/internet-guests`, and the LaunchDaemon `com.wezzard.vm-service.guest-firewall`.

## MG-2: API

- `/machines` and `/machines/<name>` list and act on machines of both kinds. `/machines/<name>/exec`, `push`, `pull`, `files`, `heartbeat`, `release` and `refresh` are the operations.
- `/vms` and `/vms/<name>` are removed. They answer HTTP 410 Gone, and the error names the `/machines` path that replaces them (DC-37).
- `POST /acquire` stays. It gains the `machine` field for durable machines ([DM-3](durable-machine.md#dm-3-acquire)), and every request that is valid today keeps its meaning.
- `/images`, `/applications`, `/capabilities`, `/acquisition-capabilities` and `/gc` stay. `/images` gains the catalog status and `POST /images/reload` ([IC](image-catalog.md)).

## MG-3: One coordinated switch

- The service, its CLI and every known caller switch to the new names in the same milestone, and they are deployed together (DC-33).
- There is no `/vms` alias, not even within the milestone (DC-37):
  - Every known caller is listed below and switched in the same milestone, so no alias is needed for them.
  - The installer refuses to install while any lease exists, so no lease spans the switch, and no client has an operation in flight on an old path.
  - An alias would keep a missed caller working unnoticed, which is what the coordinated switch exists to prevent. The HTTP 410 answer makes a missed caller fail at once with the fix in its error.
- For the same reason the daemon refuses to start when its environment holds any `VM_SERVICE_*` variable, and names the variable. The installer rewrites the installed `VM_SERVICE_*` values to their `MACHINE_SERVICE_*` names, so a normal installation never hits this refusal.
- A profile at `schemaVersion` 1, or one that carries `vmctlPath`, is refused by every reader with an error that names `machctlPath` and version 2 (DC-42). No tool rewrites a profile.
- No `vmctl` link is installed. The installer removes the `vmctl` link it finds in `~/.local/bin` that points into the old install folder.
- The known callers, and what each changes:

| Caller | Repository | What changes |
| --- | --- | --- |
| mcp-relay's client (`src/vm-service.ts`), doctor and environment names | mcp-relay | `/vms` paths become `/machines`; the health check expects `machine-service`; `src/environment.ts` and the doctor read `machctlPath` at schema 2 and export `MACHCTL`; a new release is published and installed in the same deployment |
| The evaluation runner, through the relay | AnyDict | the pinned relay version; the documents name machine-service and `machctl` |
| The corpus builder's VM client (`utils/shared/vm.mjs`) | AnyDict | `/vms` paths become `/machines`; `VM_SERVICE_URL` becomes `MACHINE_SERVICE_URL` |
| `host/check-clone.py`, `host/environment.py` and the README | pilot-images | `vmctl` becomes `machctl`; the bootstrap reads `machctlPath` at schema 2, and the `VMCTL` export becomes `MACHCTL` |
| `examples/pi-subagents/vm-use.workflow.js` and the README | this repository | `vmctl` becomes `machctl` |
| The `environment` crate and `docs/selected-environments.md` | this repository | the profile schema 2 with `machctlPath` and `MACHCTL`, changed with the code |
| The service-catalog description on 192.168.50.3 | local file | `vm-service.json` is replaced by `machine-service.json` |

## MG-4: Order of migration

1. Build and test the service, the relay release, and the AnyDict and pilot-images changes on their branches, each against a second instance of the renamed service (TL-4).
2. Release every lease on 192.168.50.3.
3. In one sitting: run `machine-service-install install` on 192.168.50.3, which migrates the installation and starts `com.wezzard.machine-service` on port 6240; install the new relay release; merge the AnyDict and pilot-images changes; and replace the catalog description with `machine-service.json`, tagged `vm` and `tart`. Durable exposure stays off on 192.168.50.3 (DC-39).
4. Check that `machctl list`, a relay acquisition and one evaluation row work, and that `GET /vms` answers 410.
5. Later, in M2, install machine-service on 192.168.50.4 and 192.168.50.5 with `MACHINE_SERVICE_HOST=0.0.0.0` and `--expose-durable on`. On a Mac without Tart the installer does not require the virtual-machine helpers (the guest agents, the console worker, the firewall and the forwarder), and the instance offers only its durable machine. Place `machine-service.json`, tagged `durable` and without `vm`, on each.
- An instance with exposure off and no Tart would offer nothing. The installer refuses that combination and names both settings.
