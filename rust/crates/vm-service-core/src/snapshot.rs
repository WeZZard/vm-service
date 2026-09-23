//! Read-only snapshots for `GET /vms` and `GET /images`.

use serde_json::{json, Map, Value};

use crate::config::unix_now;
use crate::error::{OpError, OpResult};
use crate::ops::{GRACE_HOURS, MAX_MACOS_RUNNING};
use crate::service::Service;

/// Build the `GET /vms` body.
pub fn snapshot(service: &Service) -> OpResult<Value> {
    let data = service.state.read()?;
    let now = unix_now();
    let mut vms = Map::new();
    if let Some(records) = data.get("vms").and_then(Value::as_object) {
        for (vm, record) in records {
            let mut result = record.as_object().cloned().unwrap_or_default();
            if let Some(console) = record.get("console").and_then(Value::as_object) {
                let resolved = record
                    .as_object()
                    .and_then(|map| {
                        let lease_id = map.get("lease_id").and_then(Value::as_str);
                        service.consoles.resolve(map, lease_id).ok()
                    })
                    .unwrap_or_else(|| {
                        let mut revoked = console.clone();
                        revoked.insert("status".to_string(), Value::String("revoked".to_string()));
                        revoked.insert(
                            "reason".to_string(),
                            Value::String("controller-unavailable-after-restart".to_string()),
                        );
                        revoked.insert(
                            "authentication".to_string(),
                            Value::String("unverified".to_string()),
                        );
                        revoked.insert(
                            "viewer_connected".to_string(),
                            Value::String("unverified".to_string()),
                        );
                        revoked.insert(
                            "human_confirmation".to_string(),
                            Value::String("unverified".to_string()),
                        );
                        revoked.insert(
                            "pixels".to_string(),
                            Value::String("unverified".to_string()),
                        );
                        revoked
                    });
                result.insert("console".to_string(), Value::Object(resolved));
            }
            let ttl = record
                .get("ttl_expires_at")
                .and_then(Value::as_f64)
                .unwrap_or(now);
            result.insert(
                "ttl_hours_remaining".to_string(),
                number(((ttl - now) / 3600.0 * 100.0).round() / 100.0),
            );
            if let Some(grace) = record.get("grace_until").and_then(Value::as_f64) {
                result.insert(
                    "grace_hours_remaining".to_string(),
                    number(((grace - now) / 3600.0 * 100.0).round() / 100.0),
                );
            }
            result.insert(
                "actually_running".to_string(),
                Value::Bool(service.host.vm_running(vm)?),
            );
            vms.insert(vm.clone(), Value::Object(result));
        }
    }
    Ok(json!({
        "vms": vms,
        "limits": {"max_macos_running": MAX_MACOS_RUNNING, "grace_hours": GRACE_HOURS},
        "pilot_repo": service.config.pilot.to_string_lossy(),
        "port": service.config.port,
    }))
}

/// Build the `GET /images` body.
pub fn images_snapshot(service: &Service) -> OpResult<Value> {
    let (lines, _) = {
        let mut cache = service
            .lines
            .lock()
            .map_err(|_| OpError::new("lines poisoned"))?;
        service.host.discover_lines(&mut cache, false)?
    };
    let data = service.state.read()?;
    let records: Vec<&Value> = data
        .get("vms")
        .and_then(Value::as_object)
        .map(|vms| vms.values().collect())
        .unwrap_or_default();

    let running_macos = records
        .iter()
        .filter(|record| {
            let active = record
                .get("state")
                .and_then(Value::as_str)
                .map(|state| crate::lines::ACTIVE_STATES.contains(&state))
                .unwrap_or(false);
            active && record.get("image_kind").and_then(Value::as_str) == Some("macos")
        })
        .count();
    let mut capacity = json!({
        "macos_running": running_macos,
        "macos_limit": MAX_MACOS_RUNNING,
    });
    if let Some(guests) = service.host.host_macos_guests() {
        capacity["host_macos_guests"] = json!(guests);
        capacity["foreign_macos_guests"] = json!(guests.saturating_sub(running_macos));
    }

    let mut out = Map::new();
    let mut names: Vec<&String> = lines.keys().collect();
    names.sort();
    for name in names {
        let cfg = &lines[name];
        let kind = cfg.get("kind").and_then(Value::as_str).unwrap_or("macos");
        let base_vm = cfg
            .get("base_vm")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let exists = service.host.vm_exists(base_vm)?;
        let running_line = records
            .iter()
            .filter(|record| {
                let active = record
                    .get("state")
                    .and_then(Value::as_str)
                    .map(|state| crate::lines::ACTIVE_STATES.contains(&state))
                    .unwrap_or(false);
                active && record.get("image").and_then(Value::as_str) == Some(name.as_str())
            })
            .count();
        let mut purposes: Vec<String> = records
            .iter()
            .filter(|record| record.get("image").and_then(Value::as_str) == Some(name.as_str()))
            .filter(|record| {
                record
                    .get("state")
                    .and_then(Value::as_str)
                    .map(|state| crate::lines::ACTIVE_STATES.contains(&state))
                    .unwrap_or(false)
            })
            .filter_map(|record| record.get("purpose").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        purposes.sort();
        purposes.dedup();
        let limit = if kind == "macos" {
            json!(MAX_MACOS_RUNNING)
        } else {
            Value::Null
        };
        let acquirable = exists && (kind != "macos" || running_macos < MAX_MACOS_RUNNING);
        out.insert(
            name.clone(),
            json!({
                "kind": kind,
                "base_vm": base_vm,
                "base_available": exists,
                "clone_prefix": cfg.get("clone_prefix").cloned().unwrap_or(Value::Null),
                "defaults": cfg.get("defaults").cloned().unwrap_or(Value::Null),
                "source": cfg.get("source").cloned().unwrap_or(Value::Null),
                "concurrency": {"running": running_line, "limit": limit, "acquirable": acquirable},
                "active_purposes": purposes,
            }),
        );
    }
    Ok(json!({"images": out, "capacity": capacity}))
}

fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}
