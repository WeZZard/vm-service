//! The guest network over the real daemon (DC-13; `docs/acquisition-contract.md`,
//! "Guest network").
//!
//! The fixture's `bin` comes first on the daemon's `PATH`, so a `softnet`
//! placed there is the one the daemon checks, as Tart would. A symlink to
//! `/usr/bin/sudo` stands in for a setuid-root Softnet; a plain file stands in
//! for one that Tart cannot run unattended.

mod common;

use common::{Daemon, LegacyFixture, Response};
use serde_json::{json, Value};

struct Live {
    fixture: LegacyFixture,
    _daemon: Daemon,
}

enum Softnet {
    SetuidRoot,
    NotSetuid,
}

impl Live {
    fn new(softnet: Softnet) -> Live {
        let fixture = LegacyFixture::new();
        let path = fixture.bin.join("softnet");
        match softnet {
            Softnet::SetuidRoot => {
                std::os::unix::fs::symlink("/usr/bin/sudo", &path).expect("softnet symlink")
            }
            Softnet::NotSetuid => std::fs::write(&path, "#!/bin/sh\n").expect("softnet file"),
        }
        let daemon = fixture.start_daemon();
        Live {
            fixture,
            _daemon: daemon,
        }
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        self.fixture.request(method, path, body)
    }

    fn acquire(&self, purpose: &str, network: Option<&str>) -> Response {
        let mut body = json!({"purpose": purpose, "image": "ubuntu2404", "env": "none"});
        if let Some(network) = network {
            body["network"] = json!(network);
        }
        self.request("POST", "/acquire", Some(&body))
    }

    /// The `tart run` line the fake Tart recorded for `vm`.
    fn run_line(&self, vm: &str) -> String {
        self.fixture
            .tart_calls()
            .lines()
            .find(|line| line.starts_with(&format!("run {vm}")))
            .unwrap_or_else(|| panic!("no tart run for {vm}: {}", self.fixture.tart_calls()))
            .to_string()
    }

    fn release(&self, vm: &str) {
        let response = self.request("POST", &format!("/vms/{vm}/release"), Some(&json!({})));
        assert_eq!(response.status, 200, "{}", response.body);
    }
}

#[test]
fn internet_boots_on_softnet_and_is_reported() {
    let live = Live::new(Softnet::SetuidRoot);
    let response = live.acquire("web-only", Some("internet"));
    assert_eq!(response.status, 200, "{}", response.body);
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    assert_eq!(response.body["network"], "internet");
    assert_eq!(
        response.body["configuration"]["requested"]["network"],
        "internet"
    );
    assert_eq!(
        response.body["configuration"]["effective"]["network"],
        "internet"
    );
    assert_eq!(
        live.run_line(&vm),
        format!("run {vm} --no-graphics --net-softnet")
    );

    let one = live.request("GET", &format!("/vms/{vm}"), None);
    assert_eq!(one.body["network"], "internet");
    let all = live.request("GET", "/vms", None);
    assert_eq!(all.body["vms"][&vm]["network"], "internet");

    // The host still reaches the guest over the lease's SSH key.
    let exec = live.request(
        "POST",
        &format!("/vms/{vm}/exec"),
        Some(&json!({"argv": ["uname", "-a"]})),
    );
    assert_eq!(exec.status, 200, "{}", exec.body);
    live.release(&vm);
}

#[test]
fn nat_and_absent_network_keep_today_s_command() {
    let live = Live::new(Softnet::SetuidRoot);
    for (purpose, network) in [("absent", None), ("explicit", Some("nat"))] {
        let response = live.acquire(purpose, network);
        assert_eq!(response.status, 200, "{}", response.body);
        let vm = response.body["vm"].as_str().expect("vm").to_string();
        assert_eq!(response.body["network"], "nat");
        assert_eq!(
            response.body["configuration"]["effective"]["network"],
            "nat"
        );
        assert_eq!(live.run_line(&vm), format!("run {vm} --no-graphics"));
        live.release(&vm);
    }
}

#[test]
fn internet_without_usable_softnet_fails_before_the_clone() {
    let live = Live::new(Softnet::NotSetuid);
    let response = live.acquire("web-only", Some("internet"));
    assert_eq!(response.status, 409, "{}", response.body);
    let error = response.body["error"].as_str().expect("error");
    assert!(error.starts_with("Softnet unavailable: "), "{error}");
    assert!(error.contains("no NAT fallback"), "{error}");
    assert!(
        !live.fixture.tart_calls().contains("clone"),
        "Tart cloned: {}",
        live.fixture.tart_calls()
    );
    assert!(live.fixture.vms_empty(), "a refused lease left a record");

    // NAT is unaffected by a missing Softnet.
    let response = live.acquire("nat-still", None);
    assert_eq!(response.status, 200, "{}", response.body);
    live.release(response.body["vm"].as_str().expect("vm"));
}

#[test]
fn other_networks_are_rejected_before_reservation() {
    let live = Live::new(Softnet::SetuidRoot);
    for network in ["bridged", "host", "softnet", ""] {
        let response = live.acquire("bad-net", Some(network));
        assert_eq!(response.status, 409, "{}", response.body);
        assert_eq!(response.body["error"], "network must be nat or internet");
    }
    // The control-only contract refuses as before.
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "ctl", "image": "ubuntu2404", "env": "none", "network": "control-only"})),
    );
    assert_eq!(response.status, 409, "{}", response.body);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .starts_with("control-only unavailable"));
    assert!(
        !live.fixture.tart_calls().contains("clone"),
        "Tart cloned: {}",
        live.fixture.tart_calls()
    );
}

#[test]
fn discovery_reports_whether_internet_is_available() {
    let available = Live::new(Softnet::SetuidRoot);
    let response = available.request("GET", "/acquisition-capabilities", None);
    assert_eq!(response.status, 200);
    let network = &response.body["options"]["network"];
    assert_eq!(network["default"], "nat");
    assert_eq!(network["values"], json!(["nat", "internet"]));
    assert_eq!(network["internet"]["available"], true);

    let unavailable = Live::new(Softnet::NotSetuid);
    let response = unavailable.request("GET", "/acquisition-capabilities", None);
    let internet = &response.body["options"]["network"]["internet"];
    assert_eq!(internet["available"], false);
    assert!(internet["reason"]
        .as_str()
        .expect("reason")
        .contains("is not a setuid-root file"));
    assert!(
        unavailable.fixture.tart_calls().is_empty(),
        "discovery invoked Tart: {}",
        unavailable.fixture.tart_calls()
    );
}
