//! `machine-service-install catalog-description` prints the description the
//! service-catalog registrar publishes, tagged for the Mac it runs on (FD-1,
//! AC-37). Consul itself is never involved: AC-37's live half, the tags as
//! Consul shows them on each Mac, is checked when the files are placed.
//!
//! Every test runs the built installer against an isolated `HOME` with a
//! fake Tart and the `launchctl` stub. No test touches a real LaunchAgent or
//! the registrar's folder.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{write_exec, Harness};
use serde_json::{json, Value};

/// A fake Tart that answers `--version` and lists `bases` as local VMs.
fn fake_tart(harness: &Harness, bases: &[&str]) -> PathBuf {
    let mut rows = String::from("Source Name Disk Size Accessed State\\n");
    for base in bases {
        rows.push_str(&format!("local {base} 50 20 2 days ago stopped\\n"));
    }
    let tart = harness.home.join("tart-bin/tart");
    fs::create_dir_all(tart.parent().unwrap()).unwrap();
    write_exec(
        &tart,
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) printf '2.32.1\\n' ;;\n  list) printf '{rows}' ;;\n  *) exit 99 ;;\nesac\n"
        ),
    );
    tart
}

/// A pilot-images checkout with one valid line whose base is `base`.
fn pilot_repo(root: &Path, base: &str) -> PathBuf {
    let repo = root.join("pilot-images");
    let line = repo.join("images/macos26");
    fs::create_dir_all(&line).unwrap();
    fs::write(
        line.join("line.conf"),
        format!(
            "LINE_KIND=macos\nBASE_VM={base}\nCLONE_PREFIX=pilot-mac\nGUEST_USER=admin\nGUEST_PASS=admin\n"
        ),
    )
    .unwrap();
    repo
}

/// Write an installed machine-service LaunchAgent with `environment`.
fn install_agent(harness: &Harness, environment: &[(&str, &str)]) {
    let mut env = serde_json::Map::new();
    for (key, value) in environment {
        env.insert(key.to_string(), json!(value));
    }
    let value = json!({
        "Label": "com.wezzard.machine-service",
        "EnvironmentVariables": Value::Object(env),
        "ProgramArguments": [harness.bin.join("machine-service").to_str().unwrap()],
    });
    fs::create_dir_all(harness.plist.parent().unwrap()).unwrap();
    fs::write(
        &harness.plist,
        machine_service_install::plist::dumps(&value),
    )
    .unwrap();
}

/// Run `catalog-description` and return the parsed description and stderr.
fn describe(harness: &Harness, extra: &[&str]) -> (Value, String) {
    let mut args = vec!["catalog-description"];
    args.extend_from_slice(extra);
    let output = harness.run(&args);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    let description = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not one JSON description: {error}\n{stderr}"));
    (description, stderr)
}

fn tags(description: &Value) -> Vec<String> {
    description["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag.as_str().unwrap().to_string())
        .collect()
}

/// AC-54/MI-4/DC-87: the real read-only installer command publishes the
/// same hardware identity as the daemon and stores no host-name label.
#[test]
fn ac54_catalog_command_publishes_hardware_identity_without_name() {
    let harness = Harness::new();
    let (description, stderr) = describe(&harness, &["--expose-durable", "on"]);
    let machine_id = machine_service_core::durable::capabilities::hardware_uuid().unwrap();
    assert_eq!(description["meta"], json!({"machine_id": machine_id}));
    assert!(description["meta"].get("instance").is_none());
    assert!(stderr.contains("meta.machine_id"));
}

/// AC-37: a Mac with Tart, a configured line and its base VM, and durable
/// exposure off, is tagged `vm` and `tart` and not `durable`.
#[test]
fn ac37_tart_mac_is_tagged_vm_and_tart_without_durable() {
    let harness = Harness::new();
    let tart = fake_tart(&harness, &["pilot-base"]);
    let repo = pilot_repo(&harness.home, "pilot-base");
    install_agent(
        &harness,
        &[
            ("HOME", harness.home_str()),
            ("PATH", "/usr/bin:/bin"),
            ("TART", tart.to_str().unwrap()),
            ("PILOT_REPO", repo.to_str().unwrap()),
            ("MACHINE_SERVICE_HOST", "0.0.0.0"),
        ],
    );

    let (description, stderr) = describe(&harness, &[]);
    let tags = tags(&description);
    for tag in ["machines", "agents", "vm", "tart"] {
        assert!(tags.contains(&tag.to_string()), "{tags:?}");
    }
    assert!(!tags.contains(&"durable".to_string()), "{tags:?}");
    assert_eq!(description["id"], "machine-service");
    assert_eq!(description["port"], 6240);
    assert!(stderr.contains("durable exposure: off"), "{stderr}");
    assert!(stderr.contains("pilot-base"), "{stderr}");
    assert!(!stderr.contains("loopback_only"), "{stderr}");
    // MI-4/DC-87: the description uses the daemon's hardware identity,
    // independent of the current host label and Consul node name.
    assert_eq!(
        description["meta"],
        json!({"machine_id": machine_service_core::durable::capabilities::hardware_uuid().unwrap()})
    );
}

/// AC-37: a Mac without Tart whose installed agent turns durable exposure
/// on is tagged `durable` and not `vm`.
#[test]
fn ac37_durable_mac_without_tart_is_tagged_durable_without_vm() {
    let harness = Harness::new();
    install_agent(
        &harness,
        &[
            ("HOME", harness.home_str()),
            ("PATH", "/usr/bin:/bin"),
            ("MACHINE_SERVICE_HOST", "0.0.0.0"),
            ("MACHINE_SERVICE_EXPOSE_DURABLE", "on"),
        ],
    );

    let (description, stderr) = describe(&harness, &[]);
    assert_eq!(tags(&description), ["machines", "agents", "durable"]);
    assert!(
        stderr.contains("durable exposure: on (from the installed LaunchAgent)"),
        "{stderr}"
    );
    assert!(stderr.contains("Tart is not installed"), "{stderr}");
}

/// AC-37: Tart alone is not enough. A configured line whose base VM is
/// missing from the Tart store leaves the instance unable to create virtual
/// machines (MM-1), so it carries no `vm` tag.
#[test]
fn ac37_missing_base_vm_drops_the_vm_tags() {
    let harness = Harness::new();
    let tart = fake_tart(&harness, &["some-other-vm"]);
    let repo = pilot_repo(&harness.home, "pilot-base");
    install_agent(
        &harness,
        &[
            ("HOME", harness.home_str()),
            ("PATH", "/usr/bin:/bin"),
            ("TART", tart.to_str().unwrap()),
            ("PILOT_REPO", repo.to_str().unwrap()),
        ],
    );

    let (description, stderr) = describe(&harness, &[]);
    assert_eq!(tags(&description), ["machines", "agents"]);
    assert!(stderr.contains("no configured base VM"), "{stderr}");
    // The installed agent binds loopback, which other Macs cannot reach.
    assert!(stderr.contains("loopback_only"), "{stderr}");
}

/// AC-37, DC-60: `--expose-durable` describes the Mac with that setting
/// instead of the installed one, in both directions.
#[test]
fn ac37_expose_durable_argument_overrides_the_installed_setting() {
    let harness = Harness::new();
    install_agent(
        &harness,
        &[
            ("HOME", harness.home_str()),
            ("PATH", "/usr/bin:/bin"),
            ("MACHINE_SERVICE_EXPOSE_DURABLE", "off"),
        ],
    );
    let (description, stderr) = describe(&harness, &["--expose-durable", "on"]);
    assert!(tags(&description).contains(&"durable".to_string()));
    assert!(stderr.contains("from --expose-durable"), "{stderr}");

    install_agent(
        &harness,
        &[
            ("HOME", harness.home_str()),
            ("PATH", "/usr/bin:/bin"),
            ("MACHINE_SERVICE_EXPOSE_DURABLE", "on"),
        ],
    );
    let (description, _) = describe(&harness, &["--expose-durable", "off"]);
    assert!(!tags(&description).contains(&"durable".to_string()));
}

/// FD-1: the command reads only. It writes no plist and no description
/// file, and it calls neither launchctl nor anything but `tart list`.
#[test]
fn ac37_catalog_description_reads_only() {
    let harness = Harness::new();
    let tart = fake_tart(&harness, &["pilot-base"]);
    let repo = pilot_repo(&harness.home, "pilot-base");
    install_agent(
        &harness,
        &[
            ("HOME", harness.home_str()),
            ("PATH", "/usr/bin:/bin"),
            ("TART", tart.to_str().unwrap()),
            ("PILOT_REPO", repo.to_str().unwrap()),
        ],
    );
    let before = fs::read(&harness.plist).unwrap();

    describe(&harness, &[]);

    assert_eq!(fs::read(&harness.plist).unwrap(), before);
    assert!(!harness.log.exists(), "launchctl was called");
    assert!(!harness.home.join(".config/service-catalog").exists());
}

/// A typo in the installed exposure setting is refused, never read as on
/// or off (DC-39).
#[test]
fn installed_exposure_typo_is_refused() {
    let harness = Harness::new();
    install_agent(
        &harness,
        &[
            ("HOME", harness.home_str()),
            ("MACHINE_SERVICE_EXPOSE_DURABLE", "yes"),
        ],
    );
    let stderr = harness.assert_refused(&["catalog-description"]);
    assert!(
        stderr.contains("MACHINE_SERVICE_EXPOSE_DURABLE") && stderr.contains("\"yes\""),
        "{stderr}"
    );
}

/// Without an installed agent the defaults are described, and the note
/// says so.
#[test]
fn no_installed_agent_describes_the_defaults() {
    let harness = Harness::new();
    let (description, stderr) = describe(&harness, &[]);
    assert!(stderr.contains("no LaunchAgent"), "{stderr}");
    assert!(!tags(&description).contains(&"durable".to_string()));
}

/// MM-1: `install --expose-durable on` writes the variable, and a later
/// install without the argument keeps it.
#[test]
fn install_writes_and_preserves_durable_exposure() {
    let harness = Harness::new();
    let output = harness.run(&["install", "--expose-durable", "on"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.saved()["EnvironmentVariables"]["MACHINE_SERVICE_EXPOSE_DURABLE"],
        json!("on")
    );

    let output = harness.run(&["install"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.saved()["EnvironmentVariables"]["MACHINE_SERVICE_EXPOSE_DURABLE"],
        json!("on")
    );
}
