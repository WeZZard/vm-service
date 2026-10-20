//! The host-wide durable lease lock (`docs/arch/durable-machine.md`, DM-2).
//!
//! A durable machine admits one lease at a time, across every instance on the
//! Mac (DC-26). The lock is a file whose content names its holder: the
//! holding instance's state directory, the `lease_id`, the purpose and the
//! expiry. A lease outlives the service process (DM-8), so the lock is the
//! file's content, not a kernel lock held by a process; a short `flock` only
//! serializes the check-and-set between instances.
//!
//! A holder whose TTL plus grace plus an hour has passed is stale and may be
//! taken over. A living holder's own GC releases it at TTL plus grace, so a
//! takeover only ever happens after its instance stopped for good.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The lock file's name inside the lock folder.
pub const LOCK_FILE: &str = "durable-machine.lock";
/// Slack after a holder's reclamation time before another instance may take
/// over its stale lock.
const TAKEOVER_SLACK_S: f64 = 3600.0;

/// The instance and lease that hold the lock.
#[derive(Debug, Clone, PartialEq)]
pub struct Holder {
    /// The holding instance's state directory.
    pub state_dir: String,
    /// The holding lease.
    pub lease_id: String,
    /// The holding lease's purpose.
    pub purpose: String,
    /// The durable machine's name.
    pub name: String,
    /// The lease's TTL expiry, Unix seconds.
    pub ttl_expires_at: f64,
    /// When the holder's own GC reclaims the lease, Unix seconds.
    pub reclaim_at: f64,
}

impl Holder {
    fn to_json(&self) -> Value {
        json!({
            "state_dir": self.state_dir,
            "lease_id": self.lease_id,
            "purpose": self.purpose,
            "name": self.name,
            "ttl_expires_at": self.ttl_expires_at,
            "reclaim_at": self.reclaim_at,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            state_dir: text("state_dir")?,
            lease_id: text("lease_id")?,
            purpose: text("purpose").unwrap_or_default(),
            name: text("name").unwrap_or_default(),
            ttl_expires_at: value.get("ttl_expires_at").and_then(Value::as_f64)?,
            reclaim_at: value.get("reclaim_at").and_then(Value::as_f64)?,
        })
    }

    /// Whether this holder is stale at `now`.
    pub fn stale(&self, now: f64) -> bool {
        now > self.reclaim_at + TAKEOVER_SLACK_S
    }

    fn same_lease(&self, state_dir: &str, lease_id: &str) -> bool {
        self.state_dir == state_dir && self.lease_id == lease_id
    }
}

/// A lock failure.
#[derive(Debug)]
pub enum LockError {
    /// Another lease holds the machine.
    Held(Holder),
    /// The lock folder or file is unusable.
    Unusable(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held(holder) => write!(formatter, "held by lease {}", holder.lease_id),
            LockError::Unusable(reason) => write!(formatter, "{reason}"),
        }
    }
}

/// The host-wide lock in one folder.
#[derive(Debug, Clone)]
pub struct HostLock {
    dir: PathBuf,
    uid: u32,
}

impl HostLock {
    /// A lock in `dir`, which must be owned by `uid`.
    pub fn new(dir: &Path, uid: u32) -> Self {
        Self {
            dir: dir.to_path_buf(),
            uid,
        }
    }

    /// The lock file's path.
    pub fn path(&self) -> PathBuf {
        self.dir.join(LOCK_FILE)
    }

    /// Take the lock for `holder`, or report who holds it. Re-taking a lock
    /// the same lease already holds succeeds and refreshes its record.
    pub fn take(&self, holder: &Holder, now: f64) -> Result<(), LockError> {
        self.with_file(|current| match current {
            Some(existing)
                if !existing.same_lease(&holder.state_dir, &holder.lease_id)
                    && !existing.stale(now) =>
            {
                Err(LockError::Held(existing.clone()))
            }
            _ => Ok(Some(Some(holder.clone()))),
        })
    }

    /// Refresh the expiry of a lock this lease holds. Fails when another
    /// lease holds it.
    pub fn renew(&self, holder: &Holder) -> Result<(), LockError> {
        self.with_file(|current| match current {
            Some(existing) if !existing.same_lease(&holder.state_dir, &holder.lease_id) => {
                Err(LockError::Held(existing.clone()))
            }
            _ => Ok(Some(Some(holder.clone()))),
        })
    }

    /// Release the lock when this lease holds it. Returns whether it did; a
    /// lock that another lease holds is left alone.
    pub fn release(&self, state_dir: &str, lease_id: &str) -> Result<bool, LockError> {
        let mut released = false;
        self.with_file(|current| match current {
            Some(existing) if existing.same_lease(state_dir, lease_id) => {
                released = true;
                Ok(Some(None))
            }
            _ => Ok(None),
        })?;
        Ok(released)
    }

    /// The current holder, if any.
    pub fn holder(&self) -> Result<Option<Holder>, LockError> {
        let mut found = None;
        self.with_file(|current| {
            found = current.clone();
            Ok(None)
        })?;
        Ok(found)
    }

    /// Run `decide` on the current holder under an exclusive `flock`.
    /// `Ok(Some(next))` writes `next` (a holder or an empty lock),
    /// `Ok(None)` leaves the file unchanged.
    fn with_file<F>(&self, decide: F) -> Result<(), LockError>
    where
        F: FnOnce(&Option<Holder>) -> Result<Option<Option<Holder>>, LockError>,
    {
        self.ensure_dir()?;
        let path = self.path();
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| LockError::Unusable("invalid lock path".to_string()))?;
        // SAFETY: `c_path` is a valid NUL-terminated string for the call.
        // `O_NOFOLLOW` refuses a planted symlink.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(LockError::Unusable(format!(
                "cannot open durable lease lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: `fd` is a fresh descriptor this function owns from here on.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file
            .metadata()
            .map_err(|error| LockError::Unusable(error.to_string()))?;
        if metadata.uid() != self.uid || !metadata.is_file() {
            return Err(LockError::Unusable(format!(
                "durable lease lock {} is not a regular file owned by this user",
                path.display()
            )));
        }
        // SAFETY: `file` keeps the descriptor open for the duration.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(LockError::Unusable(
                "cannot lock the durable lease lock".to_string(),
            ));
        }
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        let current = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.get("holder").cloned())
            .and_then(|holder| Holder::from_json(&holder));
        let outcome = decide(&current);
        let written = match outcome {
            Ok(Some(next)) => {
                let body = json!({"holder": next.map(|holder| holder.to_json())});
                let mut bytes = serde_json::to_vec_pretty(&body).unwrap_or_default();
                bytes.push(b'\n');
                file.set_len(0)
                    .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
                    .and_then(|_| file.write_all(&bytes))
                    .and_then(|_| file.sync_all())
                    .map_err(|error| {
                        LockError::Unusable(format!("cannot write the durable lease lock: {error}"))
                    })
            }
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        // SAFETY: as above; closing the file also drops the flock.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        written
    }

    /// Create the lock folder with mode 700, and refuse one that is a
    /// symlink or belongs to another user.
    fn ensure_dir(&self) -> Result<(), LockError> {
        match std::fs::symlink_metadata(&self.dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700).recursive(true);
                builder.create(&self.dir).map_err(|error| {
                    LockError::Unusable(format!(
                        "cannot create durable lease lock folder {}: {error}",
                        self.dir.display()
                    ))
                })?;
            }
            Err(error) => return Err(LockError::Unusable(error.to_string())),
            Ok(_) => {}
        }
        let metadata = std::fs::symlink_metadata(&self.dir)
            .map_err(|error| LockError::Unusable(error.to_string()))?;
        if !metadata.is_dir() || metadata.uid() != self.uid {
            return Err(LockError::Unusable(format!(
                "durable lease lock folder {} is not a folder owned by this user",
                self.dir.display()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(state: &str, lease: &str, reclaim_at: f64) -> Holder {
        Holder {
            state_dir: state.to_string(),
            lease_id: lease.to_string(),
            purpose: "metal-check".to_string(),
            name: "mac".to_string(),
            ttl_expires_at: reclaim_at - 10.0,
            reclaim_at,
        }
    }

    fn uid() -> u32 {
        // SAFETY: `geteuid` only reads process state.
        unsafe { libc::geteuid() }
    }

    /// AC-21 (TL-1): a second state directory cannot take a held lock, and
    /// sees the holder; the holder releases, and only then can it take it.
    #[test]
    fn ac21_one_holder_across_state_directories() {
        let dir = tempfile::tempdir().unwrap();
        let lock = HostLock::new(&dir.path().join("lock"), uid());
        let now = 1000.0;
        lock.take(&holder("/a", "lease-a", now + 100.0), now)
            .unwrap();
        match lock.take(&holder("/b", "lease-b", now + 100.0), now) {
            Err(LockError::Held(found)) => assert_eq!(found.lease_id, "lease-a"),
            other => panic!("expected held, got {other:?}"),
        }
        // Releasing someone else's lease changes nothing.
        assert!(!lock.release("/b", "lease-a").unwrap());
        assert!(!lock.release("/a", "lease-b").unwrap());
        assert_eq!(lock.holder().unwrap().unwrap().lease_id, "lease-a");
        assert!(lock.release("/a", "lease-a").unwrap());
        assert!(lock.holder().unwrap().is_none());
        lock.take(&holder("/b", "lease-b", now + 100.0), now)
            .unwrap();
    }

    /// DM-2: a stale holder (past reclamation plus an hour) is taken over;
    /// renewal by another lease is refused.
    #[test]
    fn dm2_stale_holder_is_taken_over_and_renewal_is_owned() {
        let dir = tempfile::tempdir().unwrap();
        let lock = HostLock::new(dir.path(), uid());
        lock.take(&holder("/a", "lease-a", 100.0), 50.0).unwrap();
        assert!(matches!(
            lock.take(&holder("/b", "lease-b", 9000.0), 200.0),
            Err(LockError::Held(_))
        ));
        assert!(matches!(
            lock.renew(&holder("/b", "lease-b", 9000.0)),
            Err(LockError::Held(_))
        ));
        lock.take(
            &holder("/b", "lease-b", 9000.0),
            100.0 + TAKEOVER_SLACK_S + 1.0,
        )
        .unwrap();
        assert_eq!(lock.holder().unwrap().unwrap().lease_id, "lease-b");
    }

    /// DM-2: a lock file that is a symlink is refused, never followed.
    #[test]
    fn dm2_symlinked_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(LOCK_FILE)).unwrap();
        let lock = HostLock::new(dir.path(), uid());
        assert!(matches!(
            lock.take(&holder("/a", "lease-a", 100.0), 0.0),
            Err(LockError::Unusable(_))
        ));
    }
}
