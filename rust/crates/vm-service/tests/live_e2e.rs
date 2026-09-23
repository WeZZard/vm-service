//! Live-only ports of the two e2e cases in `tests/e2e/test_e2e.py`.
//!
//! These exercise the real production path: a running daemon, a real Tart
//! golden image, a real guest, real networking, and the gateway credentials in
//! the `default` environment pack. They are `#[ignore]`d so the normal
//! workspace run never boots, clones, or leases a VM. Removing the attribute
//! runs them exactly as the Python suite does (including the Python
//! `require_daemon` skip when no daemon answers).
//!
//! The Python `E2ETestCase` base released every acquired VM in `tearDown`;
//! [`Lease`] reproduces that with `Drop`, so a failing assertion still releases.

mod common;

use std::time::Duration;

use serde_json::{json, Value};

/// Default live daemon port, matching the Python `VM_SERVICE_PORT` default.
const DEFAULT_PORT: u16 = 6240;
/// Default e2e image, matching the Python `E2E_IMAGE` default.
const E2E_IMAGE: &str = "ubuntu2404";
/// Default lease TTL, matching the Python `E2E_TTL_HOURS` default.
const E2E_TTL_HOURS: f64 = 1.0;
/// Python `E2E_ACQUIRE_TIMEOUT` default.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(900);
/// Python `test_pi_reaches_gateway` exec timeout.
const PI_EXEC_TIMEOUT: Duration = Duration::from_secs(600);
/// Python `test_gh_token_works` exec timeout.
const GH_EXEC_TIMEOUT: Duration = Duration::from_secs(120);
/// Python teardown release timeout.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(600);

fn live_port() -> u16 {
    std::env::var("VM_SERVICE_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Python `E2ETestCase.require_daemon`: skip when the daemon is unreachable.
fn require_daemon(port: u16) -> bool {
    match common::try_http(port, "GET", "/health", None, None) {
        Ok(response) if response.status == 200 => true,
        _ => {
            eprintln!("vm-service daemon not reachable at 127.0.0.1:{port}; skipping live E2E");
            false
        }
    }
}

/// A leased VM that is released on drop, mirroring the Python `tearDown`.
struct Lease {
    port: u16,
    vm: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = common::try_http_timeout(
            self.port,
            "POST",
            &format!("/vms/{}/release", self.vm),
            Some(&json!({"reason": "e2e-teardown"})),
            None,
            RELEASE_TIMEOUT,
        );
    }
}

/// Python `E2ETestCase.acquire`.
fn acquire(port: u16, purpose: &str, env: &str) -> (Value, Lease) {
    let response = common::http_timeout(
        port,
        "POST",
        "/acquire",
        Some(&json!({
            "purpose": purpose,
            "image": E2E_IMAGE,
            "env": env,
            "ttl_hours": E2E_TTL_HOURS,
        })),
        None,
        ACQUIRE_TIMEOUT,
    );
    assert_eq!(response.status, 200, "acquire failed: {:?}", response.body);
    let vm = response
        .body
        .get("vm")
        .and_then(Value::as_str)
        .expect("acquired VM name")
        .to_string();
    let lease = Lease {
        port,
        vm: vm.clone(),
    };
    (response.body, lease)
}

/// Python `api("POST", f"/vms/{rec['vm']}/exec", body, timeout=...)`.
fn exec(port: u16, vm: &str, body: &Value, timeout: Duration) -> common::Response {
    common::http_timeout(
        port,
        "POST",
        &format!("/vms/{vm}/exec"),
        Some(body),
        None,
        timeout,
    )
}

/// `test_pi_reaches_gateway`
///
/// US3: `pi` in the guest reaches the gateway with the `default` credential
/// pack and answers a trivial prompt.
#[test]
#[ignore = "test_pi_reaches_gateway: requires a live vm-service daemon on 127.0.0.1:6240, a real Tart hypervisor with a bootable ubuntu2404 golden image, guest network access, and the configured pi gateway credentials in the default pack"]
fn test_pi_reaches_gateway() {
    let port = live_port();
    if !require_daemon(port) {
        return;
    }
    let (record, _lease) = acquire(port, "e2e-pi", "default");
    let vm = record["vm"].as_str().expect("acquired VM name");
    let response = exec(
        port,
        vm,
        &json!({
            "script": "exec bash -lc \"pi --model glm-5.3-flash 'Reply with exactly: ok'\""
        }),
        PI_EXEC_TIMEOUT,
    );
    assert_eq!(response.status, 200, "pi exec failed: {:?}", response.body);
    assert_eq!(response.body["rc"], 0);
    assert!(response.body["output"]
        .as_str()
        .unwrap_or("")
        .to_lowercase()
        .contains("ok"));
}

/// `test_gh_token_works`
///
/// US3: the `default` pack's GitHub token authenticates `gh` in the guest; a
/// login shell is used because the pack env vars land via `.bashrc`/`.profile`.
#[test]
#[ignore = "test_gh_token_works: requires a live vm-service daemon on 127.0.0.1:6240, a real Tart hypervisor with a bootable ubuntu2404 golden image, guest network access to github.com, and the configured GitHub token in the default credential pack"]
fn test_gh_token_works() {
    let port = live_port();
    if !require_daemon(port) {
        return;
    }
    let (record, _lease) = acquire(port, "e2e-gh", "default");
    let vm = record["vm"].as_str().expect("acquired VM name");
    let response = exec(
        port,
        vm,
        &json!({"argv": ["bash", "-lc", "gh api user --jq .login"]}),
        GH_EXEC_TIMEOUT,
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body["rc"], 0);
    assert_eq!(
        response.body["output"].as_str().unwrap_or("").trim(),
        "WeZZard"
    );
}
