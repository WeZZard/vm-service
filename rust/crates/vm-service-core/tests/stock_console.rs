//! Ported from `tests/unit/test_stock_console.py`.
//!
//! Stock-runtime acquisition and managed-console integration with no VMs and no
//! viewers. The Python suite replaced `svc.CONSOLES` with a real
//! `console_sessions.Manager` and patched only `prepare`; this port injects an
//! in-memory [`FakeConsoles`] through the always-compiled
//! [`vm_service_core::console_api::ConsoleController`] seam, so the ordering
//! between lease state and the console controller is observable.
//!
//! The real `Manager`'s reserve/prepare/revoke/renew/resolve semantics are
//! covered by that crate's own tests (`crates/console/src/sessions.rs`); here
//! the controller is a recording double, exactly as the Python tests replaced
//! it. The `deadline` field is unix seconds rather than the real manager's
//! monotonic `Instant`, because these tests only need a deadline that ordering
//! against `unix_now` can expire.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use vm_service_core::config::unix_now;
use vm_service_core::console_api::{ConsoleController, ConsoleError};

// --------------------------------------------------------------- FakeConsoles

/// The in-memory console controller used by the ported tests.
///
/// It mirrors the slice of `console_sessions.Manager` that the Python
/// white-box tests reached directly: a `sessions` map keyed by VM, each with
/// `status`, `deadline` and `generation`, plus a scripted `prepare`.
struct FakeConsoles {
    enabled: bool,
    sessions: Mutex<HashMap<String, Map<String, Value>>>,
    prepare_failure: Mutex<Option<String>>,
    renew_count: Mutex<usize>,
}

impl FakeConsoles {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            sessions: Mutex::new(HashMap::new()),
            prepare_failure: Mutex::new(None),
            renew_count: Mutex::new(0),
        }
    }

    /// Make every subsequent `prepare` fail with the Python error text.
    fn fail_prepare(&self, message: &str) {
        *self.prepare_failure.lock().expect("prepare_failure") = Some(message.to_string());
    }

    fn session(&self, vm: &str) -> Option<Map<String, Value>> {
        self.sessions.lock().expect("sessions").get(vm).cloned()
    }

    fn session_status(&self, vm: &str) -> Option<String> {
        self.session(vm).and_then(|session| {
            session
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    }

    fn sessions_empty(&self) -> bool {
        self.sessions.lock().expect("sessions").is_empty()
    }

    /// Set a session deadline in unix seconds; used to simulate expiry.
    fn set_deadline(&self, vm: &str, deadline: f64) {
        let mut sessions = self.sessions.lock().expect("sessions");
        if let Some(session) = sessions.get_mut(vm) {
            session.insert("deadline".to_string(), json!(deadline));
        }
    }

    /// The session's `(deadline, generation)`, mirroring the Python tuple.
    #[cfg(debug_assertions)]
    fn deadline_generation(&self, vm: &str) -> Option<(f64, i64)> {
        let session = self.session(vm)?;
        Some((
            session.get("deadline").and_then(Value::as_f64)?,
            session.get("generation").and_then(Value::as_i64)?,
        ))
    }

    #[cfg(debug_assertions)]
    fn renew_count(&self) -> usize {
        *self.renew_count.lock().expect("renew_count")
    }

    fn report(
        &self,
        record: &Map<String, Value>,
        session: &Map<String, Value>,
    ) -> Map<String, Value> {
        let mut report = session.clone();
        report.insert(
            "vm".to_string(),
            record.get("vm").cloned().unwrap_or(Value::Null),
        );
        report.insert(
            "lease_id".to_string(),
            record.get("lease_id").cloned().unwrap_or(Value::Null),
        );
        report
    }
}

impl ConsoleController for FakeConsoles {
    fn capabilities(&self) -> Value {
        json!({"linux": {"available": self.enabled, "backend": "x11vnc-inetd"}})
    }

    fn require_available(&self, _kind: &str) -> Result<(), ConsoleError> {
        if self.enabled {
            Ok(())
        } else {
            Err(ConsoleError::ViewerUnavailable)
        }
    }

    fn reserve(&self, record: &Map<String, Value>) -> Result<(), ConsoleError> {
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ttl = record
            .get("ttl_expires_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let mut session = Map::new();
        session.insert(
            "lease_id".to_string(),
            record.get("lease_id").cloned().unwrap_or(Value::Null),
        );
        session.insert(
            "environment_fingerprint".to_string(),
            record
                .get("environment_fingerprint")
                .cloned()
                .unwrap_or(Value::Null),
        );
        session.insert(
            "console_id".to_string(),
            Value::String("console-1".to_string()),
        );
        session.insert("status".to_string(), Value::String("preparing".to_string()));
        session.insert("reason".to_string(), Value::Null);
        session.insert("generation".to_string(), json!(0));
        session.insert("deadline".to_string(), json!(ttl));
        session.insert("access_expires_at".to_string(), json!(ttl));
        self.sessions.lock().expect("sessions").insert(vm, session);
        Ok(())
    }

    fn prepare(
        &self,
        record: &Map<String, Value>,
        _key_dir: &Path,
        _timeout_s: u64,
    ) -> Result<Map<String, Value>, ConsoleError> {
        if let Some(message) = self
            .prepare_failure
            .lock()
            .expect("prepare_failure")
            .clone()
        {
            return Err(ConsoleError::Other(message));
        }
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut sessions = self.sessions.lock().expect("sessions");
        let session = sessions
            .get_mut(&vm)
            .ok_or(ConsoleError::UnavailableAfterRestart)?;
        session.insert("status".to_string(), Value::String("ready".to_string()));
        Ok(self.report(record, session))
    }

    fn revoke(&self, record: &Map<String, Value>, reason: &str) {
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(session) = self.sessions.lock().expect("sessions").get_mut(&vm) {
            session.insert("status".to_string(), Value::String("revoked".to_string()));
            session.insert("reason".to_string(), Value::String(reason.to_string()));
        }
    }

    fn renew(&self, record: &Map<String, Value>, _deadline: Option<Instant>) {
        *self.renew_count.lock().expect("renew_count") += 1;
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut sessions = self.sessions.lock().expect("sessions");
        let Some(session) = sessions.get_mut(&vm) else {
            return;
        };
        let revoked = session.get("status").and_then(Value::as_str) == Some("revoked");
        let expired = session
            .get("deadline")
            .and_then(Value::as_f64)
            .map(|deadline| deadline <= unix_now())
            .unwrap_or(false);
        if revoked || expired {
            session.insert("status".to_string(), Value::String("revoked".to_string()));
            session.insert("reason".to_string(), Value::String("expired".to_string()));
            return;
        }
        let generation = session
            .get("generation")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        session.insert("generation".to_string(), json!(generation + 1));
        let ttl = record
            .get("ttl_expires_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        session.insert("deadline".to_string(), json!(ttl));
        session.insert("access_expires_at".to_string(), json!(ttl));
    }

    fn forget(&self, vm: &str) {
        self.sessions.lock().expect("sessions").remove(vm);
    }

    fn resolve(
        &self,
        record: &Map<String, Value>,
        lease_id: Option<&str>,
    ) -> Result<Map<String, Value>, ConsoleError> {
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let sessions = self.sessions.lock().expect("sessions");
        let session = sessions
            .get(&vm)
            .ok_or(ConsoleError::UnavailableAfterRestart)?;
        if let Some(lease_id) = lease_id {
            if session.get("lease_id").and_then(Value::as_str) != Some(lease_id) {
                return Err(ConsoleError::LeaseIdentityMismatch);
            }
        }
        Ok(self.report(record, session))
    }

    fn open(
        &self,
        _record: &Map<String, Value>,
        _lease_id: &str,
        _console_id: &str,
        _attempt_id: &str,
    ) -> Result<Value, ConsoleError> {
        Ok(json!({"attempt": {"status": "launched"}}))
    }

    fn cancel(
        &self,
        _record: &Map<String, Value>,
        _lease_id: &str,
        _console_id: &str,
        _attempt_id: &str,
    ) -> Result<Value, ConsoleError> {
        Ok(json!({"attempt": {"status": "cancelled"}}))
    }

    fn shutdown(&self) {}
}

// -------------------------------------------------------------------- helpers

/// Build a fixture whose console controller is a fresh enabled [`FakeConsoles`].
fn console_fixture() -> (common::Fixture, Arc<FakeConsoles>) {
    let consoles = Arc::new(FakeConsoles::new(true));
    let fixture =
        common::Fixture::with_consoles(Arc::clone(&consoles) as Arc<dyn ConsoleController>);
    (fixture, consoles)
}

// ---------------------------------------------------------------- tests

/// `test_ordinary_boot_uses_stock_flags`: a normal acquire produces a 32-hex
/// lease id, an applied resource record, and no `console` key.
#[test]
fn test_ordinary_boot_uses_stock_flags() {
    let fixture = common::Fixture::new();
    let rec = fixture.acquire("normal", "macos26", "none");
    let lease_id = rec["lease_id"].as_str().expect("lease_id");
    assert_eq!(lease_id.len(), 32, "lease_id: {lease_id:?}");
    assert!(
        lease_id
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "lease_id: {lease_id:?}"
    );
    assert_eq!(
        rec["configuration"]["resources_applied"]["status"],
        json!("applied")
    );
    assert!(
        rec.get("console").is_none(),
        "console present on ordinary boot: {rec}"
    );
}

/// `test_release_revokes_before_waiting_for_guest_lock`: revoke is published
/// inside the state transaction, before release blocks on the per-VM lock.
#[test]
fn test_release_revokes_before_waiting_for_guest_lock() {
    let (fixture, consoles) = console_fixture();
    let rec = fixture
        .try_acquire_vnc("watch", "ubuntu2404", "none", &json!(24), true, true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();

    let lock = fixture.service.operation_lock(&vm);
    let guard = lock.lock();

    let service = Arc::clone(&fixture.service);
    let release_vm = vm.clone();
    let task = std::thread::spawn(move || service.release(&release_vm, "released", false));

    let mut revoked = false;
    for _ in 0..100 {
        if consoles.session_status(&vm).as_deref() == Some("revoked") {
            revoked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(revoked, "console was not revoked before the guest lock");
    assert!(
        !task.is_finished(),
        "release returned before the guest lock was released"
    );
    assert!(
        fixture.service.heartbeat(&vm, Some(&json!(1))).is_err(),
        "heartbeat must fail while the lease is releasing"
    );

    drop(guard);
    let outcome = task.join().expect("release thread");
    assert!(outcome.is_ok(), "release failed: {outcome:?}");
}

/// `test_prepare_failure_rolls_back`: a console prepare failure rolls the
/// acquire back to empty state and forgets the reserved session.
#[test]
fn test_prepare_failure_rolls_back() {
    let (fixture, consoles) = console_fixture();
    consoles.fail_prepare("guest not ready");
    let error = fixture
        .try_acquire_vnc("watch", "ubuntu2404", "none", &json!(24), true, true)
        .expect_err("prepare failure must fail the acquire");
    assert!(
        error.to_string().contains("guest not ready"),
        "unexpected error: {error}"
    );
    assert!(
        fixture.read_state().is_empty(),
        "state not rolled back: {:?}",
        fixture.read_state()
    );
    assert!(
        consoles.sessions_empty(),
        "console sessions were not forgotten after rollback"
    );
}

/// `test_vnc_lease_and_cancel_before_open`: cancelling the console before a
/// viewer opens must not disturb the lease, which stays `running`. The
/// no-viewer-replay half is pinned by
/// `console::sessions::tests::cancel_before_open_never_replays_a_worker`; this
/// daemon-level test asserts the lease record, which is only observable here.
#[test]
fn test_vnc_lease_and_cancel_before_open() {
    let (fixture, _consoles) = console_fixture();
    let rec = fixture
        .try_acquire_vnc("watch", "ubuntu2404", "none", &json!(24), true, true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let lease_id = rec["lease_id"].as_str().expect("lease id");
    let console_id = rec["console"]["console_id"].as_str().expect("console id");
    let response = fixture
        .service
        .consoles
        .cancel(
            rec.as_object().expect("lease object"),
            lease_id,
            console_id,
            "early",
        )
        .expect("cancel");
    assert_eq!(response["attempt"]["status"], json!("cancelled"));
    assert_eq!(
        fixture.service.get_record(&vm).expect("record")["state"],
        json!("running"),
        "console cancel changed the lease state"
    );
}

/// `test_expired_lease_cannot_be_revived`: a heartbeat on an expired console
/// session revokes it instead of extending it.
#[test]
fn test_expired_lease_cannot_be_revived() {
    let (fixture, consoles) = console_fixture();
    let rec = fixture
        .try_acquire_vnc("watch", "ubuntu2404", "none", &json!(24), true, true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    consoles.set_deadline(&vm, unix_now() - 1.0);

    fixture
        .service
        .heartbeat(&vm, Some(&json!(1)))
        .expect("heartbeat itself succeeds");
    assert_eq!(consoles.session_status(&vm).as_deref(), Some("revoked"));
}

/// `test_failed_heartbeat_store_does_not_publish_deadline`: when the store
/// fails, heartbeat neither persists the new ttl nor renews the session.
#[test]
#[cfg(debug_assertions)]
fn test_failed_heartbeat_store_does_not_publish_deadline() {
    let (fixture, consoles) = console_fixture();
    let rec = fixture
        .try_acquire_vnc("watch", "ubuntu2404", "none", &json!(0.1), true, true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();
    let original = consoles.deadline_generation(&vm).expect("session");
    let renews_before = consoles.renew_count();

    vm_service_core::state::set_store_failure(Some("disk failure"));
    let result = fixture.service.heartbeat(&vm, Some(&json!(2)));
    vm_service_core::state::set_store_failure(None);

    let error = result.expect_err("store failure must fail the heartbeat");
    assert!(error.to_string().contains("disk failure"), "{error}");
    assert_eq!(
        consoles.deadline_generation(&vm),
        Some(original),
        "session deadline/generation changed despite the failed store"
    );
    assert_eq!(
        consoles.renew_count(),
        renews_before,
        "renew ran despite the failed store"
    );
    let stored = fixture.service.get_record(&vm).expect("record");
    assert_eq!(stored["ttl_expires_at"], rec["ttl_expires_at"]);
}

/// `test_failed_release_store_revokes_fail_closed`: a failed store during
/// release still revokes the console and leaves the persisted lease untouched.
#[test]
#[cfg(debug_assertions)]
fn test_failed_release_store_revokes_fail_closed() {
    let (fixture, consoles) = console_fixture();
    let rec = fixture
        .try_acquire_vnc("watch", "ubuntu2404", "none", &json!(24), true, true)
        .expect("acquire");
    let vm = rec["vm"].as_str().expect("vm name").to_string();

    vm_service_core::state::set_store_failure(Some("disk failure"));
    let result = fixture.service.release(&vm, "released", false);
    vm_service_core::state::set_store_failure(None);

    let error = result.expect_err("store failure must fail the release");
    assert!(error.to_string().contains("disk failure"), "{error}");
    assert_eq!(
        consoles.session_status(&vm).as_deref(),
        Some("revoked"),
        "console was not revoked fail-closed"
    );
    let stored = fixture.service.get_record(&vm).expect("record");
    assert_eq!(stored["state"], json!("running"));
}
