//! The coordinated switch to machine-service (MG-3): old names fail loudly.
//!
//! The real `machine-service` binary runs against the fake-Tart fixture in
//! `tests/common/mod.rs`.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{free_port, try_http, LegacyFixture, DAEMON};

/// AC-36: machine-service started with `VM_SERVICE_PORT` set refuses to
/// start, and names the variable and its replacement `MACHINE_SERVICE_PORT`.
#[test]
fn ac36_daemon_refuses_an_old_variable() {
    let fixture = LegacyFixture::new();
    let port = free_port();
    let mut command = Command::new(DAEMON);
    command
        .envs(fixture.env())
        .env("MACHINE_SERVICE_PORT", port.to_string())
        .env("VM_SERVICE_PORT", "6290")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn machine-service");

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("machine-service kept running with VM_SERVICE_PORT set");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().expect("output");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!status.success());
    assert!(
        stderr.contains("VM_SERVICE_PORT is now MACHINE_SERVICE_PORT"),
        "{stderr}"
    );
    assert!(try_http(port, "GET", "/health", None, None).is_err());
}
