//! Port of `tests/integration/test_selected_environment.py`.
//!
//! Real loopback HTTP and `vmctl` against an isolated executable fake Tart. No
//! hypervisor, guest, live SSH, or running service is touched. The Python suite
//! started the daemon in-process and replaced `discover_lines`, `boot_refused`,
//! `lease_keys.create`/`bootstrap`/`cleanup`, `wait_ssh`, and `verify_transfer`
//! with stubs. This port starts the real daemon binary with `--environment` and
//! supplies fake `tart` (as the selected `tartPath`) and fake `ssh`/`scp` (via
//! `PATH`), which is the process-level equivalent of those stubs.
//!
//! Forced deviations:
//!   * `test_legacy_server_rejects_selected_header_but_keeps_health_shape`
//!     flipped `svc.ENVIRONMENT = None` on the same in-process server. A
//!     process selects legacy-vs-selected at startup, so the port starts a
//!     fresh legacy daemon and sends it the selected fingerprint; the
//!     observable assertions are unchanged.
//!   * The Python in-process fixture never started `serve()`, so its GC loop
//!     never persisted `state.json` and the tests could assert file absence.
//!     The real daemon (Python and Rust alike) persists an empty state file at
//!     startup, so the port asserts that no leases exist instead.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use common::{Daemon, LegacyFixture, Response};
use serde_json::{json, Value};
use tempfile::TempDir;

/// One selected-environment fixture: profile, images repository, fake Tart,
/// PATH shims, and a spawned daemon.
struct Selected {
    _dir: TempDir,
    root: PathBuf,
    _home: PathBuf,
    _repo: PathBuf,
    profile_path: PathBuf,
    profile: Value,
    bundle: Value,
    _bin: PathBuf,
    store: PathBuf,
    port: u16,
    daemon: Option<Daemon>,
}

impl Selected {
    fn new() -> Selected {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = common::canonical_dir(dir.path());
        let home = root.join("home");
        let repo = root.join("images-repo");
        let bin = root.join("bin");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(repo.join("images/ubuntu2404")).expect("repo");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::write(
            repo.join("images/ubuntu2404/line.conf"),
            "LINE_KIND=linux\nBASE_VM=pilot-ubuntu-base\nCLONE_PREFIX=pilot-\nGUEST_USER=admin\nGUEST_PASS=admin\n",
        )
        .expect("line.conf");

        // The fake Tart is the selected `tartPath`; ssh/scp come from PATH.
        common::write_exec(&root, "fake-tart", common::TART_SHIM);
        common::install_shims(&bin);
        let tart = root.join("fake-tart");
        let vmctl = common::vmctl_bin().to_path_buf();

        for _ in 0..16 {
            let port = common::free_port();
            // Fresh mutable roots per attempt: a failed bind must not leave an
            // ownership marker that a differently-fingerprinted retry rejects.
            let attempt = root.join(format!("attempt-{port}"));
            let store = attempt.join("store");
            let profile = json!({
                "schemaVersion": 1,
                "id": "fixture",
                "vmServiceUrl": format!("http://127.0.0.1:{port}/"),
                "imageRepository": repo.to_string_lossy(),
                "tartHome": store.to_string_lossy(),
                "serviceStateDir": attempt.join("service").to_string_lossy(),
                "imageStateDir": attempt.join("images-state").to_string_lossy(),
                "relayStateDir": attempt.join("relay").to_string_lossy(),
                "vmctlPath": vmctl.to_string_lossy(),
                "tartPath": tart.to_string_lossy(),
            });
            let profile_path = root.join(format!("environment-{port}.json"));
            std::fs::write(
                &profile_path,
                serde_json::to_vec(&profile).expect("profile json"),
            )
            .expect("write profile");

            let mut environ = HashMap::new();
            environ.insert("HOME".to_string(), home.to_string_lossy().into_owned());
            let bundle = environment::load_environment_with(profile_path.to_str(), &environ)
                .expect("load environment")
                .expect("selected environment");

            let env = vec![
                ("HOME".to_string(), home.to_string_lossy().into_owned()),
                ("PATH".to_string(), common::path_with(&bin)),
                // Stale ambient values that the selected profile must override.
                ("TART_HOME".to_string(), "/must-not-use".to_string()),
                ("VM_SERVICE_STATE".to_string(), "/must-not-use".to_string()),
                (
                    "FAKE_ROOT".to_string(),
                    store.to_string_lossy().into_owned(),
                ),
            ];
            let arguments = ["--environment", profile_path.to_str().expect("path")];
            if let Ok(daemon) = Daemon::try_start(&root, port, &arguments, &env) {
                return Selected {
                    _dir: dir,
                    root,
                    _home: home,
                    _repo: repo,
                    profile_path,
                    profile,
                    bundle,
                    _bin: bin,
                    store,
                    port,
                    daemon: Some(daemon),
                };
            }
        }
        panic!("could not start the selected daemon on a free port");
    }

    fn stop_daemon(&mut self) {
        self.daemon.take();
    }

    fn cli_env(&self) -> Vec<(String, String)> {
        vec![
            ("VM_SERVICE_PORT".to_string(), "1".to_string()),
            ("TART_HOME".to_string(), "/wrong".to_string()),
        ]
    }

    fn cli(&self, arguments: &[&str], file: Option<&Path>) -> std::process::Output {
        let profile = file
            .unwrap_or_else(|| Path::new(&self.profile_path))
            .to_str()
            .expect("profile path");
        let mut all = vec!["--environment", profile];
        all.extend_from_slice(arguments);
        common::run_vmctl(&all, &self.cli_env())
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        payload: Option<&Value>,
        fingerprint: Option<&str>,
    ) -> Response {
        common::http(self.port, method, path, payload, fingerprint)
    }

    fn fingerprint(&self) -> String {
        self.bundle["identity"]["fingerprint"]
            .as_str()
            .expect("fingerprint")
            .to_string()
    }

    fn calls_path(&self) -> PathBuf {
        self.store.join("calls.jsonl")
    }

    fn calls(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.calls_path()).unwrap_or_default();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("call json"))
            .collect()
    }

    fn state_file(&self) -> PathBuf {
        Path::new(self.profile["serviceStateDir"].as_str().expect("state dir")).join("state.json")
    }

    /// Whether the daemon holds no leases.
    fn vms_empty(&self) -> bool {
        match std::fs::read_to_string(self.state_file()) {
            Ok(text) => serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("vms").and_then(Value::as_object).cloned())
                .map(|vms| vms.is_empty())
                .unwrap_or(true),
            Err(_) => true,
        }
    }
}

#[test]
fn test_health_and_header_admission_before_any_tart_or_state_mutation() {
    let selected = Selected::new();
    let response = selected.request("GET", "/health", None, None);
    assert_eq!(response.status, 200);
    assert_eq!(response.body["environment"], selected.bundle["identity"]);
    for fingerprint in [None, Some("wrong")] {
        let response = selected.request(
            "POST",
            "/acquire",
            Some(&json!({"purpose": "denied"})),
            fingerprint,
        );
        assert_eq!(response.status, 409, "fingerprint {fingerprint:?}");
    }
    let response = selected.request("GET", "/health", None, Some("wrong"));
    assert_eq!(response.status, 409);
    assert!(!selected.calls_path().exists());
    assert!(selected.vms_empty(), "admission mutated lease state");
}

#[test]
fn test_cli_lifecycle_uses_selected_executable_store_and_lease_binding() {
    let selected = Selected::new();
    let result = selected.cli(
        &[
            "acquire",
            "--purpose",
            "selected-test",
            "--image",
            "ubuntu2404",
            "--env",
            "none",
        ],
        None,
    );
    assert!(
        result.status.success(),
        "acquire failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let lease: Value = serde_json::from_slice(&result.stdout).expect("lease json");
    assert_eq!(
        lease["environment_fingerprint"],
        selected.bundle["identity"]["fingerprint"]
    );
    let vm = lease["vm"].as_str().expect("vm").to_string();
    assert!(
        selected.cli(&["heartbeat", &vm], None).status.success(),
        "heartbeat failed"
    );
    assert!(
        selected.cli(&["release", &vm], None).status.success(),
        "release failed"
    );
    let calls = selected.calls();
    let ops: std::collections::BTreeSet<String> = calls
        .iter()
        .filter_map(|call| call["argv"][0].as_str().map(str::to_string))
        .collect();
    for required in ["list", "clone", "set", "run", "ip", "stop", "delete"] {
        assert!(
            ops.contains(required),
            "missing tart op {required}: {ops:?}"
        );
    }
    let tart_home = selected.profile["tartHome"].as_str().expect("tartHome");
    let tart_path = selected.profile["tartPath"].as_str().expect("tartPath");
    for call in &calls {
        assert_eq!(call["store"], tart_home);
        assert_eq!(call["exe"], tart_path);
    }
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(selected.state_file()).expect("state"))
            .expect("state json");
    assert_eq!(state, json!({"vms": {}}));
}

#[test]
fn test_cli_refuses_mismatched_backend_before_post() {
    let selected = Selected::new();
    let wrong = selected.root.join("wrong.json");
    let mut wrong_profile = selected.profile.clone();
    wrong_profile["id"] = json!("another-environment");
    std::fs::write(
        &wrong,
        serde_json::to_vec(&wrong_profile).expect("wrong profile json"),
    )
    .expect("write wrong profile");
    let result = selected.cli(&["gc"], Some(&wrong));
    assert_ne!(result.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("identity mismatch"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(selected.vms_empty());
}

#[test]
fn test_cli_unavailable_backend_fails_without_local_mutation() {
    let mut selected = Selected::new();
    selected.stop_daemon();
    let result = selected.cli(&["gc"], None);
    assert_ne!(result.status.code(), Some(0));
    assert!(selected.vms_empty());
}

#[test]
fn test_legacy_server_rejects_selected_header_but_keeps_health_shape() {
    let live = LegacyFixture::new();
    let _daemon = live.start_daemon();
    let fingerprint = Selected::new();
    let fingerprint = fingerprint.fingerprint();
    let health = live.request("GET", "/health", None);
    assert_eq!(health.body, json!({"ok": true, "service": "vm-service"}));
    let gc = common::http(
        live.port(),
        "POST",
        "/gc",
        Some(&json!({})),
        Some(&fingerprint),
    );
    assert_eq!(gc.status, 409);
    let health = common::http(live.port(), "GET", "/health", None, Some("wrong"));
    assert_eq!(health.status, 409);
    assert!(live.vms_empty());
}

/// `test_import_is_inert_even_with_invalid_selector_and_unittest_args`
///
/// Python proved that importing the daemon module with a bad
/// `VM_ENVIRONMENT_FILE` and `unittest` argv neither failed nor created state.
/// Rust has no import-time work, so the observable half is startup: the daemon
/// must fail cleanly on the invalid selector before creating any state, and
/// must ignore foreign argv.
#[test]
fn test_import_is_inert_even_with_invalid_selector_and_unittest_args() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = common::canonical_dir(dir.path());
    let state = root.join("never-state");
    let home = root.join("home");
    let port = common::free_port();
    let env = vec![
        ("VM_ENVIRONMENT_FILE".to_string(), "/absent".to_string()),
        ("VM_SERVICE_STATE".to_string(), state.display().to_string()),
        ("VM_SERVICE_PORT".to_string(), port.to_string()),
        ("HOME".to_string(), home.display().to_string()),
    ];
    let error = match Daemon::try_start(&root, port, &["unittest", "-v"], &env) {
        Ok(_) => panic!("invalid selector must refuse startup"),
        Err(error) => error,
    };
    assert!(
        error.contains("exited early"),
        "expected a clean startup failure: {error}"
    );
    assert!(!state.exists(), "invalid selector created lease state");
    assert!(
        !home.join(".tart").exists(),
        "invalid selector created the Tart store"
    );
}
