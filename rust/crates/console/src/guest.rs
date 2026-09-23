//! Fixed SSH commands for the existing-session guest console adapters.
//!
//! This is the Rust port of `bin/guest_console.py`, with one deliberate
//! adaptation: the guest agent is a native binary delivered once over SSH
//! standard input instead of Python source embedded in the argv. A native
//! binary can exceed the per-argument limit (128 KiB), so it cannot be carried
//! inside the command string.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

/// Supported guest console kinds.
pub const KINDS: [&str; 2] = ["linux", "macos"];
/// The probe protocol version.
pub const PROTOCOL_VERSION: i64 = 1;
/// The fixed guest-side agent location, expanded by the login shell.
pub const AGENT_COMMAND: &str = "\"$HOME/.vm-service-console/agent\"";

/// A guest-module failure. The `Display` text matches the Python exception.
#[derive(Debug, Error)]
pub enum GuestError {
    /// An unsupported console kind was requested.
    #[error("unsupported guest console kind")]
    UnsupportedKind,
    /// A probe record did not validate.
    #[error("invalid guest console probe")]
    InvalidProbe,
    /// The native guest agent could not be located.
    #[error("guest console agent unavailable")]
    AgentUnavailable,
}

// The adapter's diagnostic codes are fixed constants chosen by the guest agent,
// never subprocess output (see guest-console-agent.py's Unavailable). Only a
// recognized code may be named to a caller; anything else is reported as
// unrecognized, so an unexpected adapter can never inject text into a message.
/// The diagnostic codes this host recognizes.
pub const DIAGNOSTIC_CODES: &[&str] = &[
    "console_user_required",
    "session_metadata_invalid",
    "active_console_session_required",
    "x11_session_required",
    "session_identity_mismatch",
    "x11_environment_unavailable",
    "display_identity_mismatch",
    "local_x11_display_required",
    "xauthority_invalid",
    "xauthority_unavailable",
    "x11vnc_unavailable",
    "x11vnc_options_unavailable",
    "inspection_failed",
    "deadline_expired",
    "guest_platform_mismatch",
    "remote_management_not_supported",
    "screensharing_service_unverified",
    "screensharing_listener_unavailable",
    "screensharing_listener_unverified",
    "pf_disabled",
    "pf_rules_unverified",
    "pf_interfaces_unverified",
    "pf_interface_bypass",
    "pf_existing_connection",
    "pf_states_unverified",
    "pf_translation_not_supported",
];

// Codes that mean the guest has not reached the state yet, as opposed to cannot
// reach it. A freshly booted guest answers SSH before its graphical session
// exists: measured on a pilot-ubuntu-base clone (2026-09-22), SSH first answered
// at boot+121s and the console probe reported active_console_session_required
// for 9s, then x11_environment_unavailable for 2s, before reporting ready at
// boot+132s. A caller may wait on these; every other code is a standing fault
// that waiting cannot cure, and must fail immediately so the reason stays
// legible. screensharing_listener_unavailable is deliberately NOT here: a
// disabled sharing service is the likelier cause and must not consume a wait.
/// The diagnostic codes that a caller may wait on.
pub const PENDING_CODES: &[&str] = &[
    "active_console_session_required",
    "x11_session_required",
    "x11_environment_unavailable",
    "xauthority_unavailable",
    "display_identity_mismatch",
    "session_identity_mismatch",
    "inspection_failed",
];

/// The framework-facing accessor for the pending diagnostic codes.
pub fn pending_codes() -> &'static [&'static str] {
    PENDING_CODES
}

/// Whether a diagnostic code means "not yet" rather than "cannot".
pub fn is_pending(code: &str) -> bool {
    PENDING_CODES.contains(&code)
}

fn recognized(code: &str) -> Option<&'static str> {
    DIAGNOSTIC_CODES
        .iter()
        .copied()
        .find(|known| *known == code)
}

/// The probe's own code when it is one this host recognizes, else a marker.
pub fn diagnostic(probe: &Map<String, Value>) -> &'static str {
    match probe
        .get("error")
        .and_then(Value::as_str)
        .and_then(recognized)
    {
        Some(code) => code,
        None => "unrecognized_guest_diagnostic",
    }
}

fn validate_kind(kind: &str) -> Result<(), GuestError> {
    if KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(GuestError::UnsupportedKind)
    }
}

fn command(kind: &str, operation: &str) -> Result<String, GuestError> {
    validate_kind(kind)?;
    Ok(format!("{AGENT_COMMAND} {kind} {operation}"))
}

/// The fixed probe command for a console kind.
pub fn probe_command(kind: &str) -> Result<String, GuestError> {
    command(kind, "probe")
}

/// The fixed streaming command for a console kind.
pub fn stream_command(kind: &str) -> Result<String, GuestError> {
    command(kind, "serve")
}

/// Validate one bounded probe record, without echoing untrusted diagnostics.
pub fn parse_probe(stdout: &[u8]) -> Result<Map<String, Value>, GuestError> {
    if stdout.len() > 32768 {
        return Err(GuestError::InvalidProbe);
    }
    let text = std::str::from_utf8(stdout).map_err(|_| GuestError::InvalidProbe)?;
    let parsed: Value = serde_json::from_str(text).map_err(|_| GuestError::InvalidProbe)?;
    let result = match parsed {
        Value::Object(object) => object,
        _ => return Err(GuestError::InvalidProbe),
    };
    if result.get("version").and_then(Value::as_i64) != Some(PROTOCOL_VERSION) {
        return Err(GuestError::InvalidProbe);
    }
    let kind = result.get("kind").and_then(Value::as_str);
    if !kind.map(|k| KINDS.contains(&k)).unwrap_or(false) {
        return Err(GuestError::InvalidProbe);
    }
    let ready = match result.get("ready").and_then(Value::as_bool) {
        Some(ready) => ready,
        None => return Err(GuestError::InvalidProbe),
    };
    if ready {
        let session = match result.get("session") {
            Some(Value::Object(session)) => session,
            _ => return Err(GuestError::InvalidProbe),
        };
        if session
            .get("id")
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true)
        {
            return Err(GuestError::InvalidProbe);
        }
        if session
            .get("uid")
            .and_then(Value::as_i64)
            .map(|uid| uid <= 0)
            .unwrap_or(true)
        {
            return Err(GuestError::InvalidProbe);
        }
        if session
            .get("user")
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true)
        {
            return Err(GuestError::InvalidProbe);
        }
        if !matches!(result.get("error"), None | Some(Value::Null)) {
            return Err(GuestError::InvalidProbe);
        }
    } else if !matches!(result.get("error"), Some(Value::String(_))) {
        return Err(GuestError::InvalidProbe);
    }
    Ok(result)
}

/// The absolute path of the native guest agent binary.
///
/// `VM_GUEST_CONSOLE_AGENT` wins; otherwise `guest-console-agent` next to the
/// running executable.
pub fn agent_path() -> Result<PathBuf, GuestError> {
    if let Some(path) = std::env::var_os("VM_GUEST_CONSOLE_AGENT") {
        return Ok(PathBuf::from(path));
    }
    let executable = std::env::current_exe().map_err(|_| GuestError::AgentUnavailable)?;
    let directory = executable.parent().ok_or(GuestError::AgentUnavailable)?;
    Ok(directory.join("guest-console-agent"))
}

/// The absolute path of the guest agent binary for a guest of `kind`.
///
/// The Python original shipped `guest-console-agent.py`, which runs on any
/// guest that has an interpreter, so one artifact served every guest. The
/// native port streams machine code instead, and machine code must match the
/// *guest's* operating system rather than the host's: a macOS host leasing a
/// Linux guest would otherwise upload a Mach-O image that the guest cannot
/// execute (observed as a probe that never answers).
///
/// Resolution order:
/// 1. `VM_GUEST_CONSOLE_AGENT` — explicit override, wins for every kind.
/// 2. `guest-console-agent-<kind>` beside the running executable.
/// 3. `guest-console-agent` beside the running executable. This is the
///    host-native build, and is correct only when the guest matches the host.
///    It stays the fallback so a matching-kind lease keeps working when no
///    kind-qualified artifact is shipped.
pub fn agent_path_for(kind: &str) -> Result<PathBuf, GuestError> {
    if let Some(path) = std::env::var_os("VM_GUEST_CONSOLE_AGENT") {
        return Ok(PathBuf::from(path));
    }
    let executable = std::env::current_exe().map_err(|_| GuestError::AgentUnavailable)?;
    let directory = executable.parent().ok_or(GuestError::AgentUnavailable)?;
    Ok(resolve_agent_for(directory, kind))
}

/// The kind-qualified sibling wins when it exists; otherwise the host-native
/// name is used. Split out from `agent_path_for` so the choice can be exercised
/// without touching the process environment or the test binary's directory.
fn resolve_agent_for(directory: &Path, kind: &str) -> PathBuf {
    let qualified = directory.join(format!("guest-console-agent-{kind}"));
    if qualified.is_file() {
        return qualified;
    }
    directory.join("guest-console-agent")
}

/// Read the native guest agent binary that `prepare` streams to the guest.
pub fn agent_bytes() -> Result<Vec<u8>, GuestError> {
    std::fs::read(agent_path()?).map_err(|_| GuestError::AgentUnavailable)
}

/// Read the agent binary for a guest of `kind`.
pub fn agent_bytes_for(kind: &str) -> Result<Vec<u8>, GuestError> {
    std::fs::read(agent_path_for(kind)?).map_err(|_| GuestError::AgentUnavailable)
}

/// The remote `sh -c` script that stores the agent atomically from stdin.
///
/// The directory and file are `0700`; the write goes to a temporary sibling
/// first and is then moved into place.
pub fn upload_command() -> String {
    "sh -c 'set -e; d=\"$HOME/.vm-service-console\"; mkdir -p \"$d\"; \
     chmod 700 \"$d\"; t=\"$d/agent.tmp.$$\"; (umask 077; cat >\"$t\"); \
     chmod 700 \"$t\"; mv -f \"$t\" \"$d/agent\"'"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PASSWORD: &str = "a9B7c2D4";

    fn probe(session: Value) -> Map<String, Value> {
        json!({
            "version": 1,
            "kind": "linux",
            "ready": true,
            "session": session,
            "error": null,
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn commands_contain_only_fixed_arguments_and_no_credential() {
        for kind in KINDS {
            for (command, operation) in [
                (probe_command(kind).unwrap(), "probe"),
                (stream_command(kind).unwrap(), "serve"),
            ] {
                assert_eq!(
                    command,
                    format!("\"$HOME/.vm-service-console/agent\" {kind} {operation}")
                );
                assert!(!command.contains(PASSWORD));
                assert!(!command.contains("ssh"));
            }
        }
        for kind in ["Linux", "linux; id", ""] {
            assert!(probe_command(kind).is_err());
            assert!(stream_command(kind).is_err());
        }
    }

    #[test]
    fn upload_command_is_atomic_private_and_stdin_driven() {
        let command = upload_command();
        assert!(command.starts_with("sh -c '"));
        assert!(command.contains("mkdir -p"));
        assert!(command.contains("chmod 700"));
        assert!(command.contains("cat >"));
        assert!(command.contains("mv -f"));
        assert!(command.contains("umask 077"));
        assert!(!command.contains(PASSWORD));
    }

    #[test]
    fn parse_probe_accepts_a_ready_record() {
        let record = json!({
            "version": 1,
            "kind": "linux",
            "ready": true,
            "session": {
                "id": "2",
                "uid": 501,
                "user": "guest",
                "type": "x11",
                "display": ":7",
                "xauthority": "/run/user/501/gdm/Xauthority",
                "boot": "boot-id"
            },
            "error": null,
        });
        let parsed = parse_probe(serde_json::to_string(&record).unwrap().as_bytes()).unwrap();
        assert_eq!(parsed, record.as_object().unwrap().clone());
        let mut without_error = record.as_object().unwrap().clone();
        without_error.remove("error");
        assert!(parse_probe(&serde_json::to_vec(&Value::Object(without_error)).unwrap()).is_ok());
    }

    #[test]
    fn parse_probe_rejects_malformed_records() {
        let record = probe(json!({"id": "2", "uid": 501, "user": "guest"}));
        let mut cases: Vec<Vec<u8>> = vec![
            b"not json".to_vec(),
            b"{}\n{}".to_vec(),
            b"\xff".to_vec(),
            b"null".to_vec(),
            b"[]".to_vec(),
            b"{}".to_vec(),
        ];
        let mut wrong_ready = record.clone();
        wrong_ready.insert("ready".into(), Value::String("yes".into()));
        cases.push(serde_json::to_vec(&Value::Object(wrong_ready)).unwrap());
        let mut empty_session = record.clone();
        empty_session.insert("session".into(), Value::Object(Map::new()));
        cases.push(serde_json::to_vec(&Value::Object(empty_session)).unwrap());
        let mut error_set = record.clone();
        error_set.insert("error".into(), Value::String("bad".into()));
        cases.push(serde_json::to_vec(&Value::Object(error_set)).unwrap());
        cases.push(vec![b'x'; 32769]);
        for case in cases {
            let result = parse_probe(&case);
            assert!(
                result.is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(&case)
            );
            assert_eq!(
                result.unwrap_err().to_string(),
                "invalid guest console probe"
            );
        }
    }

    #[test]
    fn diagnostic_names_only_recognized_codes() {
        let mut known = probe(json!({"id": "2", "uid": 501, "user": "guest"}));
        known.insert("ready".into(), Value::Bool(false));
        known.insert("error".into(), Value::String("x11vnc_unavailable".into()));
        assert_eq!(diagnostic(&known), "x11vnc_unavailable");
        let mut unknown = known.clone();
        unknown.insert("error".into(), Value::String("cat /etc/shadow".into()));
        assert_eq!(diagnostic(&unknown), "unrecognized_guest_diagnostic");
        assert!(is_pending("x11_environment_unavailable"));
        assert!(!is_pending("screensharing_listener_unavailable"));
        assert!(pending_codes().contains(&"inspection_failed"));
    }
}

#[cfg(test)]
mod agent_selection {
    use super::*;
    use std::fs;

    /// The native agent must match the *guest's* operating system. A
    /// kind-qualified sibling must therefore outrank the host-native name: a
    /// macOS host leasing a Linux guest has to stream the ELF build, not its
    /// own Mach-O image (the defect live acceptance exposed).
    #[test]
    fn kind_qualified_sibling_outranks_host_native_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("guest-console-agent"), b"host").unwrap();
        fs::write(dir.path().join("guest-console-agent-linux"), b"elf").unwrap();
        assert_eq!(
            resolve_agent_for(dir.path(), "linux"),
            dir.path().join("guest-console-agent-linux")
        );
        // A kind with no qualified artifact still resolves the host-native name.
        assert_eq!(
            resolve_agent_for(dir.path(), "macos"),
            dir.path().join("guest-console-agent")
        );
    }

    #[test]
    fn missing_qualified_sibling_falls_back_to_host_native_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("guest-console-agent"), b"host").unwrap();
        assert_eq!(
            resolve_agent_for(dir.path(), "linux"),
            dir.path().join("guest-console-agent")
        );
    }
}
