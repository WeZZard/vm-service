//! The mutable label and seven-day former-name history (MI-2, MI-3;
//! DC-88, DC-89). The hardware UUID is independent of this state.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How long a former name remains discoverable (MI-3).
pub const FORMER_NAME_RETENTION_S: f64 = 7.0 * 24.0 * 3600.0;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct FormerName {
    name: String,
    renamed_at: f64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct State {
    name: String,
    #[serde(default)]
    former_names: Vec<FormerName>,
}

/// One instance's shared label, persisted before a rename becomes visible.
pub struct Identity {
    path: PathBuf,
    state: RwLock<State>,
    startup_rename: Option<String>,
}

impl Identity {
    /// Read the prior label so a rename while stopped is remembered too.
    pub fn load(state_dir: &Path, name: &str, now: f64) -> Result<Self, String> {
        let path = state_dir.join("identity.json");
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State {
                name: name.to_string(),
                former_names: Vec::new(),
            },
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let mut identity = Self {
            path,
            state: RwLock::new(state),
            startup_rename: None,
        };
        identity.startup_rename = identity.rename(name, now)?;
        Ok(identity)
    }

    /// The old label when startup discovered a rename while stopped.
    pub fn startup_rename(&self) -> Option<&str> {
        self.startup_rename.as_deref()
    }

    /// Read a consistent name and history for one HTTP answer (MI-2).
    pub fn snapshot(&self, now: f64) -> (String, Value) {
        let state = self.state.read().unwrap_or_else(|error| error.into_inner());
        (
            state.name.clone(),
            json!(state
                .former_names
                .iter()
                .filter(|former| now - former.renamed_at < FORMER_NAME_RETENTION_S)
                .collect::<Vec<_>>()),
        )
    }

    /// Hold the current label stable while a lease reservation commits.
    pub fn current_name(&self) -> impl std::ops::Deref<Target = str> + '_ {
        struct Name<'a>(std::sync::RwLockReadGuard<'a, State>);
        impl std::ops::Deref for Name<'_> {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0.name
            }
        }
        Name(self.state.read().unwrap_or_else(|error| error.into_inner()))
    }

    /// The current human-facing name.
    pub fn name(&self) -> String {
        self.state
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .name
            .clone()
    }

    /// Remember a rename atomically on disk, then make it visible to readers.
    pub fn rename(&self, name: &str, now: f64) -> Result<Option<String>, String> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut next = state.clone();
        next.former_names.retain(|former| {
            former.name != name && now - former.renamed_at < FORMER_NAME_RETENTION_S
        });
        let old = (name != next.name).then(|| next.name.clone());
        if let Some(old) = &old {
            next.former_names.retain(|former| &former.name != old);
            next.former_names.push(FormerName {
                name: old.clone(),
                renamed_at: now,
            });
            next.name = name.to_string();
        }
        if *state == next && self.path.exists() {
            return Ok(None);
        }
        self.persist(&next)?;
        *state = next;
        Ok(old)
    }

    /// Former names still within the seven-day discovery window.
    pub fn former_names(&self, now: f64) -> Value {
        let state = self.state.read().unwrap_or_else(|error| error.into_inner());
        json!(state
            .former_names
            .iter()
            .filter(|former| now - former.renamed_at < FORMER_NAME_RETENTION_S)
            .collect::<Vec<_>>())
    }

    /// Explain an old label using the current name and the UTC rename date.
    pub fn renamed_message(&self, name: &str, now: f64) -> Option<String> {
        let state = self.state.read().unwrap_or_else(|error| error.into_inner());
        let former = state.former_names.iter().find(|former| {
            former.name == name && now - former.renamed_at < FORMER_NAME_RETENTION_S
        })?;
        let date = super::lease::format_gmtime(former.renamed_at);
        let date = date.get(..10).unwrap_or(&date);
        Some(format!(
            "{name} was renamed {} on {date}; use {}",
            state.name, state.name
        ))
    }

    fn persist(&self, state: &State) -> Result<(), String> {
        let parent = self.path.parent().ok_or("identity path has no parent")?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| error.to_string())?;
        let temporary = parent.join(format!(".identity-{}", uuid::Uuid::new_v4().simple()));
        let result = (|| -> Result<(), String> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            // Value uses sorted object keys, matching other service state.
            let value = serde_json::to_value(state).map_err(|error| error.to_string())?;
            let mut bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
            bytes.push(b'\n');
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .and_then(|_| std::fs::rename(&temporary, &self.path))
                .map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|error| format!("cannot persist {}: {error}", self.path.display()))
    }
}
