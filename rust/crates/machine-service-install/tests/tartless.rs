//! An installation on a Mac without Tart (`docs/arch/rename.md` MG-4 step 5;
//! AC-38, DC-65).
//!
//! A Mac without Tart creates no virtual machine, so its instance offers only
//! its durable machine: the installer does not require the virtual-machine
//! helpers there. With exposure off too it offers nothing, and installs so
//! (DC-93).
//!
//! Every test runs the built `machine-service-install` against an isolated
//! `HOME` and the `launchctl` stub, with no `tart` on `PATH` and no `TART`.

mod common;

use common::Harness;
use serde_json::json;

/// AC-38: with exposure on, only `machine-service` and `machctl` beside the
/// installer are enough; the LaunchAgent is written with exposure on and
/// loaded, and no Tart is run.
#[test]
fn ac38_installs_without_tart_and_without_vm_helpers() {
    let mut harness = Harness::new();
    harness.remove_tart();
    harness.remove_vm_helpers();
    harness.set_env("MACHINE_SERVICE_HOST", "0.0.0.0");
    let output = harness.run(&["install", "--expose-durable", "on"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}\n{stdout}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("No Tart found"), "{stdout}");
    let saved = harness.saved();
    let environment = &saved["EnvironmentVariables"];
    assert_eq!(environment["MACHINE_SERVICE_EXPOSE_DURABLE"], json!("on"));
    assert_eq!(environment["MACHINE_SERVICE_HOST"], json!("0.0.0.0"));
    assert!(environment.get("TART").is_none(), "{environment}");
    assert_eq!(
        saved["ProgramArguments"],
        json!([harness.bin.join("machine-service").to_str().unwrap()])
    );
    let calls = std::fs::read_to_string(&harness.log).unwrap();
    assert!(
        calls.lines().any(|line| line.starts_with("load ")),
        "{calls}"
    );
    assert!(!harness.tartlog.exists(), "no Tart was run");
    // The CLI link is installed as on any Mac.
    let link = harness.home.join(".local/bin/machctl");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        harness.bin.join("machctl")
    );
}

/// AC-38: a later installation without the flag keeps exposure on, so it is
/// still allowed without Tart.
#[test]
fn ac38_reinstall_keeps_exposure_without_tart() {
    let mut harness = Harness::new();
    harness.remove_tart();
    harness.remove_vm_helpers();
    let first = harness.run(&["install", "--expose-durable", "on"]);
    assert!(first.status.success());
    let again = harness.run(&["install"]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(
        harness.saved()["EnvironmentVariables"]["MACHINE_SERVICE_EXPOSE_DURABLE"],
        json!("on")
    );
}

/// DC-93: without Tart and with exposure off the instance offers nothing.
/// That is a configuration the owner may choose, such as a laptop that only
/// runs the CLI, so the installer installs it and says what it offers.
#[test]
fn dc93_installs_an_instance_that_offers_nothing() {
    let mut harness = Harness::new();
    harness.remove_tart();
    harness.remove_vm_helpers();
    for args in [
        &["install", "--check"][..],
        &["install"][..],
        &["install", "--expose-durable", "off"][..],
    ] {
        let output = harness.run(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("this instance offers nothing"), "{stdout}");
    }
    assert_eq!(
        harness.saved()["EnvironmentVariables"]["MACHINE_SERVICE_EXPOSE_DURABLE"],
        json!("off")
    );
}

/// MG-4: the `machctl` CLI is still required without Tart, and a Mac with
/// Tart still requires the virtual-machine helpers.
#[test]
fn mg4_core_helpers_always_and_vm_helpers_only_with_tart() {
    let mut harness = Harness::new();
    harness.remove_vm_helpers();
    let error = harness.assert_refused(&["install", "--expose-durable", "on"]);
    assert!(error.contains("missing service helper"), "{error}");
    assert!(error.contains("console-worker"), "{error}");

    harness.remove_tart();
    std::fs::remove_file(harness.bin.join("machctl")).unwrap();
    let error = harness.assert_refused(&["install", "--expose-durable", "on"]);
    assert!(error.contains("missing service helper"), "{error}");
    assert!(error.contains("machctl"), "{error}");
}
