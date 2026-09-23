//! Unit tests for the pure parsing, validation and framing helpers.
//!
//! Behavior that needs a real Linux or macOS session (actual `loginctl`,
//! `ioreg`, `pfctl`, `launchctl`, `lsof`, an x11vnc run or a real RFB peer) is
//! documented as live behavior and not exercised here.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::config::{self, CLOCK_SKEW_ALLOWANCE, MAX_LIFETIME};
use crate::linux;
use crate::macos::{self, PfReadings, MAC_SERVER};
use crate::proc;
use crate::relay;
use crate::serve;
use crate::{Kind, Unavailable};

const PASSWORD: &str = "a9B7c2D4";
const RULE: &str = "block drop in quick on ! lo0 proto tcp from any to any port = 5900";

/// Serializes the tests that mutate process-wide state (the subprocess deadline
/// and the relay check interval).
static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

/// A queued fake `(exit code, stdout)` result per fake process invocation.
type ProcessOutcomes = Rc<RefCell<Vec<(i32, Vec<u8>)>>>;

/// A recorded `(argv, env pairs)` call made through the fake runner.
type RunnerCalls = Rc<RefCell<Vec<(Vec<String>, Vec<(String, String)>)>>>;

/// A queued fake runner result per fake runner invocation.
type RunnerOutcomes = Rc<RefCell<Vec<Result<Vec<u8>, Unavailable>>>>;

// ---------------------------------------------------------------------------
// properties / identity helpers
// ---------------------------------------------------------------------------

#[test]
fn properties_parses_key_value_lines() {
    let parsed = linux::properties("Id=2\nUser=501\nActive=yes\n").unwrap();
    assert_eq!(parsed.get("Id").map(String::as_str), Some("2"));
    assert_eq!(parsed.get("User").map(String::as_str), Some("501"));
    assert_eq!(parsed.get("Active").map(String::as_str), Some("yes"));
}

#[test]
fn properties_rejects_missing_separator_and_duplicates() {
    assert_eq!(
        linux::properties("Id=2\nnonsense\n").unwrap_err().code(),
        "session_metadata_invalid"
    );
    assert_eq!(
        linux::properties("Id=2\nId=3\n").unwrap_err().code(),
        "session_metadata_invalid"
    );
}

#[test]
fn session_id_and_display_shapes() {
    assert!(linux::is_session_id("2"));
    assert!(linux::is_session_id("session-1_A"));
    assert!(!linux::is_session_id(""));
    assert!(!linux::is_session_id("a b"));
    assert!(!linux::is_session_id("a/b"));

    assert!(linux::is_local_x11_display(":0"));
    assert!(linux::is_local_x11_display(":7.0"));
    assert!(!linux::is_local_x11_display("7"));
    assert!(!linux::is_local_x11_display(":"));
    assert!(!linux::is_local_x11_display(":.0"));
    assert!(!linux::is_local_x11_display("remote:0"));
    assert!(!linux::is_local_x11_display(":7."));
}

#[test]
fn environment_block_split_keeps_last_duplicate() {
    let values = linux::split_environment(b"DISPLAY=:7\0DISPLAY=:8\0XAUTHORITY=/x\0NOEQUAL\0");
    assert_eq!(
        values.get(b"DISPLAY".as_slice()).map(Vec::as_slice),
        Some(b":8".as_slice())
    );
    assert_eq!(
        values.get(b"XAUTHORITY".as_slice()).map(Vec::as_slice),
        Some(b"/x".as_slice())
    );
    assert!(!values.contains_key(b"NOEQUAL".as_slice()));
}

// ---------------------------------------------------------------------------
// configuration validation
// ---------------------------------------------------------------------------

fn session_proof() -> Value {
    json!({"id": "2", "uid": 501, "user": "guest", "type": "x11"})
}

fn config_at(kind: Kind, now: f64) -> Value {
    let mut object = Map::new();
    object.insert("version".to_string(), json!(1));
    object.insert("session".to_string(), session_proof());
    object.insert("expires_at".to_string(), json!(now + 60.0));
    if kind == Kind::Linux {
        object.insert("password".to_string(), json!(PASSWORD));
    }
    Value::Object(object)
}

fn config_for(kind: Kind) -> Value {
    config_at(kind, crate::api_time::unix_seconds())
}

#[test]
fn valid_config_returns_a_future_deadline() {
    assert!(
        config::validate_config(&config_for(Kind::Linux), Kind::Linux).unwrap() > Instant::now()
    );
    assert!(
        config::validate_config(&config_for(Kind::Macos), Kind::Macos).unwrap() > Instant::now()
    );
}

#[test]
fn strict_configuration_rejects_every_deviation() {
    let now = 1000000.0;
    assert!(
        config::validate_config_at(&config_at(Kind::Linux, now), Kind::Linux, now, 0.0).is_ok()
    );

    // A password is required for Linux and forbidden for macOS.
    let mut with_password = config_at(Kind::Macos, now).as_object().unwrap().clone();
    with_password.insert("password".to_string(), json!(PASSWORD));
    assert_eq!(
        config::validate_config_at(&Value::Object(with_password), Kind::Macos, now, 0.0)
            .unwrap_err()
            .code(),
        "configuration_invalid"
    );

    let mut extra_key = config_at(Kind::Linux, now).as_object().unwrap().clone();
    extra_key.insert("command".to_string(), json!("anything"));

    let mut missing_version = config_at(Kind::Linux, now).as_object().unwrap().clone();
    missing_version.remove("version");

    let mut null_session = config_at(Kind::Linux, now).as_object().unwrap().clone();
    null_session.insert("session".to_string(), Value::Null);

    let mut empty_session = config_at(Kind::Linux, now).as_object().unwrap().clone();
    empty_session.insert("session".to_string(), json!({}));

    let mut bool_version = config_at(Kind::Linux, now).as_object().unwrap().clone();
    bool_version.insert("version".to_string(), json!(true));

    let mut wrong_version = config_at(Kind::Linux, now).as_object().unwrap().clone();
    wrong_version.insert("version".to_string(), json!(2));

    let mut bool_expiry = config_at(Kind::Linux, now).as_object().unwrap().clone();
    bool_expiry.insert("expires_at".to_string(), json!(true));

    let mut past_expiry = config_at(Kind::Linux, now).as_object().unwrap().clone();
    past_expiry.insert("expires_at".to_string(), json!(now - 1.0));

    let mut far_expiry = config_at(Kind::Linux, now).as_object().unwrap().clone();
    far_expiry.insert(
        "expires_at".to_string(),
        json!(now + MAX_LIFETIME + CLOCK_SKEW_ALLOWANCE + 10.0),
    );

    let cases = [
        (Value::Object(extra_key), "configuration_invalid"),
        (Value::Object(missing_version), "configuration_invalid"),
        (Value::Object(null_session), "session_proof_required"),
        (Value::Object(empty_session), "session_proof_required"),
        (Value::Object(bool_version), "configuration_invalid"),
        (Value::Object(wrong_version), "configuration_invalid"),
        (Value::Object(bool_expiry), "deadline_invalid"),
        (Value::Object(past_expiry), "deadline_invalid"),
        (Value::Object(far_expiry), "deadline_invalid"),
    ];
    for (mutation, expected) in cases {
        assert_eq!(
            config::validate_config_at(&mutation, Kind::Linux, now, 0.0)
                .unwrap_err()
                .code(),
            expected,
            "unexpected result for {mutation}"
        );
    }
}

#[test]
fn password_shape_is_enforced() {
    let now = 1000000.0;
    for password in [
        "short",
        "123456789",
        "#abcdefg",
        "__SKIP__",
        "__COMM__",
        "a\nbcdefg",
        "\u{e9}bcdefgh",
        " abcdefg",
    ] {
        let mut object = config_at(Kind::Linux, now).as_object().unwrap().clone();
        object.insert("password".to_string(), json!(password));
        let error =
            config::validate_config_at(&Value::Object(object), Kind::Linux, now, 0.0).unwrap_err();
        assert_eq!(error.code(), "password_invalid", "password {password:?}");
    }
}

#[test]
fn deadline_errors_are_distinct_from_configuration_errors() {
    let now = 1000000.0;
    let mut object = config_at(Kind::Linux, now).as_object().unwrap().clone();
    object.insert("expires_at".to_string(), json!(true));
    assert_eq!(
        config::validate_config_at(&Value::Object(object), Kind::Linux, now, 0.0)
            .unwrap_err()
            .code(),
        "deadline_invalid"
    );
}

#[test]
fn hard_ceiling_tolerates_skew_without_extending_the_monotonic_limit() {
    let now = 1000000.0;
    let mut object = config_at(Kind::Linux, now).as_object().unwrap().clone();
    object.insert("expires_at".to_string(), json!(now + MAX_LIFETIME + 120.0));
    let deadline =
        config::validate_config_at(&Value::Object(object.clone()), Kind::Linux, now, 50.0).unwrap();
    assert_eq!(deadline, 50.0 + MAX_LIFETIME);

    object.insert("expires_at".to_string(), json!(now + 20.0));
    let deadline =
        config::validate_config_at(&Value::Object(object), Kind::Linux, now, 50.0).unwrap();
    assert_eq!(deadline, 70.0);
}

// ---------------------------------------------------------------------------
// configuration framing
// ---------------------------------------------------------------------------

fn read_wire(wire: &[u8], max: usize) -> Result<Value, Unavailable> {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(wire).unwrap();
    drop(writer);
    config::read_config_limited(reader.as_raw_fd(), max, 10.0)
}

#[test]
fn binary_after_json_is_not_consumed() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    let config_value = config_for(Kind::Linux);
    writer
        .write_all(serde_json::to_string(&config_value).unwrap().as_bytes())
        .unwrap();
    writer.write_all(b"\nRFB 003.008\n\x00\xff").unwrap();

    let parsed = config::read_config(reader.as_raw_fd()).unwrap();
    assert_eq!(parsed, config_value);

    // Everything after the newline must still be readable.
    reader.set_nonblocking(true).unwrap();
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    assert_eq!(rest, b"RFB 003.008\n\x00\xff");
}

#[test]
fn bounded_initial_line_and_eof() {
    assert_eq!(
        read_wire(b"{}", 4).unwrap_err().code(),
        "configuration_incomplete"
    );
    assert_eq!(
        read_wire(b"\xff\n", 4).unwrap_err().code(),
        "configuration_invalid"
    );
    assert_eq!(
        read_wire(b"123456", 4).unwrap_err().code(),
        "configuration_too_large"
    );
}

// ---------------------------------------------------------------------------
// probe shape
// ---------------------------------------------------------------------------

#[test]
fn probe_record_shape_on_success() {
    let session = session_proof();
    let isolation = json!({"mechanism": "inetd", "guest_tcp_listener": false});
    let record = serve::probe_record(Kind::Linux, Ok((session.clone(), isolation.clone())));
    assert_eq!(record["version"], json!(1));
    assert_eq!(record["kind"], json!("linux"));
    assert_eq!(record["ready"], json!(true));
    assert_eq!(record["session"], session);
    assert_eq!(record["isolation"], isolation);
    assert_eq!(record["backend"], json!("x11vnc-inetd"));
    assert_eq!(record["authentication"], json!("vnc-password"));
    assert_eq!(record["view_only"], json!(true));
    assert_eq!(record["acceptance"], json!("unverified"));
    assert_eq!(record["error"], Value::Null);
}

#[test]
fn probe_record_shape_on_failure_never_invents_a_session() {
    let record = serve::probe_record(Kind::Macos, Err(Unavailable::new("pf_disabled")));
    assert_eq!(record["version"], json!(1));
    assert_eq!(record["kind"], json!("macos"));
    assert_eq!(record["ready"], json!(false));
    assert_eq!(record["session"], Value::Null);
    assert_eq!(record["error"], json!("pf_disabled"));
    assert_eq!(record["view_only"], json!(false));
    assert_eq!(record["backend"], json!("apple-screen-sharing"));
    assert_eq!(record["authentication"], json!("human-apple-account"));
    assert!(record.get("isolation").is_none());
}

#[test]
fn ready_rejects_the_other_platform_without_probing() {
    let other = if cfg!(target_os = "macos") {
        Kind::Linux
    } else {
        Kind::Macos
    };
    let error = serve::ready(other).unwrap_err();
    assert_eq!(error.code(), "guest_platform_mismatch");
}

// ---------------------------------------------------------------------------
// PF policy verification
// ---------------------------------------------------------------------------

struct PfFixture {
    status: String,
    rules: String,
    interfaces: String,
    states: String,
    names: Vec<String>,
    nat: String,
}

impl Default for PfFixture {
    fn default() -> Self {
        PfFixture {
            status: "Status: Enabled for 1 day\n".to_string(),
            rules: format!("{RULE}\nanchor com.apple/* all\n"),
            interfaces: "all\nlo0 (skip)\nen0\nutun0\n".to_string(),
            states: String::new(),
            names: ["lo0", "en0", "utun0"]
                .iter()
                .map(|name| name.to_string())
                .collect(),
            nat: String::new(),
        }
    }
}

impl PfFixture {
    fn readings(&self) -> PfReadings<'_> {
        PfReadings {
            status_text: &self.status,
            rules_text: &self.rules,
            interfaces_text: &self.interfaces,
            states_text: &self.states,
            interface_names: &self.names,
            nat_text: &self.nat,
        }
    }
}

fn check_pf(fixture: &PfFixture) -> Result<Value, Unavailable> {
    macos::verify_pf(fixture.readings())
}

#[test]
fn actual_active_policy_is_required_and_both_families_are_covered() {
    let result = check_pf(&PfFixture::default()).unwrap();
    assert_eq!(result["policy_checks_passed"], json!(true));
    assert_eq!(result["external_acceptance"], json!("unverified"));
    assert!(result.get("verified").is_none());
    assert_eq!(result["rules_sha256"].as_str().unwrap().len(), 64);

    let families = RULE.replace("proto", "inet proto").to_string()
        + "\n"
        + &RULE.replace("proto", "inet6 proto");
    assert!(check_pf(&PfFixture {
        rules: families,
        ..Default::default()
    })
    .is_ok());

    for rule in [
        "",
        "# vm-service prepared",
        &RULE.replace("quick ", ""),
        &RULE.replace("proto", "inet proto"),
        &format!("pass in quick all\n{RULE}"),
        &format!("anchor \"console\"\n{RULE}"),
    ] {
        assert_eq!(
            check_pf(&PfFixture {
                rules: rule.to_string(),
                ..Default::default()
            })
            .unwrap_err()
            .code(),
            "pf_rules_unverified",
            "rule {rule:?}"
        );
    }
}

#[test]
fn test_any_translation_or_anchor_rejects_even_without_states() {
    for translation in [
        "rdr pass on en0 proto tcp from any to any port = 15900 -> 127.0.0.1 port 5900",
        "rdr-anchor \"com.apple/*\" all",
        "nat-anchor \"outer/nested/*\" all",
        "binat-anchor \"outer\" all",
        "nat on en0 from any to any -> (en0)",
        "unknown translation syntax",
    ] {
        let fixture = PfFixture {
            nat: translation.to_string(),
            ..Default::default()
        };
        assert_eq!(
            check_pf(&fixture).unwrap_err().code(),
            "pf_translation_not_supported",
            "translation {translation:?}"
        );
    }
    assert!(check_pf(&PfFixture {
        nat: " \n\t".to_string(),
        ..Default::default()
    })
    .is_ok());
}

/// Expectations measured with CPython 3.14.7 from
/// `re.search(r"^Status:\s+Enabled\b", status_text, re.M)` at
/// `bin/guest-console-agent.py:207`: `\s+` may cross newlines, and `re.M`
/// makes `^` match at the string start and after every `\n`.
#[test]
fn pf_status_line_matches_python_multiline_regex() {
    for (status, expected) in [
        ("Status: Enabled", true),
        ("Status:  Enabled", true),
        ("Status:\tEnabled", true),
        ("Status:\nEnabled", true),
        ("Status:  \n\tEnabled", true),
        ("Status:\r\nEnabled", true),
        ("Status:\rEnabled", true),
        ("Status:\u{000c}Enabled", true),
        ("Status:\u{000b}Enabled", true),
        ("Status: EnabledX", false),
        ("Status: Enabled_", false),
        ("Status: Enabled9", false),
        ("SStatus: Enabled", false),
        ("x\nStatus: Enabled", true),
        ("x\nStatus:\nEnabled", true),
        ("Status:Enabled", false),
        ("Status: ", false),
        (" Status: Enabled", false),
        ("\nStatus: Enabled", true),
        ("Status: Enabled\n", true),
        ("Status:\n\nEnabled", true),
        ("Status:\n  Enabled", true),
        ("Status:\nEnabled@", true),
        ("Status:\nEnabled-", true),
        ("Status: Enabled ", true),
        ("Status:\nStatus: Enabled", true),
        ("Status: Disabled", false),
        ("Status:\nEnabled\n", true),
        ("Status:\n\n\nEnabled", true),
        ("Status:\u{001c}Enabled", true),
        ("Status:\u{00a0}Enabled", true),
        ("Status: Enabled\u{00e9}", false),
        ("Status: Enabled\u{4e09}", false),
        ("Status: Enabled\u{00b2}", false),
        // `Other_Alphabetic` marks are not `\\w` in CPython, so the boundary
        // holds and the line matches.
        ("Status: Enabled\u{0345}", true),
        ("Status: Enabled\u{05b0}", true),
        ("Status: Enabled\u{0363}", true),
        ("Status:\nEnabledX", false),
        ("xxStatus: Enabled", false),
        ("\nStatus:\nEnabled", true),
    ] {
        let result = check_pf(&PfFixture {
            status: status.to_string(),
            ..Default::default()
        });
        if expected {
            assert!(result.is_ok(), "status {status:?} should be enabled");
        } else {
            assert_eq!(
                result.unwrap_err().code(),
                "pf_disabled",
                "status {status:?}"
            );
        }
    }
}

#[test]
fn test_disabled_pf_and_skipped_interfaces_fail() {
    assert_eq!(
        check_pf(&PfFixture {
            status: "Status: Disabled".to_string(),
            ..Default::default()
        })
        .unwrap_err()
        .code(),
        "pf_disabled"
    );

    assert_eq!(
        check_pf(&PfFixture {
            interfaces: "lo0 (skip)\nen0 (skip)\nutun0".to_string(),
            ..Default::default()
        })
        .unwrap_err()
        .code(),
        "pf_interface_bypass"
    );

    assert_eq!(
        check_pf(&PfFixture {
            interfaces: "all (skip)\nlo0\nen0\nutun0".to_string(),
            ..Default::default()
        })
        .unwrap_err()
        .code(),
        "pf_interface_bypass"
    );

    assert_eq!(
        check_pf(&PfFixture {
            interfaces: "lo0\nen0".to_string(),
            ..Default::default()
        })
        .unwrap_err()
        .code(),
        "pf_interfaces_unverified"
    );
}

#[test]
fn preexisting_external_states_cannot_bypass_new_rules() {
    // Expected codes come from CPython `bin/guest-console-agent.py:244`:
    // `not all(ipaddress.ip_address(ip.split("%", 1)[0]).is_loopback ...)`
    // fails `pf_existing_connection`, while an unparseable endpoint
    // (`ValueError`) fails `pf_states_unverified`.
    for state in [
        "all tcp 192.168.64.2:5900 <- 192.168.64.1:50001 ESTABLISHED:ESTABLISHED",
        "all tcp [2001:db8::2]:5900 <- [2001:db8::1]:50001 ESTABLISHED:ESTABLISHED",
        "all tcp 2001:db8::2[5900] <- 2001:db8::1[50001] ESTABLISHED:ESTABLISHED",
        // IPv4-mapped IPv6 is non-loopback when the embedded IPv4 is not 127/8.
        "all tcp [::ffff:8.8.8.8]:5900 <- [::ffff:8.8.8.8]:50001 ESTABLISHED:ESTABLISHED",
        "all tcp [::ffff:0:127.0.0.1]:5900 <- [::ffff:0:127.0.0.1]:50001 ESTABLISHED:ESTABLISHED",
        "all tcp [::2]:5900 <- [::2]:50001 ESTABLISHED:ESTABLISHED",
    ] {
        assert_eq!(
            check_pf(&PfFixture {
                states: state.to_string(),
                ..Default::default()
            })
            .unwrap_err()
            .code(),
            "pf_existing_connection",
            "state {state:?}"
        );
    }

    for state in [
        "all tcp 127.0.0.1:5900 <- 127.0.0.1:50001 ESTABLISHED:ESTABLISHED",
        "all tcp ::1[5900] <- ::1[50001] ESTABLISHED:ESTABLISHED",
        // CPython treats `::ffff:127.0.0.0/104`-style mapped addresses as
        // loopback via `IPv6Address.ipv4_mapped.is_loopback`.
        "all tcp [::ffff:127.0.0.1]:5900 <- [::ffff:127.0.0.1]:50001 ESTABLISHED:ESTABLISHED",
        "all tcp [0:0:0:0:0:ffff:7f00:1]:5900 <- [0:0:0:0:0:ffff:7f00:1]:50001 ESTABLISHED:ESTABLISHED",
        "all tcp [::1%lo0]:5900 <- [::1%lo0]:50001 ESTABLISHED:ESTABLISHED",
    ] {
        assert!(
            check_pf(&PfFixture {
                states: state.to_string(),
                ..Default::default()
            })
            .is_ok(),
            "state {state:?}"
        );
    }

    assert_eq!(
        check_pf(&PfFixture {
            states: "all tcp [not-an-ip]:5900 <- [not-an-ip]:50001 ESTABLISHED:ESTABLISHED"
                .to_string(),
            ..Default::default()
        })
        .unwrap_err()
        .code(),
        "pf_states_unverified"
    );
}

#[test]
fn python_whitespace_and_word_boundaries_are_honoured_in_pf_verification() {
    // CPython `str.strip()`, `str.isspace()` and `str.split()` treat the ASCII
    // information separators U+001C-U+001F as whitespace, while Rust's
    // `char::is_whitespace` does not. Every input below is ignored or split by
    // the Python adapter, so verification still succeeds.
    for nat in ["\u{1c}", "\u{1c}\u{1f}"] {
        assert!(
            check_pf(&PfFixture {
                nat: nat.to_string(),
                ..Default::default()
            })
            .is_ok(),
            "nat {nat:?}"
        );
    }
    for interfaces in [
        "all\nlo0 (skip)\nen0\nutun0\n\u{1c}",
        "all\nlo0 (skip)\nen0\nutun0\n\u{1c}vtnet0",
    ] {
        assert!(
            check_pf(&PfFixture {
                interfaces: interfaces.to_string(),
                ..Default::default()
            })
            .is_ok(),
            "interfaces {interfaces:?}"
        );
    }
    // Python's `splitlines()` breaks at U+001C, so `lo0\u{1c}(skip)` becomes
    // the two lines `lo0` and `(skip)`. The second is not an interface line and
    // the whole check fails. A splitter that only knows `\n` would read the
    // line as `lo0\u{1c}(skip)`, take `skip` as its flags, and accept it, which
    // is what this fixture used to assert. Measured on CPython 3.14.7.
    assert_eq!(
        check_pf(&PfFixture {
            interfaces: "all\nlo0\u{1c}(skip)\nen0\nutun0".to_string(),
            ..Default::default()
        })
        .unwrap_err()
        .code(),
        "pf_interfaces_unverified"
    );

    // CPython's `\b` is Unicode-aware, so a state line whose `5900` is followed
    // by a word character does not mention the port and is ignored. An
    // ASCII-only boundary check would match it and fail the whole check.
    for state in ["x:5900é", "x[5900é"] {
        assert!(
            check_pf(&PfFixture {
                states: state.to_string(),
                ..Default::default()
            })
            .is_ok(),
            "state {state:?}"
        );
    }
    // A non-word character after `5900` does match, so the external connection
    // is still reported.
    assert_eq!(
        check_pf(&PfFixture {
            states: "all tcp 192.168.64.2:5900é <- 192.168.64.1:50001 ESTABLISHED:ESTABLISHED\nall tcp 192.168.64.2:5900- <- 192.168.64.1:50001 ESTABLISHED:ESTABLISHED"
                .to_string(),
            ..Default::default()
        })
        .unwrap_err()
        .code(),
        "pf_existing_connection"
    );
}

// ---------------------------------------------------------------------------
// XML plist parsing
// ---------------------------------------------------------------------------

#[test]
fn plist_parser_reads_the_ioreg_shape() {
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- comment -->
<plist version="1.0">
<dict>
    <key>IOConsoleUsers</key>
    <array>
        <dict>
            <key>kCGSSessionOnConsoleKey</key><true/>
            <key>kCGSSessionIDKey</key><integer>257</integer>
            <key>kCGSSessionUserNameKey</key><string>guest &amp; co</string>
            <key>kCGSSessionLoggedOutKey</key><false/>
            <key>confidence</key><real>0.5</real>
        </dict>
    </array>
</dict>
</plist>"#;
    let value = macos::parse_plist(xml).unwrap();
    let entry = &value["IOConsoleUsers"][0];
    assert_eq!(entry["kCGSSessionOnConsoleKey"], json!(true));
    assert_eq!(entry["kCGSSessionIDKey"], json!(257));
    assert_eq!(entry["kCGSSessionUserNameKey"], json!("guest & co"));
    assert_eq!(entry["kCGSSessionLoggedOutKey"], json!(false));
    assert_eq!(entry["confidence"], json!(0.5));
}

#[test]
fn plist_parser_rejects_malformed_documents() {
    assert!(macos::parse_plist(b"not a plist").is_err());
    assert!(macos::parse_plist(b"<plist><dict><key>x</key></dict></plist>").is_err());
    assert!(macos::parse_plist(b"<plist><unknown/></plist>").is_err());
}

// ---------------------------------------------------------------------------
// relay
// ---------------------------------------------------------------------------

fn read_exact_from(stream: &mut UnixStream, length: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; length];
    let mut filled = 0;
    while filled < length {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(_) => break,
        }
    }
    buffer.truncate(filled);
    buffer
}

#[test]
fn relay_forwards_binary_both_ways_and_owner_eof_revokes() {
    let (endpoint, mut server) = UnixStream::pair().unwrap();
    let (input, mut input_writer) = UnixStream::pair().unwrap();
    let (output, mut output_reader) = UnixStream::pair().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let handle = std::thread::spawn(move || {
        let mut check = || -> Result<(), Unavailable> { Ok(()) };
        relay::relay(
            &endpoint,
            deadline,
            &mut check,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
    });

    server.write_all(b"RFB 003.008\n\x00\xff").unwrap();
    assert_eq!(
        read_exact_from(&mut output_reader, 14),
        b"RFB 003.008\n\x00\xff"
    );

    input_writer.write_all(b"\xff\x00response").unwrap();
    assert_eq!(read_exact_from(&mut server, 11), b"\xff\x00response");

    // EOF on the owner side revokes the whole stream.
    drop(input_writer);
    handle.join().unwrap().unwrap();
}

#[test]
fn relay_deadline_closes_an_idle_stream() {
    let (endpoint, _server) = UnixStream::pair().unwrap();
    let (input, _input_writer) = UnixStream::pair().unwrap();
    let (output, _output_reader) = UnixStream::pair().unwrap();
    let deadline = Instant::now() + Duration::from_millis(50);
    let handle = std::thread::spawn(move || {
        let mut check = || -> Result<(), Unavailable> { Ok(()) };
        relay::relay(
            &endpoint,
            deadline,
            &mut check,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
    });
    assert!(handle.join().unwrap().is_ok());
}

#[test]
fn relay_session_change_ends_an_existing_stream() {
    let _guard = GLOBAL_LOCK.lock().unwrap();
    relay::set_check_interval(Duration::from_millis(10));
    let (endpoint, _server) = UnixStream::pair().unwrap();
    let (input, _input_writer) = UnixStream::pair().unwrap();
    let (output, _output_reader) = UnixStream::pair().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let handle = std::thread::spawn(move || {
        let mut check = || -> Result<(), Unavailable> { Err(Unavailable::new("session_changed")) };
        relay::relay(
            &endpoint,
            deadline,
            &mut check,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
    });
    assert_eq!(
        handle.join().unwrap().unwrap_err().code(),
        "session_changed"
    );
    relay::reset_check_interval();
}

#[test]
fn relay_backpressure_cannot_hold_the_stream_past_the_deadline() {
    let (endpoint, mut server) = UnixStream::pair().unwrap();
    let (input, _input_writer) = UnixStream::pair().unwrap();
    let (output, _output_reader) = UnixStream::pair().unwrap();
    server.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_millis(120);
    let handle = std::thread::spawn(move || {
        let mut check = || -> Result<(), Unavailable> { Ok(()) };
        relay::relay(
            &endpoint,
            deadline,
            &mut check,
            input.as_raw_fd(),
            output.as_raw_fd(),
        )
    });
    let chunk = vec![b'x'; 65536];
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(60) {
        let _ = server.write(&chunk);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(handle.join().unwrap().is_ok());
}

// ---------------------------------------------------------------------------
// subprocess timeout rules
// ---------------------------------------------------------------------------

fn write_script(directory: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn run_captures_stdout_and_honors_allowed_returncodes() {
    let directory = tempfile::tempdir().unwrap();
    let hello = write_script(
        directory.path(),
        "hello.sh",
        "#!/bin/sh\nprintf 'hello\\n'\n",
    );
    let hello = hello.to_str().unwrap().to_string();
    assert_eq!(
        proc::run_text(&[hello.as_str()], None, &[0]).unwrap(),
        "hello\n"
    );

    let exit_three = write_script(directory.path(), "exit3.sh", "#!/bin/sh\nexit 3\n");
    let exit_three = exit_three.to_str().unwrap().to_string();
    assert_eq!(
        proc::run_text(&[exit_three.as_str()], None, &[0])
            .unwrap_err()
            .code(),
        "inspection_failed"
    );
    assert_eq!(
        proc::run_text(&[exit_three.as_str()], None, &[0, 3]).unwrap(),
        ""
    );
}

#[test]
fn run_reports_deadline_expired_without_spawning() {
    let _guard = GLOBAL_LOCK.lock().unwrap();
    proc::set_deadline(Some(Instant::now() - Duration::from_secs(1)));
    let error = proc::run_text(&["/nonexistent-binary"], None, &[0]).unwrap_err();
    assert_eq!(error.code(), "deadline_expired");
    proc::set_deadline(None);
}

// ---------------------------------------------------------------------------
// macOS inspection (ported from tests/unit/test_guest_console.py)
// ---------------------------------------------------------------------------
//
// Python drives these through `mock.patch.object(guest, "run", ...)`,
// `guest.identity`, `guest.os.stat`, `guest.Path.exists` and
// `guest.socket.create_connection`. The Rust equivalents are the per-thread
// seams in `proc` (see `proc.rs`) and the connector parameter on
// `serve::serve_macos`.

/// Installs the fake identity, console owner and absent Remote Management
/// marker every macOS inspection test starts from, and clears every seam on
/// drop so a test cannot leak into another.
struct MacSeams;

impl MacSeams {
    fn install() -> Self {
        proc::set_test_runner(None);
        proc::set_test_identity(Some((501, "guest".to_string())));
        proc::set_test_console_uid(Some(501));
        proc::set_test_remote_management(Some(false));
        MacSeams
    }
}

impl Drop for MacSeams {
    fn drop(&mut self) {
        proc::set_test_runner(None);
        proc::set_test_identity(None);
        proc::set_test_console_uid(None);
        proc::set_test_remote_management(None);
    }
}

/// An active `IOConsoleUsers` entry shaped like `ioreg`'s.
fn console_user_entry(uid: u64, user: &str, session_id: i64) -> Value {
    json!({
        "kCGSSessionOnConsoleKey": true,
        "kCGSSessionLoginDoneKey": true,
        "kCGSSessionIDKey": session_id,
        "kCGSSessionUserIDKey": uid,
        "kCGSSessionUserNameKey": user,
    })
}

/// Serialize the small plist subset the tests need, the way `plistlib.dumps`
/// would for the value shapes `ioreg -a` can emit.
fn plist_xml(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "<array>{}</array>",
            items.iter().map(plist_xml).collect::<String>()
        ),
        Value::Object(map) => format!(
            "<dict>{}</dict>",
            map.iter()
                .map(|(key, value)| format!("<key>{key}</key>{}", plist_xml(value)))
                .collect::<String>()
        ),
        Value::Bool(true) => "<true/>".to_string(),
        Value::Bool(false) => "<false/>".to_string(),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                format!("<integer>{number}</integer>")
            } else {
                format!("<real>{number}</real>")
            }
        }
        Value::String(text) => format!("<string>{text}</string>"),
        Value::Null => "<string></string>".to_string(),
    }
}

/// The `ioreg -a -d 1 -n Root` document whose root holds `io_console_users`.
fn ioreg_document(io_console_users: Value) -> Vec<u8> {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\">{}</plist>",
        plist_xml(&json!([{ "IOConsoleUsers": io_console_users }]))
    )
    .into_bytes()
}

/// A runner that records every argv and dispatches on it, mirroring the
/// `def command(argv, **kwargs)` helpers of the Python tests.
fn recording_runner<F>(calls: Rc<RefCell<Vec<Vec<String>>>>, dispatch: F) -> proc::TestRunner
where
    F: Fn(&[&str]) -> Result<Vec<u8>, Unavailable> + 'static,
{
    Box::new(move |argv: &[&str], _env: Option<&[(&str, &str)]>| {
        calls
            .borrow_mut()
            .push(argv.iter().map(|argument| argument.to_string()).collect());
        dispatch(argv)
    })
}

fn is_command(argv: &[String], expected: &[&str]) -> bool {
    argv.len() == expected.len() && argv.iter().zip(expected).all(|(a, b)| a == b)
}

#[test]
fn test_macos_does_not_probe_endpoint_before_filter_verification() {
    let _seams = MacSeams::install();
    let calls: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
    let entry = console_user_entry(501, "guest", 257);
    let runner = recording_runner(Rc::clone(&calls), move |argv: &[&str]| {
        match argv.first().copied().unwrap_or("") {
            "/usr/sbin/ioreg" => Ok(ioreg_document(json!([entry.clone()]))),
            "/usr/sbin/sysctl" => Ok(b"{ sec = 1234, usec = 0 }\n".to_vec()),
            "/usr/bin/sudo" => {
                assert_eq!(&argv[..3], &["/usr/bin/sudo", "-n", "/sbin/pfctl"]);
                let value: &[u8] = match &argv[3..] {
                    ["-s", "info"] => b"Status: Disabled",
                    ["-s", "rules"] => RULE.as_bytes(),
                    ["-v", "-s", "Interfaces"] => b"lo0\nen0\nutun0",
                    ["-s", "states"] => b"",
                    ["-s", "nat"] => b"",
                    other => panic!("unexpected pfctl arguments {other:?}"),
                };
                Ok(value.to_vec())
            }
            "/sbin/ifconfig" => Ok(b"lo0 en0 utun0".to_vec()),
            other => panic!("endpoint inspection ran before filter verification: {other}"),
        }
    });
    proc::set_test_runner(Some(runner));

    let error = macos::mac_ready().unwrap_err();
    assert_eq!(error.code(), "pf_disabled");

    // The endpoint (`ps`, `launchctl`, `lsof`) must not have been inspected.
    assert!(calls.borrow().iter().all(|argv| {
        !is_command(argv, &["/bin/ps", "-axo", "comm="])
            && !is_command(
                argv,
                &["/bin/launchctl", "print", "system/com.apple.screensharing"],
            )
            && !(argv.first().map(String::as_str) == Some("/usr/bin/sudo")
                && argv.get(3).map(String::as_str) == Some("/usr/sbin/lsof"))
    }));
}

#[test]
fn test_console_identity_comes_from_active_login() {
    let _seams = MacSeams::install();
    let entry = Rc::new(RefCell::new(console_user_entry(501, "guest", 257)));
    let source = Rc::clone(&entry);
    proc::set_test_runner(Some(Box::new(
        move |argv: &[&str], _env: Option<&[(&str, &str)]>| {
            if argv
                .first()
                .is_some_and(|argument| argument.contains("ioreg"))
            {
                Ok(ioreg_document(json!([source.borrow().clone()])))
            } else {
                Ok(b"{ sec = 1234, usec = 0 }".to_vec())
            }
        },
    )));

    assert_eq!(macos::mac_session().unwrap()["id"], json!("257"));

    // A different console user is a mismatch, never a silent success.
    entry.borrow_mut()["kCGSSessionUserIDKey"] = json!(502);
    assert_eq!(
        macos::mac_session().unwrap_err().code(),
        "session_identity_mismatch"
    );
}

#[test]
fn test_endpoint_requires_apple_job_and_owned_listener() {
    let _seams = MacSeams::install();

    // The Apple job plus a pid-1 `launchd` listener bound to `*:5900` is accepted.
    proc::set_test_runner(Some(Box::new(
        |argv: &[&str], _env: Option<&[(&str, &str)]>| {
            if argv.first().copied() == Some("/bin/launchctl") {
                Ok(format!("\tprogram = {MAC_SERVER}\n").into_bytes())
            } else {
                Ok(b"p1\nclaunchd\nu0\nn*:5900\n".to_vec())
            }
        },
    )));
    macos::mac_endpoint().unwrap();

    // A listener owned by a non-root, non-Apple process is rejected.
    let responses: Rc<RefCell<Vec<Vec<u8>>>> = Rc::new(RefCell::new(vec![
        Vec::new(),
        format!("program = {MAC_SERVER}\n").into_bytes(),
        b"p999\ncother\nu501\nn*:5900".to_vec(),
    ]));
    proc::set_test_runner(Some(Box::new(
        move |_argv: &[&str], _env: Option<&[(&str, &str)]>| Ok(responses.borrow_mut().remove(0)),
    )));
    assert_eq!(
        macos::mac_endpoint().unwrap_err().code(),
        "screensharing_listener_unverified"
    );

    // Remote Management is rejected before any process or listener is read.
    proc::set_test_remote_management(Some(true));
    assert_eq!(
        macos::mac_endpoint().unwrap_err().code(),
        "remote_management_not_supported"
    );
}

/// Expectations measured with CPython 3.14.7 from `pid.isdigit()` at
/// `bin/guest-console-agent.py:292`: true for Unicode decimal digits (`Nd`)
/// and digit-like `No` characters, false for `Nl` such as `Ⅷ`.
#[test]
fn listener_pid_digit_test_matches_cpython_isdigit() {
    for (pid, expected) in [
        ("0", true),
        ("9", true),
        ("\u{00b2}", true),
        ("\u{00b9}", true),
        ("\u{2460}", true),
        ("\u{2469}", false),
        ("\u{2167}", false),
        ("\u{0660}", true),
        ("\u{06f0}\u{06f1}", true),
        ("\u{0f20}", true),
        ("\u{ff17}", true),
        ("\u{1d7d8}", true),
        ("\u{104a0}", true),
        ("\u{1f100}", true),
        ("", false),
        ("12a", false),
        ("a12", false),
        ("x", false),
        ("-1", false),
        (" 1", false),
        ("1 ", false),
    ] {
        assert_eq!(linux::is_decimal(pid), expected, "pid {pid:?}");
    }
}

#[test]
fn listener_pid_accepts_cpython_digit_like_superscript() {
    let _seams = MacSeams::install();
    proc::set_test_runner(Some(Box::new(
        |argv: &[&str], _env: Option<&[(&str, &str)]>| match argv.first().copied() {
            Some("/bin/launchctl") => Ok(format!("\tprogram = {MAC_SERVER}\n").into_bytes()),
            Some("/bin/ps") => Ok(format!("{MAC_SERVER}\n").into_bytes()),
            _ => Ok("p\u{00b2}\ncscreensharingd\nu0\nn*:5900\n"
                .as_bytes()
                .to_vec()),
        },
    )));

    macos::mac_endpoint().unwrap();
}

#[test]
fn test_live_translation_inspection_is_required_and_read_only() {
    let _seams = MacSeams::install();
    let calls: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
    let runner = recording_runner(Rc::clone(&calls), |argv: &[&str]| {
        if argv.len() == 2 && argv[0] == "/sbin/ifconfig" && argv[1] == "-l" {
            return Ok(b"lo0 en0 utun0".to_vec());
        }
        assert_eq!(&argv[..3], &["/usr/bin/sudo", "-n", "/sbin/pfctl"]);
        let value: &[u8] = match &argv[3..] {
            ["-s", "info"] => b"Status: Enabled",
            ["-s", "rules"] => RULE.as_bytes(),
            ["-v", "-s", "Interfaces"] => b"lo0\nen0\nutun0",
            ["-s", "states"] => b"",
            ["-s", "nat"] => b"",
            other => panic!("unexpected pfctl arguments {other:?}"),
        };
        Ok(value.to_vec())
    });
    proc::set_test_runner(Some(runner));

    assert_eq!(
        macos::mac_isolation().unwrap()["policy_checks_passed"],
        json!(true)
    );
    assert!(calls
        .borrow()
        .iter()
        .any(|argv| is_command(argv, &["/usr/bin/sudo", "-n", "/sbin/pfctl", "-s", "nat"])));

    // An unreadable NAT table fails the whole inspection instead of being
    // treated as an absent translation ruleset.
    proc::set_test_runner(Some(Box::new(
        |argv: &[&str], _env: Option<&[(&str, &str)]>| {
            if argv.last() == Some(&"nat") {
                return Err(Unavailable::new("inspection_failed"));
            }
            if argv.len() == 2 && argv[0] == "/sbin/ifconfig" && argv[1] == "-l" {
                return Ok(b"lo0 en0 utun0".to_vec());
            }
            let value: &[u8] = match &argv[3..] {
                ["-s", "info"] => b"Status: Enabled",
                ["-s", "rules"] => RULE.as_bytes(),
                ["-v", "-s", "Interfaces"] => b"lo0\nen0\nutun0",
                ["-s", "states"] => b"",
                _ => b"",
            };
            Ok(value.to_vec())
        },
    )));
    assert_eq!(
        macos::mac_isolation().unwrap_err().code(),
        "inspection_failed"
    );
}

#[test]
fn test_connector_is_loopback_only_and_accepts_no_server_password() {
    let _seams = MacSeams::install();

    // macOS carries no server password: a password-bearing macOS config is
    // rejected before any endpoint is contacted.
    let mut config = config_at(Kind::Macos, 1000000.0)
        .as_object()
        .unwrap()
        .clone();
    config.insert("password".to_string(), json!(PASSWORD));
    assert_eq!(
        config::validate_config_at(&Value::Object(config), Kind::Macos, 1000000.0, 0.0)
            .unwrap_err()
            .code(),
        "configuration_invalid"
    );

    // The connector target is the fixed loopback address, never caller input.
    let recorded: Rc<RefCell<Option<SocketAddr>>> = Rc::new(RefCell::new(None));
    let sink = Rc::clone(&recorded);
    let (endpoint, server) = UnixStream::pair().unwrap();
    drop(server);
    let deadline = Instant::now() + Duration::from_millis(50);
    let mut check = || -> Result<(), Unavailable> { Ok(()) };
    serve::serve_macos(deadline, &mut check, move |address, _timeout| {
        *sink.borrow_mut() = Some(address);
        Ok(endpoint)
    })
    .unwrap();
    assert_eq!(
        recorded.borrow().unwrap(),
        "127.0.0.1:5900".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn test_console_users_shape_failures_match_python() {
    let _seams = MacSeams::install();

    // Each case is `(IOConsoleUsers value, expected code, is_unexpected)`.
    // Python raises an uncaught `AttributeError` when it iterates a non-empty
    // dict's keys, a non-empty string's characters, or a list holding a
    // non-object. A non-iterable value raises `TypeError`, which Python's
    // caught tuple turns into `session_metadata_invalid`. Empty iterables
    // simply produce no active entry.
    let cases = vec![
        (
            json!([console_user_entry(501, "guest", 257), "x"]),
            "inspection_failed",
            true,
        ),
        (
            json!([console_user_entry(501, "guest", 257), 5]),
            "inspection_failed",
            true,
        ),
        (json!([]), "active_console_session_required", false),
        (
            json!({"kCGSSessionOnConsoleKey": true}),
            "inspection_failed",
            true,
        ),
        (json!({}), "active_console_session_required", false),
        (json!("x"), "inspection_failed", true),
        (json!(""), "active_console_session_required", false),
        (json!(5), "session_metadata_invalid", false),
        (json!(true), "session_metadata_invalid", false),
    ];
    for (console_users, expected, unexpected) in cases {
        let users = console_users.clone();
        proc::set_test_runner(Some(Box::new(
            move |_argv: &[&str], _env: Option<&[(&str, &str)]>| Ok(ioreg_document(users.clone())),
        )));
        let error = macos::mac_session().unwrap_err();
        assert_eq!(error.code(), expected, "console users {console_users}");
        assert_eq!(
            error.is_unexpected(),
            unexpected,
            "console users {console_users}"
        );
    }

    // A list mixing the active entry with an *object* entry is still accepted:
    // Python's `u.get(...)` only raises for a non-object. This is the boundary
    // the old Rust port got wrong in the other direction by skipping entries.
    let users = json!([console_user_entry(501, "guest", 257), {}]);
    proc::set_test_runner(Some(Box::new(
        move |_argv: &[&str], _env: Option<&[(&str, &str)]>| Ok(ioreg_document(users.clone())),
    )));
    assert_eq!(macos::mac_session().unwrap()["id"], json!("257"));
}

#[test]
fn test_probe_sanitizes_inspection_failure() {
    // Python's `ready` raising `RuntimeError(PASSWORD)` is a generic exception;
    // `probe` reports it as `inspection_failed` and never copies the exception
    // text into the record.
    let record = serve::probe_record(Kind::Macos, Err(Unavailable::unexpected()));
    assert_eq!(record["ready"], json!(false));
    assert_eq!(record["error"], json!("inspection_failed"));
    assert_eq!(record["view_only"], json!(false));
    assert!(!serde_json::to_string(&record).unwrap().contains(PASSWORD));
}

#[test]
fn test_unexpected_inspection_failure_maps_to_probe_and_serve_codes() {
    // `probe` reports an uncaught Python exception as `inspection_failed`...
    let record = serve::probe_record(Kind::Macos, Err(Unavailable::unexpected()));
    assert_eq!(record["ready"], json!(false));
    assert_eq!(record["error"], json!("inspection_failed"));
    assert!(record.get("isolation").is_none());

    // ...while `main`'s generic handler reports it as `stream_failed`.
    assert_eq!(
        serve::failure_message(&Unavailable::unexpected()),
        "stream_failed"
    );

    // An explicit `Unavailable` keeps its own code in both places.
    let record = serve::probe_record(Kind::Macos, Err(Unavailable::new("pf_disabled")));
    assert_eq!(record["error"], json!("pf_disabled"));
    assert_eq!(
        serve::failure_message(&Unavailable::new("pf_disabled")),
        "pf_disabled"
    );
}

#[test]
fn plist_serializer_round_trips_the_test_shapes() {
    // The test serializer must produce what `parse_plist` reads back, so a
    // broken fixture fails here rather than as an unrelated inspection error.
    let document = ioreg_document(json!({"nested": {"flag": true, "count": 3.5}}));
    let value = macos::parse_plist(&document).unwrap();
    assert_eq!(value[0]["IOConsoleUsers"]["nested"]["flag"], json!(true));
    assert_eq!(value[0]["IOConsoleUsers"]["nested"]["count"], json!(3.5));
}

// ---------------------------------------------------------------------------
// Linux session, readiness and serve
// (ported from tests/unit/test_guest_console.py)
// ---------------------------------------------------------------------------
//
// Python drives these through `mock.patch.object(guest, "Path", ...)`,
// `guest.run`, `guest.subprocess.run`, `guest.subprocess.Popen`, `guest.os`,
// `guest.os.access`, `guest.identity`, `guest.read_config`, `guest.ready`,
// `guest.linux_session` and `guest.relay`. The Rust equivalents are the
// per-thread seams in `proc`, `linux`, `config` and `serve`; this suite is the
// only place that installs them and every one is cleared on drop so a test
// cannot leak into another thread's next test.

const SESSION_PROPERTY_ORDER: [&str; 12] = [
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

/// Python's `SESSION` constant: the session proof every patched test uses.
fn session_constant() -> Value {
    json!({
        "id": "2",
        "uid": 501,
        "user": "guest",
        "type": "x11",
        "display": ":7",
        "xauthority": "/run/user/501/gdm/Xauthority",
        "boot": "boot-id"
    })
}

/// The Linux configuration `serve` reads, with a live `expires_at`.
fn serve_config(session: Value, lifetime_seconds: f64) -> Value {
    json!({
        "version": 1,
        "session": session,
        "expires_at": crate::api_time::unix_seconds() + lifetime_seconds,
        "password": PASSWORD,
    })
}

/// Clears every Linux/macOS test seam on drop, and starts from a clean slate.
struct TestSeams;

impl TestSeams {
    fn new() -> Self {
        TestSeams::clear();
        TestSeams
    }

    fn clear() {
        proc::set_test_runner(None);
        proc::set_test_identity(None);
        proc::set_test_console_uid(None);
        proc::set_test_remote_management(None);
        proc::set_test_proc_root(None);
        proc::set_test_process(None);
        linux::set_test_access(None);
        linux::set_test_linux_session(None);
        config::set_test_config(None);
        serve::set_test_ready(None);
        serve::set_test_spawner(None);
        serve::set_test_relay(None);
        serve::clear_test_terminations();
    }
}

impl Drop for TestSeams {
    fn drop(&mut self) {
        TestSeams::clear();
    }
}

/// Clears the process-wide subprocess deadline installed by `serve`.
struct DeadlineReset;

impl Drop for DeadlineReset {
    fn drop(&mut self) {
        proc::set_deadline(None);
    }
}

/// `os.fstat` for the raw file descriptors a spawn request carries.
fn fstat_raw(fd: RawFd) -> libc::stat {
    let mut info: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid out-parameter for the duration of the call.
    assert_eq!(unsafe { libc::fstat(fd, &mut info) }, 0);
    info
}

/// A temporary `/proc` tree, private X authority file and `loginctl` fixture
/// mirroring `LinuxSessionTests.setUp`.
struct LinuxFixture {
    _directory: tempfile::TempDir,
    proc_root: PathBuf,
    auth: PathBuf,
    uid: libc::uid_t,
    metadata: Rc<RefCell<BTreeMap<String, String>>>,
}

impl LinuxFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let proc_root = directory.path().join("proc");
        std::fs::create_dir_all(proc_root.join("101")).unwrap();
        std::fs::create_dir_all(proc_root.join("sys/kernel/random")).unwrap();
        std::fs::write(proc_root.join("sys/kernel/random/boot_id"), "boot-id\n").unwrap();
        let auth = directory.path().join("Xauthority");
        std::fs::write(&auth, b"a cookie, not exposed in the probe").unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();

        let uid = unsafe { libc::getuid() };
        let mut metadata = BTreeMap::new();
        metadata.insert("Id".to_string(), "2".to_string());
        metadata.insert("User".to_string(), uid.to_string());
        metadata.insert("Name".to_string(), "guest".to_string());
        metadata.insert("Active".to_string(), "yes".to_string());
        metadata.insert("Remote".to_string(), "no".to_string());
        metadata.insert("Type".to_string(), "x11".to_string());
        metadata.insert("Class".to_string(), "user".to_string());
        metadata.insert("State".to_string(), "active".to_string());
        metadata.insert("Seat".to_string(), "seat0".to_string());
        metadata.insert("Display".to_string(), ":7".to_string());
        metadata.insert("Leader".to_string(), "101".to_string());
        metadata.insert("TimestampMonotonic".to_string(), "111".to_string());

        let fixture = LinuxFixture {
            _directory: directory,
            proc_root,
            auth,
            uid,
            metadata: Rc::new(RefCell::new(metadata)),
        };
        fixture.set_environment(":7", "2", true);
        fixture
    }

    /// Rewrite `/proc/101/environ`, mirroring `LinuxSessionTests.environment`.
    fn set_environment(&self, display: &str, sid: &str, authority: bool) {
        let mut data = format!("DISPLAY={display}\0XDG_SESSION_ID={sid}\0");
        if authority {
            data.push_str(&format!("XAUTHORITY={}\0", self.auth.display()));
        }
        std::fs::write(self.proc_root.join("101/environ"), data.as_bytes()).unwrap();
    }

    /// Install the `/proc` root, identity and `loginctl` fakes for this thread.
    fn install(&self) {
        proc::set_test_proc_root(Some(self.proc_root.clone()));
        proc::set_test_identity(Some((self.uid, "guest".to_string())));
        let metadata = Rc::clone(&self.metadata);
        let uid_string = self.uid.to_string();
        proc::set_test_runner(Some(Box::new(
            move |argv: &[&str], _env: Option<&[(&str, &str)]>| {
                if argv.contains(&"list-sessions") {
                    return Ok(format!("2 {uid_string} guest seat0 tty7\n").into_bytes());
                }
                // Python's `command` asserts the exact show-session argv.
                let mut expected: Vec<String> =
                    ["/usr/bin/loginctl", "show-session", "2", "--no-pager"]
                        .iter()
                        .map(|value| value.to_string())
                        .collect();
                for name in SESSION_PROPERTY_ORDER {
                    expected.push("-p".to_string());
                    expected.push(name.to_string());
                }
                assert_eq!(
                    argv.iter()
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>(),
                    expected
                );
                let metadata = metadata.borrow();
                let mut output = String::new();
                for name in SESSION_PROPERTY_ORDER {
                    if let Some(value) = metadata.get(name) {
                        output.push_str(&format!("{name}={value}\n"));
                    }
                }
                Ok(output.into_bytes())
            },
        )));
    }
}

impl Drop for LinuxFixture {
    fn drop(&mut self) {
        proc::set_test_proc_root(None);
        proc::set_test_identity(None);
        proc::set_test_runner(None);
    }
}

#[test]
fn test_existing_display_and_authority_are_proven_not_guessed() {
    let _seams = TestSeams::new();
    let fixture = LinuxFixture::new();
    fixture.install();

    let session = linux::linux_session().unwrap();
    assert_eq!(session["display"], json!(":7"));
    assert_eq!(session["id"], json!("2"));
    assert_eq!(
        session["xauthority"],
        json!(fixture.auth.display().to_string())
    );
    assert!(!serde_json::to_string(&session).unwrap().contains("cookie"));
    assert_eq!(session["xauthority_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(session["boot"], json!("boot-id"));
}

#[test]
fn test_ssh_or_other_login_environment_is_not_console_evidence() {
    let _seams = TestSeams::new();
    let fixture = LinuxFixture::new();
    fixture.install();

    fixture.set_environment(":7", "ssh-session", true);
    assert_eq!(
        linux::linux_session().unwrap_err().code(),
        "x11_environment_unavailable"
    );
}

#[test]
fn test_wayland_and_greeter_are_not_substituted() {
    let _seams = TestSeams::new();
    let fixture = LinuxFixture::new();
    fixture.install();

    let cases = [
        ("Type", "wayland", "x11_session_required"),
        ("Class", "greeter", "active_console_session_required"),
        ("Active", "no", "active_console_session_required"),
        ("Remote", "yes", "active_console_session_required"),
        ("Name", "other", "session_identity_mismatch"),
        ("User", "different", "session_identity_mismatch"),
    ];
    for (key, value, expected) in cases {
        let previous = fixture
            .metadata
            .borrow_mut()
            .insert(key.to_string(), value.to_string());
        let error = linux::linux_session().unwrap_err();
        assert_eq!(error.code(), expected, "{key}={value}");
        if let Some(previous) = previous {
            fixture
                .metadata
                .borrow_mut()
                .insert(key.to_string(), previous);
        }
    }
}

#[test]
fn test_missing_or_conflicting_environment_fails() {
    let _seams = TestSeams::new();
    let fixture = LinuxFixture::new();
    fixture.install();

    fixture.set_environment(":7", "2", false);
    assert!(linux::linux_session().is_err());

    fixture.set_environment(":8", "2", true);
    assert_eq!(
        linux::linux_session().unwrap_err().code(),
        "display_identity_mismatch"
    );

    fixture
        .metadata
        .borrow_mut()
        .insert("Display".to_string(), String::new());
    assert_eq!(linux::linux_session().unwrap()["display"], json!(":8"));

    fixture.set_environment("remote.example:0", "2", true);
    assert_eq!(
        linux::linux_session().unwrap_err().code(),
        "local_x11_display_required"
    );
}

#[test]
fn test_authority_must_be_private_regular_owned_file() {
    let _seams = TestSeams::new();
    let fixture = LinuxFixture::new();
    fixture.install();

    std::fs::set_permissions(&fixture.auth, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        linux::linux_session().unwrap_err().code(),
        "xauthority_invalid"
    );

    std::fs::remove_file(&fixture.auth).unwrap();
    std::os::unix::fs::symlink(fixture.proc_root.join("101/environ"), &fixture.auth).unwrap();
    assert_eq!(
        linux::linux_session().unwrap_err().code(),
        "xauthority_unavailable"
    );
}

#[test]
fn test_noble_x11vnc_help_exit_one_is_narrowly_accepted() {
    let _seams = TestSeams::new();
    linux::set_test_linux_session(Some(session_constant()));
    linux::set_test_access(Some(true));

    let options: &[u8] = b"-inetd -viewonly -passwdfile -noremote -nocmds";
    let sequence: ProcessOutcomes = Rc::new(RefCell::new(vec![
        (1, options.to_vec()),
        (0, b"display information".to_vec()),
    ]));
    proc::set_test_process(Some(Box::new(move |_argv: &[&str]| {
        sequence.borrow_mut().remove(0)
    })));
    // x11vnc `-help` exits 1 on noble, but its options are accepted because the
    // `allowed_returncodes=(0, 1)` window is narrow.
    assert_eq!(
        linux::linux_ready().unwrap().0,
        session_constant(),
        "exit status 1 with valid options must be accepted"
    );

    // The same exit status is rejected for a command that allows only 0.
    proc::set_test_process(Some(Box::new(|_argv: &[&str]| (1, options.to_vec()))));
    assert_eq!(
        proc::run_text(&["/usr/bin/xdpyinfo"], None, &[0])
            .unwrap_err()
            .code(),
        "inspection_failed"
    );

    proc::set_test_process(Some(Box::new(|_argv: &[&str]| (2, options.to_vec()))));
    assert_eq!(
        linux::linux_ready().unwrap_err().code(),
        "inspection_failed"
    );

    proc::set_test_process(Some(Box::new(|_argv: &[&str]| {
        (1, b"not valid help".to_vec())
    })));
    assert_eq!(
        linux::linux_ready().unwrap_err().code(),
        "x11vnc_options_unavailable"
    );
}

#[test]
fn test_linux_requires_supported_x11vnc_and_real_display_access() {
    let _seams = TestSeams::new();
    linux::set_test_linux_session(Some(session_constant()));
    linux::set_test_access(Some(true));

    let calls: RunnerCalls = Rc::new(RefCell::new(Vec::new()));
    let recorder = Rc::clone(&calls);
    let sequence: RunnerOutcomes = Rc::new(RefCell::new(vec![
        Ok(b"-inetd -viewonly -passwdfile -noremote -nocmds".to_vec()),
        Ok(b"display information".to_vec()),
    ]));
    proc::set_test_runner(Some(Box::new(
        move |argv: &[&str], env: Option<&[(&str, &str)]>| {
            recorder.borrow_mut().push((
                argv.iter().map(|value| value.to_string()).collect(),
                env.map(|pairs| {
                    pairs
                        .iter()
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            ));
            sequence.borrow_mut().remove(0)
        },
    )));

    let (session, isolation) = linux::linux_ready().unwrap();
    assert_eq!(session, session_constant());
    assert_eq!(isolation["guest_tcp_listener"], json!(false));
    {
        let calls = calls.borrow();
        let (argv, env) = calls.last().unwrap();
        assert_eq!(
            argv,
            &vec![
                "/usr/bin/xdpyinfo".to_string(),
                "-display".to_string(),
                ":7".to_string()
            ]
        );
        assert_eq!(
            env.iter()
                .find(|(key, _)| key == "XAUTHORITY")
                .map(|(_, value)| value.clone()),
            session_constant()["xauthority"]
                .as_str()
                .map(str::to_string)
        );
    }

    // A display probe that fails aborts readiness.
    let sequence: RunnerOutcomes = Rc::new(RefCell::new(vec![
        Ok(b"-inetd -viewonly -passwdfile -noremote -nocmds".to_vec()),
        Err(Unavailable::new("inspection_failed")),
    ]));
    proc::set_test_runner(Some(Box::new(
        move |_argv: &[&str], _env: Option<&[(&str, &str)]>| sequence.borrow_mut().remove(0),
    )));
    assert!(linux::linux_ready().is_err());

    // Unsupported x11vnc options are rejected.
    proc::set_test_runner(Some(Box::new(
        |_argv: &[&str], _env: Option<&[(&str, &str)]>| Ok(b"-inetd".to_vec()),
    )));
    assert_eq!(
        linux::linux_ready().unwrap_err().code(),
        "x11vnc_options_unavailable"
    );
}

#[test]
fn test_root_and_mismatched_effective_uid_are_rejected() {
    for (uid, euid) in [(0, 0), (501, 0)] {
        assert_eq!(
            proc::identity_for(uid, euid).unwrap_err().code(),
            "console_user_required",
            "uid={uid} euid={euid}"
        );
    }
}

#[test]
fn test_session_is_rechecked_before_creating_stream() {
    let _lock = GLOBAL_LOCK.lock().unwrap();
    let _seams = TestSeams::new();
    let _reset = DeadlineReset;

    config::set_test_config(Some(serve_config(session_constant(), 60.0)));
    let mut changed = session_constant();
    changed["id"] = json!("3");
    serve::set_test_ready(Some(Ok((changed, json!({})))));

    let spawned: Rc<RefCell<Vec<Vec<String>>>> = Rc::new(RefCell::new(Vec::new()));
    let recorder = Rc::clone(&spawned);
    serve::set_test_spawner(Some(Box::new(move |request| {
        recorder.borrow_mut().push(request.argv.clone());
        Ok(99999999)
    })));

    assert_eq!(
        serve::serve(Kind::Linux).unwrap_err().code(),
        "session_changed"
    );
    // `Popen` must not have been called.
    assert!(spawned.borrow().is_empty());
}

#[test]
fn test_private_password_inetd_flags_and_cleanup() {
    let _lock = GLOBAL_LOCK.lock().unwrap();
    let _seams = TestSeams::new();
    let _reset = DeadlineReset;

    config::set_test_config(Some(serve_config(session_constant(), 30.0)));
    serve::set_test_ready(Some(Ok((session_constant(), json!({})))));
    linux::set_test_linux_session(Some(session_constant()));
    serve::set_test_relay(Some(Box::new(|| Err(Unavailable::new("session_changed")))));

    let paths: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
    let captured = Rc::clone(&paths);
    serve::set_test_spawner(Some(Box::new(move |request| {
        assert!(
            !request.argv.join(" ").contains(PASSWORD),
            "the password must not appear in argv"
        );
        assert!(
            !serde_json::to_string(&request.env)
                .unwrap()
                .contains(PASSWORD),
            "the password must not appear in the environment"
        );
        // Python asserts `kwargs["stdin"] is kwargs["stdout"]`: x11vnc's stdin
        // and stdout are the same bidirectional socket, not two pipes.
        let stdin = fstat_raw(request.stdin);
        let stdout = fstat_raw(request.stdout);
        assert_eq!(stdin.st_mode & libc::S_IFMT, libc::S_IFSOCK);
        assert_eq!((stdin.st_dev, stdin.st_ino), (stdout.st_dev, stdout.st_ino));
        assert_eq!(request.stderr, serve::TestStdio::Null);
        for argument in [
            "-inetd",
            "-viewonly",
            "-norc",
            "-noremote",
            "-nocmds",
            "-nosel",
        ] {
            assert!(
                request.argv.iter().any(|value| value == argument),
                "argv is missing {argument}"
            );
        }
        for argument in [
            "-create",
            "-find",
            "-passwd",
            "-storepasswd",
            "-forever",
            "-bg",
        ] {
            assert!(
                !request.argv.iter().any(|value| value == argument),
                "argv must not contain {argument}"
            );
        }
        let index = request
            .argv
            .iter()
            .position(|value| value == "-passwdfile")
            .expect("argv carries -passwdfile");
        let password_path = PathBuf::from(
            request.argv[index + 1]
                .strip_prefix("rm:")
                .expect("the password file is removed on read"),
        );
        assert_eq!(
            std::fs::metadata(&password_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(password_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::read(&password_path).unwrap(),
            format!("{PASSWORD}\n").into_bytes()
        );
        captured.borrow_mut().push(password_path);
        Ok(99999999)
    })));

    assert_eq!(
        serve::serve(Kind::Linux).unwrap_err().code(),
        "session_changed"
    );

    let paths = paths.borrow();
    assert!(!paths.is_empty());
    // `TemporaryDirectory` also removes the password file.
    assert!(!paths[0].exists());
    assert!(!paths[0].parent().unwrap().exists());
    assert_eq!(serve::test_terminations().len(), 1);
}

#[test]
fn test_start_failure_removes_secret() {
    let _lock = GLOBAL_LOCK.lock().unwrap();
    let _seams = TestSeams::new();
    let _reset = DeadlineReset;

    config::set_test_config(Some(serve_config(session_constant(), 30.0)));
    serve::set_test_ready(Some(Ok((session_constant(), json!({})))));
    linux::set_test_linux_session(Some(session_constant()));

    let paths: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
    let captured = Rc::clone(&paths);
    serve::set_test_spawner(Some(Box::new(move |request| {
        let index = request
            .argv
            .iter()
            .position(|value| value == "-passwdfile")
            .expect("argv carries -passwdfile");
        captured.borrow_mut().push(PathBuf::from(
            request.argv[index + 1]
                .strip_prefix("rm:")
                .expect("the password file is removed on read"),
        ));
        Err(())
    })));

    // Python raises `OSError`; the Rust entry point reports the fixed
    // `stream_failed` diagnostic. The secret must be gone either way.
    assert_eq!(
        serve::serve(Kind::Linux).unwrap_err().code(),
        "stream_failed"
    );
    assert!(!paths.borrow()[0].exists());
}
