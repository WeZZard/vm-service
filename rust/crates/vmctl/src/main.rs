//! `vmctl` — CLI client for `vm-service` (same code path as the HTTP API).
//!
//! This is the Rust port of `bin/vmctl`. It preserves the observable CLI
//! contract: subcommands, flags and defaults, human-readable formatting,
//! `--json` output, exit codes, and the `vmctl: ...` diagnostics on stderr.
//! Agents should prefer the HTTP API (localhost:6240); this CLI exists for
//! humans and for shell-based agents.

mod argparse_error;
mod argparse_help;
mod textwrap;

use std::fs;
use std::io::Write;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use serde_json::{Map, Value};

/// Default loopback port used when `VM_SERVICE_PORT` is unset.
const DEFAULT_PORT: &str = "6240";

/// Default per-request timeout in seconds, matching Python `call(timeout=3600)`.
const DEFAULT_TIMEOUT_S: u64 = 3600;

// `argparse` has no `help` subcommand: `vmctl help` is an invalid choice that
// exits 2, so clap's built-in `help` subcommand must stay off to keep the
// command surface identical.
#[derive(Parser, Debug)]
#[command(
    name = "vmctl",
    about = "client for vm-service (Tart VM leases)",
    disable_help_subcommand = true,
    infer_long_args = true
)]
struct Cli {
    /// absolute selected-environment profile path
    #[arg(long, value_name = "ENVIRONMENT")]
    environment: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// resolve environment offline without writing files
    Environment {
        #[arg(long)]
        json: bool,
    },
    /// clone + boot a fresh VM and lease it
    Acquire(AcquireArgs),
    /// block until a lease is running (useful after --no-wait)
    Wait(WaitArgs),
    /// list leases
    List {
        /// machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// query available images and capacity
    Images {
        /// machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// show one image's full record (JSON)
    #[command(name = "images-show")]
    ImagesShow { image: String },
    /// show one lease
    Status { vm: String },
    /// run a command in the guest
    Exec(ExecArgs),
    /// copy host -> guest
    Push {
        vm: String,
        local: String,
        remote: String,
    },
    /// copy guest -> host
    Pull {
        vm: String,
        remote: String,
        local: String,
    },
    /// reset TTL; keeps a leased VM alive
    Heartbeat {
        vm: String,
        #[arg(long)]
        ttl_hours: Option<f64>,
    },
    /// stop + delete + unregister
    Release { vm: String },
    /// show supported and unavailable network capabilities
    Capabilities,
    /// describe acquisition and console prerequisites without allocating a VM
    AcquisitionCapabilities,
    /// resolve the exact lease console; viewer is on the service host
    #[command(name = "console-resolve")]
    ConsoleResolve(ConsoleResolveArgs),
    /// open the exact lease console; viewer is on the service host
    #[command(name = "console-open")]
    ConsoleOpen(ConsoleArgs),
    /// cancel the exact lease console; viewer is on the service host
    #[command(name = "console-cancel")]
    ConsoleCancel(ConsoleArgs),
    /// run TTL reclaim now
    Gc,
}

#[derive(Args, Debug)]
struct AcquireArgs {
    /// task name; becomes part of the VM name
    #[arg(long)]
    purpose: String,
    /// image to clone (see: vmctl images; default: macos26)
    #[arg(long, default_value = "macos26", alias = "line")]
    image: String,
    /// default | none | <env-pack-name> (credential env vars)
    #[arg(long, default_value = "default", alias = "pack")]
    env: String,
    #[arg(long, value_parser = ["nat", "control-only"], default_value = "nat")]
    network: String,
    /// explicit control-only helper profile
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    ttl_hours: Option<f64>,
    #[arg(long)]
    cpu: Option<i64>,
    #[arg(long)]
    memory_mb: Option<i64>,
    #[arg(long)]
    disk_gb: Option<i64>,
    /// use shorter synchronous SSH readiness checks; transfers are still verified
    #[arg(long)]
    no_wait: bool,
    /// prepare guest sharing without opening a viewer; requires configured console support
    #[arg(long)]
    vnc: bool,
    /// work is explicitly for fresh-clone acceptance of configured WORK_VM; requires --env none
    #[arg(long, value_parser = ["base", "work"], default_value = "base")]
    source: String,
    /// JSON fingerprint file to require before cloning a work image
    #[arg(long)]
    expected_source_fingerprint: Option<String>,
}

#[derive(Args, Debug)]
struct WaitArgs {
    vm: String,
    /// give up after N seconds (default: 900)
    #[arg(long, default_value_t = 900)]
    timeout_s: i64,
}

#[derive(Args, Debug)]
struct ExecArgs {
    vm: String,
    /// run a local script file via guest shell stdin
    #[arg(long)]
    script: Option<String>,
    /// guest command timeout in seconds; transport allows an additional 30 seconds
    #[arg(long, default_value_t = 600)]
    timeout: i64,
    /// command to run (use -- to separate)
    #[arg(value_name = "ARGV", num_args = 0..)]
    argv: Vec<String>,
}

#[derive(Args, Debug)]
struct ConsoleResolveArgs {
    vm: String,
    #[arg(long)]
    lease_id: String,
}

#[derive(Args, Debug)]
struct ConsoleArgs {
    vm: String,
    #[arg(long)]
    lease_id: String,
    #[arg(long)]
    console_id: String,
    #[arg(long)]
    attempt_id: String,
}

/// HTTP JSON client for the loopback service.
struct Client<'a> {
    base: String,
    environment: Option<&'a Value>,
}

impl<'a> Client<'a> {
    fn new(base: String, environment: Option<&'a Value>) -> Self {
        Self { base, environment }
    }

    /// The selected-environment fingerprint, when one is selected.
    fn fingerprint(&self) -> Option<&str> {
        self.environment
            .and_then(|env| env.get("identity"))
            .and_then(|identity| identity.get("fingerprint"))
            .and_then(Value::as_str)
    }

    /// Perform one request and return `(status, parsed_body)`.
    ///
    /// Mirrors Python `call()`: mutations first pre-check the service identity
    /// through `GET /health`, transport failures exit 2 with the restart hint,
    /// and any HTTP status (including 4xx/5xx) returns the parsed body.
    fn call(&self, method: &str, path: &str, body: Option<&Value>, timeout_s: u64) -> (u16, Value) {
        if self.environment.is_some() && method != "GET" {
            let (status, health) = self.call("GET", "/health", None, 10);
            let actual = health
                .get("environment")
                .and_then(|environment| environment.get("fingerprint"))
                .and_then(Value::as_str);
            if status != 200 || actual.is_none() || actual != self.fingerprint() {
                eprintln!("vmctl: selected environment identity mismatch; refusing mutation");
                std::process::exit(2);
            }
        }

        let url = format!("{}{}", self.base, path);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout_s)))
            .http_status_as_error(false)
            .build()
            .into();

        let response = if method == "GET" {
            let mut request = agent
                .get(url.as_str())
                .header("Content-Type", "application/json");
            if let Some(fingerprint) = self.fingerprint() {
                request = request.header("X-VM-Environment-Fingerprint", fingerprint);
            }
            request.call()
        } else {
            let mut request = agent
                .post(url.as_str())
                .header("Content-Type", "application/json");
            if let Some(fingerprint) = self.fingerprint() {
                request = request.header("X-VM-Environment-Fingerprint", fingerprint);
            }
            let data = match body {
                Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
                None => String::new(),
            };
            request.send(data)
        };

        match response {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let text = response.body_mut().read_to_string().unwrap_or_default();
                (status, parse_response_body(&text))
            }
            Err(error) => {
                eprintln!(
                    "vmctl: cannot reach vm-service at {} ({})",
                    self.base,
                    error_reason(&error)
                );
                eprintln!(
                    "start it with: vm-service (launchd) or: ~/Artifacts/Repositories/com.github/WeZZard/vm-service/bin/vm-service"
                );
                std::process::exit(2);
            }
        }
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // `argparse` renders `--help` itself, with a different layout from clap's.
    // Intercept every help request before clap parses and emit the argparse
    // rendering, leaving clap in charge of parsing and of all error paths.
    let command = <Cli as clap::CommandFactory>::command();
    let root = argparse_help::model_from_command(&command);
    if let Some(help) = argparse_help::intercept_help(&root, &arguments) {
        print!("{help}");
        std::process::exit(0);
    }

    let (arguments, command_argv) = split_exec_arguments(arguments);

    let cli =
        match Cli::try_parse_from(std::iter::once("vmctl".to_string()).chain(arguments.clone())) {
            Ok(cli) => cli,
            Err(error) => argparse_error::exit_with(&root, &arguments, &error),
        };
    let mut cli = cli;
    if let (Some(argv), Cmd::Exec(exec)) = (command_argv, &mut cli.cmd) {
        exec.argv = argv;
    }

    let environment = match environment::load_environment(cli.environment.as_deref()) {
        Ok(environment) => environment,
        Err(error) => usage_error(&error.to_string()),
    };

    if cli.environment.is_none() {
        if let Ok(bound) = std::env::var("VM_ENVIRONMENT_FINGERPRINT") {
            let unchanged = environment
                .as_ref()
                .and_then(|env| env.get("identity"))
                .and_then(|identity| identity.get("fingerprint"))
                .and_then(Value::as_str)
                .map(|fingerprint| fingerprint == bound)
                .unwrap_or(false);
            if !unchanged {
                usage_error("selected environment changed during an active command sequence");
            }
        }
    }

    let mut base = format!(
        "http://127.0.0.1:{}",
        std::env::var("VM_SERVICE_PORT").unwrap_or_else(|_| DEFAULT_PORT.to_string())
    );
    if let Some(url) = environment
        .as_ref()
        .and_then(|env| env.get("profile"))
        .and_then(|profile| profile.get("vmServiceUrl"))
        .and_then(Value::as_str)
    {
        base = url.to_string();
    }

    let client = Client::new(base, environment.as_ref());

    match cli.cmd {
        Cmd::Environment { .. } => match &environment {
            Some(bundle) => println!("{}", pretty_unicode(bundle)),
            None => {
                let mut bundle = Map::new();
                bundle.insert("profile".to_string(), Value::Null);
                bundle.insert("identity".to_string(), Value::Null);
                bundle.insert("environment".to_string(), Value::Object(Map::new()));
                println!("{}", pretty(&Value::Object(bundle)));
            }
        },

        Cmd::Capabilities => {
            let (status, resp) = client.call("GET", "/capabilities", None, DEFAULT_TIMEOUT_S);
            if status != 200 {
                die_from(status, &resp, None);
            }
            println!("{}", pretty(&resp));
        }

        Cmd::AcquisitionCapabilities => {
            let (status, resp) =
                client.call("GET", "/acquisition-capabilities", None, DEFAULT_TIMEOUT_S);
            if status != 200 {
                die_from(status, &resp, None);
            }
            println!("{}", pretty(&resp));
        }

        Cmd::ConsoleResolve(args) => {
            let mut body = Map::new();
            body.insert("lease_id".to_string(), Value::String(args.lease_id));
            run_console(&client, &args.vm, "resolve", body);
        }

        Cmd::ConsoleOpen(args) => {
            let mut body = Map::new();
            body.insert("lease_id".to_string(), Value::String(args.lease_id));
            body.insert("console_id".to_string(), Value::String(args.console_id));
            body.insert("attempt_id".to_string(), Value::String(args.attempt_id));
            run_console(&client, &args.vm, "open", body);
        }

        Cmd::ConsoleCancel(args) => {
            let mut body = Map::new();
            body.insert("lease_id".to_string(), Value::String(args.lease_id));
            body.insert("console_id".to_string(), Value::String(args.console_id));
            body.insert("attempt_id".to_string(), Value::String(args.attempt_id));
            run_console(&client, &args.vm, "cancel", body);
        }

        Cmd::Acquire(args) => {
            if args.network == "control-only" && args.env != "none" {
                usage_error("control-only requires --env none");
            }
            let mut body = Map::new();
            body.insert("purpose".to_string(), Value::String(args.purpose));
            body.insert("image".to_string(), Value::String(args.image));
            body.insert("env".to_string(), Value::String(args.env.clone()));
            body.insert("ttl_hours".to_string(), acquire_ttl_hours(args.ttl_hours));
            body.insert("network".to_string(), Value::String(args.network));
            if args.vnc {
                body.insert("vnc".to_string(), Value::Bool(true));
            }
            if args.source != "base" {
                if args.env != "none" {
                    usage_error("--source work requires --env none");
                }
                body.insert("source".to_string(), Value::String(args.source));
            }
            if let Some(path) = args
                .expected_source_fingerprint
                .as_deref()
                .filter(|path| !path.is_empty())
            {
                if !body
                    .get("source")
                    .and_then(Value::as_str)
                    .map(|source| source == "work")
                    .unwrap_or(false)
                {
                    usage_error("--expected-source-fingerprint requires --source work");
                }
                let text = match fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(error) => {
                        eprintln!("vmctl: {error}");
                        std::process::exit(1);
                    }
                };
                let fingerprint = match serde_json::from_str::<Value>(&text) {
                    Ok(fingerprint) => fingerprint,
                    Err(error) => {
                        eprintln!("vmctl: {error}");
                        std::process::exit(1);
                    }
                };
                body.insert("expected_source_fingerprint".to_string(), fingerprint);
            }
            if let Some(profile) = args.profile {
                body.insert("profile".to_string(), Value::String(profile));
            }
            for (key, value) in [
                ("cpu", args.cpu),
                ("memory_mb", args.memory_mb),
                ("disk_gb", args.disk_gb),
            ] {
                if let Some(value) = value {
                    body.insert(key.to_string(), json_int(value));
                }
            }
            if args.no_wait {
                body.insert("wait".to_string(), Value::Bool(false));
            }
            let (status, resp) = client.call(
                "POST",
                "/acquire",
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, None);
            }
            println!("{}", pretty(&resp));
        }

        Cmd::Wait(args) => {
            let deadline = Instant::now()
                + Duration::from_secs(if args.timeout_s > 0 {
                    args.timeout_s as u64
                } else {
                    0
                });
            loop {
                let (status, resp) =
                    client.call("GET", &format!("/vms/{}", args.vm), None, DEFAULT_TIMEOUT_S);
                if status == 200 && resp.get("state").and_then(Value::as_str) == Some("running") {
                    println!("{}", pretty(&resp));
                    return;
                }
                if status == 404 {
                    die_from(status, &resp, Some(&args.vm));
                }
                if Instant::now() >= deadline {
                    let state = if status == 200 {
                        match resp.get("state") {
                            Some(state) => python_str(state),
                            None => "?".to_string(),
                        }
                    } else {
                        format!("HTTP {status}")
                    };
                    eprintln!(
                        "vmctl: {} not running after {}s (state: {state}); check ~/.local/state/vm-service/service.log and /tmp/tart-run-{}.log",
                        args.vm, args.timeout_s, args.vm
                    );
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }

        Cmd::Images { json } => {
            let (_status, resp) = client.call("GET", "/images", None, DEFAULT_TIMEOUT_S);
            if json {
                println!("{}", pretty(&resp));
            } else {
                print!("{}", format_images(&resp));
            }
        }

        Cmd::ImagesShow { image } => {
            let (_status, resp) = client.call("GET", "/images", None, DEFAULT_TIMEOUT_S);
            let found = resp
                .get("images")
                .and_then(Value::as_object)
                .and_then(|images| images.get(&image))
                .filter(|record| !is_falsy(record));
            match found {
                Some(record) => println!("{}", pretty(record)),
                None => {
                    eprintln!("{}", format_unknown_image(&image, &resp));
                    std::process::exit(1);
                }
            }
        }

        Cmd::List { json } => {
            let (_status, resp) = client.call("GET", "/vms", None, DEFAULT_TIMEOUT_S);
            if json {
                println!("{}", pretty(&resp));
            } else {
                print!("{}", format_leases(&resp));
            }
        }

        Cmd::Status { vm } => {
            let (status, resp) = client.call("GET", &format!("/vms/{vm}"), None, DEFAULT_TIMEOUT_S);
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!("{}", pretty(&resp));
        }

        Cmd::Exec(args) => {
            let mut body = Map::new();
            if let Some(script) = args.script.as_deref().filter(|script| !script.is_empty()) {
                let contents = match fs::read_to_string(script) {
                    Ok(contents) => contents,
                    Err(error) => {
                        eprintln!("vmctl: {error}");
                        std::process::exit(1);
                    }
                };
                body.insert("script".to_string(), Value::String(contents));
            } else if !args.argv.is_empty() {
                body.insert(
                    "argv".to_string(),
                    Value::Array(args.argv.iter().cloned().map(Value::String).collect()),
                );
            } else {
                usage_error("exec needs a command after -- or --script FILE");
            }
            if args.timeout <= 0 {
                usage_error("--timeout must be positive");
            }
            body.insert("timeout".to_string(), json_int(args.timeout));
            let transport_timeout = if args.timeout > 0 {
                (args.timeout + 30) as u64
            } else {
                30
            };
            let (status, resp) = client.call(
                "POST",
                &format!("/vms/{}/exec", args.vm),
                Some(&Value::Object(body)),
                transport_timeout,
            );
            if status != 200 {
                die_from(status, &resp, Some(&args.vm));
            }
            let output = resp.get("output").and_then(Value::as_str).unwrap_or("");
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            let _ = stdout.write_all(output.as_bytes());
            let _ = stdout.flush();
            let rc = resp.get("rc").and_then(Value::as_i64).unwrap_or(0);
            std::process::exit(rc as i32);
        }

        Cmd::Push { vm, local, remote } => {
            let mut body = Map::new();
            body.insert("local_path".to_string(), Value::String(local.clone()));
            body.insert("remote_path".to_string(), Value::String(remote.clone()));
            let (status, resp) = client.call(
                "POST",
                &format!("/vms/{vm}/push"),
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!("pushed {local} -> {vm}:{remote}");
        }

        Cmd::Pull { vm, remote, local } => {
            let mut body = Map::new();
            body.insert("remote_path".to_string(), Value::String(remote.clone()));
            body.insert("local_path".to_string(), Value::String(local.clone()));
            let (status, resp) = client.call(
                "POST",
                &format!("/vms/{vm}/pull"),
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!("pulled {vm}:{remote} -> {local}");
        }

        Cmd::Heartbeat { vm, ttl_hours } => {
            let mut body = Map::new();
            if let Some(ttl_hours) = ttl_hours {
                if ttl_hours != 0.0 {
                    body.insert("ttl_hours".to_string(), json_float(ttl_hours));
                }
            }
            let (status, resp) = client.call(
                "POST",
                &format!("/vms/{vm}/heartbeat"),
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!(
                "heartbeat ok; ttl_hours_remaining={}",
                python_str(resp.get("ttl_hours_remaining").unwrap_or(&Value::Null))
            );
        }

        Cmd::Release { vm } => {
            let (status, resp) = client.call(
                "POST",
                &format!("/vms/{vm}/release"),
                Some(&Value::Object(Map::new())),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!("released {vm}");
        }

        Cmd::Gc => {
            let (status, resp) = client.call(
                "POST",
                "/gc",
                Some(&Value::Object(Map::new())),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, None);
            }
            println!("gc done");
        }
    }
}

fn run_console(client: &Client<'_>, vm: &str, action: &str, body: Map<String, Value>) -> ! {
    let (status, resp) = client.call(
        "POST",
        &format!("/vms/{vm}/console/{action}"),
        Some(&Value::Object(body)),
        DEFAULT_TIMEOUT_S,
    );
    if status != 200 {
        die_from(status, &resp, Some(vm));
    }
    println!("{}", pretty(&resp));
    std::process::exit(0);
}

/// Reproduce Python's pre-argparse `exec` handling: split at the first `--`
/// only when `exec` is the subcommand (after an optional global
/// `--environment` prefix), so the guest command is passed through untouched.
fn split_exec_arguments(mut arguments: Vec<String>) -> (Vec<String>, Option<Vec<String>>) {
    let prefix = if arguments
        .first()
        .map(|a| a == "--environment")
        .unwrap_or(false)
    {
        2
    } else if arguments
        .first()
        .map(|a| a.starts_with("--environment="))
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    if arguments.get(prefix).map(|a| a == "exec").unwrap_or(false) {
        if let Some(boundary) = arguments.iter().position(|a| a == "--") {
            let command = arguments[boundary + 1..].to_vec();
            arguments.truncate(boundary);
            return (arguments, Some(command));
        }
    }
    (arguments, None)
}

/// Print an `argparse`-style usage error and exit 2.
///
/// Every `p.error(...)` call in `bin/vmctl` uses the top-level parser, so the
/// preamble is always this exact usage line.
fn usage_error(message: &str) -> ! {
    eprintln!("usage: vmctl [-h] [--environment ENVIRONMENT]");
    eprintln!(
        "             {{environment,acquire,wait,list,images,images-show,status,exec,push,pull,heartbeat,release,capabilities,acquisition-capabilities,console-resolve,console-open,console-cancel,gc}} ..."
    );
    eprintln!("vmctl: error: {message}");
    std::process::exit(2);
}

/// Map service errors to actionable messages, matching `die_from`.
fn die_from(status: u16, resp: &Value, vm: Option<&str>) -> ! {
    eprintln!("{}", die_message(status, resp, vm));
    std::process::exit(1);
}

fn die_message(status: u16, resp: &Value, vm: Option<&str>) -> String {
    let error = error_message_text(resp);
    let mut msg = format!("vmctl: {error}");
    if status == 409 && error.contains("already leased") {
        msg.push_str(
            " — release the existing lease (vmctl list, vmctl release <vm>) or pick another --purpose",
        );
    } else if status == 409 && error.contains("limit reached") {
        msg.push_str(" — free capacity: vmctl list, then vmctl release <vm>");
    } else if (status == 409 && error.contains("BASE-MISSING")) || error.contains("not built yet") {
        msg.push_str(" — build the golden base first (pilot-images host/build-base.zsh <image>)");
    } else if status == 404 && vm.is_some() {
        msg.push_str(" — check spelling: vmctl list");
    }
    msg
}

/// `resp["error"] or resp`, rendered the way Python's f-string would.
fn error_message_text(resp: &Value) -> String {
    let error = match resp {
        Value::Object(map) => map.get("error").cloned(),
        other => Some(other.clone()),
    };
    let chosen = match &error {
        Some(value) if !is_falsy(value) => value.clone(),
        _ => resp.clone(),
    };
    python_str(&chosen)
}

/// Format a transport failure the way Python's `str(e.reason)` renders it.
fn error_reason(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Io(io) => match io.raw_os_error() {
            Some(code) => {
                let text = io.to_string();
                let suffix = format!(" (os error {code})");
                let message = text.strip_suffix(&suffix).unwrap_or(&text);
                format!("[Errno {code}] {message}")
            }
            None => io.to_string(),
        },
        ureq::Error::Timeout(_) => "timed out".to_string(),
        other => other.to_string(),
    }
}

fn parse_response_body(text: &str) -> Value {
    if text.is_empty() {
        return Value::Object(Map::new());
    }
    match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("vmctl: invalid JSON response: {error}");
            std::process::exit(1);
        }
    }
}

/// 2-space indented JSON with Python's default `ensure_ascii=True` escaping,
/// matching `json.dumps(obj, indent=2)`.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value)
        .map(|text| ensure_ascii(&text))
        .unwrap_or_else(|_| "null".to_string())
}

/// 2-space indented JSON preserving non-ASCII, matching
/// `json.dumps(obj, indent=2, ensure_ascii=False)` used by `vmctl environment`.
fn pretty_unicode(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string())
}

/// Escape every non-ASCII code point as Python's `ensure_ascii=True` does.
///
/// JSON syntax is pure ASCII, so non-ASCII only ever appears inside string
/// literals and a flat scan is safe.
fn ensure_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        let code = character as u32;
        if code <= 0x7e {
            out.push(character);
        } else if code <= 0xFFFF {
            out.push_str(&format!("\\u{code:04x}"));
        } else {
            let offset = code - 0x10000;
            let high = 0xD800 + (offset >> 10);
            let low = 0xDC00 + (offset & 0x3FF);
            out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
        }
    }
    out
}

/// Render a JSON value the way Python `str()` would (`None`, `True`, `False`).
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Python truthiness for the value kinds the CLI reads.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64().map(|n| n == 0.0).unwrap_or(false),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// Python `a or b or c`, returning `null` when all are absent or falsy.
fn or_chain(values: &[Option<&Value>]) -> Value {
    for value in values.iter().flatten() {
        if !is_falsy(value) {
            return (*value).clone();
        }
    }
    Value::Null
}

/// JSON integer, matching a Python `int` field.
fn json_int(value: i64) -> Value {
    Value::Number(serde_json::Number::from(value))
}

/// JSON float, matching a Python `float` field such as `--ttl-hours`.
fn json_float(value: f64) -> Value {
    Value::Number(
        serde_json::Number::from_f64(value).unwrap_or_else(|| serde_json::Number::from(0)),
    )
}

/// `vmctl acquire --ttl-hours` value: `argparse` leaves the untyped default
/// `24` as a Python `int`, while an explicit flag is type-converted to `float`.
fn acquire_ttl_hours(value: Option<f64>) -> Value {
    match value {
        Some(value) => json_float(value),
        None => json_int(24),
    }
}

/// Human-readable `vmctl images` table.
fn format_images(resp: &Value) -> String {
    let mut out = String::new();
    let Some(images) = resp.get("images").and_then(Value::as_object) else {
        return out;
    };
    for (name, image) in images {
        let concurrency = image.get("concurrency");
        let running = concurrency
            .and_then(|c| c.get("running"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let limit = concurrency
            .and_then(|c| c.get("limit"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let ok = if image
            .get("base_available")
            .map(|value| !is_falsy(value))
            .unwrap_or(false)
        {
            "ok"
        } else {
            "BASE-MISSING"
        };
        let cap = if limit != 0 {
            format!("{running}/{limit}")
        } else {
            format!("{running}")
        };
        let kind = image.get("kind").and_then(Value::as_str).unwrap_or("");
        let base_vm = image.get("base_vm").and_then(Value::as_str).unwrap_or("");
        let acquirable = concurrency
            .and_then(|c| c.get("acquirable"))
            .map(|value| python_str(value).to_lowercase())
            .unwrap_or_else(|| "none".to_string());
        out.push_str(&format!(
            "{name:<14} {kind:<6} {base_vm:<22} {ok:<13} running={cap:<5} acquirable={acquirable}\n"
        ));
    }
    out
}

/// Human-readable `vmctl list` table, or `no active leases` when empty.
fn format_leases(resp: &Value) -> String {
    let Some(vms) = resp.get("vms").and_then(Value::as_object) else {
        return "no active leases\n".to_string();
    };
    if vms.is_empty() {
        return "no active leases\n".to_string();
    }
    let mut entries: Vec<(&String, &Value)> = vms.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let null = Value::Null;
    let mut out = String::new();
    for (vm, record) in entries {
        let state = record.get("state").map(python_str).unwrap_or_default();
        let image = record
            .get("image")
            .filter(|value| !is_falsy(value))
            .or_else(|| record.get("line").filter(|value| !is_falsy(value)))
            .map(python_str)
            .unwrap_or_default();
        let env = python_str(&or_chain(&[
            record.get("env"),
            record.get("pack"),
            record.get("lane"),
        ]));
        let ip = python_str(record.get("ip").unwrap_or(&null));
        let ttl = record
            .get("ttl_hours_remaining")
            .map(python_str)
            .unwrap_or_default();
        out.push_str(&format!(
            "{vm:<44} {state:<13} image={image:<11} env={env:<10} ip={ip:<15} ttl={ttl}h\n"
        ));
    }
    out
}

/// `vmctl images-show` unknown-image diagnostic.
fn format_unknown_image(image: &str, resp: &Value) -> String {
    let available = resp
        .get("images")
        .and_then(Value::as_object)
        .map(|images| images.keys().cloned().collect::<Vec<String>>().join(", "))
        .unwrap_or_default();
    format!("vmctl: unknown image '{image}' (available: {available})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn acquire_ttl_hours_matches_argparse_default_typing() {
        assert_eq!(
            serde_json::to_string(&acquire_ttl_hours(None)).unwrap(),
            "24"
        );
        assert_eq!(
            serde_json::to_string(&acquire_ttl_hours(Some(6.0))).unwrap(),
            "6.0"
        );
    }

    #[test]
    fn split_exec_arguments_honors_plain_prefix() {
        let (remaining, command) =
            split_exec_arguments(strings(&["exec", "vm", "--", "echo", "hi"]));
        assert_eq!(remaining, strings(&["exec", "vm"]));
        assert_eq!(command, Some(strings(&["echo", "hi"])));
    }

    #[test]
    fn split_exec_arguments_honors_environment_prefix() {
        let (remaining, command) = split_exec_arguments(strings(&[
            "--environment",
            "profile.json",
            "exec",
            "vm",
            "--timeout",
            "5",
            "--",
            "echo",
            "hi",
        ]));
        assert_eq!(
            remaining,
            strings(&[
                "--environment",
                "profile.json",
                "exec",
                "vm",
                "--timeout",
                "5"
            ])
        );
        assert_eq!(command, Some(strings(&["echo", "hi"])));
    }

    #[test]
    fn split_exec_arguments_honors_inline_environment_prefix() {
        let (remaining, command) = split_exec_arguments(strings(&[
            "--environment=profile.json",
            "exec",
            "vm",
            "--",
            "ls",
        ]));
        assert_eq!(
            remaining,
            strings(&["--environment=profile.json", "exec", "vm"])
        );
        assert_eq!(command, Some(strings(&["ls"])));
    }

    #[test]
    fn split_exec_arguments_leaves_non_exec_untouched() {
        let arguments = strings(&["acquire", "--purpose", "x", "--", "trailing"]);
        let (remaining, command) = split_exec_arguments(arguments.clone());
        assert_eq!(remaining, arguments);
        assert_eq!(command, None);
    }

    #[test]
    fn acquire_defaults_match_python() {
        let cli = Cli::try_parse_from(["vmctl", "acquire", "--purpose", "demo"]).unwrap();
        match cli.cmd {
            Cmd::Acquire(args) => {
                assert_eq!(args.purpose, "demo");
                assert_eq!(args.image, "macos26");
                assert_eq!(args.env, "default");
                assert_eq!(args.network, "nat");
                assert_eq!(args.profile, None);
                assert_eq!(args.ttl_hours, None);
                assert_eq!(args.source, "base");
                assert!(!args.no_wait);
                assert!(!args.vnc);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn acquire_deprecated_aliases_parse() {
        let cli = Cli::try_parse_from([
            "vmctl",
            "acquire",
            "--purpose",
            "demo",
            "--line",
            "ubuntu2404",
            "--pack",
            "none",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Acquire(args) => {
                assert_eq!(args.image, "ubuntu2404");
                assert_eq!(args.env, "none");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn exec_defaults_and_separator() {
        let cli = Cli::try_parse_from(["vmctl", "exec", "vm1", "--", "echo", "hi"]).unwrap();
        match cli.cmd {
            Cmd::Exec(args) => {
                assert_eq!(args.vm, "vm1");
                assert_eq!(args.script, None);
                assert_eq!(args.timeout, 600);
                assert_eq!(args.argv, strings(&["echo", "hi"]));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn wait_timeout_default_and_heartbeat_optional() {
        let cli = Cli::try_parse_from(["vmctl", "wait", "vm1"]).unwrap();
        match cli.cmd {
            Cmd::Wait(args) => assert_eq!(args.timeout_s, 900),
            other => panic!("unexpected command: {other:?}"),
        }
        let cli = Cli::try_parse_from(["vmctl", "heartbeat", "vm1"]).unwrap();
        match cli.cmd {
            Cmd::Heartbeat { ttl_hours, .. } => assert_eq!(ttl_hours, None),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn console_actions_require_lease_and_ids() {
        let missing = Cli::try_parse_from(["vmctl", "console-open", "vm1", "--lease-id", "l"]);
        assert!(missing.is_err());
        let cli = Cli::try_parse_from([
            "vmctl",
            "console-open",
            "vm1",
            "--lease-id",
            "l",
            "--console-id",
            "c",
            "--attempt-id",
            "a",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::ConsoleOpen(args) => {
                assert_eq!(args.vm, "vm1");
                assert_eq!(args.lease_id, "l");
                assert_eq!(args.console_id, "c");
                assert_eq!(args.attempt_id, "a");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn pretty_escapes_non_ascii_like_python() {
        assert_eq!(pretty(&json!("caf\u{00e9}")), "\"caf\\u00e9\"");
        assert_eq!(pretty(&json!("\u{1f600}")), "\"\\ud83d\\ude00\"");
        assert_eq!(pretty_unicode(&json!("caf\u{00e9}")), "\"caf\u{00e9}\"");
    }

    #[test]
    fn pretty_preserves_insertion_order() {
        let value = json!({"b": 1, "a": 2});
        assert_eq!(pretty(&value), "{\n  \"b\": 1,\n  \"a\": 2\n}");
    }

    #[test]
    fn pretty_matches_python_empty_container_shapes() {
        assert_eq!(pretty(&json!({})), "{}");
        assert_eq!(pretty(&json!([])), "[]");
    }

    #[test]
    fn images_table_matches_python_format() {
        let resp = json!({
            "images": {
                "macos26": {
                    "kind": "macos",
                    "base_vm": "pilot-base",
                    "base_available": true,
                    "concurrency": {"running": 1, "limit": 2, "acquirable": true}
                }
            }
        });
        assert_eq!(
            format_images(&resp),
            "macos26        macos  pilot-base             ok            running=1/2   acquirable=true\n"
        );
    }

    #[test]
    fn images_table_uses_running_only_without_limit() {
        let resp = json!({
            "images": {
                "ubuntu2404": {
                    "kind": "linux",
                    "base_vm": "pilot-ubuntu-base",
                    "base_available": false,
                    "concurrency": {"running": 3, "limit": 0, "acquirable": false}
                }
            }
        });
        let line = format_images(&resp);
        assert!(line.contains("BASE-MISSING"));
        assert!(line.contains("running=3    "));
        assert!(line.contains("acquirable=false"));
    }

    #[test]
    fn leases_table_empty_and_rows() {
        assert_eq!(format_leases(&json!({"vms": {}})), "no active leases\n");
        let resp = json!({
            "vms": {
                "vm-a": {
                    "state": "running",
                    "image": "ubuntu2404",
                    "env": null,
                    "ip": "192.0.2.1",
                    "ttl_hours_remaining": 24
                }
            }
        });
        let text = format_leases(&resp);
        assert!(text.starts_with("vm-a"));
        assert!(text.contains("image=ubuntu2404"));
        assert!(text.contains("env=None"));
        assert!(text.contains("ip=192.0.2.1"));
        assert!(text.contains("ttl=24h"));
    }

    #[test]
    fn leases_fall_back_to_legacy_fields() {
        let resp = json!({
            "vms": {
                "vm-b": {
                    "state": "stopped",
                    "line": "macos26",
                    "pack": "default",
                    "lane": "ignored",
                    "ip": null,
                    "ttl_hours_remaining": 1.5
                }
            }
        });
        let text = format_leases(&resp);
        assert!(text.contains("image=macos26"));
        assert!(text.contains("env=default"));
        assert!(text.contains("ttl=1.5h"));
    }

    #[test]
    fn unknown_image_message_lists_available_keys() {
        let resp = json!({"images": {"macos26": {}, "ubuntu2404": {}}});
        assert_eq!(
            format_unknown_image("nope", &resp),
            "vmctl: unknown image 'nope' (available: macos26, ubuntu2404)"
        );
    }

    #[test]
    fn die_messages_add_actionable_hints() {
        let already = die_message(
            409,
            &json!({"error": "purpose 'x' already leased on image y"}),
            None,
        );
        assert!(already.contains("already leased"));
        assert!(already.contains("vmctl release"));

        let limit = die_message(
            409,
            &json!({"error": "macOS VM limit reached (2 active)"}),
            None,
        );
        assert!(limit.contains("limit reached"));
        assert!(limit.contains("free capacity"));

        let missing = die_message(409, &json!({"error": "BASE-MISSING"}), None);
        assert!(missing.contains("build the golden base first"));

        let not_built = die_message(500, &json!({"error": "base not built yet"}), None);
        assert!(not_built.contains("build the golden base first"));

        let unknown = die_message(404, &json!({"error": "unknown VM"}), Some("ghost"));
        assert!(unknown.contains("vmctl list"));

        let plain = die_message(500, &json!({"error": "boom"}), None);
        assert_eq!(plain, "vmctl: boom");
    }

    #[test]
    fn python_strings_match_python_spelling() {
        assert_eq!(python_str(&Value::Null), "None");
        assert_eq!(python_str(&json!(true)), "True");
        assert_eq!(python_str(&json!(false)), "False");
        assert_eq!(python_str(&json!(24)), "24");
        assert_eq!(python_str(&json!(1.5)), "1.5");
        assert_eq!(python_str(&json!("text")), "text");
    }

    #[test]
    fn or_chain_skips_python_falsy_values() {
        let empty = Value::String(String::new());
        let lane = Value::String("lane".to_string());
        assert_eq!(
            python_str(&or_chain(&[Some(&Value::Null), Some(&empty), Some(&lane)])),
            "lane"
        );
        assert_eq!(or_chain(&[Some(&Value::Null), None]), Value::Null);
    }

    #[test]
    fn json_helpers_keep_python_integer_and_float_distinction() {
        assert_eq!(json_int(6), json!(6));
        assert_eq!(json_float(24.0), json!(24.0));
        assert_eq!(json_float(1.5), json!(1.5));
        assert_eq!(serde_json::to_string(&json_float(24.0)).unwrap(), "24.0");
        assert_eq!(serde_json::to_string(&json_int(24)).unwrap(), "24");
    }
}
