//! Port of `bin/environment_runtime.py`: startup-only ownership of the selected
//! service state and Tart storage.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::EnvironmentError;

/// Name of the per-root marker file binding a root to an environment identity.
pub const MARKER: &str = ".vm-service-environment.json";

/// Name of the Tart store lock file.
pub const STORE_LOCK: &str = ".vm-service-daemon.lock";

/// RAII ownership of both selected roots.
///
/// Holding an `Ownership` means both `flock`s are held and both marker files
/// have been read or bound. Dropping it releases the locks; the marker files
/// are never removed or replaced.
pub struct Ownership {
    _locks: Vec<File>,
}

/// Hold the state/store locks and bind both roots; never replace a marker.
pub fn ownership(config: &Value) -> Result<Ownership, EnvironmentError> {
    let profile = config
        .get("profile")
        .ok_or_else(|| EnvironmentError::msg("environment config has no profile"))?;
    let identity = config
        .get("identity")
        .ok_or_else(|| EnvironmentError::msg("environment config has no identity"))?;
    let state = path_component(profile, "serviceStateDir")?;
    let store = path_component(profile, "tartHome")?;

    // Locks are acquired store-first to serialise writers over the Tart store,
    // then state. The `Vec` owns the open files; any early return drops it and
    // releases the flocks.
    let mut locks: Vec<File> = Vec::new();
    for (root, name) in [(&store, STORE_LOCK), (&state, "daemon.lock")] {
        fs::create_dir_all(root).map_err(|error| {
            EnvironmentError::msg(format!("cannot create {}: {error}", root.display()))
        })?;
        let lock_path = root.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(|error| {
                EnvironmentError::msg(format!("cannot open {}: {error}", lock_path.display()))
            })?;
        let metadata = file.metadata().map_err(|error| {
            EnvironmentError::msg(format!("cannot stat {}: {error}", lock_path.display()))
        })?;
        if !metadata.is_file() {
            return Err(EnvironmentError::msg("nonregular daemon lock"));
        }
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(EnvironmentError::msg(format!(
                    "another vm-service daemon owns {}",
                    root.display()
                )));
            }
            return Err(EnvironmentError::msg(format!(
                "cannot lock {}: {error}",
                lock_path.display()
            )));
        }
        locks.push(file);
    }

    let markers = [state.join(MARKER), store.join(MARKER)];
    for marker in &markers {
        read_marker(marker, identity)?;
    }

    let state_file = state.join("state.json");
    if state_file.exists() {
        let valid = validate_leases(&state_file, identity);
        if valid.is_err() {
            return Err(EnvironmentError::msg(
                "selected service state is invalid or has unbound leases",
            ));
        }
    }

    for marker in &markers {
        bind_marker(marker, identity)?;
    }

    Ok(Ownership { _locks: locks })
}

fn path_component(profile: &Value, key: &str) -> Result<PathBuf, EnvironmentError> {
    profile
        .get(key)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| EnvironmentError::msg(format!("profile has no {key}")))
}

/// Read and validate a marker. Returns `Ok(false)` when it does not exist.
fn read_marker(path: &Path, identity: &Value) -> Result<bool, EnvironmentError> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(EnvironmentError::msg(format!(
                "cannot read environment marker: {}: {error}",
                path.display()
            )))
        }
    };
    let metadata = file.metadata().map_err(|error| {
        EnvironmentError::msg(format!(
            "cannot stat environment marker: {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(EnvironmentError::msg(format!(
            "nonregular environment marker: {}",
            path.display()
        )));
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|_| {
        EnvironmentError::msg(format!("invalid environment marker: {}", path.display()))
    })?;
    let mut actual: Value = serde_json::from_str(&text).map_err(|_| {
        EnvironmentError::msg(format!("invalid environment marker: {}", path.display()))
    })?;
    crate::python_json::normalize_numbers(&mut actual);
    if &actual != identity {
        return Err(EnvironmentError::msg(format!(
            "environment marker mismatch: {}",
            path.display()
        )));
    }
    Ok(true)
}

/// Create-and-bind a marker if absent, else validate the existing one.
fn bind_marker(path: &Path, identity: &Value) -> Result<(), EnvironmentError> {
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            read_marker(path, identity)?;
            return Ok(());
        }
        Err(error) => {
            return Err(EnvironmentError::msg(format!(
                "cannot bind environment marker: {}: {error}",
                path.display()
            )))
        }
    };
    let mut encoded = serde_json::to_vec(&sorted_object(identity)).unwrap_or_default();
    encoded.push(b'\n');
    file.write_all(&encoded).map_err(|error| {
        EnvironmentError::msg(format!(
            "cannot write environment marker: {}: {error}",
            path.display()
        ))
    })?;
    file.flush().map_err(|error| {
        EnvironmentError::msg(format!(
            "cannot flush environment marker: {}: {error}",
            path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        EnvironmentError::msg(format!(
            "cannot sync environment marker: {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn sorted_object(value: &Value) -> Value {
    match value.as_object() {
        Some(object) => {
            let sorted: BTreeMap<&String, &Value> = object.iter().collect();
            serde_json::to_value(sorted).unwrap_or(Value::Null)
        }
        None => value.clone(),
    }
}

/// `Ok(())` when every lease is bound to this environment fingerprint.
fn validate_leases(state_file: &Path, identity: &Value) -> Result<(), ()> {
    let text = fs::read_to_string(state_file).map_err(|_| ())?;
    let mut data: Value = serde_json::from_str(&text).map_err(|_| ())?;
    crate::python_json::normalize_numbers(&mut data);
    let records = data.get("vms").and_then(Value::as_object).ok_or(())?;
    let fingerprint = identity.get("fingerprint");
    for record in records.values() {
        if record.get("environment_fingerprint") != fingerprint {
            return Err(());
        }
    }
    Ok(())
}
