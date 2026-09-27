//! The service object: resolved configuration, lease state, line discovery,
//! console controller, and application associations.
//!
//! The Python daemon used module-level globals. The Rust port keeps the same
//! values in one `Service` so the daemon can serve concurrently and tests can
//! build an isolated instance.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::config::{log_to_file, Config};
use crate::console_api::{ConsoleConfig, ConsoleController, Manager};
use crate::error::{OpError, OpResult};
use crate::host::{BootProcess, Host, RealHost};
use crate::lines::LineCache;
use crate::reentrant::ReentrantLock;
use crate::state::State;

/// One validated image-to-base association.
#[derive(Debug, Clone)]
pub struct Association {
    /// The image name (the `images/<name>` directory).
    pub image: String,
    /// The OS kind, `linux` or `macos`.
    pub kind: String,
    /// The configured base VM.
    pub base_vm: String,
}

#[derive(Default)]
struct Associations {
    attempted: bool,
    value: Option<Vec<Association>>,
}

/// The bound service instance.
pub struct Service {
    /// The resolved configuration.
    pub config: Config,
    /// The lease state store.
    pub state: State,
    /// Line discovery cache.
    pub lines: Mutex<LineCache>,
    /// The console controller.
    pub consoles: Arc<dyn ConsoleController>,
    /// The host boundary: Tart, SSH, and line discovery.
    pub host: Arc<dyn Host>,
    associations: Mutex<Associations>,
    operation_locks: Mutex<HashMap<String, Arc<ReentrantLock>>>,
    /// Per-VM cancellation flags, set once a release has committed the
    /// `releasing` state. Guest operations observe them; see
    /// `docs/lifecycle-fixes.md` (V4).
    release_flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Consecutive GC passes in which a `running` record's VM was not running,
    /// keyed by VM name. Held in memory only; see `docs/lifecycle-fixes.md`.
    pub(crate) absent_passes: Mutex<HashMap<String, u32>>,
}

impl Service {
    /// Build a service from a configuration and an optional trusted console
    /// configuration, backed by the real host.
    pub fn new(config: Config, console_config: Option<ConsoleConfig>) -> Self {
        let host: Arc<dyn Host> = Arc::new(RealHost::new(config.clone()));
        Self::with_host(config, console_config, host)
    }

    /// Build a service against an injected host implementation.
    ///
    /// Tests use this to substitute an in-memory Tart and SSH, mirroring the
    /// Python suite's `mock.patch.object` of the module-level helpers. The
    /// console controller is the real `Manager`; use [`Service::with_consoles`]
    /// to inject a test controller at the same time.
    pub fn with_host(
        config: Config,
        console_config: Option<ConsoleConfig>,
        host: Arc<dyn Host>,
    ) -> Self {
        Self::with_consoles(config, Arc::new(Manager::new(console_config)), host)
    }

    /// Build a service against injected host and console implementations.
    ///
    /// The console analogue of [`Service::with_host`]. Production only ever
    /// binds the real `Manager`, so the seam cannot relax any validation.
    pub fn with_consoles(
        config: Config,
        consoles: Arc<dyn ConsoleController>,
        host: Arc<dyn Host>,
    ) -> Self {
        let fingerprint = config.fingerprint();
        let state = State::new(
            config.state_dir.clone(),
            config.state_file.clone(),
            config.lock_file.clone(),
            fingerprint,
        );
        Self {
            config,
            state,
            lines: Mutex::new(LineCache::default()),
            consoles,
            host,
            associations: Mutex::new(Associations::default()),
            operation_locks: Mutex::new(HashMap::new()),
            release_flags: Mutex::new(HashMap::new()),
            absent_passes: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve one image configuration through the host, or fail with the
    /// Python error text.
    pub fn line_cfg(&self, image: &str) -> OpResult<Value> {
        let mut cache = self
            .lines
            .lock()
            .map_err(|_| OpError::new("lines poisoned"))?;
        let (lines, _) = self.host.discover_lines(&mut cache, false)?;
        match lines.get(image) {
            Some(value) => Ok(value.clone()),
            None => {
                let mut names: Vec<&String> = lines.keys().collect();
                names.sort();
                let available = names
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(OpError::new(format!(
                    "unknown image '{image}' (available: {available})"
                )))
            }
        }
    }

    /// Resolve an env pack name to its directory through the host's home.
    pub fn pack_dir(&self, name: &str) -> OpResult<Option<PathBuf>> {
        let home = self.host.home_dir().unwrap_or_else(|| PathBuf::from("/"));
        crate::packs::pack_dir(&home, name)
    }

    /// Inject a credential pack into a running guest through the host.
    pub fn inject_pack(
        &self,
        ip: &str,
        cfg: &Value,
        pack: &std::path::Path,
        key_dir: &std::path::Path,
    ) -> OpResult<bool> {
        crate::packs::inject_pack(self.host.as_ref(), &self.config, ip, cfg, pack, key_dir)
    }

    /// Detect a refused boot through the host's settle window.
    pub fn boot_refused(
        &self,
        process: &mut dyn BootProcess,
        vm: &str,
        settle_s: Option<f64>,
    ) -> OpResult<()> {
        self.host.boot_refused(process, vm, settle_s)
    }

    /// Write a timestamped line to the service log.
    pub fn log(&self, message: &str) {
        log_to_file(&self.config.log_file, message);
    }

    /// Load the trusted console configuration, mapping failure to the daemon's
    /// startup error text.
    pub fn load_console_config() -> OpResult<Option<ConsoleConfig>> {
        crate::console_api::load_config(None, None)
            .map_err(|_| OpError::new("Invalid console configuration; service not started"))
    }

    /// Return the per-VM operation lock, mirroring Python's
    /// `_operation_locks.setdefault(vm, threading.RLock())`.
    ///
    /// The lock is reentrant because `release` re-enters through
    /// `begin_release`, and a failed `acquire` rolls back through the same
    /// path while still holding it.
    pub fn operation_lock(&self, vm: &str) -> Arc<ReentrantLock> {
        let mut locks = self
            .operation_locks
            .lock()
            .expect("operation lock map is never poisoned");
        locks
            .entry(vm.to_string())
            .or_insert_with(|| Arc::new(ReentrantLock::new()))
            .clone()
    }

    /// Return the per-VM release flag that sits beside the operation lock.
    ///
    /// A release sets it after committing the `releasing` state and before
    /// taking the operation lock. Exec, push, and pull run their subprocesses
    /// under [`crate::proc::with_cancellation`] with this flag, so a release
    /// kills the running subprocess instead of waiting for it. The flag is
    /// never cleared: VM names are unique per lease, and a `releasing` record
    /// refuses every new operation.
    pub fn release_flag(&self, vm: &str) -> Arc<AtomicBool> {
        let mut flags = self
            .release_flags
            .lock()
            .expect("release flag map is never poisoned");
        flags
            .entry(vm.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    /// Fetch a lease record and verify its environment fingerprint.
    pub fn get_record(&self, vm: &str) -> OpResult<Value> {
        let data = self.state.read()?;
        let record = data
            .get("vms")
            .and_then(|vms| vms.get(vm))
            .cloned()
            .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
        self.state.require_lease_environment(&record)?;
        Ok(record)
    }

    /// Validate and return the lease key directory for a record.
    pub fn key_directory(&self, record: &Value) -> OpResult<PathBuf> {
        let network = record
            .get("network")
            .and_then(Value::as_str)
            .unwrap_or("nat");
        if network != "nat" {
            return Err(OpError::new(
                "control-only ownership auditor unavailable; execution/transfers denied",
            ));
        }
        let keyed = record.get("ssh_auth").and_then(Value::as_str) == Some("lease-key")
            && record.get("ssh_verified") == Some(&Value::Bool(true));
        if !keyed {
            return Err(OpError::new(
                "Lease has no verified SSH key; release/reacquire it with key provisioning (no password fallback)",
            ));
        }
        let vm = record.get("vm").and_then(Value::as_str).unwrap_or_default();
        lease_keys::directory(&self.config.state_dir, vm).map_err(|_| {
            OpError::new(
                "Lease SSH credentials unavailable; retained lease requires operator recovery",
            )
        })
    }

    /// Publish all image associations or none, mirroring
    /// `initialize_application_associations`.
    pub fn initialize_application_associations(&self) -> OpResult<()> {
        let mut associations = self
            .associations
            .lock()
            .map_err(|_| OpError::new("association state poisoned"))?;
        if associations.attempted {
            return Err(OpError::new(
                "application associations require daemon restart",
            ));
        }
        associations.attempted = true;

        let mut pending: Vec<Association> = Vec::new();
        let images_dir = self.config.pilot.join("images");
        let entries = std::fs::read_dir(&images_dir)
            .map_err(|_| OpError::new("application association configuration unreadable"))?;
        let mut directories: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        directories.sort();
        for directory in directories {
            let Ok(metadata) = std::fs::metadata(&directory) else {
                return Err(OpError::new(
                    "application association configuration unreadable",
                ));
            };
            if !metadata.is_dir() {
                continue;
            }
            let conf_path = directory.join("line.conf");
            let conf_meta = match std::fs::metadata(&conf_path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    return Err(OpError::new(
                        "application association configuration unreadable",
                    ))
                }
            };
            if !conf_meta.is_file() {
                return Err(OpError::new("nonregular line configuration"));
            }
            let conf = crate::lines::parse_line_conf(&conf_path, true)
                .ok_or_else(|| OpError::new("line configuration parse failed"))?;
            let image = directory
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let kind = conf
                .get("LINE_KIND")
                .and_then(Value::as_str)
                .unwrap_or("macos")
                .to_string();
            let base = conf
                .get("BASE_VM")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            for key in [&image, &base] {
                if !valid_association_key(key) {
                    return Err(OpError::new("invalid image/base association"));
                }
            }
            if kind != "linux" && kind != "macos" {
                return Err(OpError::new("invalid association OS"));
            }
            pending.push(Association {
                image,
                kind,
                base_vm: base,
            });
            if pending.len() > application_catalog::MAX_IMAGES {
                return Err(OpError::new("too many image associations"));
            }
        }
        if pending.is_empty() {
            return Err(OpError::new("no configured image associations"));
        }
        associations.value = Some(pending);
        Ok(())
    }

    /// Read-only detached association view. Never initializes or retries.
    pub fn application_associations(&self) -> OpResult<Map<String, Value>> {
        let associations = self
            .associations
            .lock()
            .map_err(|_| OpError::new("association state poisoned"))?;
        let value = associations.value.as_ref().ok_or_else(|| {
            OpError::new("application associations unavailable; daemon restart required")
        })?;
        let mut out = Map::new();
        for association in value {
            out.insert(
                association.image.clone(),
                serde_json::json!({"kind": association.kind, "base_vm": association.base_vm}),
            );
        }
        Ok(out)
    }

    /// The raw association tuples for capability reporting.
    pub fn association_tuples(&self) -> Vec<(String, String, String)> {
        self.associations
            .lock()
            .map(|associations| {
                associations
                    .value
                    .as_ref()
                    .map(|value| {
                        value
                            .iter()
                            .map(|item| {
                                (item.image.clone(), item.kind.clone(), item.base_vm.clone())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Whether associations were initialized, for capability reporting.
    pub fn associations_initialized(&self) -> bool {
        self.associations
            .lock()
            .map(|associations| associations.value.is_some())
            .unwrap_or(false)
    }
}

fn valid_association_key(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}
