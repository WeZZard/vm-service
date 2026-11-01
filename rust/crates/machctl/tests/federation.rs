//! The real `machctl` against fake instances and a fake Consul agent
//! (TL-2; `docs/arch/federation.md` FD-2, FD-3; `docs/ux/cli/list.md` CL-1,
//! CL-3; AC-34).
//!
//! Every server here is an in-process HTTP fake on loopback. `CONSUL_HTTP_ADDR`
//! always points at a fake or at a closed port, so no test reads the real
//! Consul agent, and `MACHINE_SERVICE_PORT` always points at a fake, so no
//! test reaches an installed instance.

use std::collections::HashMap;
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

/// A fake HTTP server with fixed JSON answers per path, recording every
/// path it was asked for.
struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn new(routes: Vec<(&str, Value)>) -> Fake {
        Fake::listening("127.0.0.1:0", routes)
    }

    /// A fake on every interface, for an instance found through its Bonjour
    /// advertisement, whose address is this Mac's own network address.
    fn on_all_interfaces(routes: Vec<(&str, Value)>) -> Fake {
        Fake::listening("0.0.0.0:0", routes)
    }

    fn listening(address: &str, routes: Vec<(&str, Value)>) -> Fake {
        let server = tiny_http::Server::http(address).unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let routes: HashMap<String, String> = routes
            .into_iter()
            .map(|(path, body)| (path.to_string(), body.to_string()))
            .collect();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        thread::spawn(move || {
            for request in server.incoming_requests() {
                record.lock().unwrap().push(request.url().to_string());
                let response = match routes.get(request.url()) {
                    Some(body) => tiny_http::Response::from_string(body.clone()),
                    None => tiny_http::Response::from_string("{\"error\": \"unknown VM\"}")
                        .with_status_code(404),
                };
                let _ = request.respond(response);
            }
        });
        Fake { port, seen }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn asked(&self, path: &str) -> bool {
        self.seen.lock().unwrap().iter().any(|seen| seen == path)
    }
}

fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// A registration that states its instance ID, `node`, as every current
/// description does (FD-1, DC-80).
fn registration(node: &str, port: u16, status: &str) -> Value {
    let mut row = unstated(node, port, status);
    row["Service"]["Meta"]["instance"] = json!(node);
    row
}

/// A registration whose description predates the `instance` metadata.
fn unstated(node: &str, port: u16, status: &str) -> Value {
    json!({
        "Node": {"Node": node, "Address": "127.0.0.1"},
        "Service": {"ID": "catalog-machine-service", "Service": "machine-service",
                    "Tags": ["machines", "agents"], "Address": "127.0.0.1", "Port": port,
                    "Meta": {"loopback_only": "false"}},
        "Checks": [
            {"ServiceID": "", "Status": "passing"},
            {"ServiceID": "catalog-machine-service", "Status": status}
        ]
    })
}

/// The AC-34 fixture: the local instance `mac-a`, a reachable `mac-b`, and
/// a stopped `mac-c`, all three listed by Consul.
struct Intranet {
    local: Fake,
    remote: Fake,
    consul: Fake,
    home: tempfile::TempDir,
}

impl Intranet {
    fn new() -> Intranet {
        let local = Fake::new(vec![
            (
                "/health",
                json!({"ok": true, "service": "machine-service", "instance": "mac-a"}),
            ),
            (
                "/machines",
                json!({"machines": {
                    "pilot-mac-local-1a2b": {"state": "running", "image": "macos26",
                                             "image_kind": "macos", "purpose": "local"},
                    "shared": {"state": "running"}
                }, "catalog": {"status": "current", "generation": 1}}),
            ),
            (
                "/machines/pilot-mac-local-1a2b",
                json!({"name": "pilot-mac-local-1a2b", "state": "running", "where": "local"}),
            ),
        ]);
        let remote = Fake::new(vec![
            (
                "/health",
                json!({"ok": true, "service": "machine-service", "instance": "mac-b"}),
            ),
            (
                "/machines",
                json!({"machines": {
                    "mac-b": {"kind": "durable", "state": "available",
                              "capabilities": {"gpu": {"metal3": true},
                                               "ci_runner": {"present": true, "busy": false}}},
                    "pilot-remote-3c4d": {"kind": "virtual", "state": "running"},
                    "shared": {"kind": "virtual", "state": "running"}
                }}),
            ),
            (
                "/machines/pilot-remote-3c4d",
                json!({"name": "pilot-remote-3c4d", "state": "running", "where": "remote"}),
            ),
            (
                "/machines/shared",
                json!({"name": "shared", "state": "running", "where": "remote"}),
            ),
        ]);
        let consul = Fake::new(vec![(
            "/v1/health/service/machine-service",
            json!([
                registration("mac-c", closed_port(), "critical"),
                registration("mac-a", local.port, "passing"),
                registration("mac-b", remote.port, "passing")
            ]),
        )]);
        Intranet {
            local,
            remote,
            consul,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_consul(args, &self.consul.base())
    }

    fn run_with_consul(&self, args: &[&str], consul: &str) -> Output {
        run_machctl(args, self.home.path(), self.local.port, consul)
    }
}

fn run_machctl(args: &[&str], home: &std::path::Path, local_port: u16, consul: &str) -> Output {
    Command::new(MACHCTL)
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("MACHINE_SERVICE_PORT", local_port.to_string())
        .env("CONSUL_HTTP_ADDR", consul)
        // These fixtures stand in for a Consul agent (DC-84).
        .env("MACHINE_SERVICE_DISCOVERY", "consul")
        .output()
        .unwrap()
}

/// The local instance named `instance` by its `/health`, offering one
/// image, and a Consul agent listing `rows` built for the instance's port.
fn this_mac(instance: &str, rows: impl Fn(u16) -> Vec<Value>) -> (Fake, Fake) {
    let local = Fake::new(vec![
        (
            "/health",
            json!({"ok": true, "service": "machine-service", "instance": instance}),
        ),
        (
            "/machines",
            json!({"machines": {}, "offers": [{"name": "macos26", "kind": "virtual"}]}),
        ),
    ]);
    let consul = Fake::new(vec![(
        "/v1/health/service/machine-service",
        Value::Array(rows(local.port)),
    )]);
    (local, consul)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// AC-34: the local instance's machines first, then the reachable
/// instance's, the stopped instance as unreachable with its error and its
/// Consul health, and the local instance once although Consul lists it.
#[test]
fn ac34_list_merges_every_instance_and_keeps_the_stopped_one() {
    let intranet = Intranet::new();
    let output = intranet.run(&["list"]);
    let table = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{table}{}", stderr(&output));

    let lines: Vec<&str> = table.lines().collect();
    assert!(lines[0].starts_with("INSTANCE"), "{table}");
    let instances: Vec<&str> = lines[1..]
        .iter()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        instances,
        ["mac-a", "mac-a", "mac-b", "mac-b", "mac-b", "mac-c"],
        "{table}"
    );
    assert!(
        lines[1].contains("pilot-mac-local-1a2b") && lines[1].contains("local"),
        "{table}"
    );
    // Inside an instance the durable machine comes first.
    assert!(
        lines[3].contains("mac-b") && lines[3].contains("durable"),
        "{table}"
    );
    assert!(
        lines[3].contains("metal3") && lines[3].ends_with("idle"),
        "{table}"
    );
    assert!(lines[6].contains("unreachable"), "{table}");
    assert!(
        lines[6].contains("connection refused (Consul: critical)"),
        "{table}"
    );
    // The local instance is listed once, under its own name.
    assert_eq!(table.matches("pilot-mac-local-1a2b").count(), 1, "{table}");
    assert!(intranet.remote.asked("/machines"));
}

/// AC-34, CL-1: the JSON form is one object per instance, in the same
/// order, naming reachability, the error and the Consul health.
#[test]
fn ac34_list_json_is_one_object_per_instance() {
    let intranet = Intranet::new();
    let output = intranet.run(&["list", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    let list = list.as_array().unwrap();
    let names: Vec<&str> = list
        .iter()
        .map(|i| i["instance"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mac-a", "mac-b", "mac-c"]);
    assert_eq!(list[0]["local"], true);
    assert_eq!(list[0]["consul"], "passing");
    assert_eq!(list[0]["machines"].as_array().unwrap().len(), 2);
    assert_eq!(list[1]["reachable"], true);
    assert_eq!(list[1]["machines"][0]["name"], "mac-b");
    assert_eq!(list[2]["reachable"], false);
    assert_eq!(list[2]["error"], "connection refused");
    assert_eq!(list[2]["consul"], "critical");
    assert_eq!(list[2]["machines"], json!([]));
}

/// CL-1: `--local` never asks Consul; `--instance` lists one instance;
/// `--kind` and `--require` filter machines.
#[test]
fn list_filters() {
    let intranet = Intranet::new();
    let output = intranet.run(&["list", "--local"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!stdout(&output).contains("mac-b"));
    assert!(!intranet.consul.asked("/v1/health/service/machine-service"));

    let output = intranet.run(&["list", "--instance", "mac-b"]);
    let table = stdout(&output);
    assert!(
        table.contains("pilot-remote-3c4d") && !table.contains("mac-a"),
        "{table}"
    );

    let output = intranet.run(&["list", "--kind", "durable", "--require", "metal3"]);
    let table = stdout(&output);
    let rows: Vec<&str> = table.lines().skip(1).collect();
    assert_eq!(rows.len(), 2, "{table}");
    assert!(
        rows[0].starts_with("mac-b") && rows[0].contains("durable"),
        "{table}"
    );
    assert!(
        rows[1].starts_with("mac-c") && rows[1].contains("unreachable"),
        "{table}"
    );

    let output = intranet.run(&["list", "--require", "metal"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("unknown capability"),
        "{}",
        stderr(&output)
    );
}

/// FD-2: without a Consul agent the local instance still lists, and the
/// missing registry is said on stderr, never shown as an empty intranet.
#[test]
fn list_without_consul_warns_and_lists_the_local_instance() {
    let intranet = Intranet::new();
    let consul = format!("127.0.0.1:{}", closed_port());
    let output = intranet.run_with_consul(&["list"], &consul);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("pilot-mac-local-1a2b"));
    let errors = stderr(&output);
    assert!(
        errors.contains("Consul at http://127.0.0.1:") && errors.contains("connection refused"),
        "{errors}"
    );
}

/// FD-3: when no instance answers the table still names them, and the CLI
/// exits 2 with the start hint.
#[test]
fn list_with_no_instance_answering_exits_2() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(MACHCTL)
        .args(["list"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("MACHINE_SERVICE_PORT", closed_port().to_string())
        .env("CONSUL_HTTP_ADDR", format!("127.0.0.1:{}", closed_port()))
        .env("MACHINE_SERVICE_DISCOVERY", "consul")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stdout(&output).contains("unreachable  connection refused"),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("start it with"),
        "{}",
        stderr(&output)
    );
}

/// CL-3: a lease command goes to the instance that owns the machine, a
/// name two instances list must be qualified, and `<vm>@<instance>` names
/// the owner.
#[test]
fn cl3_lease_commands_go_to_the_owner() {
    let intranet = Intranet::new();

    let output = intranet.run(&["status", "pilot-remote-3c4d"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["where"], "remote");

    let output = intranet.run(&["status", "pilot-mac-local-1a2b"]);
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["where"], "local");

    let output = intranet.run(&["status", "shared"]);
    assert_eq!(output.status.code(), Some(1));
    let errors = stderr(&output);
    assert!(
        errors.contains("shared exists on mac-a and mac-b") && errors.contains("shared@<instance>"),
        "{errors}"
    );

    let output = intranet.run(&["status", "shared@mac-b"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["where"], "remote");

    // An unknown name stays with the local instance, whose answer says so.
    let output = intranet.run(&["status", "ghost"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("unknown VM"),
        "{}",
        stderr(&output)
    );
    assert!(intranet.local.asked("/machines/ghost"));

    // Push to another Mac sends the bytes to the owner's file endpoint
    // (FD-4); this fake has none, and the failure says so.
    let file = intranet.home.path().join("a.txt");
    std::fs::write(&file, "bytes").unwrap();
    let output = intranet.run(&[
        "push",
        "pilot-remote-3c4d",
        file.to_str().unwrap(),
        "/tmp/b",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("push to pilot-remote-3c4d failed: HTTP 404"),
        "{}",
        stderr(&output)
    );
    assert!(intranet
        .remote
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|seen| seen.starts_with("/machines/pilot-remote-3c4d/files?path=/tmp/b")));
}

/// AC-47, DC-80: 192.168.50.3's Consul node is `mac-mini-m4-pro` and its
/// own name `Mac-mini`. Its registration states `Mac-mini`, so the list
/// shows it once, under its own name, with its Consul health, and says
/// nothing about a fallback.
#[test]
fn ac47_a_mac_whose_node_name_differs_is_listed_once_by_its_own_name() {
    let (local, consul) = this_mac("Mac-mini", |port| {
        let mut row = unstated("mac-mini-m4-pro", port, "passing");
        row["Service"]["Meta"]["instance"] = json!("Mac-mini");
        vec![row]
    });
    let home = tempfile::tempdir().unwrap();

    let output = run_machctl(&["list"], home.path(), local.port, &consul.base());
    let table = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{table}{}", stderr(&output));
    let rows: Vec<&str> = table.lines().skip(1).collect();
    assert_eq!(rows.len(), 1, "{table}");
    assert!(
        rows[0].starts_with("Mac-mini ") && rows[0].contains("macos26"),
        "{table}"
    );
    assert!(!table.contains("mac-mini-m4-pro"), "{table}");
    assert!(!stderr(&output).contains("note:"), "{}", stderr(&output));

    let output = run_machctl(&["list", "--json"], home.path(), local.port, &consul.base());
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["instance"], "Mac-mini");
    assert_eq!(list[0]["local"], true);
    assert_eq!(list[0]["consul"], "passing");
}

/// AC-47, DC-80: a registration that states no instance ID falls back to
/// its node name, and the CLI says so on standard error. Where the node
/// name is the Mac's name it is still listed once; where it is not, the
/// fallback cannot tell, and the note is what explains the second row.
#[test]
fn ac47_a_registration_without_metadata_falls_back_to_the_node_name_and_says_so() {
    let home = tempfile::tempdir().unwrap();

    let (local, consul) = this_mac("mac-a", |port| vec![unstated("mac-a", port, "passing")]);
    let output = run_machctl(&["list", "--json"], home.path(), local.port, &consul.base());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|instance| instance["instance"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mac-a"]);
    assert_eq!(list[0]["consul"], "passing");
    let errors = stderr(&output);
    assert!(
        errors.contains("machctl: note: the registration on Consul node mac-a")
            && errors.contains("states no instance ID")
            && errors.contains("catalog-description"),
        "{errors}"
    );

    let (local, consul) = this_mac("Mac-mini", |port| {
        vec![unstated("mac-mini-m4-pro", port, "passing")]
    });
    let output = run_machctl(&["list", "--json"], home.path(), local.port, &consul.base());
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2, "{list:?}");
    assert!(
        stderr(&output).contains("Consul node mac-mini-m4-pro"),
        "{}",
        stderr(&output)
    );
}

/// An instance that advertises itself on this Mac only, through the Mac's
/// own DNS-SD service, so nothing is sent on any network (DC-85). The fake
/// answers `/health` with `answers` and offers one durable machine.
fn advertised(advertise_as: &str, answers: &str) -> (discovery::dnssd::Advertisement, Fake) {
    let fake = Fake::on_all_interfaces(vec![
        (
            "/health",
            json!({"ok": true, "service": "machine-service", "instance": answers, "kinds": ["durable"]}),
        ),
        (
            "/machines",
            json!({"machines": {answers: {"kind": "durable", "state": "available", "name": answers}}}),
        ),
    ]);
    let advertisement = discovery::dnssd::register(
        advertise_as,
        discovery::SERVICE_TYPE,
        fake.port,
        &discovery::txt_entries(advertise_as, "", &["durable"]),
        discovery::dnssd::Scope::LocalOnly,
        std::time::Duration::from_secs(5),
    )
    .expect("register a local-only advertisement");
    (advertisement, fake)
}

/// `machctl list --json` with the discovery mode `mode` (`None` for the
/// default), no local instance, and a Consul agent that records what it was
/// asked.
fn list_json(mode: Option<&str>, consul: &Fake) -> (Vec<Value>, String) {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::new(MACHCTL);
    command
        .args(["list", "--json"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("MACHINE_SERVICE_PORT", closed_port().to_string())
        .env("CONSUL_HTTP_ADDR", consul.base());
    if let Some(mode) = mode {
        command.env("MACHINE_SERVICE_DISCOVERY", mode);
    }
    let output = command.output().unwrap();
    let listed: Vec<Value> = serde_json::from_str(&stdout(&output)).unwrap_or_default();
    (listed, stderr(&output))
}

fn instance<'a>(listed: &'a [Value], name: &str) -> Option<&'a Value> {
    listed.iter().find(|instance| instance["instance"] == name)
}

/// AC-48: with the default mode and no Consul agent asked, `machctl list`
/// lists an instance that advertises itself, under its own ID, once its
/// `/health` confirms that ID.
#[cfg(target_os = "macos")]
#[test]
fn ac48_an_advertised_instance_is_listed_without_consul() {
    let name = format!("ac48-{}", std::process::id());
    let (_advertisement, _fake) = advertised(&name, &name);
    let consul = Fake::new(vec![]);
    let (listed, errors) = list_json(None, &consul);
    let found =
        instance(&listed, &name).unwrap_or_else(|| panic!("{name} not in {listed:?}\n{errors}"));
    assert_eq!(found["found_by"], "bonjour", "{found}");
    assert_eq!(found["reachable"], true, "{found}");
    assert_eq!(found["machines"][0]["name"], name.as_str(), "{found}");
    assert!(
        !consul.asked("/v1/health/service/machine-service"),
        "Consul was asked"
    );
}

/// AC-48, FD-2: an advertisement whose `/health` answers another ID is
/// listed as unreachable with both IDs, never used.
#[cfg(target_os = "macos")]
#[test]
fn ac48_an_advertisement_whose_health_answers_another_id_is_not_used() {
    let name = format!("ac48-mismatch-{}", std::process::id());
    let (_advertisement, _fake) = advertised(&name, "someone-else");
    let (listed, errors) = list_json(None, &Fake::new(vec![]));
    let found =
        instance(&listed, &name).unwrap_or_else(|| panic!("{name} not in {listed:?}\n{errors}"));
    assert_eq!(found["reachable"], false, "{found}");
    let error = found["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(&format!("found as {name}")) && error.contains("someone-else"),
        "{error}"
    );
    assert_eq!(found["machines"], json!([]), "{found}");
}

/// AC-49: in Consul mode the CLI asks Consul and lists no advertised
/// instance that Consul does not list.
#[cfg(target_os = "macos")]
#[test]
fn ac49_consul_mode_asks_only_consul() {
    let name = format!("ac49-{}", std::process::id());
    let (_advertisement, _fake) = advertised(&name, &name);
    let consul = Fake::new(vec![("/v1/health/service/machine-service", json!([]))]);
    let (listed, errors) = list_json(Some("consul"), &consul);
    assert!(instance(&listed, &name).is_none(), "{listed:?}\n{errors}");
    assert!(consul.asked("/v1/health/service/machine-service"));
}

/// DC-84: an unknown mode is refused with the two values.
#[test]
fn an_unknown_discovery_mode_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(MACHCTL)
        .args(["list"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("MACHINE_SERVICE_PORT", closed_port().to_string())
        .env("MACHINE_SERVICE_DISCOVERY", "mdns")
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("MACHINE_SERVICE_DISCOVERY must be one of bonjour, consul"),
        "{}",
        stderr(&output)
    );
}
