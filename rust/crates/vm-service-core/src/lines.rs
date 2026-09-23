//! Image "line" discovery from `pilot-images/images/*/line.conf`.
//!
//! The configuration files are zsh assignments, so they are sourced with zsh
//! and the required keys are echoed back. This matches the Python
//! `_parse_line_conf` and `discover_lines`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::{log_to_file, Config};
use crate::error::{OpError, OpResult};
use crate::proc::run_capture;
use crate::ssh::shell_quote;

/// The keys read out of a `line.conf`.
pub const CONF_KEYS: [&str; 8] = [
    "LINE_KIND",
    "BASE_VM",
    "WORK_VM",
    "CLONE_PREFIX",
    "GUEST_USER",
    "GUEST_PASS",
    "CPU",
    "MEMORY_MB",
];

/// Cached discovery result, invalidated by the images directory mtime.
#[derive(Debug, Default)]
pub struct LineCache {
    pub mtime: f64,
    pub lines: Option<Map<String, Value>>,
    pub bases: Option<Map<String, Value>>,
}

/// Source one `line.conf` and return the requested keys, or `None` on failure.
pub fn parse_line_conf(conf_path: &Path, strict: bool) -> Option<Map<String, Value>> {
    let quoted = shell_quote(&conf_path.to_string_lossy());
    let prints: Vec<String> = CONF_KEYS
        .iter()
        .map(|key| format!("print -- \"{key}=${{{key}}}\""))
        .collect();
    let script = if strict {
        format!("source {quoted} || exit $?; {}", prints.join(" ; "))
    } else {
        format!("source {quoted} && {}", prints.join(" ; "))
    };
    let mut command = Command::new("/bin/zsh");
    command.args(["-c", &script]);
    let output = run_capture(&mut command, None, Duration::from_secs(15)).ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut values = Map::new();
    for line in crate::python::splitlines(&text) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if CONF_KEYS.contains(&key) {
            values.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
    if !values.contains_key("BASE_VM") || !values.contains_key("CLONE_PREFIX") {
        return None;
    }
    Some(values)
}

/// Discover image lines, caching on the images directory mtime.
///
/// Returns the per-image configuration map and the base-VM map.
pub fn discover_lines(
    config: &Config,
    cache: &mut LineCache,
    force: bool,
) -> OpResult<(Map<String, Value>, Map<String, Value>)> {
    let selected = config.environment.is_some();
    let lines_dir = config.pilot.join("images");
    let mtime = std::fs::metadata(&lines_dir)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    if !force && cache.lines.is_some() && (selected || cache.mtime == mtime) {
        return Ok((
            cache.lines.clone().unwrap_or_default(),
            cache.bases.clone().unwrap_or_default(),
        ));
    }

    let mut lines = Map::new();
    let mut bases = Map::new();
    let mut entries: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(read) = std::fs::read_dir(&lines_dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.join("line.conf").is_file() {
                entries.push(path);
            }
        }
    }
    entries.sort();

    for directory in entries {
        let name = directory
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let conf_path = directory.join("line.conf");
        let Some(conf) = parse_line_conf(&conf_path, selected) else {
            if selected {
                return Err(OpError::new(format!(
                    "invalid selected image configuration: {}",
                    conf_path.display()
                )));
            }
            log_to_file(
                &config.log_file,
                &format!(
                    "WARN: could not parse {}/line.conf; skipping line '{name}'",
                    directory.display()
                ),
            );
            continue;
        };
        let defaults = match resource_defaults(&conf) {
            Ok(defaults) => defaults,
            Err(()) => {
                if selected {
                    return Err(OpError::new("invalid selected image resource defaults"));
                }
                json!({"cpu": 6, "memory_mb": 16384, "disk_gb": Value::Null})
            }
        };
        let text = |key: &str| conf.get(key).and_then(Value::as_str).map(str::to_string);
        let record = json!({
            "kind": text("LINE_KIND").unwrap_or_else(|| "macos".to_string()),
            "base_vm": conf.get("BASE_VM").cloned().unwrap_or(Value::Null),
            "work_vm": text("WORK_VM").filter(|value| !value.is_empty()),
            "clone_prefix": conf.get("CLONE_PREFIX").cloned().unwrap_or(Value::Null),
            "ssh_user": text("GUEST_USER").unwrap_or_default(),
            "ssh_pass": text("GUEST_PASS").unwrap_or_default(),
            "defaults": defaults,
            "source": conf_path.to_string_lossy(),
        });
        if let Some(base) = conf.get("BASE_VM") {
            bases.insert(name.clone(), base.clone());
        }
        lines.insert(name, record);
    }

    if lines.is_empty() {
        if selected {
            return Err(OpError::new(
                "selected image repository has no usable image configurations",
            ));
        }
        lines = hardcoded_lines();
        bases = hardcoded_bases();
        log_to_file(
            &config.log_file,
            "WARN: no lines discovered from pilot-images; using hardcoded fallback",
        );
    }
    cache.mtime = mtime;
    cache.lines = Some(lines.clone());
    cache.bases = Some(bases.clone());
    Ok((lines, bases))
}

/// Resolve the per-image resource defaults, mirroring the Python parsing.
fn resource_defaults(conf: &Map<String, Value>) -> Result<Value, ()> {
    let number = |key: &str, fallback: i64| -> Result<i64, ()> {
        match conf.get(key).and_then(Value::as_str) {
            None => Ok(fallback),
            Some("") => Ok(fallback),
            Some(text) => text.parse::<i64>().map_err(|_| ()),
        }
    };
    let cpu = number("CPU", 6)?;
    let memory_mb = number("MEMORY_MB", 16384)?;
    // An absent or zero disk value means inherit, represented as null.
    let disk_gb = match conf.get("DISK_GB").and_then(Value::as_str) {
        None => None,
        Some("") => None,
        Some(text) => {
            let parsed = text.parse::<i64>().map_err(|_| ())?;
            if parsed == 0 {
                None
            } else {
                Some(parsed)
            }
        }
    };
    Ok(json!({"cpu": cpu, "memory_mb": memory_mb, "disk_gb": disk_gb}))
}

fn hardcoded_lines() -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
        "macos26".to_string(),
        json!({
            "kind": "macos", "base_vm": "pilot-macos26-base", "work_vm": Value::Null,
            "clone_prefix": "pilot-mac-", "ssh_user": "station", "ssh_pass": "station",
            "defaults": {"cpu": 6, "memory_mb": 16384, "disk_gb": 200},
            "source": "hardcoded",
        }),
    );
    map.insert(
        "ubuntu2404".to_string(),
        json!({
            "kind": "linux", "base_vm": "pilot-ubuntu-base", "work_vm": Value::Null,
            "clone_prefix": "pilot-", "ssh_user": "admin", "ssh_pass": "admin",
            "defaults": {"cpu": 6, "memory_mb": 16384, "disk_gb": 80},
            "source": "hardcoded",
        }),
    );
    map
}

fn hardcoded_bases() -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
        "macos26".to_string(),
        Value::String("pilot-macos26-base".to_string()),
    );
    map.insert(
        "ubuntu2404".to_string(),
        Value::String("pilot-ubuntu-base".to_string()),
    );
    map
}

/// The active-state tuple used for lease slot accounting.
pub const ACTIVE_STATES: [&str; 4] = ["pending", "provisioning", "running", "releasing"];
