//! The durable machine's capability report, first cut
//! (`docs/arch/machines.md`, MM-3 and MM-4).
//!
//! Cheap facts are read on every request: free disk, the display, the CI
//! runner and the agent keys. Expensive facts are read at startup, once a
//! day, at the start and the end of every durable lease and on refresh: the
//! OS, the hardware, the GPU, the software and the privacy permissions. A
//! refresh runs only read-only commands, each with a timeout.
//!
//! A fact the instance could not observe is `null`, and its reason is listed
//! under `unknown`. The instance never guesses a fact.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::agents::AgentKeys;
use super::config::{user_name, DurableConfig};
use super::process;
use crate::config::unix_now;

/// The report's schema version (MM-3).
pub const SCHEMA_VERSION: u64 = 1;
/// The reason of a section that was not observed yet.
const NOT_YET: &str = "not observed yet";

/// The expensive sections and when they were observed.
#[derive(Debug, Clone)]
struct Expensive {
    os: Value,
    hardware: Value,
    gpu: Value,
    software: Value,
    permissions: Value,
    unknown: Map<String, Value>,
    observed_at: Map<String, Value>,
    refreshed_at: Option<f64>,
}

impl Default for Expensive {
    fn default() -> Self {
        let mut unknown = Map::new();
        for section in ["os", "hardware", "gpu", "software", "permissions"] {
            unknown.insert(section.to_string(), json!(NOT_YET));
        }
        Self {
            os: json!({"name": null, "version": null, "build": null}),
            hardware: json!({"chip": null, "cpu_cores": null, "memory_bytes": null, "hardware_uuid": null}),
            gpu: gpu_unknown(),
            software: Value::Null,
            permissions: json!({"screen_recording": "unknown", "accessibility": "unknown", "post_event": "unknown"}),
            unknown,
            observed_at: Map::new(),
            refreshed_at: None,
        }
    }
}

fn gpu_unknown() -> Value {
    json!({"name": null, "metal_families": null, "metal3": null, "argument_buffers_tier": null})
}

/// The cached capability facts of one durable machine.
#[derive(Debug, Default)]
pub struct Capabilities {
    expensive: Mutex<Expensive>,
}

impl Capabilities {
    /// Facts not observed yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// When the expensive facts were last refreshed.
    pub fn refreshed_at(&self) -> Option<f64> {
        self.expensive
            .lock()
            .ok()
            .and_then(|facts| facts.refreshed_at)
    }

    /// The `software` section, for the inventory (HY-4).
    pub fn software(&self) -> Option<Value> {
        self.expensive
            .lock()
            .ok()
            .map(|facts| facts.software.clone())
            .filter(|software| !software.is_null())
    }

    /// The machine record's `cpu`, `memory_mb` and `disk_gb` (MM-3).
    pub fn resources(&self) -> (Value, Value, Value) {
        let facts = self.expensive.lock().ok();
        let cpu = facts
            .as_ref()
            .map(|facts| facts.hardware["cpu_cores"].clone())
            .unwrap_or(Value::Null);
        let memory = facts
            .as_ref()
            .and_then(|facts| facts.hardware["memory_bytes"].as_u64())
            .map(|bytes| json!(bytes / (1024 * 1024)))
            .unwrap_or(Value::Null);
        let disk = disk("/")
            .ok()
            .map(|(total, _)| json!(total / (1024 * 1024 * 1024)))
            .unwrap_or(Value::Null);
        (cpu, memory, disk)
    }

    /// Re-read the expensive facts (MM-4). `login_env` is the account's
    /// login environment, whose `PATH` finds the tools. Runs only read-only
    /// commands, each bounded by the configured fact timeout.
    pub fn refresh(&self, config: &DurableConfig, login_env: &[(String, String)]) {
        let timeout = config.fact_timeout;
        let mut unknown = Map::new();
        let mut observed_at = Map::new();
        let now = unix_now();

        let os = match os_facts(timeout) {
            Ok(os) => os,
            Err(reason) => {
                unknown.insert("os".to_string(), json!(reason));
                json!({"name": null, "version": null, "build": null})
            }
        };
        observed_at.insert("os".to_string(), json!(now));
        let hardware = hardware_facts(&mut unknown);
        observed_at.insert("hardware".to_string(), json!(now));

        let (gpu, permissions) = match &config.probe {
            None => {
                let reason = "no host probe configured";
                unknown.insert("gpu".to_string(), json!(reason));
                unknown.insert("permissions".to_string(), json!(reason));
                (
                    gpu_unknown(),
                    json!({"screen_recording": "unknown", "accessibility": "unknown", "post_event": "unknown"}),
                )
            }
            Some(command) => match run_probe(command, timeout) {
                Ok(answer) => {
                    let gpu = match answer.get("gpu") {
                        Some(gpu) if gpu.is_object() => gpu.clone(),
                        _ => {
                            unknown.insert(
                                "gpu".to_string(),
                                json!("the probe found no Metal device"),
                            );
                            gpu_unknown()
                        }
                    };
                    let permissions = answer.get("permissions").cloned().unwrap_or_else(|| {
                        unknown.insert(
                            "permissions".to_string(),
                            json!("the probe answered no permissions"),
                        );
                        json!({"screen_recording": "unknown", "accessibility": "unknown", "post_event": "unknown"})
                    });
                    (gpu, permissions)
                }
                Err(reason) => {
                    unknown.insert("gpu".to_string(), json!(reason.clone()));
                    unknown.insert("permissions".to_string(), json!(reason));
                    (
                        gpu_unknown(),
                        json!({"screen_recording": "unknown", "accessibility": "unknown", "post_event": "unknown"}),
                    )
                }
            },
        };
        observed_at.insert("gpu".to_string(), json!(now));
        observed_at.insert("permissions".to_string(), json!(now));

        let software = software_facts(login_env, timeout);
        observed_at.insert("software".to_string(), json!(unix_now()));

        if let Ok(mut facts) = self.expensive.lock() {
            *facts = Expensive {
                os,
                hardware,
                gpu,
                software,
                permissions,
                unknown,
                observed_at,
                refreshed_at: Some(unix_now()),
            };
        }
    }

    /// The full report: the cached expensive facts plus the cheap facts read
    /// now (MM-3, MM-4). `agents` carries states only, never a key.
    pub fn report(&self, config: &DurableConfig, agents: &AgentKeys) -> Value {
        let facts = self
            .expensive
            .lock()
            .map(|facts| facts.clone())
            .unwrap_or_default();
        let mut unknown = facts.unknown.clone();
        let mut observed_at = facts.observed_at.clone();
        let now = unix_now();

        let disk_section = match disk("/") {
            Ok((total, free)) => json!({"total_bytes": total, "free_bytes": free}),
            Err(reason) => {
                unknown.insert("disk".to_string(), json!(reason));
                json!({"total_bytes": null, "free_bytes": null})
            }
        };
        observed_at.insert("disk".to_string(), json!(now));

        let console_user = std::fs::metadata("/dev/console")
            .ok()
            .map(|metadata| user_name(metadata.uid()));
        if console_user.is_none() {
            unknown.insert(
                "display.console_user".to_string(),
                json!("/dev/console could not be read"),
            );
        }
        unknown.insert(
            "display.screen_locked".to_string(),
            json!("not observed by this version"),
        );
        let display = json!({
            "console_user": console_user,
            "gui_session": console_user.as_deref().map(|user| user == config.user),
            "screen_locked": null,
        });
        observed_at.insert("display".to_string(), json!(now));

        let ci_runner = ci_runner(&config.ci_root, config.uid);
        observed_at.insert("ci_runner".to_string(), json!(now));
        observed_at.insert("agents".to_string(), json!(now));

        json!({
            "schema_version": SCHEMA_VERSION,
            "network": "lan",
            "os": facts.os,
            "hardware": facts.hardware,
            "disk": disk_section,
            "gpu": facts.gpu,
            "display": display,
            "software": facts.software,
            "permissions": facts.permissions,
            "agents": agents.capability(),
            "ci_runner": ci_runner,
            "observed_at": observed_at,
            "unknown": unknown,
        })
    }
}

/// The total and free bytes of the volume holding `path`.
pub fn disk(path: &str) -> Result<(u64, u64), String> {
    let c_path = std::ffi::CString::new(path).map_err(|_| "invalid path".to_string())?;
    // SAFETY: `statfs` is plain old data; zeroed is a valid value.
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated; `stats` is writable.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stats) } != 0 {
        return Err(format!(
            "statfs {path}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let block = stats.f_bsize as u64;
    Ok((stats.f_blocks * block, stats.f_bavail * block))
}

fn os_facts(timeout: Duration) -> Result<Value, String> {
    let mut command = Command::new("/usr/bin/sw_vers");
    let output = crate::proc::run_capture(&mut command, None, timeout)
        .map_err(|error| format!("sw_vers: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |name: &str| -> Value {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| json!(value.trim()))
            .unwrap_or(Value::Null)
    };
    Ok(json!({
        "name": field("ProductName:"),
        "version": field("ProductVersion:"),
        "build": field("BuildVersion:"),
    }))
}

fn sysctl_text(name: &str) -> Option<String> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut size: libc::size_t = 0;
    // SAFETY: a size query with a null buffer.
    if unsafe {
        libc::sysctlbyname(
            c_name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: the buffer is writable for `size` bytes.
    if unsafe {
        libc::sysctlbyname(
            c_name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buffer.truncate(size);
    let text = String::from_utf8_lossy(&buffer)
        .trim_end_matches('\0')
        .to_string();
    (!text.is_empty()).then_some(text)
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let mut value: u64 = 0;
    let mut size: libc::size_t = std::mem::size_of::<u64>();
    // SAFETY: `value` is writable for `size` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            c_name.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    match (rc, size) {
        (0, 8) => Some(value),
        (0, 4) => Some(value & 0xffff_ffff),
        _ => None,
    }
}

fn hardware_uuid() -> Option<String> {
    let mut id = [0u8; 16];
    let wait = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    // SAFETY: `id` is a 16-byte buffer as `gethostuuid` requires.
    if unsafe { libc::gethostuuid(id.as_mut_ptr(), &wait) } != 0 {
        return None;
    }
    let hex: Vec<String> = id.iter().map(|byte| format!("{byte:02X}")).collect();
    let hex = hex.concat();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn hardware_facts(unknown: &mut Map<String, Value>) -> Value {
    let chip = sysctl_text("machdep.cpu.brand_string");
    let cores = sysctl_u64("hw.ncpu");
    let memory = sysctl_u64("hw.memsize");
    let uuid = hardware_uuid();
    for (name, missing) in [
        ("hardware.chip", chip.is_none()),
        ("hardware.cpu_cores", cores.is_none()),
        ("hardware.memory_bytes", memory.is_none()),
        ("hardware.hardware_uuid", uuid.is_none()),
    ] {
        if missing {
            unknown.insert(name.to_string(), json!("the kernel did not answer"));
        }
    }
    json!({"chip": chip, "cpu_cores": cores, "memory_bytes": memory, "hardware_uuid": uuid})
}

/// Run the host probe and parse its one JSON answer.
fn run_probe(command: &[String], timeout: Duration) -> Result<Value, String> {
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| "empty host probe command".to_string())?;
    let mut process = Command::new(program);
    process.args(arguments);
    let output = crate::proc::run_capture(&mut process, None, timeout)
        .map_err(|error| format!("host probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "host probe exited with {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| format!("host probe answer: {error}"))
}

/// The version a tool reports, first line, or `null`.
fn tool_version(
    name: &str,
    env: &[(String, String)],
    path: Option<&String>,
    timeout: Duration,
) -> Value {
    let Some(executable) = super::inventory::which(name, path) else {
        return Value::Null;
    };
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .env_clear()
        .envs(env.iter().cloned());
    match crate::proc::run_capture(&mut command, None, timeout) {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| json!(line.chars().take(200).collect::<String>()))
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

fn installed(
    pattern_dir: &Path,
    filter: impl Fn(&str) -> Option<String>,
) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(pattern_dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            filter(&name).map(|version| (version, entry.path()))
        })
        .collect();
    out.sort();
    out
}

fn software_facts(login_env: &[(String, String)], timeout: Duration) -> Value {
    let env: Vec<(String, String)> = login_env
        .iter()
        .filter(|(name, _)| !super::session_env::CREDENTIAL_VARIABLES.contains(&name.as_str()))
        .cloned()
        .collect();
    let path = env
        .iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone());
    let version = |tool: &str| tool_version(tool, &env, path.as_ref(), timeout);
    let xcode: Vec<Value> = installed(Path::new("/Applications"), |name| {
        (name.starts_with("Xcode") && name.ends_with(".app")).then(|| name.to_string())
    })
    .into_iter()
    .map(|(_, path)| {
        json!({
            "version": super::inventory::bundle_version(&path, timeout),
            "path": path.to_string_lossy(),
        })
    })
    .collect();
    let unity: Vec<Value> = installed(Path::new("/Applications/Unity/Hub/Editor"), |name| {
        (!name.starts_with('.')).then(|| name.to_string())
    })
    .into_iter()
    .map(|(version, path)| json!({"version": version, "path": path.join("Unity.app").to_string_lossy()}))
    .collect();
    let unreal: Vec<Value> = installed(Path::new("/Users/Shared/Epic Games"), |name| {
        name.strip_prefix("UE_").map(str::to_string)
    })
    .into_iter()
    .map(|(version, path)| json!({"version": version, "path": path.to_string_lossy()}))
    .collect();
    json!({
        "node": version("node"),
        "cua_driver": version("cua-driver"),
        "claude_code": version("claude"),
        "codex": version("codex"),
        "tart": version("tart"),
        "xcode": xcode,
        "unity": unity,
        "unreal": unreal,
    })
}

/// The CI runner's presence, name and whether a job runs now (MM-3, HY-5).
pub fn ci_runner(ci_root: &Path, uid: u32) -> Value {
    let runner_file = ci_root.join(".runner");
    let present = runner_file.is_file();
    let name = std::fs::read(&runner_file)
        .ok()
        .and_then(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim_start_matches('\u{feff}');
            serde_json::from_str::<Value>(text).ok()
        })
        .and_then(|value| {
            value
                .get("agentName")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let busy = process::process_table(Some(uid))
        .iter()
        .any(|info| info.comm == "Runner.Worker" && !info.zombie);
    json!({"present": present, "busy": busy, "name": name})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(probe: Option<Vec<String>>) -> (tempfile::TempDir, DurableConfig) {
        let root = tempfile::tempdir().unwrap();
        let (user, uid, _) = super::super::config::account();
        let config = DurableConfig {
            exposed: true,
            instance: "test-mac".to_string(),
            home: root.path().to_path_buf(),
            user,
            uid,
            state_dir: root.path().join("state"),
            log_file: root.path().join("state/service.log"),
            config_dir: root.path().join("config"),
            lock_dir: root.path().join("lock"),
            workspace_root: root.path().join("machine-service/leases"),
            login_shell: PathBuf::from("/bin/zsh"),
            grace_hours: 6.0,
            stop_grace: Duration::from_secs(1),
            inventory_timeout: Duration::from_secs(10),
            fact_timeout: Duration::from_secs(10),
            probe,
            ci_root: root.path().join("actions-runner"),
        };
        (root, config)
    }

    /// MM-3 (TL-2): the report carries every section with schema version 1;
    /// facts it could not observe are null with a reason, and the agents
    /// section carries states only.
    #[test]
    fn mm3_report_shape_and_unknowns() {
        let (_root, config) = config(None);
        let capabilities = Capabilities::new();
        let agents = super::super::agents::inspect(&config.agents_dir(), config.uid);
        let before = capabilities.report(&config, &agents);
        assert_eq!(before["unknown"]["gpu"], NOT_YET);

        capabilities.refresh(&config, &[("PATH".to_string(), "/nonexistent".to_string())]);
        let report = capabilities.report(&config, &agents);
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["network"], "lan");
        for section in [
            "os",
            "hardware",
            "disk",
            "gpu",
            "display",
            "software",
            "permissions",
            "agents",
            "ci_runner",
            "observed_at",
            "unknown",
        ] {
            assert!(report.get(section).is_some(), "{section}");
        }
        assert!(report["hardware"]["memory_bytes"].as_u64().unwrap() > 0);
        assert!(report["hardware"]["chip"].is_string());
        assert!(report["disk"]["free_bytes"].as_u64().unwrap() > 0);
        assert_eq!(report["gpu"]["metal3"], Value::Null);
        assert_eq!(report["unknown"]["gpu"], "no host probe configured");
        assert_eq!(report["software"]["node"], Value::Null);
        assert_eq!(
            report["agents"]["claude_code"],
            json!({"key": "absent", "endpoint": "absent"})
        );
        assert_eq!(report["ci_runner"]["present"], false);
        assert!(capabilities.refreshed_at().is_some());
    }

    /// MM-3 (TL-2): the probe's answer fills the GPU and permission
    /// sections; a failing probe leaves them unknown with its reason.
    #[test]
    fn mm3_probe_answer_and_failure() {
        let script = r#"printf '{"gpu":{"name":"Fake GPU","metal_families":["apple9","metal3"],"metal3":true,"argument_buffers_tier":2},"permissions":{"screen_recording":"granted","accessibility":"denied","post_event":"granted"}}'"#;
        let (_root, config) = config(Some(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ]));
        let capabilities = Capabilities::new();
        capabilities.refresh(&config, &[]);
        let agents = super::super::agents::inspect(&config.agents_dir(), config.uid);
        let report = capabilities.report(&config, &agents);
        assert_eq!(report["gpu"]["metal3"], true);
        assert_eq!(report["gpu"]["argument_buffers_tier"], 2);
        assert_eq!(report["permissions"]["accessibility"], "denied");
        assert!(report["unknown"].get("gpu").is_none());

        let (_root, config) = config_with_failing_probe();
        let capabilities = Capabilities::new();
        capabilities.refresh(&config, &[]);
        let report = capabilities.report(&config, &agents);
        assert_eq!(report["gpu"]["name"], Value::Null);
        assert_eq!(report["unknown"]["gpu"], "host probe exited with 3");
    }

    fn config_with_failing_probe() -> (tempfile::TempDir, DurableConfig) {
        config(Some(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 3".to_string(),
        ]))
    }

    /// HY-5 (TL-1): the CI runner's name comes from `.runner`, which may
    /// carry a byte-order mark.
    #[test]
    fn hy5_ci_runner_presence_and_name() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".runner"),
            "\u{feff}{\"agentName\": \"mini-1\"}",
        )
        .unwrap();
        let report = ci_runner(root.path(), u32::MAX);
        assert_eq!(
            report,
            json!({"present": true, "busy": false, "name": "mini-1"})
        );
    }
}
