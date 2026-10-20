//! Migration from an installed vm-service to machine-service (MG-3).
//!
//! The rename lands as one coordinated switch with no compatibility window
//! (DC-33, DC-37), so the installer carries an installed vm-service across it
//! in the same run that installs machine-service:
//!
//! - the old agent's `VM_SERVICE_*` values are written under their
//!   `MACHINE_SERVICE_*` names, so the daemon's refusal of the old names never
//!   fires on a normal installation;
//! - the old agent is stopped and its plist removed;
//! - the old default state folder moves to the new default, and the old
//!   default console configuration is copied to the new configuration folder;
//! - the `vmctl` link in `~/.local/bin` that points into the old install folder
//!   is removed. No `vmctl` link is ever installed.
//!
//! The state folder carries no lease across the switch: the installer refuses
//! to run while any lease record exists in the old root or the new one, so the
//! moved `state.json` holds an empty lease set and the `ssh/` folder no lease
//! keys. The move keeps the service log and the lock files in place for the
//! operator, and it is one `rename` on the same file system.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::installer::InstallError;

/// The launchd label of the installed vm-service agent.
pub const LEGACY_LABEL: &str = "com.wezzard.vm-service";

/// The old default install folder, relative to `HOME`.
pub const LEGACY_INSTALL_DIR: &str = ".local/libexec/vm-service";

/// The old default state folder, relative to `HOME`.
pub const LEGACY_STATE_DIR: &str = ".local/state/vm-service";

/// The new default state folder, relative to `HOME`.
pub const STATE_DIR: &str = ".local/state/machine-service";

/// The old default console configuration, relative to `HOME`.
pub const LEGACY_CONSOLE_CONFIG: &str = ".config/vm-service/console.json";

/// The new default console configuration, relative to `HOME`.
pub const CONSOLE_CONFIG: &str = ".config/machine-service/console.json";

/// The folder that holds the CLI link, relative to `HOME`.
pub const LINK_DIR: &str = ".local/bin";

/// The old CLI's link name.
pub const LEGACY_LINK: &str = "vmctl";

/// The CLI's link name and executable name.
pub const LINK: &str = "machctl";

const STATE_KEY: &str = "MACHINE_SERVICE_STATE";
const CONSOLE_KEY: &str = "MACHINE_SERVICE_CONSOLE_CONFIG";

/// The plist path of the LaunchAgent with `label`.
pub fn agent_plist(home: &Path, label: &str) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

/// Rewrite every `VM_SERVICE_*` key to its `MACHINE_SERVICE_*` name. A key
/// that already exists under the new name keeps the new name's value.
pub fn rename_variables(environment: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut renamed = BTreeMap::new();
    for (key, value) in environment {
        if environment::renamed::replacement(key).is_none() {
            renamed.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in environment {
        if let Some(new) = environment::renamed::replacement(key) {
            renamed.entry(new).or_insert_with(|| value.clone());
        }
    }
    renamed
}

/// The installed vm-service agent, and what moving it across the rename needs.
pub struct Legacy {
    /// The old agent's plist.
    pub plist: PathBuf,
    /// The old agent's environment exactly as installed, for its state root
    /// and its daemon locks.
    pub installed: BTreeMap<String, String>,
    /// The old agent's environment under the new names, with the state and
    /// console paths that pointed at the old defaults moved to the new ones.
    pub migrated: BTreeMap<String, String>,
    /// The old agent's plist keys that are not the installer's own, carried
    /// into the new agent.
    pub extra: Map<String, Value>,
    /// The old default state folder to move, when the new one does not exist.
    pub state_move: Option<(PathBuf, PathBuf)>,
    /// The old default console configuration to copy, when the new one does
    /// not exist.
    pub console_copy: Option<(PathBuf, PathBuf)>,
}

impl Legacy {
    /// Plan the migration of the vm-service agent installed at `plist` with
    /// the environment `installed`.
    pub fn find(
        home: &Path,
        plist: Map<String, Value>,
        installed: BTreeMap<String, String>,
    ) -> Legacy {
        let path = agent_plist(home, LEGACY_LABEL);
        let mut migrated = rename_variables(&installed);
        // The interpreter slot of the Python service is not carried.
        migrated.remove("MACHINE_SERVICE_PYTHON");

        let legacy_state = home.join(LEGACY_STATE_DIR);
        let new_state = home.join(STATE_DIR);
        let selected = migrated.contains_key("VM_ENVIRONMENT_FILE");
        let state_is_default = match migrated.get(STATE_KEY) {
            None => true,
            Some(value) => Path::new(value) == legacy_state,
        };
        let state_move = if !selected && state_is_default {
            migrated.remove(STATE_KEY);
            (legacy_state.is_dir() && (!exists(&new_state) || holds_only_cli_files(&new_state)))
                .then_some((legacy_state, new_state))
        } else {
            None
        };

        let legacy_console = home.join(LEGACY_CONSOLE_CONFIG);
        let new_console = home.join(CONSOLE_CONFIG);
        let console_is_default = match migrated.get(CONSOLE_KEY) {
            None => true,
            Some(value) => Path::new(value) == legacy_console,
        };
        let console_copy = if console_is_default && exists(&legacy_console) && !exists(&new_console)
        {
            if migrated.contains_key(CONSOLE_KEY) {
                migrated.insert(
                    CONSOLE_KEY.to_string(),
                    new_console.to_string_lossy().into_owned(),
                );
            }
            Some((legacy_console, new_console))
        } else {
            None
        };

        let mut extra = plist;
        for key in [
            "Label",
            "ProgramArguments",
            "EnvironmentVariables",
            "StandardOutPath",
            "StandardErrorPath",
        ] {
            extra.remove(key);
        }

        Legacy {
            plist: path,
            installed,
            migrated,
            extra,
            state_move,
            console_copy,
        }
    }

    /// The old agent's environment for checking its state root and daemon
    /// locks: the installed values, with `HOME` and the old default state
    /// folder made explicit, renamed so the shared resolvers can read them.
    pub fn ownership_environment(&self, home: &Path) -> BTreeMap<String, String> {
        let mut environment = rename_variables(&self.installed);
        environment.insert("HOME".to_string(), home.to_string_lossy().into_owned());
        if !environment.contains_key("VM_ENVIRONMENT_FILE") {
            environment
                .entry(STATE_KEY.to_string())
                .or_insert_with(|| home.join(LEGACY_STATE_DIR).to_string_lossy().into_owned());
        }
        environment
    }

    /// The environment the new agent's console check reads: the copy source
    /// stands in for the copy until it is made, so validation reads the same
    /// bytes the new agent will.
    pub fn validation_console(&self) -> Option<String> {
        self.console_copy
            .as_ref()
            .map(|(source, _)| source.to_string_lossy().into_owned())
    }

    /// Print what an install run will do to the old agent.
    pub fn describe(&self) {
        println!(
            "migrating {LEGACY_LABEL}: its VM_SERVICE_* values become MACHINE_SERVICE_*, \
             and {} is removed",
            self.plist.display()
        );
        if let Some((from, to)) = &self.state_move {
            println!("state: {} moves to {}", from.display(), to.display());
        }
        if let Some((from, to)) = &self.console_copy {
            println!(
                "console configuration: {} is copied to {}",
                from.display(),
                to.display()
            );
        }
    }

    /// Move the state folder and copy the console configuration. Called only
    /// after the old daemon is stopped and while its state lock is held.
    pub fn apply(&self) -> Result<(), InstallError> {
        if let Some((from, to)) = &self.state_move {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // A new folder that holds only the CLI's remembered leases
            // (CL-3) steps aside, and its files join the moved folder.
            let aside = to.with_file_name(format!(".machine-service-cli-{}", std::process::id()));
            let set_aside = exists(to);
            if set_aside {
                std::fs::rename(to, &aside)?;
            }
            if let Err(error) = std::fs::rename(from, to) {
                if set_aside {
                    let _ = std::fs::rename(&aside, to);
                }
                return Err(InstallError::Refused(format!(
                    "cannot move {} to {}: {error}; the old agent is stopped and its plist kept",
                    from.display(),
                    to.display()
                )));
            }
            if set_aside {
                for entry in std::fs::read_dir(&aside)? {
                    let entry = entry?;
                    let target = to.join(entry.file_name());
                    if !exists(&target) {
                        std::fs::rename(entry.path(), target)?;
                    }
                }
                std::fs::remove_dir_all(&aside)?;
            }
        }
        if let Some((from, to)) = &self.console_copy {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // `copy` carries the permission bits the console check requires.
            std::fs::copy(from, to)?;
        }
        Ok(())
    }

    /// Remove the old agent's plist after its job is unloaded.
    pub fn remove_plist(&self) -> Result<(), InstallError> {
        match std::fs::remove_file(&self.plist) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// The folder the old agent's executable ran from.
    pub fn install_dir(&self, program: Option<&Value>) -> Option<PathBuf> {
        program
            .and_then(Value::as_array)
            .and_then(|arguments| arguments.first())
            .and_then(Value::as_str)
            .and_then(|path| Path::new(path).parent().map(Path::to_path_buf))
    }
}

/// Remove `~/.local/bin/vmctl` when it is a link that points into one of
/// `old_dirs`. Anything else under that name is the operator's and stays.
pub fn remove_legacy_link(home: &Path, old_dirs: &[PathBuf]) -> Result<(), InstallError> {
    let link = home.join(LINK_DIR).join(LEGACY_LINK);
    let Some(target) = link_target(&link)? else {
        return Ok(());
    };
    if old_dirs
        .iter()
        .any(|directory| target.starts_with(directory))
    {
        std::fs::remove_file(&link)?;
        println!("removed {}", link.display());
    }
    Ok(())
}

/// Point `~/.local/bin/machctl` at the `machctl` beside the installer. An
/// existing link is replaced; a file that is not a link is left alone.
pub fn install_link(home: &Path, source: &Path) -> Result<(), InstallError> {
    let directory = home.join(LINK_DIR);
    let link = directory.join(LINK);
    let target = source.join(LINK);
    match std::fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if std::fs::read_link(&link)? == target {
                return Ok(());
            }
            std::fs::remove_file(&link)?;
        }
        Ok(_) => {
            println!(
                "{} exists and is not a link; leaving it in place",
                link.display()
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::create_dir_all(&directory)?;
    std::os::unix::fs::symlink(&target, &link)?;
    println!("linked {} -> {}", link.display(), target.display());
    Ok(())
}

/// Remove `~/.local/bin/machctl` when it is a link to the `machctl` beside the
/// installer.
pub fn remove_link(home: &Path, source: &Path) -> Result<(), InstallError> {
    let link = home.join(LINK_DIR).join(LINK);
    if link_target(&link)?.as_deref() == Some(source.join(LINK).as_path()) {
        std::fs::remove_file(&link)?;
        println!("removed {}", link.display());
    }
    Ok(())
}

/// The absolute target of the link at `path`, or `None` when `path` is not a
/// link.
fn link_target(path: &Path) -> Result<Option<PathBuf>, InstallError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = std::fs::read_link(path)?;
            Ok(Some(if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or(Path::new("/")).join(target)
            }))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The files `machctl` keeps in the state folder (CL-3, DC-72): its
/// remembered leases, their lock, and a write in progress.
fn is_cli_file(name: &str) -> bool {
    name == "cli-leases.json"
        || name == ".cli-leases.json.lock"
        || name.starts_with(".cli-leases.json.")
}

/// Whether `folder` is a folder holding nothing but the CLI's files, which
/// `machctl` may create before the service is installed.
fn holds_only_cli_files(folder: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        entry.is_ok_and(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.file_name().to_str().is_some_and(is_cli_file)
        })
    })
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_old_keys_and_keeps_new_values() {
        let mut installed = BTreeMap::new();
        installed.insert("VM_SERVICE_HOST".to_string(), "0.0.0.0".to_string());
        installed.insert("VM_SERVICE_PORT".to_string(), "1".to_string());
        installed.insert("MACHINE_SERVICE_PORT".to_string(), "2".to_string());
        installed.insert("VM_ENVIRONMENT_FILE".to_string(), "/p.json".to_string());
        let renamed = rename_variables(&installed);
        assert_eq!(renamed["MACHINE_SERVICE_HOST"], "0.0.0.0");
        assert_eq!(renamed["MACHINE_SERVICE_PORT"], "2");
        assert_eq!(renamed["VM_ENVIRONMENT_FILE"], "/p.json");
        assert!(renamed.keys().all(|key| !key.starts_with("VM_SERVICE_")));
    }
}
