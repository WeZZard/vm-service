//! The `vm-service` daemon: HTTP API, lease state, and the GC loop.

use std::sync::Arc;

use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use vm_service_core::config::Config;
use vm_service_core::error::{OpError, OpResult};
use vm_service_core::service::Service;
use vm_service_core::{
    acquisition_options, application_catalog, control_only, environment, snapshot,
};

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = run(&arguments) {
        eprintln!("vm-service: {message}");
        std::process::exit(1);
    }
}

fn run(arguments: &[String]) -> Result<(), String> {
    let mut environment_path: Option<String> = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--environment" => {
                index += 1;
                environment_path = arguments.get(index).cloned();
            }
            other if other.starts_with("--environment=") => {
                environment_path = Some(other["--environment=".len()..].to_string());
            }
            _ => {}
        }
        index += 1;
    }

    let bundle = environment::load_environment(environment_path.as_deref())
        .map_err(|error| error.to_string())?;
    let config = match &bundle {
        Some(bundle) => Config::from_selected(bundle),
        None => Config::legacy(),
    };

    if bundle.is_none() {
        // A selected startup binds a store/state; legacy startup must not
        // reinterpret it.
        let legacy_store = std::env::var("TART_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/"))
                    .join(".tart")
            });
        let roots = [config.state_dir.clone(), legacy_store];
        if roots
            .iter()
            .any(|root| root.join(environment::MARKER).exists())
        {
            return Err("selected environment marker requires an explicit profile".to_string());
        }
    }

    let _ownership = match &bundle {
        Some(bundle) => Some(environment::ownership(bundle).map_err(|error| error.to_string())?),
        None => None,
    };

    serve(config)
}

fn serve(config: Config) -> Result<(), String> {
    let console_config = Service::load_console_config().map_err(|error| error.to_string())?;
    let selected = config.environment.is_some();
    let service = Arc::new(Service::new(config, console_config));
    std::fs::create_dir_all(&service.config.state_dir).map_err(|error| error.to_string())?;

    // Legacy mode takes the daemon lock explicitly; a selected environment
    // already holds it through `environment::ownership`.
    if !selected {
        let lock_path = service.config.state_dir.join("daemon.lock");
        let descriptor = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lock_path)
            .map_err(|error| error.to_string())?;
        use std::os::fd::AsRawFd;
        // SAFETY: `descriptor` is open for the process lifetime; the flock is
        // released by the kernel on exit.
        let locked =
            unsafe { libc::flock(descriptor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            return Err("Another vm-service daemon owns this state directory".to_string());
        }
        std::mem::forget(descriptor);
    }

    if let Err(error) = service.initialize_application_associations() {
        if selected {
            return Err(format!("selected image configuration is invalid: {error}"));
        }
        let bounded: String = error.to_string().chars().take(512).collect();
        service.log(&format!("WARN: /applications unavailable: {bounded}"));
    }
    if selected {
        let mut cache = service
            .lines
            .lock()
            .map_err(|_| "lines poisoned".to_string())?;
        service
            .host
            .discover_lines(&mut cache, true)
            .map_err(|error| error.to_string())?;
    }

    // No acquisition survives a restart; release the records it left behind
    // before any request can observe or retry them.
    if let Err(error) = service.reconcile_startup() {
        service.log(&format!("WARN: startup reconciliation failed: {error}"));
    }

    let address = if service.config.host.contains(':') {
        format!("[{}]:{}", service.config.host, service.config.port)
    } else {
        format!("{}:{}", service.config.host, service.config.port)
    };
    let server = Server::http(&address).map_err(|error| error.to_string())?;
    service.log(&format!(
        "vm-service starting on {}:{} (pilot repo: {})",
        service.config.host,
        service.config.port,
        service.config.pilot.display()
    ));

    let gc_service = service.clone();
    std::thread::spawn(move || gc_service.gc_loop());

    for request in server.incoming_requests() {
        let service = service.clone();
        std::thread::spawn(move || handle(&service, request));
    }
    service.consoles.shutdown();
    Ok(())
}

fn handle(service: &Service, mut request: Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let response = match method {
        Method::Get => handle_get(service, &mut request, &url),
        Method::Post => handle_post(service, &mut request, &url),
        _ => {
            send(request, 404, &json!({"error": "not found"}));
            return;
        }
    };
    match response {
        Ok((code, value)) => send(request, code, &value),
        Err(error) => send(request, 409, &json!({"error": error.to_string()})),
    }
}

fn supplied_fingerprint(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("X-VM-Environment-Fingerprint"))
        .map(|header| header.value.as_str().to_string())
}

fn environment_allowed(service: &Service, supplied: &Option<String>, mutating: bool) -> bool {
    let expected = service.config.fingerprint();
    if let Some(supplied) = supplied {
        if Some(supplied.as_str()) != expected.as_deref() {
            return false;
        }
    } else if mutating && expected.is_some() {
        return false;
    }
    true
}

fn handle_get(service: &Service, request: &mut Request, url: &str) -> OpResult<(u16, Value)> {
    let supplied = supplied_fingerprint(request);
    if !environment_allowed(service, &supplied, false) {
        return Ok((
            409,
            json!({"error": "environment fingerprint mismatch; refusing request"}),
        ));
    }
    // CPython compares the raw `BaseHTTPRequestHandler.self.path`, which still
    // carries any query string, so a path with a query matches no route and
    // falls through to the generic 404. Do not strip the query here.
    let path = url;
    if path == "/" {
        return Ok((
            200,
            json!({
                "service": "vm-service",
                "endpoints": ["/health", "/images", "/applications", "/vms", "/vms/{name}"],
            }),
        ));
    }
    if path == "/health" {
        let mut body = json!({"ok": true, "service": "vm-service"});
        if let Some(environment) = service
            .config
            .environment
            .as_ref()
            .and_then(|bundle| bundle.get("identity"))
        {
            body["environment"] = environment.clone();
        }
        return Ok((200, body));
    }
    if path == "/capabilities" {
        return Ok((200, control_only::descriptor()));
    }
    if path == "/acquisition-capabilities" {
        let images = if service.associations_initialized() {
            Some(Value::Array(
                service
                    .association_tuples()
                    .into_iter()
                    .map(|(image, kind, _)| json!({"image": image, "os": kind}))
                    .collect(),
            ))
        } else {
            None
        };
        let capabilities = service.consoles.capabilities();
        return Ok((
            200,
            acquisition_options::descriptor(&capabilities, images.as_ref()),
        ));
    }
    if path == "/images" {
        return Ok((200, snapshot::images_snapshot(service)?));
    }
    if path == "/applications" {
        let associations = service.application_associations()?;
        let log = |message: &str| service.log(message);
        match application_catalog::load_catalog(
            &service.config.pilot,
            &Value::Object(associations),
            None,
            Some(&log),
        ) {
            Ok(catalog) => return Ok((200, catalog)),
            Err(error) => return Ok((503, json!({"error": error.to_string()}))),
        }
    }
    if path == "/vms" {
        return Ok((200, snapshot::snapshot(service)?));
    }
    if let Some(name) = path.strip_prefix("/vms/") {
        if !name.is_empty() && !name.contains('/') {
            let vms = snapshot::snapshot(service)?;
            if let Some(record) = vms.get("vms").and_then(|vms| vms.get(name)) {
                return Ok((200, record.clone()));
            }
            return Ok((404, json!({"error": "unknown VM"})));
        }
    }
    Ok((404, json!({"error": "not found"})))
}

fn handle_post(service: &Service, request: &mut Request, url: &str) -> OpResult<(u16, Value)> {
    let supplied = supplied_fingerprint(request);
    if !environment_allowed(service, &supplied, true) {
        return Ok((
            409,
            json!({"error": "environment fingerprint mismatch; refusing request"}),
        ));
    }
    let body = match read_body(request) {
        Ok(body) => body,
        Err(error) => return Ok((409, json!({"error": error.to_string()}))),
    };
    // CPython compares the raw `BaseHTTPRequestHandler.self.path`, which still
    // carries any query string, so a path with a query matches no route and
    // falls through to the generic 404. Do not strip the query here.
    let path = url;

    if path == "/acquire" {
        control_only::validate_request(&body).map_err(|error| OpError::new(error.to_string()))?;
        let Some(purpose) = body.get("purpose").and_then(Value::as_str) else {
            return Ok((409, json!({"error": "'purpose' (string) is required"})));
        };
        let image =
            string_field(&body, &["image", "line"]).unwrap_or_else(|| "macos26".to_string());
        let env = string_field(&body, &["env", "pack", "lane"])
            .unwrap_or_else(|| vm_service_core::packs::DEFAULT_PACK.to_string());
        let ttl = body.get("ttl_hours").cloned().unwrap_or_else(|| json!(24));
        check_option_integers(&body)?;
        let record = service.acquire(
            purpose,
            &image,
            &env,
            &ttl,
            body.get("cpu").and_then(Value::as_i64),
            body.get("memory_mb").and_then(Value::as_i64),
            body.get("disk_gb").and_then(Value::as_i64),
            body.get("wait").and_then(Value::as_bool).unwrap_or(true),
            body.get("network").and_then(Value::as_str).unwrap_or("nat"),
            body.get("profile").and_then(Value::as_str),
            body.get("source").and_then(Value::as_str).unwrap_or("base"),
            body.get("expected_source_fingerprint"),
            body.get("vnc").and_then(Value::as_bool).unwrap_or(false),
        )?;
        return Ok((200, record));
    }

    if let Some(rest) = path.strip_prefix("/vms/") {
        let mut segments = rest.split('/');
        let vm = segments.next().unwrap_or_default().to_string();
        let action = segments.next().unwrap_or_default().to_string();
        let tail = segments.next();
        if action == "console" {
            let operation = tail.ok_or_else(|| OpError::new("not found"))?;
            return handle_console(service, &vm, operation, &body);
        }
        match action.as_str() {
            "release" => {
                let reason = body
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("requested");
                let result = service.release(&vm, reason, false)?;
                return Ok((200, result));
            }
            "heartbeat" => {
                let result = service.heartbeat(&vm, body.get("ttl_hours"))?;
                return Ok((200, result));
            }
            "exec" => return Ok((200, service.guest_exec(&vm, &body)?)),
            "push" => return Ok((200, service.guest_push(&vm, &body)?)),
            "pull" => return Ok((200, service.guest_pull(&vm, &body)?)),
            _ => {}
        }
    }

    if path == "/gc" {
        service.gc_once()?;
        return Ok((200, json!({"gc": "done"})));
    }
    Ok((404, json!({"error": "not found"})))
}

fn handle_console(
    service: &Service,
    vm: &str,
    operation: &str,
    body: &Value,
) -> OpResult<(u16, Value)> {
    let required: Vec<&str> = match operation {
        "resolve" => vec!["lease_id"],
        "open" | "cancel" => vec!["lease_id", "console_id", "attempt_id"],
        // The Python route regex accepts only `resolve`, `open`, and `cancel`
        // before the body is examined, so any other operation is unroutable.
        _ => return Ok((404, json!({"error": "not found"}))),
    };
    let object = match body.as_object() {
        Some(object)
            if object.len() == required.len()
                && required.iter().all(|key| object.contains_key(*key)) =>
        {
            object
        }
        _ => {
            return Err(OpError::new("Invalid console request identity or fields"));
        }
    };
    if !required.iter().all(|key| {
        object
            .get(*key)
            .map(vm_service_core::console_api::identity)
            .unwrap_or(false)
    }) {
        return Err(OpError::new("Invalid console request identity or fields"));
    }
    let record = service.get_record(vm)?;
    let record_map: Map<String, Value> = record
        .as_object()
        .cloned()
        .ok_or_else(|| OpError::new("lease record is not an object"))?;
    let lease_id = object["lease_id"].as_str().unwrap_or_default().to_string();
    let result = match operation {
        "resolve" => {
            let resolved = service
                .consoles
                .resolve(&record_map, Some(&lease_id))
                .map_err(|error| OpError::new(error.to_string()))?;
            Value::Object(resolved)
        }
        "open" => service
            .consoles
            .open(
                &record_map,
                &lease_id,
                object["console_id"].as_str().unwrap_or_default(),
                object["attempt_id"].as_str().unwrap_or_default(),
            )
            .map_err(|error| OpError::new(error.to_string()))?,
        "cancel" => service
            .consoles
            .cancel(
                &record_map,
                &lease_id,
                object["console_id"].as_str().unwrap_or_default(),
                object["attempt_id"].as_str().unwrap_or_default(),
            )
            .map_err(|error| OpError::new(error.to_string()))?,
        _ => return Ok((404, json!({"error": "not found"}))),
    };
    Ok((200, result))
}

fn check_option_integers(body: &Value) -> OpResult<()> {
    for name in ["cpu", "memory_mb", "disk_gb"] {
        match body.get(name) {
            None | Some(Value::Null) => {}
            Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {}
            Some(_) => {
                return Err(OpError::new(format!(
                    "{name} must be a positive integer or null"
                )));
            }
        }
    }
    Ok(())
}

fn string_field(body: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| body.get(*name).and_then(Value::as_str).map(str::to_string))
}

fn read_body(request: &mut Request) -> OpResult<Value> {
    let length: usize = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Length"))
        .and_then(|header| header.value.as_str().parse().ok())
        .unwrap_or(0);
    if length == 0 {
        return Ok(json!({}));
    }
    let mut bytes = vec![0u8; length];
    request
        .as_reader()
        .read_exact(&mut bytes)
        .map_err(|error| OpError::new(format!("invalid JSON body: {error}")))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    let mut parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|error| OpError::new(format!("invalid JSON body: {error}")))?;
    // CPython's `json` normalizes float literals when it loads them.
    vm_service_core::python_json::normalize_numbers(&mut parsed);
    Ok(parsed)
}

fn send(request: Request, code: u16, value: &Value) {
    let mut body = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
    body.push('\n');
    let content_type =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header");
    let response = Response::from_string(body)
        .with_status_code(code)
        .with_header(content_type);
    let _ = request.respond(response);
}
