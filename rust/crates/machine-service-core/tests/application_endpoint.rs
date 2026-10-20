//! Application-association discovery, ported from
//! `tests/unit/test_application_endpoint.py` and moved onto the image catalog
//! (`docs/arch/image-catalog.md`, IC).
//!
//! The Python suite loaded the daemon module and drove `GET /applications`
//! through `Handler.do_GET`, patching `Path.home` and the module globals. The
//! Rust port reads the associations from the image catalog, which the daemon
//! starts with [`Service::initialize_image_catalog`] once it owns
//! `daemon.lock` and reads with [`Service::application_associations`]. Catalog
//! loading is the free function `application_catalog::load_catalog`.
//!
//! The Python cases that pinned "configuration changes need a restart" were
//! superseded by DC-34: a change now rebuilds the catalog on the next read,
//! and a broken change keeps the last good associations. Those cases are
//! rewritten here to the new rule rather than deleted.
//!
//! Cases that assert daemon startup order (`main`, `fcntl.flock`,
//! `ThreadingHTTPServer`, `threading.Thread`) live in the `machine-service` binary,
//! not in `machine-service-core`, and are marked `#[ignore]` with the Python name.
//!
//! Run with:
//! `cd rust && CARGO_TARGET_DIR=$PWD/target-host cargo test -p machine-service-core --test application_endpoint`

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use machine_service_core::application_catalog;
use machine_service_core::catalog::Freshness;
use machine_service_core::host::Host;
use machine_service_core::service::Service;

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

/// A fresh service over `pilot` whose catalog reads the real files, the
/// analogue of Python's `load_daemon(); fresh.PILOT = svc.PILOT`.
fn service_with_pilot(fixture: &common::Fixture, pilot: PathBuf) -> Arc<Service> {
    let mut config = fixture.service.config.clone();
    config.pilot = pilot;
    fixture.host.file_lines.store(true, Ordering::SeqCst);
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

/// Rewrite a `line.conf` so its identity certainly moves, even within one
/// timestamp tick.
fn rewrite(path: &Path, text: &str) {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text).expect("write conf");
    std::fs::rename(&temporary, path).expect("replace conf");
}

// --------------------------------------------------------------------- tests

/// AC-42: before the daemon starts the catalog, which it does only once it
/// owns `daemon.lock`, a read answers unavailable and executes nothing.
#[test]
fn test_uninitialized_read_never_initializes_or_runs_process() {
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    // A line that would leave a marker if anything sourced it.
    let marker = pilot.join("sourced");
    let conf = pilot.join("images/zz-marker/line.conf");
    std::fs::create_dir_all(conf.parent().expect("parent")).expect("dir");
    std::fs::write(
        &conf,
        format!(
            "touch {}\nLINE_KIND=linux\nBASE_VM=marker-base\nCLONE_PREFIX=m-\n",
            marker.display()
        ),
    )
    .expect("marker conf");

    for _ in 0..2 {
        let error = fixture.service.application_associations().unwrap_err();
        assert!(
            error.to_string().contains("unavailable"),
            "an unstarted read must fail closed: {error}"
        );
    }
    assert_eq!(
        fixture.service.image_catalog().status.freshness,
        Freshness::Unavailable
    );
    assert!(!marker.exists(), "a read executed a line.conf");

    fixture
        .service
        .initialize_image_catalog()
        .expect("the daemon's start still succeeds");
    assert!(marker.exists(), "the start executes the lines");
}

#[test]
fn test_initialized_success_and_detachment() {
    let fixture = common::Fixture::with_file_lines();
    seed_fixture_line(&pilot(&fixture));

    fixture.service.initialize_image_catalog().expect("init");
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
    let again = fixture
        .service
        .application_associations()
        .expect("associations again");
    assert_eq!(again["fixture"]["base_vm"], "fixture-base");

    // The Python case also asserts `load_catalog(PILOT, expected_lines,
    // diagnostic=log)` is invoked once by the HTTP handler. That call site is
    // in the daemon binary, not in `machine-service-core`; the catalog function
    // itself is exercised by `test_live_inventory_and_fingerprint_refresh`.
}

#[test]
fn test_discovery_reads_images_and_ignores_legacy_lines() {
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    let legacy = pilot.join("lines/legacy/line.conf");
    std::fs::create_dir_all(legacy.parent().expect("parent")).expect("legacy dir");
    std::fs::write(
        &legacy,
        "LINE_KIND=linux\nBASE_VM=old-base\nCLONE_PREFIX=old-\n",
    )
    .expect("legacy conf");

    fixture.service.initialize_image_catalog().expect("init");
    let view = fixture.service.image_catalog();
    let names: Vec<&String> = view.snapshot().expect("snapshot").lines.keys().collect();
    assert_eq!(names, vec!["fixture"]);
    assert_eq!(
        view.snapshot().expect("snapshot").lines["fixture"].base_vm,
        "fixture-base"
    );
    let associated: Vec<String> = fixture
        .service
        .application_associations()
        .expect("associations")
        .keys()
        .cloned()
        .collect();
    assert_eq!(associated, vec!["fixture".to_string()]);
}

#[test]
fn test_legacy_only_configuration_does_not_initialize_catalog() {
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    let legacy = pilot.join("lines/legacy/line.conf");
    std::fs::create_dir_all(legacy.parent().expect("parent")).expect("legacy dir");
    std::fs::write(
        &legacy,
        "LINE_KIND=linux\nBASE_VM=old-base\nCLONE_PREFIX=old-\n",
    )
    .expect("legacy conf");

    let error = fixture.service.initialize_image_catalog().unwrap_err();
    assert!(error.to_string().contains("unreadable"), "{error}");
    let view = fixture.service.image_catalog();
    assert!(view.snapshot().is_none());
    assert_eq!(view.status.freshness, Freshness::Unavailable);
}

/// AC-40: a configuration change takes effect on the next read, without a
/// restart. Supersedes the Python `test_restart_only_config_change` (DC-34).
#[test]
fn test_config_change_takes_effect_without_restart() {
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    let conf = pilot.join("images/fixture/line.conf");

    fixture.service.initialize_image_catalog().expect("init");
    let first = fixture.service.image_catalog();
    rewrite(
        &conf,
        "LINE_KIND=macos\nBASE_VM=new-base\nCLONE_PREFIX=new-\n",
    );

    assert_eq!(
        fixture.service.application_associations().expect("view")["fixture"],
        json!({"kind": "macos", "base_vm": "new-base"})
    );
    let second = fixture.service.image_catalog();
    assert_eq!(second.status.freshness, Freshness::Current);
    assert!(
        second.status.generation > first.status.generation,
        "a rebuild publishes a new generation"
    );
    // The view taken before the change keeps its own snapshot (IC-4).
    assert_eq!(
        first.snapshot().expect("first").lines["fixture"].base_vm,
        "fixture-base"
    );
}

/// AC-41, AC-42: every strict rule fails the whole build, nothing partial is
/// published, and a failure after a good build keeps the last good snapshot.
#[test]
fn test_strict_config_failure_no_partial_snapshot_or_fallback() {
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    let invalid = [
        "false\n",
        "BASE_VM=base\nCLONE_PREFIX=x\n",
        "LINE_KIND=linux\nBASE_VM=../bad\nCLONE_PREFIX=x\n",
        "LINE_KIND=windows\nBASE_VM=base\nCLONE_PREFIX=x\n",
        "LINE_KIND=linux\nBASE_VM=base\nCLONE_PREFIX=x\nfalse\n",
        "LINE_KIND=linux\nBASE_VM=base\nCLONE_PREFIX=x\nCPU=four\n",
    ];
    let bad = pilot.join("images/zzz/line.conf");
    for text in invalid {
        std::fs::create_dir_all(bad.parent().expect("bad parent")).expect("bad dir");
        rewrite(&bad, text);
        let service = service_with_pilot(&fixture, pilot.clone());

        let error = service.initialize_image_catalog().unwrap_err();
        assert!(!error.to_string().is_empty(), "{text}");
        let view = service.image_catalog();
        assert!(
            view.snapshot().is_none(),
            "{text}: published a partial snapshot"
        );
        assert_eq!(view.status.freshness, Freshness::Unavailable, "{text}");
        assert_eq!(
            view.status
                .failure
                .as_ref()
                .and_then(|f| f.failure.file.clone()),
            Some(bad.display().to_string()),
            "{text}: the status names the failing file"
        );
        assert!(
            service.application_associations().is_err(),
            "{text}: served a partial view"
        );

        // After a good build, the same failure keeps the last good lines.
        std::fs::remove_dir_all(bad.parent().expect("bad parent")).expect("remove bad");
        let service = service_with_pilot(&fixture, pilot.clone());
        service.initialize_image_catalog().expect("good start");
        std::fs::create_dir_all(bad.parent().expect("bad parent")).expect("bad dir");
        rewrite(&bad, text);
        let view = service.image_catalog();
        assert_eq!(view.status.freshness, Freshness::Stale, "{text}");
        let names: Vec<&String> = view.snapshot().expect("last good").lines.keys().collect();
        assert_eq!(names, vec!["fixture"], "{text}");
        std::fs::remove_dir_all(bad.parent().expect("bad parent")).expect("remove bad");
    }
}

#[test]
fn test_timeout_enumeration_no_lines_fail_closed() {
    // Python patched `subprocess.run` to raise TimeoutExpired/OSError. The
    // Rust parser fails a timed-out or failed `/bin/zsh` source; an
    // unsourceable conf is the reachable equivalent of that failure.
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    std::fs::write(pilot.join("images/fixture/line.conf"), "exit 1\n").expect("bad conf");
    let error = fixture.service.initialize_image_catalog().unwrap_err();
    assert!(!error.to_string().is_empty(), "{error}");
    assert!(fixture.service.image_catalog().snapshot().is_none());

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
        let error = service.initialize_image_catalog().unwrap_err();
        assert!(!error.to_string().is_empty(), "{mode}: {error}");
        assert!(service.image_catalog().snapshot().is_none(), "{mode}");
    }
}

/// AC-41: a failed build is not retried by requests while the files stay as
/// they are, and the next change is tried at once, with no restart.
#[test]
fn test_failed_setup_requests_do_not_retry() {
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    let conf = pilot.join("images/fixture/line.conf");
    std::fs::write(&conf, "false\n").expect("bad conf");

    assert!(fixture.service.initialize_image_catalog().is_err());
    let failed_at = |service: &Service| {
        service
            .image_catalog()
            .status
            .failure
            .expect("failure")
            .failed_at
    };
    let first = failed_at(&fixture.service);
    // Two requests answer unavailable without retrying the failed build.
    for _ in 0..2 {
        assert!(fixture.service.application_associations().is_err());
        assert_eq!(failed_at(&fixture.service), first, "a request retried");
    }

    rewrite(&conf, FIXTURE_CONF);
    assert_eq!(
        fixture.service.application_associations().expect("fixed")["fixture"]["base_vm"],
        "fixture-base"
    );
}

#[test]
#[ignore = "test_startup_order_and_known_failure_availability: daemon startup order (fcntl.flock, initialize, ThreadingHTTPServer, threading.Thread, log truncation) lives in vm-service/src/main.rs, not machine-service-core"]
fn test_startup_order_and_known_failure_availability() {}

#[test]
#[ignore = "test_lock_loser_no_configuration_and_programming_error_propagates: single-instance lock admission and error propagation live in vm-service/src/main.rs, not machine-service-core"]
fn test_lock_loser_no_configuration_and_programming_error_propagates() {}

#[test]
fn test_live_inventory_and_fingerprint_refresh() {
    let fixture = common::Fixture::with_file_lines();
    let pilot = pilot(&fixture);
    seed_fixture_line(&pilot);
    fixture.service.initialize_image_catalog().expect("init");

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
