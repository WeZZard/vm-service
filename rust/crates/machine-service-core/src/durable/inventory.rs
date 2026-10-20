//! The inventory before and after a durable lease (`docs/arch/hygiene.md`,
//! HY-4).
//!
//! An inventory is a set of independent parts. Each part runs on its own
//! thread with its own timeout; a part that fails or times out is recorded as
//! missing with its reason and the others are kept. An inventory never
//! refuses or blocks a lease (HY-1).
//!
//! This first cut records the user's processes, the running regular
//! applications, the installed applications, the launchd jobs, the Homebrew
//! formulae and casks, the top-level entries of the home folder,
//! `~/Library/Application Support` and `~/Library/Preferences`, and the free
//! disk space. Global npm packages and the crontab come with the rest of
//! HY-4.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::process;

/// One inventory part: its name and the work that produces it.
pub type Part = (String, Box<dyn FnOnce() -> Result<Value, String> + Send>);

/// What the standard parts need to know.
#[derive(Debug, Clone)]
pub struct Context {
    /// The service's user ID.
    pub uid: u32,
    /// The account's home folder.
    pub home: PathBuf,
    /// The CI runner's folder (HY-5).
    pub ci_root: PathBuf,
    /// The workspace root, which changes with every lease and is left out.
    pub workspace_root: PathBuf,
    /// The session's environment, whose `PATH` finds `brew`.
    pub env: BTreeMap<String, String>,
    /// Each part's timeout.
    pub timeout: Duration,
}

/// Take an inventory from `parts`, each bounded by `timeout`.
pub fn take(parts: Vec<Part>, timeout: Duration) -> Value {
    let mut receivers = Vec::new();
    for (name, work) in parts {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receivers.push((name, receiver));
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut found = Map::new();
    let mut missing = Map::new();
    for (name, receiver) in receivers {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(left) {
            Ok(Ok(value)) => {
                found.insert(name, value);
            }
            Ok(Err(reason)) => {
                missing.insert(name, Value::String(reason));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                missing.insert(
                    name,
                    Value::String(format!("timed out after {} s", timeout.as_secs_f64())),
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                missing.insert(name, Value::String("the part failed".to_string()));
            }
        }
    }
    json!({
        "taken_at": crate::config::unix_now(),
        "parts": found,
        "missing": missing,
    })
}

/// The standard parts of this first cut (HY-4).
pub fn standard_parts(context: &Context, software: Option<Value>) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    let ctx = context.clone();
    parts.push((
        "processes".to_string(),
        Box::new(move || Ok(processes(&ctx))),
    ));
    let ctx = context.clone();
    parts.push((
        "applications".to_string(),
        Box::new(move || running_applications(ctx.timeout)),
    ));
    let ctx = context.clone();
    parts.push((
        "installed_applications".to_string(),
        Box::new(move || Ok(installed_applications(&ctx))),
    ));
    let ctx = context.clone();
    parts.push((
        "launchd_jobs".to_string(),
        Box::new(move || Ok(launchd_jobs(&ctx.home))),
    ));
    let ctx = context.clone();
    parts.push(("homebrew".to_string(), Box::new(move || homebrew(&ctx))));
    let ctx = context.clone();
    parts.push(("folders".to_string(), Box::new(move || Ok(folders(&ctx)))));
    parts.push((
        "disk".to_string(),
        Box::new(|| {
            super::capabilities::disk("/")
                .map(|(total, free)| json!({"total_bytes": total, "free_bytes": free}))
        }),
    ));
    if let Some(software) = software {
        parts.push(("software".to_string(), Box::new(move || Ok(software))));
    }
    parts
}

fn processes(context: &Context) -> Value {
    let table = process::process_table(Some(context.uid));
    let ci = process::ci_pids(&table, &context.ci_root, &process::process_arguments);
    Value::Array(
        table
            .iter()
            .filter(|info| !info.zombie)
            .map(|info| {
                let mut entry = json!({
                    "pid": info.pid,
                    "executable": info.executable(),
                    "started_at": info.started_at(),
                });
                if ci.contains(&info.pid) {
                    entry["origin"] = json!("ci");
                }
                entry
            })
            .collect(),
    )
}

/// The running regular applications, those with a Dock presence, from
/// `lsappinfo`.
fn running_applications(timeout: Duration) -> Result<Value, String> {
    let mut command = Command::new("/usr/bin/lsappinfo");
    command.arg("list");
    let output = crate::proc::run_capture(&mut command, None, timeout)
        .map_err(|error| format!("lsappinfo: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "lsappinfo exited with {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(Value::Array(parse_lsappinfo(&String::from_utf8_lossy(
        &output.stdout,
    ))))
}

/// Parse `lsappinfo list`, keeping the `Foreground` applications.
pub fn parse_lsappinfo(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut current: Option<Map<String, Value>> = None;
    let flush = |entry: Option<Map<String, Value>>, out: &mut Vec<Value>| {
        if let Some(entry) = entry {
            if entry.get("type").and_then(Value::as_str) == Some("Foreground") {
                let mut entry = entry;
                entry.remove("type");
                out.push(Value::Object(entry));
            }
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some((number, rest)) = trimmed.split_once(") \"") {
            if number.chars().all(|c| c.is_ascii_digit()) && !number.is_empty() {
                flush(current.take(), &mut out);
                let name = rest.split('"').next().unwrap_or_default();
                let mut entry = Map::new();
                entry.insert("name".to_string(), json!(name));
                current = Some(entry);
                continue;
            }
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(value) = quoted_after(trimmed, "bundleID=") {
            entry.insert("bundle_id".to_string(), json!(value));
        } else if let Some(value) = quoted_after(trimmed, "bundle path=") {
            entry.insert("path".to_string(), json!(value));
        } else if let Some(rest) = trimmed.strip_prefix("pid = ") {
            let pid = rest
                .split_whitespace()
                .next()
                .and_then(|pid| pid.parse::<i64>().ok());
            entry.insert("pid".to_string(), json!(pid));
            if let Some(kind) = rest
                .split("type=\"")
                .nth(1)
                .and_then(|tail| tail.split('"').next())
            {
                entry.insert("type".to_string(), json!(kind));
            }
        }
    }
    flush(current.take(), &mut out);
    out
}

fn quoted_after(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    let rest = rest.strip_prefix('"')?;
    Some(rest.split('"').next().unwrap_or_default().to_string())
}

/// The bundle version of an application, from its `Info.plist`.
pub fn bundle_version(app: &Path, timeout: Duration) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    for key in ["CFBundleShortVersionString", "CFBundleVersion"] {
        let mut command = Command::new("/usr/bin/plutil");
        command
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&plist);
        if let Ok(output) = crate::proc::run_capture(&mut command, None, timeout) {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if output.status.success() && !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn installed_applications(context: &Context) -> Value {
    let mut out = Map::new();
    for folder in [
        PathBuf::from("/Applications"),
        context.home.join("Applications"),
    ] {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let mut apps: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
            .collect();
        apps.sort();
        for app in apps {
            let version = bundle_version(&app, context.timeout);
            out.insert(app.to_string_lossy().to_string(), json!(version));
        }
    }
    Value::Object(out)
}

/// Top-level entries of `folder` with their modification times.
fn entries_with_mtime(folder: &Path, skip: &[&Path]) -> Map<String, Value> {
    let mut out = Map::new();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if skip.iter().any(|skipped| *skipped == path) {
            continue;
        }
        let mtime = std::fs::symlink_metadata(&path)
            .map(|metadata| metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9)
            .ok();
        out.insert(path.to_string_lossy().to_string(), json!(mtime));
    }
    out
}

fn launchd_jobs(home: &Path) -> Value {
    let mut out = Map::new();
    for folder in [
        home.join("Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchDaemons"),
    ] {
        out.extend(entries_with_mtime(&folder, &[]));
    }
    Value::Object(out)
}

fn folders(context: &Context) -> Value {
    // The CI runner's folder is never touched (HY-5), and the workspace
    // root changes with every lease by design.
    let workspace_top = context
        .workspace_root
        .ancestors()
        .find(|path| path.parent() == Some(context.home.as_path()))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| context.workspace_root.clone());
    let skip = [context.ci_root.as_path(), workspace_top.as_path()];
    let mut out = Map::new();
    for folder in [
        context.home.clone(),
        context.home.join("Library/Application Support"),
        context.home.join("Library/Preferences"),
    ] {
        out.extend(entries_with_mtime(&folder, &skip));
    }
    Value::Object(out)
}

/// Find an executable on a `PATH`.
pub fn which(name: &str, path: Option<&String>) -> Option<PathBuf> {
    path?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
        })
}

fn homebrew(context: &Context) -> Result<Value, String> {
    let brew = which("brew", context.env.get("PATH"))
        .ok_or_else(|| "brew not found on the session PATH".to_string())?;
    let list = |kind: &str| -> Result<Map<String, Value>, String> {
        let mut command = Command::new(&brew);
        command
            .args(["list", kind, "--versions"])
            .env_clear()
            .envs(&context.env)
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_ANALYTICS", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .current_dir(&context.home);
        let output = crate::proc::run_capture(&mut command, None, context.timeout)
            .map_err(|error| format!("brew list {kind}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "brew list {kind} exited with {}",
                output.status.code().unwrap_or(-1)
            ));
        }
        let mut out = Map::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut words = line.split_whitespace();
            if let Some(name) = words.next() {
                let versions: Vec<&str> = words.collect();
                out.insert(name.to_string(), json!(versions.join(" ")));
            }
        }
        Ok(out)
    };
    Ok(json!({"formulae": list("--formula")?, "casks": list("--cask")?}))
}

/// Compare two inventories into `changes.json` (HY-4). CI-runner processes
/// keep their `origin` so a client does not attribute them to the lease.
pub fn diff(before: &Value, after: &Value) -> Value {
    let part = |inventory: &Value, name: &str| inventory["parts"].get(name).cloned();
    let mut parts = Map::new();
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for inventory in [before, after] {
        if let Some(map) = inventory.get("parts").and_then(Value::as_object) {
            names.extend(map.keys().cloned());
        }
    }
    for name in names {
        let (Some(old), Some(new)) = (part(before, &name), part(after, &name)) else {
            continue;
        };
        let change = match name.as_str() {
            "processes" => list_diff(&old, &new, |entry| {
                format!(
                    "{}:{}",
                    entry["pid"],
                    entry["started_at"].as_f64().unwrap_or_default()
                )
            }),
            "applications" => list_diff(&old, &new, |entry| {
                entry
                    .get("path")
                    .or_else(|| entry.get("name"))
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            }),
            "homebrew" => json!({
                "formulae": map_diff(&old["formulae"], &new["formulae"]),
                "casks": map_diff(&old["casks"], &new["casks"]),
            }),
            "disk" => json!({
                "free_bytes_before": old["free_bytes"],
                "free_bytes_after": new["free_bytes"],
            }),
            _ => map_diff(&old, &new),
        };
        parts.insert(name, change);
    }
    let missing = |inventory: &Value| inventory.get("missing").cloned().unwrap_or(json!({}));
    json!({
        "parts": parts,
        "missing": {"before": missing(before), "after": missing(after)},
    })
}

fn list_diff(old: &Value, new: &Value, key: impl Fn(&Value) -> String) -> Value {
    let empty = Vec::new();
    let old = old.as_array().unwrap_or(&empty);
    let new = new.as_array().unwrap_or(&empty);
    let old_keys: HashSet<String> = old.iter().map(&key).collect();
    let new_keys: HashSet<String> = new.iter().map(&key).collect();
    json!({
        "added": new.iter().filter(|entry| !old_keys.contains(&key(entry))).cloned().collect::<Vec<_>>(),
        "removed": old.iter().filter(|entry| !new_keys.contains(&key(entry))).cloned().collect::<Vec<_>>(),
    })
}

fn map_diff(old: &Value, new: &Value) -> Value {
    let empty = Map::new();
    let old = old.as_object().unwrap_or(&empty);
    let new = new.as_object().unwrap_or(&empty);
    let mut added = Map::new();
    let mut removed = Map::new();
    let mut changed = Map::new();
    for (key, value) in new {
        match old.get(key) {
            None => {
                added.insert(key.clone(), value.clone());
            }
            Some(previous) if previous != value => {
                changed.insert(key.clone(), json!({"before": previous, "after": value}));
            }
            Some(_) => {}
        }
    }
    for (key, value) in old {
        if !new.contains_key(key) {
            removed.insert(key.clone(), value.clone());
        }
    }
    json!({"added": added, "removed": removed, "changed": changed})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-31 (TL-1): a part that times out is recorded as missing with its
    /// reason, and the other parts are kept.
    #[test]
    fn ac31_timed_out_part_is_missing_and_others_kept() {
        let parts: Vec<Part> = vec![
            ("fast".to_string(), Box::new(|| Ok(json!({"a": 1})))),
            (
                "slow".to_string(),
                Box::new(|| {
                    std::thread::sleep(Duration::from_secs(5));
                    Ok(json!({}))
                }),
            ),
            (
                "broken".to_string(),
                Box::new(|| Err("brew not found on the session PATH".to_string())),
            ),
        ];
        let begun = std::time::Instant::now();
        let inventory = take(parts, Duration::from_millis(300));
        assert!(begun.elapsed() < Duration::from_secs(3));
        assert_eq!(inventory["parts"]["fast"], json!({"a": 1}));
        assert!(inventory["missing"]["slow"]
            .as_str()
            .unwrap()
            .starts_with("timed out after"));
        assert_eq!(
            inventory["missing"]["broken"],
            "brew not found on the session PATH"
        );
    }

    /// AC-31 (TL-1): changes list an added formula, an added and a changed
    /// Application Support entry, and keep a CI process's origin.
    #[test]
    fn ac31_diff_reports_additions_and_changes() {
        let before = json!({"parts": {
            "homebrew": {"formulae": {"jq": "1.7"}, "casks": {}},
            "folders": {"/h/Library/Application Support/A": 1.0},
            "processes": [{"pid": 1, "started_at": 1.0, "executable": "/bin/a"}],
        }, "missing": {}});
        let after = json!({"parts": {
            "homebrew": {"formulae": {"jq": "1.7", "hello": "2.12"}, "casks": {}},
            "folders": {
                "/h/Library/Application Support/A": 2.0,
                "/h/Library/Application Support/B": 3.0,
            },
            "processes": [
                {"pid": 1, "started_at": 1.0, "executable": "/bin/a"},
                {"pid": 9, "started_at": 5.0, "executable": "/ci/Runner.Worker", "origin": "ci"},
            ],
        }, "missing": {"applications": "timed out after 60 s"}});
        let changes = diff(&before, &after);
        assert_eq!(
            changes["parts"]["homebrew"]["formulae"]["added"],
            json!({"hello": "2.12"})
        );
        assert_eq!(
            changes["parts"]["folders"]["added"],
            json!({"/h/Library/Application Support/B": 3.0})
        );
        assert_eq!(
            changes["parts"]["folders"]["changed"]["/h/Library/Application Support/A"],
            json!({"before": 1.0, "after": 2.0})
        );
        assert_eq!(
            changes["parts"]["processes"]["added"][0]["origin"],
            json!("ci")
        );
        assert_eq!(
            changes["missing"]["after"]["applications"],
            "timed out after 60 s"
        );
    }

    #[test]
    fn lsappinfo_keeps_foreground_applications() {
        let text = r#" 1) "loginwindow" ASN:0x0-0x1001:
    bundleID="com.apple.loginwindow"
    bundle path="/System/Library/CoreServices/loginwindow.app"
    pid = 450 token=[sess=1] type="UIElement" flavor=3
 2) "TextEdit" ASN:0x0-0x2002:
    bundleID="com.apple.TextEdit"
    bundle path="/System/Applications/TextEdit.app"
    executable path="/System/Applications/TextEdit.app/Contents/MacOS/TextEdit"
    pid = 777 type="Foreground" flavor=3 Version="1"
"#;
        assert_eq!(
            parse_lsappinfo(text),
            vec![json!({
                "name": "TextEdit",
                "bundle_id": "com.apple.TextEdit",
                "path": "/System/Applications/TextEdit.app",
                "pid": 777,
            })]
        );
    }
}
