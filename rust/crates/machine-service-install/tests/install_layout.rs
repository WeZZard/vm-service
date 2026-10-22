//! The installation result (`docs/arch/installation.md` IN-1, IN-2; AC-46,
//! DC-79).
//!
//! The expected set is `service_layout::required`, the list the daemon
//! resolves its helpers from, never a list kept here: an executable the
//! daemon looks up and the installer leaves out fails these tests. Every
//! test runs the built `machine-service-install` against an isolated `HOME`
//! and the `launchctl` stub.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{write_exec, Harness};
use serde_json::json;
use service_layout::Executable;

fn files_in(directory: &Path) -> BTreeSet<String> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn prefix(harness: &Harness) -> PathBuf {
    harness.home.join(".local/libexec/machine-service")
}

fn install_into_prefix(harness: &Harness) -> std::process::Output {
    let prefix = prefix(harness);
    harness.run(&["install", "--prefix", prefix.to_str().unwrap()])
}

/// AC-46: an installation into an empty folder yields exactly the
/// executables the daemon resolves, plus the CLI and the installer: each one
/// byte-equal to its build and executable, and nothing else beside them.
#[test]
fn ac46_installs_exactly_the_layout_into_a_prefix() {
    let harness = Harness::new();
    // Something beside the installer that is not part of the installation.
    write_exec(&harness.bin.join("stray-tool"), "#!/bin/sh\nexit 0\n");
    let output = install_into_prefix(&harness);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let prefix = prefix(&harness);
    let expected: BTreeSet<String> = service_layout::required(true).into_iter().collect();
    assert_eq!(files_in(&prefix), expected);
    for name in &expected {
        let installed = prefix.join(name);
        assert_eq!(
            fs::read(&installed).unwrap(),
            fs::read(harness.bin.join(name)).unwrap(),
            "{name} is a copy of its build"
        );
        let mode = fs::metadata(&installed).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "{name}");
    }

    // The LaunchAgent runs the installed daemon, and the CLI link names the
    // installed machctl.
    assert_eq!(
        harness.saved()["ProgramArguments"],
        json!([prefix.join("machine-service").to_str().unwrap()])
    );
    assert_eq!(
        fs::read_link(harness.home.join(".local/bin/machctl")).unwrap(),
        prefix.join("machctl")
    );
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(
        calls.lines().any(|line| line.starts_with("load ")),
        "{calls}"
    );
}

/// AC-46: the session agent the guest link uploads is installed, for macOS
/// and Linux guests alike: the regression that left `machctl exec` and
/// `machctl push` failing with "guest session agent unavailable".
#[test]
fn ac46_installs_the_guest_session_agent_for_both_guest_kinds() {
    let harness = Harness::new();
    let output = install_into_prefix(&harness);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let directory = prefix(&harness);
    let agent = Executable::GuestSessionAgent;
    for kind in ["macos", "linux"] {
        let resolved = service_layout::guest_build_in(&directory, agent, kind);
        assert!(resolved.is_file(), "{kind}: {}", resolved.display());
    }
    assert_eq!(
        service_layout::guest_build_in(&directory, agent, "linux"),
        directory.join("guest-session-agent-linux")
    );
}

/// AC-46: any one missing executable refuses the installation, naming it,
/// before a file is copied, the LaunchAgent written or launchctl called.
#[test]
fn ac46_refuses_a_build_missing_any_executable() {
    for name in service_layout::required(true) {
        // The installer is the program running, so it is always present.
        if name == Executable::Installer.name() {
            continue;
        }
        let harness = Harness::new();
        fs::remove_file(harness.bin.join(&name)).unwrap();
        let stderr =
            harness.assert_refused(&["install", "--prefix", prefix(&harness).to_str().unwrap()]);
        let missing = harness.bin.join(&name);
        assert!(
            stderr.contains(&format!("{}", missing.display())),
            "{name}: {stderr}"
        );
        assert!(!prefix(&harness).exists(), "{name}: nothing is installed");
        assert!(!harness.plist.exists(), "{name}: no LaunchAgent is written");
        let calls = fs::read_to_string(&harness.log).unwrap_or_default();
        assert!(
            !calls.lines().any(|line| line.starts_with("load ")),
            "{name}: {calls}"
        );
    }
}

/// AC-46: on a Mac without Tart the installation is the core alone, even
/// when the build folder also holds the virtual-machine helpers.
#[test]
fn ac46_a_mac_without_tart_gets_the_core_only() {
    let mut harness = Harness::new();
    harness.remove_tart();
    let prefix = prefix(&harness);
    let output = harness.run(&[
        "install",
        "--expose-durable",
        "on",
        "--prefix",
        prefix.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: BTreeSet<String> = service_layout::required(false).into_iter().collect();
    assert_eq!(files_in(&prefix), expected);
}

/// AC-46: `--check` with a prefix validates the build and writes nothing.
#[test]
fn ac46_check_with_a_prefix_writes_nothing() {
    let harness = Harness::new();
    let output = harness.run(&[
        "install",
        "--check",
        "--prefix",
        prefix(&harness).to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!prefix(&harness).exists());
    assert!(!harness.plist.exists());
}

/// `--prefix` names where an installation goes, so it means nothing to
/// `remove`, which is refused rather than ignored.
#[test]
fn prefix_is_refused_outside_install() {
    let harness = Harness::new();
    let stderr = harness.assert_refused(&["remove", "--prefix", "/tmp/elsewhere"]);
    assert!(
        stderr.contains("--prefix applies to install only"),
        "{stderr}"
    );
}

/// IN-1: every helper path the daemon resolves names a file of the
/// installation, so the daemon cannot look up an executable the installer
/// does not install.
#[test]
fn in1_every_path_the_daemon_resolves_is_installed() {
    let overrides = [
        "VM_GUEST_SESSION_AGENT",
        "VM_GUEST_CONSOLE_AGENT",
        "VM_CONSOLE_WORKER",
    ];
    if overrides
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        eprintln!("skipped: an agent override is set in the test environment");
        return;
    }
    let installed: BTreeSet<String> = service_layout::required(true).into_iter().collect();
    let name = |path: PathBuf| path.file_name().unwrap().to_string_lossy().into_owned();
    let mut resolved = vec![
        name(machine_service_core::isolation::forwarder_path(None)),
        name(PathBuf::from(
            machine_service_core::isolation::installer_path(),
        )),
        name(console::guest::agent_path().unwrap()),
    ];
    for kind in ["macos", "linux"] {
        resolved.push(name(
            machine_service_core::guest_link::agent_path_for(kind).unwrap(),
        ));
        resolved.push(name(console::guest::agent_path_for(kind).unwrap()));
    }
    for path in resolved {
        assert!(installed.contains(&path), "{path} is not installed");
    }
}
