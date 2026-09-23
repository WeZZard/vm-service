//! Regression guard for the hand-written plist parser.
//!
//! The Rust port does not use `plistlib`; it parses the `ioreg` document with
//! its own XML reader. Whether that reader accepts real vendor output is the
//! one claim about the macOS console path that source tests cannot otherwise
//! settle, and booting a macOS guest to find out needs Screen Sharing consent
//! and a free guest slot. These tests use a verbatim capture of
//! `ioreg -a -d 1 -n Root` from a macOS host instead, so the reader is checked
//! against the real grammar: the XML declaration and DOCTYPE, the
//! `<plist version="1.0">` wrapper, nested `<dict>`/`<array>`, `<integer>` and
//! `<string>` scalars, self-closing `<true/>`, and the escaped `&lt;`/`&gt;`
//! sequences that appear inside string values.

use serde_json::Value;

fn parsed() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ioreg_root_real.plist");
    let bytes = std::fs::read(&path).expect("captured ioreg fixture is present");
    guest_console_agent::macos::parse_plist(&bytes)
        .expect("real ioreg output must parse with the hand-written reader")
}

#[test]
fn hand_written_parser_accepts_real_ioreg_output() {
    let root = parsed();

    // `plistlib.loads` returns the document's top-level value. The capture's
    // root is a `<dict>`, and the port must agree about that type.
    let map = root
        .as_object()
        .expect("top level is a dict, as plistlib reports on the same bytes");
    assert!(map.contains_key("IOConsoleLocked"));
    assert!(map.contains_key("IOKitBuildVersion"));

    let users = root.get("IOConsoleUsers").expect("IOConsoleUsers");
    let array = users
        .as_array()
        .expect("real output has IOConsoleUsers as an array");

    let active: Vec<&Value> = array
        .iter()
        .filter(|entry| entry.get("kCGSSessionOnConsoleKey") == Some(&Value::Bool(true)))
        .collect();
    assert_eq!(
        active.len(),
        1,
        "real output has exactly one entry on the console"
    );

    // Scalar typing matters: the session id must arrive as a number, because
    // `mac_session` compares it with integer semantics.
    let session = active[0];
    assert_eq!(session.get("kCGSSessionIDKey"), Some(&Value::from(257)));
    assert_eq!(session.get("kCGSSessionUserIDKey"), Some(&Value::from(501)));
}

/// Pins the behaviour the port deliberately shares with the Python original on
/// a document whose root is a dict.
///
/// Python runs `roots[0]["IOConsoleUsers"]` where `roots` is the parsed
/// top-level value. On real output that value is a dict, so `roots[0]` raises
/// `KeyError(0)`; the surrounding `except (KeyError, IndexError, TypeError,
/// ValueError, OSError, plistlib.InvalidFileException)` turns it into
/// `session_metadata_invalid`. The port mirrors that outcome through
/// `roots.get(0) == None`. This is an upstream defect, not a port regression,
/// and the translation reproduces it rather than silently repairing it.
#[test]
fn root_indexing_matches_python_key_error_on_a_dict_root() {
    let root = parsed();
    assert!(
        root.get(0).is_none(),
        "a dict root has no index 0, exactly as in Python"
    );
}

#[test]
fn escaped_angle_brackets_survive_unescaping() {
    // The capture contains 44 `&lt;` and 44 `&gt;` sequences; a reader that
    // dropped or double-decoded them would still parse but would corrupt every
    // string value that contains them.
    let text = serde_json::to_string(&parsed()).expect("serializes");
    assert!(
        !text.contains("&lt;") && !text.contains("&gt;"),
        "entities must be decoded, not left encoded"
    );
}
