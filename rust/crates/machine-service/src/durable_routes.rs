//! The daemon's durable-machine routes (`docs/arch/machines.md`,
//! `docs/arch/durable-machine.md`).
//!
//! The routes sit beside the virtual-machine routes and answer only for the
//! instance's own durable machine. With exposure off every durable request
//! is answered with HTTP 404 before any lock or workspace is touched, and
//! `/machines` and `/health` show no durable machine (DC-39, AC-43).

use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use machine_service_core::durable::{DurableConfig, DurableMachine, Reply};
use machine_service_core::service::Service;

static MACHINE: OnceLock<Arc<DurableMachine>> = OnceLock::new();

/// Build the durable machine, reconcile it after a restart, and start its
/// GC loop (DM-6, DM-8). Call once at startup, before serving.
pub fn install(service: &Service) -> Result<(), String> {
    let probe = std::env::current_exe().ok().map(|exe| {
        vec![
            exe.to_string_lossy().to_string(),
            "--probe-host".to_string(),
        ]
    });
    let config = DurableConfig::from_environment(
        &service.config.state_dir,
        &service.config.log_file,
        probe,
    )?;
    let exposed = config.exposed;
    let machine = Arc::new(DurableMachine::new(config)?);
    machine.startup();
    if exposed {
        service.log(&format!(
            "durable machine {} exposed (lock folder {})",
            machine.name(),
            machine.config.lock_dir.display()
        ));
    }
    let gc = Arc::clone(&machine);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(
            machine_service_core::GC_INTERVAL_S,
        ));
        gc.gc_once();
    });
    MACHINE
        .set(machine)
        .map_err(|_| "durable machine installed twice".to_string())
}

fn machine() -> Option<&'static Arc<DurableMachine>> {
    MACHINE.get()
}

fn reply((status, body): (u16, Value)) -> Option<(u16, Value)> {
    Some((status, body))
}

fn answer(reply_value: Reply) -> Option<(u16, Value)> {
    reply((reply_value.status, reply_value.body))
}

/// Add `instance` and `kinds` to the `/health` answer (FD-1).
pub fn health(body: &mut Value, virtual_kind: bool) {
    let Some((instance, _, kinds)) = identity(virtual_kind) else {
        return;
    };
    body["instance"] = json!(instance);
    body["kinds"] = json!(kinds);
    // MI-1 / DC-87: virtual-only instances report hardware identity too.
    if let Some(machine) = machine() {
        let (name, former_names) = machine
            .identity
            .snapshot(machine_service_core::config::unix_now());
        body["instance"] = json!(name);
        body["machine_id"] = json!(machine.machine_id);
        body["former_names"] = former_names;
    }
}

/// The instance ID and the kinds it offers, as `/health` states them and the
/// Bonjour advertisement carries them (FD-1, DC-85).
pub fn identity(virtual_kind: bool) -> Option<(String, String, Vec<&'static str>)> {
    let machine = machine()?;
    let mut kinds = Vec::new();
    if machine.exposed() {
        kinds.push("durable");
    }
    if virtual_kind {
        kinds.push("virtual");
    }
    Some((
        machine.name().to_string(),
        machine.machine_id.clone().unwrap_or_default(),
        kinds,
    ))
}

/// Mark the virtual machines' records and list the durable machine in the
/// `/machines` answer (MM-1, MM-3).
pub fn extend_machines(body: &mut Value) {
    let Some(machine) = machine() else { return };
    let name = machine.name();
    if let Some(offers) = body.get_mut("offers").and_then(Value::as_array_mut) {
        for offer in offers.iter_mut() {
            if let Some(object) = offer.as_object_mut() {
                object.insert("instance".to_string(), json!(name));
                object.insert("machine_id".to_string(), json!(machine.machine_id));
            }
        }
    }
    let Some(machines) = body.get_mut("machines").and_then(Value::as_object_mut) else {
        return;
    };
    for record in machines.values_mut() {
        if let Some(object) = record.as_object_mut() {
            object.insert("kind".to_string(), json!("virtual"));
            object.insert("instance".to_string(), json!(name));
            object.insert("machine_id".to_string(), json!(machine.machine_id));
        }
    }
    if machine.exposed() {
        let mut record = machine.machine_record();
        record["name"] = json!(name);
        record["instance"] = json!(name);
        machines.insert(name, record);
    }
}

/// Run one durable GC pass, for `POST /gc`.
pub fn gc_once() {
    if let Some(machine) = machine() {
        machine.gc_once();
    }
}

/// Answer a `GET` for the durable machine, or `None` for any other path.
pub fn get(path: &str) -> Option<(u16, Value)> {
    let machine = machine()?;
    let rest = path.strip_prefix("/machines/")?;
    let mut segments = rest.split('/');
    let name = segments.next()?;
    if !machine.owns_name(name) {
        return None;
    }
    if !machine.exposed() {
        return answer(if name == machine.name() {
            machine.not_exposed()
        } else {
            machine.unknown_name(name)
        });
    }
    let tail: Vec<&str> = segments.collect();
    match tail.as_slice() {
        [] if name == machine.name() => reply((200, machine.machine_record())),
        [] => answer(machine.unknown_name(name)),
        ["leases", lease_id, "hygiene"] => answer(machine.hygiene(name, lease_id)),
        _ => reply((404, json!({"error": "not found"}))),
    }
}

/// Answer a `POST` for the durable machine, or `None` for any other path.
/// `loopback` says whether the client is on this Mac (DM-5).
pub fn post(path: &str, body: &Value, loopback: bool) -> Option<(u16, Value)> {
    let machine = machine()?;
    if path == "/acquire" {
        if body.get("machine").is_none_or(Value::is_null) {
            return None;
        }
        return answer(machine.acquire(body));
    }
    let rest = path.strip_prefix("/machines/")?;
    let mut segments = rest.split('/');
    let name = segments.next()?;
    if !machine.owns_name(name) {
        return None;
    }
    if !machine.exposed() {
        return answer(if name == machine.name() {
            machine.not_exposed()
        } else {
            machine.unknown_name(name)
        });
    }
    let action = segments.next().unwrap_or_default();
    if segments.next().is_some() && action != "console" {
        return reply((404, json!({"error": "not found"})));
    }
    match action {
        "exec" => answer(machine.exec(name, body)),
        "push" => answer(machine.transfer(name, body, true, loopback)),
        "pull" => answer(machine.transfer(name, body, false, loopback)),
        "heartbeat" => answer(machine.heartbeat(name, body)),
        "release" => answer(machine.release(name, body)),
        "refresh" => answer(machine.refresh(name, body)),
        // DM-9: the owner views a durable machine with Screen Sharing.
        "console" => reply((
            404,
            json!({"error": "a durable machine has no console operations"}),
        )),
        _ => reply((404, json!({"error": "not found"}))),
    }
}

/// For the byte-carrying file endpoints (FD-4): `None` when `name` is not
/// the durable machine; otherwise whether the transfer may proceed, checked
/// before any byte is received.
pub fn files_check(name: &str, body: &Value) -> Option<Result<(), (u16, Value)>> {
    let machine = machine()?;
    if !machine.owns_name(name) {
        return None;
    }
    if !machine.exposed() {
        let refused = if name == machine.name() {
            machine.not_exposed()
        } else {
            machine.unknown_name(name)
        };
        return Some(Err((refused.status, refused.body)));
    }
    Some(
        machine
            .check_lease(name, body)
            .map_err(|refused| (refused.status, refused.body)),
    )
}

/// Run a byte-carrying transfer's file-system half on the durable machine:
/// the instance itself holds the staged path, so the path-based transfer
/// applies (FD-4).
pub fn files_transfer(name: &str, body: &Value, push: bool) -> (u16, Value) {
    match machine() {
        Some(machine) => {
            let done = machine.transfer(name, body, push, true);
            (done.status, done.body)
        }
        None => (404, json!({"error": "not found"})),
    }
}

/// Re-read the same source used at startup (MI-2 / DC-88).
pub fn reread_name() -> Result<Option<(String, String)>, String> {
    let Some(machine) = machine() else {
        return Ok(None);
    };
    let next = machine_service_core::durable::config::read_instance_name()?;
    machine
        .identity
        .rename(&next, machine_service_core::config::unix_now())
        .map(|old| old.map(|old| (old, next)))
}
