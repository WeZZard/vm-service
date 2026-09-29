//! Shared harness for the `vm-service-core` tests.
//!
//! Mirrors `tests/common.py`: real isolated disk state and lease keys, with an
//! in-memory Tart and SSH behind the [`Host`] seam. The Python suite reached
//! these with `mock.patch.object` on the daemon module; the port substitutes a
//! `FakeHost` instead.
//!
//! Included by each integration test binary via `mod common;`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};
use tempfile::TempDir;

use vm_service_core::config::Config;
use vm_service_core::console_api::ConsoleController;
use vm_service_core::error::{OpError, OpResult};
use vm_service_core::host::{BootProcess, Host, TartOutput};
use vm_service_core::lines::LineCache;
use vm_service_core::service::Service;

/// The IP reported by the fake host, matching `ServiceFixture`.
pub const FAKE_IP: &str = "192.168.64.99";

// ------------------------------------------------------------------ FakeTart

/// In-memory `tart`: a name set with a running flag, recorded calls, and
/// injectable failures. Mirrors `tests/common.py::FakeTart`.
#[derive(Default)]
pub struct FakeTart {
    /// `name -> running`.
    pub vms: Mutex<HashMap<String, bool>>,
    /// `op -> message` for failure injection.
    pub fail: Mutex<HashMap<String, String>>,
    /// Recorded `(op, args)` pairs.
    pub calls: Mutex<Vec<(String, Vec<String>)>>,
}

impl FakeTart {
    /// An empty fake with no VMs and no injected failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a name exists: a cloned VM, or one of the two golden bases.
    pub fn exists(&self, name: &str) -> bool {
        self.vms.lock().expect("vms").contains_key(name)
            || name.starts_with("pilot-macos26-base")
            || name.starts_with("pilot-ubuntu-base")
    }

    /// Number of VMs the fake considers running.
    pub fn running_count(&self) -> usize {
        self.vms
            .lock()
            .expect("vms")
            .values()
            .filter(|running| **running)
            .count()
    }

    /// Injected failure for `op`, if any.
    pub fn inject(&self, op: &str, message: &str) {
        self.fail
            .lock()
            .expect("fail")
            .insert(op.to_string(), message.to_string());
    }

    /// All recorded operations, in order.
    pub fn ops(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .map(|(op, _)| op.clone())
            .collect()
    }

    /// Arguments recorded for one operation, in order.
    pub fn args_for(&self, wanted: &str) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|(op, _)| op == wanted)
            .map(|(_, args)| args.clone())
            .collect()
    }

    /// First argument recorded for the first matching operation.
    pub fn first_arg_of(&self, wanted: &str) -> Option<String> {
        self.args_for(wanted)
            .first()
            .and_then(|args| args.first().cloned())
    }

    /// Run one fake `tart` invocation.
    pub fn call(&self, args: &[String], check: bool) -> OpResult<TartOutput> {
        let op = args.first().cloned().unwrap_or_default();
        let rest: Vec<String> = args.iter().skip(1).cloned().collect();
        self.calls
            .lock()
            .expect("calls")
            .push((op.clone(), rest.clone()));
        if let Some(message) = self.fail.lock().expect("fail").get(&op).cloned() {
            return Err(OpError::new(message));
        }
        match op.as_str() {
            "list" => {
                let mut rows = vec!["Name\tUUID\tArch\tDisk\tState".to_string()];
                for (name, running) in self.vms.lock().expect("vms").iter() {
                    let state = if *running { "running" } else { "stopped" };
                    rows.push(format!("local  {name}  100  11  aarch64  {state}"));
                }
                for base in ["pilot-macos26-base", "pilot-ubuntu-base"] {
                    rows.push(format!("local  {base}  100  11  aarch64  stopped"));
                }
                Ok(TartOutput {
                    code: 0,
                    stdout: rows.join("\n").into_bytes(),
                    stderr: Vec::new(),
                })
            }
            "clone" => {
                if let Some(name) = rest.get(1) {
                    self.vms.lock().expect("vms").insert(name.clone(), false);
                }
                Ok(TartOutput::default())
            }
            "set" => Ok(TartOutput::default()),
            "stop" => {
                if let Some(name) = rest.first() {
                    if let Some(running) = self.vms.lock().expect("vms").get_mut(name) {
                        *running = false;
                    }
                }
                Ok(TartOutput::default())
            }
            "delete" => {
                let mut vms = self.vms.lock().expect("vms");
                if let Some(name) = rest.first() {
                    let running = vms.get(name).copied() == Some(true);
                    if running && check {
                        return Err(OpError::new("cannot delete running VM"));
                    }
                    if running {
                        vms.insert(name.clone(), false);
                    }
                    vms.remove(name);
                }
                Ok(TartOutput::default())
            }
            _ => Ok(TartOutput::default()),
        }
    }
}

// ------------------------------------------------------------------- FakeSSH

/// A recorded `ssh` call: `(remote command, stdin as text, timeout seconds)`.
pub type SshCall = (String, Option<String>, u64);

/// A recorded `scp` call: `(source, destination, timeout seconds)`.
pub type ScpCall = (String, String, u64);

/// In-memory `ssh`/`scp` with injectable failures and scripted responses.
/// Mirrors `tests/common.py::FakeSSH`.
#[derive(Default)]
pub struct FakeSSH {
    /// Recorded `ssh` calls.
    pub ssh_calls: Mutex<Vec<SshCall>>,
    /// Recorded `scp` calls.
    pub scp_calls: Mutex<Vec<ScpCall>>,
    /// A failure to return once, for the next `ssh` or `scp`.
    pub fail_next: Mutex<Option<String>>,
    /// `substring -> (rc, output)` responses, checked in insertion order.
    pub rc_map: Mutex<Vec<(String, (i32, String))>>,
}

impl FakeSSH {
    /// An empty fake: every command succeeds with empty output.
    pub fn new() -> Self {
        Self::default()
    }

    /// Map any remote command containing `fragment` to `(rc, output)`.
    pub fn map(&self, fragment: &str, rc: i32, output: &str) {
        self.rc_map
            .lock()
            .expect("rc_map")
            .push((fragment.to_string(), (rc, output.to_string())));
    }

    /// The recorded remote commands, in order.
    pub fn commands(&self) -> Vec<String> {
        self.ssh_calls
            .lock()
            .expect("ssh_calls")
            .iter()
            .map(|(command, _, _)| command.clone())
            .collect()
    }

    /// Whether any `ssh` call carried a command containing `needle`.
    pub fn saw_command(&self, needle: &str) -> bool {
        self.commands()
            .iter()
            .any(|command| command.contains(needle))
    }

    /// The stdin recorded for the first `ssh` command containing `needle`.
    pub fn stdin_for(&self, needle: &str) -> Option<String> {
        self.ssh_calls
            .lock()
            .expect("ssh_calls")
            .iter()
            .find(|(command, _, _)| command.contains(needle))
            .and_then(|(_, stdin, _)| stdin.clone())
    }

    /// The timeout recorded for the first `ssh` command containing `needle`.
    pub fn ssh_timeout_for(&self, needle: &str) -> Option<u64> {
        self.ssh_calls
            .lock()
            .expect("ssh_calls")
            .iter()
            .find(|(command, _, _)| command.contains(needle))
            .map(|(_, _, timeout)| *timeout)
    }

    /// Run one fake `ssh`.
    pub fn ssh(
        &self,
        remote_cmd: &str,
        stdin: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> Option<(i32, String)> {
        let text = stdin.map(|bytes| String::from_utf8_lossy(&bytes).to_string());
        self.ssh_calls
            .lock()
            .expect("ssh_calls")
            .push((remote_cmd.to_string(), text, timeout_s));
        if self.fail_next.lock().expect("fail_next").take().is_some() {
            return None;
        }
        for (fragment, response) in self.rc_map.lock().expect("rc_map").iter() {
            if remote_cmd.contains(fragment.as_str()) {
                return Some(response.clone());
            }
        }
        if remote_cmd == "true" {
            return Some((0, String::new()));
        }
        if remote_cmd.contains("test -s ~/.config/zsh/secrets.zsh") {
            return Some((0, "OK".to_string()));
        }
        if remote_cmd.contains("bash -s") || remote_cmd.contains("zsh -s") {
            return Some((0, "script-ran".to_string()));
        }
        Some((0, String::new()))
    }

    /// Run one fake `scp`.
    pub fn scp(&self, src: &str, dst: &str, timeout_s: u64) -> Option<(i32, String)> {
        self.scp_calls.lock().expect("scp_calls").push((
            src.to_string(),
            dst.to_string(),
            timeout_s,
        ));
        if self.fail_next.lock().expect("fail_next").take().is_some() {
            return None;
        }
        Some((0, String::new()))
    }
}

// ------------------------------------------------------------------ BootProc

/// A fake boot process with a fixed observed exit status.
pub struct FakeBootProcess {
    exit: Option<i32>,
}

/// Build a boot process that has already exited with `exit` (`None` = alive).
///
/// Mirrors the Python fixture's `P` object with a scripted `poll()`.
pub fn boot_process(exit: Option<i32>) -> Box<dyn BootProcess> {
    Box::new(FakeBootProcess { exit })
}

impl BootProcess for FakeBootProcess {
    fn poll(&mut self) -> Option<i32> {
        self.exit
    }
}

// ------------------------------------------------------------------ FakeHost

/// A replacement `ssh` implementation, used by the key-lifecycle fixture.
pub type SshHook =
    Arc<dyn Fn(&str, &str, &Path, &str, Option<&[u8]>, u64) -> Option<(i32, String)> + Send + Sync>;
/// A replacement `scp` implementation.
pub type ScpHook =
    Arc<dyn Fn(&str, &str, &Path, &str, &str, u64) -> Option<(i32, String)> + Send + Sync>;
/// A replacement `wait_ssh` implementation.
pub type WaitSshHook = Arc<dyn Fn(&str, &str, &Path, u64) -> bool + Send + Sync>;
/// A replacement `bootstrap` implementation.
pub type BootstrapHook = Arc<dyn Fn(&str, &str, &str, &Path, f64) + Send + Sync>;
/// A replacement `verify_transfer` implementation.
pub type TransferHook = Arc<dyn Fn(&str, &str, &Path) -> OpResult<()> + Send + Sync>;

/// The in-memory host: fake Tart, fake SSH, injected line discovery, and the
/// knobs `ServiceFixture` exposed.
pub struct FakeHost {
    /// In-memory Tart.
    pub tart: FakeTart,
    /// In-memory SSH and SCP.
    pub ssh: FakeSSH,
    /// The image map returned by `discover_lines`.
    pub lines: Map<String, Value>,
    /// Replaces [`FakeHost::lines`] when set, for tests that change an image's configuration.
    pub lines_override: Mutex<Option<Map<String, Value>>>,
    /// The base-VM map returned by `discover_lines`.
    pub bases: Map<String, Value>,
    /// The IP `wait_ip` reports.
    pub wait_ip_value: Mutex<Option<String>>,
    /// What `wait_ssh` reports.
    pub wait_ssh_ok: AtomicBool,
    /// Whether `verify_transfer` is stubbed (the Python default).
    pub stub_transfer: AtomicBool,
    /// The boot-settle window in seconds.
    pub boot_settle: Mutex<f64>,
    /// The advisory host-wide guest gauge.
    pub gauge: Mutex<Option<usize>>,
    /// The home directory used for credential packs and the Tart store.
    pub home: PathBuf,
    /// When set, `spawn_run` reports an immediately-exited boot process with
    /// this code and does not mark the VM running.
    pub boot_exit: Mutex<Option<i32>>,
    /// Optional replacement for `ssh`.
    pub ssh_hook: Mutex<Option<SshHook>>,
    /// Optional replacement for `scp`.
    pub scp_hook: Mutex<Option<ScpHook>>,
    /// Optional replacement for `wait_ssh`.
    pub wait_ssh_hook: Mutex<Option<WaitSshHook>>,
    /// Optional replacement for `bootstrap`.
    pub bootstrap_hook: Mutex<Option<BootstrapHook>>,
    /// Optional replacement for `verify_transfer`.
    pub transfer_hook: Mutex<Option<TransferHook>>,
    /// The Tart store root reported to the service.
    pub tart_store: Mutex<PathBuf>,
}

impl FakeHost {
    /// Build a host with the fixture's default image lines.
    pub fn new(home: PathBuf) -> Self {
        let home_tart_store = home.join(".tart");
        Self {
            tart: FakeTart::new(),
            ssh: FakeSSH::new(),
            lines: default_lines(),
            lines_override: Mutex::new(None),
            bases: default_bases(),
            wait_ip_value: Mutex::new(Some(FAKE_IP.to_string())),
            wait_ssh_ok: AtomicBool::new(true),
            stub_transfer: AtomicBool::new(true),
            boot_settle: Mutex::new(0.05),
            gauge: Mutex::new(Some(0)),
            home,
            boot_exit: Mutex::new(None),
            ssh_hook: Mutex::new(None),
            scp_hook: Mutex::new(None),
            wait_ssh_hook: Mutex::new(None),
            bootstrap_hook: Mutex::new(None),
            transfer_hook: Mutex::new(None),
            tart_store: Mutex::new(home_tart_store),
        }
    }

    /// Set the boot-settle window.
    pub fn set_boot_settle(&self, seconds: f64) {
        *self.boot_settle.lock().expect("boot_settle") = seconds;
    }

    /// Make the next `spawn_run` report an immediately-exited process.
    pub fn refuse_boot(&self, code: i32) {
        *self.boot_exit.lock().expect("boot_exit") = Some(code);
    }

    /// Replace the advisory host gauge.
    pub fn set_gauge(&self, value: Option<usize>) {
        *self.gauge.lock().expect("gauge") = value;
    }
}

impl Host for FakeHost {
    fn tart(&self, args: &[String], check: bool, timeout_s: u64) -> OpResult<TartOutput> {
        let _ = timeout_s;
        self.tart.call(args, check)
    }

    fn ssh(
        &self,
        ip: &str,
        user: &str,
        key_dir: &std::path::Path,
        remote_cmd: &str,
        stdin: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> Option<(i32, String)> {
        if let Some(hook) = self.ssh_hook.lock().expect("ssh_hook").clone() {
            return hook(ip, user, key_dir, remote_cmd, stdin.as_deref(), timeout_s);
        }
        self.ssh.ssh(remote_cmd, stdin, timeout_s)
    }

    fn scp(
        &self,
        ip: &str,
        user: &str,
        key_dir: &std::path::Path,
        src: &str,
        dst: &str,
        timeout_s: u64,
    ) -> Option<(i32, String)> {
        if let Some(hook) = self.scp_hook.lock().expect("scp_hook").clone() {
            return hook(ip, user, key_dir, src, dst, timeout_s);
        }
        self.ssh.scp(src, dst, timeout_s)
    }

    fn wait_ip(&self, _name: &str, _timeout_s: u64) -> Option<String> {
        self.wait_ip_value.lock().expect("wait_ip").clone()
    }

    fn wait_ssh(&self, ip: &str, user: &str, key_dir: &std::path::Path, timeout_s: u64) -> bool {
        if let Some(hook) = self.wait_ssh_hook.lock().expect("wait_ssh_hook").clone() {
            return hook(ip, user, key_dir, timeout_s);
        }
        self.wait_ssh_ok.load(Ordering::SeqCst)
    }

    fn verify_transfer(&self, ip: &str, user: &str, key_dir: &std::path::Path) -> OpResult<()> {
        if let Some(hook) = self.transfer_hook.lock().expect("transfer_hook").clone() {
            return hook(ip, user, key_dir);
        }
        Ok(())
    }

    fn bootstrap(
        &self,
        ip: &str,
        user: &str,
        password: &str,
        key_dir: &std::path::Path,
        timeout_s: f64,
    ) -> OpResult<()> {
        if let Some(hook) = self.bootstrap_hook.lock().expect("bootstrap_hook").clone() {
            hook(ip, user, password, key_dir, timeout_s);
        }
        Ok(())
    }

    fn spawn_run(&self, vm: &str) -> OpResult<Box<dyn BootProcess>> {
        self.tart
            .calls
            .lock()
            .expect("calls")
            .push(("run".to_string(), vec![vm.to_string()]));
        if let Some(code) = *self.boot_exit.lock().expect("boot_exit") {
            return Ok(Box::new(FakeBootProcess { exit: Some(code) }));
        }
        self.tart
            .vms
            .lock()
            .expect("vms")
            .insert(vm.to_string(), true);
        Ok(Box::new(FakeBootProcess { exit: None }))
    }

    fn boot_settle_s(&self) -> f64 {
        *self.boot_settle.lock().expect("boot_settle")
    }

    fn host_macos_guests(&self) -> Option<usize> {
        *self.gauge.lock().expect("gauge")
    }

    fn discover_lines(
        &self,
        _cache: &mut LineCache,
        _force: bool,
    ) -> OpResult<(Map<String, Value>, Map<String, Value>)> {
        let lines = self.lines_override.lock().expect("lines_override").clone();
        Ok((lines.unwrap_or_else(|| self.lines.clone()), self.bases.clone()))
    }

    fn home_dir(&self) -> Option<PathBuf> {
        Some(self.home.clone())
    }

    fn tart_store_root(&self) -> PathBuf {
        self.tart_store.lock().expect("tart_store").clone()
    }
}

/// The two image lines every fixture exposes, matching `ServiceFixture.lines`.
pub fn default_lines() -> Map<String, Value> {
    let mut lines = Map::new();
    lines.insert(
        "macos26".to_string(),
        json!({
            "kind": "macos", "base_vm": "pilot-macos26-base",
            "clone_prefix": "pilot-mac-", "ssh_user": "station",
            "ssh_pass": "station", "defaults": {},
            "source": "test://macos26/line.conf",
        }),
    );
    lines.insert(
        "ubuntu2404".to_string(),
        json!({
            "kind": "linux", "base_vm": "pilot-ubuntu-base",
            "clone_prefix": "pilot-", "ssh_user": "admin",
            "ssh_pass": "admin", "defaults": {},
            "source": "test://ubuntu2404/line.conf",
        }),
    );
    lines
}

/// The base-VM map matching [`default_lines`].
pub fn default_bases() -> Map<String, Value> {
    let mut bases = Map::new();
    bases.insert(
        "macos26".to_string(),
        Value::String("pilot-macos26-base".to_string()),
    );
    bases.insert(
        "ubuntu2404".to_string(),
        Value::String("pilot-ubuntu-base".to_string()),
    );
    bases
}

// ------------------------------------------------------------------ Fixture

/// One isolated service instance with real disk state and a fake host.
pub struct Fixture {
    /// The temporary root; dropped at the end of the test.
    pub dir: TempDir,
    /// The in-memory host, shared with the service.
    pub host: Arc<FakeHost>,
    /// The bound service.
    pub service: Arc<Service>,
    /// `state_dir/state.json`.
    pub state_file: PathBuf,
}

impl Fixture {
    /// Build a fixture with `verify_transfer` stubbed, like the Python default.
    pub fn new() -> Self {
        Self::with_transfer_stub(true)
    }

    /// Build a fixture with an injected console controller.
    ///
    /// The console analogue of the Python fixture's
    /// `self.svc.CONSOLES = <manager>` replacement, using the same
    /// `verify_transfer` stub as [`Fixture::new`].
    pub fn with_consoles(consoles: Arc<dyn ConsoleController>) -> Self {
        Self::build(true, Some(consoles))
    }

    /// Build a fixture, optionally exercising the real transfer check.
    pub fn with_transfer_stub(stub: bool) -> Self {
        Self::build(stub, None)
    }

    fn build(stub: bool, consoles: Option<Arc<dyn ConsoleController>>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS `/var` is a symlink; lease keys intentionally reject those.
        let root = dir.path().canonicalize().expect("canonicalize");
        let config = Config {
            environment: None,
            tart_executable: "tart".to_string(),
            subprocess_env: None,
            pilot: root.join("pilot"),
            state_dir: root.clone(),
            state_file: root.join("state.json"),
            lock_file: root.join("state.lock"),
            log_file: root.join("service.log"),
            host: "127.0.0.1".to_string(),
            port: 6240,
        };
        let host = Arc::new(FakeHost::new(root.join("home")));
        host.stub_transfer.store(stub, Ordering::SeqCst);
        let state_file = config.state_file.clone();
        let host_trait = Arc::clone(&host) as Arc<dyn Host>;
        let service = Arc::new(match consoles {
            Some(consoles) => Service::with_consoles(config, consoles, host_trait),
            None => Service::with_host(config, None, host_trait),
        });
        Self {
            dir,
            host,
            service,
            state_file,
        }
    }

    /// Overwrite the state file with a `vms` map.
    pub fn write_state(&self, vms: Value) {
        if let Some(parent) = self.state_file.parent() {
            std::fs::create_dir_all(parent).expect("state dir");
        }
        let body = json!({"vms": vms});
        std::fs::write(&self.state_file, body.to_string()).expect("write state");
    }

    /// Read the `vms` map from the state file.
    pub fn read_state(&self) -> Map<String, Value> {
        let text = std::fs::read_to_string(&self.state_file).expect("read state");
        let value: Value = serde_json::from_str(&text).expect("parse state");
        value
            .get("vms")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    /// Create a credential pack under the fake home; returns the pack path.
    pub fn seed_pack(&self, name: &str, with_env: bool, with_git: bool) -> PathBuf {
        let pack = self.host.home.join(".config/vm-credentials").join(name);
        std::fs::create_dir_all(&pack).expect("pack dir");
        if with_env {
            std::fs::write(pack.join("env.extra"), "export FOO=bar\n").expect("env.extra");
        }
        if with_git {
            std::fs::write(pack.join("git-identity"), "Test User <t@example.com>")
                .expect("git-identity");
        }
        pack
    }

    /// Build a lease key directory for `vm`.
    pub fn key_dir(&self, vm: &str) -> PathBuf {
        lease_keys::create(&self.service.config.state_dir, vm).expect("lease keys")
    }

    /// Snapshot the `GET /vms` body.
    pub fn snapshot(&self) -> Value {
        vm_service_core::snapshot::snapshot(&self.service).expect("snapshot")
    }

    /// Snapshot the `GET /images` body.
    pub fn images_snapshot(&self) -> Value {
        vm_service_core::snapshot::images_snapshot(&self.service).expect("images snapshot")
    }

    /// Acquire with the Python defaults: base source, NAT, TTL 24h, wait.
    pub fn acquire(&self, purpose: &str, image: &str, env: &str) -> Value {
        self.try_acquire(purpose, image, env, &json!(24), true)
            .expect("acquire")
    }

    /// Acquire, returning the operation result.
    pub fn try_acquire(
        &self,
        purpose: &str,
        image: &str,
        env: &str,
        ttl_hours: &Value,
        wait: bool,
    ) -> OpResult<Value> {
        self.try_acquire_vnc(purpose, image, env, ttl_hours, wait, false)
    }

    /// Acquire with an explicit console flag, mirroring `vnc=True`.
    pub fn try_acquire_vnc(
        &self,
        purpose: &str,
        image: &str,
        env: &str,
        ttl_hours: &Value,
        wait: bool,
        vnc: bool,
    ) -> OpResult<Value> {
        self.service.acquire(
            purpose, image, env, ttl_hours, None, None, None, wait, "nat", None, "base", None, vnc,
        )
    }

    /// Release with the Python defaults.
    pub fn release(&self, vm: &str) -> Value {
        self.service
            .release(vm, "released", false)
            .expect("release")
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}
