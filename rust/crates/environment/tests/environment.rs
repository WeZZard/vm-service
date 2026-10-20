//! Ported tests from `tests/unit/test_environment_config.py`.
//!
//! Daemon-level cases (`load_daemon`, `discover_lines`, HTTP routing) belong to
//! `machine-service-core` / `machine-service` and are exercised there. This file covers
//! the resolver and ownership behaviour the `environment` crate owns.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use environment::{canonical_profile, load_environment_with, ownership, EnvironmentError, MARKER};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// Guards the process-wide current directory for the one test that needs it.
static CWD_LOCK: Mutex<()> = Mutex::new(());

fn string(value: impl AsRef<std::ffi::OsStr>) -> Value {
    Value::String(value.as_ref().to_string_lossy().into_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

/// `json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False)`.
fn canonical_json_bytes(value: &Value) -> Vec<u8> {
    match value.as_object() {
        Some(object) => {
            let sorted: BTreeMap<&String, &Value> = object.iter().collect();
            serde_json::to_vec(&sorted).expect("serializing JSON cannot fail")
        }
        None => serde_json::to_vec(value).expect("serializing JSON cannot fail"),
    }
}

fn list_files(root: &Path) -> Vec<PathBuf> {
    fn walk(directory: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("readable directory") {
            let entry = entry.expect("readable entry");
            out.push(entry.path());
            if entry.file_type().expect("file type").is_dir() {
                walk(&entry.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

struct CwdGuard {
    previous: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    repo: PathBuf,
    fake: PathBuf,
    profile: Map<String, Value>,
    file: PathBuf,
    env: HashMap<String, String>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let root = tmp.path().canonicalize().expect("canonical temp dir");
        let repo = root.join("images-repo");
        fs::create_dir_all(repo.join("images")).expect("create repo");
        let fake = root.join("fake-tart");
        fs::write(&fake, "#!/bin/sh\nprintf \"%s\\n\" \"$TART_HOME\"\n").expect("write fake");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod fake");

        // `machctlPath` must be an executable regular file, and the resolver is
        // fitted with a synthetic `tart` for the same reason. The Python
        // `bin/vmctl` this fixture used to point at was removed together with
        // the rest of the Python implementation.
        let fake_machctl = root.join("fake-machctl");
        fs::write(&fake_machctl, "#!/bin/sh\nexit 0\n").expect("write fake machctl");
        fs::set_permissions(&fake_machctl, fs::Permissions::from_mode(0o755))
            .expect("chmod fake machctl");

        let mut profile = Map::new();
        profile.insert("schemaVersion".to_string(), json!(2));
        profile.insert("id".to_string(), json!("fixture"));
        profile.insert("vmServiceUrl".to_string(), json!("http://127.0.0.1:6249/"));
        profile.insert("imageRepository".to_string(), string(&repo));
        profile.insert("tartHome".to_string(), string(root.join("store")));
        profile.insert("serviceStateDir".to_string(), string(root.join("service")));
        profile.insert(
            "imageStateDir".to_string(),
            string(root.join("images-state")),
        );
        profile.insert("relayStateDir".to_string(), string(root.join("relay")));
        profile.insert("machctlPath".to_string(), string(&fake_machctl));
        profile.insert("tartPath".to_string(), string(&fake));

        let file = root.join("environment.json");
        let mut env = HashMap::new();
        env.insert(
            "HOME".to_string(),
            root.join("home").to_string_lossy().into_owned(),
        );
        env.insert(
            "VM_ENVIRONMENT_FILE".to_string(),
            file.to_string_lossy().into_owned(),
        );

        let fixture = Fixture {
            _tmp: tmp,
            root,
            repo,
            fake,
            profile,
            file,
            env,
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        fs::write(
            &self.file,
            serde_json::to_vec(&Value::Object(self.profile.clone())).expect("serialize profile"),
        )
        .expect("write profile");
    }

    fn resolve(&self) -> Value {
        load_environment_with(Some(self.file.to_str().expect("utf8 path")), &self.env)
            .expect("profile resolves")
            .expect("selector present")
    }

    fn resolve_is_err(&self) -> bool {
        load_environment_with(Some(self.file.to_str().expect("utf8 path")), &self.env).is_err()
    }
}

#[test]
fn no_selection_is_legacy() {
    assert!(load_environment_with(None, &HashMap::new())
        .expect("empty environment is valid")
        .is_none());
}

#[test]
fn read_only_canonical_bundle() {
    let fixture = Fixture::new();
    let before = list_files(&fixture.root);

    let bundle = fixture.resolve();
    let profile = &bundle["profile"];
    assert_eq!(profile["vmServiceUrl"], json!("http://127.0.0.1:6249"));

    let expected = sha256_hex(&canonical_json_bytes(profile));
    assert_eq!(bundle["identity"]["fingerprint"], json!(expected));
    assert_eq!(bundle["identity"]["id"], json!("fixture"));
    assert_eq!(
        bundle["identity"]["vmServiceUrl"],
        json!("http://127.0.0.1:6249")
    );
    assert_eq!(
        bundle["identity"]["imageRepository"],
        profile["imageRepository"]
    );
    assert_eq!(bundle["environment"]["TART"], string(&fixture.fake));
    assert_eq!(bundle["environment"]["MACHINE_SERVICE_PORT"], json!("6249"));
    assert_eq!(
        bundle["environment"]["VM_ENVIRONMENT_FILE"],
        string(fixture.file.canonicalize().unwrap())
    );

    assert_eq!(before, list_files(&fixture.root));
}

#[test]
fn absolute_disjoint_nonlegacy_validation() {
    let fixture = Fixture::new();
    let cases: Vec<(&str, Value)> = vec![
        ("imageStateDir", json!("relative")),
        ("relayStateDir", string(fixture.root.join("store/child"))),
        ("tartHome", string(&fixture.root)),
        ("tartHome", string(fixture.repo.join("store"))),
        (
            "tartHome",
            string(fixture.root.join("home/.local/state/pi-vm-relay/sub")),
        ),
        (
            "tartHome",
            string(fixture.root.join("home/.local/state/mcp-vm-relay/sub")),
        ),
        ("serviceStateDir", string(&fixture.fake)),
        ("tartPath", string(&fixture.file)),
        ("imageRepository", string(fixture.root.join("absent"))),
        ("schemaVersion", json!(true)),
        ("id", json!("UPPER")),
    ];
    for (key, value) in cases {
        let mut raw = fixture.profile.clone();
        raw.insert(key.to_string(), value.clone());
        assert!(
            canonical_profile(&Value::Object(raw), &fixture.env).is_err(),
            "expected rejection for {key}={value}"
        );
    }
}

#[test]
fn url_allowlist() {
    let fixture = Fixture::new();
    let rejected = [
        "https://localhost:80",
        "http://localhost",
        "http://127.0.0.256:80",
        "http://127.1:80",
        "http://localhost:80/path",
        "http://user@localhost:80",
        "http://localhost:80?",
        "http://[::2]:80",
        "http://LOCALHOST:80",
        "http://localhost:0",
    ];
    for url in rejected {
        let mut raw = fixture.profile.clone();
        raw.insert("vmServiceUrl".to_string(), json!(url));
        assert!(
            canonical_profile(&Value::Object(raw), &fixture.env).is_err(),
            "expected rejection for {url}"
        );
    }

    let accepted = [
        ("http://localhost:00080/", "http://localhost:80"),
        ("http://[::1]:80/", "http://[::1]:80"),
        ("http://127.2.3.4:80", "http://127.2.3.4:80"),
    ];
    for (url, expected) in accepted {
        let mut raw = fixture.profile.clone();
        raw.insert("vmServiceUrl".to_string(), json!(url));
        let canonical = canonical_profile(&Value::Object(raw), &fixture.env)
            .unwrap_or_else(|error| panic!("{url} must be accepted: {error}"));
        assert_eq!(canonical["vmServiceUrl"], json!(expected));
    }
}

#[test]
fn schema_numeric_one_and_symlink_resolution() {
    let fixture = Fixture::new();
    let alias = fixture.root.join("alias");
    symlink(fixture.root.join("home"), &alias).expect("create dangling symlink");

    let mut raw = fixture.profile.clone();
    raw.insert("tartHome".to_string(), string(alias.join("new")));
    assert!(
        canonical_profile(&Value::Object(raw), &fixture.env).is_err(),
        "dangling symlink ancestor must be rejected"
    );

    fs::create_dir_all(fixture.root.join("home")).expect("create home");
    let mut raw = fixture.profile.clone();
    raw.insert("schemaVersion".to_string(), json!(2.0));
    raw.insert("tartHome".to_string(), string(alias.join("new")));
    let profile = canonical_profile(&Value::Object(raw), &fixture.env).expect("valid profile");
    assert_eq!(profile["schemaVersion"].as_i64(), Some(2));
    assert!(profile["schemaVersion"].is_i64() || profile["schemaVersion"].is_u64());
    assert_eq!(profile["tartHome"], string(fixture.root.join("home/new")));
}

#[test]
fn malformed_unknown_duplicate_and_oversized_profiles_fail_closed() {
    let fixture = Fixture::new();
    let mut unknown = fixture.profile.clone();
    unknown.insert("unknown".to_string(), json!(1));
    let cases = [
        "{".to_string(),
        serde_json::to_string(&Value::Object(unknown)).expect("serialize"),
        "{\"id\":\"a\",\"id\":\"b\"}".to_string(),
        " ".repeat(65537),
    ];
    for text in cases {
        fs::write(&fixture.file, &text).expect("write profile");
        assert!(
            fixture.resolve_is_err(),
            "expected failure for input: {text:?}"
        );
    }
    assert!(!fixture.root.join("store").exists());
}

#[test]
fn control_characters_in_paths_rejected() {
    let fixture = Fixture::new();
    let bad_paths = [
        format!("{}/bad\npath", fixture.root.display()),
        format!("{}/bad\u{7f}path", fixture.root.display()),
        format!("{}/bad\u{0}path", fixture.root.display()),
    ];
    for bad in bad_paths {
        let mut raw = fixture.profile.clone();
        raw.insert("tartHome".to_string(), json!(bad));
        assert!(
            canonical_profile(&Value::Object(raw), &fixture.env).is_err(),
            "expected rejection for control characters"
        );
    }
}

#[test]
fn selector_relative_paths() {
    let fixture = Fixture::new();
    let _guard = CWD_LOCK.lock().expect("cwd lock");
    let _cwd = CwdGuard {
        previous: std::env::current_dir().expect("current dir"),
    };
    std::env::set_current_dir(&fixture.root).expect("chdir to fixture root");

    let relative = load_environment_with(Some("environment.json"), &fixture.env)
        .expect("relative selector resolves")
        .expect("selector present");
    assert_eq!(relative, fixture.resolve());
}

#[test]
fn blank_selector_rejected() {
    let fixture = Fixture::new();
    assert!(load_environment_with(Some("   "), &fixture.env).is_err());
    let mut env = fixture.env.clone();
    env.insert("VM_ENVIRONMENT_FILE".to_string(), String::new());
    assert!(load_environment_with(None, &env).is_err());
    assert!(load_environment_with(None, &HashMap::new())
        .expect("no selector")
        .is_none());
}

#[test]
fn markers_and_store_lock_persist_without_rewriting() {
    let fixture = Fixture::new();
    let bundle = fixture.resolve();
    let marker =
        PathBuf::from(bundle["profile"]["tartHome"].as_str().expect("tartHome")).join(MARKER);

    let initial;
    {
        let _guard = ownership(&bundle).expect("first ownership");

        initial = fs::read(&marker).expect("read marker");
        assert!(marker.exists());

        // A second state directory must not bypass ownership of the store.
        let other_file = fixture.root.join("other.json");
        let mut other_profile = fixture.profile.clone();
        other_profile.insert(
            "serviceStateDir".to_string(),
            string(fixture.root.join("other-state")),
        );
        fs::write(
            &other_file,
            serde_json::to_vec(&Value::Object(other_profile)).expect("serialize"),
        )
        .expect("write other profile");
        let other_config =
            load_environment_with(Some(other_file.to_str().expect("utf8 path")), &fixture.env)
                .expect("other resolves")
                .expect("selector present");
        assert!(
            ownership(&other_config).is_err(),
            "second daemon over the same store admitted"
        );
        assert!(ownership(&bundle).is_err(), "second store owner admitted");
    }

    {
        let _guard = ownership(&bundle).expect("reopen ownership");
        assert_eq!(fs::read(&marker).expect("read marker"), initial);
    }

    let mut wrong = bundle.clone();
    wrong["identity"]["fingerprint"] = json!("wrong");
    assert!(ownership(&wrong).is_err(), "mismatched marker admitted");
    assert_eq!(fs::read(&marker).expect("read marker"), initial);
}

#[test]
fn unbound_leases_rejected_before_marker_publication() {
    let fixture = Fixture::new();
    let bundle = fixture.resolve();
    let state = PathBuf::from(
        bundle["profile"]["serviceStateDir"]
            .as_str()
            .expect("serviceStateDir"),
    );
    fs::create_dir_all(&state).expect("create state");
    fs::write(
        state.join("state.json"),
        serde_json::to_vec(&json!({"vms": {"old": {"vm": "old"}}})).expect("serialize"),
    )
    .expect("write state");

    assert!(ownership(&bundle).is_err(), "unbound state admitted");
    assert!(!state.join(MARKER).exists());
}

#[test]
fn error_messages_match_python() {
    let fixture = Fixture::new();
    let mut raw = fixture.profile.clone();
    raw.insert("imageStateDir".to_string(), json!("relative"));
    let error = canonical_profile(&Value::Object(raw), &fixture.env).expect_err("relative path");
    assert_eq!(error.to_string(), "imageStateDir must be an absolute path");
    assert!(matches!(error, EnvironmentError::Invalid(_)));

    let mut raw = fixture.profile.clone();
    raw.insert("schemaVersion".to_string(), json!(true));
    let error = canonical_profile(&Value::Object(raw), &fixture.env).expect_err("bool version");
    assert_eq!(error.to_string(), "schemaVersion must be integer 2");

    // Python's check was `type(x) not in (int, float) or x != 1`, so an
    // integral float is accepted, while a bool, a string, a non-integral float
    // and any other integer are rejected. Measured on CPython 3.14.7 at schema
    // 1; schema 2 keeps the same rule (DC-42).
    for literal in ["2.0", "2.00", "2e0", "2.0e0"] {
        let mut raw = fixture.profile.clone();
        raw.insert(
            "schemaVersion".to_string(),
            serde_json::from_str(literal).expect("literal parses"),
        );
        assert!(
            canonical_profile(&Value::Object(raw.clone()), &fixture.env).is_ok(),
            "canonical_profile rejected schemaVersion {literal}"
        );
        fs::write(
            &fixture.file,
            serde_json::to_vec(&Value::Object(raw)).expect("serialize"),
        )
        .expect("write profile");
        assert!(
            load_environment_with(
                Some(fixture.file.to_str().expect("utf8 path")),
                &fixture.env
            )
            .is_ok(),
            "the file path rejected schemaVersion {literal}"
        );
    }
    for literal in ["3.0", "1.5", "1", "1.0", "0", "\"2\"", "null"] {
        let mut raw = fixture.profile.clone();
        raw.insert(
            "schemaVersion".to_string(),
            serde_json::from_str(literal).expect("literal parses"),
        );
        assert!(
            canonical_profile(&Value::Object(raw), &fixture.env).is_err()
                || load_environment_with(
                    Some(fixture.file.to_str().expect("utf8 path")),
                    &fixture.env
                )
                .is_err(),
            "canonical_profile accepted schemaVersion {literal}"
        );
    }
    fixture.save();

    let mut raw = fixture.profile.clone();
    raw.insert("id".to_string(), json!("UPPER"));
    let error = canonical_profile(&Value::Object(raw), &fixture.env).expect_err("bad id");
    assert_eq!(
        error.to_string(),
        "id must be a lowercase slug of 1..48 characters"
    );
}

/// AC-45: a profile at schemaVersion 1 with a `vmctlPath` field is refused,
/// and the error names `machctlPath` and schemaVersion 2. A profile that
/// carries `vmctlPath` at any version is refused the same way, and no reader
/// falls back to the old field (DC-42).
#[test]
fn ac45_schema_one_and_vmctl_path_are_refused_with_their_fix() {
    let fixture = Fixture::new();
    let machctl = fixture.profile["machctlPath"].clone();

    let mut old = fixture.profile.clone();
    old.remove("machctlPath");
    old.insert("vmctlPath".to_string(), machctl.clone());
    old.insert("schemaVersion".to_string(), json!(1));
    let mut old_field_only = old.clone();
    old_field_only.insert("schemaVersion".to_string(), json!(2));
    let mut old_version_only = fixture.profile.clone();
    old_version_only.insert("schemaVersion".to_string(), json!(1));
    let mut both_fields = fixture.profile.clone();
    both_fields.insert("vmctlPath".to_string(), machctl);

    for raw in [old, old_field_only, old_version_only, both_fields] {
        let error = canonical_profile(&Value::Object(raw.clone()), &fixture.env)
            .expect_err("a retired profile must be refused");
        let message = error.to_string();
        assert!(message.contains("machctlPath"), "{message}");
        assert!(message.contains("schemaVersion 2"), "{message}");

        fs::write(
            &fixture.file,
            serde_json::to_vec(&Value::Object(raw)).expect("serialize"),
        )
        .expect("write profile");
        let error = load_environment_with(
            Some(fixture.file.to_str().expect("utf8 path")),
            &fixture.env,
        )
        .expect_err("a retired profile file must be refused");
        assert!(error.to_string().contains("machctlPath"), "{error}");
    }
}

/// AC-45: a schema 2 profile exports `MACHCTL` from `machctlPath` and the
/// service's `MACHINE_SERVICE_*` names, and no `VMCTL` or `VM_SERVICE_*`.
#[test]
fn ac45_schema_two_exports_machctl_and_the_renamed_variables() {
    let fixture = Fixture::new();
    fixture.save();
    let bundle = load_environment_with(
        Some(fixture.file.to_str().expect("utf8 path")),
        &fixture.env,
    )
    .expect("valid profile")
    .expect("selected");
    let exports = bundle["environment"].as_object().expect("exports");
    let mut names: Vec<&str> = exports.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "MACHCTL",
            "MACHINE_SERVICE_HOST",
            "MACHINE_SERVICE_PORT",
            "MACHINE_SERVICE_STATE",
            "PILOT_IMAGES_STATE_DIR",
            "PILOT_REPO",
            "TART",
            "TART_HOME",
            "VM_ENVIRONMENT_FILE",
            "VM_ENVIRONMENT_FINGERPRINT",
            "VM_RELAY_STATE_DIR",
            "VM_RELAY_URL",
        ]
    );
    assert_eq!(exports["MACHCTL"], bundle["profile"]["machctlPath"]);
    assert_eq!(bundle["profile"]["schemaVersion"], json!(2));
}
