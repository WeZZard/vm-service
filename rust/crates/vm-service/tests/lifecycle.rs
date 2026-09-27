//! Lease lifecycle reproducers at the daemon process boundary.
//!
//! These tests start the real `vm-service` binary against the PATH shims in
//! `tests/common/mod.rs`, with an isolated temporary state directory and Tart
//! store. They cover the lifecycle failures that exist only in the daemon
//! entry point (startup, HTTP response delivery). See
//! `docs/lifecycle-fixes.md`.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::LegacyFixture;
use serde_json::{json, Value};

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs_f64()
}

/// A lease record as a previous daemon process would have persisted it.
fn record(vm: &str, purpose: &str, image: &str, kind: &str, state: &str) -> Value {
    let now = unix_now();
    json!({
        "vm": vm,
        "purpose": purpose,
        "image": image,
        "image_kind": kind,
        "lease_id": "0123456789abcdef0123456789abcdef",
        "env": null,
        "state": state,
        "created_at": now - 60.0,
        "ttl_expires_at": now + 24.0 * 3600.0,
        "grace_until": null,
        "warned": false,
        "ip": null,
        "cpu": null,
        "memory_mb": null,
        "disk_gb": null,
        "ssh_auth": "lease-key",
        "ssh_user": "admin",
        "ssh_verified": state == "running",
        "ssh_host_trust": "lease-tofu",
    })
}

/// V1: a daemon restart during acquisition leaves `pending`/`provisioning`
/// records, a clone, and a running `tart run`. They held capacity until
/// TTL + grace and blocked a retry with the same purpose. Startup must
/// release them through the normal teardown path before serving requests,
/// and must leave `running` leases alone.
#[test]
fn startup_reconciles_acquisitions_interrupted_by_a_restart() {
    let fixture = LegacyFixture::new();
    let pending = "pilot-interrupted-a1b2c3";
    let provisioning = "pilot-mac-interrupted-d4e5f6";
    let running = "pilot-kept-0a0b0c";
    let state = json!({
        "vms": {
            pending: record(pending, "interrupted", "ubuntu2404", "linux", "pending"),
            provisioning: record(provisioning, "interrupted", "macos26", "macos", "provisioning"),
            running: record(running, "kept", "ubuntu2404", "linux", "running"),
        }
    });
    std::fs::write(
        fixture.state_file(),
        serde_json::to_vec_pretty(&state).expect("state json"),
    )
    .expect("write state");
    // The pending clone was never created; the provisioning clone was booted
    // by the previous process and is still running.
    std::fs::write(
        fixture.tart_home.join("fake-vms"),
        format!("pilot-ubuntu-work=0\n{provisioning}=1\n{running}=1\n"),
    )
    .expect("fake-vms");

    let _daemon = fixture.start_daemon();

    let vms = fixture.read_state();
    assert!(
        !vms.contains_key(provisioning),
        "a provisioning record from the previous process survived startup: {vms:?}"
    );
    assert!(
        !vms.contains_key(pending),
        "a pending record from the previous process survived startup: {vms:?}"
    );
    assert!(vms.contains_key(running), "a running lease was reclaimed");
    let calls = fixture.tart_calls();
    assert!(
        calls.contains(&format!("stop {provisioning}")),
        "the orphaned clone was not stopped: {calls}"
    );
    assert!(
        calls.contains(&format!("delete {provisioning}")),
        "the orphaned clone was not deleted: {calls}"
    );
    let log = std::fs::read_to_string(fixture.state_dir.join("service.log")).unwrap_or_default();
    assert!(
        log.contains(&format!("startup: reconciling {provisioning}")),
        "reconciliation was not logged: {log}"
    );
}
