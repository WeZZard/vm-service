//! The image catalog at the component layer (`docs/arch/image-catalog.md`,
//! IC-6, TL-2): the real daemon against a fixture image repository and the
//! fake Tart store, never restarted.
//!
//! Each test cites the acceptance criterion it proves (AC-39 to AC-41 in
//! `docs/acceptance/operator.acceptance`). The live layer (TL-4) is in
//! `live_image_catalog.rs`.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{LegacyFixture, Response};
use serde_json::{json, Value};

/// Replace a `line.conf` in one rename, as an editor's atomic save does.
fn write_line(pilot: &Path, image: &str, text: &str) -> PathBuf {
    let directory = pilot.join("images").join(image);
    std::fs::create_dir_all(&directory).expect("line folder");
    let path = directory.join("line.conf");
    let temporary = directory.join(".line.conf.tmp");
    std::fs::write(&temporary, text).expect("write line.conf");
    std::fs::rename(&temporary, &path).expect("replace line.conf");
    path
}

/// Edit the fake Tart store's VM list (`name=running` per line, as the shim
/// keeps it): drop `remove`, then add `add` stopped. Other VMs, running
/// leases included, keep their lines.
fn edit_fake_vms(fixture: &LegacyFixture, remove: Option<&str>, add: &str) {
    let path = fixture.tart_home.join("fake-vms");
    let mut text: String = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|line| Some(line.split('=').next().unwrap_or_default()) != remove)
        .map(|line| format!("{line}\n"))
        .collect();
    text.push_str(&format!("{add}=0\n"));
    std::fs::write(&path, text).expect("fake-vms");
}

fn images(fixture: &LegacyFixture) -> Response {
    fixture.request("GET", "/images", None)
}

fn image_names(response: &Response) -> Vec<String> {
    response.body["images"]
        .as_object()
        .map(|images| images.keys().cloned().collect())
        .unwrap_or_default()
}

fn acquire(fixture: &LegacyFixture, purpose: &str, image: &str) -> Value {
    let response = fixture.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": purpose, "image": image, "env": "none"})),
    );
    assert_eq!(response.status, 200, "acquire {image}: {}", response.body);
    response.body
}

fn error_lines(fixture: &LegacyFixture) -> Vec<String> {
    std::fs::read_to_string(fixture.state_dir.join("service.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("ERROR"))
        .map(str::to_string)
        .collect()
}

const EXTRA: &str = "LINE_KIND=linux\nBASE_VM=pilot-extra-base\nCLONE_PREFIX=pilot-extra-\nGUEST_USER=admin\nGUEST_PASS=admin\n";

/// AC-39: adding a line's folder shows the image on the next request and an
/// acquisition of it succeeds; removing a line hides it while a running
/// lease of it can still heartbeat and release. The daemon's process never
/// changes.
#[test]
fn ac39_adding_and_removing_a_line_without_a_restart() {
    let fixture = LegacyFixture::new();
    let daemon = fixture.start_daemon();
    let pid = daemon.pid();
    assert_eq!(image_names(&images(&fixture)), ["macos26", "ubuntu2404"]);

    let ubuntu = acquire(&fixture, "kept-lease", "ubuntu2404");
    let ubuntu_vm = ubuntu["vm"].as_str().expect("vm").to_string();

    edit_fake_vms(&fixture, None, "pilot-extra-base");
    write_line(&fixture.pilot, "linux-extra", EXTRA);
    let listed = images(&fixture);
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(
        image_names(&listed),
        ["linux-extra", "macos26", "ubuntu2404"]
    );
    assert_eq!(listed.body["catalog"]["status"], "current");
    assert_eq!(listed.body["catalog"]["generation"], 2);
    let machines = fixture.request("GET", "/machines", None);
    assert_eq!(machines.body["catalog"]["generation"], 2);
    assert_eq!(machines.body["catalog"]["status"], "current");

    let extra = acquire(&fixture, "new-line", "linux-extra");
    assert_eq!(extra["image"], "linux-extra");
    assert!(extra["vm"]
        .as_str()
        .expect("vm")
        .starts_with("pilot-extra-"));

    std::fs::remove_dir_all(fixture.pilot.join("images/ubuntu2404")).expect("remove line");
    let listed = images(&fixture);
    assert_eq!(image_names(&listed), ["linux-extra", "macos26"]);

    let heartbeat = fixture.request(
        "POST",
        &format!("/machines/{ubuntu_vm}/heartbeat"),
        Some(&json!({"ttl_hours": 2})),
    );
    assert_eq!(heartbeat.status, 200, "{}", heartbeat.body);
    for vm in [ubuntu_vm.as_str(), extra["vm"].as_str().expect("vm")] {
        let released =
            fixture.request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
        assert_eq!(released.status, 200, "release {vm}: {}", released.body);
    }
    assert_eq!(daemon.pid(), pid, "the daemon was restarted");
}

/// AC-40: editing CPU shows in `machctl images-show`, and renaming the work
/// VM to the base name, as promotion does, makes the image acquirable with
/// no rebuild at all.
#[test]
fn ac40_editing_a_line_and_promoting_a_base_without_a_restart() {
    let fixture = LegacyFixture::new();
    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=station\nGUEST_PASS=station\nCPU=6\n",
    );
    let daemon = fixture.start_daemon();
    let pid = daemon.pid();
    let cpu = |fixture: &LegacyFixture| {
        let output = common::run_machctl(&["images-show", "macos26"], &fixture.machctl_env());
        assert!(output.status.success(), "{output:?}");
        let record: Value = serde_json::from_slice(&output.stdout).expect("image json");
        record["defaults"]["cpu"].clone()
    };
    assert_eq!(cpu(&fixture), 6);
    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=station\nGUEST_PASS=station\nCPU=4\n",
    );
    assert_eq!(cpu(&fixture), 4);

    edit_fake_vms(&fixture, None, "pilot-promoted-work");
    write_line(
        &fixture.pilot,
        "promoted",
        "LINE_KIND=linux\nBASE_VM=pilot-promoted-base\nWORK_VM=pilot-promoted-work\nCLONE_PREFIX=pilot-promoted-\nGUEST_USER=admin\nGUEST_PASS=admin\n",
    );
    let before = images(&fixture);
    assert_eq!(before.body["images"]["promoted"]["base_available"], false);
    assert_eq!(
        before.body["images"]["promoted"]["concurrency"]["acquirable"],
        false
    );
    let generation = before.body["catalog"]["generation"].clone();

    // `tart rename pilot-promoted-work pilot-promoted-base`.
    edit_fake_vms(&fixture, Some("pilot-promoted-work"), "pilot-promoted-base");
    let after = images(&fixture);
    assert_eq!(after.body["images"]["promoted"]["base_available"], true);
    assert_eq!(
        after.body["images"]["promoted"]["concurrency"]["acquirable"],
        true
    );
    assert_eq!(
        after.body["catalog"]["generation"], generation,
        "a promotion needs no rebuild"
    );
    assert_eq!(daemon.pid(), pid);
}

/// AC-41: a syntax error keeps every image of generation 7, reports `stale`
/// with the file, the error and the time in `/images`, `/machines`,
/// `/health` and the CLI, writes exactly one ERROR line however many
/// requests follow, and fixing the file returns `current` at a new
/// generation. `POST /images/reload` reaches generation 7.
#[test]
fn ac41_a_broken_line_keeps_the_last_good_list_and_is_loud() {
    let fixture = LegacyFixture::new();
    let _daemon = fixture.start_daemon();
    for expected in 2..=7 {
        let reloaded = fixture.request("POST", "/images/reload", None);
        assert_eq!(reloaded.status, 200, "{}", reloaded.body);
        assert_eq!(reloaded.body["catalog"]["generation"], expected);
        assert_eq!(reloaded.body["catalog"]["status"], "current");
    }

    let broken = write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nif then ( fi\n",
    );
    for _ in 0..5 {
        let listed = images(&fixture);
        assert_eq!(listed.status, 200, "{}", listed.body);
        assert_eq!(image_names(&listed), ["macos26", "ubuntu2404"]);
        let catalog = &listed.body["catalog"];
        assert_eq!(catalog["status"], "stale");
        assert_eq!(catalog["generation"], 7);
        assert_eq!(catalog["file"], broken.display().to_string());
        assert!(catalog["error"]
            .as_str()
            .expect("error")
            .contains("parse failed"));
        assert!(catalog["failed_at"].as_f64().expect("failed_at") > 0.0);
    }
    let health = fixture.request("GET", "/health", None);
    assert_eq!(health.body["catalog"]["status"], "stale");
    let machines = fixture.request("GET", "/machines", None);
    assert_eq!(machines.body["catalog"]["status"], "stale");

    let listing = common::run_machctl(&["list"], &fixture.machctl_env());
    let text = String::from_utf8_lossy(&listing.stdout);
    let first = text.lines().next().expect("a warning line");
    assert!(
        first.starts_with("machctl: warning: image catalog stale since "),
        "{text}"
    );
    assert!(first.contains(&broken.display().to_string()), "{text}");
    let reload = common::run_machctl(&["images", "--reload"], &fixture.machctl_env());
    assert_eq!(reload.status.code(), Some(1), "{reload:?}");

    assert_eq!(
        error_lines(&fixture).len(),
        1,
        "{:?}",
        error_lines(&fixture)
    );
    assert!(error_lines(&fixture)[0].contains(&broken.display().to_string()));

    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=station\nGUEST_PASS=station\n",
    );
    let listed = images(&fixture);
    assert_eq!(listed.body["catalog"]["status"], "current");
    assert_eq!(listed.body["catalog"]["generation"], 8);
    assert!(listed.body["catalog"].get("error").is_none());
    let reload = common::run_machctl(&["images", "--reload"], &fixture.machctl_env());
    assert!(reload.status.success(), "{reload:?}");
}

/// AC-41: a legacy startup with broken files serves with the catalog
/// `unavailable`, answers catalog requests with HTTP 503 and the
/// diagnostic, and builds at once when the files are fixed.
#[test]
fn ac41_a_broken_start_is_unavailable_until_the_files_are_fixed() {
    let fixture = LegacyFixture::new();
    let broken = write_line(&fixture.pilot, "macos26", "false\n");
    let _daemon = fixture.start_daemon();

    let listed = images(&fixture);
    assert_eq!(listed.status, 503, "{}", listed.body);
    assert_eq!(listed.body["catalog"]["status"], "unavailable");
    assert_eq!(listed.body["catalog"]["generation"], Value::Null);
    assert_eq!(listed.body["catalog"]["file"], broken.display().to_string());
    assert!(listed.body["error"]
        .as_str()
        .expect("diagnostic")
        .contains("image catalog unavailable"));
    assert_eq!(fixture.request("GET", "/applications", None).status, 503);
    let health = fixture.request("GET", "/health", None);
    assert_eq!(health.status, 200);
    assert_eq!(health.body["catalog"]["status"], "unavailable");
    let refused = fixture.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "nothing", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(refused.status, 409);
    assert!(refused.body["error"]
        .as_str()
        .expect("error")
        .contains("image catalog unavailable"));
    let shown = common::run_machctl(&["images-show", "macos26"], &fixture.machctl_env());
    assert_eq!(shown.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&shown.stderr).contains("image catalog unavailable"));

    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\n",
    );
    let listed = images(&fixture);
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(listed.body["catalog"]["status"], "current");
    assert_eq!(listed.body["catalog"]["generation"], 1);
    assert_eq!(error_lines(&fixture).len(), 1);
}

/// IC-5: under a selected environment a startup whose catalog cannot be
/// built still refuses to start.
#[test]
fn ic5_a_selected_startup_with_a_broken_catalog_refuses_to_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = common::canonical_dir(dir.path());
    let home = root.join("home");
    let repo = root.join("images-repo");
    let bin = root.join("bin");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&bin).expect("bin");
    write_line(
        &repo,
        "ubuntu2404",
        "LINE_KIND=linux\nBASE_VM=pilot-ubuntu-base\nfalse\n",
    );
    common::write_exec(&root, "fake-tart", common::TART_SHIM);
    common::install_shims(&bin);
    let port = common::free_port();
    let state = root.join("attempt");
    let profile = json!({
        "schemaVersion": 2,
        "id": "fixture",
        "vmServiceUrl": format!("http://127.0.0.1:{port}/"),
        "imageRepository": repo.to_string_lossy(),
        "tartHome": state.join("store").to_string_lossy(),
        "serviceStateDir": state.join("service").to_string_lossy(),
        "imageStateDir": state.join("images-state").to_string_lossy(),
        "relayStateDir": state.join("relay").to_string_lossy(),
        "machctlPath": common::machctl_bin().to_string_lossy(),
        "tartPath": root.join("fake-tart").to_string_lossy(),
    });
    let profile_path = root.join("environment.json");
    std::fs::write(&profile_path, profile.to_string()).expect("profile");
    let mut environ = HashMap::new();
    environ.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    environment::load_environment_with(profile_path.to_str(), &environ)
        .expect("a valid profile")
        .expect("selected");

    let mut child = Command::new(common::DAEMON)
        .args(["--environment", profile_path.to_str().expect("path")])
        .env("HOME", &home)
        .env("PATH", common::path_with(&bin))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("a selected daemon with a broken catalog kept running");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = child.wait_with_output().expect("output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!status.success());
    assert!(
        stderr.contains("selected image configuration is invalid"),
        "{stderr}"
    );
}
