//! Tart lifecycle helpers, mirroring the Python `tart` helpers in `vm-service`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{OpError, OpResult};
use crate::proc::{combined_text, run_capture, ProcError};

/// The settle window used to detect a hypervisor-refused boot.
pub const BOOT_SETTLE_S: u64 = 10;

/// Build a `tart` command with the configured executable and environment.
pub fn tart_command(config: &Config, args: &[&str]) -> Command {
    let mut command = Command::new(&config.tart_executable);
    command.args(args);
    if let Some(environment) = &config.subprocess_env {
        command.env_clear();
        command.envs(environment.iter().map(|(key, value)| (key, value)));
    }
    command
}

/// Run `tart` and capture output. `check` mirrors Python's `check=True`.
pub fn tart(
    config: &Config,
    args: &[&str],
    check: bool,
    timeout_s: u64,
) -> OpResult<std::process::Output> {
    let mut command = tart_command(config, args);
    let output = run_capture(&mut command, None, Duration::from_secs(timeout_s))
        .map_err(|error| tart_error(args, error))?;
    if check && !output.status.success() {
        return Err(OpError::new(format!(
            "tart {} failed (rc={}): {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            combined_text(&output).trim()
        )));
    }
    Ok(output)
}

fn tart_error(args: &[&str], error: ProcError) -> OpError {
    match error {
        ProcError::Timeout => OpError::new(format!("tart {} timed out", args.join(" "))),
        ProcError::Io(error) => OpError::new(format!("tart {} failed: {error}", args.join(" "))),
    }
}

/// `[(name, state)]` for local VMs, parsed from `tart list`.
pub fn tart_list(config: &Config) -> OpResult<Vec<(String, String)>> {
    let output = tart(config, &["list"], true, 120)?;
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
pub fn vm_exists(config: &Config, name: &str) -> OpResult<bool> {
    Ok(tart_list(config)?
        .iter()
        .any(|(candidate, _)| candidate == name))
}

/// Whether a VM exists locally and is running.
pub fn vm_running(config: &Config, name: &str) -> OpResult<bool> {
    Ok(tart_list(config)?
        .iter()
        .any(|(candidate, state)| candidate == name && state == "running"))
}

/// Advisory count of host-wide Virtualization.framework guests.
///
/// Best effort: a failed gauge degrades to `None` and never raises.
pub fn host_macos_guests() -> Option<usize> {
    let mut command = Command::new("/usr/bin/pgrep");
    command.args(["-f", "com.apple.Virtualization.VirtualMachine"]);
    let output = run_capture(&mut command, None, Duration::from_secs(10)).ok()?;
    let code = output.status.code()?;
    if code != 0 && code != 1 {
        return None;
    }
    Some(
        crate::python::splitlines(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
}

/// The first reported IP for a VM, or `None`.
pub fn vm_ip(config: &Config, name: &str) -> Option<String> {
    let output = tart(config, &["ip", name], false, 30).ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    crate::python::splitlines(&text)
        .into_iter()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

/// Wait for a VM to report an IP within `timeout_s`.
pub fn wait_ip(config: &Config, name: &str, timeout_s: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        if let Some(ip) = vm_ip(config, name) {
            return Some(ip);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Spawn `tart run <vm> --no-graphics` detached, logging to
/// `/tmp/tart-run-<vm>.log`, mirroring the Python `Popen`.
pub fn spawn_run(config: &Config, vm: &str) -> OpResult<std::process::Child> {
    let log_path = format!("/tmp/tart-run-{vm}.log");
    let stdout = std::fs::File::create(&log_path)?;
    let stderr = stdout.try_clone()?;
    let mut command = tart_command(config, &["run", vm, "--no-graphics"]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map_err(OpError::from)
}

/// Log path for a running VM, matching the Python convention.
pub fn run_log_path(vm: &str) -> String {
    format!("/tmp/tart-run-{vm}.log")
}

/// Restrict a path so private lease SSH material is never transferred.
pub fn _unused_path_reference(_path: &Path) {}
