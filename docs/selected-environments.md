# Selected environments

A selected environment provides one explicit configuration for the service URL,
image checkout, Tart executable, and mutable storage. It does not add a new
hypervisor or change the default installation. Without a profile, existing
configuration and the legacy `/health` response remain unchanged.

## Select a profile

Set `VM_ENVIRONMENT_FILE` to a JSON profile path, or pass
`--environment FILE` before the CLI command. Relative selector paths are resolved against the invoking directory; paths inside the profile must be absolute. The CLI option takes precedence.
The daemon accepts the same option. An invalid selected profile is an error;
it never falls back to legacy configuration.

```sh
vmctl --environment /absolute/path/environment.json environment --json
vmctl --environment /absolute/path/environment.json images --json
vm-service --environment /absolute/path/environment.json
```

The first command resolves configuration offline. It does not contact the
service, create state directories, acquire locks, or start Tart. Python's normal
interpreter bytecode caching is separate from profile loading; callers that
require no interpreter cache writes can set `PYTHONDONTWRITEBYTECODE=1`.

The profile must contain exactly these fields. Replace every example path with
an existing checkout or executable, or with a dedicated mutable directory.

```json
{
  "schemaVersion": 1,
  "id": "isolated-test",
  "vmServiceUrl": "http://127.0.0.1:6249",
  "imageRepository": "/absolute/checkouts/pilot-images",
  "tartHome": "/absolute/environments/test/tart",
  "serviceStateDir": "/absolute/environments/test/service",
  "imageStateDir": "/absolute/environments/test/images",
  "relayStateDir": "/absolute/environments/test/relay",
  "vmctlPath": "/absolute/libexec/vm-service/vmctl",
  "tartPath": "/absolute/bin/tart"
}
```

- The profile must be at most 65536 bytes, and `id` must match `[a-z0-9][a-z0-9-]{0,47}`.
- Every path must be absolute. Existing symlinks are resolved, including ancestors of directories that do not exist yet. Dangling symlink ancestors are rejected.
- The image repository must exist and contain an `images/` directory. Both executables must be executable regular files.
- Existing mutable roots must be directories. The four roots must be mutually disjoint, and they must not contain or be contained by the image repository.
- Mutable roots must not overlap `~/.tart`, `$XDG_STATE_HOME/vm-service`, `$XDG_STATE_HOME/pilot-images`, `$XDG_STATE_HOME/mcp-vm-relay`, or the retired `$XDG_STATE_HOME/pi-vm-relay`. The corresponding `~/.local/state/` defaults remain protected even when XDG state is redirected.
- The URL must use lowercase `http`, a literal `localhost`, a valid `127.x.x.x` address, or `[::1]`, and an explicit port between 1 and 65535. Credentials, paths, queries, and fragments are rejected. A single trailing slash is removed, and the port is normalized to decimal.
- The profile is trusted local configuration. Its executable paths and image line configuration may execute host code. It is not an authorization credential or a sandbox boundary against another process running as the same user.

## Resolver API and bootstrap output

The `environment` crate exposes these entry points:

- `load_environment(path)` returns `None` when no selector exists, or the bundle described below. It reads the process environment, and `load_environment_with(path, environ)` takes an explicit mapping instead. Explicit selection errors return `EnvironmentError`.
- `canonical_profile(raw, environ)` validates a parsed profile and returns its canonical object without writing files.
- `profile_identity(profile)` computes identity from an already canonical profile.

`vmctl environment --json` prints a JSON object with three fields:

- `profile` contains the canonical profile object.
- `identity` contains `id`, `fingerprint`, `vmServiceUrl`, `imageRepository`, `tartHome`, `serviceStateDir`, `imageStateDir`, and `relayStateDir`.
- `environment` contains the exported string values in the table below.

Without a selected profile, the output is
`{"profile": null, "identity": null, "environment": {}}`.

The fingerprint is SHA-256 over UTF-8 JSON of the canonical profile, with
lexically sorted keys, no whitespace, and unescaped Unicode. Numeric schema
version `1.0` is normalized to integer `1` before hashing.

| Export | Source |
| --- | --- |
| `VM_ENVIRONMENT_FILE` | The canonical profile file path is exported. |
| `VM_ENVIRONMENT_FINGERPRINT` | The resolved fingerprint binds subprocess command sequences to their selected configuration. |
| `PILOT_REPO` | `imageRepository`. |
| `TART_HOME` | `tartHome`. |
| `VM_SERVICE_STATE` | `serviceStateDir`. |
| `PILOT_IMAGES_STATE_DIR` | `imageStateDir`. |
| `VM_RELAY_STATE_DIR` | `relayStateDir`. |
| `VM_RELAY_URL` | `vmServiceUrl`. |
| `VM_SERVICE_HOST` | Parsed loopback hostname or address. |
| `VM_SERVICE_PORT` | Parsed port as a decimal string. |
| `VMCTL` | `vmctlPath`. |
| `TART` | `tartPath`. |

Image tooling can read the trusted absolute `vmctlPath` from the JSON, invoke
that CLI's `environment --json` with `VM_ENVIRONMENT_FILE` set, verify that
`identity.imageRepository` is its own canonical checkout, and apply only the
listed exports. This avoids maintaining another full profile parser.

- A CLI invoked with an inherited `VM_ENVIRONMENT_FINGERPRINT` refuses a changed profile before making API requests. Explicit `--environment` selection takes precedence and starts a new selection.
- Selected image configuration is validated without the legacy hardcoded fallback, and its selection is fixed for the daemon lifetime. Invalid selected configuration prevents the daemon from serving requests.

## Startup ownership and lease binding

Startup injects the resolved paths, executable, URL, and subprocess environment.
The selected values override individual ambient variables. All Tart operations,
including the boot process and deletion, use the selected executable and store.
Application inventory and work-image metadata use the selected image state and
Tart store as well.

Before serving requests or starting GC, selected startup holds both the state
`daemon.lock` and the Tart store `.vm-service-daemon.lock` for the daemon's
lifetime. It binds each root using `.vm-service-environment.json`. The marker
contains the identity object. Existing mismatched or malformed markers are
never overwritten. Existing leases must already carry the selected
`environment_fingerprint`; missing or mismatched binding is rejected.

New lease records persist `environment_fingerprint`. State access, execution,
transfers, heartbeat, release, and GC refuse mismatched records. Legacy startup
also refuses roots bearing selected environment markers, and legacy operations
refuse records carrying a selected fingerprint. A failed check
must be investigated rather than bypassed by deleting markers. Changing any
canonical profile field changes its fingerprint. Use new dedicated roots for
a different environment, or perform an explicit operator-reviewed migration.
There is no automatic migration or marker reset command.

## HTTP identity checks

Selected `/health` responses add `environment: identity`. Requests without a
fingerprint can read health for discovery. `vmctl` verifies health before every
mutating request, and sends `X-VM-Environment-Fingerprint` on every selected
API request.

- A selected service rejects mutation unless the header matches its fingerprint.
- Any service rejects a supplied nonmatching fingerprint, including on GET requests.
- A legacy service rejects selected-environment headers instead of accepting work intended for another environment.

These server-side checks prevent an endpoint replacement between the client's
health request and its POST from silently redirecting work. They are identity
checks, not authentication.

## Validation scope

`tests/unit/test_environment_config.py` tests read-only parsing, canonicalization,
invalid configuration, startup locks and markers, and lease binding.
`tests/integration/test_selected_environment.py` uses a real loopback HTTP server,
the real CLI, and an executable fake Tart to test selection, admission,
acquisition, boot dispatch, heartbeat, release, and backend rejection. Guest SSH
and key provisioning are stubbed. These tests do not prove real hypervisor or
SSH behavior, and they do not start, stop, or restart a production service.
