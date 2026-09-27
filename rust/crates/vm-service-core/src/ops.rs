//! Lease lifecycle operations: acquire, provision, release, heartbeat, and
//! guest execution and transfers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Map, Value};

use crate::config::unix_now;
use crate::error::{OpError, OpResult};
use crate::service::Service;
use crate::ssh::shell_quote;
use crate::GC_INTERVAL_S;

/// Maximum concurrent macOS guests on the whole host.
pub const MAX_MACOS_RUNNING: usize = 2;
/// The reclamation grace period after TTL expiry, in hours.
pub const GRACE_HOURS: i64 = 6;
/// The largest accepted guest execution timeout, in seconds.
///
/// The per-VM operation lock is held for the whole command, and release and
/// the serial GC loop wait on it, so the timeout bounds how long one command
/// can delay reclamation. mcp-vm-relay's largest request is 4080 seconds
/// (a 3600 s command, a 180 s receiver allowance, and a 300 s snapshot
/// delay); this maximum keeps a 120 s margin. See `docs/lifecycle-fixes.md`.
pub const MAX_EXEC_TIMEOUT_S: i64 = 4200;
/// Consecutive GC passes a `running` record's VM may be missing from Tart's
/// running set before GC reclaims the lease. Two passes tolerate a record
/// that became `running` between the `tart list` snapshot and the state read.
pub const ABSENT_PASSES_BEFORE_RECLAIM: u32 = 2;

fn vms_mut(data: &mut Map<String, Value>) -> &mut Map<String, Value> {
    data.get_mut("vms")
        .and_then(Value::as_object_mut)
        .expect("state always has a vms object")
}

fn active(record: &Value) -> bool {
    let state = record.get("state").and_then(Value::as_str).unwrap_or("");
    crate::lines::ACTIVE_STATES.contains(&state)
}

/// Compute the rounded hours remaining, matching Python `round(x, 2)`.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

impl Service {
    /// Acquire a fresh lease, reserving under the state lock and provisioning
    /// outside it.
    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        &self,
        purpose: &str,
        image: &str,
        env: &str,
        ttl_hours: &Value,
        cpu: Option<i64>,
        memory_mb: Option<i64>,
        disk_gb: Option<i64>,
        wait: bool,
        network: &str,
        profile: Option<&str>,
        source: &str,
        expected_source_fingerprint: Option<&Value>,
        vnc: bool,
    ) -> OpResult<Value> {
        let configuration =
            acquisition_options::resolve(cpu, memory_mb, disk_gb, wait, ttl_hours, vnc)
                .map_err(|error| OpError::new(error.to_string()))?;
        let effective_ttl = configuration
            .get("effective")
            .and_then(|value| value.get("initial_ttl_hours"))
            .and_then(Value::as_f64)
            .unwrap_or(24.0);

        let mut request = Map::new();
        request.insert("network".to_string(), Value::String(network.to_string()));
        request.insert("env".to_string(), Value::String(env.to_string()));
        if let Some(profile) = profile {
            request.insert("profile".to_string(), Value::String(profile.to_string()));
        }
        let request = Value::Object(request);
        control_only::validate_request(&request)
            .map_err(|error| OpError::new(error.to_string()))?;
        if network == "control-only" {
            control_only::require_available().map_err(|error| OpError::new(error.to_string()))?;
        }

        if !valid_purpose(purpose) {
            return Err(OpError::new("purpose must match [a-z0-9][a-z0-9-]{0,63}"));
        }
        if !(0.1..=24.0 * 30.0).contains(&effective_ttl) {
            return Err(OpError::new("ttl_hours must be within [0.1, 720]"));
        }

        let cfg = self.line_cfg(image)?;
        let kind = cfg
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("macos")
            .to_string();
        if vnc {
            self.consoles
                .require_available(&kind)
                .map_err(|error| OpError::new(error.to_string()))?;
        }
        if source != "base" && source != "work" {
            return Err(OpError::new("source must be base or work"));
        }
        if source == "base" && expected_source_fingerprint.is_some() {
            return Err(OpError::new(
                "expected_source_fingerprint requires source=work",
            ));
        }
        let mut base = cfg
            .get("base_vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut source_fingerprint: Option<Value> = None;
        if source == "work" {
            if env != "none" {
                return Err(OpError::new("work-image acceptance requires env=none"));
            }
            let work = cfg
                .get("work_vm")
                .and_then(Value::as_str)
                .filter(|value| valid_vm_name(value))
                .filter(|value| *value != base)
                .ok_or_else(|| OpError::new("image has no distinct configured WORK_VM"))?;
            base = work.to_string();
            let fingerprint = self.stopped_work_fingerprint(&base, &kind)?;
            if let Some(expected) = expected_source_fingerprint {
                if &fingerprint != expected {
                    return Err(OpError::new(
                        "work source fingerprint differs from expected acceptance source",
                    ));
                }
            }
            source_fingerprint = Some(fingerprint);
        }

        let chosen: Option<String> = if env.is_empty() || env == "none" {
            None
        } else {
            Some(env.to_string())
        };
        if let Some(name) = &chosen {
            self.pack_dir(name)?;
        }

        // Phase 1: reserve a lease slot under the cross-process lock.
        let clone_prefix = cfg
            .get("clone_prefix")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let kind_for_record = kind.clone();
        let image_for_record = image.to_string();
        let purpose_for_record = purpose.to_string();
        let cpu_for_record = cpu;
        let memory_for_record = memory_mb;
        let disk_for_record = disk_gb;
        let fingerprint = self.config.fingerprint();
        let source_for_record = source.to_string();
        let base_for_record = base.clone();
        let chosen_for_record = chosen.clone();
        let vm = self.state.update(
            |data| {
                let vms = vms_mut(data);
                for record in vms.values() {
                    let same_purpose = record.get("purpose").and_then(Value::as_str)
                        == Some(purpose_for_record.as_str());
                    let same_image = record.get("image").and_then(Value::as_str)
                        == Some(image_for_record.as_str());
                    if same_purpose && same_image && active(record) {
                        let name = record.get("vm").and_then(Value::as_str).unwrap_or("");
                        return Err(OpError::new(format!(
                            "purpose '{purpose_for_record}' already leased on image {image_for_record}: {name}"
                        )));
                    }
                }
                let active_records: Vec<&Value> =
                    vms.values().filter(|record| active(record)).collect();
                if kind_for_record == "macos" {
                    let mac_active = active_records
                        .iter()
                        .filter(|record| {
                            record.get("image_kind").and_then(Value::as_str) == Some("macos")
                        })
                        .count();
                    if mac_active >= MAX_MACOS_RUNNING {
                        let extra = match self.host.host_macos_guests() {
                            Some(guests) => format!(
                                "; host-wide Virtualization.framework guests: {guests} ({} not ours)",
                                guests as i64 - mac_active as i64
                            ),
                            None => "; host-wide gauge unavailable".to_string(),
                        };
                        return Err(OpError::new(format!(
                            "macOS VM limit reached ({MAX_MACOS_RUNNING} active); release one first{extra}"
                        )));
                    }
                }
                let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
                let vm = format!("{clone_prefix}{purpose_for_record}-{suffix}");
                let now = unix_now();
                let mut record = Map::new();
                record.insert("vm".to_string(), Value::String(vm.clone()));
                record.insert(
                    "purpose".to_string(),
                    Value::String(purpose_for_record.clone()),
                );
                record.insert(
                    "image".to_string(),
                    Value::String(image_for_record.clone()),
                );
                record.insert(
                    "image_kind".to_string(),
                    Value::String(kind_for_record.clone()),
                );
                record.insert(
                    "lease_id".to_string(),
                    Value::String(uuid::Uuid::new_v4().simple().to_string()),
                );
                record.insert("configuration".to_string(), configuration.clone());
                if let Some(fingerprint) = &fingerprint {
                    record.insert(
                        "environment_fingerprint".to_string(),
                        Value::String(fingerprint.clone()),
                    );
                }
                record.insert(
                    "env".to_string(),
                    chosen_for_record
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                record.insert("state".to_string(), Value::String("pending".to_string()));
                record.insert("created_at".to_string(), number(now));
                record.insert(
                    "ttl_expires_at".to_string(),
                    number(now + effective_ttl * 3600.0),
                );
                record.insert("grace_until".to_string(), Value::Null);
                record.insert("warned".to_string(), Value::Bool(false));
                record.insert("ip".to_string(), Value::Null);
                record.insert(
                    "cpu".to_string(),
                    cpu_for_record.map_or(Value::Null, number_i),
                );
                record.insert(
                    "memory_mb".to_string(),
                    memory_for_record.map_or(Value::Null, number_i),
                );
                record.insert(
                    "disk_gb".to_string(),
                    disk_for_record.map_or(Value::Null, number_i),
                );
                record.insert(
                    "ssh_auth".to_string(),
                    Value::String("lease-key".to_string()),
                );
                record.insert(
                    "ssh_user".to_string(),
                    cfg.get("ssh_user").cloned().unwrap_or(Value::String(String::new())),
                );
                record.insert("ssh_verified".to_string(), Value::Bool(false));
                record.insert(
                    "ssh_host_trust".to_string(),
                    Value::String("lease-tofu".to_string()),
                );
                if source_for_record == "work" {
                    record.insert(
                        "source".to_string(),
                        Value::String("work".to_string()),
                    );
                    record.insert(
                        "source_vm".to_string(),
                        Value::String(base_for_record.clone()),
                    );
                    record.insert(
                        "source_fingerprint".to_string(),
                        source_fingerprint.clone().unwrap_or(Value::Null),
                    );
                }
                let record_value = Value::Object(record);
                vms.insert(vm.clone(), record_value.clone());
                if vnc {
                    self.consoles
                        .reserve(record_value.as_object().expect("record is an object"))
                        .map_err(|error| OpError::new(error.to_string()))?;
                }
                Ok(vm.clone())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;

        // Phase 2 holds only this VM's operation lock, not the state lock.
        let lock = self.operation_lock(&vm);
        let _guard = lock.lock();
        self.provision(&vm, &base, &cfg, chosen, cpu, memory_mb, disk_gb, wait)?;
        self.get_record(&vm)
    }

    /// Read-only verification that a configured work image exists and is stopped.
    pub fn stopped_work_fingerprint(&self, vm: &str, kind: &str) -> OpResult<Value> {
        if !self.host.vm_exists(vm)? || self.host.vm_running(vm)? {
            return Err(OpError::new("work source must exist and be stopped"));
        }
        let root = self.tart_store_root().join("vms");
        application_catalog::fingerprint_base(&root, vm, kind)
            .map_err(|error| OpError::new(format!("work source metadata unavailable: {error}")))
    }

    fn tart_store_root(&self) -> PathBuf {
        self.host.tart_store_root()
    }

    /// Clone, boot, and verify a lease outside the state lock.
    #[allow(clippy::too_many_arguments)]
    pub fn provision(
        &self,
        vm: &str,
        base: &str,
        cfg: &Value,
        chosen: Option<String>,
        cpu: Option<i64>,
        memory_mb: Option<i64>,
        disk_gb: Option<i64>,
        wait: bool,
    ) -> OpResult<()> {
        let clone_owned = AtomicBool::new(false);
        let keys_created = AtomicBool::new(false);
        let result = self.provision_inner(
            vm,
            base,
            cfg,
            &chosen,
            cpu,
            memory_mb,
            disk_gb,
            wait,
            &clone_owned,
            &keys_created,
        );
        if let Err(error) = result {
            self.log(&format!("acquire failed for {vm}: {error}; rolling back"));
            let cleanup = (|| -> OpResult<()> {
                if clone_owned.load(Ordering::SeqCst) {
                    self.begin_release(vm, "acquire-failed", false)?;
                } else {
                    if keys_created.load(Ordering::SeqCst) {
                        let _ = lease_keys::cleanup(&self.config.state_dir, vm);
                    }
                    self.state.update(
                        |data| {
                            vms_mut(data).remove(vm);
                            Ok(())
                        },
                        Option::<fn(&Map<String, Value>)>::None,
                    )?;
                }
                Ok(())
            })();
            if let Err(cleanup_error) = cleanup {
                return Err(OpError::new(format!(
                    "Acquisition failed; {vm} retained for cleanup: {cleanup_error}"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn provision_inner(
        &self,
        vm: &str,
        base: &str,
        cfg: &Value,
        chosen: &Option<String>,
        cpu: Option<i64>,
        memory_mb: Option<i64>,
        disk_gb: Option<i64>,
        wait: bool,
        clone_owned: &AtomicBool,
        keys_created: &AtomicBool,
    ) -> OpResult<()> {
        let record = self.get_record(vm)?;
        if record.get("state").and_then(Value::as_str) == Some("releasing") {
            return Err(OpError::new("Acquisition cancelled by release"));
        }
        let kind = cfg.get("kind").and_then(Value::as_str).unwrap_or("macos");
        let expected = record.get("source_fingerprint").cloned();
        if record.get("source").and_then(Value::as_str) == Some("work")
            && self.stopped_work_fingerprint(base, kind)? != expected.unwrap_or(Value::Null)
        {
            return Err(OpError::new("work source changed before clone"));
        }
        if !self.host.vm_exists(base)? {
            return Err(OpError::new(format!("golden base '{base}' not built yet")));
        }
        if self.host.vm_exists(vm)? {
            return Err(OpError::new(format!("clone name collision: {vm}")));
        }
        let key_dir = lease_keys::create(&self.config.state_dir, vm)
            .map_err(|error| OpError::new(error.to_string()))?;
        keys_created.store(true, Ordering::SeqCst);
        clone_owned.store(true, Ordering::SeqCst);
        self.log(&format!("cloning {base} -> {vm}"));
        self.host.tart(
            &["clone".to_string(), base.to_string(), vm.to_string()],
            true,
            1800,
        )?;
        if record.get("source").and_then(Value::as_str) == Some("work")
            && self.stopped_work_fingerprint(base, kind)?
                != record
                    .get("source_fingerprint")
                    .cloned()
                    .unwrap_or(Value::Null)
        {
            return Err(OpError::new("work source changed during clone"));
        }
        let mut set_args = vec![
            "set".to_string(),
            vm.to_string(),
            "--cpu".to_string(),
            (cpu.unwrap_or(6)).to_string(),
            "--memory".to_string(),
            (memory_mb.unwrap_or(16384)).to_string(),
        ];
        if let Some(disk) = disk_gb {
            set_args.push("--disk-size".to_string());
            set_args.push(disk.to_string());
        }
        self.host.tart(&set_args, true, 300)?;

        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                if record.get("state").and_then(Value::as_str) == Some("releasing") {
                    return Err(OpError::new("Acquisition cancelled by release"));
                }
                record["state"] = Value::String("provisioning".to_string());
                if let Some(applied) = record
                    .get_mut("configuration")
                    .and_then(|config| config.get_mut("resources_applied"))
                    .and_then(Value::as_object_mut)
                {
                    applied.insert("status".to_string(), Value::String("applied".to_string()));
                    applied.insert("observed_at".to_string(), number(unix_now()));
                }
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;

        self.log(&format!(
            "booting {vm} headless (NAT); log {}",
            crate::tart::run_log_path(vm)
        ));
        let mut child = self.host.spawn_run(vm)?;
        self.host.boot_refused(&mut *child, vm, None)?;
        let ip = self.host.wait_ip(vm, 420).ok_or_else(|| {
            OpError::new(format!(
                "no IP for {vm} within 420s (see {})",
                crate::tart::run_log_path(vm)
            ))
        })?;
        let ip_for_record = ip.clone();
        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                record["ip"] = Value::String(ip_for_record.clone());
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;

        let ssh_user = cfg
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ssh_pass = cfg
            .get("ssh_pass")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let bootstrap_timeout = if wait { 420.0 } else { 30.0 };
        self.host
            .bootstrap(&ip, &ssh_user, &ssh_pass, &key_dir, bootstrap_timeout)?;
        if !self
            .host
            .wait_ssh(&ip, &ssh_user, &key_dir, if wait { 420 } else { 0 })
        {
            return Err(OpError::new(format!(
                "Key-only SSH never became ready for {vm}"
            )));
        }
        self.host.verify_transfer(&ip, &ssh_user, &key_dir)?;
        if let Some(name) = chosen {
            let pack = self
                .pack_dir(name)?
                .ok_or_else(|| OpError::new(format!("credential pack not found: {name}")))?;
            if !self.inject_pack(&ip, cfg, &pack, &key_dir)? {
                return Err(OpError::new(format!(
                    "env pack injection failed for pack {name}"
                )));
            }
        }

        let wants_vnc = record
            .get("configuration")
            .and_then(|config| config.get("effective"))
            .and_then(|effective| effective.get("vnc"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut console_record: Option<Map<String, Value>> = None;
        if wants_vnc {
            let fresh = self.get_record(vm)?;
            let map = fresh
                .as_object()
                .cloned()
                .ok_or_else(|| OpError::new("lease record is not an object"))?;
            let console = self
                .consoles
                .prepare(&map, &key_dir, if wait { 180 } else { 30 })
                .map_err(|error| OpError::new(error.to_string()))?;
            console_record = Some(console);
        }

        let console_for_ready = console_record.clone();
        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                if record.get("state").and_then(Value::as_str) == Some("releasing") {
                    return Err(OpError::new("Acquisition cancelled by release"));
                }
                record["state"] = Value::String("running".to_string());
                record["ssh_verified"] = Value::Bool(true);
                if let Some(console) = &console_for_ready {
                    record["console"] = Value::Object(console.clone());
                }
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        self.log(&format!(
            "acquired {vm} (key-only SSH verified, env={}, ip={ip})",
            chosen.as_deref().unwrap_or("none")
        ));
        Ok(())
    }

    /// Public release entry point, mirroring the Python lock decision.
    pub fn release(&self, vm: &str, reason: &str, expired_only: bool) -> OpResult<Value> {
        let record = self.get_record(vm)?;
        let vnc = record
            .get("configuration")
            .and_then(|config| config.get("effective"))
            .and_then(|effective| effective.get("vnc"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if expired_only || !vnc {
            let lock = self.operation_lock(vm);
            let _guard = lock.lock();
            self.begin_release(vm, reason, expired_only)
        } else {
            self.begin_release(vm, reason, expired_only)
        }
    }

    /// Commit the releasing state and tear the VM down.
    pub fn begin_release(&self, vm: &str, reason: &str, expired_only: bool) -> OpResult<Value> {
        let proceed = self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                self.state.require_lease_environment(record)?;
                if expired_only && record.get("state").and_then(Value::as_str) != Some("releasing")
                {
                    let now = unix_now();
                    let state_value = record.get("state").and_then(Value::as_str).unwrap_or("");
                    let grace = record.get("grace_until").and_then(Value::as_f64);
                    let ttl = record
                        .get("ttl_expires_at")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    if !crate::lines::ACTIVE_STATES.contains(&state_value)
                        || grace.is_none()
                        || now < ttl
                        || now < grace.unwrap_or(0.0)
                    {
                        return Ok(false);
                    }
                }
                record["state"] = Value::String("releasing".to_string());
                let record = record.as_object().expect("lease record is an object");
                self.consoles.revoke(record, "released");
                Ok(true)
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        if !proceed {
            return Ok(serde_json::json!({
                "vm": vm, "released": false,
                "reason": "lease-renewed-before-reclamation"
            }));
        }
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        self.destroy_lease(vm, reason)
    }

    /// Stop, delete, and unregister a lease, then remove its credentials.
    pub fn destroy_lease(&self, vm: &str, reason: &str) -> OpResult<Value> {
        let present = self
            .state
            .read()?
            .get("vms")
            .and_then(|vms| vms.get(vm))
            .is_some();
        if !present {
            return Ok(serde_json::json!({"vm": vm, "released": true, "reason": reason}));
        }
        let teardown = (|| -> OpResult<()> {
            if self.host.vm_running(vm)? {
                self.log(&format!("stopping {vm} ({reason})"));
                self.host
                    .tart(&["stop".to_string(), vm.to_string()], true, 180)?;
            }
            if self.host.vm_exists(vm)? {
                self.host
                    .tart(&["delete".to_string(), vm.to_string()], true, 600)?;
            }
            if self.host.vm_exists(vm)? {
                return Err(OpError::new("Tart still reports the clone after deletion"));
            }
            lease_keys::cleanup(&self.config.state_dir, vm)
                .map_err(|error| OpError::new(error.to_string()))?;
            self.log(&format!(
                "deleted {vm} ({reason}); lease credentials removed"
            ));
            Ok(())
        })();
        if let Err(error) = teardown {
            self.log(&format!(
                "WARN: teardown of {vm} incomplete; retaining credentials and record: {error}"
            ));
            return Err(OpError::new(format!(
                "Teardown incomplete for {vm}; lease retained for retry"
            )));
        }
        self.state.update(
            |data| {
                vms_mut(data).remove(vm);
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        self.consoles.forget(vm);
        Ok(serde_json::json!({"vm": vm, "released": true, "reason": reason}))
    }

    /// Renew a lease TTL and clear the grace warning.
    pub fn heartbeat(&self, vm: &str, ttl_hours: Option<&Value>) -> OpResult<Value> {
        // A renewal must not keep a lease alive whose VM is gone. Tart is
        // asked outside the state lock; an unknown record falls through to the
        // update below, which reports it.
        let recorded_running = self
            .get_record(vm)
            .ok()
            .and_then(|record| {
                record
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|state| state == "running");
        if recorded_running {
            match self.host.vm_running(vm) {
                Ok(true) => {}
                Ok(false) => {
                    self.log(&format!(
                        "heartbeat refused for {vm}: Tart reports the VM stopped or absent"
                    ));
                    return Err(OpError::new(format!(
                        "{vm} is not running on the host (Tart reports it stopped or absent); \
release it and acquire a new lease"
                    )));
                }
                Err(error) => self.log(&format!(
                    "WARN: heartbeat {vm}: liveness check unavailable ({error}); renewing anyway"
                )),
            }
        }
        let grant = std::cell::Cell::new(None::<Instant>);
        let renewal = std::cell::Cell::new(None::<(f64, f64)>);
        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                self.state.require_lease_environment(record)?;
                if record.get("state").and_then(Value::as_str) == Some("releasing") {
                    return Err(OpError::new(format!(
                        "{vm} is releasing; renewal cannot cancel committed teardown"
                    )));
                }
                let ttl = match ttl_hours {
                    Some(ttl_value) => {
                        let resolved =
                            acquisition_options::resolve(None, None, None, true, ttl_value, false)
                                .map_err(|error| OpError::new(error.to_string()))?;
                        resolved
                            .get("effective")
                            .and_then(|value| value.get("initial_ttl_hours"))
                            .and_then(Value::as_f64)
                            .unwrap_or(DEFAULT_TTL_HOURS)
                    }
                    // A bare heartbeat renews the lease for its own initial
                    // TTL; see `docs/lifecycle-fixes.md` (V5).
                    None => lease_initial_ttl_hours(record),
                };
                let expires = unix_now() + ttl * 3600.0;
                record["ttl_expires_at"] = number(expires);
                grant.set(Some(
                    Instant::now() + std::time::Duration::from_secs_f64(ttl * 3600.0),
                ));
                renewal.set(Some((ttl, expires)));
                record["grace_until"] = Value::Null;
                record["warned"] = Value::Bool(false);
                Ok(())
            },
            Some(|data: &Map<String, Value>| {
                if let Some(deadline) = grant.get() {
                    if let Some(record) = data
                        .get("vms")
                        .and_then(|vms| vms.get(vm))
                        .and_then(Value::as_object)
                    {
                        self.consoles.renew(record, Some(deadline));
                    }
                }
            }),
        )?;
        if let Some((ttl, expires)) = renewal.get() {
            self.log(&format!(
                "heartbeat {vm}: TTL renewed for {ttl}h (until {})",
                format_gmtime(expires)
            ));
        }
        let now = unix_now();
        let mut record = self.get_record(vm)?;
        if let Some(object) = record.as_object_mut() {
            let ttl = object
                .get("ttl_expires_at")
                .and_then(Value::as_f64)
                .unwrap_or(now);
            object.insert(
                "ttl_hours_remaining".to_string(),
                number(round2((ttl - now) / 3600.0)),
            );
        }
        Ok(record)
    }

    /// Run a command or script in the guest.
    pub fn guest_exec(&self, vm: &str, body: &Value) -> OpResult<Value> {
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        let record = self.get_record(vm)?;
        let state = record.get("state").and_then(Value::as_str).unwrap_or("");
        if state != "running" {
            return Err(OpError::new(format!("{vm} is not running (state={state})")));
        }
        let key_dir = self.key_directory(&record)?;
        let ssh_user = record
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let kind = record
            .get("image_kind")
            .and_then(Value::as_str)
            .unwrap_or("linux");
        let ip = record
            .get("ip")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|ip| !ip.is_empty())
            .or_else(|| self.host.vm_ip(vm))
            .ok_or_else(|| OpError::new("VM has no IP; is it running?"))?;
        let timeout = body.get("timeout").and_then(Value::as_i64).unwrap_or(600);
        if timeout <= 0 {
            return Err(OpError::new("timeout must be positive"));
        }
        if timeout > MAX_EXEC_TIMEOUT_S {
            return Err(OpError::new(format!(
                "timeout must not exceed {MAX_EXEC_TIMEOUT_S} seconds"
            )));
        }
        let outcome = if let Some(script) = body.get("script").and_then(Value::as_str) {
            let shell = if kind == "macos" { "zsh -s" } else { "bash -s" };
            self.host.ssh(
                &ip,
                &ssh_user,
                &key_dir,
                shell,
                Some(script.as_bytes().to_vec()),
                timeout as u64,
            )
        } else if let Some(argv) = body.get("argv").and_then(Value::as_array) {
            let command = argv
                .iter()
                .map(|value| shell_quote(&arg_text(value)))
                .collect::<Vec<_>>()
                .join(" ");
            self.host
                .ssh(&ip, &ssh_user, &key_dir, &command, None, timeout as u64)
        } else {
            return Err(OpError::new("need 'argv' or 'script'"));
        };
        let (rc, text) = outcome.ok_or_else(|| OpError::new("ssh failed"))?;
        let truncated = tail_chars(&text, 64000);
        Ok(serde_json::json!({"vm": vm, "rc": rc, "output": truncated}))
    }

    /// Copy a host path into the guest.
    pub fn guest_push(&self, vm: &str, body: &Value) -> OpResult<Value> {
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        let record = self.running_record(vm)?;
        let key_dir = self.key_directory(&record)?;
        let ssh_user = record
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ip = self.record_ip(vm, &record)?;
        let local = body.get("local_path").and_then(Value::as_str);
        let remote = body.get("remote_path").and_then(Value::as_str);
        let (Some(local), Some(remote)) = (local, remote) else {
            return Err(OpError::new("need local_path and remote_path"));
        };
        if !std::path::Path::new(local).exists() {
            return Err(OpError::new(format!("no such local path: {local}")));
        }
        crate::packs::protect_key_material(
            &self.config.state_dir,
            std::path::Path::new(local),
            true,
        )?;
        let result = self.host.scp(
            &ip,
            &ssh_user,
            &key_dir,
            local,
            &format!("{ssh_user}@{ip}:{remote}"),
            300,
        );
        match result {
            Some((0, _)) => Ok(serde_json::json!({"vm": vm, "pushed": local, "to": remote})),
            other => Err(OpError::new(format!(
                "scp failed: {}",
                other.map(|(_, text)| text).unwrap_or_default()
            ))),
        }
    }

    /// Copy a guest path to the host.
    pub fn guest_pull(&self, vm: &str, body: &Value) -> OpResult<Value> {
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        let record = self.running_record(vm)?;
        let key_dir = self.key_directory(&record)?;
        let ssh_user = record
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ip = self.record_ip(vm, &record)?;
        let local = body.get("local_path").and_then(Value::as_str);
        let remote = body.get("remote_path").and_then(Value::as_str);
        let (Some(local), Some(remote)) = (local, remote) else {
            return Err(OpError::new("need local_path and remote_path"));
        };
        crate::packs::protect_key_material(
            &self.config.state_dir,
            std::path::Path::new(local),
            true,
        )?;
        if let Some(parent) = std::path::Path::new(local).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let result = self.host.scp(
            &ip,
            &ssh_user,
            &key_dir,
            &format!("{ssh_user}@{ip}:{remote}"),
            local,
            300,
        );
        match result {
            Some((0, _)) => Ok(serde_json::json!({"vm": vm, "pulled": remote, "to": local})),
            other => Err(OpError::new(format!(
                "scp failed: {}",
                other.map(|(_, text)| text).unwrap_or_default()
            ))),
        }
    }

    fn running_record(&self, vm: &str) -> OpResult<Value> {
        let record = self.get_record(vm)?;
        let state = record.get("state").and_then(Value::as_str).unwrap_or("");
        if state != "running" {
            return Err(OpError::new(format!("{vm} is not running (state={state})")));
        }
        Ok(record)
    }

    fn record_ip(&self, vm: &str, record: &Value) -> OpResult<String> {
        record
            .get("ip")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|ip| !ip.is_empty())
            .or_else(|| self.host.vm_ip(vm))
            .ok_or_else(|| OpError::new("VM has no IP; is it running?"))
    }

    /// Run the garbage collector once.
    pub fn gc_once(&self) -> OpResult<()> {
        let now = unix_now();
        let mut actions: Vec<(String, &'static str, Option<f64>)> = Vec::new();
        // One Tart snapshot per pass, taken outside the state lock and only
        // after every record passed the environment check, so a mismatched
        // store never reaches Tart. When Tart cannot be asked, liveness is not
        // judged in this pass.
        let snapshot = self.state.read()?;
        let mut any_running = false;
        if let Some(records) = snapshot.get("vms").and_then(Value::as_object) {
            for record in records.values() {
                self.state.require_lease_environment(record)?;
                any_running |= record.get("state").and_then(Value::as_str) == Some("running");
            }
        }
        let listing = if any_running {
            Some(self.host.tart_list())
        } else {
            None
        };
        let live: Option<std::collections::HashSet<String>> = match listing {
            None => None,
            Some(Ok(rows)) => Some(
                rows.into_iter()
                    .filter(|(_, state)| state == "running")
                    .map(|(name, _)| name)
                    .collect(),
            ),
            Some(Err(error)) => {
                self.log(&format!("WARN: GC liveness check skipped: {error}"));
                None
            }
        };
        let previous_absent = self
            .absent_passes
            .lock()
            .map_err(|_| OpError::new("absent-pass counter poisoned"))?
            .clone();
        let mut absent: std::collections::HashMap<String, u32> = Default::default();
        self.state.update(
            |data| {
                for (vm, record) in vms_mut(data).iter_mut() {
                    self.state.require_lease_environment(record)?;
                    let state = record.get("state").and_then(Value::as_str).unwrap_or("");
                    if state == "releasing" {
                        actions.push((vm.clone(), "reclaim", None));
                        continue;
                    }
                    if !crate::lines::ACTIVE_STATES.contains(&state) {
                        continue;
                    }
                    if let Some(live) = &live {
                        if state == "running" && !live.contains(vm) {
                            let passes = previous_absent.get(vm).copied().unwrap_or(0) + 1;
                            absent.insert(vm.clone(), passes);
                            if passes >= ABSENT_PASSES_BEFORE_RECLAIM {
                                actions.push((vm.clone(), "reclaim-absent", None));
                                continue;
                            }
                            actions.push((vm.clone(), "absent", Some(passes as f64)));
                        }
                    }
                    let ttl = record
                        .get("ttl_expires_at")
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::INFINITY);
                    if now >= ttl {
                        let grace = record.get("grace_until").and_then(Value::as_f64);
                        match grace {
                            None => {
                                let until = now + GRACE_HOURS as f64 * 3600.0;
                                record["grace_until"] = number(until);
                                record["warned"] = Value::Bool(true);
                                actions.push((vm.clone(), "warn", Some(until)));
                            }
                            Some(until) if now >= until => {
                                actions.push((vm.clone(), "reclaim", None));
                            }
                            Some(_) => {}
                        }
                    }
                }
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        if live.is_some() || !any_running {
            // Records no longer absent (or gone) drop their count.
            if let Ok(mut counter) = self.absent_passes.lock() {
                *counter = absent;
            }
        }
        for (vm, action, grace) in actions {
            if action == "warn" {
                self.log(&format!(
                    "LEASE WARN: {vm} past TTL; grace until {}",
                    grace.map(format_gmtime).unwrap_or_default()
                ));
            } else if action == "absent" {
                self.log(&format!(
                    "LEASE WARN: {vm} is recorded running but Tart reports it stopped or absent \
(pass {} of {ABSENT_PASSES_BEFORE_RECLAIM})",
                    grace.unwrap_or(0.0) as u32
                ));
            } else if action == "reclaim-absent" {
                if matches!(self.host.vm_running(&vm), Ok(true)) {
                    self.log(&format!("GC: {vm} is running again; not reclaiming"));
                    if let Ok(mut counter) = self.absent_passes.lock() {
                        counter.remove(&vm);
                    }
                    continue;
                }
                self.log(&format!(
                    "GC: reclaiming {vm} (VM not running for {ABSENT_PASSES_BEFORE_RECLAIM} \
consecutive passes)"
                ));
                if let Err(error) = self.release(&vm, "vm-not-running", false) {
                    self.log(&format!("WARN: GC release of {vm} failed: {error}"));
                }
                if let Ok(mut counter) = self.absent_passes.lock() {
                    counter.remove(&vm);
                }
            } else {
                self.log(&format!("GC: reclaiming {vm} (TTL + grace expired)"));
                if let Err(error) = self.release(&vm, "ttl-expired", true) {
                    self.log(&format!("WARN: GC release of {vm} failed: {error}"));
                }
            }
        }
        Ok(())
    }

    /// Run the garbage collector forever.
    pub fn gc_loop(self: Arc<Self>) {
        loop {
            if let Err(error) = self.gc_once() {
                self.log(&format!("WARN: gc error: {error:?}"));
            }
            std::thread::sleep(std::time::Duration::from_secs(GC_INTERVAL_S));
        }
    }
}

/// The acquisition default TTL, in hours, used when a record carries none.
const DEFAULT_TTL_HOURS: f64 = 24.0;

/// The lease's own initial TTL: its resolved `initial_ttl_hours`, or the
/// acquisition default for a legacy record without a configuration report.
fn lease_initial_ttl_hours(record: &Value) -> f64 {
    record
        .get("configuration")
        .and_then(|config| config.get("effective"))
        .and_then(|effective| effective.get("initial_ttl_hours"))
        .and_then(Value::as_f64)
        .filter(|ttl| ttl.is_finite() && (0.1..=720.0).contains(ttl))
        .unwrap_or(DEFAULT_TTL_HOURS)
}

fn valid_purpose(purpose: &str) -> bool {
    if purpose.is_empty() || purpose.len() > 64 {
        return false;
    }
    let mut chars = purpose.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_vm_name(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn arg_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn tail_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn number_i(value: i64) -> Value {
    Value::Number(serde_json::Number::from(value))
}

fn format_gmtime(seconds: f64) -> String {
    // SAFETY: `gmtime_r` writes into a caller-provided `tm`.
    unsafe {
        let stamp = seconds as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&stamp, &mut tm).is_null() {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}
