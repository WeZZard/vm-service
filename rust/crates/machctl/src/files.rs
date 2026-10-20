//! `machctl push` and `pull` over the byte-carrying endpoints
//! (`docs/arch/federation.md` FD-4, `docs/ux/cli/list.md` CL-3; DC-70,
//! DC-78).
//!
//! A host path means nothing to an instance on another Mac, so a transfer
//! to an instance that is not on loopback sends the bytes: a file as
//! itself, a folder as a tar stream of its contents, each with its SHA-256
//! in `X-Content-SHA256`. `--bytes` takes the same path on loopback. A pull
//! is written to a temporary sibling, its digest checked, and only then
//! renamed into place, so a failed or corrupted pull leaves nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// Whether the instance at `base` is on this Mac's loopback.
pub fn is_loopback(base: &str) -> bool {
    let authority = base
        .split_once("://")
        .map_or(base, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// Percent-encode one query component.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn url(base: &str, vm: &str, remote: &str, lease_id: Option<&str>, name: Option<&str>) -> String {
    let mut url = format!(
        "{}/machines/{vm}/files?path={}",
        base.trim_end_matches('/'),
        encode(remote)
    );
    if let Some(lease_id) = lease_id {
        url.push_str(&format!("&lease_id={}", encode(lease_id)));
    }
    if let Some(name) = name {
        url.push_str(&format!("&name={}", encode(name)));
    }
    url
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(crate::DEFAULT_TIMEOUT_S)))
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .build()
        .into()
}

/// A scratch folder for one transfer, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn beside(path: &Path) -> Result<Scratch, String> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let folder = parent.join(format!(".machctl-transfer-{}", std::process::id()));
        fs::create_dir_all(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        Ok(Scratch(folder))
    }

    fn temporary() -> Result<Scratch, String> {
        let folder = std::env::temp_dir().join(format!("machctl-transfer-{}", std::process::id()));
        fs::create_dir_all(&folder).map_err(|error| format!("{}: {error}", folder.display()))?;
        Ok(Scratch(folder))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn error_of(status: u16, text: &str) -> String {
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    match parsed.get("error").and_then(Value::as_str) {
        Some(error) => format!("HTTP {status}: {error}"),
        None => format!("HTTP {status}: {}", text.trim()),
    }
}

/// Send `local` to `remote` on `vm` as bytes (`PUT .../files`).
pub fn push(
    base: &str,
    fingerprint: Option<&str>,
    vm: &str,
    local: &Path,
    remote: &str,
    lease_id: Option<&str>,
) -> Result<Value, String> {
    let meta =
        fs::symlink_metadata(local).map_err(|error| format!("{}: {error}", local.display()))?;
    let name = local
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let scratch;
    let (body_path, content_type) = if meta.is_dir() {
        scratch = Scratch::temporary()?;
        let archive = scratch.0.join("body.tar");
        byte_transfer::pack(local, &archive)?;
        (archive, byte_transfer::TAR_TYPE)
    } else if meta.is_file() {
        (local.to_path_buf(), byte_transfer::FILE_TYPE)
    } else {
        return Err(format!(
            "{} is not a file or a folder; a transfer carries only those",
            local.display()
        ));
    };
    let (_, digest) = byte_transfer::hash_file(&body_path).map_err(|error| format!("{error}"))?;
    let file = fs::File::open(&body_path).map_err(|error| format!("{error}"))?;
    let mut request = agent()
        .put(url(base, vm, remote, lease_id, name.as_deref()).as_str())
        .header("Content-Type", content_type)
        .header(byte_transfer::DIGEST_HEADER, digest.as_str());
    if let Some(fingerprint) = fingerprint {
        request = request.header("X-VM-Environment-Fingerprint", fingerprint);
    }
    let mut response = request
        .send(file)
        .map_err(|error| format!("cannot reach {base}: {error}"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    if status != 200 {
        return Err(error_of(status, &text));
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Fetch `remote` on `vm` as bytes (`GET .../files`) into `local`. As a
/// path pull, an existing folder at `local` receives the item under its
/// own name.
pub fn pull(
    base: &str,
    fingerprint: Option<&str>,
    vm: &str,
    remote: &str,
    local: &Path,
    lease_id: Option<&str>,
) -> Result<PathBuf, String> {
    let mut request = agent().get(url(base, vm, remote, lease_id, None).as_str());
    if let Some(fingerprint) = fingerprint {
        request = request.header("X-VM-Environment-Fingerprint", fingerprint);
    }
    let mut response = request
        .call()
        .map_err(|error| format!("cannot reach {base}: {error}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        let text = response.body_mut().read_to_string().unwrap_or_default();
        return Err(error_of(status, &text));
    }
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let expected = header(byte_transfer::DIGEST_HEADER)
        .map(|digest| digest.to_ascii_lowercase())
        .filter(|digest| byte_transfer::valid_digest(digest))
        .ok_or_else(|| format!("the answer carries no {}", byte_transfer::DIGEST_HEADER))?;
    let content_type = header("Content-Type").unwrap_or_default();
    let name = header("X-Content-Name")
        .filter(|name| !name.is_empty() && name != "." && name != ".." && !name.contains('/'));
    let target = match (local.is_dir(), name) {
        (true, Some(name)) => local.join(name),
        (true, None) => {
            return Err(format!(
                "{} is a folder and the answer names no item",
                local.display()
            ))
        }
        (false, _) => local.to_path_buf(),
    };
    if fs::symlink_metadata(&target).is_ok() && content_type == byte_transfer::TAR_TYPE {
        return Err(format!("{} already exists", target.display()));
    }
    let scratch = Scratch::beside(&target)?;
    let body = scratch.0.join("body");
    let copied = fs::File::create(&body).and_then(|mut file| {
        let mut reader = response.body_mut().as_reader();
        let done = byte_transfer::copy_hashing(&mut reader, &mut file)?;
        file.sync_all()?;
        Ok(done)
    });
    let (_, digest) = copied.map_err(|error| format!("cannot read the answer: {error}"))?;
    if digest != expected {
        return Err(format!(
            "digest mismatch: header {expected}, body {digest}; nothing was written"
        ));
    }
    if content_type == byte_transfer::TAR_TYPE {
        let unpacked = scratch.0.join("item");
        byte_transfer::unpack(&body, &unpacked)?;
        fs::rename(&unpacked, &target)
    } else {
        fs::rename(&body, &target)
    }
    .map_err(|error| format!("cannot move into {}: {error}", target.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd4_loopback_and_urls() {
        for base in [
            "http://127.0.0.1:6240",
            "http://localhost:6240/",
            "http://[::1]:6240",
        ] {
            assert!(is_loopback(base), "{base}");
        }
        for base in ["http://10.0.0.4:6240", "http://mac-b.local:6240"] {
            assert!(!is_loopback(base), "{base}");
        }
        assert_eq!(
            url("http://h:1/", "m", "/tmp/a b+c", Some("L"), Some("x y")),
            "http://h:1/machines/m/files?path=/tmp/a%20b%2Bc&lease_id=L&name=x%20y"
        );
    }
}
