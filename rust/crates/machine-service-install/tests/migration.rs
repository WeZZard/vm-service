//! The installer carries an installed vm-service across the rename (MG-3).
//!
//! Every test runs the built `machine-service-install` against an isolated
//! `HOME`, with an old `com.wezzard.vm-service` agent written into it, a fake
//! Tart, and the `launchctl` stub. No test touches a real LaunchAgent.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::Harness;
use serde_json::json;

fn succeed(harness: &Harness, args: &[&str]) -> String {
    let output = harness.run(args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// AC-36: the installer rewrites an installed vm-service's `VM_SERVICE_*`
/// values to their `MACHINE_SERVICE_*` names, sets no `VM_SERVICE_*` variable,
/// and replaces the old agent with `com.wezzard.machine-service`.
#[test]
fn ac36_install_rewrites_the_old_agents_variables() {
    let harness = Harness::new();
    let legacy = harness.save_legacy(&[
        ("HOME", harness.home_str()),
        ("VM_SERVICE_HOST", "0.0.0.0"),
        ("VM_SERVICE_GUEST_LINK", "ssh-session"),
        ("PATH", "/usr/bin:/bin"),
    ]);

    succeed(&harness, &[]);

    let data = harness.saved();
    let environment = data["EnvironmentVariables"].as_object().unwrap();
    assert_eq!(environment["MACHINE_SERVICE_HOST"], json!("0.0.0.0"));
    assert_eq!(
        environment["MACHINE_SERVICE_GUEST_LINK"],
        json!("ssh-session")
    );
    assert!(
        environment
            .keys()
            .all(|key| !key.starts_with("VM_SERVICE_")),
        "{environment:?}"
    );
    assert_eq!(data["Label"], json!("com.wezzard.machine-service"));
    assert_eq!(
        data["StandardOutPath"],
        json!("/tmp/com.wezzard.machine-service.out")
    );
    assert_eq!(
        data["StandardErrorPath"],
        json!("/tmp/com.wezzard.machine-service.err")
    );

    // The old job was unloaded before its plist was removed.
    let log = fs::read_to_string(&harness.log).unwrap();
    assert!(
        log.contains(&format!("unload {}", legacy.display())),
        "{log}"
    );
    assert!(!legacy.exists());
    assert!(!harness
        .home
        .join("loaded-job-com.wezzard.vm-service")
        .exists());
    assert!(harness.home.join("loaded-job").exists());
}

/// The old default state folder moves to the new default, the old default
/// console configuration is copied to the new configuration folder, the
/// `vmctl` link into the old install folder is removed, and a `machctl` link
/// is installed (AC-35).
#[test]
fn ac35_install_moves_state_and_console_and_replaces_the_cli_link() {
    let harness = Harness::new();
    let legacy_state = harness.home.join(".local/state/vm-service");
    fs::create_dir_all(legacy_state.join("ssh")).unwrap();
    fs::write(legacy_state.join("state.json"), "{\"vms\": {}}\n").unwrap();
    fs::write(legacy_state.join("service.log"), "old log\n").unwrap();

    let legacy_console = harness.home.join(".config/vm-service/console.json");
    fs::create_dir_all(legacy_console.parent().unwrap()).unwrap();
    fs::copy(&harness.config, &legacy_console).unwrap();
    fs::set_permissions(&legacy_console, fs::Permissions::from_mode(0o600)).unwrap();
    let legacy_console_text = legacy_console.to_string_lossy().into_owned();

    let links = harness.home.join(".local/bin");
    fs::create_dir_all(&links).unwrap();
    std::os::unix::fs::symlink(
        harness.home.join(".local/libexec/vm-service/vmctl"),
        links.join("vmctl"),
    )
    .unwrap();

    harness.save_legacy(&[
        ("HOME", harness.home_str()),
        ("VM_SERVICE_CONSOLE_CONFIG", legacy_console_text.as_str()),
    ]);

    let stdout = succeed(&harness, &[]);

    let state = harness.home.join(".local/state/machine-service");
    assert_eq!(
        fs::read_to_string(state.join("service.log")).unwrap(),
        "old log\n"
    );
    assert!(!legacy_state.exists());
    assert!(
        stdout.contains(&format!("state: {}", state.display())),
        "{stdout}"
    );

    let console = harness.home.join(".config/machine-service/console.json");
    assert_eq!(
        fs::read(&console).unwrap(),
        fs::read(&legacy_console).unwrap()
    );
    assert_eq!(
        fs::metadata(&console).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let data = harness.saved();
    assert_eq!(
        data["EnvironmentVariables"]["MACHINE_SERVICE_CONSOLE_CONFIG"],
        json!(console.to_str().unwrap())
    );
    assert!(data["EnvironmentVariables"]
        .as_object()
        .unwrap()
        .get("MACHINE_SERVICE_STATE")
        .is_none());

    assert!(fs::symlink_metadata(links.join("vmctl")).is_err());
    assert_eq!(
        fs::read_link(links.join("machctl")).unwrap(),
        harness.bin.join("machctl")
    );
}

/// A `vmctl` that is not a link into the old install folder is the
/// operator's own and stays.
#[test]
fn unrelated_vmctl_link_is_kept() {
    let harness = Harness::new();
    let links = harness.home.join(".local/bin");
    fs::create_dir_all(&links).unwrap();
    std::os::unix::fs::symlink("/opt/elsewhere/vmctl", links.join("vmctl")).unwrap();

    succeed(&harness, &[]);

    assert_eq!(
        fs::read_link(links.join("vmctl")).unwrap(),
        std::path::PathBuf::from("/opt/elsewhere/vmctl")
    );
    assert!(fs::symlink_metadata(links.join("machctl")).is_ok());

    succeed(&harness, &["remove"]);
    assert!(fs::symlink_metadata(links.join("machctl")).is_err());
}

/// A lease in the old state folder refuses the migration, and the old agent
/// is left exactly as it was.
#[test]
fn lease_in_the_old_state_refuses_the_migration() {
    let harness = Harness::new();
    let legacy_state = harness.home.join(".local/state/vm-service");
    fs::create_dir_all(&legacy_state).unwrap();
    fs::write(
        legacy_state.join("state.json"),
        "{\"vms\": {\"pilot-x\": {\"state\": \"running\"}}}\n",
    )
    .unwrap();
    let legacy = harness.save_legacy(&[("HOME", harness.home_str())]);
    let before = fs::read(&legacy).unwrap();

    let error = harness.assert_refused(&[]);
    assert!(error.contains("leases exist"), "{error}");
    assert_eq!(fs::read(&legacy).unwrap(), before);
    assert!(!harness.plist.exists());
    assert!(!harness.log.exists());
}

/// `--check` reports the migration and changes nothing.
#[test]
fn check_reports_the_migration_without_changing_anything() {
    let harness = Harness::new();
    let legacy_state = harness.home.join(".local/state/vm-service");
    fs::create_dir_all(&legacy_state).unwrap();
    let legacy =
        harness.save_legacy(&[("HOME", harness.home_str()), ("VM_SERVICE_HOST", "0.0.0.0")]);

    let stdout = succeed(&harness, &["--check"]);

    assert!(
        stdout.contains("migrating com.wezzard.vm-service"),
        "{stdout}"
    );
    assert!(stdout.contains("Check passed"), "{stdout}");
    assert!(legacy.exists());
    assert!(legacy_state.exists());
    assert!(!harness.plist.exists());
    assert!(!harness.log.exists());
}

/// AC-36: a caller that still exports an old variable is refused with the new
/// name, instead of having the setting silently dropped.
#[test]
fn ac36_caller_with_an_old_variable_is_refused() {
    let mut harness = Harness::new();
    harness.set_env("VM_SERVICE_PORT", "6290");

    let error = harness.assert_refused(&[]);
    assert!(
        error.contains("VM_SERVICE_PORT is now MACHINE_SERVICE_PORT"),
        "{error}"
    );
    assert!(!harness.plist.exists());
}
