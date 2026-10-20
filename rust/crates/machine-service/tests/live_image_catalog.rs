//! The image catalog at the live layer (`docs/arch/image-catalog.md`, IC-6;
//! `docs/arch/test-layers.md`, TL-4; AC-39, AC-40, AC-18).
//!
//! A second instance built from this worktree, with real Tart and its own
//! image checkout, sees a line appear, its base get promoted and the line
//! disappear, without a restart. The test clones one stopped VM and renames
//! the clone, as promotion does; it boots nothing and deletes what it made.
//!
//! ```sh
//! cp -R ~/path/to/pilot-images /tmp/ic-live-pilot     # a checkout the test may write
//! PILOT_REPO=/tmp/ic-live-pilot scripts/live-instance.sh start ssh   # port 6250
//! LIVE_PILOT=/tmp/ic-live-pilot \
//!   cargo test -p machine-service --test live_image_catalog -- --ignored
//! ```
//!
//! `LIVE_PORT` (default 6250) selects the instance and `LIVE_CLONE_SOURCE`
//! (default `pilot-ubuntu-base`) the stopped VM to clone. The test refuses
//! the installed daemon's port and an instance whose image checkout is not
//! `LIVE_PILOT`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const INSTALLED_PORT: u16 = 6240;

fn live_port() -> u16 {
    std::env::var("LIVE_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(6250)
}

fn listener_pid(port: u16) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn tart(arguments: &[&str]) {
    let status = Command::new("tart")
        .args(arguments)
        .status()
        .expect("run tart");
    assert!(status.success(), "tart {arguments:?} failed");
}

/// Removes the test's line folder and VMs, however the test ends.
struct Cleanup {
    folder: PathBuf,
    vms: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
        for vm in &self.vms {
            let _ = Command::new("tart").args(["delete", vm]).status();
        }
    }
}

fn image(port: u16, name: &str) -> Option<Value> {
    let response = common::http(port, "GET", "/images", None, None);
    assert_eq!(response.status, 200, "{}", response.body);
    response.body["images"].get(name).cloned()
}

/// AC-39, AC-40: a line added to the instance's checkout appears, its base's
/// promotion makes it acquirable, and its removal hides it, while the
/// instance and the installed daemon keep their processes (AC-18).
#[test]
#[ignore = "TL-4: needs real Tart, a second instance on its own port started with a writable PILOT_REPO, and LIVE_PILOT naming that checkout"]
fn ac39_ac40_live_line_promotion_and_removal() {
    let port = live_port();
    assert_ne!(
        port, INSTALLED_PORT,
        "never test against the installed daemon"
    );
    let pilot = PathBuf::from(std::env::var("LIVE_PILOT").expect("LIVE_PILOT"));
    let machines = common::http(port, "GET", "/machines", None, None);
    assert_eq!(machines.status, 200, "no machine-service on port {port}");
    assert_eq!(
        Path::new(machines.body["pilot_repo"].as_str().expect("pilot_repo")),
        pilot.as_path(),
        "the instance does not read LIVE_PILOT"
    );
    let installed = listener_pid(INSTALLED_PORT);
    let instance = listener_pid(port);

    let suffix = &uuid_suffix();
    let name = format!("ic-live-{suffix}");
    let work = format!("{name}-work");
    let base = format!("{name}-base");
    let source = std::env::var("LIVE_CLONE_SOURCE").unwrap_or_else(|_| "pilot-ubuntu-base".into());
    let folder = pilot.join("images").join(&name);
    let _cleanup = Cleanup {
        folder: folder.clone(),
        vms: vec![work.clone(), base.clone()],
    };

    tart(&["clone", &source, &work]);
    std::fs::create_dir_all(&folder).expect("line folder");
    std::fs::write(
        folder.join("line.conf"),
        format!(
            "LINE_KIND=linux\nBASE_VM={base}\nWORK_VM={work}\nCLONE_PREFIX={name}-\nGUEST_USER=admin\nGUEST_PASS=admin\n"
        ),
    )
    .expect("line.conf");

    let added = image(port, &name).expect("the new line is listed");
    assert_eq!(added["base_available"], false);
    assert_eq!(added["concurrency"]["acquirable"], false);

    tart(&["rename", &work, &base]);
    let promoted = image(port, &name).expect("the line is still listed");
    assert_eq!(promoted["base_available"], true);
    assert_eq!(promoted["concurrency"]["acquirable"], true);

    std::fs::remove_dir_all(&folder).expect("remove line");
    assert!(image(port, &name).is_none(), "the removed line is listed");

    assert_eq!(listener_pid(port), instance, "the instance was restarted");
    assert_eq!(
        listener_pid(INSTALLED_PORT),
        installed,
        "the installed daemon's process changed"
    );
}

fn uuid_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    format!("{:06x}", (nanos ^ std::process::id()) & 0xff_ffff)
}
