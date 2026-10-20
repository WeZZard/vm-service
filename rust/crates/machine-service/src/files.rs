//! The byte-carrying file endpoints (`docs/arch/federation.md` FD-4;
//! DC-70, DC-71).
//!
//! `PUT /machines/<name>/files?lease_id=<id>&path=<remote path>[&name=<n>]`
//! takes a file's bytes (`application/octet-stream`) or a folder's tar
//! stream (`application/x-tar`), with the body's SHA-256 in
//! `X-Content-SHA256`. `GET /machines/<name>/files?lease_id=<id>&path=<p>`
//! answers a file's bytes or a folder's tar stream the same way.
//!
//! The bytes are staged under `<state>/transfers/<id>/` and the staged path
//! then takes the path-based transfer the instance already has: the local
//! file system for the durable machine, the guest link for a virtual
//! machine. So a byte transfer keeps every rule a path transfer has (the
//! lease check, the protected key material, the temporary sibling and the
//! rename), and the staging folder is removed when the request ends. A
//! body whose digest differs from its header is refused and nothing is
//! placed.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response};

use machine_service_core::service::Service;

/// The machine of a `/machines/<name>/files` URL, query string allowed.
pub fn machine_of(url: &str) -> Option<String> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let rest = path.strip_prefix("/machines/")?;
    let (name, tail) = rest.split_once('/')?;
    (tail == "files" && !name.is_empty()).then(|| name.to_string())
}

/// Decode one `application/x-www-form-urlencoded` component.
fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = text.get(index + 1..index + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The query parameters of `url`, decoded.
fn query(url: &str) -> Result<Vec<(String, String)>, String> {
    let Some((_, query)) = url.split_once('?') else {
        return Ok(Vec::new());
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match (decode(key), decode(value)) {
                (Some(key), Some(value)) => Ok((key, value)),
                _ => Err(format!("bad query parameter: {pair}")),
            }
        })
        .collect()
}

fn header(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

/// The name a staged item takes: `name`, or the remote path's last part.
fn staged_name(name: Option<&str>, path: &str, fallback: &str) -> Result<String, String> {
    let chosen = match name {
        Some(name) => name.to_string(),
        None => Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| fallback.to_string()),
    };
    if chosen.is_empty() || chosen == "." || chosen == ".." || chosen.contains('/') {
        return Err(format!("bad name: {chosen:?}"));
    }
    Ok(chosen)
}

/// A staging folder that removes itself.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn staging(service: &Service) -> Result<Staging, String> {
    let root = service.config.state_dir.join("transfers");
    fs::create_dir_all(&root).map_err(|error| format!("{}: {error}", root.display()))?;
    let folder = root.join(uuid::Uuid::new_v4().simple().to_string());
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&folder)
        .map_err(|error| format!("{}: {error}", folder.display()))?;
    Ok(Staging(folder))
}

/// Answer one request on the file endpoints of `name`.
pub fn handle(service: &Service, request: Request, name: &str) {
    if let Some((request, status, body)) = answer(service, request, name) {
        let _ = crate::send(request, status, &body);
    }
}

/// A JSON answer still to send.
type Answer = Option<(Request, u16, Value)>;

fn refuse(request: Request, status: u16, message: impl Into<String>) -> Answer {
    Some((request, status, json!({"error": message.into()})))
}

/// Serve the request: a JSON answer to send, or `None` when the bytes were
/// sent already.
fn answer(service: &Service, mut request: Request, name: &str) -> Answer {
    let method = request.method().clone();
    let push = match method {
        Method::Put => true,
        Method::Get => false,
        _ => return refuse(request, 405, "use PUT or GET on /machines/<name>/files"),
    };
    let supplied = crate::supplied_fingerprint(&request);
    if !crate::environment_allowed(service, &supplied, push) {
        return refuse(
            request,
            409,
            "environment fingerprint mismatch; refusing request",
        );
    }
    let parameters = match query(request.url()) {
        Ok(parameters) => parameters,
        Err(error) => return refuse(request, 400, error),
    };
    let get = |key: &str| {
        parameters
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let Some(remote) = get("path").filter(|path| !path.is_empty()) else {
        return refuse(request, 400, "'path' (query parameter) is required");
    };
    let mut body = json!({"remote_path": remote});
    if let Some(lease_id) = get("lease_id") {
        body["lease_id"] = json!(lease_id);
    }

    // Who owns the machine, and whether the transfer may proceed, before
    // any byte is received.
    let durable = match crate::durable_routes::files_check(name, &body) {
        None => false,
        Some(Ok(())) => true,
        Some(Err((status, value))) => return Some((request, status, value)),
    };
    if !durable {
        let known = machine_service_core::snapshot::snapshot(service)
            .ok()
            .is_some_and(|snapshot| snapshot["machines"].get(name).is_some());
        if !known {
            return refuse(request, 404, format!("unknown machine: {name}"));
        }
    }
    let staged_root = match staging(service) {
        Ok(staged) => staged,
        Err(error) => return refuse(request, 500, error),
    };
    let fallback = if push { "upload" } else { "download" };
    let item = match staged_name(get("name").as_deref(), &remote, fallback) {
        Ok(item) => item,
        Err(error) => return refuse(request, 400, error),
    };
    let staged = staged_root.0.join(&item);
    body["local_path"] = json!(staged.to_string_lossy());

    if push {
        let kind = match header(&request, "Content-Type").as_deref() {
            Some(byte_transfer::FILE_TYPE) => "file",
            Some(byte_transfer::TAR_TYPE) => "directory",
            other => {
                return refuse(
                    request,
                    400,
                    format!(
                        "Content-Type must be {} or {} (got {})",
                        byte_transfer::FILE_TYPE,
                        byte_transfer::TAR_TYPE,
                        other.unwrap_or("none")
                    ),
                )
            }
        };
        let Some(expected) = header(&request, byte_transfer::DIGEST_HEADER)
            .map(|digest| digest.to_ascii_lowercase())
            .filter(|digest| byte_transfer::valid_digest(digest))
        else {
            return refuse(
                request,
                400,
                format!(
                    "{} (the SHA-256 of the body, in hex) is required",
                    byte_transfer::DIGEST_HEADER
                ),
            );
        };
        let received = staged_root.0.join(".body");
        let copied = fs::File::create(&received).and_then(|mut file| {
            let done = byte_transfer::copy_hashing(request.as_reader(), &mut file)?;
            file.sync_all()?;
            Ok(done)
        });
        let (bytes, digest) = match copied {
            Ok(done) => done,
            Err(error) => return refuse(request, 400, format!("cannot read the body: {error}")),
        };
        if digest != expected {
            return refuse(
                request,
                400,
                format!("digest mismatch: header {expected}, body {digest}; nothing was written"),
            );
        }
        let materialized = if kind == "file" {
            fs::rename(&received, &staged).map_err(|error| error.to_string())
        } else {
            byte_transfer::unpack(&received, &staged)
        };
        if let Err(error) = materialized {
            return refuse(request, 400, error);
        }
        let (status, mut out) = transfer(service, name, &body, durable, true);
        if status != 200 {
            return Some((request, status, out));
        }
        out["pushed"] = json!(item);
        out["bytes"] = json!(bytes);
        out["sha256"] = json!(digest);
        out["content"] = json!(kind);
        service.log(&format!(
            "files: received {bytes} bytes ({kind}) for {name}:{remote}"
        ));
        return Some((request, 200, out));
    }

    let (status, out) = transfer(service, name, &body, durable, false);
    if status != 200 {
        return Some((request, status, out));
    }
    let is_dir = fs::symlink_metadata(&staged).is_ok_and(|meta| meta.is_dir());
    let (file, kind) = if is_dir {
        let archive = staged_root.0.join(".body.tar");
        if let Err(error) = byte_transfer::pack(&staged, &archive) {
            return refuse(request, 409, error);
        }
        (archive, byte_transfer::TAR_TYPE)
    } else {
        (staged.clone(), byte_transfer::FILE_TYPE)
    };
    let (bytes, digest) = match byte_transfer::hash_file(&file) {
        Ok(done) => done,
        Err(error) => return refuse(request, 500, error.to_string()),
    };
    let reader = match fs::File::open(&file) {
        Ok(reader) => reader,
        Err(error) => return refuse(request, 500, error.to_string()),
    };
    let headers = [
        ("Content-Type", kind.to_string()),
        (byte_transfer::DIGEST_HEADER, digest),
        ("X-Content-Name", item.clone()),
    ];
    let mut response = Response::new(
        tiny_http::StatusCode(200),
        Vec::new(),
        reader,
        Some(bytes as usize),
        None,
    );
    for (field, value) in headers {
        if let Ok(header) = Header::from_bytes(field.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    service.log(&format!(
        "files: sending {bytes} bytes ({kind}) of {name}:{remote}"
    ));
    if let Err(error) = request.respond(response) {
        service.log(&format!(
            "WARN: files: {name}:{remote} not delivered: {error}"
        ));
    }
    drop(staged_root);
    None
}

/// The path-based half of a byte transfer, on the owner's own paths.
fn transfer(
    service: &Service,
    name: &str,
    body: &Value,
    durable: bool,
    push: bool,
) -> (u16, Value) {
    if durable {
        return crate::durable_routes::files_transfer(name, body, push);
    }
    let done = if push {
        service.guest_push(name, body)
    } else {
        service.guest_pull(name, body)
    };
    match done {
        Ok(value) => (200, value),
        Err(error) => (409, json!({"error": error.to_string()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd4_urls_and_queries() {
        assert_eq!(
            machine_of("/machines/mac-b/files?lease_id=x&path=a"),
            Some("mac-b".to_string())
        );
        assert_eq!(
            machine_of("/machines/mac-b/files"),
            Some("mac-b".to_string())
        );
        assert_eq!(machine_of("/machines/mac-b/push"), None);
        assert_eq!(machine_of("/machines//files"), None);
        let parsed = query("/x?path=%2Ftmp%2Fa%20b&name=c%2Bd&lease_id=1").unwrap();
        assert_eq!(parsed[0], ("path".to_string(), "/tmp/a b".to_string()));
        assert_eq!(parsed[1], ("name".to_string(), "c+d".to_string()));
        assert!(query("/x?path=%zz").is_err());
        assert_eq!(staged_name(None, "/a/b.txt", "upload").unwrap(), "b.txt");
        assert_eq!(staged_name(None, "/", "upload").unwrap(), "upload");
        assert!(staged_name(Some(".."), "/a", "upload").is_err());
        assert!(staged_name(Some("a/b"), "/a", "upload").is_err());
    }
}
