# Hygiene and CI coexistence (HY)

## Why

- A virtual machine is clean because every lease gets a fresh clone. A durable machine cannot be cloned, so a lease on it inherits whatever earlier work left behind.
- The owner ruled that a session runs as the admin account and that hygiene is a pragmatic best effort (DC-23). Hygiene therefore tells a client what state the machine was in and what the lease changed. It does not promise a clean machine.
- The approach generalizes what the evaluation runner already does for its licensed persistent VM: a workspace, a sweep of leftover processes, and an inventory before and after ([CN-2](consumers.md#cn-2-the-evaluation-runner)).

## HY-1: Best effort, not isolation

- Hygiene has three parts: a scratch workspace ([HY-2](#hy-2-scratch-workspace)), a leftover-process sweep ([HY-3](#hy-3-leftover-process-sweep)), and an inventory before and after the lease ([HY-4](#hy-4-inventory-before-and-after)) (DC-28).
- Hygiene reports. It never refuses an acquisition, never blocks a release, and never undoes a change outside the workspace.
- A failure in any hygiene step is recorded with its reason in the lease's hygiene record.

## HY-2: Scratch workspace

- Every durable lease starts with an empty workspace, `~/machine-service/leases/<lease_id>/`, which is the default working directory and the session's `TMPDIR` ([DM-1](durable-machine.md#dm-1-a-lease-is-a-session-and-a-workspace)).
- Release removes the workspace unless the client asks to keep it ([DM-7](durable-machine.md#dm-7-release)).

## HY-3: Leftover-process sweep

- At release, after the lease's own process groups are stopped, the service ends the processes that the lease left running.
- A process is a leftover when it runs as `wezzard`, it started after the lease began, and one of these holds:
  - It belongs to a process session that the lease started, or it descends from one.
  - Its environment carries `MACHINE_SERVICE_LEASE_ID` with this lease's ID. macOS 27 does not let a process read another process's environment, so there this rule finds nothing, and a process that left the lease's sessions after its parents exited is not swept (DC-48).
  - It is a regular application, one with a Dock presence, that was not running in the inventory before the lease. Applications started through Launch Services, for example with `open`, are children of launchd and inherit no session variable, so this rule catches them.
- A leftover application is first asked to quit with an Apple event and is given 10 seconds. A leftover process then receives `SIGTERM`, and after 10 more seconds `SIGKILL`.
- The sweep never ends these processes, whatever the rules above say:
  - The CI runner and every process that descends from it ([HY-5](#hy-5-coexistence-with-the-ci-runner)).
  - Any launchd job's own process that the lease did not start, such as Consul, the service-catalog registrar and machine-service itself.
  - Finder, the Dock, SystemUIServer and the other processes of the login session.
- The rules are applied to one snapshot of the processes taken before any signal, so stopping a session never hides a descendant that left it.
- The hygiene record lists every swept process with its PID, executable, start time, the rule that matched it, and whether it ended.

## HY-4: Inventory before and after

- The service takes one inventory at acquisition and one at release, after the sweep.
- An inventory records:
  - The running processes of `wezzard`, with executable and start time.
  - The running regular applications.
  - The installed applications in `/Applications` and `~/Applications`, with bundle version.
  - The launchd jobs in `~/Library/LaunchAgents`, `/Library/LaunchAgents` and `/Library/LaunchDaemons`.
  - The Homebrew formulae and casks, the global npm packages, and `wezzard`'s crontab.
  - The top-level entries of the home folder, `~/Library/Application Support` and `~/Library/Preferences`, with their modification times.
  - The free disk space and the `software` section of the capability report ([MM-3](machines.md#mm-3-capability-report)).
- Each part of the inventory has its own timeout of 60 seconds. A part that fails or times out is recorded as missing, and the others are kept.
- The service compares the two inventories and records what was added, removed and changed. The CI runner's processes and files are marked with their origin, so a client does not attribute a CI job's work to the lease.
- The lease record keeps `inventory-before.json`, `inventory-after.json` and `changes.json` under the lease's state folder. The release response carries `changes.json` and the paths of the full inventories.
- `GET /machines/<name>/leases/<lease_id>/hygiene` returns the hygiene record of a current or recent lease for 30 days.

## HY-5: Coexistence with the CI runner

- The readycheck-dev GitHub Actions runner on 192.168.50.4 and 192.168.50.5 runs as `wezzard`, as the LaunchAgent `com.github.actions.runner`, from `~/actions-runner`. machine-service never pauses, stops or reconfigures it (DC-25).
- A lease may start while a CI job runs, and a CI job may start during a lease. They share the CPU, the memory, the GPU, the disk and the desktop.
- `ci_runner.busy` in the capability report says whether a job is running when the client asks ([MM-3](machines.md#mm-3-capability-report)). A client that measures performance, or that drives the desktop, can prefer an idle machine. A client that must wait for an idle machine does so itself; the service never makes a busy runner a refusal.
- During a lease, the service samples whether a CI job runs once a minute and records the busy intervals in the hygiene record. A client can then judge a measurement taken while a job ran.
- The sweep, the workspace and the inventory never touch `~/actions-runner`. The runner's nightly `com.wezzard.cleanup-runner` job, which removes `~/actions-runner/_work/*/*/target`, is likewise left alone.

## HY-6: Free disk

- Each Mac mini at 192.168.50.4 and 192.168.50.5 has a 228 GiB system volume with about 125 GiB free. A task that installs an engine can use a large share of it.
- The capability report always carries the free disk space. Below the configured warning level, `MACHINE_SERVICE_DISK_WARN_GB`, the machine record carries the warning `low free disk`. The warning never makes the machine unavailable.
