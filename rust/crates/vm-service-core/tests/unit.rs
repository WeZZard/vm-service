//! Unit tests for the vm-service core, ported from `tests/unit/test_unit.py`.
//!
//! The Python suite reached the Tart/SSH boundary with `mock.patch.object` on
//! the daemon module. This port substitutes the in-memory
//! [`common::FakeHost`] behind the `vm_service_core::host::Host` trait. Every
//! assertion, JSON field name, and error substring in the Python source is
//! preserved.
//!
//! Run with:
//! `cd rust && CARGO_TARGET_DIR=$PWD/target-host cargo test -p vm-service-core --test unit`

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use vm_service_core::config::unix_now;
use vm_service_core::error::OpResult;
use vm_service_core::service::Service;

// ------------------------------------------------------------------ helpers

/// Acquire with the Python fixture's defaults.
///
/// Mirrors `ServiceFixture` plus `svc.acquire(...)`: base source, NAT, and the
/// caller-selected TTL and wait flag.
fn service_acquire(
    service: &Service,
    purpose: &str,
    image: &str,
    env: &str,
    ttl_hours: &Value,
    wait: bool,
) -> OpResult<Value> {
    service.acquire(
        purpose, image, env, ttl_hours, None, None, None, wait, "nat", None, "base", None, false,
    )
}

/// Overwrite fields on one persisted lease record, as Python's `STATE.update`.
fn set_fields(service: &Service, vm: &str, fields: &[(&str, Value)]) {
    service
        .state
        .update(
            |data| {
                let record = data
                    .get_mut("vms")
                    .and_then(Value::as_object_mut)
                    .and_then(|vms| vms.get_mut(vm))
                    .unwrap_or_else(|| panic!("unknown VM: {vm}"));
                for (key, value) in fields {
                    record[*key] = value.clone();
                }
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )
        .expect("state update");
}

/// Read one record from the persisted state file.
fn persisted(fixture: &common::Fixture, vm: &str) -> Value {
    fixture.read_state().get(vm).cloned().unwrap_or(Value::Null)
}

/// The standard injection config used by `TestInjectPack`.
fn inject_cfg(kind: &str) -> Value {
    json!({"kind": kind, "ssh_user": "admin", "ssh_pass": "admin"})
}

/// Build a fixture with a seeded pack and a lease key directory.
fn inject_environment(with_env: bool, with_git: bool) -> (common::Fixture, PathBuf, PathBuf) {
    let fixture = common::Fixture::new();
    let pack = fixture.seed_pack("default", with_env, with_git);
    let key_dir = fixture.key_dir("inject-test");
    (fixture, pack, key_dir)
}

/// The stdin of the first `ssh` call whose stdin contains `needle`.
fn script_stdin(fixture: &common::Fixture, needle: &str) -> String {
    fixture
        .host
        .ssh
        .ssh_calls
        .lock()
        .expect("ssh_calls")
        .iter()
        .filter_map(|(_, stdin, _)| stdin.clone())
        .find(|stdin| stdin.contains(needle))
        .unwrap_or_else(|| panic!("no ssh script containing {needle:?}"))
}

/// The VM names recorded for one `tart` operation.
fn tart_args(fixture: &common::Fixture, op: &str) -> Vec<Vec<String>> {
    fixture.host.tart.args_for(op)
}

// ---------------------------------------------------------------- acquire

/// US1/US4 happy paths + input validation (no Tart needed).
#[test]
fn test_acquire_returns_ready_record() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("task-a", "ubuntu2404", "none");
    assert_eq!(rec["state"], json!("running"));
    assert_eq!(rec["image"], json!("ubuntu2404"));
    assert!(rec["vm"]
        .as_str()
        .expect("vm name")
        .starts_with("pilot-task-a-"));
}

#[test]
fn test_purpose_regex_rejected() {
    let fixture = common::Fixture::new();
    let long = "x".repeat(65);
    for bad in ["", "UPPER", "has space", "-lead", long.as_str(), "dot.name"] {
        assert!(
            fixture
                .try_acquire(bad, "ubuntu2404", "none", &json!(24), true)
                .is_err(),
            "purpose {bad:?} should be rejected"
        );
    }
}

#[test]
fn test_ttl_bounds() {
    let fixture = common::Fixture::new();
    for bad in [json!(0.05), json!(721), json!(-1)] {
        assert!(
            fixture
                .try_acquire("t", "ubuntu2404", "none", &bad, true)
                .is_err(),
            "ttl {bad} should be rejected"
        );
    }
    let rec = fixture
        .try_acquire("t", "ubuntu2404", "none", &json!(0.1), true)
        .expect("0.1h is accepted");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let snap = fixture.snapshot();
    let remaining = snap["vms"][vm.as_str()]["ttl_hours_remaining"]
        .as_f64()
        .expect("ttl_hours_remaining");
    assert!(remaining <= 0.11, "remaining={remaining}");
}

#[test]
fn test_unknown_image() {
    let fixture = common::Fixture::new();
    let err = fixture
        .try_acquire("t", "nope", "none", &json!(24), true)
        .expect_err("unknown image must fail");
    assert!(err.to_string().contains("unknown image"), "{err}");
}

#[test]
fn test_unknown_pack_fails_fast_before_clone() {
    let fixture = common::Fixture::new();
    let err = fixture
        .try_acquire("t", "ubuntu2404", "missing-pack", &json!(24), true)
        .expect_err("unknown pack must fail");
    assert!(err.to_string().contains("not found"), "{err}");
    assert!(fixture.host.tart.ops().is_empty()); // nothing cloned
}

#[test]
fn test_env_none_records_no_pack() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    assert!(rec["env"].is_null());
}

// ------------------------------------------------------- acquire lifecycle

/// US1/US2/US5/N2: clone -> boot -> inject -> running; failure rolls back.
#[test]
fn test_full_lifecycle_record_and_tart_calls() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("task-a", "ubuntu2404", "none");
    // Python booted through `subprocess.Popen`, which recorded nothing on the
    // fake Tart. The Rust host seam records `run` through `spawn_run`.
    let ops: Vec<String> = fixture
        .host
        .tart
        .ops()
        .into_iter()
        .filter(|op| op != "list")
        .collect();
    assert_eq!(ops, vec!["clone", "set", "run"]);
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let vms = fixture.host.tart.vms.lock().expect("vms");
    assert_eq!(vms.get(&vm), Some(&true));
    // The golden base must never be left running (US8).
    assert!(!vms.get("pilot-ubuntu-base").copied().unwrap_or(false));
}

#[test]
fn test_failure_rolls_back_clone_and_state() {
    let fixture = common::Fixture::new();
    // The Python test injected a `clone` failure with a side effect that left
    // the incomplete clone in `FakeTart.vms`, then verified teardown deleted
    // it. `common::FakeTart` checks the injected failure before its clone body
    // runs, so the identical observable (clone left on disk, then deleted) is
    // produced by failing the following `set` after the clone exists.
    fixture.host.tart.inject("set", "disk full");
    let err = fixture
        .try_acquire("task-a", "ubuntu2404", "none", &json!(24), true)
        .expect_err("failure must roll back");
    assert_eq!(err.to_string(), "disk full");
    // The partial clone was attempted then deleted; lease and keys gone.
    let deleted: Vec<String> = tart_args(&fixture, "delete")
        .into_iter()
        .filter_map(|args| args.first().cloned())
        .collect();
    assert!(!deleted.is_empty(), "clone was not deleted");
    assert!(fixture.read_state().is_empty());
    assert!(fixture.host.tart.vms.lock().expect("vms").is_empty());
    let key_dir = fixture
        .service
        .config
        .state_dir
        .join("ssh")
        .join(&deleted[0]);
    assert!(!key_dir.exists(), "lease keys survived rollback");
}

#[test]
fn test_ssh_never_ready_rolls_back() {
    let fixture = common::Fixture::new();
    fixture.host.wait_ssh_ok.store(false, Ordering::SeqCst);
    let err = fixture
        .try_acquire("task-a", "ubuntu2404", "none", &json!(24), true)
        .expect_err("SSH readiness failure must roll back");
    assert!(err.to_string().contains("SSH never became ready"), "{err}");
    assert!(fixture.read_state().is_empty());
}

#[test]
fn test_macos_base_never_booted_even_for_macos_line() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("m", "macos26", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let vms = fixture.host.tart.vms.lock().expect("vms");
    assert_eq!(vms.get(&vm), Some(&true));
    assert!(!vms.get("pilot-macos26-base").copied().unwrap_or(false));
}

// ---------------------------------------------------------------- concurrency

/// US8/N3: the macOS limit and purpose exclusivity, including races.
#[test]
fn test_macos_limit_rejected() {
    let fixture = common::Fixture::new();
    fixture.acquire("m1", "macos26", "none");
    fixture.acquire("m2", "macos26", "none");
    let err = fixture
        .try_acquire("m3", "macos26", "none", &json!(24), true)
        .expect_err("third macOS lease must be rejected");
    assert!(err.to_string().contains("limit reached"), "{err}");
}

#[test]
fn test_linux_unlimited() {
    let fixture = common::Fixture::new();
    for i in 0..4 {
        fixture.acquire(&format!("u{i}"), "ubuntu2404", "none");
    }
}

#[test]
fn test_released_macos_slot_frees_capacity() {
    let fixture = common::Fixture::new();
    let r1 = fixture.acquire("m1", "macos26", "none");
    let vm = r1["vm"].as_str().expect("vm name").to_string();
    fixture.acquire("m2", "macos26", "none");
    fixture.release(&vm);
    let rec = fixture.acquire("m3", "macos26", "none");
    assert!(rec["vm"]
        .as_str()
        .expect("vm name")
        .starts_with("pilot-mac-m3-"));
}

#[test]
fn test_pending_counts_toward_limit() {
    // Two macOS running; a third acquire is rejected even though the first
    // two were only just reserved (ACTIVE_STATES counting, not running-only).
    let fixture = common::Fixture::new();
    fixture.acquire("m1", "macos26", "none");
    fixture.acquire("m2", "macos26", "none");
    assert!(fixture
        .try_acquire("m3", "macos26", "none", &json!(24), true)
        .is_err());
}

#[test]
fn test_parallel_acquires_never_oversubscribe_macos() {
    // N3: 6 threads race for 2 macOS slots -> exactly 2 win, 4 OpError.
    let fixture = common::Fixture::new();
    let mut handles = Vec::new();
    for i in 0..6 {
        let service = Arc::clone(&fixture.service);
        handles.push(std::thread::spawn(move || {
            let purpose = format!("race{i}");
            // wait=False still provisions and verifies keys, with bounded
            // bootstrap and a single command probe. The race under test is
            // locked phase 1.
            match service_acquire(&service, &purpose, "macos26", "none", &json!(24), false) {
                Ok(rec) => Ok(rec["vm"].as_str().expect("vm name").to_string()),
                Err(error) => Err(error.to_string()),
            }
        }));
    }
    let mut wins = Vec::new();
    let mut errors = Vec::new();
    for handle in handles {
        match handle.join().expect("worker thread") {
            Ok(vm) => wins.push(vm),
            Err(message) => errors.push(message),
        }
    }
    assert_eq!(wins.len(), 2, "wins={wins:?} errors={errors:?}");
    assert_eq!(errors.len(), 4);
    assert!(errors.iter().all(|e| e.contains("limit reached")));
}

#[test]
fn test_parallel_same_purpose_different_images_both_win() {
    let fixture = common::Fixture::new();
    let a = fixture.acquire("same", "macos26", "none");
    let b = fixture.acquire("same", "ubuntu2404", "none");
    assert_ne!(a["vm"], b["vm"]);
}

#[test]
fn test_parallel_same_purpose_same_image_one_wins() {
    let fixture = common::Fixture::new();
    let service = Arc::clone(&fixture.service);
    let first = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            service_acquire(&service, "dup", "ubuntu2404", "none", &json!(24), true)
        })
    };
    let second = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            service_acquire(&service, "dup", "ubuntu2404", "none", &json!(24), true)
        })
    };
    let outcomes = [
        first.join().expect("thread"),
        second.join().expect("thread"),
    ];
    let errors: Vec<String> = outcomes
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("already leased"), "{}", errors[0]);
    assert_eq!(fixture.read_state().len(), 1);
}

// ---------------------------------------------------------------- TTL / GC

/// US6/US9: warn -> grace -> reclaim; heartbeat resets.
#[test]
fn test_gc_warns_within_grace() {
    let fixture = common::Fixture::new();
    let rec = fixture
        .try_acquire("t", "ubuntu2404", "none", &json!(0.1), true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    set_fields(
        &fixture.service,
        &vm,
        &[("ttl_expires_at", json!(unix_now() - 1.0))],
    );
    fixture.service.gc_once().expect("gc");
    let r = persisted(&fixture, &vm);
    assert_eq!(r["warned"], json!(true));
    assert!(!r["grace_until"].is_null());
    assert!(fixture.read_state().contains_key(&vm)); // not reclaimed
}

#[test]
fn test_gc_reclaims_after_grace() {
    let fixture = common::Fixture::new();
    let rec = fixture
        .try_acquire("t", "ubuntu2404", "none", &json!(0.1), true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let now = unix_now();
    set_fields(
        &fixture.service,
        &vm,
        &[
            ("ttl_expires_at", json!(now - 100.0)),
            ("grace_until", json!(now - 1.0)),
        ],
    );
    fixture.service.gc_once().expect("gc");
    assert!(!fixture.read_state().contains_key(&vm));
    let deleted: Vec<String> = tart_args(&fixture, "delete")
        .into_iter()
        .filter_map(|args| args.first().cloned())
        .collect();
    assert!(deleted.contains(&vm));
}

#[test]
fn test_gc_retries_releasing_records() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let key_dir = fixture.service.key_directory(&rec).expect("key dir");
    set_fields(&fixture.service, &vm, &[("state", json!("releasing"))]);
    fixture.service.gc_once().expect("gc");
    assert!(!fixture.read_state().contains_key(&vm));
    assert!(!fixture.host.tart.vms.lock().expect("vms").contains_key(&vm));
    assert!(!key_dir.exists());
}

#[test]
fn test_heartbeat_resets_ttl_and_grace() {
    let fixture = common::Fixture::new();
    let rec = fixture
        .try_acquire("t", "ubuntu2404", "none", &json!(0.1), true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let now = unix_now();
    set_fields(
        &fixture.service,
        &vm,
        &[
            ("ttl_expires_at", json!(now - 100.0)),
            ("grace_until", json!(now - 1.0)),
            ("warned", json!(true)),
        ],
    );
    let out = fixture
        .service
        .heartbeat(&vm, Some(&json!(4)))
        .expect("heartbeat");
    let r = persisted(&fixture, &vm);
    assert_eq!(r["warned"], json!(false));
    assert!(r["grace_until"].is_null());
    assert!(r["ttl_expires_at"].as_f64().expect("ttl") > now);
    let remaining = out["ttl_hours_remaining"].as_f64().expect("remaining");
    assert!((remaining - 4.0).abs() <= 0.02, "remaining={remaining}");
}

#[test]
fn test_heartbeat_unknown_vm() {
    let fixture = common::Fixture::new();
    assert!(fixture.service.heartbeat("nope", None).is_err());
}

#[test]
fn test_heartbeat_ttl_bounds() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name");
    assert!(fixture.service.heartbeat(vm, Some(&json!(1000))).is_err());
}

// ---------------------------------------------------------------- injection

/// US3/US4: env pack delivery and failure detection.
#[test]
fn test_inject_writes_secrets_and_verifies() {
    let (fixture, pack, key_dir) = inject_environment(true, false);
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &pack, &key_dir)
        .expect("inject");
    assert!(ok);
    let script = script_stdin(&fixture, "secrets.zsh");
    assert!(script.contains("cat env.extra"), "{script}");
    assert!(script.contains("install -m 600"), "{script}");
}

#[test]
fn test_inject_macos_script_neutral() {
    // The script body is shell-neutral; shell choice (zsh vs bash) is decided
    // by cfg["kind"] inside inject_pack and is covered by E2E.
    let (fixture, pack, key_dir) = inject_environment(true, false);
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("macos"), &pack, &key_dir)
        .expect("inject");
    assert!(ok);
    let script = script_stdin(&fixture, "secrets.zsh");
    assert!(!script.is_empty());
}

#[test]
fn test_missing_env_extra_still_ok_when_pack_has_other_files() {
    let (fixture, pack, key_dir) = inject_environment(true, false);
    std::fs::remove_file(pack.join("env.extra")).expect("remove env.extra");
    std::fs::write(pack.join("git-identity"), "A <a@b.c>").expect("git-identity");
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &pack, &key_dir)
        .expect("inject");
    assert!(ok);
}

#[test]
fn test_scp_failure_detected() {
    // scp returns nonzero -> inject_pack must catch it (the silent-failure bug).
    let (fixture, pack, key_dir) = inject_environment(true, false);
    let hook: common::ScpHook = Arc::new(|_ip, _user, _key_dir, _src, _dst, _timeout| {
        Some((1, "permission denied".to_string()))
    });
    *fixture.host.scp_hook.lock().expect("scp_hook") = Some(hook);
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &pack, &key_dir)
        .expect("inject");
    assert!(!ok);
}

#[test]
fn test_script_failure_detected() {
    let (fixture, pack, key_dir) = inject_environment(true, false);
    fixture.host.ssh.map("bash -s", 1, "fail");
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &pack, &key_dir)
        .expect("inject");
    assert!(!ok);
}

#[test]
fn test_verification_failure_detected() {
    // env.extra present but the guest secrets check reports empty.
    let (fixture, pack, key_dir) = inject_environment(true, false);
    fixture.host.ssh.map("test -s", 1, "");
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &pack, &key_dir)
        .expect("inject");
    assert!(!ok);
}

#[test]
fn test_ssh_exception_detected() {
    let (fixture, pack, key_dir) = inject_environment(true, false);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let hook: common::SshHook = Arc::new(move |_ip, _user, _key_dir, _cmd, _stdin, _timeout| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n == 1 {
            // emulate _ssh's exception -> None contract on the script call
            None
        } else {
            Some((0, String::new()))
        }
    });
    *fixture.host.ssh_hook.lock().expect("ssh_hook") = Some(hook);
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &pack, &key_dir)
        .expect("inject");
    assert!(!ok);
}

#[test]
fn test_empty_pack_injects_nothing_and_succeeds() {
    let (fixture, _pack, key_dir) = inject_environment(true, false);
    let empty = fixture.dir.path().join("empty-pack");
    std::fs::create_dir_all(&empty).expect("empty pack");
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &empty, &key_dir)
        .expect("inject");
    assert!(ok);
}

// ---------------------------------------------------------------- git identity

#[test]
fn test_git_identity_applied_when_present() {
    let fixture = common::Fixture::new();
    let pack = fixture.seed_pack("default", false, true);
    let key_dir = fixture.key_dir("git-test");
    let ok = fixture
        .service
        .inject_pack("1.2.3.4", &inject_cfg("linux"), &pack, &key_dir)
        .expect("inject");
    assert!(ok);
    let script = script_stdin(&fixture, "git config");
    assert!(script.contains("git config --global user.name"), "{script}");
}

// ---------------------------------------------------------------- state migration

/// Old line/pack(/line_kind) records normalize to image/env(/image_kind).
#[test]
fn test_old_record_migrated_on_load() {
    let fixture = common::Fixture::new();
    fixture.write_state(json!({
        "pilot-old-abc": {
            "vm": "pilot-old-abc", "purpose": "old", "line": "ubuntu2404",
            "line_kind": "linux", "pack": "default", "state": "running",
            "ttl_expires_at": unix_now() + 3600.0
        }
    }));
    let loaded = fixture.service.state.read().expect("state");
    let r = &loaded["vms"]["pilot-old-abc"];
    assert_eq!(r["image"], json!("ubuntu2404"));
    assert_eq!(r["env"], json!("default"));
    assert_eq!(r["image_kind"], json!("linux"));
    assert!(r.get("line").is_none());
    assert!(r.get("pack").is_none());
}

#[test]
fn test_lane_alias_migrated() {
    let fixture = common::Fixture::new();
    fixture.write_state(json!({
        "pilot-old-abc": {
            "vm": "pilot-old-abc", "purpose": "old", "line": "ubuntu2404",
            "lane": "default", "state": "running",
            "ttl_expires_at": unix_now() + 3600.0
        }
    }));
    let loaded = fixture.service.state.read().expect("state");
    let r = &loaded["vms"]["pilot-old-abc"];
    assert_eq!(r["env"], json!("default"));
}

#[test]
fn test_new_records_untouched() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let r = persisted(&fixture, &vm);
    assert_eq!(r["image"], json!("ubuntu2404"));
    assert!(r["env"].is_null());
}

#[test]
fn test_migration_idempotent() {
    let fixture = common::Fixture::new();
    fixture.write_state(json!({
        "pilot-old-abc": {
            "vm": "pilot-old-abc", "purpose": "old", "line": "ubuntu2404",
            "line_kind": "linux", "pack": "default", "state": "running",
            "ttl_expires_at": unix_now() + 3600.0
        }
    }));
    let first = fixture.service.state.read().expect("state")["vms"]["pilot-old-abc"].clone();
    let second = fixture.service.state.read().expect("state")["vms"]["pilot-old-abc"].clone();
    assert_eq!(first, second);
}

// ---------------------------------------------------------------- record ops

#[test]
fn test_release_removes_record_and_deletes_vm() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let out = fixture
        .service
        .release(&vm, "test", false)
        .expect("release");
    assert_eq!(out["released"], json!(true));
    assert!(!fixture.read_state().contains_key(&vm));
    let deleted: Vec<String> = tart_args(&fixture, "delete")
        .into_iter()
        .filter_map(|args| args.first().cloned())
        .collect();
    assert!(deleted.contains(&vm));
}

#[test]
fn test_release_unknown_vm() {
    let fixture = common::Fixture::new();
    assert!(fixture.service.release("ghost", "released", false).is_err());
}

#[test]
fn test_guest_exec_requires_running() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    set_fields(&fixture.service, &vm, &[("state", json!("provisioning"))]);
    let err = fixture
        .service
        .guest_exec(&vm, &json!({"argv": ["true"]}))
        .expect_err("not running must fail");
    assert!(err.to_string().contains("not running"), "{err}");
}

#[test]
fn test_guest_exec_argv_roundtrip() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let out = fixture
        .service
        .guest_exec(&vm, &json!({"argv": ["echo", "hi"]}))
        .expect("guest_exec");
    assert_eq!(out["rc"], json!(0));
}

#[test]
fn test_guest_exec_needs_argv_or_script() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    assert!(fixture.service.guest_exec(&vm, &json!({})).is_err());
}

#[test]
fn test_push_missing_local_path() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("t", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let err = fixture
        .service
        .guest_push(
            &vm,
            &json!({"local_path": "/nope", "remote_path": "/tmp/x"}),
        )
        .expect_err("missing local path must fail");
    assert!(err.to_string().contains("no such local path"), "{err}");
}

/// `test_push_pull_roundtrip` from `tests/e2e/test_e2e.py` (US-file transfer).
///
/// The Python case drives a live daemon; the only guest interaction is `scp`,
/// which the `Host` fake exposes as a hook. This test installs a hook that
/// simulates the guest file store, so a host->guest push followed by a
/// guest->host pull returns the pushed bytes rather than the mutated host copy.
#[test]
fn test_push_pull_roundtrip() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("e2e-files", "ubuntu2404", "none");
    let vm = rec["vm"].as_str().expect("vm name").to_string();

    // Simulated guest filesystem: one pushed payload keyed by nothing else.
    let guest_file: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let guest_for_hook = Arc::clone(&guest_file);
    let hook: common::ScpHook = Arc::new(move |_ip, user, _key, src, dst, _timeout| {
        let mut stored = guest_for_hook.lock().expect("guest file");
        if src.starts_with(&format!("{user}@")) {
            // Pull: guest -> host.
            let contents = stored.as_ref().expect("guest payload present").clone();
            std::fs::write(dst, contents).expect("write pulled payload");
        } else {
            // Push: host -> guest.
            let contents = std::fs::read(src).expect("read pushed payload");
            *stored = Some(contents);
        }
        Some((0, String::new()))
    });
    *fixture.host.scp_hook.lock().expect("scp_hook") = Some(hook);

    let dir = tempfile::tempdir().expect("tempdir");
    let local = dir.path().join("payload.txt");
    let payload = format!("e2e-payload {}", unix_now());
    std::fs::write(&local, &payload).expect("write payload");
    let local_text = local.to_string_lossy().to_string();

    fixture
        .service
        .guest_push(
            &vm,
            &json!({"local_path": local_text, "remote_path": "/tmp/payload.txt"}),
        )
        .expect("push");
    // Mutate the host copy to prove pull fetches the guest's version later.
    std::fs::write(&local, "host-side-different").expect("mutate host copy");
    fixture
        .service
        .guest_pull(
            &vm,
            &json!({"remote_path": "/tmp/payload.txt", "local_path": local_text}),
        )
        .expect("pull");
    assert_eq!(
        std::fs::read_to_string(&local).expect("read local"),
        payload
    );
}

// ---------------------------------------------------------------- images

#[test]
fn test_snapshot_reports_capacity() {
    let fixture = common::Fixture::new();
    let snap = fixture.images_snapshot();
    let images = &snap["images"];
    assert_eq!(images["macos26"]["concurrency"]["limit"], json!(2));
    assert_eq!(images["macos26"]["base_available"], json!(true));
    assert_eq!(images["ubuntu2404"]["base_available"], json!(true));
}

#[test]
fn test_running_counted_per_image() {
    let fixture = common::Fixture::new();
    fixture.acquire("m1", "macos26", "none");
    let snap = fixture.images_snapshot();
    let images = &snap["images"];
    assert_eq!(images["macos26"]["concurrency"]["running"], json!(1));
    assert_eq!(images["ubuntu2404"]["concurrency"]["running"], json!(0));
}

// ------------------------------------------------------- US14: refused boot

/// US14: a hypervisor-refused boot fails fast with a useful diagnosis instead
/// of burning the 420s wait_ip timeout.
#[test]
fn test_refused_boot_fails_fast_with_diagnosis() {
    let fixture = common::Fixture::new();
    // Exercise the real polling loop with a visible window.
    fixture.host.set_boot_settle(1.0);
    // The fake boot process is immediately dead (rc=1) — the real boot_refused
    // runs.
    fixture.host.refuse_boot(1);
    let err = fixture
        .try_acquire("refused", "macos26", "none", &json!(24), true)
        .expect_err("refused boot must fail");
    let msg = err.to_string();
    assert!(msg.contains("refused to start"), "{msg}");
    assert!(msg.contains("Virtualization.framework"), "{msg}");
    assert!(msg.contains("host-wide"), "{msg}");
    // clean rollback (US-N2): no lease, no VM
    assert!(fixture.read_state().is_empty());
    assert!(!fixture
        .host
        .tart
        .vms
        .lock()
        .expect("vms")
        .keys()
        .any(|name| name.starts_with("pilot-mac-refused-")));
}

#[test]
fn test_alive_boot_process_is_no_false_failure() {
    // Default fake boot process is alive (poll() -> None). acquire proceeds to
    // wait_ip/wait_ssh stubs and succeeds.
    let fixture = common::Fixture::new();
    fixture.host.set_boot_settle(1.0);
    let rec = fixture.acquire("alive", "macos26", "none");
    assert_eq!(rec["state"], json!("running"));
}

#[test]
fn test_refused_boot_rolls_back_for_linux_too() {
    // The refusal check is image-agnostic: a Linux boot refused by the
    // hypervisor also fails fast (e.g. disk full / corrupt clone).
    let fixture = common::Fixture::new();
    fixture.host.set_boot_settle(1.0);
    fixture.host.refuse_boot(125);
    let err = fixture
        .try_acquire("refused-lx", "ubuntu2404", "none", &json!(24), true)
        .expect_err("refused boot must fail");
    assert!(err.to_string().contains("refused to start"), "{err}");
    assert!(fixture.read_state().is_empty());
}

#[test]
fn test_boot_refused_unit_contract() {
    // Direct contract test of boot_refused itself.
    let fixture = common::Fixture::new();
    let mut dead = common::boot_process(Some(1));
    let err = fixture
        .service
        .boot_refused(dead.as_mut(), "vm-x", Some(0.3))
        .expect_err("dead process must be a refusal");
    assert!(err.to_string().contains("vm-x refused to start"), "{err}");
    let mut alive = common::boot_process(None);
    fixture
        .service
        .boot_refused(alive.as_mut(), "vm-x", Some(0.3))
        .expect("alive process is OK");
}

// ------------------------------------------------------- US15: host gauge

/// US15: capacity distinguishes our leases from host-wide
/// Virtualization.framework guests; the gauge is advisory only.
#[test]
fn test_snapshot_carries_gauge_fields() {
    let fixture = common::Fixture::new();
    fixture.host.set_gauge(Some(2));
    let cap = fixture.images_snapshot()["capacity"].clone();
    assert_eq!(cap["macos_running"], json!(0));
    assert_eq!(cap["host_macos_guests"], json!(2));
    // both foreign: we have none
    assert_eq!(cap["foreign_macos_guests"], json!(2));
}

#[test]
fn test_our_leases_subtracted_from_host_total() {
    let fixture = common::Fixture::new();
    fixture.acquire("m1", "macos26", "none");
    fixture.acquire("m2", "macos26", "none");
    fixture.host.set_gauge(Some(2));
    let cap = fixture.images_snapshot()["capacity"].clone();
    assert_eq!(cap["macos_running"], json!(2));
    assert_eq!(cap["host_macos_guests"], json!(2));
    assert_eq!(cap["foreign_macos_guests"], json!(0));
}

#[test]
fn test_gauge_never_negative() {
    // host gauge lower than our own count (race between pgrep and state) must
    // not report negative foreign guests.
    let fixture = common::Fixture::new();
    fixture.acquire("m1", "macos26", "none");
    fixture.host.set_gauge(Some(0));
    let cap = fixture.images_snapshot()["capacity"].clone();
    assert_eq!(cap["foreign_macos_guests"], json!(0));
}

#[test]
fn test_gauge_omitted_when_unavailable() {
    let fixture = common::Fixture::new();
    fixture.host.set_gauge(None);
    let cap = fixture.images_snapshot()["capacity"].clone();
    assert!(cap.get("host_macos_guests").is_none());
    assert!(cap.get("foreign_macos_guests").is_none());
    // still present
    assert_eq!(cap["macos_running"], json!(0));
    assert!(cap["macos_limit"].is_number());
}

#[test]
fn test_limit_409_mentions_gauge() {
    let fixture = common::Fixture::new();
    fixture.acquire("m1", "macos26", "none");
    fixture.acquire("m2", "macos26", "none");
    fixture.host.set_gauge(Some(4));
    let err = fixture
        .try_acquire("m3", "macos26", "none", &json!(24), true)
        .expect_err("limit must reject");
    let msg = err.to_string();
    assert!(msg.contains("limit reached"), "{msg}");
    assert!(
        msg.contains("host-wide Virtualization.framework guests: 4"),
        "{msg}"
    );
    assert!(msg.contains("2 not ours"), "{msg}");
}

#[test]
fn test_limit_409_without_gauge() {
    let fixture = common::Fixture::new();
    fixture.acquire("m1", "macos26", "none");
    fixture.acquire("m2", "macos26", "none");
    fixture.host.set_gauge(None);
    let err = fixture
        .try_acquire("m3", "macos26", "none", &json!(24), true)
        .expect_err("limit must reject");
    assert!(err.to_string().contains("gauge unavailable"), "{err}");
}

#[test]
fn test_gauge_never_gates_acquire() {
    // Advisory only: even a full host gauge must not block acquires for images
    // with free vm-service capacity.
    let fixture = common::Fixture::new();
    fixture.host.set_gauge(Some(99));
    let rec = fixture.acquire("u1", "ubuntu2404", "none");
    assert_eq!(rec["state"], json!("running"));
    // 1 of 2 slots
    let rec = fixture.acquire("m1", "macos26", "none");
    assert_eq!(rec["state"], json!("running"));
}
