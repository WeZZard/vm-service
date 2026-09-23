# Rust translation conventions

This document fixes the rules for the Python-to-Rust translation of `vm-service`.
Every crate in this workspace follows it. Where this document and a Python source
file disagree, the Python source file is the behavioral specification and this
document is wrong; raise the conflict instead of guessing.

## Source of truth

- The Python implementation in `../bin/` and its tests in `../tests/` define the
  required observable behavior.
- The translation preserves observable behavior: HTTP request and response
  shapes, status codes, CLI commands and output, the on-disk state and lease-key
  layout, error messages that reach a client, and the security checks.
- The translation does not add features, rename JSON fields, relax validation,
  or "improve" behavior. A behavior that looks wrong is ported as-is and noted.
- The translation does not introduce a dependency on the Python runtime.

## Crate map

| Python source | Rust crate |
|---|---|
| `bin/lease_keys.py` | `crates/lease-keys` (lib `lease_keys`) |
| `bin/environment_config.py`, `bin/environment_runtime.py` | `crates/environment` (lib `environment`) |
| `bin/acquisition_options.py` | `crates/acquisition-options` (lib `acquisition_options`) |
| `bin/control_only.py` | `crates/control-only` (lib `control_only`) |
| `bin/application_catalog.py` | `crates/application-catalog` (lib `application_catalog`) |
| `bin/console_config.py`, `bin/guest_console.py`, `bin/console_sessions.py` | `crates/console` (lib `console`) |
| `bin/vm-service` (state, tart, ssh, packs, operations) | `crates/vm-service-core` (lib `vm_service_core`) |
| `bin/vm-service` (HTTP server, main) | `crates/vm-service` (binary `vm-service`) |
| `bin/vmctl` | `crates/vmctl` (binary `vmctl`) |
| `bin/console_worker.py` | `crates/console-worker` (binary `console-worker`) |
| `bin/guest-console-agent.py` | `crates/guest-console-agent` (binary `guest-console-agent`) |

Dependency direction is acyclic and points downward:

```
guest-console-agent  console-worker  vmctl
        |                               |
        v                               v
   lease-keys <--- console         environment
        ^             ^
        |             |
        +--- vm-service-core ---+
                  |             |
   acquisition-options  control-only  application-catalog
                  |
              vm-service
```

`crates/console` must not depend on `vm-service-core`. `crates/lease-keys` must
not depend on any other workspace crate.

## Errors

- Each crate defines one public error enum with `thiserror`.
- The `Display` text of an error that reaches an HTTP client or CLI user is the
  same text as the Python exception message, including punctuation.
- Python raises bare `RuntimeError`/`ValueError` for many internal failures; map
  those to a crate error, not to `String`.
- Never panic on input supplied by a request, a caller, a file, or a subprocess.
  `unwrap`/`expect` are allowed only for values proven by construction, and the
  reason must be obvious from the line.

## JSON

- Use `serde_json::Value` and `serde_json::Map<String, Value>` for dynamic
  records, mirroring Python `dict`.
- Preserve field names, null-vs-absent distinctions, and integer-vs-float
  distinctions. `serde_json` represents JSON numbers as `u64`/`i64`/`f64`; a
  Python `int` that fits must stay an integer.
- HTTP responses are serialized with 2-space indentation and a trailing newline,
  matching Python `json.dumps(obj, indent=2).encode() + b"\n"`.
- The state file is serialized with sorted keys, 2-space indentation, and a
  trailing newline, matching Python `json.dump(data, f, indent=2, sort_keys=True)`.
- Python escapes non-ASCII in JSON by default (`ensure_ascii=True`);
  `serde_json` does not. Do not re-encode non-ASCII unless a contract requires
  exact bytes. The application-catalog digest covers raw file bytes, so it is
  unaffected.

## Time, IDs, hashing, randomness

- Times are Unix seconds as `f64` from `SystemTime::now()`. Monotonic deadlines
  use `Instant`.
- UUIDs are v4, lowercase hex. `uuid::Uuid::new_v4().simple().to_string()` gives
  the 32-character hex form Python calls `.hex`.
- Hashes are lowercase hex SHA-256 (`sha2`).
- Random bytes come from the OS. `rand` 0.10 is available; prefer
  `getrandom`-backed APIs over a seeded PRNG for security-relevant material.

## Subprocesses

- Use `std::process::Command`. Never build a shell string when an argv vector is
  possible. The one exception is the SSH remote command, which is already a
  server-side shell string in Python (`shlex.join`/literal script) and must stay
  byte-identical.
- Capture stdout and stderr separately (`output()`), combine them the same way
  Python does for each call site.
- Timeouts: Rust has no `subprocess.run(timeout=...)`. Use
  `child.wait_timeout()`-style waiting from a small helper, or
  `std::process::Child` plus `try_wait` polling. Do not silently drop the
  timeout.
- Environment overrides are explicit. `SUBPROCESS_ENV`, when configured, is the
  full environment for `tart`; do not inherit implicitly.

## Files, permissions, locking

- Use `libc` for `flock`, `lstat`/`fstat` mode, `uid`, `nlink`, and directory
  creation modes. `std::fs` alone does not expose them.
- Every file-permission and symlink check in Python is security-relevant and
  must be ported exactly, including the order of checks and the "fail closed"
  behavior.
- Atomic state writes use a temporary file in the same directory, then
  `std::fs::rename`.
- Path comparisons use canonicalized absolute paths where Python calls
  `.resolve()`, and lexical absolute paths where Python calls `.absolute()`.

## Console guest agent embedding

- `crates/guest-console-agent` builds a native binary. Unlike the Python
  original (`bin/guest-console-agent.py`, which ran on any guest that had an
  interpreter), the port ships machine code, so the artifact must match the
  *guest's* operating system rather than the host's. `crates/console` resolves
  it at run time:
  1. `VM_GUEST_CONSOLE_AGENT` if set — explicit override, wins for every kind,
  2. otherwise `guest-console-agent-<kind>` (`linux`/`macos`) next to the
     running executable, when that artifact exists,
  3. otherwise `guest-console-agent` next to the running executable — the
     host-native build, correct only when the guest OS matches the host.
- `scripts/build-guest-agents.sh` produces the host-native artifact plus a
  cross-built Linux ELF (`aarch64-unknown-linux-musl`). A Linux guest cannot
  execute the macOS build: the probe fails with `Exec format error`, which is
  indistinguishable from a slow guest and therefore surfaces as a pending
  `probe_unanswered` until the prepare deadline, rolling the lease back.
- A native binary can exceed the per-argument limit, so it is not embedded in the
  SSH argument vector. `Manager::prepare` uploads the agent bytes once per lease
  over SSH standard input to `$HOME/.vm-service-console/agent`, with the
  directory and file mode `0700` and an atomic temporary file plus rename.
- `probe_command` and `stream_command` then run
  `"$HOME/.vm-service-console/agent" <kind> probe|serve`.
- The guest agent reads exactly one JSON configuration line from its standard
  input. After that line, binary RFB reuses the same standard input and standard
  output, so nothing else may be prepended to stdin. The worker writes the serve
  configuration as that one line.
- `crates/console` launches the native `console-worker` binary, located by
  `VM_CONSOLE_WORKER` or as a sibling of the running executable, with
  `--control-fd <fd>`.

## Tests

- Port the corresponding Python unit tests in `../tests/unit/` into Rust tests
  in the crate, under `#[cfg(test)] mod tests` or `crates/<crate>/tests/`.
- Use `tempfile::TempDir` for filesystem fixtures and `std::process::Command`
  for fake executables, as the Python tests do.
- Test names and intent follow the Python test, not necessarily its spelling.
- `cargo test --workspace` must pass before a crate is considered done.
- Do not delete or weaken an assertion to make a test pass. If a Python test
  depends on running `tart` or a real VM, port the parts that do not and mark the
  live part `#[ignore]` with the reason.

## Style

- Rust edition 2021, `cargo fmt`, `cargo clippy` clean where practical.
- Public items have doc comments. Module-level `//!` comments carry the Python
  module docstring's meaning.
- No `unsafe` except thin, commented `libc` wrappers.

## Interface contracts

These signatures are the cross-crate contract. A crate that owns an item below
must expose it exactly; a crate that consumes it must not reach around it.
Internal helpers stay private.

### `lease-keys`

```rust
pub enum LeaseKeyError { /* ... */ }

pub fn create(state_dir: &Path, vm: &str) -> Result<PathBuf, LeaseKeyError>;
pub fn directory(state_dir: &Path, vm: &str) -> Result<PathBuf, LeaseKeyError>;
pub fn key_args(key_dir: &Path) -> Result<Vec<String>, LeaseKeyError>;
pub fn bootstrap(
    ip: &str, user: &str, password: &str, key_dir: &Path, timeout_s: f64,
) -> Result<(), LeaseKeyError>;
pub fn cleanup(state_dir: &Path, vm: &str) -> Result<(), LeaseKeyError>;
```

### `environment`

```rust
pub enum EnvironmentError { /* ... */ }

pub const MARKER: &str = ".vm-service-environment.json";

pub fn canonical_profile(
    raw: &Value, environ: &HashMap<String, String>,
) -> Result<Value, EnvironmentError>;
pub fn profile_identity(profile: &Value) -> Value;
pub fn load_environment(path: Option<&str>) -> Result<Option<Value>, EnvironmentError>;
pub fn load_environment_with(
    path: Option<&str>, environ: &HashMap<String, String>,
) -> Result<Option<Value>, EnvironmentError>;

pub struct Ownership { /* RAII: holds both flocks, writes both markers */ }
pub fn ownership(config: &Value) -> Result<Ownership, EnvironmentError>;
```

### `acquisition-options`

```rust
pub enum ResolveError { /* Display equals the Python ValueError text */ }

pub fn resolve(
    cpu: Option<i64>, memory_mb: Option<i64>, disk_gb: Option<i64>,
    wait: bool, ttl_hours: &Value, vnc: bool,
) -> Result<Value, ResolveError>;
pub fn descriptor(backends: &Value, images: Option<&Value>) -> Value;
```

### `control-only`

```rust
pub enum Rejected { /* Display equals the Python Rejected text */ }

pub fn validate_request(body: &Value) -> Result<String, Rejected>;
pub fn require_available() -> Result<(), Rejected>;
pub fn descriptor() -> Value;
pub fn digest(path: &Path) -> Result<String, Rejected>;   // if retained
```

### `application-catalog`

```rust
pub enum CatalogError { /* includes unreadable filesystem variants */ }

pub const MAX_IMAGES: usize = 64;

pub fn fingerprint_base(
    base_root: &Path, base_vm: &str, os: &str,
) -> Result<Value, CatalogError>;
pub fn load_catalog(
    pilot_repo: &Path,
    lines: &Value,
    base_root: Option<&Path>,
    diagnostic: Option<&dyn Fn(&str)>,
) -> Result<Value, CatalogError>;
```

### `console`

```rust
pub enum ConsoleConfigError { /* ... */ }
pub struct ConsoleConfig { /* trusted viewer configuration */ }
pub fn load_config(
    path: Option<&str>, environ: Option<&HashMap<String, String>>,
) -> Result<Option<ConsoleConfig>, ConsoleConfigError>;
pub fn availability(kind: &str, config: Option<&ConsoleConfig>) -> bool;

pub enum ConsoleError { /* ... */ }
pub fn identity(value: &Value) -> bool;
pub struct Manager { /* inert until constructed with a config */ }
impl Manager {
    pub fn new(config: Option<ConsoleConfig>) -> Self;
    pub fn capabilities(&self) -> Value;
    pub fn require_available(&self, kind: &str) -> Result<(), ConsoleError>;
    pub fn reserve(&self, record: &Map<String, Value>) -> Result<(), ConsoleError>;
    pub fn prepare(
        &self, record: &Map<String, Value>, key_dir: &Path, timeout_s: u64,
    ) -> Result<Map<String, Value>, ConsoleError>;
    pub fn resolve(
        &self, record: &Map<String, Value>, lease_id: Option<&str>,
    ) -> Result<Map<String, Value>, ConsoleError>;
    pub fn open(
        &self, record: &Map<String, Value>,
        lease_id: &str, console_id: &str, attempt_id: &str,
    ) -> Result<Value, ConsoleError>;
    pub fn cancel(
        &self, record: &Map<String, Value>,
        lease_id: &str, console_id: &str, attempt_id: &str,
    ) -> Result<Value, ConsoleError>;
    pub fn renew(&self, record: &Map<String, Value>, deadline: Option<Instant>);
    pub fn revoke(&self, record: &Map<String, Value>, reason: &str);
    pub fn forget(&self, vm: &str);
    pub fn shutdown(&self);
}
```

### `console-worker` and `guest-console-agent`

```text
console-worker --control-fd <fd>                     # see bin/console_worker.py main()
guest-console-agent linux|macos probe|serve          # serve reads one config JSON line on stdin, then RFB
```

Credentials travel inside the private control stream, not on a second
file descriptor.
