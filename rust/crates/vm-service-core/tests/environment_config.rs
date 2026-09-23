//! Ported from `tests/unit/test_environment_config.py`.
//!
//! `test_mismatched_lease_refuses_all_state_operations` is the only case from
//! that file that lives at the `vm-service-core` state boundary: a lease bound
//! to a different environment fingerprint must be refused by every state
//! operation before Tart is ever invoked, and the refusal must not depend on
//! which entry point is called.

mod common;

use std::sync::Arc;

use serde_json::{json, Map, Value};

use vm_service_core::host::Host;
use vm_service_core::service::Service;
use vm_service_core::state::State;

/// `test_mismatched_lease_refuses_all_state_operations`.
#[test]
fn test_mismatched_lease_refuses_all_state_operations() {
    let fixture = common::Fixture::new();
    let mut config = fixture.service.config.clone();
    // `configure_environment(self.resolve())`: a selected fingerprint is the
    // only thing that makes a mismatched record fatal instead of legacy-tolerant.
    config.environment = Some(json!({"identity": {"fingerprint": "expected"}}));
    let service = Service::with_host(config, None, Arc::clone(&fixture.host) as Arc<dyn Host>);
    fixture.write_state(json!({
        "bad": {"environment_fingerprint": "wrong"}
    }));

    assert!(
        service.release("bad", "requested", false).is_err(),
        "release admitted a mismatched lease"
    );
    assert!(
        service.heartbeat("bad", None).is_err(),
        "heartbeat admitted a mismatched lease"
    );
    assert!(
        service
            .guest_exec("bad", &json!({"argv": ["true"]}))
            .is_err(),
        "guest_exec admitted a mismatched lease"
    );
    assert!(
        service.gc_once().is_err(),
        "gc_once admitted a mismatched lease"
    );
    assert!(
        fixture.host.tart.calls.lock().expect("calls").is_empty(),
        "tart was invoked despite the fingerprint mismatch"
    );
}

/// `test_legacy_service_rejects_selected_lease_binding`: the direct
/// `State::require_lease_environment` call rejects a record bound to a selected
/// environment when the daemon runs in legacy mode, but accepts an unbound
/// record.
#[test]
fn test_legacy_service_rejects_selected_lease_binding() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let state = State::new(
        dir.path().to_path_buf(),
        dir.path().join("state.json"),
        dir.path().join("state.lock"),
        None,
    );
    assert!(
        state
            .require_lease_environment(&json!({
                "environment_fingerprint": "selected-fingerprint"
            }))
            .is_err(),
        "legacy state admitted a record bound to a selected environment"
    );
    assert!(
        state.require_lease_environment(&json!({})).is_ok(),
        "legacy state rejected an unbound record"
    );
}

/// The state-load failure message carries no implementation detail.
///
/// Python raises `OpError('selected service state is unreadable or invalid')
/// from error`, so the text a client sees stops after `invalid` and the parse
/// error survives only in the exception chain. A legacy daemon keeps the
/// tolerant behaviour and treats unreadable state as empty.
#[test]
fn test_selected_state_failure_message_matches_python() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let state_file = dir.path().join("state.json");
    std::fs::write(&state_file, b"{ not json").expect("write state");
    let selected = State::new(
        dir.path().to_path_buf(),
        state_file.clone(),
        dir.path().join("state.lock"),
        Some("expected".to_string()),
    );
    let error = selected
        .read()
        .expect_err("a selected environment must refuse unreadable state");
    assert_eq!(
        error.0, "selected service state is unreadable or invalid",
        "the failure message must match Python's"
    );

    let legacy = State::new(
        dir.path().to_path_buf(),
        state_file,
        dir.path().join("state.lock"),
        None,
    );
    assert!(
        legacy.read().is_ok(),
        "legacy mode must keep discarding unreadable state"
    );
}

/// The state file escapes non-ASCII the way Python's `ensure_ascii` does.
///
/// Python writes the state with `json.dump(data, f, indent=2, sort_keys=True)`
/// and `ensure_ascii=True` by default, so every code point outside U+0020
/// through U+007E becomes a `\uXXXX` escape and an astral code point becomes a
/// surrogate pair. The expectations here are the bytes CPython 3.14.7 writes.
#[test]
fn test_state_file_escapes_non_ascii() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let state_file = dir.path().join("state.json");
    let state = State::new(
        dir.path().to_path_buf(),
        state_file.clone(),
        dir.path().join("state.lock"),
        None,
    );
    state
        .update(
            |data| {
                data.insert("purpose".to_string(), json!("café 🙂"));
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )
        .expect("store state");

    let raw = std::fs::read_to_string(&state_file).expect("read state");
    assert!(
        raw.contains(r#""purpose": "caf\u00e9 \ud83d\ude42""#),
        "unexpected escapes: {raw}"
    );
    assert!(!raw.contains('é'), "raw non-ASCII leaked: {raw}");
    assert!(!raw.contains('🙂'), "raw astral character leaked: {raw}");
}

/// A malformed state document is a load failure, not an empty state.
///
/// Python's `_load` calls `data.get('vms', {}).values()`, so a document whose
/// root is not an object, or whose `vms` member is not an object, raises
/// `AttributeError`. A selected environment surfaces that as the same
/// "unreadable or invalid" failure it uses for a parse error, instead of
/// silently discarding leases. A legacy daemon keeps discarding unreadable
/// state.
#[test]
fn test_state_rejects_malformed_document_shapes() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let cases = [
        "[]",
        "123",
        "\"text\"",
        "{\"vms\": 5}",
        "{\"vms\": null}",
        "{\"vms\": []}",
    ];
    for (index, text) in cases.into_iter().enumerate() {
        let state_file = dir.path().join(format!("state{index}.json"));
        let lock_file = dir.path().join(format!("state{index}.lock"));
        std::fs::write(&state_file, text).expect("write state");
        let selected = State::new(
            dir.path().to_path_buf(),
            state_file.clone(),
            lock_file.clone(),
            Some("expected".to_string()),
        );
        let error = selected
            .read()
            .expect_err("a selected environment must refuse a malformed state document");
        assert_eq!(
            error.0, "selected service state is unreadable or invalid",
            "unexpected message for {text}"
        );
        let legacy = State::new(dir.path().to_path_buf(), state_file, lock_file, None);
        assert!(
            legacy.read().is_ok(),
            "legacy mode must keep discarding a malformed document: {text}"
        );
    }
}

/// Parsed floats are normalized to CPython's `repr` on load and on store.
///
/// CPython's `json` turns `0.10` into `0.1` and `1e2` into `100.0` when it
/// loads a document, while `serde_json`'s `arbitrary_precision` keeps the
/// original text. The state file is read back and rewritten, so both the value
/// a client sees and the bytes on disk have to match Python.
#[test]
fn test_state_normalizes_float_literals_like_python() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let state_file = dir.path().join("state.json");
    std::fs::write(
        &state_file,
        r#"{"vms": {}, "ratio": 0.10, "exp": 1e2, "count": 1, "wide": 79228162514264337593543950336}"#,
    )
    .expect("write state");
    let state = State::new(
        dir.path().to_path_buf(),
        state_file.clone(),
        dir.path().join("state.lock"),
        None,
    );
    let data = state.read().expect("state loads");
    let render = |value: &Value| serde_json::to_string(value).expect("serialize");
    assert_eq!(render(&data["ratio"]), "0.1", "0.10 must load as 0.1");
    assert_eq!(render(&data["exp"]), "100.0", "1e2 must load as 100.0");
    assert_eq!(render(&data["count"]), "1", "an integer keeps its digits");
    assert_eq!(
        render(&data["wide"]),
        "79228162514264337593543950336",
        "a wide integer keeps its exact digits"
    );

    state
        .update(
            |data| {
                data.insert("touched".to_string(), json!(true));
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )
        .expect("store state");
    let raw = std::fs::read_to_string(&state_file).expect("read state");
    assert!(
        raw.contains(r#""ratio": 0.1,"#),
        "float not normalized: {raw}"
    );
    assert!(
        raw.contains(r#""exp": 100.0,"#),
        "exponent not normalized: {raw}"
    );
    assert!(
        raw.contains(r#""wide": 79228162514264337593543950336"#),
        "wide integer lost digits: {raw}"
    );
}
