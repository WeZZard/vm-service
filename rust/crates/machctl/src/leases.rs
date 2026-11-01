//! The leases this CLI acquired, remembered so a person does not type a
//! `lease_id` (`docs/ux/cli/list.md` CL-3, DC-72).
//!
//! The file is `cli-leases.json` in the service state folder:
//! `$MACHINE_SERVICE_STATE`, or `~/.local/state/machine-service/`. It maps
//! each lease to its machine and the instance that owns it, is written with
//! mode 0600 under an exclusive `flock` on a sibling lock file, and is
//! replaced by a rename, so two `machctl` processes never lose each other's
//! entries and a reader never sees half a file. An acquisition adds an
//! entry, a release removes it, and `--lease-id` overrides it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The file's name inside the state folder.
pub const FILE_NAME: &str = "cli-leases.json";

/// The remembered-leases file, from the environment.
pub fn path() -> Option<PathBuf> {
    if let Some(state) = std::env::var_os("MACHINE_SERVICE_STATE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(state).join(FILE_NAME));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".local/state/machine-service")
            .join(FILE_NAME),
    )
}

/// One remembered lease.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub machine: String,
    pub instance: String,
    pub lease_id: String,
    /// Permanent identity; absent only in older lease files (MI-5).
    pub machine_id: Option<String>,
    /// Durable requests retain this label until release (MI-3, DC-89).
    pub granted_name: Option<String>,
}

fn entries(document: &Value) -> Vec<Lease> {
    document
        .get("leases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(Lease {
                machine: entry.get("machine")?.as_str()?.to_string(),
                instance: entry.get("instance")?.as_str()?.to_string(),
                lease_id: entry.get("lease_id")?.as_str()?.to_string(),
                machine_id: entry
                    .get("machine_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string),
                granted_name: entry
                    .get("granted_name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

fn read(path: &Path) -> Vec<Lease> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|document| entries(&document))
        .unwrap_or_default()
}

/// The lease remembered for `machine` on the instance at `instance`. A
/// machine remembered on exactly one other address still matches, because
/// an instance's address may be written two ways (an ID or a URL).
pub fn find(path: &Path, machine: &str, instance: &str) -> Option<String> {
    let leases = read(path);
    if let Some(lease) = leases
        .iter()
        .find(|lease| lease.machine == machine && lease.instance == instance)
    {
        return Some(lease.lease_id.clone());
    }
    let named: Vec<&Lease> = leases.iter().filter(|l| l.machine == machine).collect();
    match named.as_slice() {
        [only] => Some(only.lease_id.clone()),
        _ => None,
    }
}

/// Resolve a remembered lease against its confirmed owner. New entries use
/// machine ID plus lease ID; legacy entries retain their name/address lookup
/// until the next acquisition (MI-5, DC-90).
pub fn resolve(
    path: &Path,
    machine: &str,
    instance: &str,
    machine_id: Option<&str>,
    lease_id: Option<&str>,
) -> Option<Lease> {
    let leases = read(path);
    if let (Some(machine_id), Some(lease_id)) = (machine_id, lease_id) {
        if let Some(lease) = leases.iter().find(|lease| {
            lease.machine_id.as_deref() == Some(machine_id) && lease.lease_id == lease_id
        }) {
            return Some(lease.clone());
        }
    }
    let named: Vec<_> = leases
        .into_iter()
        .filter(|lease| {
            (lease.machine == machine || lease.granted_name.as_deref() == Some(machine))
                && machine_id
                    .zip(lease.machine_id.as_deref())
                    .is_none_or(|(expected, saved)| expected == saved)
                && lease_id.is_none_or(|id| lease.lease_id == id)
        })
        .collect();
    named
        .iter()
        .find(|lease| lease.instance == instance)
        .cloned()
        .or_else(|| match named.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        })
}

/// Change the file under its lock: `change` edits the list in place.
fn update(path: &Path, change: impl FnOnce(&mut Vec<Lease>)) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let lock_path = parent.join(format!(".{FILE_NAME}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| format!("{}: {error}", lock_path.display()))?;
    // SAFETY: `flock` on a descriptor this function owns; the lock ends when
    // `lock` is dropped.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "{}: {}",
            lock_path.display(),
            std::io::Error::last_os_error()
        ));
    }
    let mut leases = read(path);
    change(&mut leases);
    let document = json!({
        "schema_version": 2,
        "leases": leases
            .iter()
            .map(|lease| json!({
                "machine": lease.machine,
                "instance": lease.instance,
                "lease_id": lease.lease_id,
                "machine_id": lease.machine_id,
                "granted_name": lease.granted_name,
            }))
            .collect::<Vec<_>>(),
    });
    let temporary = parent.join(format!(".{FILE_NAME}.{}", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut file: File = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(serde_json::to_string_pretty(&document)?.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(format!("{}: {error}", path.display()));
    }
    drop(lock);
    Ok(())
}

/// Remember a lease under machine ID plus lease ID (MI-5, DC-90). Older
/// entries without identity retain the compatible machine/address key.
pub fn remember(path: &Path, lease: Lease) -> Result<(), String> {
    update(path, |leases| {
        leases.retain(|old| match (&old.machine_id, &lease.machine_id) {
            (Some(old_id), Some(new_id)) => old_id != new_id || old.lease_id != lease.lease_id,
            _ => old.machine != lease.machine || old.instance != lease.instance,
        });
        leases.push(lease);
    })
}

/// Forget the lease `lease_id`.
pub fn forget(path: &Path, lease_id: &str) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    update(path, |leases| leases.retain(|old| old.lease_id != lease_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(machine: &str, instance: &str, id: &str) -> Lease {
        Lease {
            machine: machine.to_string(),
            instance: instance.to_string(),
            lease_id: id.to_string(),
            machine_id: None,
            granted_name: None,
        }
    }

    /// CL-3: an acquisition is remembered by machine and instance with mode
    /// 0600, a newer lease of the same machine replaces it, and a release
    /// forgets it.
    #[test]
    fn cl3_remember_find_forget() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("state").join(FILE_NAME);
        assert_eq!(find(&path, "mac-b", "http://b:6240"), None);
        remember(&path, lease("mac-b", "http://b:6240", "one")).unwrap();
        remember(&path, lease("pilot-x", "http://127.0.0.1:6240", "two")).unwrap();
        remember(&path, lease("mac-b", "http://b:6240", "three")).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            find(&path, "mac-b", "http://b:6240").as_deref(),
            Some("three")
        );
        // Written another way, a machine remembered once still matches.
        assert_eq!(find(&path, "mac-b", "mac-b").as_deref(), Some("three"));
        forget(&path, "three").unwrap();
        assert_eq!(find(&path, "mac-b", "http://b:6240"), None);
        assert_eq!(
            find(&path, "pilot-x", "http://127.0.0.1:6240").as_deref(),
            Some("two")
        );
        // Two machines of one name on two instances need the instance.
        remember(&path, lease("shared", "http://a:6240", "a")).unwrap();
        remember(&path, lease("shared", "http://b:6240", "b")).unwrap();
        assert_eq!(find(&path, "shared", "http://c:6240"), None);
        assert_eq!(find(&path, "shared", "http://b:6240").as_deref(), Some("b"));
    }
    /// MI-5/DC-90: renamed labels and moved addresses replace one permanent
    /// key; different lease IDs remain distinct and old entries still load.
    #[test]
    fn mi5_remembered_leases_are_keyed_by_machine_and_lease_ids() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join(FILE_NAME);
        let mut first = lease("Mac-A", "http://192.0.2.1:6240", "lease-a");
        first.machine_id = Some("machine-a".into());
        first.granted_name = Some("Mac-A".into());
        remember(&path, first.clone()).unwrap();
        let mut renamed = first.clone();
        renamed.machine = "Mac-B".into();
        renamed.instance = "http://192.0.2.2:6240".into();
        remember(&path, renamed.clone()).unwrap();
        assert_eq!(read(&path), vec![renamed.clone()]);
        let mut second = renamed.clone();
        second.lease_id = "lease-b".into();
        remember(&path, second).unwrap();
        assert_eq!(read(&path).len(), 2);
        assert_eq!(
            resolve(&path, "Mac-B", "unused", Some("machine-a"), Some("lease-a")),
            Some(renamed)
        );
        assert!(resolve(
            &path,
            "Mac-B",
            "unused",
            Some("other-machine"),
            Some("lease-a")
        )
        .is_none());
        std::fs::write(&path, json!({"schema_version":1,"leases":[{"machine":"Mac-A","instance":"http://192.0.2.1:6240","lease_id":"old"}]}).to_string()).unwrap();
        assert_eq!(
            resolve(
                &path,
                "Mac-A",
                "http://192.0.2.1:6240",
                Some("machine-a"),
                Some("old")
            )
            .unwrap()
            .lease_id,
            "old"
        );
    }
}
