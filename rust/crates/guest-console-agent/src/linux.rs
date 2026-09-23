//! Linux inspection and readiness. Mirrors the `linux_session`, `linux_ready`
//! and `x11vnc_argv` functions of `bin/guest-console-agent.py`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[cfg(test)]
use std::cell::RefCell;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::proc::{identity, run_text};
use crate::Unavailable;

/// Property names requested from `loginctl show-session`, in Python's order.
const SESSION_PROPERTIES: [&str; 12] = [
    "Id",
    "User",
    "Name",
    "Active",
    "Remote",
    "Type",
    "Class",
    "State",
    "Seat",
    "Display",
    "Leader",
    "TimestampMonotonic",
];

/// Parse `key=value` lines, rejecting duplicates and lines without `=`.
pub fn properties(text: &str) -> Result<HashMap<String, String>, Unavailable> {
    let mut result = HashMap::new();
    for line in crate::python::splitlines(text) {
        let (key, value) = match line.split_once('=') {
            Some(pair) => pair,
            None => return Err(Unavailable::new("session_metadata_invalid")),
        };
        if result.contains_key(key) {
            return Err(Unavailable::new("session_metadata_invalid"));
        }
        result.insert(key.to_string(), value.to_string());
    }
    Ok(result)
}

/// The session id alphabet accepted by Python's `[A-Za-z0-9_-]+`.
pub fn is_session_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The local X display form accepted by Python's `:[0-9]+(\.[0-9]+)?`.
pub fn is_local_x11_display(value: &str) -> bool {
    let rest = match value.strip_prefix(':') {
        Some(rest) => rest,
        None => return false,
    };
    let (whole, fraction) = match rest.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (rest, None),
    };
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match fraction {
        Some(fraction) => {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        }
        None => true,
    }
}

/// Code-point ranges for which CPython's `str.isdigit()` is true.
///
/// Measured exhaustively on this machine against Python 3.14.7 (see
/// `bin/guest-console-agent.py:102,112,292` and `linux.rs`'s `/proc` walk for
/// the `.isdigit()` call sites). The predicate is true for Unicode decimal
/// digits (`Nd`) and digit-like `No` characters (for example `'\u{00b2}'`
/// and `'\u{2460}'`) and false for `Nl` characters such as `'\u{2167}'`.
const CPYTHON_ISDIGIT_RANGES: &[(u32, u32)] = &[
    (0x0030, 0x0039),
    (0x00B2, 0x00B3),
    (0x00B9, 0x00B9),
    (0x0660, 0x0669),
    (0x06F0, 0x06F9),
    (0x07C0, 0x07C9),
    (0x0966, 0x096F),
    (0x09E6, 0x09EF),
    (0x0A66, 0x0A6F),
    (0x0AE6, 0x0AEF),
    (0x0B66, 0x0B6F),
    (0x0BE6, 0x0BEF),
    (0x0C66, 0x0C6F),
    (0x0CE6, 0x0CEF),
    (0x0D66, 0x0D6F),
    (0x0DE6, 0x0DEF),
    (0x0E50, 0x0E59),
    (0x0ED0, 0x0ED9),
    (0x0F20, 0x0F29),
    (0x1040, 0x1049),
    (0x1090, 0x1099),
    (0x1369, 0x1371),
    (0x17E0, 0x17E9),
    (0x1810, 0x1819),
    (0x1946, 0x194F),
    (0x19D0, 0x19DA),
    (0x1A80, 0x1A89),
    (0x1A90, 0x1A99),
    (0x1B50, 0x1B59),
    (0x1BB0, 0x1BB9),
    (0x1C40, 0x1C49),
    (0x1C50, 0x1C59),
    (0x2070, 0x2070),
    (0x2074, 0x2079),
    (0x2080, 0x2089),
    (0x2460, 0x2468),
    (0x2474, 0x247C),
    (0x2488, 0x2490),
    (0x24EA, 0x24EA),
    (0x24F5, 0x24FD),
    (0x24FF, 0x24FF),
    (0x2776, 0x277E),
    (0x2780, 0x2788),
    (0x278A, 0x2792),
    (0xA620, 0xA629),
    (0xA8D0, 0xA8D9),
    (0xA900, 0xA909),
    (0xA9D0, 0xA9D9),
    (0xA9F0, 0xA9F9),
    (0xAA50, 0xAA59),
    (0xABF0, 0xABF9),
    (0xFF10, 0xFF19),
    (0x104A0, 0x104A9),
    (0x10A40, 0x10A43),
    (0x10D30, 0x10D39),
    (0x10D40, 0x10D49),
    (0x10E60, 0x10E68),
    (0x11052, 0x1105A),
    (0x11066, 0x1106F),
    (0x110F0, 0x110F9),
    (0x11136, 0x1113F),
    (0x111D0, 0x111D9),
    (0x112F0, 0x112F9),
    (0x11450, 0x11459),
    (0x114D0, 0x114D9),
    (0x11650, 0x11659),
    (0x116C0, 0x116C9),
    (0x116D0, 0x116E3),
    (0x11730, 0x11739),
    (0x118E0, 0x118E9),
    (0x11950, 0x11959),
    (0x11BF0, 0x11BF9),
    (0x11C50, 0x11C59),
    (0x11D50, 0x11D59),
    (0x11DA0, 0x11DA9),
    (0x11F50, 0x11F59),
    (0x16130, 0x16139),
    (0x16A60, 0x16A69),
    (0x16AC0, 0x16AC9),
    (0x16B50, 0x16B59),
    (0x16D70, 0x16D79),
    (0x1CCF0, 0x1CCF9),
    (0x1D7CE, 0x1D7FF),
    (0x1E140, 0x1E149),
    (0x1E2F0, 0x1E2F9),
    (0x1E4F0, 0x1E4F9),
    (0x1E5F1, 0x1E5FA),
    (0x1E950, 0x1E959),
    (0x1F100, 0x1F10A),
    (0x1FBF0, 0x1FBF9),
];

/// CPython's `str.isdigit()` for a single character.
fn python_isdigit(character: char) -> bool {
    let code = character as u32;
    CPYTHON_ISDIGIT_RANGES
        .binary_search_by(|&(start, end)| {
            if code < start {
                std::cmp::Ordering::Greater
            } else if code > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// CPython's `str.isdigit()` for the whole string.
pub(crate) fn is_decimal(value: &str) -> bool {
    !value.is_empty() && value.chars().all(python_isdigit)
}

/// Resolve the single active local X11 session of the calling user.
pub fn linux_session() -> Result<Value, Unavailable> {
    #[cfg(test)]
    if let Some(session) = TEST_SESSION.with(|cell| cell.borrow().clone()) {
        return Ok(session);
    }
    let (uid, user) = identity()?;
    let uid_string = uid.to_string();

    let sessions = run_text(
        &[
            "/usr/bin/loginctl",
            "list-sessions",
            "--no-legend",
            "--no-pager",
        ],
        None,
        &[0],
    )?;

    let mut candidates: Vec<HashMap<String, String>> = Vec::new();
    for row in crate::python::splitlines(&sessions) {
        let fields: Vec<&str> = row.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != uid_string {
            continue;
        }
        let session_id = fields[0];
        if !is_session_id(session_id) {
            return Err(Unavailable::new("session_metadata_invalid"));
        }
        let mut argv: Vec<String> = vec![
            "/usr/bin/loginctl".to_string(),
            "show-session".to_string(),
            session_id.to_string(),
            "--no-pager".to_string(),
        ];
        for name in SESSION_PROPERTIES {
            argv.push("-p".to_string());
            argv.push(name.to_string());
        }
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let shown = properties(&run_text(&argv_refs, None, &[0])?)?;
        let active = shown.get("Active").map(String::as_str) == Some("yes");
        let remote = shown.get("Remote").map(String::as_str) == Some("no");
        let class = shown.get("Class").map(String::as_str) == Some("user");
        let seat = shown
            .get("Seat")
            .map(|value| !value.is_empty())
            .unwrap_or(false);
        if active && remote && class && seat {
            candidates.push(shown);
        }
    }

    if candidates.len() != 1 {
        return Err(Unavailable::new("active_console_session_required"));
    }
    let shown = &candidates[0];

    if shown.get("Type").map(String::as_str) != Some("x11") {
        return Err(Unavailable::new("x11_session_required"));
    }
    let leader_ok = shown.get("Leader").map(|value| is_decimal(value)) == Some(true);
    let identity_ok = shown.get("User").map(String::as_str) == Some(uid_string.as_str())
        && shown.get("Name").map(String::as_str) == Some(user.as_str())
        && shown.get("State").map(String::as_str) == Some("active")
        && shown
            .get("TimestampMonotonic")
            .map(|value| !value.is_empty())
            == Some(true)
        && leader_ok;
    if !identity_ok {
        return Err(Unavailable::new("session_identity_mismatch"));
    }

    let session_id = shown.get("Id").cloned().unwrap_or_default();
    if !is_session_id(&session_id) {
        return Err(Unavailable::new("session_metadata_invalid"));
    }
    let mut display = shown.get("Display").cloned().unwrap_or_default();

    // The SSH environment is not the graphical session's environment. Inspect
    // only processes owned by that UID and explicitly bound to this login ID.
    let mut environments: HashSet<(String, String)> = HashSet::new();
    let entries = match fs::read_dir(crate::proc::procfs_root()) {
        Ok(entries) => entries,
        Err(_) => return Err(Unavailable::new("x11_environment_unavailable")),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.uid() != uid {
            continue;
        }
        let raw = match fs::read(entry.path().join("environ")) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        if raw.len() > 1024 * 1024 {
            continue;
        }
        let values = split_environment(&raw);
        if values.get(b"XDG_SESSION_ID".as_slice()).map(Vec::as_slice)
            != Some(session_id.as_bytes())
        {
            continue;
        }
        let raw_display = values
            .get(b"DISPLAY".as_slice())
            .map(Vec::as_slice)
            .unwrap_or(b"");
        let raw_authority = values
            .get(b"XAUTHORITY".as_slice())
            .map(Vec::as_slice)
            .unwrap_or(b"");
        let display_value = match std::str::from_utf8(raw_display) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let authority_value = match std::str::from_utf8(raw_authority) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if !display_value.is_empty() && !authority_value.is_empty() {
            environments.insert((display_value.to_string(), authority_value.to_string()));
        }
    }

    if environments.len() != 1 {
        return Err(Unavailable::new("x11_environment_unavailable"));
    }
    let (actual_display, authority) = environments
        .into_iter()
        .next()
        .ok_or_else(|| Unavailable::new("x11_environment_unavailable"))?;
    if !display.is_empty() && display != actual_display {
        return Err(Unavailable::new("display_identity_mismatch"));
    }
    display = actual_display;
    if !is_local_x11_display(&display) {
        return Err(Unavailable::new("local_x11_display_required"));
    }
    if !Path::new(&authority).is_absolute() {
        return Err(Unavailable::new("xauthority_invalid"));
    }

    let authority_digest = read_authority_digest(&authority, uid)?;
    let boot =
        match fs::read_to_string(crate::proc::procfs_root().join("sys/kernel/random/boot_id")) {
            Ok(boot) => boot.trim().to_string(),
            Err(_) => return Err(Unavailable::new("xauthority_unavailable")),
        };

    let mut result = Map::new();
    result.insert("id".to_string(), Value::String(session_id));
    result.insert("uid".to_string(), Value::from(uid));
    result.insert("user".to_string(), Value::String(user));
    result.insert("type".to_string(), Value::String("x11".to_string()));
    result.insert(
        "seat".to_string(),
        Value::String(shown.get("Seat").cloned().unwrap_or_default()),
    );
    result.insert("display".to_string(), Value::String(display));
    result.insert("xauthority".to_string(), Value::String(authority));
    result.insert(
        "xauthority_sha256".to_string(),
        Value::String(authority_digest),
    );
    result.insert(
        "leader".to_string(),
        Value::String(shown.get("Leader").cloned().unwrap_or_default()),
    );
    result.insert(
        "started".to_string(),
        Value::String(shown.get("TimestampMonotonic").cloned().unwrap_or_default()),
    );
    result.insert("boot".to_string(), Value::String(boot));
    Ok(Value::Object(result))
}

fn read_authority_digest(authority: &str, uid: libc::uid_t) -> Result<String, Unavailable> {
    let path = CString::new(authority).map_err(|_| Unavailable::new("xauthority_unavailable"))?;
    // SAFETY: `path` is a valid NUL-terminated path; `open` returns a checked fd.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(Unavailable::new("xauthority_unavailable"));
    }
    let result = (|| -> Result<String, Unavailable> {
        // SAFETY: `stat` writes into the zeroed struct; the fd is owned here.
        let mut info: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut info) } != 0 {
            return Err(Unavailable::new("xauthority_unavailable"));
        }
        if (info.st_mode & libc::S_IFMT) != libc::S_IFREG
            || info.st_uid != uid
            || info.st_mode & 0o077 != 0
        {
            return Err(Unavailable::new("xauthority_invalid"));
        }
        let mut buffer = vec![0u8; 1024 * 1024 + 1];
        // SAFETY: `buffer` is valid for its length; the return value is checked.
        let count =
            unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) };
        if count < 0 {
            return Err(Unavailable::new("xauthority_unavailable"));
        }
        if count == 0 {
            return Err(Unavailable::new("xauthority_invalid"));
        }
        let raw = &buffer[..count as usize];
        if raw.len() > 1024 * 1024 {
            return Err(Unavailable::new("xauthority_invalid"));
        }
        Ok(sha256_hex(raw))
    })();
    // SAFETY: `fd` was returned by `open` and is closed exactly once here.
    unsafe {
        libc::close(fd);
    }
    result
}

/// Lowercase hex SHA-256, matching `hashlib.sha256(...).hexdigest()`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Split a NUL-delimited `/proc` environment block, last duplicate key winning.
pub(crate) fn split_environment(raw: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut values = HashMap::new();
    for item in raw.split(|byte| *byte == 0) {
        if let Some(position) = item.iter().position(|byte| *byte == b'=') {
            values.insert(item[..position].to_vec(), item[position + 1..].to_vec());
        }
    }
    values
}

/// Confirm x11vnc is installed, supports the required options and can reach the
/// session's display.
pub fn linux_ready() -> Result<(Value, Value), Unavailable> {
    let session = linux_session()?;

    if !is_executable("/usr/bin/x11vnc") {
        return Err(Unavailable::new("x11vnc_unavailable"));
    }
    let help = crate::proc::run_text(&["/usr/bin/x11vnc", "-norc", "-help"], None, &[0, 1])?;
    for option in ["-inetd", "-viewonly", "-passwdfile", "-noremote", "-nocmds"] {
        if !help.contains(option) {
            return Err(Unavailable::new("x11vnc_options_unavailable"));
        }
    }
    let display = field_string(&session, "display");
    let authority = field_string(&session, "xauthority");
    let environment = [
        ("DISPLAY", display.as_str()),
        ("XAUTHORITY", authority.as_str()),
    ];
    crate::proc::run_text(
        &["/usr/bin/xdpyinfo", "-display", display.as_str()],
        Some(&environment),
        &[0],
    )?;

    let mut isolation = Map::new();
    isolation.insert("mechanism".to_string(), Value::String("inetd".to_string()));
    isolation.insert("guest_tcp_listener".to_string(), Value::Bool(false));
    Ok((session, Value::Object(isolation)))
}

fn is_executable(path: &str) -> bool {
    #[cfg(test)]
    if let Some(value) = TEST_ACCESS.with(|cell| *cell.borrow()) {
        return value;
    }
    match CString::new(path) {
        // SAFETY: `path` is a valid NUL-terminated path and `access` cannot fault.
        Ok(path) => unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 },
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Test-only seams
// ---------------------------------------------------------------------------
//
// Python's Linux session tests patch `guest.Path` (a `/proc` override),
// `guest.os.access`, and `guest.linux_session` itself. Rust cannot patch a
// process global, so each is a per-thread override consulted by the production
// helper. With none installed the production behavior is exactly as before.

#[cfg(test)]
thread_local! {
    static TEST_ACCESS: RefCell<Option<bool>> = const { RefCell::new(None) };
    static TEST_SESSION: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// Install or clear the fake `os.access` result for the current thread.
#[cfg(test)]
pub(crate) fn set_test_access(value: Option<bool>) {
    TEST_ACCESS.with(|cell| *cell.borrow_mut() = value);
}

/// Install or clear the resolved session `linux_session` returns.
#[cfg(test)]
pub(crate) fn set_test_linux_session(session: Option<Value>) {
    TEST_SESSION.with(|cell| *cell.borrow_mut() = session);
}

/// The x11vnc argv for inetd mode. The password is passed as `rm:<path>` so it
/// never appears in the argument vector or the environment.
pub fn x11vnc_argv(session: &Value, password_path: &str) -> Vec<String> {
    let display = field_string(session, "display");
    let authority = field_string(session, "xauthority");
    vec![
        "/usr/bin/x11vnc".to_string(),
        "-norc".to_string(),
        "-inetd".to_string(),
        "-q".to_string(),
        "-once".to_string(),
        "-viewonly".to_string(),
        "-display".to_string(),
        display,
        "-auth".to_string(),
        authority,
        "-passwdfile".to_string(),
        format!("rm:{password_path}"),
        "-noremote".to_string(),
        "-nocmds".to_string(),
        "-nosel".to_string(),
        "-noclipboard".to_string(),
        "-nosetclipboard".to_string(),
        "-noprimary".to_string(),
        "-nosetprimary".to_string(),
    ]
}

/// Read a string field from a session record that this crate built.
pub fn field_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
