# Machine identity and renames (MI)

## Why

- Every computer on the network that installs `machctl` finds every instance and builds on the machines they offer. mcp-relay and the evaluation runners are such consumers. A Mac's owner may rename it at any time, in System Settings, and nothing that consumes machine-service may break when they do (DC-86).
- Today the Mac's name is its identity everywhere. The daemon reads it once at startup (MM-2), the installer copies it into the Consul description, clients confirm an instance by it, a durable lease is addressed by it, and consumers have written it into their own code. A rename is therefore ignored until the daemon restarts, and then breaks every one of those places at once:
  - a held durable lease can no longer be reached by any request, release included, until its TTL ends, and the Mac shows as leased to everyone else meanwhile;
  - `machctl list` drops an instance whose advertised name and `/health` name differ, as an impostor;
  - mcp-relay takes the "not found" for the end of its durable lease, and after its own restart refuses the guest, release included;
  - a Consul registration keeps the old name until the instance is reinstalled, and every client in Consul mode rejects the instance;
  - a consumer that wrote the name into its code stops working.
- This section separates the two: a permanent ID that identifies a Mac, and a name that is only a label and follows the Mac live.

## MI-1: The permanent ID and the name

- A Mac's **machine ID** is its hardware UUID, the `IOPlatformUUID` that a durable machine already reports as `hardware_uuid` (MM-2). It never changes while the Mac exists, whatever the Mac is called.
- An instance is identified by its machine ID and its port. Two instances on one Mac, such as a test instance beside the installed one (DC-40), share the machine ID and differ by port.
- The Mac's **name** is its local host name, `scutil --get LocalHostName`, as today. It is the label people read and type. It identifies nothing on its own.
- Every place that states an instance states both:
  - `/health` answers `instance` (the name, as today) and `machine_id`;
  - the Bonjour TXT record adds `machine_id` beside `txtvers=1`, `instance` and `kinds`. A new key keeps `txtvers=1`, because a client ignores keys it does not know (RFC 6763 §6.7);
  - each record in `/machines` carries `machine_id`;
  - the Consul description states `machine_id` instead of the name (MI-4).
- A durable machine's name is the Mac's current name, as MM-2 sets. A virtual machine's name does not contain the Mac's name and is not affected by a rename.

## MI-2: Following a rename live

- The daemon subscribes to the system's notification for a change of the local host name (the System Configuration dynamic store key for host names). When it fires, the daemon reads the new name and, in one step:
  - withdraws its Bonjour advertisement, which sends the goodbye for the old name, and registers it again under the new name with the same TXT record apart from `instance`;
  - answers the new name in `/health` and `/machines` from then on;
  - remembers the old name as a former name (MI-3);
  - logs the old and the new name.
- The daemon also reads the name every 30 seconds, so a failed subscription or a missed notification follows the same change path.
- TL-2 tests set `MACHINE_SERVICE_NAME_FILE` to a UTF-8 file containing the local host name and set `MACHINE_SERVICE_NAME_POLL_MS` to a positive polling interval in milliseconds. Startup and every notification or poll read the same source. An unreadable or empty file does not replace a valid name. Tests edit this file and never change the real Mac's host name (DC-91).
- The host name inside the advertisement's address record, such as `Mac-A.local.`, is already renamed by the system's DNS-SD service; the daemon does nothing for it.
- No restart is needed, and a restart after a rename behaves the same as a live rename (MI-3).

## MI-3: A lease keeps the name it was granted under

- A durable lease records the name it was granted under, `granted_name`, in its lease record on disk (DM-1). The name of a request, `/machines/<name>/...`, matches the durable machine when it is the current name, or when it is the `granted_name` of the current lease and the request carries that lease's `lease_id`. Every request a holder makes keeps working through a rename and through a restart after one, until the lease is released.
- The `granted_name` is the name at the moment the lease is granted. A lease that sees two renames keeps the name it began with.
- New acquisitions use the current name only.
- The daemon keeps its former names, each with the time it was left, in its state directory, for 7 days. A request or an acquisition for a former name outside a held lease is refused with 404, and the message names the current name: `Mac-A was renamed Mac-B on 2026-11-01; use Mac-B`.
- `/health` lists the former names under `former_names`, an array of objects with `name` and `renamed_at` (Unix seconds). A client can resolve an instance named by its old name, such as `<vm>@Mac-A`. The state directory also records the last observed current name, so a rename while the daemon is stopped produces the same former-name history when it starts. A lease written before `granted_name` existed uses its persisted `name` as the granted name (DC-91).

## MI-4: Consul registration

- The description file that `machine-service-install` writes for the service-catalog registrar states `machine_id` in its metadata, and no name. The machine ID never changes, so the file never goes stale and needs no reinstall after a rename.
- A client in Consul mode confirms a registration by its `machine_id` and takes the instance's name from its `/health`. A registration that states only the older `instance` key is confirmed by that name, as today, and the CLI says on standard error that reinstalling refreshes it.
- The Consul node name stays independent of both, as DC-80 sets.

## MI-5: Clients

- These rules hold for every client: `machctl`, mcp-relay, and any consumer that finds instances itself.
- **Confirming an instance.** A client confirms an instance it found by `machine_id`, which must match between the advertisement or registration and `/health`. A different name with the same `machine_id` is a rename, not an impostor: the client keeps the instance and uses the name `/health` answers. A different `machine_id` is still refused, as FD-2 sets.
- **A second look.** When a client cannot find a named machine or instance, it browses once more after 2 seconds before it fails, to cover the moment between a goodbye and the new advertisement.
- **Former names.** A client resolves an instance named by a former name through `former_names`, and says on standard error what the name became.
- **Saved leases.** A client that remembers leases, such as `machctl`'s `cli-leases.json` (CL-3), keys them by machine ID and lease ID, and keeps addressing a durable lease by its `granted_name`.
- **No written names.** A consumer never writes a Mac's name or address into its code or configuration. It names an instance only as the operator gives it at run time, or as `machctl list --json` reports it.

## MI-6: What a rename does not change

- Open connections: a client connects to an address, and a rename does not change the Mac's address. A command that runs, a transfer and a download continue.
- Lease IDs, virtual machine names, durable workspaces and the CI runner do not contain the Mac's name.
- A change of the Mac's IP address is a different event, not covered here. A client that keeps an instance's address across requests, such as mcp-relay during a session, finds the instance again by its `machine_id` when the address stops answering.

## MI-7: Consumers

| Consumer | What it changes |
| --- | --- |
| machine-service | MI-1 to MI-3: report `machine_id`, follow the rename, keep `granted_name` and former names |
| `machine-service-install` | MI-4: write `machine_id` into the Consul description |
| `machctl` | MI-5: confirm by `machine_id`, look twice, resolve former names, key saved leases by machine ID |
| mcp-relay | MI-5, and its own release check ([CN-1](consumers.md#cn-1-mcp-relay)) |
| Evaluation runners | MI-5: no written names or addresses; instances only from the operator or from `machctl` |
| pilot-images | none: it leases virtual machines by image through `machctl` |
