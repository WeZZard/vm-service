//! Serve-configuration framing and validation. Mirrors the `read_config` and
//! `validate_config` functions of `bin/guest-console-agent.py`.

use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::cell::RefCell;

use serde_json::{Map, Value};

use crate::api_time;
use crate::signal;
use crate::{Kind, Unavailable};

/// Maximum JSON payload accepted before the newline.
pub const MAX_CONFIG: usize = 16384;
/// The whole configuration line must arrive within this bound.
pub const CONFIG_TIMEOUT_SECS: f64 = 10.0;
/// Longest session lifetime the agent will honor.
pub const MAX_LIFETIME: f64 = 720.0 * 3600.0;
/// Clock-skew tolerance applied to the hard ceiling only.
pub const CLOCK_SKEW_ALLOWANCE: f64 = 300.0;

/// Read exactly one JSON configuration line from `fd`.
///
/// The read is byte by byte, so not a single byte of the binary RFB stream that
/// follows on the same descriptor is consumed. A file descriptor is used
/// instead of a `Read` so the framing can be tested with a pipe.
pub fn read_config(fd: RawFd) -> Result<Value, Unavailable> {
    #[cfg(test)]
    if let Some(value) = TEST_CONFIG.with(|cell| cell.borrow().clone()) {
        return Ok(value);
    }
    read_config_limited(fd, MAX_CONFIG, CONFIG_TIMEOUT_SECS)
}

// ---------------------------------------------------------------------------
// Test-only seam
// ---------------------------------------------------------------------------
//
// Python's `serve` tests patch `guest.read_config`; this is the per-thread
// equivalent. With no override installed the production framing read runs
// unchanged.

#[cfg(test)]
thread_local! {
    static TEST_CONFIG: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// Install or clear the configuration `read_config` returns.
#[cfg(test)]
pub(crate) fn set_test_config(value: Option<Value>) {
    TEST_CONFIG.with(|cell| *cell.borrow_mut() = value);
}

/// `read_config` with the limits exposed so tests can bound them.
pub fn read_config_limited(
    fd: RawFd,
    max_config: usize,
    timeout_secs: f64,
) -> Result<Value, Unavailable> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
    let mut data: Vec<u8> = Vec::new();
    while data.len() <= max_config {
        signal::check_interrupted()?;
        let now = Instant::now();
        if now >= deadline {
            return Err(Unavailable::new("configuration_timeout"));
        }
        let remaining = deadline.saturating_duration_since(now);
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `pollfd` points at one initialized entry for the call.
        let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if ready == 0 {
            return Err(Unavailable::new("configuration_timeout"));
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Unavailable::new("stream_failed"));
        }
        let mut byte = [0u8; 1];
        // SAFETY: `byte` is a one-byte buffer; the return value is checked.
        let count = unsafe { libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
        if count < 0 {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => continue,
                _ => return Err(Unavailable::new("stream_failed")),
            }
        }
        if count == 0 {
            return Err(Unavailable::new("configuration_incomplete"));
        }
        if byte[0] == b'\n' {
            return serde_json::from_slice(&data)
                .map_err(|_| Unavailable::new("configuration_invalid"));
        }
        data.push(byte[0]);
    }
    Err(Unavailable::new("configuration_too_large"))
}

/// Validate a configuration and return the monotonic deadline as seconds.
///
/// `now` is Unix seconds and `mono_now` is monotonic seconds; both are injected
/// so the pure deadline arithmetic is testable without a clock.
pub fn validate_config_at(
    config: &Value,
    kind: Kind,
    now: f64,
    mono_now: f64,
) -> Result<f64, Unavailable> {
    let object = match config.as_object() {
        Some(object) => object,
        None => return Err(Unavailable::new("configuration_invalid")),
    };
    // Python compares `set(config) != required`, so exactly these keys must be
    // present and no others.
    let keys_match = match kind {
        Kind::Linux => object.len() == 4,
        Kind::Macos => object.len() == 3,
    };
    if !keys_match
        || !object.contains_key("version")
        || !object.contains_key("session")
        || !object.contains_key("expires_at")
        || (kind == Kind::Linux && !object.contains_key("password"))
    {
        return Err(Unavailable::new("configuration_invalid"));
    }

    let version_ok = matches!(
        object.get("version"),
        Some(Value::Number(number)) if number.as_u64() == Some(1) || number.as_i64() == Some(1)
    );
    if !version_ok {
        return Err(Unavailable::new("configuration_invalid"));
    }

    // `type(expiry) in (int, float)` excludes booleans; `math.isfinite` excludes
    // non-finite floats.
    let expiry = match object.get("expires_at") {
        Some(Value::Number(number)) => match number.as_f64() {
            Some(value) if value.is_finite() => value,
            _ => return Err(Unavailable::new("deadline_invalid")),
        },
        _ => return Err(Unavailable::new("deadline_invalid")),
    };
    let lifetime = expiry - now;
    if !(lifetime > 0.0 && lifetime <= MAX_LIFETIME + CLOCK_SKEW_ALLOWANCE) {
        return Err(Unavailable::new("deadline_invalid"));
    }

    match object.get("session") {
        Some(Value::Object(session)) if !session.is_empty() => {}
        _ => return Err(Unavailable::new("session_proof_required")),
    }

    if kind == Kind::Linux {
        let password = match object.get("password").and_then(Value::as_str) {
            Some(password) => password,
            None => return Err(Unavailable::new("password_invalid")),
        };
        if !password_valid(password) {
            return Err(Unavailable::new("password_invalid"));
        }
    }

    Ok(mono_now + lifetime.min(MAX_LIFETIME))
}

/// Validate a configuration and return its monotonic deadline.
pub fn validate_config(config: &Value, kind: Kind) -> Result<Instant, Unavailable> {
    let mono_now = api_time::monotonic_seconds();
    let offset = validate_config_at(config, kind, api_time::unix_seconds(), mono_now)?;
    Ok(api_time::monotonic_instant(offset, mono_now))
}

/// `re.fullmatch(r"[!-~]{8}", password)` plus the three literal rejections.
fn password_valid(password: &str) -> bool {
    let bytes = password.as_bytes();
    if bytes.len() != 8 {
        return false;
    }
    if !bytes.iter().all(|byte| (b'!'..=b'~').contains(byte)) {
        return false;
    }
    if password.starts_with('#') {
        return false;
    }
    if password.contains("__SKIP__") || password.contains("__COMM__") {
        return false;
    }
    true
}

/// Build an empty object with the JSON insertion order Python uses.
pub fn empty_object() -> Map<String, Value> {
    Map::new()
}

/// Parse JSON text the way `json.loads` is used during configuration framing.
pub fn parse_json_line(data: &[u8]) -> Result<Value, Unavailable> {
    serde_json::from_slice(data).map_err(|_| Unavailable::new("configuration_invalid"))
}
