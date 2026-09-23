//! JSON-file lease state with cross-process and cross-thread exclusion.
//!
//! Mirrors the Python `_Flock` and `State` classes. The mutex serializes
//! threads; the `flock` serializes processes. Slow Tart operations run outside
//! any lock and publish through [`State::update`].

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::config::ThreadLock;
use crate::error::{OpError, OpResult};

thread_local! {
    /// Per-thread nesting depth for the re-entrant file lock.
    static FLOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

// Test-only store failure injection. Python patched `STATE._store` to raise
// `OSError`; integration tests link this crate without `cfg(test)`, so the seam
// is gated on `debug_assertions` instead: it exists in every dev and test
// build, defaults to the real store, and is compiled out entirely in release.
// The override is per-thread so parallel tests never observe each other's
// failure.
#[cfg(debug_assertions)]
thread_local! {
    static STORE_FAILURE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Make the next `State::store` calls on this thread fail with `message`.
///
/// Passing `None` restores the real store. This is `debug_assertions`-gated, so
/// it does not exist in release builds and never changes production behaviour.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn set_store_failure(message: Option<&str>) {
    STORE_FAILURE.with(|cell| *cell.borrow_mut() = message.map(str::to_string));
}

/// A re-entrant, cross-process exclusive lock on a lock file.
pub struct Flock {
    fd: Option<std::os::fd::RawFd>,
}

impl Flock {
    /// Acquire the lock, or no-op when this thread already holds it.
    pub fn acquire(path: &Path) -> OpResult<Self> {
        let depth = FLOCK_DEPTH.with(|cell| cell.get());
        FLOCK_DEPTH.with(|cell| cell.set(depth + 1));
        if depth > 0 {
            return Ok(Self { fd: None });
        }
        let result = Self::open_and_lock(path);
        if result.is_err() {
            FLOCK_DEPTH.with(|cell| cell.set(depth));
        }
        result
    }

    fn open_and_lock(path: &Path) -> OpResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(OpError::from)?;
        }
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| OpError::new("invalid lock file path"))?;
        // SAFETY: `c_path` is a valid NUL-terminated string for the duration of
        // the call. `open` returns a fresh descriptor or -1.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(OpError::new(format!(
                "cannot open lock file {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: `fd` is an open descriptor owned by this guard.
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            // SAFETY: `fd` is open and not otherwise used.
            unsafe { libc::close(fd) };
            return Err(OpError::new("cannot acquire state lock"));
        }
        Ok(Self { fd: Some(fd) })
    }
}

impl Drop for Flock {
    fn drop(&mut self) {
        let depth = FLOCK_DEPTH.with(|cell| cell.get());
        let next = depth.saturating_sub(1);
        FLOCK_DEPTH.with(|cell| cell.set(next));
        if next == 0 {
            if let Some(fd) = self.fd.take() {
                // SAFETY: `fd` is an open descriptor owned by this guard.
                unsafe {
                    libc::flock(fd, libc::LOCK_UN);
                    libc::close(fd);
                }
            }
        }
    }
}

/// The JSON lease state, matching the Python `State` class.
pub struct State {
    state_dir: PathBuf,
    state_file: PathBuf,
    lock_file: PathBuf,
    /// The selected environment fingerprint expected on every record.
    fingerprint: Option<String>,
    mu: ThreadLock<()>,
}

impl State {
    /// Construct state rooted at the resolved configuration paths.
    pub fn new(
        state_dir: PathBuf,
        state_file: PathBuf,
        lock_file: PathBuf,
        fingerprint: Option<String>,
    ) -> Self {
        Self {
            state_dir,
            state_file,
            lock_file,
            fingerprint,
            mu: Mutex::new(()),
        }
    }

    /// Reject a record that belongs to a different selected environment.
    pub fn require_lease_environment(&self, record: &Value) -> OpResult<()> {
        let observed = record
            .get("environment_fingerprint")
            .and_then(Value::as_str);
        if observed != self.fingerprint.as_deref() {
            return Err(OpError::new(
                "lease environment fingerprint mismatch; refusing operation",
            ));
        }
        Ok(())
    }

    /// Load and normalize the state, mirroring `State._load`.
    fn load(&self) -> OpResult<Map<String, Value>> {
        let bytes = match std::fs::read(&self.state_file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(empty_state());
            }
            Err(_) => return self.load_failure(),
        };
        let mut data: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => return self.load_failure(),
        };
        // CPython's `json` normalizes float literals on load (`0.10` becomes
        // `0.1`, `1e2` becomes `100.0`); `arbitrary_precision` otherwise keeps
        // whatever text the file carried.
        environment::python_json::normalize_numbers(&mut data);
        // Python's `_load` calls `data.get('vms', {}).values()`, so a document
        // whose root is not an object, or whose `vms` member is not an object,
        // raises `AttributeError` rather than yielding an empty state. Treat
        // those shapes as a load failure instead of discarding the file.
        if !data.is_object() || !data.get("vms").map(Value::is_object).unwrap_or(true) {
            return self.load_failure();
        }
        if data.get("vms").is_none() {
            let mut state = Map::new();
            state.insert("vms".to_string(), Value::Object(Map::new()));
            return Ok(state);
        }
        let Some(vms) = data.get_mut("vms").and_then(Value::as_object_mut) else {
            return self.load_failure();
        };
        for record in vms.values_mut() {
            self.require_lease_environment(record)?;
        }
        for record in vms.values_mut() {
            let Some(object) = record.as_object_mut() else {
                continue;
            };
            rename(object, "line", "image");
            rename(object, "line_kind", "image_kind");
            if !object.contains_key("env") {
                if object.contains_key("pack") {
                    rename(object, "pack", "env");
                } else if object.contains_key("lane") {
                    rename(object, "lane", "env");
                }
            }
        }
        Ok(data.as_object().cloned().unwrap_or_else(empty_state))
    }

    /// Legacy mode silently discards unreadable state; a selected environment
    /// refuses to operate on state it cannot trust.
    ///
    /// The underlying read or parse error deliberately stays out of the message.
    /// Python raises `OpError('selected service state is unreadable or invalid')
    /// from error`, so the text an HTTP client sees carries no detail; the
    /// original error survives only in the exception chain.
    fn load_failure(&self) -> OpResult<Map<String, Value>> {
        if self.fingerprint.is_some() {
            Err(OpError::new(
                "selected service state is unreadable or invalid",
            ))
        } else {
            Ok(empty_state())
        }
    }

    /// Atomically write the state through a temporary file and rename.
    fn store(&self, data: &Map<String, Value>) -> OpResult<()> {
        #[cfg(debug_assertions)]
        if let Some(message) = STORE_FAILURE.with(|cell| cell.borrow().clone()) {
            return Err(OpError::new(message));
        }
        std::fs::create_dir_all(&self.state_dir)?;
        // Python writes the state with `sort_keys=True` at every level. Sorting
        // explicitly keeps that guarantee independent of the `serde_json`
        // `preserve_order` feature, which another workspace crate enables.
        let canonical = sorted(Value::Object(data.clone()));
        let pretty = serde_json::to_string_pretty(&canonical)
            .map_err(|error| OpError::new(error.to_string()))?;
        let mut text = python_ensure_ascii(&pretty);
        text.push('\n');
        let tmp = self.state_file.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.state_file)?;
        Ok(())
    }

    /// Atomic read-modify-write with an optional post-commit publication hook.
    ///
    /// `after_commit` runs only after persistence succeeds, while the same
    /// locks still exclude release and renewal. It must not perform blocking
    /// external I/O.
    pub fn update<T, F, A>(&self, mutate: F, after_commit: Option<A>) -> OpResult<T>
    where
        F: FnOnce(&mut Map<String, Value>) -> OpResult<T>,
        A: FnOnce(&Map<String, Value>),
    {
        let _guard = self
            .mu
            .lock()
            .map_err(|_| OpError::new("state mutex poisoned"))?;
        let _lock = Flock::acquire(&self.lock_file)?;
        let mut data = self.load()?;
        let out = mutate(&mut data)?;
        self.store(&data)?;
        if let Some(after) = after_commit {
            after(&data);
        }
        Ok(out)
    }

    /// Read a consistent snapshot of the state.
    pub fn read(&self) -> OpResult<Map<String, Value>> {
        let _guard = self
            .mu
            .lock()
            .map_err(|_| OpError::new("state mutex poisoned"))?;
        let _lock = Flock::acquire(&self.lock_file)?;
        self.load()
    }
}

/// Recursively rebuild a value with object keys in ascending order.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                let entry = map.get(&key).cloned().unwrap_or(Value::Null);
                out.insert(key, sorted(entry));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Escape a serialized JSON document the way `json.dump` does by default.
///
/// Python's `json` escapes with `ensure_ascii=True`, so every code point
/// outside U+0020 through U+007E becomes a `\uXXXX` escape and an astral code
/// point becomes a UTF-16 surrogate pair. `serde_json` leaves non-ASCII raw and
/// does not escape U+007F. A serialized document only contains non-ASCII inside
/// string literals, so scanning the whole text is safe.
fn python_ensure_ascii(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        let code = character as u32;
        if code <= 0x7e {
            escaped.push(character);
            continue;
        }
        if code > 0xffff {
            let adjusted = code - 0x10000;
            let high = 0xd800 + (adjusted >> 10);
            let low = 0xdc00 + (adjusted & 0x3ff);
            escaped.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
        } else {
            escaped.push_str(&format!("\\u{code:04x}"));
        }
    }
    escaped
}

fn empty_state() -> Map<String, Value> {
    let mut state = Map::new();
    state.insert("vms".to_string(), Value::Object(Map::new()));
    state
}

fn rename(object: &mut Map<String, Value>, from: &str, to: &str) {
    if !object.contains_key(to) {
        if let Some(value) = object.remove(from) {
            object.insert(to.to_string(), value);
        }
    }
}
