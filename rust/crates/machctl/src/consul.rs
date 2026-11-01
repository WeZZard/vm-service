//! The local Consul agent's HTTP API, read-only (`docs/arch/federation.md`
//! FD-2).
//!
//! The CLI asks the agent on its own Mac for every instance registered as
//! `machine-service`, healthy or not, and never registers or changes
//! anything. The service-catalog registrar on each Mac is what publishes an
//! instance (FD-1).

use std::time::Duration;

use serde_json::Value;

use crate::http;

/// Where the agent listens when `CONSUL_HTTP_ADDR` is unset.
pub const DEFAULT_ADDR: &str = "http://127.0.0.1:8500";

/// The agent's base URL from `CONSUL_HTTP_ADDR`, Consul's own variable.
/// Consul accepts a bare `host:port` there, which means plain HTTP.
pub fn base_address(variable: Option<&str>) -> String {
    let raw = variable.map(str::trim).filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return DEFAULT_ADDR.to_string();
    };
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// The service metadata key under which an instance states its ID (FD-1,
/// DC-80).
pub const INSTANCE_META: &str = "instance";

/// One instance as Consul lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// The Consul node it is registered under. This is where it is, not who
    /// it is: a Mac's node name need not be its instance ID (MM-2, DC-80).
    pub node: String,
    /// The instance ID it states in its service metadata, `instance` (FD-1).
    /// `None` for a registration whose description predates the field.
    pub instance: Option<String>,
    /// The permanent hardware identity (MI-4, DC-87).
    pub machine_id: Option<String>,
    /// The Consul service ID.
    pub service_id: String,
    /// The address to reach it at: the service's own, else the node's.
    pub host: String,
    /// The service port.
    pub port: u16,
    /// The tags the description file gave it (FD-1).
    pub tags: Vec<String>,
    /// The registrar found it bound to loopback, so only its own Mac can
    /// reach it (FD-1).
    pub loopback_only: bool,
    /// The worst of its checks: `passing`, `warning`, `critical`, or
    /// `unknown` when no check reported.
    pub status: String,
}

impl Registration {
    /// The instance's base URL.
    pub fn base_url(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("http://[{}]:{}", self.host, self.port)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }
}

/// Every instance of `service`, healthy or not, from
/// `GET /v1/health/service/<service>`.
pub fn instances(
    base: &str,
    service: &str,
    timeout: Duration,
) -> Result<Vec<Registration>, String> {
    let (status, body) =
        http::get_json(&format!("{base}/v1/health/service/{service}"), timeout, &[])?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    parse_health(&body)
}

/// Parse a `/v1/health/service` answer.
pub fn parse_health(body: &Value) -> Result<Vec<Registration>, String> {
    let rows = body
        .as_array()
        .ok_or_else(|| "the health answer is not a list".to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or("").to_string();
        let node = row.get("Node");
        let service = row.get("Service");
        let node_name = text(node.and_then(|node| node.get("Node")));
        let service_id = text(service.and_then(|service| service.get("ID")));
        let mut host = text(service.and_then(|service| service.get("Address")));
        if host.is_empty() {
            host = text(node.and_then(|node| node.get("Address")));
        }
        let port = service
            .and_then(|service| service.get("Port"))
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok());
        let (Some(port), false, false) = (port, node_name.is_empty(), host.is_empty()) else {
            // A row without a node, an address or a port names nothing the
            // CLI could ask; skip it rather than invent one.
            continue;
        };
        let tags = service
            .and_then(|service| service.get("Tags"))
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let meta = |key: &str| {
            service
                .and_then(|service| service.get("Meta"))
                .and_then(|meta| meta.get(key))
                .and_then(Value::as_str)
        };
        let loopback_only = meta("loopback_only") == Some("true");
        let instance = meta(INSTANCE_META)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        // Node checks (no service ID) count too: a Mac that left the
        // cluster makes every service on it unusable.
        let status = worst_status(
            row.get("Checks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|check| {
                    let owner = check.get("ServiceID").and_then(Value::as_str).unwrap_or("");
                    owner.is_empty() || owner == service_id
                })
                .filter_map(|check| check.get("Status").and_then(Value::as_str)),
        );
        out.push(Registration {
            node: node_name,
            instance,
            machine_id: meta("machine_id")
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            service_id,
            host,
            port,
            tags,
            loopback_only,
            status,
        });
    }
    Ok(out)
}

/// The worst status: `critical` over `warning` over `passing`, and
/// `unknown` when nothing reported.
fn worst_status<'a>(statuses: impl Iterator<Item = &'a str>) -> String {
    let rank = |status: &str| match status {
        "critical" => 3,
        "warning" => 2,
        "passing" => 1,
        _ => 0,
    };
    let mut worst = "unknown";
    for status in statuses {
        if rank(status) > rank(worst) {
            worst = status;
        }
    }
    worst.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base_address_follows_consul_conventions() {
        assert_eq!(base_address(None), "http://127.0.0.1:8500");
        assert_eq!(base_address(Some("")), "http://127.0.0.1:8500");
        assert_eq!(
            base_address(Some("127.0.0.1:8501")),
            "http://127.0.0.1:8501"
        );
        assert_eq!(
            base_address(Some("http://consul.example:8500/")),
            "http://consul.example:8500"
        );
    }

    /// FD-2: the health answer's shape, as a real agent returns it, with a
    /// node check and a service check.
    #[test]
    fn parses_a_health_answer() {
        let body = json!([
            {
                "Node": {"Node": "mac-a", "Address": "192.0.2.10"},
                "Service": {
                    "ID": "catalog-machine-service", "Service": "machine-service",
                    "Tags": ["machines", "agents", "vm", "tart"],
                    "Address": "192.0.2.10", "Port": 6240,
                    "Meta": {"loopback_only": "false", "instance": "Mac-A"}
                },
                "Checks": [
                    {"ServiceID": "", "Status": "passing"},
                    {"ServiceID": "catalog-machine-service", "Status": "critical"},
                    {"ServiceID": "catalog-other", "Status": "warning"}
                ]
            },
            {
                "Node": {"Node": "mac-b", "Address": "192.0.2.11"},
                "Service": {"ID": "s", "Address": "", "Port": 6240,
                            "Meta": {"loopback_only": "true"}},
                "Checks": []
            },
            {"Node": {"Node": "broken"}, "Service": {"ID": "x"}}
        ]);
        let rows = parse_health(&body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].node, "mac-a");
        // DC-80: the stated instance ID is read as it is, and a
        // registration without one states none.
        assert_eq!(rows[0].instance.as_deref(), Some("Mac-A"));
        assert_eq!(rows[1].instance, None);
        assert_eq!(rows[0].status, "critical");
        assert_eq!(rows[0].base_url(), "http://192.0.2.10:6240");
        assert!(!rows[0].loopback_only);
        assert!(rows[0].tags.contains(&"vm".to_string()));
        // An empty service address falls back to the node's.
        assert_eq!(rows[1].host, "192.0.2.11");
        assert!(rows[1].loopback_only);
        assert_eq!(rows[1].status, "unknown");
        assert!(parse_health(&json!({})).is_err());
    }

    #[test]
    fn ipv6_hosts_are_bracketed() {
        let registration = Registration {
            node: "n".into(),
            instance: None,
            machine_id: None,
            service_id: "s".into(),
            host: "2001:db8::1".into(),
            port: 6240,
            tags: vec![],
            loopback_only: false,
            status: "passing".into(),
        };
        assert_eq!(registration.base_url(), "http://[2001:db8::1]:6240");
    }

    /// A read-only check against the real local agent: it must parse what
    /// the agent answers and never hang. Ignored because it depends on a
    /// Consul agent on this Mac; run it with `--ignored` on a Mac in the
    /// cluster. It only reads (`GET`), and it asks for the service that is
    /// registered today as well as the one the switch registers.
    #[test]
    #[ignore = "reads the real local Consul agent at 127.0.0.1:8500 (TL-6, read-only)"]
    fn live_local_agent_answers_and_parses() {
        let base = base_address(std::env::var("CONSUL_HTTP_ADDR").ok().as_deref());
        for service in ["machine-service", "vm-service"] {
            let rows = instances(&base, service, Duration::from_secs(5)).expect("health answer");
            for row in rows {
                assert!(!row.node.is_empty() && row.port != 0, "{row:?}");
            }
        }
    }
}
