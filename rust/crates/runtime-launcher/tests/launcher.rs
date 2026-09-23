//! Behavioural tests for the native guest launcher.
//!
//! The launcher only performs its real work as service root with a staged
//! `/var/tmp/recorder-rpc` tree and an isolated `vmruntime` account. Those
//! checks cannot run from a source test; the ported assertions here cover the
//! launch contract that is observable without root.

use std::process::Command;

fn launcher() -> Command {
    Command::new(env!("CARGO_BIN_EXE_runtime-launcher"))
}

fn is_root() -> bool {
    // SAFETY: `geteuid` takes no arguments and only reads process state.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn arguments_are_refused_before_any_privileged_work() {
    // The C program refuses any argument regardless of the caller's identity.
    let output = launcher().arg("--unexpected").output().unwrap();
    assert_eq!(output.status.code(), Some(73));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "runtime-launcher: requires service root launch, no arguments\n"
    );
}

#[test]
fn non_root_launch_is_refused() {
    if is_root() {
        eprintln!("skipping: the test host is root, so the launch contract cannot be refused");
        return;
    }
    let output = launcher().output().unwrap();
    assert_eq!(output.status.code(), Some(73));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "runtime-launcher: requires service root launch, no arguments\n"
    );
}

#[test]
#[ignore = "no Python test counterpart: the Python project implements this launcher in C \
            (bin/runtime-launcher.c). Live acceptance only, because it requires service root, an \
            isolated vmruntime account, and a staged /var/tmp/recorder-rpc tree"]
fn drops_credentials_and_execs_the_adapter() {
    // Live acceptance only: provisioning the account and the immutable tree is
    // not something a source test may do.
    let output = launcher().output().unwrap();
    assert_eq!(output.status.code(), Some(0));
}
