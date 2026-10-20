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
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
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

fn registration(node: &str, port: u16, status: &str) -> Value {
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
        let consul = Fake::new(vec![
            (
                "/v1/health/service/machine-service",
                json!([
                    registration("mac-c", closed_port(), "critical"),
                    registration("mac-a", local.port, "passing"),
                    registration("mac-b", remote.port, "passing")
                ]),
            ),
            ("/v1/agent/self", json!({"Config": {"NodeName": "mac-a"}})),
        ]);
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
        Command::new(MACHCTL)
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("MACHINE_SERVICE_PORT", self.local.port.to_string())
            .env("CONSUL_HTTP_ADDR", consul)
            .output()
            .unwrap()
    }
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

    // Push and pull by host path cannot reach another Mac (FD-4).
    let output = intranet.run(&["push", "pilot-remote-3c4d", "/tmp/a", "/tmp/b"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("FD-4"), "{}", stderr(&output));
}
