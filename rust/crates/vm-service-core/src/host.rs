//! The host boundary: every interaction with Tart, SSH, and the guest passes
//! through one [`Host`] implementation.
//!
//! The Python daemon reached these through module-level functions, and the test
//! suite replaced them with `mock.patch.object`. The Rust port keeps the same
//! seam as an explicit trait: [`RealHost`] performs the real work, and tests
//! substitute an in-memory implementation without touching production code.
//!
//! Helpers that are pure derivations of `tart` (`tart_list`, `vm_exists`,
//! `vm_running`, `vm_ip`, `boot_refused`) are provided methods so an
//! implementation only has to supply the primitive it genuinely cannot fake.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::config::Config;
use crate::error::{OpError, OpResult};
use crate::lines::LineCache;

/// A booting guest process, observed while the hypervisor settles.
///
/// The Python fixture replaced `subprocess.Popen` with an object exposing
/// `poll()`. This trait is that same contract.
pub trait BootProcess: Send {
    /// `None` while the process is alive, `Some(rc)` once it has exited.
    fn poll(&mut self) -> Option<i32>;
}

/// The real boot process: a detached `tart run` child.
struct ChildBootProcess {
    child: Child,
}

impl BootProcess for ChildBootProcess {
    fn poll(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }
}

/// Raw captured output of one `tart` invocation.
#[derive(Debug, Clone, Default)]
pub struct TartOutput {
    /// The exit code, or `-1` when the process was signalled.
    pub code: i32,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

/// The host operations the service performs.
///
/// Implementations must be safe to share across the daemon's request threads.
pub trait Host: Send + Sync {
    /// Run `tart` and capture output; `check` mirrors Python `check=True`.
    fn tart(&self, args: &[String], check: bool, timeout_s: u64) -> OpResult<TartOutput>;

    /// Run a remote command. `None` on a connection or process failure,
    /// matching the Python `_ssh` helper.
    fn ssh(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        remote_cmd: &str,
        stdin: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> Option<(i32, String)>;

    /// Copy one path with `scp -r`. `None` on a process failure.
    fn scp(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        src: &str,
        dst: &str,
        timeout_s: u64,
    ) -> Option<(i32, String)>;

    /// Wait for a VM to report an IP within `timeout_s`.
    fn wait_ip(&self, name: &str, timeout_s: u64) -> Option<String>;

    /// Wait for key-only SSH readiness.
    fn wait_ssh(&self, ip: &str, user: &str, key_dir: &Path, timeout_s: u64) -> bool;

    /// Prove separate key-only push and pull connections and byte integrity.
    fn verify_transfer(&self, ip: &str, user: &str, key_dir: &Path) -> OpResult<()>;

    /// Install lease credentials in the guest.
    fn bootstrap(
        &self,
        ip: &str,
        user: &str,
        password: &str,
        key_dir: &Path,
        timeout_s: f64,
    ) -> OpResult<()>;

    /// Spawn `tart run <vm> --no-graphics` detached.
    fn spawn_run(&self, vm: &str) -> OpResult<Box<dyn BootProcess>>;

    /// The boot-settle window, in seconds.
    fn boot_settle_s(&self) -> f64;

    /// Advisory host-wide Virtualization.framework guest count. `None` when the
    /// gauge is unavailable; never fails.
    fn host_macos_guests(&self) -> Option<usize>;

    /// Discover image lines, caching on the images directory mtime.
    fn discover_lines(
        &self,
        cache: &mut LineCache,
        force: bool,
    ) -> OpResult<(Map<String, Value>, Map<String, Value>)>;

    /// The caller's home directory, used for credential packs and the Tart
    /// store. Defaults to `$HOME`.
    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    /// The Tart store root, whose `vms` child holds VM metadata.
    fn tart_store_root(&self) -> PathBuf {
        if let Ok(explicit) = std::env::var("TART_HOME") {
            return PathBuf::from(explicit);
        }
        self.home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".tart")
    }

    // ---------------------------------------------------------------- derived

    /// `[(name, state)]` for local VMs, parsed from `tart list`.
    fn tart_list(&self) -> OpResult<Vec<(String, String)>> {
        let output = self.tart(&["list".to_string()], true, 120)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut rows = Vec::new();
        for line in crate::python::splitlines(&text).into_iter().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 && parts[0] == "local" {
                rows.push((parts[1].to_string(), parts[parts.len() - 1].to_string()));
            }
        }
        Ok(rows)
    }

    /// Whether a VM name exists locally.
    fn vm_exists(&self, name: &str) -> OpResult<bool> {
        Ok(self
            .tart_list()?
            .iter()
            .any(|(candidate, _)| candidate == name))
    }

    /// Whether a VM exists locally and is running.
    fn vm_running(&self, name: &str) -> OpResult<bool> {
        Ok(self
            .tart_list()?
            .iter()
            .any(|(candidate, state)| candidate == name && state == "running"))
    }

    /// The first reported IP for a VM, or `None`.
    fn vm_ip(&self, name: &str) -> Option<String> {
        let output = self
            .tart(&["ip".to_string(), name.to_string()], false, 30)
            .ok()?;
        crate::python::splitlines(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
    }

    /// Detect a guest the hypervisor refused to start within the settle window.
    ///
    /// A `tart run` process that exits during the window is a refusal; a process
    /// that survives the window is booting.
    fn boot_refused(
        &self,
        process: &mut dyn BootProcess,
        vm: &str,
        settle_s: Option<f64>,
    ) -> OpResult<()> {
        let settle = settle_s.unwrap_or_else(|| self.boot_settle_s());
        let deadline = Instant::now() + Duration::from_secs_f64(settle.max(0.0));
        while Instant::now() < deadline {
            if let Some(code) = process.poll() {
                return Err(OpError::new(format!(
                    "{vm} refused to start (boot process exited rc={code} within {}s) — \
for macOS images the host-wide 2-VM limit may be consumed by another \
Virtualization.framework user; check 'GET /images' capacity and /tmp/tart-run-{vm}.log",
                    py_number(settle)
                )));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(Duration::from_millis(500)));
        }
        Ok(())
    }
}

/// Format a number the way Python renders a float default and an int default.
///
/// `10.0` prints as `10` and `0.3` as `0.3`, matching the Python message that
/// interpolates an `int` settle window and the tests' `float` overrides.
fn py_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The production host: real Tart, real SSH, real filesystem.
#[derive(Debug, Clone)]
pub struct RealHost {
    config: Config,
}

impl RealHost {
    /// Bind a host to a resolved configuration.
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Host for RealHost {
    fn tart(&self, args: &[String], check: bool, timeout_s: u64) -> OpResult<TartOutput> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = crate::tart::tart(&self.config, &refs, check, timeout_s)?;
        Ok(TartOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn ssh(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        remote_cmd: &str,
        stdin: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> Option<(i32, String)> {
        crate::ssh::ssh(
            &self.config,
            ip,
            user,
            key_dir,
            remote_cmd,
            stdin,
            timeout_s,
        )
    }

    fn scp(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        src: &str,
        dst: &str,
        timeout_s: u64,
    ) -> Option<(i32, String)> {
        crate::ssh::scp(&self.config, ip, user, key_dir, src, dst, timeout_s)
    }

    fn wait_ip(&self, name: &str, timeout_s: u64) -> Option<String> {
        crate::tart::wait_ip(&self.config, name, timeout_s)
    }

    fn wait_ssh(&self, ip: &str, user: &str, key_dir: &Path, timeout_s: u64) -> bool {
        crate::ssh::wait_ssh(&self.config, ip, user, key_dir, timeout_s)
    }

    fn verify_transfer(&self, ip: &str, user: &str, key_dir: &Path) -> OpResult<()> {
        crate::ssh::verify_transfer(&self.config, ip, user, key_dir, &self.config.state_dir)
    }

    fn bootstrap(
        &self,
        ip: &str,
        user: &str,
        password: &str,
        key_dir: &Path,
        timeout_s: f64,
    ) -> OpResult<()> {
        lease_keys::bootstrap(ip, user, password, key_dir, timeout_s)
            .map_err(|error| OpError::new(error.to_string()))
    }

    fn spawn_run(&self, vm: &str) -> OpResult<Box<dyn BootProcess>> {
        let child = crate::tart::spawn_run(&self.config, vm)?;
        Ok(Box::new(ChildBootProcess { child }))
    }

    fn boot_settle_s(&self) -> f64 {
        crate::tart::BOOT_SETTLE_S as f64
    }

    fn host_macos_guests(&self) -> Option<usize> {
        crate::tart::host_macos_guests()
    }

    fn discover_lines(
        &self,
        cache: &mut LineCache,
        force: bool,
    ) -> OpResult<(Map<String, Value>, Map<String, Value>)> {
        crate::lines::discover_lines(&self.config, cache, force)
    }

    fn tart_store_root(&self) -> PathBuf {
        if let Some(environment) = &self.config.environment {
            if let Some(path) = environment
                .get("profile")
                .and_then(|profile| profile.get("tartHome"))
                .and_then(Value::as_str)
            {
                return PathBuf::from(path);
            }
        }
        if let Ok(explicit) = std::env::var("TART_HOME") {
            return PathBuf::from(explicit);
        }
        self.home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".tart")
    }
}
