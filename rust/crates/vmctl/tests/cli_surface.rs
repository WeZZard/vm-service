//! Pin the parts of the command surface that `argparse` defines and `clap`
//! generates, so a framework default cannot quietly add a command.
//!
//! Python's `argparse` has no `help` subcommand: `vmctl help` is an invalid
//! choice and exits 2. `clap` adds one unless it is disabled, which would be
//! surface the original does not have.

use std::process::Command;

const VMCTL: &str = env!("CARGO_BIN_EXE_vmctl");

#[test]
fn help_is_not_a_subcommand() {
    let output = Command::new(VMCTL)
        .arg("help")
        .output()
        .expect("spawn vmctl");
    assert_eq!(
        output.status.code(),
        Some(2),
        "`vmctl help` must fail the way argparse does"
    );
}

#[test]
fn help_flag_still_works_and_advertises_no_help_command() {
    let output = Command::new(VMCTL)
        .arg("--help")
        .output()
        .expect("spawn vmctl");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("acquire"), "command list missing: {stdout}");
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("help ")),
        "clap's `help` subcommand leaked into the command list: {stdout}"
    );
}
