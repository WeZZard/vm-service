//! Application-association discovery, ported from
//! `tests/unit/test_application_endpoint.py`.
//!
//! The Python suite loaded the daemon module and drove `GET /applications`
//! through `Handler.do_GET`, patching `Path.home` and the module globals. The
//! Rust port keeps association state in one `Service` and exposes the same
//! trusted-config surface: [`Service::initialize_application_associations`],
//! [`Service::application_associations`], [`Service::association_tuples`], and
//! [`Service::associations_initialized`]. Catalog loading is the free function
//! `application_catalog::load_catalog`.
//!
//! Cases that assert daemon startup order (`main`, `fcntl.flock`,
//! `ThreadingHTTPServer`, `threading.Thread`) live in the `vm-service` binary,
//! not in `vm-service-core`, and are marked `#[ignore]` with the Python name.
//!
//! Run with:
//! `cd rust && CARGO_TARGET_DIR=$PWD/target-host cargo test -p vm-service-core --test application_endpoint`

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use vm_service_core::application_catalog;
use vm_service_core::host::Host;
use vm_service_core::service::Service;

/// The fixture line written by the Python `setUp`.
const FIXTURE_CONF: &str =
    "LINE_KIND=linux\nBASE_VM=fixture-base\nCLONE_PREFIX=fixture-\nGUEST_PASS=secret\n";

fn pilot(fixture: &common::Fixture) -> PathBuf {
    fixture.service.config.pilot.clone()
}

/// Build `<pilot>/images/fixture/line.conf`, the one valid association.
fn seed_fixture_line(pilot: &Path) {
    let conf = pilot.join("images/fixture/line.conf");
    std::fs::create_dir_all(conf.parent().expect("conf parent")).expect("image dir");
    std::fs::write(&conf, FIXTURE_CONF).expect("fixture conf");
}

/// A fresh service over the same pilot directory, mirroring Python's
/// `load_daemon(); fresh.PILOT = svc.PILOT`.
fn service_with_pilot(fixture: &common::Fixture, pilot: PathBuf) -> Arc<Service> {
    let mut config = fixture.service.config.clone();
    config.pilot = pilot;
    Arc::new(Service::with_host(
        config,
        None,
        Arc::clone(&fixture.host) as Arc<dyn Host>,
    ))
}

fn sha256_hex(raw: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw);
    format!("{:x}", hasher.finalize())
}

// --------------------------------------------------------------------- tests

#[test]
fn test_uninitialized_503_never_initializes_or_runs_process() {
    let fixture = common::Fixture::new();
    seed_fixture_line(&pilot(&fixture));

    // The Python handler answers 503 from `application_associations()` without
    // attempting initialization. Twice, it must stay uninitialized.
    for _ in 0..2 {
        let error = fixture.service.application_associations().unwrap_err();
        assert!(
            error.to_string().contains("unavailable"),
            "uninitialized read must fail closed: {error}"
        );
    }
    assert!(!fixture.service.associations_initialized());

    // The reads were not startup attempts: initialization still succeeds
    // (Python's `_application_associations_attempted` stayed false).
    fixture
        .service
        .initialize_application_associations()
        .expect("a later startup initialization is still allowed");
}

#[test]
fn test_initialized_success_cache_independence_and_detachment() {
    let fixture = common::Fixture::new();
    seed_fixture_line(&pilot(&fixture));

    // Poison the permissive lifecycle cache; associations must not consult it.
    {
        let mut cache = fixture.service.lines.lock().expect("lines");
        let mut poison = Map::new();
        poison.insert("poison".to_string(), json!({"ssh_pass": "secret"}));
        cache.lines = Some(poison);
    }

    fixture
        .service
        .initialize_application_associations()
        .expect("init");
    let expected = json!({"fixture": {"kind": "linux", "base_vm": "fixture-base"}});
    let associations = fixture
        .service
        .application_associations()
        .expect("associations");
    assert_eq!(Value::Object(associations.clone()), expected);

    // The returned view is detached: mutating it must not change the service.
    let mut mutated = associations;
    mutated
        .get_mut("fixture")
        .expect("fixture")
        .as_object_mut()
        .expect("object")
        .insert("base_vm".to_string(), json!("mutated"));
    // Clearing the cache must not affect associations either.
    fixture.service.lines.lock().expect("lines").lines = None;
    let again = fixture
        .service
        .application_associations()
        .expect("associations again");
    assert_eq!(again["fixture"]["base_vm"], "fixture-base");

    // The Python case also asserts `load_catalog(PILOT, expected_lines,
    // diagnostic=log)` is invoked once by the HTTP handler. That call site is
    // in the daemon binary, not in `vm-service-core`; the catalog function
    // itself is exercised by `test_live_inventory_and_fingerprint_refresh`.
}

#[test]
fn test_discovery_reads_images_and_ignores_legacy_lines() {
    let fixture = common::Fixture::new();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    let legacy = pilot.join("lines/legacy/line.conf");
    std::fs::create_dir_all(legacy.parent().expect("parent")).expect("legacy dir");
    std::fs::write(
        &legacy,
        "LINE_KIND=linux\nBASE_VM=old-base\nCLONE_PREFIX=old-\n",
    )
    .expect("legacy conf");

    let mut cache = vm_service_core::lines::LineCache::default();
    let (lines, bases) =
        vm_service_core::lines::discover_lines(&fixture.service.config, &mut cache, true)
            .expect("discover");
    let mut names: Vec<String> = lines.keys().cloned().collect();
    names.sort();
    assert_eq!(names, vec!["fixture".to_string()]);
    assert_eq!(bases["fixture"], "fixture-base");

    fixture
        .service
        .initialize_application_associations()
        .expect("init");
    let mut associated: Vec<String> = fixture
        .service
        .application_associations()
        .expect("associations")
        .keys()
        .cloned()
        .collect();
    associated.sort();
    assert_eq!(associated, vec!["fixture".to_string()]);
}

#[test]
fn test_legacy_only_configuration_does_not_initialize_catalog() {
    let fixture = common::Fixture::new();
    let pilot = pilot(&fixture);
    let legacy = pilot.join("lines/legacy/line.conf");
    std::fs::create_dir_all(legacy.parent().expect("parent")).expect("legacy dir");
    std::fs::write(
        &legacy,
        "LINE_KIND=linux\nBASE_VM=old-base\nCLONE_PREFIX=old-\n",
    )
    .expect("legacy conf");

    let error = fixture
        .service
        .initialize_application_associations()
        .unwrap_err();
    assert!(error.to_string().contains("unreadable"), "{error}");
    assert!(!fixture.service.associations_initialized());
    assert!(fixture.service.association_tuples().is_empty());
}

#[test]
fn test_restart_only_config_change() {
    let fixture = common::Fixture::new();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    let conf = pilot.join("images/fixture/line.conf");

    fixture
        .service
        .initialize_application_associations()
        .expect("init");
    // Change the config on disk after startup.
    std::fs::write(
        &conf,
        "LINE_KIND=macos\nBASE_VM=new-base\nCLONE_PREFIX=new-\n",
    )
    .expect("rewrite conf");

    assert_eq!(
        fixture.service.application_associations().expect("view")["fixture"]["base_vm"],
        "fixture-base"
    );
    let error = fixture
        .service
        .initialize_application_associations()
        .unwrap_err();
    assert!(error.to_string().contains("restart"), "{error}");

    let fresh = service_with_pilot(&fixture, pilot);
    fresh
        .initialize_application_associations()
        .expect("fresh init");
    assert_eq!(
        fresh.application_associations().expect("fresh view")["fixture"],
        json!({"kind": "macos", "base_vm": "new-base"})
    );
}

#[test]
fn test_strict_config_failure_no_partial_snapshot_or_fallback() {
    let fixture = common::Fixture::new();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    let invalid = [
        "false\n",
        "BASE_VM=base\nCLONE_PREFIX=x\n",
        "LINE_KIND=linux\nBASE_VM=../bad\nCLONE_PREFIX=x\n",
        "LINE_KIND=windows\nBASE_VM=base\nCLONE_PREFIX=x\n",
        "LINE_KIND=linux\nBASE_VM=base\nCLONE_PREFIX=x\nfalse\n",
    ];
    for text in invalid {
        let service = service_with_pilot(&fixture, pilot.clone());
        let bad = pilot.join("images/zzz/line.conf");
        std::fs::create_dir_all(bad.parent().expect("bad parent")).expect("bad dir");
        std::fs::write(&bad, text).expect("bad conf");

        let error = service.initialize_application_associations().unwrap_err();
        assert!(!error.to_string().is_empty(), "{text}");
        assert!(
            !service.associations_initialized(),
            "{text}: published a partial snapshot"
        );
        assert!(
            service.association_tuples().is_empty(),
            "{text}: published partial tuples"
        );
        assert!(
            service.application_associations().is_err(),
            "{text}: served a partial view"
        );
        assert!(
            service.initialize_application_associations().is_err(),
            "{text}: retried after a failed setup"
        );
    }
}

#[test]
fn test_timeout_enumeration_no_lines_fail_closed() {
    // Python patched `subprocess.run` to raise TimeoutExpired/OSError. The
    // Rust parser returns `None` for a timed-out or failed `/bin/zsh` source;
    // an unsourceable conf is the reachable equivalent of that failure.
    let fixture = common::Fixture::new();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    std::fs::write(pilot.join("images/fixture/line.conf"), "exit 1\n").expect("bad conf");
    let error = fixture
        .service
        .initialize_application_associations()
        .unwrap_err();
    assert!(!error.to_string().is_empty(), "{error}");
    assert!(!fixture.service.associations_initialized());

    // missing / empty / unreadable images directory.
    for mode in ["missing", "empty", "permission"] {
        let root = fixture
            .dir
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join(mode);
        let service = service_with_pilot(&fixture, root.clone());
        match mode {
            "empty" => std::fs::create_dir_all(root.join("images")).expect("empty images"),
            "permission" => {
                // A regular file where the directory is expected fails
                // enumeration the same way an unreadable directory does; this
                // stays meaningful even when the suite runs as root.
                std::fs::create_dir_all(&root).expect("mode root");
                std::fs::write(root.join("images"), b"not a directory").expect("images file");
            }
            _ => {}
        }
        let error = service.initialize_application_associations().unwrap_err();
        assert!(!error.to_string().is_empty(), "{mode}: {error}");
        assert!(!service.associations_initialized(), "{mode}");
    }
}

#[test]
fn test_failed_setup_requests_do_not_retry() {
    let fixture = common::Fixture::new();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    std::fs::write(pilot.join("images/fixture/line.conf"), "false\n").expect("bad conf");

    assert!(fixture
        .service
        .initialize_application_associations()
        .is_err());
    // Two requests answer 503 without retrying the failed setup.
    for _ in 0..2 {
        assert!(fixture.service.application_associations().is_err());
    }
    assert!(!fixture.service.associations_initialized());
    // The failed attempt is latched.
    assert!(fixture
        .service
        .initialize_application_associations()
        .is_err());
}

#[test]
#[ignore = "test_startup_order_and_known_failure_availability: daemon startup order (fcntl.flock, initialize, ThreadingHTTPServer, threading.Thread, log truncation) lives in vm-service/src/main.rs, not vm-service-core"]
fn test_startup_order_and_known_failure_availability() {}

#[test]
#[ignore = "test_lock_loser_no_configuration_and_programming_error_propagates: single-instance lock admission and error propagation live in vm-service/src/main.rs, not vm-service-core"]
fn test_lock_loser_no_configuration_and_programming_error_propagates() {}

#[test]
fn test_live_inventory_and_fingerprint_refresh() {
    let fixture = common::Fixture::new();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    fixture
        .service
        .initialize_application_associations()
        .expect("init");

    let base = fixture
        .dir
        .path()
        .canonicalize()
        .expect("canonicalize")
        .join(".tart/vms/fixture-base");
    std::fs::create_dir_all(&base).expect("base dir");
    for name in ["config.json", "disk.img"] {
        std::fs::write(base.join(name), b"fixture").expect("base file");
    }
    let base_root = base.parent().expect("base root").to_path_buf();
    let mut doc_base =
        application_catalog::fingerprint_base(&base_root, "fixture-base", "linux").expect("base");
    let mut applications: Vec<Value> = Vec::new();
    let inventory_path = pilot.join("images/fixture/applications.json");

    // Python patched `Path.home` and `PILOT_IMAGES_STATE_DIR`; association_path
    // reads the env var directly, so point it inside the fixture.
    let state_dir = fixture
        .dir
        .path()
        .canonicalize()
        .expect("canonicalize")
        .join("associations");
    std::env::set_var("PILOT_IMAGES_STATE_DIR", &state_dir);

    let publish = |doc_base: &Value, applications: &[Value]| {
        let portable = json!({
            "schemaVersion": 1,
            "image": "fixture",
            "inventory": {
                "schemaVersion": 1,
                "os": "linux",
                "architecture": "arm64",
                "collectedAt": "2026-09-12T00:00:00Z",
                "sources": [{"id": "dpkg", "status": "available"}],
                "applications": applications,
            },
            "provenance": {
                "extractionMode": "work",
                "evidenceId": "fixture",
                "rawSha256": "0".repeat(64),
                "collectorSha256": "1".repeat(64),
                "aliasesSha256": "2".repeat(64),
            },
        });
        let raw = serde_json::to_vec(&portable).expect("portable json");
        std::fs::write(&inventory_path, &raw).expect("applications.json");
        let local =
            application_catalog::association_path(&base_root, "fixture").expect("association path");
        std::fs::create_dir_all(local.parent().expect("local parent")).expect("local dir");
        let local_doc = json!({
            "schemaVersion": 2,
            "image": "fixture",
            "base": doc_base,
            "inventorySha256": sha256_hex(&raw),
        });
        std::fs::write(&local, local_doc.to_string()).expect("association");
    };

    let associations = Value::Object(
        fixture
            .service
            .application_associations()
            .expect("associations"),
    );
    let load =
        |lines: &Value| application_catalog::load_catalog(&pilot, lines, Some(&base_root), None);

    publish(&doc_base, &applications);
    let catalog = load(&associations).expect("first load");
    assert_eq!(catalog["images"][0]["applications"], json!([]));

    applications = vec![json!({"id": "app", "name": "App", "aliases": [], "version": "2"})];
    publish(&doc_base, &applications);
    let catalog = load(&associations).expect("second load");
    assert_eq!(catalog["images"][0]["applications"][0]["version"], "2");

    // A changed base size invalidates the catalog (503 with no usable images).
    std::fs::write(base.join("disk.img"), b"changed size").expect("resize");
    assert!(load(&associations).is_err());

    doc_base =
        application_catalog::fingerprint_base(&base_root, "fixture-base", "linux").expect("base");
    publish(&doc_base, &applications);
    assert!(load(&associations).is_ok());

    std::fs::write(&inventory_path, "{}").expect("malformed inventory");
    assert!(load(&associations).is_err());

    std::env::remove_var("PILOT_IMAGES_STATE_DIR");
}
