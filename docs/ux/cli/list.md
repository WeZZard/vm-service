# Machine list and acquisition

## CL-1: The machine list

- `machctl list` discovers the instances ([FD-2](../../arch/federation.md#fd-2-discovery)) and prints one table of every machine and every virtual machine offer ([FD-3](../../arch/federation.md#fd-3-aggregation)).
- Rows are grouped by instance, with the local instance first. Inside an instance the durable machine comes first, when the instance exposes one, then the running virtual machines, then the offers.
- An instance with durable exposure off has no durable row. 192.168.50.3 is such an instance today (DC-39).
- An unreachable instance keeps one row, which names the error and its Consul health status.
- `--json` prints the merged list as JSON: one object per instance with its `instance`, `address`, `reachable`, `error` and `machines`, where each machine is the record of [MM-3](../../arch/machines.md#mm-3-capability-report).
- `--local` lists only the local instance. `--kind durable` or `--kind virtual` filters by kind. `--require <capability>` filters by capability, as CL-2 describes.

```text
$ machctl list
INSTANCE         MACHINE                         KIND     STATE      LEASE (PURPOSE, UNTIL)        CHIP / OS           GPU            FREE DISK  CI
mac-mini-m4-pro  pilot-mac-x-session-7f3a        virtual  running    x-session, 18:40              macOS 26 (macos26)  mac2,common3   -          -
mac-mini-m4-pro  macos26                         virtual  offer      acquirable 1 of 2             macOS 26            mac2,common3   -          -
mac-mini-m4-pro  ubuntu2404                      virtual  offer      acquirable                    Ubuntu 24.04        -              -          -
mac-mini-m4-1    mac-mini-m4-1                   durable  leased     unreal-post-process, 21:05    M4 / macOS 26.3     metal3         128 GiB    busy
mac-mini-m4-2    mac-mini-m4-2                   durable  available  -                             M4 / macOS 26.3     metal3         124 GiB    idle
```

- When an instance does not answer, its machines are replaced by one row:

```text
mac-mini-m4-2    -                               -        unreachable  connection refused (Consul: critical)
```

- The `GPU` column prints `metal3` when the machine supports Metal 3, and otherwise its families.
- The `CI` column prints `busy` or `idle` for a Mac with a CI runner and `-` for one without.
- A warning, such as `low free disk` or a missing permission, is printed under its row, indented.
- When an instance's image catalog is `stale` or `unavailable` ([IC-5](../../arch/image-catalog.md#ic-5-a-failed-rebuild)), a warning line above that instance's rows names the failing file and the time of the failure.

## CL-2: Acquire by capability

- `machctl acquire --purpose <slug> --require metal3 --require unreal` asks for any machine that has every listed capability.
- The capabilities a client can require are names over the capability report: `metal3`, `durable`, `virtual`, `unity`, `unreal`, `xcode`, `cua-driver`, `claude-code`, `codex`, `screen-recording` and `ci-idle`.
- The CLI lists the machines, keeps those that match, and prefers in order: an available durable machine whose CI runner is idle, then any available durable machine, then a virtual machine offer that is acquirable. It acquires from the instance that owns the first candidate and tries the next candidate when that acquisition is refused for capacity.
- `--machine <name>` acquires a named durable machine, and `--image <image>` acquires a virtual machine, as `vmctl acquire` did. `--instance <ID>` chooses the instance for an image.
- The CLI prints the machine it chose and why, before the lease, so a person can see the choice: `chose mac-mini-m4-2 (durable, metal3, CI idle)`.

## CL-3: Naming a machine

- `exec`, `push`, `pull`, `heartbeat`, `release` and `status` take a machine name. A durable machine's name is unique. A virtual machine may be written `<vm>@<instance>`, and the CLI requires that form when two instances have a machine of that name.
- The CLI finds the owning instance from the merged list and remembers the `lease_id` of the leases it acquired in `~/.local/state/machine-service/cli-leases.json`, so a person does not type it. `--lease-id` overrides it.
- `push` and `pull` to a remote instance send the bytes ([FD-4](../../arch/federation.md#fd-4-working-on-a-remote-instance)). The command line is the same as for a local instance.
