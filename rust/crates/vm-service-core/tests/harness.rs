//! Smoke tests for the shared harness itself.
//!
//! These pin the in-memory `Host` contract that the ported suites rely on, so a
//! harness regression fails here instead of confusing an unrelated test.

mod common;

#[test]
fn acquire_returns_ready_record() {
    let fixture = common::Fixture::new();
    let record = fixture.acquire("task-a", "ubuntu2404", "none");
    assert_eq!(record["state"], "running");
    assert_eq!(record["image"], "ubuntu2404");
    assert_eq!(record["image_kind"], "linux");
    assert!(record["vm"]
        .as_str()
        .expect("vm name")
        .starts_with("pilot-task-a-"));
}

#[test]
fn fake_tart_clones_sets_and_marks_running() {
    let fixture = common::Fixture::new();
    let record = fixture.acquire("task-a", "ubuntu2404", "none");
    let vm = record["vm"].as_str().expect("vm name").to_string();
    let ops: Vec<String> = fixture
        .host
        .tart
        .ops()
        .into_iter()
        .filter(|op| op != "list")
        .collect();
    assert_eq!(ops, vec!["clone", "set", "run"]);
    assert_eq!(
        fixture.host.tart.vms.lock().expect("vms").get(&vm),
        Some(&true)
    );
    // The golden base must never be left running (US8).
    assert_eq!(
        fixture
            .host
            .tart
            .vms
            .lock()
            .expect("vms")
            .get("pilot-ubuntu-base"),
        None
    );
}

#[test]
fn release_removes_record_and_deletes_vm() {
    let fixture = common::Fixture::new();
    let record = fixture.acquire("t", "ubuntu2404", "none");
    let vm = record["vm"].as_str().expect("vm name").to_string();
    let out = fixture.release(&vm);
    assert_eq!(out["released"], true);
    assert!(!fixture.read_state().contains_key(&vm));
    assert!(fixture
        .host
        .tart
        .args_for("delete")
        .iter()
        .any(|args| args.first() == Some(&vm)));
}

#[test]
fn state_round_trips_through_the_helper() {
    let fixture = common::Fixture::new();
    fixture.write_state(serde_json::json!({
        "pilot-old-abc": {
            "vm": "pilot-old-abc",
            "purpose": "old",
            "line": "ubuntu2404",
            "line_kind": "linux",
            "pack": "default",
            "state": "running",
            "ttl_expires_at": 4102444800.0,
        }
    }));
    // Loading normalizes the legacy keys in memory, not on disk.
    let loaded = fixture.service.state.read().expect("state");
    let record = &loaded["vms"]["pilot-old-abc"];
    assert_eq!(record["image"], "ubuntu2404");
    assert_eq!(record["env"], "default");
    assert_eq!(record["image_kind"], "linux");
    assert!(record.get("line").is_none());
    assert!(record.get("pack").is_none());
}
