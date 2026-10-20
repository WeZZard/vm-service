//! Read-only snapshots for `GET /machines` and `GET /images`.

use serde_json::{json, Map, Value};

use crate::catalog::CatalogView;
use crate::config::unix_now;
use crate::error::{OpError, OpResult};
use crate::ops::{GRACE_HOURS, MAX_MACOS_RUNNING};
use crate::service::Service;

/// Build the `GET /machines` body. The lease set is under `machines`; the
/// state file keeps its own `vms` key.
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
        "machines": vms,
        "limits": {"max_macos_running": MAX_MACOS_RUNNING, "grace_hours": GRACE_HOURS},
        "pilot_repo": service.config.pilot.to_string_lossy(),
        "port": service.config.port,
    }))
}

/// Build the `GET /images` body from one catalog read, with its `catalog`
/// status (IC-5). Fails with the diagnostic while no snapshot exists.
pub fn images_snapshot(service: &Service, view: &CatalogView) -> OpResult<Value> {
    let catalog = view
        .snapshot()
        .ok_or_else(|| OpError::new(view.status.diagnostic()))?;
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
    let host_guests = service.host.host_macos_guests();
    if let Some(guests) = host_guests {
        capacity["host_macos_guests"] = json!(guests);
        capacity["foreign_macos_guests"] = json!(guests.saturating_sub(running_macos));
    }
    let active_records: Vec<&Value> = records
        .iter()
        .copied()
        .filter(|record| {
            record
                .get("state")
                .and_then(Value::as_str)
                .map(|state| crate::lines::ACTIVE_STATES.contains(&state))
                .unwrap_or(false)
        })
        .collect();
    // Slots another program's macOS guests hold count too (DC-18).
    let macos_taken = macos_slots_taken(active_records.iter().copied(), host_guests);
    capacity["macos_taken"] = json!(macos_taken);

    let mut out = Map::new();
    for (name, line) in &catalog.lines {
        let kind = line.kind.as_str();
        let base_vm = line.base_vm.as_str();
        // Whether the base exists is read live, so promoting a base needs no
        // rebuild (IC-1).
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
        let acquirable = exists && (kind != "macos" || macos_taken < MAX_MACOS_RUNNING);
        out.insert(
            name.clone(),
            json!({
                "kind": kind,
                "base_vm": base_vm,
                "base_available": exists,
                "clone_prefix": line.clone_prefix,
                "defaults": line.defaults.to_json(),
                "source": line.source,
                "concurrency": {"running": running_line, "limit": limit, "acquirable": acquirable},
                "active_purposes": purposes,
            }),
        );
    }
    Ok(json!({"images": out, "capacity": capacity, "catalog": view.status.to_json()}))
}

fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// macOS slots taken on the host, as an advisory figure for `acquirable`
/// (DC-18): this service's active macOS leases, or, when more, the host's
/// Virtualization.framework guests less this service's other active guests.
fn macos_slots_taken<'a>(
    active: impl IntoIterator<Item = &'a Value>,
    host_guests: Option<usize>,
) -> usize {
    let (mut macos, mut other) = (0usize, 0usize);
    for record in active {
        if record.get("image_kind").and_then(Value::as_str) == Some("macos") {
            macos += 1
        } else {
            other += 1
        }
    }
    host_guests.map_or(macos, |guests| macos.max(guests.saturating_sub(other)))
}
