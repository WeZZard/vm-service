//! Port of `bin/environment_config.py`: profile validation, canonicalization,
//! identity fingerprinting and environment export.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::EnvironmentError;

/// Keys whose values are canonicalized filesystem paths.
const PATH_KEYS: [&str; 7] = [
    "imageRepository",
    "tartHome",
    "serviceStateDir",
    "imageStateDir",
    "relayStateDir",
    "vmctlPath",
    "tartPath",
];

/// Path keys that must be mutually disjoint and disjoint from image/legacy state.
const ROOT_KEYS: [&str; 4] = [
    "tartHome",
    "serviceStateDir",
    "imageStateDir",
    "relayStateDir",
];

/// The exact schema field set of a profile.
const FIELDS: [&str; 10] = [
    "schemaVersion",
    "id",
    "vmServiceUrl",
    "imageRepository",
    "tartHome",
    "serviceStateDir",
    "imageStateDir",
    "relayStateDir",
    "vmctlPath",
    "tartPath",
];

/// Validate and canonicalize a profile without creating any files.
///
/// `environ` supplies `HOME` and `XDG_STATE_HOME`, mirroring the Python
/// `environ=None` parameter (the caller passes the environment explicitly).
pub fn canonical_profile(
    raw: &Value,
    environ: &HashMap<String, String>,
) -> Result<Value, EnvironmentError> {
    let object = raw.as_object().ok_or_else(|| {
        EnvironmentError::msg("profile must contain exactly the supported schema fields")
    })?;
    if object.len() != FIELDS.len() || !FIELDS.iter().all(|field| object.contains_key(*field)) {
        return Err(EnvironmentError::msg(
            "profile must contain exactly the supported schema fields",
        ));
    }

    let version = object
        .get("schemaVersion")
        .expect("field present by construction");
    if !is_schema_one(version) {
        return Err(EnvironmentError::msg("schemaVersion must be integer 1"));
    }

    match object.get("id").and_then(Value::as_str) {
        Some(id) if valid_id(id) => {}
        _ => {
            return Err(EnvironmentError::msg(
                "id must be a lowercase slug of 1..48 characters",
            ))
        }
    }

    let mut profile = object.clone();
    profile.insert("schemaVersion".to_string(), Value::from(1));
    for key in PATH_KEYS {
        let value = profile.get(key).expect("field present by construction");
        let canonical = absolute(value, key)?;
        profile.insert(key.to_string(), Value::String(canonical));
    }

    let repo = PathBuf::from(
        profile["imageRepository"]
            .as_str()
            .expect("canonicalized path is a string"),
    );
    if !repo.is_dir() || !repo.join("images").is_dir() {
        return Err(EnvironmentError::msg(
            "imageRepository must exist and contain images/",
        ));
    }

    for key in ["vmctlPath", "tartPath"] {
        let raw_path = profile[key]
            .as_str()
            .expect("canonicalized path is a string");
        let path = Path::new(raw_path);
        if !path.is_file() || !is_executable(path) {
            return Err(EnvironmentError::msg(format!(
                "{key} must be an executable regular file"
            )));
        }
    }

    let home_raw = environ.get("HOME").cloned().unwrap_or_else(home_directory);
    let home = absolute(&Value::String(home_raw), "HOME")?;
    let xdg_raw = environ
        .get("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            Path::new(&home)
                .join(".local/state")
                .to_string_lossy()
                .into_owned()
        });
    let xdg = absolute(&Value::String(xdg_raw), "XDG_STATE_HOME")?;

    let mut legacy: Vec<PathBuf> = Vec::new();
    legacy.push(Path::new(&home).join(".tart"));
    for name in ["vm-service", "pilot-images", "pi-vm-relay", "mcp-vm-relay"] {
        legacy.push(Path::new(&xdg).join(name));
    }
    // Also protect the defaults when XDG_STATE_HOME redirects legacy state.
    for name in ["vm-service", "pilot-images", "pi-vm-relay", "mcp-vm-relay"] {
        legacy.push(Path::new(&home).join(".local/state").join(name));
    }

    let image_repository = Path::new(
        profile["imageRepository"]
            .as_str()
            .expect("canonicalized path is a string"),
    );
    for (index, key) in ROOT_KEYS.iter().enumerate() {
        let root = PathBuf::from(
            profile[*key]
                .as_str()
                .expect("canonicalized path is a string"),
        );
        if root.exists() && !root.is_dir() {
            return Err(EnvironmentError::msg(format!("{key} must be a directory")));
        }
        let mut overlaps = overlap(&root, image_repository);
        if !overlaps {
            for candidate in &legacy {
                let resolved = resolve_path(candidate)
                    .map_err(|error| EnvironmentError::msg(error.to_string()))?;
                if overlap(&root, &resolved) {
                    overlaps = true;
                    break;
                }
            }
        }
        if overlaps {
            return Err(EnvironmentError::msg(format!(
                "{key} overlaps an image repository or legacy root"
            )));
        }
        for other in &ROOT_KEYS[..index] {
            let other_root = Path::new(
                profile[*other]
                    .as_str()
                    .expect("canonicalized path is a string"),
            );
            if overlap(&root, other_root) {
                return Err(EnvironmentError::msg(
                    "mutable roots must be mutually disjoint",
                ));
            }
        }
    }

    let url = profile["vmServiceUrl"].as_str().ok_or_else(|| {
        EnvironmentError::msg("vmServiceUrl must be a loopback HTTP origin with explicit port")
    })?;
    let (host, port) = parse_origin(url).ok_or_else(|| {
        EnvironmentError::msg("vmServiceUrl must be a loopback HTTP origin with explicit port")
    })?;
    let rendered_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    profile.insert(
        "vmServiceUrl".to_string(),
        Value::String(format!("http://{rendered_host}:{port}")),
    );

    Ok(Value::Object(profile))
}

/// Return the cross-language identity of an already canonical profile.
pub fn profile_identity(profile: &Value) -> Value {
    let encoded = canonical_json(profile);
    let digest = Sha256::digest(&encoded);
    let mut identity = Map::new();
    identity.insert(
        "id".to_string(),
        profile.get("id").cloned().unwrap_or(Value::Null),
    );
    identity.insert("fingerprint".to_string(), Value::String(hex_lower(&digest)));
    for key in [
        "vmServiceUrl",
        "imageRepository",
        "tartHome",
        "serviceStateDir",
        "imageStateDir",
        "relayStateDir",
    ] {
        identity.insert(
            key.to_string(),
            profile.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(identity)
}

/// Return `{profile, identity, environment}`, or `None` when no selector is set.
pub fn load_environment(path: Option<&str>) -> Result<Option<Value>, EnvironmentError> {
    let environ: HashMap<String, String> = std::env::vars().collect();
    load_environment_with(path, &environ)
}

/// Same as [`load_environment`] but with an explicit environment mapping.
pub fn load_environment_with(
    path: Option<&str>,
    environ: &HashMap<String, String>,
) -> Result<Option<Value>, EnvironmentError> {
    let selected = match path {
        Some(selected) => Some(selected.to_string()),
        None => environ.get("VM_ENVIRONMENT_FILE").cloned(),
    };
    let selected = match selected {
        Some(selected) => selected,
        None => return Ok(None),
    };
    if selected.trim().is_empty() {
        return Err(EnvironmentError::msg(
            "VM_ENVIRONMENT_FILE must not be blank",
        ));
    }

    let lexical = {
        let candidate = Path::new(&selected);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| {
                    EnvironmentError::msg(format!(
                        "cannot load environment profile: {selected}: {error}"
                    ))
                })?
                .join(candidate)
        }
    };
    let filename = absolute(
        &Value::String(lexical.to_string_lossy().into_owned()),
        "VM_ENVIRONMENT_FILE",
    )?;
    if !Path::new(&filename).is_file() {
        return Err(EnvironmentError::msg("profile must be a regular JSON file"));
    }

    let loaded = (|| -> Result<Value, String> {
        let mut file = fs::File::open(&filename).map_err(|error| error.to_string())?;
        let mut buffer = Vec::new();
        file.by_ref()
            .take(65537)
            .read_to_end(&mut buffer)
            .map_err(|error| error.to_string())?;
        if buffer.len() > 65536 {
            return Err("profile exceeds 65536 bytes".to_string());
        }
        let text = String::from_utf8(buffer).map_err(|error| error.to_string())?;
        parse_strict(&text).map_err(|error| error.to_string())
    })();
    let raw = match loaded {
        Ok(raw) => raw,
        Err(error) => {
            return Err(EnvironmentError::msg(format!(
                "cannot load environment profile: {filename}: {error}"
            )))
        }
    };

    let profile = canonical_profile(&raw, environ)?;
    let canonical_url = profile["vmServiceUrl"]
        .as_str()
        .expect("canonical URL is a string");
    let (host, port) = split_origin(canonical_url);
    let identity = profile_identity(&profile);

    let mut exports = Map::new();
    exports.insert(
        "VM_ENVIRONMENT_FILE".to_string(),
        Value::String(filename.clone()),
    );
    exports.insert(
        "VM_ENVIRONMENT_FINGERPRINT".to_string(),
        identity["fingerprint"].clone(),
    );
    exports.insert("PILOT_REPO".to_string(), profile["imageRepository"].clone());
    exports.insert("TART_HOME".to_string(), profile["tartHome"].clone());
    exports.insert(
        "VM_SERVICE_STATE".to_string(),
        profile["serviceStateDir"].clone(),
    );
    exports.insert(
        "PILOT_IMAGES_STATE_DIR".to_string(),
        profile["imageStateDir"].clone(),
    );
    exports.insert(
        "VM_RELAY_STATE_DIR".to_string(),
        profile["relayStateDir"].clone(),
    );
    exports.insert("VM_RELAY_URL".to_string(), profile["vmServiceUrl"].clone());
    exports.insert("VM_SERVICE_HOST".to_string(), Value::String(host));
    exports.insert(
        "VM_SERVICE_PORT".to_string(),
        Value::String(port.to_string()),
    );
    exports.insert("VMCTL".to_string(), profile["vmctlPath"].clone());
    exports.insert("TART".to_string(), profile["tartPath"].clone());

    let mut bundle = Map::new();
    bundle.insert("profile".to_string(), profile);
    bundle.insert("identity".to_string(), identity);
    bundle.insert("environment".to_string(), Value::Object(exports));
    Ok(Some(Value::Object(bundle)))
}

// --- helpers -----------------------------------------------------------------

/// Canonicalize an absolute path exactly as Python `_absolute`.
pub(crate) fn absolute(value: &Value, name: &str) -> Result<String, EnvironmentError> {
    let text = match value.as_str() {
        Some(text) => text,
        None => {
            return Err(EnvironmentError::msg(format!(
                "{name} must be an absolute path"
            )))
        }
    };
    if text.is_empty() || text.chars().any(is_control_char) || !Path::new(text).is_absolute() {
        return Err(EnvironmentError::msg(format!(
            "{name} must be an absolute path"
        )));
    }

    let target = Path::new(text);
    let mut component = Some(target);
    while let Some(current) = component {
        if let Ok(metadata) = fs::symlink_metadata(current) {
            if metadata.file_type().is_symlink() && !current.exists() {
                return Err(EnvironmentError::msg(format!(
                    "{name} has a dangling symlink ancestor"
                )));
            }
        }
        component = current.parent();
    }

    match resolve_path(target) {
        Ok(resolved) => Ok(resolved.to_string_lossy().into_owned()),
        Err(_) => Err(EnvironmentError::msg(format!("{name} cannot be resolved"))),
    }
}

/// `lstat`-based, non-strict path resolution mirroring `pathlib.Path.resolve`.
pub(crate) fn resolve_path(path: &Path) -> io::Result<PathBuf> {
    let mut resolved: Vec<OsString> = Vec::new();
    let mut pending: VecDeque<OsString> = path
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect();
    let mut followed = 0usize;

    while let Some(component) = pending.pop_front() {
        if component == OsStr::new("/") || component == OsStr::new(".") {
            continue;
        }
        if component == OsStr::new("..") {
            resolved.pop();
            continue;
        }

        let mut candidate = PathBuf::from("/");
        for part in &resolved {
            candidate.push(part);
        }
        candidate.push(&component);

        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                followed += 1;
                if followed > 40 {
                    return Err(io::Error::other("too many levels of symbolic links"));
                }
                let target = fs::read_link(&candidate)?;
                if target.is_absolute() {
                    resolved.clear();
                }
                let mut target_parts: Vec<OsString> = target
                    .components()
                    .map(|part| part.as_os_str().to_os_string())
                    .collect();
                while let Some(part) = target_parts.pop() {
                    pending.push_front(part);
                }
            }
            Ok(_) => resolved.push(component),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                resolved.push(component);
                while let Some(rest) = pending.pop_front() {
                    if rest == OsStr::new(".") {
                        continue;
                    }
                    if rest == OsStr::new("..") {
                        resolved.pop();
                        continue;
                    }
                    resolved.push(rest);
                }
                break;
            }
            Err(error) => return Err(error),
        }
    }

    let mut output = PathBuf::from("/");
    for part in resolved {
        output.push(part);
    }
    Ok(output)
}

/// `_overlap`: equal, or one path is an ancestor of the other.
pub(crate) fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn is_control_char(character: char) -> bool {
    let code = character as u32;
    code <= 0x1f || code == 0x7f
}

fn is_schema_one(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer == 1
            } else if let Some(integer) = number.as_u64() {
                integer == 1
            } else if let Some(float) = number.as_f64() {
                float == 1.0
            } else {
                false
            }
        }
        _ => false,
    }
}

fn valid_id(id: &str) -> bool {
    let mut characters = id.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    let length = 1 + characters.clone().count();
    if length > 48 {
        return false;
    }
    characters.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

fn is_executable(path: &Path) -> bool {
    match CString::new(path.as_os_str().as_bytes()) {
        Ok(cpath) => unsafe { libc::access(cpath.as_ptr(), libc::X_OK) == 0 },
        Err(_) => false,
    }
}

fn home_directory() -> String {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return home;
        }
    }
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() && !(*entry).pw_dir.is_null() {
            let directory = std::ffi::CStr::from_ptr((*entry).pw_dir);
            return directory.to_string_lossy().into_owned();
        }
    }
    "/".to_string()
}

/// Parse `http://<host>:<port>[/]` under the same allow-list as the Python regex.
///
/// `bin/environment_config.py::canonical_profile` gates the value with
/// `re.fullmatch(r'http://(localhost|127\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|\[::1\]):[0-9]{1,5}/?')`
/// *before* it reaches `ipaddress.ip_address(host).is_loopback`. That regex
/// admits only those three host spellings, so the later `is_loopback` call only
/// ever sees `localhost`, a dotted `127.x.x.x` or `[::1]`. The `host_ok`
/// allow-list below is that regex; `is_loopback` then reproduces the
/// `ipaddress` leading-zero/range rules. IPv4-mapped and expanded spellings
/// that `ipaddress` reports as loopback (`[0:0:0:0:0:0:0:1]`,
/// `[::ffff:127.0.0.1]`, `[::ffff:7f00:1]`) are rejected earlier by the regex
/// and MUST stay rejected here.
fn parse_origin(value: &str) -> Option<(String, u16)> {
    let rest = value.strip_prefix("http://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    if !path.is_empty() {
        return None;
    }
    let (host, port_text) = authority.rsplit_once(':')?;
    if port_text.is_empty()
        || port_text.len() > 5
        || !port_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let host_ok = if host == "localhost" || host == "[::1]" {
        true
    } else if let Some(octets) = host.strip_prefix("127.") {
        let parts: Vec<&str> = octets.split('.').collect();
        parts.len() == 3
            && parts.iter().all(|part| {
                !part.is_empty() && part.len() <= 3 && part.bytes().all(|b| b.is_ascii_digit())
            })
    } else {
        false
    };
    if !host_ok {
        return None;
    }
    let port: u32 = port_text.parse().ok()?;
    if !(1..=65535).contains(&port) {
        return None;
    }
    let hostname = host.trim_start_matches('[').trim_end_matches(']');
    if hostname != "localhost" && !is_loopback(hostname) {
        return None;
    }
    Some((hostname.to_string(), port as u16))
}

/// Recover `(host, port)` from an already canonical origin.
fn split_origin(value: &str) -> (String, u16) {
    let rest = value.strip_prefix("http://").unwrap_or(value);
    let authority = rest.split_once('/').map(|(a, _)| a).unwrap_or(rest);
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, "0"));
    let hostname = host.trim_start_matches('[').trim_end_matches(']');
    (hostname.to_string(), port.parse().unwrap_or(0))
}

/// Mirror `ipaddress.ip_address(host).is_loopback` for the allowed host forms.
fn is_loopback(host: &str) -> bool {
    if host == "::1" {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let mut octets = Vec::with_capacity(4);
    for part in parts {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        // Python's ipaddress rejects leading zeros in IPv4 octets.
        if part.len() > 1 && part.starts_with('0') {
            return false;
        }
        match part.parse::<u16>() {
            Ok(octet) if octet <= 255 => octets.push(octet),
            _ => return false,
        }
    }
    octets[0] == 127
}

/// Compact, sorted-key JSON encoding used by `profile_identity`.
fn canonical_json(value: &Value) -> Vec<u8> {
    match value.as_object() {
        Some(object) => {
            let sorted: BTreeMap<&String, &Value> = object.iter().collect();
            serde_json::to_vec(&sorted).unwrap_or_default()
        }
        None => serde_json::to_vec(value).unwrap_or_default(),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

// --- strict JSON parsing (rejects duplicate keys) ----------------------------

fn parse_strict(text: &str) -> Result<Value, serde_json::Error> {
    // Reject duplicate keys the way Python's `object_pairs_hook` does.
    serde_json::from_str::<StrictValue>(text)?;
    // Then parse again with the standard value visitor, which is the one that
    // understands `serde_json`'s private number token. Under
    // `arbitrary_precision` a float reaches a custom visitor as a one-entry map
    // keyed by that token, so the strict pass above would leave `1.0` as an
    // object and every later type check would reject it. Integers arrive as a
    // plain `visit_i64`/`visit_u64` and are unaffected.
    let mut value: Value = serde_json::from_str(text)?;
    // CPython normalizes float literals when it loads JSON.
    crate::python_json::normalize_numbers(&mut value);
    Ok(value)
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictVisitor).map(StrictValue)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate profile key: {key}"
                )));
            }
            let value = access.next_value::<StrictValue>()?;
            object.insert(key, value.0);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod url_parity {
    //! Differential parity harness for `vmServiceUrl`.
    //!
    //! The expectations in [`test_url_allowlist`] were produced by running
    //! CPython 3.14.7 against `bin/environment_config.py::canonical_profile` on
    //! this machine (the same validation the Python test
    //! `tests/unit/test_environment_config.py::ResolverTests::test_url_allowlist`
    //! exercises). The Rust side calls the real [`super::canonical_profile`], so
    //! acceptance and the rendered `vmServiceUrl` are compared end to end.
    //!
    //! The first gate in Python is the `re.fullmatch` on line 73, which admits
    //! only `localhost`, `127.x.x.x` and `[::1]` before line 81 consults
    //! `ipaddress.ip_address(host).is_loopback`. Forms that `ipaddress` reports
    //! as loopback but that fail that regex (`LOCALHOST`,
    //! `[0:0:0:0:0:0:0:1]`, `[::ffff:127.0.0.1]`, `[::ffff:7f00:1]`) are
    //! therefore expected to be rejected.

    use super::canonical_profile;
    use serde_json::{json, Map, Value};
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn string(value: impl AsRef<std::ffi::OsStr>) -> Value {
        Value::String(value.as_ref().to_string_lossy().into_owned())
    }

    fn fixture() -> (TempDir, Map<String, Value>, HashMap<String, String>) {
        let tmp = TempDir::new().expect("temp dir");
        let root = tmp.path().canonicalize().expect("canonical temp dir");
        let repo = root.join("images-repo");
        fs::create_dir_all(repo.join("images")).expect("create repo");
        let fake = root.join("executable");
        fs::write(&fake, "#!/bin/sh\n").expect("write executable");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod executable");

        let mut profile = Map::new();
        profile.insert("schemaVersion".to_string(), json!(1));
        profile.insert("id".to_string(), json!("fixture"));
        profile.insert("vmServiceUrl".to_string(), json!("http://127.0.0.1:6249/"));
        profile.insert("imageRepository".to_string(), string(&repo));
        profile.insert("tartHome".to_string(), string(root.join("store")));
        profile.insert("serviceStateDir".to_string(), string(root.join("service")));
        profile.insert(
            "imageStateDir".to_string(),
            string(root.join("images-state")),
        );
        profile.insert("relayStateDir".to_string(), string(root.join("relay")));
        profile.insert("vmctlPath".to_string(), string(&fake));
        profile.insert("tartPath".to_string(), string(&fake));

        let mut env = HashMap::new();
        env.insert(
            "HOME".to_string(),
            root.join("home").to_string_lossy().into_owned(),
        );
        (tmp, profile, env)
    }

    /// CPython 3.14.7 expectations for `bin/environment_config.py` lines 73-86,
    /// matching `tests/unit/test_environment_config.py::ResolverTests::test_url_allowlist`.
    #[test]
    fn test_url_allowlist() {
        let (_tmp, profile, env) = fixture();
        let cases: &[(&str, Option<&str>)] = &[
            ("http://localhost:6240", Some("http://localhost:6240")),
            ("http://LOCALHOST:6240", None),
            ("http://127.0.0.1:6240", Some("http://127.0.0.1:6240")),
            ("http://127.0.0.2:1", Some("http://127.0.0.2:1")),
            (
                "http://127.255.255.255:65535",
                Some("http://127.255.255.255:65535"),
            ),
            ("http://[::1]:6240", Some("http://[::1]:6240")),
            ("http://[0:0:0:0:0:0:0:1]:6240", None),
            ("http://[::ffff:7f00:1]:6240", None),
            ("http://[::ffff:127.0.0.1]:6240", None),
            ("http://128.0.0.1:80", None),
            ("http://0.0.0.0:80", None),
            ("http://example.com:80", None),
            ("http://127.0.0.1:0", None),
            ("http://127.0.0.1:65536", None),
            ("http://user@127.0.0.1:80", None),
            ("http://127.0.0.1:80/path", None),
            ("http://127.0.0.1:80?a=1", None),
            ("http://127.0.0.1:80#f", None),
            ("http://127.1:80", None),
            ("http://0177.0.0.1:80", None),
            ("https://127.0.0.1:80", None),
            ("http://127.0.0.1", None),
            ("http://:80", None),
            ("http://127.0.0.1:08", Some("http://127.0.0.1:8")),
            ("http://[fe80::1%25eth0]:80", None),
            ("https://localhost:80", None),
            ("http://localhost", None),
            ("http://127.0.0.256:80", None),
            ("http://localhost:80/path", None),
            ("http://user@localhost:80", None),
            ("http://localhost:80?", None),
            ("http://[::2]:80", None),
            ("http://localhost:0", None),
            ("http://localhost:00080/", Some("http://localhost:80")),
            ("http://[::1]:80/", Some("http://[::1]:80")),
            ("http://127.2.3.4:80", Some("http://127.2.3.4:80")),
            ("http://[::ffff:8.8.8.8]:80", None),
            ("http://[::ffff:0:127.0.0.1]:80", None),
            ("http://::1:80", None),
            ("http://127.0.0.01:80", None),
            ("http://127.0.0.1:80/", Some("http://127.0.0.1:80")),
            ("http://localhost:6240/", Some("http://localhost:6240")),
            ("http://127.0.0.1:00001", Some("http://127.0.0.1:1")),
            ("http://127.0.0.1:99999", None),
        ];
        for (url, expected) in cases {
            let mut raw = profile.clone();
            raw.insert("vmServiceUrl".to_string(), json!(url));
            let canonical = canonical_profile(&Value::Object(raw), &env);
            match expected {
                Some(rendered) => {
                    let profile =
                        canonical.unwrap_or_else(|error| panic!("{url} must be accepted: {error}"));
                    assert_eq!(
                        profile["vmServiceUrl"],
                        json!(rendered),
                        "rendering for {url}"
                    );
                }
                None => assert!(canonical.is_err(), "expected rejection for {url}"),
            }
        }
    }
}
