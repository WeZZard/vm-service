# Image catalog hot reload (IC)

## Why

- Before this design, an operator who added, updated or removed an image line had to restart the daemon before the change showed in `/images`, `/applications` and acquisition. The owner ruled that the image list reloads by itself (DC-34).
- The daemon read the image lines in two places, and neither refreshed reliably:
  - At startup, after it owned `daemon.lock`, it executed every `images/*/line.conf` strictly and published one detached association snapshot (image to `{kind, base_vm}`) for `/applications` and acquisition capabilities. A failure left that snapshot unpublished, and `/applications` answered 503 until a restart.
  - `/images` and acquisition read the lines through a cache keyed on the modification time of the `images/` directory. That time changes when a line's folder is added or removed, but not when a `line.conf` is edited. Under a selected environment the cache was never refreshed after startup.
- Other inputs were already read live on every request and stay that way: the portable inventories, the host-local association documents, the base fingerprints (DC-17), and whether each base VM exists in the Tart store, which is what promoting a base changes.

## IC-1: One catalog snapshot

- The daemon keeps one **image catalog snapshot**: for every image line, its kind, base VM, work VM, clone prefix, guest user and resource defaults, with the snapshot's generation number, build time and input identity.
- The snapshot replaces both the startup association snapshot and the line cache. `/images`, `/machines`, `/applications`, `/acquisition-capabilities` and `POST /acquire` all read it.
- The snapshot holds no credential. `GUEST_PASS` stays out of it, as it stayed out of the association snapshot.
- The lease bootstrap still needs each line's guest password. The builder therefore returns the passwords beside the snapshot, and the daemon publishes both in the same swap. The passwords stay in the daemon's memory, and no response, status or log line is built from them (DC-47).
- The live inputs listed above stay live. The snapshot never caches an inventory, an association document, a fingerprint or the existence of a base VM.

## IC-2: Input identity

- The snapshot's inputs are the set of folders under `images/` that contain a `line.conf`, and each of those `line.conf` files.
- The **input identity** is the sorted list of image names, each with its `line.conf`'s inode number, size, modification time in nanoseconds and SHA-256. The device number is left out, because macOS renumbers a volume each time it mounts it (DC-17).
- Computing the identity reads one directory and at most the catalog's maximum number of small files. It executes nothing.
- The identity covers each `line.conf` and nothing it might read. pilot-images writes every key of a `line.conf` literally, so that the file is self-contained data, and the identity relies on that.

## IC-3: When the catalog is rebuilt

- Every request that reads the catalog first computes the input identity and compares it with the snapshot's. When they differ, the request asks for a rebuild and waits for it, up to 30 seconds. This comparison is what makes the catalog correct (DC-38).
- A file watcher asks for a rebuild as soon as a file changes, so that the next request usually finds the catalog already current. The watcher is only an accelerator: a missed or merged event costs one request's wait, never a stale answer.
- The watcher uses kqueue vnode events on `images/`, on each line folder and on each `line.conf`, and it opens them again after every event, because an editor may replace a file. It needs no file-watching library (DC-46).
- When the wait runs out, the request is answered from the last good snapshot with the status `stale`.
- `GET /health` compares the identities and starts a rebuild when they differ, but it never waits for the rebuild, so that a health check stays fast.
- `POST /images/reload` asks for a rebuild even when the files did not change, waits for it, and returns the catalog status. A successful reload publishes a new generation. The CLI offers it as `machctl images --reload`, which then lists the images and exits with status 1 unless the catalog is `current`.
- One rebuild runs at a time. Requests that arrive during a rebuild wait for that same rebuild instead of starting another.
- After a failed rebuild, the daemon tries again only when the input identity changes again, or on `POST /images/reload`. A broken file therefore costs one failed execution, not one per request.

## IC-4: Building and publishing

- A rebuild runs in the daemon that owns `daemon.lock`, the same process and the same trust rule as the startup build. It executes each `line.conf` with the existing strict zsh parser and its 15-second timeout.
- A rebuild applies every rule of the startup build: a parse or source failure, an unreadable `images/` folder, no lines at all, more lines than the maximum, a non-regular `line.conf`, an invalid image or base name, or a kind other than `macos` or `linux` fails the whole rebuild.
- A `CPU` or `MEMORY_MB` value that is not an integer also fails the whole rebuild. An empty or missing value takes the service default of 6 CPUs and 16384 MiB.
- No permissive mode remains. The old lifecycle discovery skipped a broken line and fell back to a built-in list of two lines when it found none, and both behaviours are gone.
- A failure reason never quotes zsh's own diagnostic, because that diagnostic may quote the file's contents, including a password. The reason names the file and the zsh exit status instead.
- The builder assembles a complete new snapshot in private memory. It then replaces the published snapshot in one atomic pointer swap. A reader holds either the old snapshot or the new one, never a mix, and never a snapshot that is still being built.
- The builder checks the input identity again after it has executed the files. When the identity moved during the build, it discards the result and builds again, so a snapshot is never labelled with an identity it was not built from.
- When the files keep moving through five builds in a row, the rebuild fails with the reason "the image files kept changing during the rebuild".
- An acquisition keeps the snapshot it started with until it finishes. A lease whose image line is later removed keeps running, and its heartbeat and release still work, because they read the lease record and not the catalog.

## IC-5: A failed rebuild

- A failed rebuild keeps the last good snapshot published. Requests keep being served from it.
- The failure is loud:
  - The service log gets one `ERROR` line per failed input identity, naming the file and the reason.
  - The same `ERROR` line also goes to the daemon's standard error.
  - `GET /images`, `GET /machines` and `POST /images/reload` carry a `catalog` status object: `generation`, `built_at`, `status` (`current`, `stale` or `unavailable`), and, when the last rebuild failed and no later rebuild has superseded it, `error`, `failed_at` and `file`.
  - `GET /health` carries the same object, so the service catalog's health check and the CLI both show it.
  - `GET /applications` and `GET /acquisition-capabilities` keep their shapes and carry no status object. mcp-relay rejects any unknown field in the application catalog, and the acquisition-capabilities descriptor is a versioned schema (DC-47).
- `stale` means that the files changed and the last good snapshot no longer matches them. The CLI prints a warning line above the rows of `machctl list` and `machctl images` while the catalog is `stale` or `unavailable`. The line names the failing file, the error, the failure time in UTC and the generation still served.
- Only when no good snapshot has ever been built, such as after a startup whose image files are broken, is the status `unavailable`. Then `/images` and `/applications` answer HTTP 503 with the diagnostic, `POST /acquire` answers HTTP 409 with it, and the next change to the files is tried at once, with no restart.
- `GET /machines` keeps answering while the catalog is `unavailable`, because releasing a lease must not depend on the catalog.
- Under a selected environment, a startup whose catalog cannot be built still refuses to start, as before. Later failures keep the last good snapshot, like any other failure.

## IC-6: How this is proved

- Unit tests (TL-1) cover the input identity, the strict builder's refusals, the single-flight rebuild, the identity recheck after a build, the bounded wait, the reload, and the atomic swap: readers running during many swaps only ever see complete snapshots of one generation.
- Unit tests also prove that the watcher reports an edited, a replaced and an added `line.conf`, and that a change is rebuilt before any request asks for it.
- Component tests (TL-2) run the real daemon against a fixture image repository and a fake Tart store, and never restart it:
  - Adding a line's folder makes the image appear in `/images` and `/machines`, and an acquisition of it succeeds.
  - Editing `CPU` in a `line.conf` changes the resource default that `machctl images-show` reports.
  - Removing a line makes it disappear, while a running lease of it can still be released.
  - Renaming the fake work VM to the base name, as promotion does, makes the image acquirable.
  - A syntax error in one `line.conf` keeps the last good list, sets `status: stale`, writes one `ERROR` line, and fixing the file returns `status: current`.
  - A legacy startup with a broken `line.conf` serves with the status `unavailable` and recovers on the next change, and a selected startup with a broken `line.conf` refuses to start.
- The live layer (TL-4) promotes and removes a line on a second instance with real Tart and checks the same answers.
- The acceptance scenarios are AC-39 to AC-42.

## IC-7: No image source

- An instance's image source is the pilot-images checkout that `PILOT_REPO` names, or a selected environment's `imageRepository`. No other path is assumed (DC-94).
- When neither names one, the instance has no image source. It reads no image files and runs no `tart list` for images. Its catalog is `current` with no lines, so nothing warns: `GET /images` answers an empty `images`, `GET /applications` answers an empty `images`, and `GET /machines` lists no offer.
- The start line of the service log says `image source: none`, and `machine-service-install catalog-description` reports `virtual machines: no (no image source: PILOT_REPO is not set)`.
- An image line whose base VM is not built on that Mac stays in the catalog with `base_available: false`, because building the base makes it acquirable without a rebuild (IC-1). `machctl list` leaves such an offer out of its table (DC-92).

