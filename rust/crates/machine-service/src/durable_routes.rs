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
    let machine = Arc::new(DurableMachine::new(config));
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
    let Some(machine) = machine() else { return };
    body["instance"] = json!(machine.name());
    let mut kinds = Vec::new();
    if machine.exposed() {
        kinds.push("durable");
    }
    if virtual_kind {
        kinds.push("virtual");
    }
    body["kinds"] = json!(kinds);
}

/// Mark the virtual machines' records and list the durable machine in the
/// `/machines` answer (MM-1, MM-3).
pub fn extend_machines(body: &mut Value) {
    let Some(machine) = machine() else { return };
    let Some(machines) = body.get_mut("machines").and_then(Value::as_object_mut) else {
        return;
    };
    for record in machines.values_mut() {
        if let Some(object) = record.as_object_mut() {
            object.insert("kind".to_string(), json!("virtual"));
            object.insert("instance".to_string(), json!(machine.name()));
        }
    }
    if machine.exposed() {
        machines.insert(machine.name().to_string(), machine.machine_record());
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
    if name != machine.name() {
        return None;
    }
    if !machine.exposed() {
        return answer(machine.not_exposed());
    }
    let tail: Vec<&str> = segments.collect();
    match tail.as_slice() {
        [] => reply((200, machine.machine_record())),
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
    if name != machine.name() {
        return None;
    }
    if !machine.exposed() {
        return answer(machine.not_exposed());
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
        "refresh" => answer(machine.refresh(name)),
        // DM-9: the owner views a durable machine with Screen Sharing.
        "console" => reply((
            404,
            json!({"error": "a durable machine has no console operations"}),
        )),
        _ => reply((404, json!({"error": "not found"}))),
    }
}
