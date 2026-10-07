# Test layers (TL)

Each behaviour is tested on the lowest layer that contains everything the behaviour depends on. A higher layer repeats a check only when it adds something real: a real process, a real network or a real VM. Acceptance criteria are in [acceptance](../acceptance/index.md).

| Layer | Guest link (GS) | Client link (RS) | Runs in |
| --- | --- | --- | --- |
| TL-1 Unit | frames, CRC-32, the agent's request loop, client outcomes | session state machine, replay, deduplication, heartbeat timers | `cargo test --workspace` |
| TL-2 Component integration | the real agent binary as a local process; the real daemon with a fake `ssh` that runs the agent locally | the real daemon and a Rust test client through a fault proxy | `cargo test --workspace` |
| TL-3 Network lab | none | real silent packet loss, address change, an HTTPS-only proxy | test VMs with network conditioning, opt-in |
| TL-4 Live VM | both channels against macOS 26 and Ubuntu 24.04 VMs | none | second instance, `#[ignore]` |
| TL-5 End to end | Pi's traced Reminders run against the 2026-10-07 baseline | the same run with added network latency | Pi repository, opt-in |

## TL-1: Unit

- Frames: encode and decode every frame type; reject a bad magic, an oversized length, a short read and every single-bit flip of the checksum's input.
- Agent loop over in-memory pipes: exec output and exit codes, a script on standard input, a timeout, `cancel`, write then read with SHA-256, a directory read answered by `is_directory`, and exit when input closes.
- Client outcomes: for each row of [GS-5](guest-session.md#gs-5-failure-outcomes-at-most-once), a scripted fake stream that fails at that point produces exactly the outcome in the table, and the number of requests sent is counted.
- Remote session state machine: a randomized test drops, duplicates and delays envelopes, cuts connections at random points and resumes. Every call completes exactly once or is reported uncertain, and no operation runs twice.

## TL-2: Component integration

- Guest link: the `guest-session` crate's tests start the real `guest-session-agent` binary as a child process and drive it through the client. Faults: kill the agent during an exec, flip a byte in its output stream, and move a 64 MiB file each way.
- Guest link through the daemon: the `vm-service` tests start the real daemon with a fake `ssh` that runs its remote command locally, with `HOME` set to a temporary guest root. The daemon uploads the agent, opens one session and serves many execs, pushes and pulls over it. The fake `ssh` counts how many connections were opened.
- Client link: the `vm-service` tests start the real daemon and connect a Rust test client through an in-process fault proxy. Faults: reset, a silent hang that keeps the connection open, latency and jitter, a reconnect from a new source port, and a daemon restart.

## TL-3: Network lab

- The lab runs in test VMs leased from vm-service, where the test has root. The client runs in one VM and the daemon's remote session listener runs in another, so the faults sit on a real network path between them.
- macOS VMs condition the network with the system's packet filter and traffic shaper (`pfctl` and `dnctl`, the mechanism under Network Link Conditioner): delay, loss, and a silent drop of all packets. Linux VMs use `tc netem` and `iptables`.
- An address change moves the client VM to another address during a session. An HTTPS-only path is a proxy in a VM that allows only `CONNECT` to port 443.
- The lab holds at most one macOS VM at a time, because a host runs at most two macOS guests and other work shares them. Faults that need no macOS behaviour run in Linux VMs.
- An agent runs the lab's test plan and records results and timings under `test-evidence/`.

## TL-4: Live VM

- Live tests are `#[ignore]` with a reason, as `rust/CONVENTIONS.md` requires. They run against second instances built from the worktree, each on its own port and state directory, and send commands directly to their own test VMs.
- Two instances run at once, one per guest link (6250 for the session link, 6251 for `ssh`), so a parity case compares two leases of one image without restarting a daemon. `rust/scripts/live-instance.sh` starts each instance in its own session: stopping the shell that started it must not signal the `tart run` processes of its VMs.
- Every live test checks that the installed daemon on 6240 kept its process and holds no test lease (AC-18).
- The conformance suite runs the same cases over the SSH stdio channel and the Tart exec channel, on a macOS 26 image and an Ubuntu 24.04 image with the Tart guest agent installed.
- Cases: exec parity with the current `ssh` link (output, exit code, `PATH`, user and home); a 64 MiB output and file each way with matching SHA-256; the guest network turned off during a session; a VM restart and a stale Tart control socket; release during a running command; round-trip time per operation.
- The report records each case's result and timings per channel, image and Tart version under `test-evidence/`, which is ignored by git.

## TL-5: End to end

- Pi's traced Reminders run (`PI_SECRETARY_VM_USE_TRACE`) is repeated with mcp-vm-relay on the new links. Its step intervals are compared with the 2026-10-07 baseline of a 4.37 s mean between actions, with and without added latency on the client link.
- Each step interval is split into the model's time, the guest's own work and the transport's time. Only the transport's time is in scope; the model's time and the guest's work set the floor.
- The work is done when the transport's time is the smallest of the three parts, both without and with added latency on the client link. The run's report states the floor, so the result is read against what the system can reach rather than against a guessed number.
