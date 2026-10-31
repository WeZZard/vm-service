//! Discovery and aggregation of machine-service instances
//! (`docs/arch/federation.md` FD-2, FD-3; `docs/ux/cli/list.md` CL-1, CL-3).
//!
//! The CLI asks the local instance first, then the local Consul agent for
//! every instance registered as `machine-service`, and then every instance
//! at once, each within its own deadline. An instance that does not answer
//! stays in the result as unreachable, with its error and its Consul
//! health: an empty answer would read as "this Mac has no machines".
//! Instances never call one another; federation is done here (DC-30).

use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capability;
use crate::consul::{self, Registration};
use crate::http;
use discovery::Found;

/// The Consul service every instance registers under (FD-1).
pub const SERVICE: &str = "machine-service";
/// Each instance's budget for every request the CLI sends it (FD-3).
pub const INSTANCE_TIMEOUT: Duration = Duration::from_secs(5);
/// The Consul agent's budget (FD-2).
pub const CONSUL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a Bonjour browse gathers advertisements (FD-2, DC-85).
pub const BROWSE_WINDOW: Duration = Duration::from_secs(2);
/// How long the instances a browse found may take to resolve.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

/// A Bonjour browse: every `_machine-service._tcp` instance it finds within
/// its window (FD-2). A plain function, so a test can stand in for the
/// network.
pub type Browser = fn(Duration) -> Result<Vec<Found>, String>;

/// Browse the network through the Mac's own DNS-SD service (DC-85).
pub fn bonjour(window: Duration) -> Result<Vec<Found>, String> {
    discovery::dnssd::browse(
        discovery::SERVICE_TYPE,
        discovery::dnssd::Scope::Network,
        window,
        RESOLVE_TIMEOUT,
    )
}

/// What one gathering asks of the network.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The local instance's base URL, `http://127.0.0.1:6240` by default.
    pub local_base: String,
    /// The Consul agent's base URL in Consul mode (DC-84).
    pub consul: Option<String>,
    /// The browse in Bonjour mode (DC-84). With neither, the CLI stays with
    /// the local instance (`--local`, or a selected environment).
    pub browse: Option<Browser>,
    /// Headers for requests to the local instance: the selected
    /// environment's fingerprint, when one is selected.
    pub local_headers: Vec<(String, String)>,
    /// Each instance's budget.
    pub timeout: Duration,
    /// The Consul agent's budget.
    pub consul_timeout: Duration,
}

/// One instance and, once asked, its answer.
#[derive(Debug, Clone)]
pub struct Instance {
    /// The name the CLI shows and accepts: the instance ID, with `:<port>`
    /// added only when two instances share an ID (DC-61).
    pub label: String,
    /// The instance ID (MM-2), when known.
    pub id: Option<String>,
    /// The base URL the CLI asks.
    pub address: String,
    /// This is the local instance.
    pub local: bool,
    /// Its Consul health, when Consul lists it.
    pub consul: Option<String>,
    /// How the CLI found it: `local`, `bonjour`, `consul` or `address`
    /// (CL-1, DC-84).
    pub found_by: &'static str,
    /// The instance ID its advertisement or registration states, which its
    /// `/health` must answer before it is used (FD-2, DC-85).
    pub expected_id: Option<String>,
    /// Why it is not asked at all, such as a loopback-only registration on
    /// another Mac (FD-1).
    pub unaskable: Option<String>,
    /// It answered `GET /machines`.
    pub reachable: bool,
    /// Why it did not answer.
    pub error: Option<String>,
    /// Its `GET /machines` answer.
    pub body: Option<Value>,
}

impl Instance {
    /// The instance's machines and offers in CL-1's order: the durable
    /// machine, then the virtual machines, then the offers.
    pub fn machines(&self) -> Vec<Value> {
        self.body.as_ref().map(machines_of).unwrap_or_default()
    }
}

/// The result of one gathering.
#[derive(Debug, Clone)]
pub struct Merged {
    /// The local instance first, then every other instance by label.
    pub instances: Vec<Instance>,
    /// Consul was asked.
    pub consul_asked: bool,
    /// Why Consul did not answer, when it did not.
    pub consul_error: Option<String>,
    /// The network was browsed (Bonjour mode).
    pub browsed: bool,
    /// Why the browse failed, when it did.
    pub browse_error: Option<String>,
    /// What the CLI assumed to name an instance, for standard error: one
    /// line per registration that states no instance ID and was named by
    /// its Consul node name instead (FD-2, DC-80).
    pub notes: Vec<String>,
}

impl Merged {
    /// No instance answered.
    pub fn none_reachable(&self) -> bool {
        self.instances.iter().all(|instance| !instance.reachable)
    }
}

/// The records of a `GET /machines` answer in CL-1's order. Records under
/// `machines` are machines; records under `offers`, or with the state
/// `offer`, are virtual machine offers (MM-3). A record keyed by name gets
/// its `name` if it lacks one.
pub fn machines_of(body: &Value) -> Vec<Value> {
    let mut records = Vec::new();
    for (key, is_offer) in [("machines", false), ("offers", true)] {
        let mut add = |name: Option<&str>, record: &Value| {
            let Some(object) = record.as_object() else {
                return;
            };
            let mut object = object.clone();
            if let Some(name) = name {
                object
                    .entry("name")
                    .or_insert_with(|| Value::String(name.to_string()));
            }
            if is_offer {
                object
                    .entry("state")
                    .or_insert_with(|| Value::String("offer".to_string()));
            }
            records.push(Value::Object(object));
        };
        match body.get(key) {
            Some(Value::Object(map)) => map
                .iter()
                .for_each(|(name, record)| add(Some(name), record)),
            Some(Value::Array(list)) => list.iter().for_each(|record| add(None, record)),
            _ => {}
        }
    }
    let rank = |record: &Value| {
        if capability::is_offer(record) {
            2
        } else if capability::kind(record) == "durable" {
            0
        } else {
            1
        }
    };
    let name = |record: &Value| {
        record
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    records.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| name(a).cmp(&name(b))));
    records
}

/// What the local instance answered.
struct LocalProbe {
    id: Option<String>,
    reachable: bool,
    error: Option<String>,
    body: Option<Value>,
}

/// Ask the local instance for its ID (`/health`) and its machines, within
/// one deadline.
fn probe_local(settings: &Settings) -> LocalProbe {
    let deadline = Instant::now() + settings.timeout;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let health = http::get_json(
        &format!("{}/health", settings.local_base),
        remaining(),
        &settings.local_headers,
    );
    let id = match &health {
        Ok((200, body)) => body
            .get("instance")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        _ => None,
    };
    if let Err(error) = health {
        return LocalProbe {
            id,
            reachable: false,
            error: Some(error),
            body: None,
        };
    }
    let (reachable, error, body) =
        ask_machines(&settings.local_base, remaining(), &settings.local_headers);
    LocalProbe {
        id,
        reachable,
        error,
        body,
    }
}

/// `GET /machines` on one instance.
fn ask_machines(
    base: &str,
    timeout: Duration,
    headers: &[(String, String)],
) -> (bool, Option<String>, Option<Value>) {
    match http::get_json(&format!("{base}/machines"), timeout, headers) {
        Ok((200, body)) if body.is_object() => (true, None, Some(body)),
        Ok((200, _)) => (
            false,
            Some("GET /machines answered something other than an object".into()),
            None,
        ),
        Ok((status, body)) => {
            let error = body
                .get("error")
                .and_then(Value::as_str)
                .map(|error| format!("HTTP {status}: {error}"))
                .unwrap_or_else(|| format!("HTTP {status}"));
            (false, Some(error), None)
        }
        Err(error) => (false, Some(error), None),
    }
}

/// The port of a base URL, when it names one.
fn port_of(base: &str) -> Option<u16> {
    let rest = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority.rsplit_once(':')?.1.parse().ok()
}

/// The host and port of a base URL, for matching `--instance`.
fn authority_of(base: &str) -> &str {
    let rest = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    rest.split('/').next().unwrap_or(rest)
}

/// This Mac's local host name, which is its instance ID (MM-2), for when
/// the local instance does not name itself. Consul's node name never stands
/// in for it: the node name need not be the Mac's name (DC-80).
fn local_host_name() -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "LocalHostName"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !name.is_empty()).then_some(name)
}

/// Who a registration says it is: the instance ID it states in its
/// metadata, and whether it stated one. A registration whose description
/// predates the field states none, and is taken to be the instance its
/// Consul node name names, the rule before DC-80, which holds only where a
/// Mac's node name is its local host name.
fn identity(registration: &Registration) -> (String, bool) {
    match &registration.instance {
        Some(id) => (id.clone(), true),
        None => (registration.node.clone(), false),
    }
}

/// Discover the instances without asking any but the local one for
/// machines (FD-2).
///
/// The local instance and a Consul registration are the same instance when
/// the ID the registration states and the local ID match, and so do their
/// ports (DC-61, DC-80). The local ID is what its `/health` says, else this
/// Mac's local host name. A registration that states no ID is matched by its
/// node name instead, and a note says so.
pub fn discover(settings: &Settings) -> Merged {
    let consul_settings = settings.consul.clone();
    let consul_timeout = settings.consul_timeout;
    let (probe, consul, browsed) = thread::scope(|scope| {
        let consul = consul_settings
            .map(|base| scope.spawn(move || consul::instances(&base, SERVICE, consul_timeout)));
        let browsed = settings
            .browse
            .map(|browse| scope.spawn(move || browse(BROWSE_WINDOW)));
        let probe = probe_local(settings);
        let consul = consul.map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| Err("the Consul query panicked".to_string()))
        });
        let browsed = browsed.map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| Err("the Bonjour browse panicked".to_string()))
        });
        (probe, consul, browsed)
    });
    let browse_asked = browsed.is_some();
    let (founds, browse_error) = match browsed {
        None => (Vec::new(), None),
        Some(Ok(founds)) => (founds, None),
        Some(Err(error)) => (Vec::new(), Some(error)),
    };

    let consul_asked = consul.is_some();
    let (registrations, consul_error) = match consul {
        None => (Vec::new(), None),
        Some(Ok(registrations)) => (registrations, None),
        Some(Err(error)) => (Vec::new(), Some(error)),
    };
    let local_id = probe.id.clone().or_else(local_host_name);
    let local_port = port_of(&settings.local_base);

    let mut local = Instance {
        label: local_id.clone().unwrap_or_else(|| "local".to_string()),
        id: local_id.clone(),
        address: settings.local_base.clone(),
        local: true,
        consul: None,
        found_by: "local",
        expected_id: None,
        unaskable: None,
        reachable: probe.reachable,
        error: probe.error,
        body: probe.body,
    };
    let mut others: Vec<Instance> = Vec::new();
    let mut notes = Vec::new();
    for registration in registrations {
        let (id, stated) = identity(&registration);
        if !stated {
            notes.push(format!(
                "the registration on Consul node {node} (port {port}) states no instance ID, so it is named {node} after its node, which need not be its Mac's name; place the description that `machine-service-install catalog-description` prints on that Mac (DC-80)",
                node = registration.node,
                port = registration.port
            ));
        }
        let same = local.consul.is_none()
            && local_id.as_deref() == Some(id.as_str())
            && local_port.is_none_or(|port| port == registration.port);
        if same {
            local.consul = Some(registration.status);
            continue;
        }
        others.push(remote(registration, id, stated));
    }
    let mut seen_local = false;
    for found in founds {
        let id = found.instance().to_string();
        let same = !seen_local
            && local_id.as_deref() == Some(id.as_str())
            && local_port.is_none_or(|port| port == found.port);
        if same {
            seen_local = true;
            continue;
        }
        others.push(Instance {
            label: id.clone(),
            id: Some(id.clone()),
            address: found.base_url(),
            local: false,
            consul: None,
            found_by: "bonjour",
            expected_id: Some(id),
            unaskable: None,
            reachable: false,
            error: None,
            body: None,
        });
    }
    others.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then_with(|| a.address.cmp(&b.address))
    });
    let mut instances = vec![local];
    instances.extend(others);
    disambiguate(&mut instances);
    Merged {
        instances,
        consul_asked,
        consul_error,
        browsed: browse_asked,
        browse_error,
        notes,
    }
}

/// An instance Consul lists on another Mac, or on this Mac at another port,
/// named `id` (DC-80).
fn remote(registration: Registration, id: String, stated: bool) -> Instance {
    let unaskable = registration
        .loopback_only
        .then(|| format!("bound to loopback on {}", registration.node));
    Instance {
        label: id.clone(),
        id: Some(id.clone()),
        address: registration.base_url(),
        local: false,
        consul: Some(registration.status),
        found_by: "consul",
        // A registration named after its node is not confirmed by an ID it
        // never stated (DC-80).
        expected_id: stated.then_some(id),
        unaskable,
        reachable: false,
        error: None,
        body: None,
    }
}

/// Two instances with one ID are two instances on one Mac, such as a
/// second instance for tests: name both `<id>:<port>` (DC-61).
fn disambiguate(instances: &mut [Instance]) {
    let labels: Vec<String> = instances.iter().map(|i| i.label.clone()).collect();
    for instance in instances.iter_mut() {
        if labels
            .iter()
            .filter(|label| **label == instance.label)
            .count()
            > 1
        {
            if let Some(port) = port_of(&instance.address) {
                instance.label = format!("{}:{port}", instance.label);
            }
        }
    }
}

/// Whether `instance` is the one `wanted` names: its label, its ID, its
/// `host:port`, its host, or its base URL.
pub fn names(instance: &Instance, wanted: &str) -> bool {
    let wanted = wanted.trim_end_matches('/');
    let authority = authority_of(&instance.address);
    let host = authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority);
    instance.label == wanted
        || instance.id.as_deref() == Some(wanted)
        || instance.address == wanted
        || authority == wanted
        || host == wanted
}

/// Keep only the instance `wanted` names (`--instance`, FD-2). A value that
/// names no discovered instance but reads as an address is asked directly.
pub fn select(merged: &mut Merged, wanted: &str) -> Result<(), String> {
    let matching: Vec<Instance> = merged
        .instances
        .iter()
        .filter(|instance| names(instance, wanted))
        .cloned()
        .collect();
    if matching.is_empty() {
        let looks_like_address =
            wanted.contains("://") || wanted.contains(':') || wanted.contains('.');
        if !looks_like_address {
            let known: Vec<&str> = merged.instances.iter().map(|i| i.label.as_str()).collect();
            return Err(format!(
                "no instance named {wanted} (known: {})",
                known.join(", ")
            ));
        }
        let address = if wanted.contains("://") {
            wanted.trim_end_matches('/').to_string()
        } else {
            format!("http://{wanted}")
        };
        merged.instances = vec![Instance {
            label: wanted.to_string(),
            id: None,
            address,
            local: false,
            consul: None,
            found_by: "address",
            expected_id: None,
            unaskable: None,
            reachable: false,
            error: None,
            body: None,
        }];
        return Ok(());
    }
    merged.instances = matching;
    Ok(())
}

/// Ask every instance that has not been asked for its machines, all at
/// once, each within `timeout` (FD-3).
pub fn ask_all(merged: &mut Merged, timeout: Duration) {
    thread::scope(|scope| {
        for instance in merged.instances.iter_mut() {
            if instance.local || instance.reachable {
                continue;
            }
            if let Some(reason) = &instance.unaskable {
                instance.error = Some(reason.clone());
                continue;
            }
            scope.spawn(move || {
                let deadline = Instant::now() + timeout;
                if let Some(expected) = &instance.expected_id {
                    if let Err(error) = confirm(&instance.address, expected, timeout) {
                        instance.error = Some(error);
                        return;
                    }
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let (reachable, error, body) = ask_machines(&instance.address, remaining, &[]);
                instance.reachable = reachable;
                instance.error = error;
                instance.body = body;
            });
        }
    });
}

/// Ask an instance found by discovery who it is: it is used only when its
/// `/health` answers machine-service with the ID it was found under, because
/// finding an address proves nothing about who answers there (FD-2, DC-85).
fn confirm(base: &str, expected: &str, timeout: Duration) -> Result<(), String> {
    let (status, body) = http::get_json(&format!("{base}/health"), timeout, &[])?;
    if status != 200 {
        return Err(format!("its /health answered HTTP {status}"));
    }
    let service = body.get("service").and_then(Value::as_str);
    if service.is_some_and(|service| service != SERVICE) {
        return Err(format!(
            "it was found as {expected}, but answers as {}, not {SERVICE}",
            service.unwrap_or_default()
        ));
    }
    match body.get("instance").and_then(Value::as_str) {
        Some(answered) if answered == expected => Ok(()),
        Some(answered) => Err(format!(
            "it was found as {expected}, but its /health answers {answered}"
        )),
        None => Err(format!(
            "it was found as {expected}, but its /health names no instance"
        )),
    }
}

/// Discover and ask every instance: the merged list of CL-1.
pub fn gather(settings: &Settings, instance: Option<&str>) -> Result<Merged, String> {
    let mut merged = discover(settings);
    if let Some(wanted) = instance {
        select(&mut merged, wanted)?;
    }
    ask_all(&mut merged, settings.timeout);
    Ok(merged)
}

/// Split `<vm>@<instance>` (CL-3). A name without `@` names no instance.
pub fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('@') {
        Some((machine, instance)) if !machine.is_empty() && !instance.is_empty() => {
            (machine, Some(instance))
        }
        _ => (name, None),
    }
}

/// Where a lease command goes (CL-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The machine's name without any `@<instance>`.
    pub machine: String,
    /// The base URL of the instance that owns it.
    pub base: String,
    /// The owner is the local instance.
    pub local: bool,
}

/// Find the instance that owns `name` (CL-3).
///
/// `<vm>@<instance>` goes to that instance. A bare name goes to the one
/// instance whose answer lists it; when two list it the name is refused
/// and the qualified form required. A name no instance lists goes to the
/// local instance, whose own answer then says it is unknown.
pub fn route(settings: &Settings, name: &str) -> Result<Route, String> {
    let (machine, instance) = split_name(name);
    let local_route = || Route {
        machine: machine.to_string(),
        base: settings.local_base.clone(),
        local: true,
    };
    if let Some(wanted) = instance {
        let mut merged = discover(settings);
        select(&mut merged, wanted)?;
        let owner = &merged.instances[0];
        if let Some(reason) = &owner.unaskable {
            return Err(format!(
                "instance {} cannot be reached: {reason}",
                owner.label
            ));
        }
        return Ok(Route {
            machine: machine.to_string(),
            base: owner.address.clone(),
            local: owner.local,
        });
    }
    let merged = gather(settings, None)?;
    let owners: Vec<&Instance> = merged
        .instances
        .iter()
        .filter(|instance| {
            instance.machines().iter().any(|record| {
                !capability::is_offer(record)
                    && record.get("name").and_then(Value::as_str) == Some(machine)
            })
        })
        .collect();
    match owners.as_slice() {
        [] => Ok(local_route()),
        [owner] => Ok(Route {
            machine: machine.to_string(),
            base: owner.address.clone(),
            local: owner.local,
        }),
        several => {
            let labels: Vec<&str> = several.iter().map(|owner| owner.label.as_str()).collect();
            Err(format!(
                "{machine} exists on {}; name it as {machine}@<instance>",
                labels.join(" and ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A fake HTTP server answering fixed JSON per path (TL-2).
    struct Fake {
        base: String,
        _thread: thread::JoinHandle<()>,
    }

    fn fake(routes: Vec<(&str, u16, Value)>) -> Fake {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let routes: HashMap<String, (u16, String)> = routes
            .into_iter()
            .map(|(path, status, body)| (path.to_string(), (status, body.to_string())))
            .collect();
        let thread = thread::spawn(move || {
            for request in server.incoming_requests() {
                let (status, body) = routes
                    .get(request.url())
                    .cloned()
                    .unwrap_or((404, "{\"error\": \"not found\"}".to_string()));
                let _ = request
                    .respond(tiny_http::Response::from_string(body).with_status_code(status));
            }
        });
        Fake {
            base: format!("http://127.0.0.1:{port}"),
            _thread: thread,
        }
    }

    fn refused_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    fn registration(node: &str, port: u16, status: &str, loopback: bool) -> Value {
        json!({
            "Node": {"Node": node, "Address": "127.0.0.1"},
            "Service": {"ID": "catalog-machine-service", "Service": "machine-service",
                        "Address": "127.0.0.1", "Port": port,
                        "Tags": ["machines", "agents"],
                        "Meta": {"loopback_only": if loopback { "true" } else { "false" }}},
            "Checks": [{"ServiceID": "catalog-machine-service", "Status": status}]
        })
    }

    /// A registration whose description states its instance ID (FD-1,
    /// DC-80), under a Consul node that may be named otherwise.
    fn stated(node: &str, instance: &str, port: u16, status: &str) -> Value {
        let mut row = registration(node, port, status, false);
        row["Service"]["Meta"]["instance"] = json!(instance);
        row
    }

    fn settings(local: &str, consul: Option<&str>, timeout: Duration) -> Settings {
        Settings {
            local_base: local.to_string(),
            consul: consul.map(str::to_string),
            browse: None,
            local_headers: Vec::new(),
            timeout,
            consul_timeout: timeout,
        }
    }

    #[test]
    fn machines_keep_cl1_order_and_gain_names() {
        let body = json!({
            "machines": {
                "pilot-mac-b": {"state": "running"},
                "mac-a": {"kind": "durable", "state": "available"},
                "pilot-mac-a": {"kind": "virtual", "state": "running"}
            },
            "offers": [{"name": "ubuntu2404", "kind": "virtual"},
                       {"name": "macos26", "kind": "virtual", "state": "offer"}]
        });
        let names: Vec<String> = machines_of(&body)
            .iter()
            .map(|record| record["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "mac-a",
                "pilot-mac-a",
                "pilot-mac-b",
                "macos26",
                "ubuntu2404"
            ]
        );
        assert!(machines_of(&body).iter().skip(3).all(capability::is_offer));
    }

    /// AC-34, FD-3: a hung instance costs its deadline, no more, and the
    /// others still answer.
    #[test]
    fn ac34_hung_instance_is_bounded_and_hides_nothing() {
        let hung = TcpListener::bind("127.0.0.1:0").unwrap();
        let hung_port = hung.local_addr().unwrap().port();
        let remote = fake(vec![
            (
                "/health",
                200,
                json!({"ok": true, "service": "machine-service", "instance": "mac-b"}),
            ),
            (
                "/machines",
                200,
                json!({"machines": {"pilot-mac-r": {"state": "running"}}}),
            ),
        ]);
        let remote_port = port_of(&remote.base).unwrap();
        let local = fake(vec![
            ("/health", 200, json!({"ok": true, "instance": "mac-a"})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let consul = fake(vec![(
            "/v1/health/service/machine-service",
            200,
            json!([
                stated("mac-b", "mac-b", remote_port, "passing"),
                stated("mac-c", "mac-c", hung_port, "passing")
            ]),
        )]);
        let started = Instant::now();
        let merged = gather(
            &settings(&local.base, Some(&consul.base), Duration::from_millis(400)),
            None,
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let labels: Vec<&str> = merged.instances.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["mac-a", "mac-b", "mac-c"]);
        assert!(merged.instances[1].reachable);
        assert!(!merged.instances[2].reachable);
        assert!(
            merged.instances[2]
                .error
                .as_deref()
                .unwrap()
                .starts_with("timed out"),
            "{:?}",
            merged.instances[2].error
        );
        drop(hung);
    }

    /// FD-2, DC-61: the local instance is matched to its registration by
    /// the ID the registration states and the port; another port is
    /// another instance, and both are then named with their ports.
    #[test]
    fn local_identity_and_second_instances() {
        let local = fake(vec![
            ("/health", 200, json!({"ok": true, "instance": "mac-a"})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let local_port = port_of(&local.base).unwrap();
        let other_port = refused_port();
        let consul = fake(vec![(
            "/v1/health/service/machine-service",
            200,
            json!([
                stated("mac-a", "mac-a", local_port, "warning"),
                stated("mac-a", "mac-a", other_port, "critical")
            ]),
        )]);
        let merged = discover(&settings(
            &local.base,
            Some(&consul.base),
            Duration::from_secs(2),
        ));
        assert_eq!(merged.instances.len(), 2);
        assert!(merged.instances[0].local);
        assert_eq!(merged.instances[0].consul.as_deref(), Some("warning"));
        assert_eq!(merged.instances[0].label, format!("mac-a:{local_port}"));
        assert_eq!(merged.instances[1].label, format!("mac-a:{other_port}"));
        assert!(merged.notes.is_empty(), "{:?}", merged.notes);
    }

    /// AC-47, DC-80: a Mac whose Consul node name differs from its own name
    /// (192.168.50.3: node `mac-mini-m4-pro`, LocalHostName `Mac-mini`) is
    /// one instance, named by its own name, because its registration states
    /// the ID its `/health` reports.
    #[test]
    fn ac47_node_name_differing_from_the_machine_name_is_one_instance() {
        let local = fake(vec![
            ("/health", 200, json!({"ok": true, "instance": "Mac-mini"})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let local_port = port_of(&local.base).unwrap();
        let consul = fake(vec![
            (
                "/v1/health/service/machine-service",
                200,
                json!([stated("mac-mini-m4-pro", "Mac-mini", local_port, "passing")]),
            ),
            (
                "/v1/agent/self",
                200,
                json!({"Config": {"NodeName": "mac-mini-m4-pro"}}),
            ),
        ]);
        let merged = discover(&settings(
            &local.base,
            Some(&consul.base),
            Duration::from_secs(2),
        ));
        let labels: Vec<&str> = merged.instances.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Mac-mini"]);
        assert!(merged.instances[0].local);
        assert_eq!(merged.instances[0].consul.as_deref(), Some("passing"));
        assert!(merged.notes.is_empty(), "{:?}", merged.notes);
    }

    /// AC-47, DC-80: another Mac is named by the ID its registration
    /// states, never by its node, and `--instance` finds it by that name.
    #[test]
    fn ac47_another_mac_is_named_by_its_stated_id() {
        let local = fake(vec![
            ("/health", 200, json!({"instance": "mac-a"})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let consul = fake(vec![(
            "/v1/health/service/machine-service",
            200,
            json!([stated("consul-node-b", "Mac-B", 6299, "passing")]),
        )]);
        let base = settings(&local.base, Some(&consul.base), Duration::from_secs(2));
        let merged = discover(&base);
        assert_eq!(merged.instances[1].label, "Mac-B");
        assert_eq!(merged.instances[1].id.as_deref(), Some("Mac-B"));
        let mut merged = discover(&base);
        select(&mut merged, "Mac-B").unwrap();
        assert_eq!(merged.instances[0].address, "http://127.0.0.1:6299");
    }

    /// AC-47, DC-80: without a `/health` ID the local ID is this Mac's local
    /// host name, never the Consul agent's node name, so a registration
    /// that states that name is still the local instance.
    #[test]
    fn ac47_local_id_falls_back_to_the_local_host_name_not_the_node() {
        let Some(host_name) = local_host_name() else {
            return;
        };
        let local = fake(vec![
            ("/health", 200, json!({"ok": true})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let local_port = port_of(&local.base).unwrap();
        let consul = fake(vec![
            (
                "/v1/health/service/machine-service",
                200,
                json!([stated("some-node", &host_name, local_port, "passing")]),
            ),
            (
                "/v1/agent/self",
                200,
                json!({"Config": {"NodeName": "some-node"}}),
            ),
        ]);
        let merged = discover(&settings(
            &local.base,
            Some(&consul.base),
            Duration::from_secs(2),
        ));
        assert_eq!(merged.instances.len(), 1);
        assert_eq!(merged.instances[0].label, host_name);
        assert_eq!(merged.instances[0].consul.as_deref(), Some("passing"));
    }

    /// AC-47, DC-80: a registration that states no ID (a description older
    /// than the field) falls back to the old rule, its node name, and a
    /// note says so for each one; where the node name is the Mac's name it
    /// is still one instance.
    #[test]
    fn ac47_registration_without_metadata_falls_back_to_the_node_name() {
        let local = fake(vec![
            ("/health", 200, json!({"instance": "mac-a"})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let local_port = port_of(&local.base).unwrap();
        let consul = fake(vec![(
            "/v1/health/service/machine-service",
            200,
            json!([
                registration("mac-a", local_port, "passing", false),
                registration("mac-b", 6299, "passing", false)
            ]),
        )]);
        let merged = discover(&settings(
            &local.base,
            Some(&consul.base),
            Duration::from_secs(2),
        ));
        let labels: Vec<&str> = merged.instances.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["mac-a", "mac-b"]);
        assert_eq!(merged.instances[0].consul.as_deref(), Some("passing"));
        assert_eq!(merged.notes.len(), 2, "{:?}", merged.notes);
        assert!(
            merged.notes[0].contains("Consul node mac-a")
                && merged.notes[0].contains("states no instance ID")
                && merged.notes[0].contains("DC-80"),
            "{:?}",
            merged.notes
        );
    }

    /// FD-1: a registration published loopback-only on another Mac is
    /// listed as unreachable without a request.
    #[test]
    fn loopback_only_registration_is_not_asked() {
        let local = fake(vec![
            ("/health", 200, json!({"instance": "mac-a"})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let consul = fake(vec![(
            "/v1/health/service/machine-service",
            200,
            json!([registration("mac-b", 6240, "passing", true)]),
        )]);
        let merged = gather(
            &settings(&local.base, Some(&consul.base), Duration::from_secs(2)),
            None,
        )
        .unwrap();
        let other = &merged.instances[1];
        assert!(!other.reachable);
        assert_eq!(other.error.as_deref(), Some("bound to loopback on mac-b"));
    }

    /// FD-2: with no Consul agent the local instance is still listed, and
    /// the failure is reported, never hidden.
    #[test]
    fn no_consul_agent_still_lists_the_local_instance() {
        let local = fake(vec![
            ("/health", 200, json!({"instance": "mac-a"})),
            (
                "/machines",
                200,
                json!({"machines": {"vm": {"state": "running"}}}),
            ),
        ]);
        let consul = format!("http://127.0.0.1:{}", refused_port());
        let merged = gather(
            &settings(&local.base, Some(&consul), Duration::from_secs(2)),
            None,
        )
        .unwrap();
        assert_eq!(merged.instances.len(), 1);
        assert!(merged.instances[0].reachable);
        assert_eq!(merged.consul_error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn select_by_label_address_or_direct() {
        let local = fake(vec![
            ("/health", 200, json!({"instance": "mac-a"})),
            ("/machines", 200, json!({"machines": {}})),
        ]);
        let consul = fake(vec![(
            "/v1/health/service/machine-service",
            200,
            json!([registration("mac-b", 6299, "passing", false)]),
        )]);
        let base = settings(&local.base, Some(&consul.base), Duration::from_secs(2));
        let mut merged = discover(&base);
        select(&mut merged, "mac-b").unwrap();
        assert_eq!(merged.instances.len(), 1);
        assert_eq!(merged.instances[0].address, "http://127.0.0.1:6299");

        let mut merged = discover(&base);
        select(&mut merged, "127.0.0.1:6299").unwrap();
        assert_eq!(merged.instances[0].label, "mac-b");

        let mut merged = discover(&base);
        select(&mut merged, "192.0.2.9:6240").unwrap();
        assert_eq!(merged.instances[0].address, "http://192.0.2.9:6240");

        let mut merged = discover(&base);
        let error = select(&mut merged, "mac-z").unwrap_err();
        assert!(
            error.contains("mac-a") && error.contains("mac-b"),
            "{error}"
        );
    }

    /// CL-3: `<vm>@<instance>` and bare names route to the owner; a name
    /// two instances list must be qualified; an unknown name stays local.
    #[test]
    fn cl3_routes_names_to_their_owner() {
        assert_eq!(split_name("vm@mac-b"), ("vm", Some("mac-b")));
        assert_eq!(split_name("vm"), ("vm", None));
        assert_eq!(split_name("@mac-b"), ("@mac-b", None));

        let local = fake(vec![
            ("/health", 200, json!({"instance": "mac-a"})),
            (
                "/machines",
                200,
                json!({"machines": {"shared": {}, "only-local": {}}}),
            ),
        ]);
        let remote = fake(vec![(
            "/machines",
            200,
            json!({"machines": {"shared": {}, "only-remote": {}},
                   "offers": {"macos26": {"kind": "virtual"}}}),
        )]);
        let remote_port = port_of(&remote.base).unwrap();
        let consul = fake(vec![(
            "/v1/health/service/machine-service",
            200,
            json!([registration("mac-b", remote_port, "passing", false)]),
        )]);
        let base = settings(&local.base, Some(&consul.base), Duration::from_secs(2));

        let route_to = |name: &str| route(&base, name);
        assert_eq!(
            route_to("only-remote").unwrap(),
            Route {
                machine: "only-remote".into(),
                base: remote.base.clone(),
                local: false
            }
        );
        assert!(route_to("only-local").unwrap().local);
        let ambiguous = route_to("shared").unwrap_err();
        assert!(ambiguous.contains("shared@<instance>"), "{ambiguous}");
        assert_eq!(route_to("shared@mac-b").unwrap().base, remote.base);
        assert!(route_to("shared@mac-a").unwrap().local);
        // An offer is not a machine; an unknown name stays local.
        assert!(route_to("macos26").unwrap().local);
        assert!(route_to("ghost").unwrap().local);
        assert!(route_to("vm@mac-z").is_err());
    }
}
