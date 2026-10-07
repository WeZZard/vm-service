# Glossary

- **Guest link:** the connection between vm-service and a leased VM on the same Mac. Today every exec opens one `ssh` process and every push or pull one `scp` process.
- **Client link:** the connection between a client (mcp-vm-relay, vmctl, a test client) and vm-service. Today it is one HTTP request per operation.
- **Guest session:** one long-lived byte stream to a small helper program in the guest. vm-service sends many operations over it instead of starting one SSH connection per operation.
- **Guest session agent:** the helper program at the guest end of a guest session. It runs commands, writes files and reads files. It is a native binary, uploaded per lease like the guest console agent.
- **Channel:** the way vm-service obtains the byte stream of a guest session. The SSH stdio channel runs the agent under `ssh -T`. The Tart exec channel runs it under `tart exec -i`, which reaches the guest over vsock through Tart's guest agent.
- **vsock:** a virtual socket between a host and its VMs that does not use the VM's network.
- **Tart guest agent:** `tart-guest-agent`, Cirrus Labs' program inside a guest that serves `tart exec`. It is not part of vm-service.
- **Frame:** one unit of the guest session protocol: a header, a payload and a CRC-32 checksum. A frame that fails its checksum ends the session.
- **Remote session:** a client link carried over one WebSocket connection, which can be resumed on a new connection after the old one breaks.
- **Resume:** reattaching a new connection to an existing remote session. Each side resends the messages the other side has not acknowledged.
- **Grace period:** how long vm-service keeps a disconnected remote session, its unacknowledged messages and its running operations, waiting for the client to resume.
- **Heartbeat:** WebSocket ping and pong frames sent on an idle connection, so that a silently dead connection is detected within a bound.
- **Execution ID:** a client-chosen identifier for one operation. vm-service runs an operation with a given execution ID at most once.
- **At most once:** an operation either ran once or did not run. It is never run a second time because a link failed.
- **Uncertain:** the outcome reported when a link failed after an operation was sent and before its result arrived. The caller must not resend the operation; it can query the outcome by execution ID.
- **Conformance suite:** one set of tests that every channel must pass.
- **Fault proxy:** a test program between two ends of a link that injects resets, silent hangs, latency and corruption.
- **Second instance:** a vm-service built from a worktree and run on another port with its own state directory, so live tests leave the installed service alone.
