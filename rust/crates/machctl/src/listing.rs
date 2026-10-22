//! `machctl list`: one table of every machine from every instance
//! (`docs/ux/cli/list.md` CL-1).

use serde_json::{json, Map, Value};

use crate::capability;
use crate::federation::{Instance, Merged};

/// The filters of `machctl list` (CL-1).
#[derive(Debug, Clone, Default)]
pub struct Filters {
    /// `--kind durable` or `--kind virtual`.
    pub kind: Option<String>,
    /// `--require <capability>`, every one required (CL-2).
    pub require: Vec<String>,
}

impl Filters {
    fn active(&self) -> bool {
        self.kind.is_some() || !self.require.is_empty()
    }

    fn keeps(&self, record: &Value) -> bool {
        self.kind
            .as_deref()
            .is_none_or(|kind| capability::kind(record) == kind)
            && capability::has_all(record, &self.require)
    }
}

/// One instance's machines after the filters.
fn kept(instance: &Instance, filters: &Filters) -> Vec<Value> {
    instance
        .machines()
        .into_iter()
        .filter(|record| filters.keeps(record))
        .collect()
}

/// The merged list as JSON: one object per instance with its `instance`,
/// `address`, `reachable`, `error` and `machines` (CL-1), plus whether it
/// is the local instance, its Consul health, and its image catalog status.
pub fn to_json(merged: &Merged, filters: &Filters) -> Value {
    let mut out = Vec::new();
    for instance in &merged.instances {
        let machines = kept(instance, filters);
        if filters.active() && instance.reachable && machines.is_empty() {
            continue;
        }
        let mut object = Map::new();
        object.insert("instance".into(), json!(instance.label));
        object.insert("address".into(), json!(instance.address));
        object.insert("local".into(), json!(instance.local));
        object.insert("reachable".into(), json!(instance.reachable));
        object.insert("error".into(), json!(instance.error));
        object.insert("consul".into(), json!(instance.consul));
        object.insert(
            "catalog".into(),
            instance
                .body
                .as_ref()
                .and_then(|body| body.get("catalog"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        object.insert("machines".into(), Value::Array(machines));
        out.push(Value::Object(object));
    }
    Value::Array(out)
}

const HEADERS: [&str; 9] = [
    "INSTANCE",
    "MACHINE",
    "KIND",
    "STATE",
    "LEASE (PURPOSE, UNTIL)",
    "CHIP / OS",
    "GPU",
    "FREE DISK",
    "CI",
];
/// The narrowest each column is drawn, from CL-1's example.
const MIN_WIDTHS: [usize; 9] = [16, 31, 8, 10, 29, 19, 14, 10, 0];
/// The column a spanning row's own text starts in: an unreachable row
/// prints its state and its error unpadded, as CL-1 shows it.
const SPAN_FROM: usize = 3;

enum Line {
    /// A table row; a spanning row prints its last cell unpadded.
    Row { cells: Vec<String>, span: bool },
    /// A line printed as is, such as a warning.
    Text(String),
}

/// The human-readable table (CL-1).
pub fn format_table(merged: &Merged, filters: &Filters) -> String {
    let mut lines = Vec::new();
    for instance in &merged.instances {
        let label = instance.label.clone();
        if !instance.reachable {
            let error = instance.error.as_deref().unwrap_or("no answer");
            let consul = match (
                &instance.consul,
                merged.consul_asked && merged.consul_error.is_none(),
            ) {
                (Some(status), _) => format!(" (Consul: {status})"),
                (None, true) => " (Consul: not registered)".to_string(),
                (None, false) => String::new(),
            };
            lines.push(Line::Row {
                cells: vec![
                    label,
                    "-".into(),
                    "-".into(),
                    "unreachable".into(),
                    format!("{error}{consul}"),
                ],
                span: true,
            });
            continue;
        }
        let machines = kept(instance, filters);
        if filters.active() && machines.is_empty() {
            continue;
        }
        if let Some(warning) = instance.body.as_ref().and_then(crate::catalog_warning) {
            let rest = warning
                .strip_prefix("machctl: warning: ")
                .unwrap_or(&warning);
            lines.push(Line::Text(format!("machctl: warning: {label}: {rest}")));
        }
        if machines.is_empty() {
            lines.push(Line::Row {
                cells: vec![label, "-".into(), "-".into(), "none".into()],
                span: false,
            });
            continue;
        }
        for record in &machines {
            lines.push(Line::Row {
                cells: row(&label, record),
                span: false,
            });
            for warning in warnings(record) {
                lines.push(Line::Text(format!("    warning: {warning}")));
            }
        }
    }

    let mut widths = MIN_WIDTHS;
    for line in &lines {
        if let Line::Row { cells, span } = line {
            for (index, cell) in cells.iter().enumerate() {
                if *span && index >= SPAN_FROM {
                    continue;
                }
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
    }
    let render = |cells: &[String], span: bool| {
        let mut text = String::new();
        for (index, cell) in cells.iter().enumerate() {
            if span && index >= SPAN_FROM {
                text.push_str(&cells[index..].join("  "));
                break;
            }
            if index + 1 == cells.len() {
                text.push_str(cell);
                break;
            }
            text.push_str(&format!("{cell:<width$} ", width = widths[index]));
        }
        format!("{}\n", text.trim_end())
    };
    let header: Vec<String> = HEADERS.iter().map(|header| header.to_string()).collect();
    let mut out = render(&header, false);
    for line in &lines {
        match line {
            Line::Row { cells, span } => out.push_str(&render(cells, *span)),
            Line::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

fn text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn capabilities<'a>(record: &'a Value, section: &str) -> Option<&'a Value> {
    record
        .get("capabilities")
        .and_then(|capabilities| capabilities.get(section))
        .filter(|value| !value.is_null())
}

/// One machine's cells.
fn row(instance: &str, record: &Value) -> Vec<String> {
    let name = text(record.get("name")).unwrap_or("-").to_string();
    let kind = capability::kind(record).to_string();
    let state = text(record.get("state")).unwrap_or("-").to_string();
    vec![
        instance.to_string(),
        name,
        kind,
        state,
        lease(record),
        chip_os(record),
        gpu(record),
        free_disk(record),
        ci(record),
    ]
}

/// `purpose, HH:MM` for a lease, the acquirable figure for an offer.
fn lease(record: &Value) -> String {
    if capability::is_offer(record) {
        let concurrency = record.get("concurrency");
        let number = |key: &str| {
            concurrency
                .and_then(|concurrency| concurrency.get(key))
                .and_then(Value::as_i64)
        };
        if !capability::offer_acquirable(record) {
            return "not acquirable".to_string();
        }
        return match (number("running"), number("limit")) {
            (Some(running), Some(limit)) if limit > 0 => {
                format!("acquirable {} of {limit}", (limit - running).max(0))
            }
            _ => "acquirable".to_string(),
        };
    }
    match text(record.get("purpose")) {
        None => "-".to_string(),
        Some(purpose) => match record.get("ttl_expires_at").and_then(Value::as_f64) {
            Some(until) => format!("{purpose}, {}", local_time(until)),
            None => purpose.to_string(),
        },
    }
}

/// The OS's display name for an image kind.
fn os_family(kind: &str) -> &str {
    match kind {
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

/// `chip / OS` for a durable machine, `OS (image)` for a virtual machine.
fn chip_os(record: &Value) -> String {
    let os = capabilities(record, "os").and_then(|os| {
        let name = text(os.get("name"))?;
        Some(match text(os.get("version")) {
            Some(version) => format!("{name} {version}"),
            None => name.to_string(),
        })
    });
    if capability::kind(record) == "durable" {
        let chip = capabilities(record, "hardware")
            .and_then(|hardware| text(hardware.get("chip")))
            .map(|chip| chip.strip_prefix("Apple ").unwrap_or(chip).to_string());
        let parts: Vec<String> = chip.into_iter().chain(os).collect();
        return if parts.is_empty() {
            "-".to_string()
        } else {
            parts.join(" / ")
        };
    }
    let os = os.or_else(|| text(record.get("image_kind")).map(|kind| os_family(kind).to_string()));
    let image = text(record.get("image")).or_else(|| text(record.get("line")));
    match (os, image, capability::is_offer(record)) {
        (Some(os), _, true) => os,
        (Some(os), Some(image), false) => format!("{os} ({image})"),
        (Some(os), None, false) => os,
        (None, Some(image), _) => format!("({image})"),
        (None, None, _) => "-".to_string(),
    }
}

/// `metal3`, else the Metal families.
fn gpu(record: &Value) -> String {
    let Some(gpu) = capabilities(record, "gpu") else {
        return "-".to_string();
    };
    if gpu.get("metal3").and_then(Value::as_bool) == Some(true) {
        return "metal3".to_string();
    }
    let families: Vec<&str> = gpu
        .get("metal_families")
        .and_then(Value::as_array)
        .map(|families| families.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if families.is_empty() {
        "-".to_string()
    } else {
        families.join(",")
    }
}

fn free_disk(record: &Value) -> String {
    capabilities(record, "disk")
        .and_then(|disk| disk.get("free_bytes"))
        .and_then(Value::as_f64)
        .map(|bytes| format!("{} GiB", (bytes / f64::from(1u32 << 30)).floor() as u64))
        .unwrap_or_else(|| "-".to_string())
}

/// `busy` or `idle` for a Mac with a CI runner, `-` otherwise.
fn ci(record: &Value) -> String {
    let Some(runner) = capabilities(record, "ci_runner") else {
        return "-".to_string();
    };
    if runner.get("present").and_then(Value::as_bool) != Some(true) {
        return "-".to_string();
    }
    match runner.get("busy").and_then(Value::as_bool) {
        Some(true) => "busy".to_string(),
        Some(false) => "idle".to_string(),
        None => "-".to_string(),
    }
}

/// The record's warnings, as strings or objects with a `message`.
fn warnings(record: &Value) -> Vec<String> {
    record
        .get("warnings")
        .and_then(Value::as_array)
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(|warning| {
                    warning
                        .as_str()
                        .or_else(|| warning.get("message").and_then(Value::as_str))
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Unix seconds as local `HH:MM`.
fn local_time(seconds: f64) -> String {
    let time = seconds.floor() as libc::time_t;
    // SAFETY: `localtime_r` writes into a caller-provided `tm` and reads only
    // `time`; both live for the call.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return "-".to_string();
        }
        tm
    };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(
        label: &str,
        local: bool,
        body: Option<Value>,
        error: Option<&str>,
        consul: Option<&str>,
    ) -> Instance {
        Instance {
            label: label.to_string(),
            id: Some(label.to_string()),
            address: format!("http://192.0.2.1:6240/{label}"),
            local,
            consul: consul.map(str::to_string),
            unaskable: None,
            reachable: body.is_some(),
            error: error.map(str::to_string),
            body,
        }
    }

    fn durable(name: &str, state: &str, purpose: Option<&str>, busy: bool) -> Value {
        json!({
            "name": name, "kind": "durable", "state": state, "purpose": purpose,
            "ttl_expires_at": null,
            "capabilities": {
                "os": {"name": "macOS", "version": "26.3", "build": "25D"},
                "hardware": {"chip": "Apple M4"},
                "disk": {"free_bytes": 137_438_953_472u64},
                "gpu": {"metal3": true, "metal_families": ["apple9", "metal3"]},
                "ci_runner": {"present": true, "busy": busy, "name": "r"}
            },
            "warnings": if busy { json!(["low free disk"]) } else { json!([]) }
        })
    }

    fn merged(instances: Vec<Instance>) -> Merged {
        Merged {
            instances,
            consul_asked: true,
            consul_error: None,
            notes: Vec::new(),
        }
    }

    /// CL-1: one table, grouped by instance, durable first, then virtual
    /// machines, then offers; an unreachable instance keeps one row with its
    /// error and its Consul health.
    #[test]
    fn ac34_table_groups_by_instance_and_keeps_unreachable_rows() {
        let local_body = json!({
            "machines": {"pilot-mac-x-7f3a": {"state": "running", "image": "macos26",
                                              "image_kind": "macos", "purpose": "x"}},
            "offers": [{"name": "macos26", "kind": "virtual", "image_kind": "macos",
                        "concurrency": {"running": 1, "limit": 2, "acquirable": true},
                        "capabilities": {"gpu": {"metal3": false,
                                                 "metal_families": ["mac2", "common3"]}}}],
            "catalog": {"status": "current", "generation": 1}
        });
        let list = merged(vec![
            instance("mac-a", true, Some(local_body), None, Some("passing")),
            instance(
                "mac-b",
                false,
                Some(
                    json!({"machines": {"mac-b": durable("mac-b", "leased", Some("post"), true)}}),
                ),
                None,
                Some("passing"),
            ),
            instance(
                "mac-c",
                false,
                None,
                Some("connection refused"),
                Some("critical"),
            ),
        ]);
        let table = format_table(&list, &Filters::default());
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].starts_with("INSTANCE         MACHINE "), "{table}");
        assert!(
            lines[1].starts_with("mac-a") && lines[1].contains("pilot-mac-x-7f3a"),
            "{table}"
        );
        assert!(
            lines[1].contains("running") && lines[1].contains("macOS (macos26)"),
            "{table}"
        );
        assert!(
            lines[2].contains("offer") && lines[2].contains("acquirable 1 of 2"),
            "{table}"
        );
        assert!(lines[2].contains("mac2,common3"), "{table}");
        assert!(
            lines[3].starts_with("mac-b") && lines[3].contains("durable"),
            "{table}"
        );
        assert!(
            lines[3].contains("M4 / macOS 26.3") && lines[3].contains("metal3"),
            "{table}"
        );
        assert!(
            lines[3].contains("128 GiB") && lines[3].ends_with("busy"),
            "{table}"
        );
        assert_eq!(lines[4], "    warning: low free disk");
        assert!(lines[5].starts_with("mac-c"), "{table}");
        assert_eq!(
            lines[5],
            "mac-c            -                               -        unreachable  connection refused (Consul: critical)"
        );
        assert_eq!(lines.len(), 6);
    }

    /// CL-1: a stale catalog's warning stands above its instance's rows.
    #[test]
    fn catalog_warning_names_the_instance() {
        let body = json!({"machines": {}, "catalog": {
            "status": "stale", "generation": 4, "error": "parse failed",
            "failed_at": 0.0, "file": "/images/x/line.conf"}});
        let table = format_table(
            &merged(vec![instance("mac-a", true, Some(body), None, None)]),
            &Filters::default(),
        );
        let lines: Vec<&str> = table.lines().collect();
        assert!(
            lines[1].starts_with("machctl: warning: mac-a: image catalog stale since"),
            "{table}"
        );
        assert!(
            lines[2].starts_with("mac-a") && lines[2].contains("none"),
            "{table}"
        );
    }

    /// CL-1: `--kind` and `--require` keep matching machines, drop
    /// instances with none, and never drop an unreachable instance.
    #[test]
    fn filters_keep_matches_and_unreachable_rows() {
        let list = merged(vec![
            instance(
                "mac-a",
                true,
                Some(json!({"machines": {"vm": {"state": "running"}}})),
                None,
                None,
            ),
            instance(
                "mac-b",
                false,
                Some(json!({"machines": {"mac-b": durable("mac-b", "available", None, false)}})),
                None,
                Some("passing"),
            ),
            instance(
                "mac-c",
                false,
                None,
                Some("timed out after 5 s"),
                Some("passing"),
            ),
        ]);
        let filters = Filters {
            kind: Some("durable".into()),
            require: vec!["metal3".into(), "ci-idle".into()],
        };
        let table = format_table(&list, &filters);
        assert!(!table.contains("mac-a"), "{table}");
        assert!(table.contains("mac-b") && table.contains("idle"), "{table}");
        assert!(
            table.contains("mac-c") && table.contains("unreachable"),
            "{table}"
        );

        let json = to_json(&list, &filters);
        let instances: Vec<&str> = json
            .as_array()
            .unwrap()
            .iter()
            .map(|instance| instance["instance"].as_str().unwrap())
            .collect();
        assert_eq!(instances, ["mac-b", "mac-c"]);
    }

    /// CL-1: the JSON form carries `instance`, `address`, `reachable`,
    /// `error` and `machines` for every instance.
    #[test]
    fn ac34_json_names_every_instance() {
        let list = merged(vec![
            instance(
                "mac-a",
                true,
                Some(json!({"machines": {"vm": {"state": "running"}}})),
                None,
                None,
            ),
            instance(
                "mac-c",
                false,
                None,
                Some("connection refused"),
                Some("critical"),
            ),
        ]);
        let json = to_json(&list, &Filters::default());
        assert_eq!(json[0]["instance"], "mac-a");
        assert_eq!(json[0]["reachable"], true);
        assert_eq!(json[0]["machines"][0]["name"], "vm");
        assert_eq!(json[1]["reachable"], false);
        assert_eq!(json[1]["error"], "connection refused");
        assert_eq!(json[1]["consul"], "critical");
        assert_eq!(json[1]["machines"], json!([]));
    }

    #[test]
    fn local_time_is_hours_and_minutes() {
        let text = local_time(0.0);
        assert_eq!(text.len(), 5);
        assert_eq!(&text[2..3], ":");
    }
}
