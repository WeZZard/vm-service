# Image catalog hot reload (IC)

## Why

- Today an operator who adds, updates or removes an image line must restart the daemon before the change shows in `/images`, `/applications` and acquisition. The owner ruled that the image list reloads by itself (DC-34).
- The daemon reads the image lines in two places today, and neither refreshes reliably:
  - At startup, after it owns `daemon.lock`, it executes every `images/*/line.conf` strictly and publishes one detached association snapshot (image to `{kind, base_vm}`) for `/applications` and acquisition capabilities. A failure leaves that snapshot unpublished, and `/applications` answers 503 until a restart.
  - `/images` and acquisition read the lines through a cache keyed on the modification time of the `images/` directory. That time changes when a line's folder is added or removed, but not when a `line.conf` is edited. Under a selected environment the cache is never refreshed after startup.
- Other inputs are already read live on every request and stay that way: the portable inventories, the host-local association documents, the base fingerprints (DC-17), and whether each base VM exists in the Tart store, which is what promoting a base changes.

## IC-1: One catalog snapshot

- The daemon keeps one **image catalog snapshot**: for every image line, its kind, base VM, work VM, clone prefix, guest user and resource defaults, with the snapshot's generation number, build time and input identity.
- The snapshot replaces both the startup association snapshot and the line cache. `/images`, `/machines`, `/applications`, `/acquisition-capabilities` and `POST /acquire` all read it.
- The snapshot holds no credential. `GUEST_PASS` stays out of it, as it stays out of the association snapshot today.
- The live inputs listed above stay live. The snapshot never caches an inventory, an association document, a fingerprint or the existence of a base VM.

## IC-2: Input identity

- The snapshot's inputs are the set of folders under `images/` that contain a `line.conf`, and each of those `line.conf` files.
- The **input identity** is the sorted list of image names, each with its `line.conf`'s inode number, size, modification time in nanoseconds and SHA-256. The device number is left out, because macOS renumbers a volume each time it mounts it (DC-17).
- Computing the identity reads one directory and at most the catalog's maximum number of small files. It executes nothing.
- The identity covers each `line.conf` and nothing it might read. pilot-images writes every key of a `line.conf` literally, so that the file is self-contained data, and the identity relies on that.

## IC-3: When the catalog is rebuilt

- Every request that reads the catalog first computes the input identity and compares it with the snapshot's. When they differ, the request asks for a rebuild and waits for it, up to 30 seconds. This comparison is what makes the catalog correct (DC-38).
- A file watcher on `images/` (FSEvents) asks for a rebuild as soon as a file changes, so that the next request usually finds the catalog already current. The watcher is only an accelerator: a missed or merged event costs one request's wait, never a stale answer.
- `POST /images/reload` asks for a rebuild and returns the catalog status. The CLI offers it as `machctl images --reload`.
- One rebuild runs at a time. Requests that arrive during a rebuild wait for that same rebuild instead of starting another.
- After a failed rebuild, the daemon tries again only when the input identity changes again, or on `POST /images/reload`. A broken file therefore costs one failed execution, not one per request.

## IC-4: Building and publishing

- A rebuild runs in the daemon that owns `daemon.lock`, the same process and the same trust rule as the startup build today. It executes each `line.conf` with the existing strict zsh parser and its 15-second timeout.
- A rebuild applies every rule of the startup build: a parse or source failure, an unreadable `images/` folder, no lines at all, more lines than the maximum, a non-regular `line.conf`, an invalid image or base name, or a kind other than `macos` or `linux` fails the whole rebuild.
- The builder assembles a complete new snapshot in private memory. It then replaces the published snapshot in one atomic pointer swap. A reader holds either the old snapshot or the new one, never a mix, and never a snapshot that is still being built.
- The builder checks the input identity again after it has executed the files. When the identity moved during the build, it discards the result and builds again, so a snapshot is never labelled with an identity it was not built from.
- An acquisition keeps the snapshot it started with until it finishes. A lease whose image line is later removed keeps running, and its heartbeat and release still work, because they read the lease record and not the catalog.

## IC-5: A failed rebuild

- A failed rebuild keeps the last good snapshot published. Requests keep being served from it.
- The failure is loud:
  - The service log gets one `ERROR` line per failed input identity, naming the file and the reason.
  - Every response that reads the catalog carries a `catalog` status object: `generation`, `built_at`, `status` (`current`, `stale` or `unavailable`), and, when the last rebuild failed, `error`, `failed_at` and the failing file.
  - `GET /health` carries the same `status`, so the service catalog's health check and the CLI both show it.
- `stale` means that the files changed and the last good snapshot no longer matches them. The CLI prints a warning line above the machine list while the catalog is stale.
- Only when no good snapshot has ever been built, such as after a startup whose image files are broken, is the status `unavailable`. Then catalog requests answer HTTP 503 with the diagnostic, and the next change to the files is tried at once, with no restart.
- Under a selected environment, a startup whose catalog cannot be built still refuses to start, as today. Later failures keep the last good snapshot, like any other failure.

## IC-6: How this is proved

- Unit tests (TL-1) cover the input identity, the strict builder's refusals, the single-flight rebuild, the identity recheck after a build, and the atomic swap: readers running during many swaps only ever see complete snapshots of one generation.
- Component tests (TL-2) run the real daemon against a fixture image repository and a fake Tart store, and never restart it:
  - Adding a line's folder makes the image appear in `/images` and `/machines`, and an acquisition of it succeeds.
  - Editing `CPU` in a `line.conf` changes the resource default that `machctl images-show` reports.
  - Removing a line makes it disappear, while a running lease of it can still be released.
  - Renaming the fake work VM to the base name, as promotion does, makes the image acquirable.
  - A syntax error in one `line.conf` keeps the last good list, sets `status: stale`, writes one `ERROR` line, and fixing the file returns `status: current`.
- The live layer (TL-4) promotes and removes a line on a second instance with real Tart and checks the same answers.
- The acceptance scenarios are AC-39 to AC-42.
