//! Pure acquisition reporting for stock Tart; no probing or process creation.
//!
//! Port of `bin/acquisition_options.py`. The crate resolves a requested
//! acquisition configuration against the service defaults and describes the
//! acquisition options a client may send. Neither function touches a process,
//! a VM, or the filesystem.

use serde_json::{json, Map, Value};

/// Error raised by [`resolve`].
///
/// The `Display` text of each variant is exactly the `ValueError` text raised by
/// the Python implementation.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// `wait` or `vnc` was not a boolean.
    ///
    /// Unreachable from Rust, where both are typed `bool`; retained so the
    /// error surface matches the Python source.
    #[error("wait and vnc must be booleans")]
    NonBooleanFlags,
    /// A resource dimension was present but not a positive integer.
    #[error("{0} must be a positive integer or null")]
    InvalidResource(&'static str),
    /// `ttl_hours` was not a finite number within `[0.1, 720]`.
    #[error("ttl_hours must be finite and within [0.1, 720]")]
    InvalidTtl,
}

/// CPU and memory defaults configured by an image's `line.conf`.
///
/// A dimension is `None` when the image does not configure it, and the service
/// default (6 CPUs, 16384 MB) applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageDefaults {
    /// The image's `CPU`.
    pub cpu: Option<i64>,
    /// The image's `MEMORY_MB`.
    pub memory_mb: Option<i64>,
}

/// Resolve a requested acquisition configuration against service defaults.
///
/// `cpu`, `memory_mb`, and `disk_gb` are `None` when the caller did not request
/// them. `ttl_hours` is a JSON value because the Python function accepts an
/// integer, a float, or a string and rejects booleans, non-finite values, and
/// values outside `[0.1, 720]`.
pub fn resolve(
    cpu: Option<i64>,
    memory_mb: Option<i64>,
    disk_gb: Option<i64>,
    wait: bool,
    ttl_hours: &Value,
    vnc: bool,
) -> Result<Value, ResolveError> {
    resolve_for_image(cpu, memory_mb, disk_gb, wait, ttl_hours, vnc, ImageDefaults::default())
}

/// Resolve a requested acquisition configuration against the image's defaults.
///
/// An omitted CPU or memory takes the image's configured value, and the
/// service default only when the image configures none.
pub fn resolve_for_image(
    cpu: Option<i64>,
    memory_mb: Option<i64>,
    disk_gb: Option<i64>,
    wait: bool,
    ttl_hours: &Value,
    vnc: bool,
    image: ImageDefaults,
) -> Result<Value, ResolveError> {
    for (name, value) in [("cpu", cpu), ("memory_mb", memory_mb), ("disk_gb", disk_gb)] {
        if let Some(value) = value {
            if value <= 0 {
                return Err(ResolveError::InvalidResource(name));
            }
        }
    }
    let ttl = parse_ttl(ttl_hours)?;

    let requested = json!({
        "cpu": cpu,
        "memory_mb": memory_mb,
        "disk_gb": disk_gb,
        "wait": wait,
        "ttl_hours": ttl,
        "vnc": vnc,
    });

    let effective = json!({
        "cpu": cpu.or(image.cpu).unwrap_or(6),
        "memory_mb": memory_mb.or(image.memory_mb).unwrap_or(16384),
        "disk_gb": disk_gb,
        "disk_mode": if disk_gb.is_none() { "inherit" } else { "resize" },
        "network": "nat",
        "headless": true,
        "vnc": vnc,
        "initial_ttl_hours": ttl,
        "readiness": {
            "mode": if wait { "normal" } else { "bounded" },
            "transfer_verification": true,
        },
    });

    let mut sources = Map::new();
    for (name, value, configured) in [
        ("cpu", cpu, image.cpu),
        ("memory_mb", memory_mb, image.memory_mb),
        ("disk_gb", disk_gb, None),
    ] {
        let source = if value.is_some() {
            "request"
        } else if name == "disk_gb" {
            "source-image"
        } else if configured.is_some() {
            "image-configuration"
        } else {
            "service-default"
        };
        sources.insert(name.to_string(), Value::String(source.to_string()));
    }

    Ok(json!({
        "schemaVersion": 1,
        "requested": requested,
        "effective": effective,
        "sources": Value::Object(sources),
        "resources_applied": {
            "status": "pending",
            "observed_at": null,
            "source": "tart-set-exit-status",
        },
    }))
}

/// Describe the acquisition options a client may send.
///
/// `backends` is embedded verbatim under the `vnc` option. `images` is `None`
/// when no startup image configuration was loaded; an empty list is treated the
/// same way for `images` but still reports the configuration as `configured`,
/// exactly as the Python `images or []` / `images is not None` pair does.
pub fn descriptor(backends: &Value, images: Option<&Value>) -> Value {
    let mut options = Map::new();
    options.insert(
        "purpose".into(),
        json!({"type": "string", "required": true}),
    );
    options.insert(
        "image".into(),
        json!({"type": "string", "default": "macos26"}),
    );
    options.insert(
        "env".into(),
        json!({"type": "string", "default": "default"}),
    );
    options.insert(
        "ttl_hours".into(),
        json!({"type": "number", "default": 24, "minimum": 0.1, "maximum": 720}),
    );
    options.insert(
        "wait".into(),
        json!({"type": "boolean", "default": true, "synchronous": true}),
    );
    options.insert(
        "source".into(),
        json!({"type": "string", "default": "base", "values": ["base", "work"]}),
    );
    options.insert(
        "network".into(),
        json!({"type": "string", "default": "nat", "values": ["nat"]}),
    );
    options.insert(
        "vnc".into(),
        json!({
            "type": "boolean",
            "default": false,
            "backends": Value::clone(backends),
            "viewer_location": "service-host",
            "runtime": "unmodified-tart",
        }),
    );
    options.insert(
        "expected_source_fingerprint".into(),
        json!({
            "type": "object",
            "nullable": false,
            "omit_when_unset": true,
            "applicable_when": {"source": "work"},
        }),
    );
    for name in ["cpu", "memory_mb", "disk_gb"] {
        options.insert(
            name.into(),
            json!({
                "type": "integer",
                "nullable": true,
                "minimum": 1,
                "default": null,
            }),
        );
    }

    let image_configuration = json!({
        "status": if images.is_some() { "configured" } else { "unverified" },
        "images": match images {
            Some(value) if is_truthy(value) => Value::clone(value),
            _ => json!([]),
        },
        "source": "startup-image-configuration",
    });

    json!({
        "schemaVersion": 1,
        "service": "vm-service",
        "options": Value::Object(options),
        "image_configuration": image_configuration,
        "runtime_readiness": "unverified",
        "note": "Configuration is not live boot, guest sharing, authentication, or pixels.",
    })
}

/// Convert the accepted `ttl_hours` representations to `f64`, matching
/// Python's `float()` plus the finiteness and range gate.
fn parse_ttl(value: &Value) -> Result<f64, ResolveError> {
    let ttl = match value {
        Value::Number(number) => number.as_f64().ok_or(ResolveError::InvalidTtl)?,
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .map_err(|_| ResolveError::InvalidTtl)?,
        _ => return Err(ResolveError::InvalidTtl),
    };
    if !ttl.is_finite() || !(0.1..=720.0).contains(&ttl) {
        return Err(ResolveError::InvalidTtl);
    }
    Ok(ttl)
}

/// Python truthiness for the `images or []` fallback.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_resolve() -> Value {
        resolve(None, None, None, true, &json!(24), false).unwrap()
    }

    #[test]
    fn defaults() {
        let result = default_resolve();
        assert_eq!(result["effective"]["cpu"], json!(6));
        assert_eq!(result["effective"]["memory_mb"], json!(16384));
        assert!(result["effective"]["disk_gb"].is_null());
        assert_eq!(result["effective"]["vnc"], json!(false));
        assert_eq!(
            result["effective"]["initial_ttl_hours"].as_f64(),
            Some(24.0)
        );
        assert_eq!(result["effective"]["disk_mode"], json!("inherit"));
        assert_eq!(result["effective"]["network"], json!("nat"));
        assert_eq!(result["effective"]["headless"], json!(true));
        assert_eq!(result["effective"]["readiness"]["mode"], json!("normal"));
        assert_eq!(result["sources"]["cpu"], json!("service-default"));
        assert_eq!(result["sources"]["memory_mb"], json!("service-default"));
        assert_eq!(result["sources"]["disk_gb"], json!("source-image"));
        assert_eq!(result["resources_applied"]["status"], json!("pending"));
        assert!(result["resources_applied"]["observed_at"].is_null());
        assert!(result["requested"]["cpu"].is_null());
        assert!(result["requested"]["ttl_hours"].is_number());
    }

    #[test]
    fn explicit_resources_are_reported_as_requested() {
        let result = resolve(Some(8), Some(8192), Some(64), false, &json!(0.1), true).unwrap();
        assert_eq!(result["effective"]["cpu"], json!(8));
        assert_eq!(result["effective"]["memory_mb"], json!(8192));
        assert_eq!(result["effective"]["disk_gb"], json!(64));
        assert_eq!(result["effective"]["disk_mode"], json!("resize"));
        assert_eq!(result["effective"]["vnc"], json!(true));
        assert_eq!(result["effective"]["readiness"]["mode"], json!("bounded"));
        assert_eq!(result["sources"]["cpu"], json!("request"));
        assert_eq!(result["sources"]["memory_mb"], json!("request"));
        assert_eq!(result["sources"]["disk_gb"], json!("request"));
    }

    #[test]
    fn omitted_resources_take_the_image_configuration() {
        let image = ImageDefaults { cpu: Some(4), memory_mb: Some(8192) };
        let result = resolve_for_image(None, None, None, true, &json!(24), false, image).unwrap();
        assert_eq!(result["effective"]["cpu"], json!(4));
        assert_eq!(result["effective"]["memory_mb"], json!(8192));
        assert_eq!(result["sources"]["cpu"], json!("image-configuration"));
        assert_eq!(result["sources"]["memory_mb"], json!("image-configuration"));
        assert_eq!(result["sources"]["disk_gb"], json!("source-image"));
        assert!(result["requested"]["memory_mb"].is_null());
    }

    #[test]
    fn a_request_overrides_the_image_configuration() {
        let image = ImageDefaults { cpu: Some(4), memory_mb: Some(8192) };
        let result =
            resolve_for_image(Some(2), None, None, true, &json!(24), false, image).unwrap();
        assert_eq!(result["effective"]["cpu"], json!(2));
        assert_eq!(result["sources"]["cpu"], json!("request"));
        assert_eq!(result["effective"]["memory_mb"], json!(8192));
    }

    #[test]
    fn an_unconfigured_image_dimension_takes_the_service_default() {
        let image = ImageDefaults { cpu: None, memory_mb: Some(8192) };
        let result = resolve_for_image(None, None, None, true, &json!(24), false, image).unwrap();
        assert_eq!(result["effective"]["cpu"], json!(6));
        assert_eq!(result["sources"]["cpu"], json!("service-default"));
    }

    #[test]
    fn invalid_integer_resources() {
        for value in [Some(0i64), Some(-1i64)] {
            let err = resolve(value, None, None, true, &json!(24), false).unwrap_err();
            assert_eq!(err.to_string(), "cpu must be a positive integer or null");
            let err = resolve(None, value, None, true, &json!(24), false).unwrap_err();
            assert_eq!(
                err.to_string(),
                "memory_mb must be a positive integer or null"
            );
            let err = resolve(None, None, value, true, &json!(24), false).unwrap_err();
            assert_eq!(
                err.to_string(),
                "disk_gb must be a positive integer or null"
            );
        }
    }

    #[test]
    fn invalid_ttl_values() {
        let invalid = [
            json!(null),
            json!(true),
            json!(false),
            json!("nan"),
            json!("inf"),
            json!("-inf"),
            json!(0),
            json!(0.05),
            json!(721),
            json!(-1),
            json!({}),
            json!([]),
            json!("not-a-number"),
        ];
        for ttl in invalid {
            let err = resolve(None, None, None, true, &ttl, false).unwrap_err();
            assert_eq!(
                err.to_string(),
                "ttl_hours must be finite and within [0.1, 720]",
                "ttl={ttl}"
            );
        }
    }

    #[test]
    fn ttl_string_is_parsed() {
        let result = resolve(None, None, None, true, &json!("1"), false).unwrap();
        assert_eq!(result["effective"]["initial_ttl_hours"].as_f64(), Some(1.0));
        assert_eq!(result["requested"]["ttl_hours"].as_f64(), Some(1.0));
    }

    #[test]
    fn ttl_boundaries_are_accepted() {
        for ttl in [json!(0.1), json!(720)] {
            assert!(resolve(None, None, None, true, &ttl, false).is_ok());
        }
    }

    #[test]
    fn descriptor_without_images() {
        let backends = json!({"tart": "unmodified"});
        let descriptor = descriptor(&backends, None);
        assert_eq!(descriptor["schemaVersion"], json!(1));
        assert_eq!(descriptor["service"], json!("vm-service"));
        assert_eq!(descriptor["options"]["vnc"]["backends"], backends);
        assert_eq!(
            descriptor["options"]["vnc"]["viewer_location"],
            json!("service-host")
        );
        assert_eq!(
            descriptor["options"]["vnc"]["runtime"],
            json!("unmodified-tart")
        );
        assert_eq!(descriptor["options"]["purpose"]["required"], json!(true));
        assert_eq!(descriptor["options"]["cpu"]["default"], Value::Null);
        assert_eq!(descriptor["options"]["disk_gb"]["minimum"], json!(1));
        assert_eq!(
            descriptor["image_configuration"]["status"],
            json!("unverified")
        );
        assert_eq!(descriptor["image_configuration"]["images"], json!([]));
        assert_eq!(descriptor["runtime_readiness"], json!("unverified"));
    }

    #[test]
    fn descriptor_with_images() {
        let backends = json!({});
        let images = json!([{"name": "macos26"}]);
        let descriptor = descriptor(&backends, Some(&images));
        assert_eq!(
            descriptor["image_configuration"]["status"],
            json!("configured")
        );
        assert_eq!(descriptor["image_configuration"]["images"], images);
    }

    #[test]
    fn descriptor_with_empty_image_list_still_reports_configured() {
        let backends = json!({});
        let empty = json!([]);
        let descriptor = descriptor(&backends, Some(&empty));
        assert_eq!(
            descriptor["image_configuration"]["status"],
            json!("configured")
        );
        assert_eq!(descriptor["image_configuration"]["images"], json!([]));
    }
}
