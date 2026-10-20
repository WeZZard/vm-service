//! Durable exposure is the instance's local configuration, off unless the
//! operator turns it on, and kept across installations (DC-39, MM-1, AC-43).
//!
//! Every test runs the built `machine-service-install` against an isolated
//! `HOME`, a fake Tart and the `launchctl` stub.

mod common;

use common::Harness;
use serde_json::{json, Value};

fn exposure(harness: &Harness) -> Value {
    harness.saved()["EnvironmentVariables"]
        .get("MACHINE_SERVICE_EXPOSE_DURABLE")
        .cloned()
        .unwrap_or(Value::Null)
}

fn install(harness: &Harness, args: &[&str]) {
    let output = harness.run(args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// AC-43: an installation without `--expose-durable` leaves exposure off;
/// `--expose-durable on` turns it on, a later installation without the flag
/// keeps it, and `--expose-durable off` turns it off again.
#[test]
fn ac43_exposure_is_off_until_installed_on_and_then_kept() {
    let harness = Harness::new();
    install(&harness, &[]);
    assert_eq!(exposure(&harness), Value::Null);
    install(&harness, &["--expose-durable", "on"]);
    assert_eq!(exposure(&harness), json!("on"));
    install(&harness, &[]);
    assert_eq!(exposure(&harness), json!("on"));
    install(&harness, &["--expose-durable", "off"]);
    assert_eq!(exposure(&harness), json!("off"));
}

/// MM-1: a value that is neither on nor off is refused before anything is
/// written.
#[test]
fn mm1_invalid_exposure_is_refused() {
    let mut harness = Harness::new();
    harness.set_env("MACHINE_SERVICE_EXPOSE_DURABLE", "yes");
    let error = harness.assert_refused(&[]);
    assert!(
        error.contains("MACHINE_SERVICE_EXPOSE_DURABLE must be on or off"),
        "{error}"
    );
    assert!(!harness.plist.exists());
    let error = harness.assert_refused(&["--expose-durable", "maybe"]);
    assert!(error.contains("maybe"), "{error}");
}
