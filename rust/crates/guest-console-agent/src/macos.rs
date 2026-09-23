//! macOS inspection, PF policy verification and readiness. Mirrors the
//! `mac_session`, `verify_pf`, `mac_isolation`, `mac_endpoint` and `mac_ready`
//! functions of `bin/guest-console-agent.py`.
//!
//! `plistlib` is replaced by the small XML plist parser below; its only job is
//! to parse the `ioreg -a` document, and every parse or shape failure is mapped
//! to `session_metadata_invalid` exactly as Python's caught
//! `plistlib.InvalidFileException`/`KeyError`/`TypeError` are.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde_json::{Map, Value};

use crate::linux::{is_decimal, sha256_hex};
use crate::proc::{identity, run_bytes, run_text};
use crate::Unavailable;

/// The only Screen Sharing server binary the endpoint check accepts.
pub const MAC_SERVER: &str =
    "/System/Library/CoreServices/RemoteManagement/screensharingd.bundle/Contents/MacOS/screensharingd";

// ---------------------------------------------------------------------------
// XML plist parsing
// ---------------------------------------------------------------------------

/// Parse a `plistlib`-compatible XML plist document.
#[allow(clippy::result_unit_err)]
pub fn parse_plist(input: &[u8]) -> Result<Value, ()> {
    let mut parser = PlistParser {
        bytes: input,
        pos: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_misc()?;
    if parser.pos != input.len() {
        return Err(());
    }
    Ok(value)
}

struct PlistParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl PlistParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn starts_with(&self, needle: &[u8]) -> bool {
        self.bytes[self.pos..].starts_with(needle)
    }

    fn skip_ws(&mut self) {
        while let Some(byte) = self.peek() {
            if byte == b' ' || byte == b'\t' || byte == b'\r' || byte == b'\n' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Skip XML prolog, comments and the doctype.
    fn skip_misc(&mut self) -> Result<(), ()> {
        loop {
            self.skip_ws();
            if self.starts_with(b"<?") {
                self.pos += 2;
                loop {
                    if self.pos >= self.bytes.len() {
                        return Err(());
                    }
                    if self.starts_with(b"?>") {
                        self.pos += 2;
                        break;
                    }
                    self.pos += 1;
                }
            } else if self.starts_with(b"<!--") {
                self.pos += 4;
                loop {
                    if self.pos >= self.bytes.len() {
                        return Err(());
                    }
                    if self.starts_with(b"-->") {
                        self.pos += 3;
                        break;
                    }
                    self.pos += 1;
                }
            } else if self.starts_with(b"<!") {
                self.pos += 2;
                let mut depth: i32 = 0;
                loop {
                    let byte = match self.peek() {
                        Some(byte) => byte,
                        None => return Err(()),
                    };
                    if byte == b'[' {
                        depth += 1;
                    } else if byte == b']' {
                        depth -= 1;
                    } else if byte == b'>' && depth <= 0 {
                        self.pos += 1;
                        break;
                    }
                    self.pos += 1;
                }
            } else {
                return Ok(());
            }
        }
    }

    fn read_name(&mut self) -> Result<String, ()> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b':' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(());
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned())
    }

    /// Consume an opening tag to its `>`, reporting whether it was self-closing.
    fn consume_tag_tail(&mut self) -> Result<bool, ()> {
        let mut last_nonspace = 0u8;
        loop {
            match self.peek() {
                None => return Err(()),
                Some(b'>') => {
                    self.pos += 1;
                    return Ok(last_nonspace == b'/');
                }
                Some(byte) => {
                    self.pos += 1;
                    if !byte.is_ascii_whitespace() {
                        last_nonspace = byte;
                    }
                }
            }
        }
    }

    fn expect_close(&mut self, name: &str) -> Result<(), ()> {
        if !self.starts_with(b"</") {
            return Err(());
        }
        self.pos += 2;
        if self.read_name()? != name {
            return Err(());
        }
        loop {
            match self.peek() {
                None => return Err(()),
                Some(b'>') => {
                    self.pos += 1;
                    return Ok(());
                }
                Some(byte) => {
                    self.pos += 1;
                    if !byte.is_ascii_whitespace() {
                        return Err(());
                    }
                }
            }
        }
    }

    /// Text between the opening tag and `</name>`.
    fn scalar_text(&mut self, name: &str) -> Result<String, ()> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if byte == b'<' {
                break;
            }
            self.pos += 1;
        }
        let text = String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned();
        self.expect_close(name)?;
        Ok(text)
    }

    fn parse_value(&mut self) -> Result<Value, ()> {
        self.skip_misc()?;
        if self.peek() != Some(b'<') {
            return Err(());
        }
        self.pos += 1;
        let name = self.read_name()?;
        let self_closing = self.consume_tag_tail()?;
        match name.as_str() {
            "plist" => {
                if self_closing {
                    return Err(());
                }
                let value = self.parse_value()?;
                self.skip_misc()?;
                self.expect_close("plist")?;
                Ok(value)
            }
            "dict" => self.parse_dict(self_closing),
            "array" => self.parse_array(self_closing),
            "true" => {
                if !self_closing {
                    self.scalar_text("true")?;
                }
                Ok(Value::Bool(true))
            }
            "false" => {
                if !self_closing {
                    self.scalar_text("false")?;
                }
                Ok(Value::Bool(false))
            }
            "string" => {
                let text = if self_closing {
                    String::new()
                } else {
                    self.scalar_text("string")?
                };
                Ok(Value::String(unescape(&text)))
            }
            "integer" => {
                let text = if self_closing {
                    String::new()
                } else {
                    self.scalar_text("integer")?
                };
                parse_integer(&text)
            }
            "real" => {
                let text = if self_closing {
                    String::new()
                } else {
                    self.scalar_text("real")?
                };
                parse_real(&text)
            }
            "date" => {
                let text = if self_closing {
                    String::new()
                } else {
                    self.scalar_text("date")?
                };
                Ok(Value::String(text.trim().to_string()))
            }
            "data" => {
                let text = if self_closing {
                    String::new()
                } else {
                    self.scalar_text("data")?
                };
                Ok(Value::String(text.trim().to_string()))
            }
            _ => Err(()),
        }
    }

    fn parse_dict(&mut self, self_closing: bool) -> Result<Value, ()> {
        let mut map = Map::new();
        if self_closing {
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_misc()?;
            if self.starts_with(b"</dict>") {
                self.pos += "</dict>".len();
                return Ok(Value::Object(map));
            }
            if self.peek() != Some(b'<') {
                return Err(());
            }
            self.pos += 1;
            if self.read_name()? != "key" {
                return Err(());
            }
            if self.consume_tag_tail()? {
                return Err(());
            }
            let start = self.pos;
            while let Some(byte) = self.peek() {
                if byte == b'<' {
                    break;
                }
                self.pos += 1;
            }
            let key = unescape(std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| ())?);
            self.expect_close("key")?;
            let value = self.parse_value()?;
            map.insert(key, value);
        }
    }

    fn parse_array(&mut self, self_closing: bool) -> Result<Value, ()> {
        let mut items = Vec::new();
        if self_closing {
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_misc()?;
            if self.starts_with(b"</array>") {
                self.pos += "</array>".len();
                return Ok(Value::Array(items));
            }
            items.push(self.parse_value()?);
        }
    }
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn parse_integer(text: &str) -> Result<Value, ()> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::from(0));
    }
    if let Ok(value) = trimmed.parse::<i64>() {
        return Ok(Value::from(value));
    }
    if let Ok(value) = trimmed.parse::<u64>() {
        return Ok(Value::from(value));
    }
    Err(())
}

fn parse_real(text: &str) -> Result<Value, ()> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::from(0.0));
    }
    let value: f64 = trimmed.parse().map_err(|_| ())?;
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or(())
}

// ---------------------------------------------------------------------------
// Session and endpoint inspection
// ---------------------------------------------------------------------------

/// Resolve the active console login from `ioreg`.
pub fn mac_session() -> Result<Value, Unavailable> {
    let (uid, user) = identity()?;

    let raw = run_bytes(
        &["/usr/sbin/ioreg", "-a", "-d", "1", "-n", "Root"],
        None,
        &[0],
    )?;
    let roots = parse_plist(&raw).map_err(|_| Unavailable::new("session_metadata_invalid"))?;
    let root = roots
        .get(0)
        .ok_or_else(|| Unavailable::new("session_metadata_invalid"))?;
    let console_users = root
        .get("IOConsoleUsers")
        .ok_or_else(|| Unavailable::new("session_metadata_invalid"))?;
    // Python iterates `IOConsoleUsers` and calls `u.get(...)` on every element.
    // A non-iterable value raises `TypeError`, which the caught tuple maps to
    // `session_metadata_invalid`. An iterable of non-objects (a non-empty
    // dict's string keys, a non-empty string's characters, or a list entry)
    // raises an uncaught `AttributeError`, which is an unexpected failure:
    // `inspection_failed` for `probe`, `stream_failed` for `serve`. An empty
    // iterable yields no active entry and fails `active_console_session_required`.
    let users: Vec<&Value> = match console_users {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) if map.is_empty() => Vec::new(),
        Value::Object(_) => return Err(Unavailable::unexpected()),
        Value::String(text) if text.is_empty() => Vec::new(),
        Value::String(_) => return Err(Unavailable::unexpected()),
        _ => return Err(Unavailable::new("session_metadata_invalid")),
    };
    if users.iter().any(|entry| !entry.is_object()) {
        return Err(Unavailable::unexpected());
    }

    let active: Vec<&Value> = users
        .iter()
        .copied()
        .filter(|entry| entry.get("kCGSSessionOnConsoleKey") == Some(&Value::Bool(true)))
        .collect();
    if active.len() != 1 {
        return Err(Unavailable::new("active_console_session_required"));
    }
    let entry = active[0];

    let session_id = entry
        .get("kCGSSessionIDKey")
        .cloned()
        .ok_or_else(|| Unavailable::new("session_metadata_invalid"))?;
    let login_done = entry.get("kCGSSessionLoginDoneKey") == Some(&Value::Bool(true));
    let user_id_ok = entry.get("kCGSSessionUserIDKey").and_then(Value::as_f64) == Some(uid as f64);
    let user_name_ok =
        entry.get("kCGSSessionUserNameKey").and_then(Value::as_str) == Some(user.as_str());
    let console_uid_ok = match console_uid() {
        Ok(owner) => owner == uid,
        Err(_) => return Err(Unavailable::new("session_metadata_invalid")),
    };
    let sid_ok = match &session_id {
        Value::Number(number) => match number.as_i64() {
            Some(value) => value >= 0,
            None => number.as_u64().is_some(),
        },
        _ => false,
    };
    if !login_done || !user_id_ok || !user_name_ok || !console_uid_ok || !sid_ok {
        return Err(Unavailable::new("session_identity_mismatch"));
    }
    let session_id = match &session_id {
        Value::Number(number) => number.to_string(),
        _ => return Err(Unavailable::new("session_identity_mismatch")),
    };

    let boot = run_text(&["/usr/sbin/sysctl", "-n", "kern.boottime"], None, &[0])?
        .trim()
        .to_string();

    let mut result = Map::new();
    result.insert("id".to_string(), Value::String(session_id));
    result.insert("uid".to_string(), Value::from(uid));
    result.insert("user".to_string(), Value::String(user));
    result.insert(
        "type".to_string(),
        Value::String("macos-console".to_string()),
    );
    result.insert("boot".to_string(), Value::String(boot));
    Ok(Value::Object(result))
}

fn pf_read(args: &[&str]) -> Result<String, Unavailable> {
    let mut argv: Vec<&str> = vec!["/usr/bin/sudo", "-n", "/sbin/pfctl"];
    argv.extend_from_slice(args);
    run_text(&argv, None, &[0])
}

/// The owner of `/dev/console`, real or faked for this thread.
fn console_uid() -> std::io::Result<libc::uid_t> {
    #[cfg(test)]
    if let Some(uid) = crate::proc::test_console_uid() {
        return Ok(uid);
    }
    fs::metadata("/dev/console").map(|metadata| metadata.uid())
}

/// Whether Apple's Remote Management marker exists, real or faked.
fn remote_management_present() -> bool {
    #[cfg(test)]
    if let Some(present) = crate::proc::test_remote_management() {
        return present;
    }
    Path::new("/Library/Application Support/Apple/Remote Desktop/RemoteManagement.launchd").exists()
}

/// The six read-only PF readings `verify_pf` inspects.
pub struct PfReadings<'a> {
    pub status_text: &'a str,
    pub rules_text: &'a str,
    pub interfaces_text: &'a str,
    pub states_text: &'a str,
    pub interface_names: &'a [String],
    pub nat_text: &'a str,
}

/// CPython's `\s` for `str` patterns: Rust's `char::is_whitespace` plus the
/// four ASCII information separators that Python's pattern language treats as
/// whitespace but Rust's `is_whitespace` does not. Measured on this machine
/// against `re.compile(r"\s")` (Python 3.14.7); the two sets differ only here.
fn python_space(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
}

/// Python's `str.strip()` with no arguments: it removes every character for
/// which `str.isspace()` is true, which is `python_space`.
fn python_strip(text: &str) -> &str {
    text.trim_matches(python_space)
}

/// Python's `str.lstrip()` with no arguments.
fn python_lstrip(text: &str) -> &str {
    text.trim_start_matches(python_space)
}

/// Python's `str.split()` with no arguments: split on runs of `str.isspace()`
/// characters and drop the empty fields between them.
fn python_split(text: &str) -> Vec<&str> {
    text.split(python_space)
        .filter(|part| !part.is_empty())
        .collect()
}

/// Python's `\w` for `str` patterns is `str.isalnum() or '_'`. Rust's
/// `char::is_alphanumeric` additionally reports the Unicode `Alphabetic`
/// property's `Other_Alphabetic` marks, which are not in category `L*`.
/// These ranges are exactly that difference, measured exhaustively on this
/// machine against Python 3.14.7; removing them makes the predicate equal
/// `str.isalnum() or '_'` for every code point.
const NON_WORD_ALPHABETIC_RANGES: &[(u32, u32)] = &[
    (0x0345, 0x0345),
    (0x0363, 0x036F),
    (0x0558, 0x0558),
    (0x058B, 0x058C),
    (0x05B0, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C9),
    (0x0610, 0x061A),
    (0x064B, 0x0657),
    (0x0659, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DC),
    (0x06E1, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06ED, 0x06ED),
    (0x0711, 0x0711),
    (0x0730, 0x073F),
    (0x07A6, 0x07B0),
    (0x0816, 0x0817),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082C),
    (0x088F, 0x088F),
    (0x0897, 0x0897),
    (0x08D4, 0x08DF),
    (0x08E3, 0x08E9),
    (0x08F0, 0x0903),
    (0x093A, 0x093B),
    (0x093E, 0x094C),
    (0x094E, 0x094F),
    (0x0955, 0x0957),
    (0x0962, 0x0963),
    (0x0981, 0x0983),
    (0x09BE, 0x09C4),
    (0x09C7, 0x09C8),
    (0x09CB, 0x09CC),
    (0x09D7, 0x09D7),
    (0x09E2, 0x09E3),
    (0x0A01, 0x0A03),
    (0x0A3E, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4C),
    (0x0A51, 0x0A51),
    (0x0A70, 0x0A71),
    (0x0A75, 0x0A75),
    (0x0A81, 0x0A83),
    (0x0ABE, 0x0AC5),
    (0x0AC7, 0x0AC9),
    (0x0ACB, 0x0ACC),
    (0x0AE2, 0x0AE3),
    (0x0AFA, 0x0AFC),
    (0x0B01, 0x0B03),
    (0x0B3E, 0x0B44),
    (0x0B47, 0x0B48),
    (0x0B4B, 0x0B4C),
    (0x0B56, 0x0B57),
    (0x0B62, 0x0B63),
    (0x0B82, 0x0B82),
    (0x0BBE, 0x0BC2),
    (0x0BC6, 0x0BC8),
    (0x0BCA, 0x0BCC),
    (0x0BD7, 0x0BD7),
    (0x0C00, 0x0C04),
    (0x0C3E, 0x0C44),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4C),
    (0x0C55, 0x0C56),
    (0x0C5C, 0x0C5C),
    (0x0C62, 0x0C63),
    (0x0C81, 0x0C83),
    (0x0CBE, 0x0CC4),
    (0x0CC6, 0x0CC8),
    (0x0CCA, 0x0CCC),
    (0x0CD5, 0x0CD6),
    (0x0CDC, 0x0CDC),
    (0x0CE2, 0x0CE3),
    (0x0CF3, 0x0CF3),
    (0x0D00, 0x0D03),
    (0x0D3E, 0x0D44),
    (0x0D46, 0x0D48),
    (0x0D4A, 0x0D4C),
    (0x0D57, 0x0D57),
    (0x0D62, 0x0D63),
    (0x0D81, 0x0D83),
    (0x0DCF, 0x0DD4),
    (0x0DD6, 0x0DD6),
    (0x0DD8, 0x0DDF),
    (0x0DF2, 0x0DF3),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E4D, 0x0E4D),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EB9),
    (0x0EBB, 0x0EBC),
    (0x0ECD, 0x0ECD),
    (0x0F71, 0x0F83),
    (0x0F8D, 0x0F97),
    (0x0F99, 0x0FBC),
    (0x102B, 0x1036),
    (0x1038, 0x1038),
    (0x103B, 0x103E),
    (0x1056, 0x1059),
    (0x105E, 0x1060),
    (0x1062, 0x1064),
    (0x1067, 0x106D),
    (0x1071, 0x1074),
    (0x1082, 0x108D),
    (0x108F, 0x108F),
    (0x109A, 0x109D),
    (0x1712, 0x1713),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B6, 0x17C8),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x192B),
    (0x1930, 0x1938),
    (0x1A17, 0x1A1B),
    (0x1A55, 0x1A5E),
    (0x1A61, 0x1A74),
    (0x1ABF, 0x1AC0),
    (0x1ACC, 0x1ACE),
    (0x1B00, 0x1B04),
    (0x1B35, 0x1B43),
    (0x1B80, 0x1B82),
    (0x1BA1, 0x1BA9),
    (0x1BAC, 0x1BAD),
    (0x1BE7, 0x1BF1),
    (0x1C24, 0x1C36),
    (0x1DD3, 0x1DF4),
    (0x208F, 0x208F),
    (0x209D, 0x209F),
    (0x24B6, 0x24E9),
    (0x2DE0, 0x2DFF),
    (0xA674, 0xA67B),
    (0xA69E, 0xA69F),
    (0xA7CE, 0xA7CF),
    (0xA7D2, 0xA7D2),
    (0xA7D4, 0xA7D4),
    (0xA7DD, 0xA7DD),
    (0xA7E2, 0xA7E2),
    (0xA7F1, 0xA7F1),
    (0xA802, 0xA802),
    (0xA80B, 0xA80B),
    (0xA823, 0xA827),
    (0xA880, 0xA881),
    (0xA8B4, 0xA8C3),
    (0xA8C5, 0xA8C5),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92A),
    (0xA947, 0xA952),
    (0xA980, 0xA983),
    (0xA9B4, 0xA9BF),
    (0xA9E5, 0xA9E5),
    (0xAA29, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4D),
    (0xAA7B, 0xAA7D),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABE),
    (0xAAEB, 0xAAEF),
    (0xAAF5, 0xAAF5),
    (0xAB6C, 0xAB6D),
    (0xABE3, 0xABEA),
    (0xFB1E, 0xFB1E),
    (0x10376, 0x1037A),
    (0x107BB, 0x107BF),
    (0x10940, 0x10959),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10D24, 0x10D27),
    (0x10D69, 0x10D69),
    (0x10EAB, 0x10EAC),
    (0x10EC5, 0x10EC7),
    (0x10ECB, 0x10ECD),
    (0x10ED9, 0x10EEE),
    (0x10EF3, 0x10EF3),
    (0x10EF5, 0x10EF5),
    (0x10EF7, 0x10EF8),
    (0x10EFA, 0x10EFC),
    (0x11000, 0x11002),
    (0x11038, 0x11045),
    (0x11073, 0x11074),
    (0x11080, 0x11082),
    (0x110B0, 0x110B8),
    (0x110C2, 0x110C2),
    (0x11100, 0x11102),
    (0x11127, 0x11132),
    (0x11145, 0x11146),
    (0x11180, 0x11182),
    (0x111B3, 0x111BF),
    (0x111CE, 0x111CF),
    (0x1122C, 0x11234),
    (0x11237, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112E8),
    (0x11300, 0x11303),
    (0x1133E, 0x11344),
    (0x11347, 0x11348),
    (0x1134B, 0x1134C),
    (0x11357, 0x11357),
    (0x11362, 0x11363),
    (0x113B8, 0x113C0),
    (0x113C2, 0x113C2),
    (0x113C5, 0x113C5),
    (0x113C7, 0x113CA),
    (0x113CC, 0x113CD),
    (0x11435, 0x11441),
    (0x11443, 0x11445),
    (0x114B0, 0x114C1),
    (0x115AF, 0x115B5),
    (0x115B8, 0x115BE),
    (0x115DC, 0x115DD),
    (0x11630, 0x1163E),
    (0x11640, 0x11640),
    (0x116AB, 0x116B5),
    (0x1171D, 0x1172A),
    (0x1182C, 0x11838),
    (0x11930, 0x11935),
    (0x11937, 0x11938),
    (0x1193B, 0x1193C),
    (0x11940, 0x11940),
    (0x11942, 0x11942),
    (0x119D1, 0x119D7),
    (0x119DA, 0x119DF),
    (0x119E4, 0x119E4),
    (0x11A01, 0x11A0A),
    (0x11A35, 0x11A39),
    (0x11A3B, 0x11A3E),
    (0x11A51, 0x11A5B),
    (0x11A8A, 0x11A97),
    (0x11B0A, 0x11B0A),
    (0x11B60, 0x11B67),
    (0x11C2F, 0x11C36),
    (0x11C38, 0x11C3E),
    (0x11C92, 0x11CA7),
    (0x11CA9, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D41),
    (0x11D43, 0x11D43),
    (0x11D47, 0x11D47),
    (0x11D8A, 0x11D8E),
    (0x11D90, 0x11D91),
    (0x11D93, 0x11D96),
    (0x11DB0, 0x11DDB),
    (0x11DE0, 0x11DE9),
    (0x11DF0, 0x11DF1),
    (0x11EF3, 0x11EF6),
    (0x11F00, 0x11F01),
    (0x11F03, 0x11F03),
    (0x11F34, 0x11F3A),
    (0x11F3E, 0x11F40),
    (0x1246F, 0x1246F),
    (0x12475, 0x1247F),
    (0x12550, 0x12686),
    (0x1611E, 0x1612E),
    (0x16EA0, 0x16EB8),
    (0x16EBB, 0x16ED3),
    (0x16F4F, 0x16F4F),
    (0x16F51, 0x16F87),
    (0x16F8F, 0x16F92),
    (0x16FF0, 0x16FF6),
    (0x187F8, 0x187FF),
    (0x18CD6, 0x18CDA),
    (0x18D09, 0x18D20),
    (0x18D80, 0x18DF2),
    (0x18E00, 0x19191),
    (0x191A0, 0x191D2),
    (0x1B123, 0x1B128),
    (0x1B168, 0x1B168),
    (0x1BC9E, 0x1BC9E),
    (0x1D6A6, 0x1D6A6),
    (0x1DF1F, 0x1DF24),
    (0x1DF2B, 0x1DF81),
    (0x1DF90, 0x1DF96),
    (0x1DFCD, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E08F, 0x1E08F),
    (0x1E6C0, 0x1E6DE),
    (0x1E6E0, 0x1E6F5),
    (0x1E6FE, 0x1E6FF),
    (0x1E947, 0x1E947),
    (0x1F130, 0x1F149),
    (0x1F150, 0x1F169),
    (0x1F170, 0x1F189),
    (0x2B73A, 0x2B73F),
    (0x2B81E, 0x2B81E),
    (0x2CEA2, 0x2CEAD),
    (0x323B0, 0x33479),
    (0x3D000, 0x3FC3F),
];

fn python_word(character: char) -> bool {
    if character == '_' {
        return true;
    }
    if !character.is_alphanumeric() {
        return false;
    }
    let code = character as u32;
    NON_WORD_ALPHABETIC_RANGES
        .binary_search_by(|&(start, end)| {
            if code < start {
                std::cmp::Ordering::Greater
            } else if code > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_err()
}

/// Port of Python's `re.search(r"^Status:\s+Enabled\b", status_text, re.M)`.
///
/// `\s+` may cross newlines, and `^` (because of `re.M`) matches at the string
/// start and after every `\n`, not after a lone `\r`.
fn status_enabled(text: &str) -> bool {
    const MARKER: &str = "Status:";
    let mut from = 0;
    while let Some(offset) = text[from..].find(MARKER) {
        let start = from + offset;
        from = start + MARKER.len();
        let at_line_start = start == 0 || text.as_bytes()[start - 1] == b'\n';
        if !at_line_start {
            continue;
        }
        let rest = &text[start + MARKER.len()..];
        // `\s+` is greedy; a shorter match would have to start with a
        // whitespace character where the literal `Enabled` must begin, so
        // consuming the whole run is equivalent.
        let spaces = rest
            .char_indices()
            .find(|(_, character)| !python_space(*character))
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        if spaces == 0 {
            continue;
        }
        if let Some(after) = rest[spaces..].strip_prefix("Enabled") {
            if after
                .chars()
                .next()
                .map(|character| !python_word(character))
                .unwrap_or(true)
            {
                return true;
            }
        }
    }
    false
}

/// Recognize only the documented top-level PF policy; unknown forms fail closed.
pub fn verify_pf(readings: PfReadings) -> Result<Value, Unavailable> {
    // rdr pass bypasses filter evaluation, even with no pre-existing state.
    if !python_strip(readings.nat_text).is_empty() {
        return Err(Unavailable::new("pf_translation_not_supported"));
    }
    if !status_enabled(readings.status_text) {
        return Err(Unavailable::new("pf_disabled"));
    }

    let rules: Vec<&str> = crate::python::splitlines(readings.rules_text)
        .into_iter()
        .map(python_strip)
        .filter(|line| !line.is_empty())
        .collect();
    let both = "block drop in quick on ! lo0 proto tcp from any to any port = 5900";
    let inet = "block drop in quick on ! lo0 inet proto tcp from any to any port = 5900";
    let inet6 = "block drop in quick on ! lo0 inet6 proto tcp from any to any port = 5900";
    let rules_match = rules.first().map(|rule| *rule == both).unwrap_or(false)
        || (rules.len() >= 2 && {
            let families: HashSet<&str> = rules[..2].iter().copied().collect();
            families.len() == 2 && families.contains(inet) && families.contains(inet6)
        });
    if !rules_match {
        return Err(Unavailable::new("pf_rules_unverified"));
    }

    let mut seen: HashSet<String> = HashSet::new();
    for line in crate::python::splitlines(readings.interfaces_text) {
        let starts_indented = line.chars().next().map(python_space).unwrap_or(false);
        if python_strip(line).is_empty() || starts_indented {
            continue;
        }
        let (name, flags) = match parse_interface_line(python_strip(line)) {
            Some(parsed) => parsed,
            None => return Err(Unavailable::new("pf_interfaces_unverified")),
        };
        seen.insert(name.clone());
        if let Some(flags) = flags {
            if !flags.is_empty() && (name != "lo0" || python_strip(&flags) != "skip") {
                return Err(Unavailable::new("pf_interface_bypass"));
            }
        }
    }
    if readings.interface_names.is_empty()
        || !readings
            .interface_names
            .iter()
            .all(|name| seen.contains(name))
        || !seen.contains("lo0")
    {
        return Err(Unavailable::new("pf_interfaces_unverified"));
    }

    // Existing PF states bypass newly added quick rules. Reject every state
    // mentioning 5900 unless all of its endpoints are numeric loopback IPs.
    for line in crate::python::splitlines(readings.states_text) {
        if !contains_port_5900(line) {
            continue;
        }
        let mut endpoints: Vec<String> = Vec::new();
        for word in python_split(line) {
            let word = word.trim_matches(|character| character == '(' || character == ')');
            if let Some(endpoint) = match_endpoint_colon(word) {
                endpoints.push(endpoint);
            } else if let Some(endpoint) = match_endpoint_bracket(word) {
                endpoints.push(endpoint);
            }
        }
        if endpoints.len() < 2 {
            return Err(Unavailable::new("pf_existing_connection"));
        }
        for endpoint in &endpoints {
            match is_loopback(endpoint) {
                Ok(true) => {}
                Ok(false) => return Err(Unavailable::new("pf_existing_connection")),
                Err(()) => return Err(Unavailable::new("pf_states_unverified")),
            }
        }
    }

    let mut result = Map::new();
    result.insert(
        "mechanism".to_string(),
        Value::String("pf-top-level-quick-no-translation-v2".to_string()),
    );
    result.insert("policy_checks_passed".to_string(), Value::Bool(true));
    result.insert(
        "rules_sha256".to_string(),
        Value::String(sha256_hex(readings.rules_text.as_bytes())),
    );
    result.insert(
        "external_acceptance".to_string(),
        Value::String("unverified".to_string()),
    );
    Ok(Value::Object(result))
}

fn parse_interface_line(line: &str) -> Option<(String, Option<String>)> {
    let name_end = line
        .find(|character: char| {
            !(character.is_ascii_alphanumeric()
                || character == '_'
                || character == '.'
                || character == ':'
                || character == '-')
        })
        .unwrap_or(line.len());
    if name_end == 0 {
        return None;
    }
    let name = &line[..name_end];
    let rest = &line[name_end..];
    if rest.is_empty() {
        return Some((name.to_string(), None));
    }
    let trimmed = python_lstrip(rest);
    if trimmed.len() == rest.len() {
        return None;
    }
    let inner = trimmed.strip_prefix('(')?.strip_suffix(')')?;
    if inner.contains('(') || inner.contains(')') {
        return None;
    }
    Some((name.to_string(), Some(inner.to_string())))
}

/// Port of Python's `re.search(r"[:\[]5900\b", line)`.
///
/// The trailing `\b` is a CPython word boundary, so the character after `5900`
/// must not be a `\w` character. An ASCII-only test would match `x:5900é`,
/// which CPython does not match, and would therefore reject a state line the
/// Python adapter ignores.
fn contains_port_5900(line: &str) -> bool {
    const NEEDLE: &str = "5900";
    let mut from = 0;
    while let Some(offset) = line[from..].find(NEEDLE) {
        let start = from + offset;
        // Resume one byte past the start so overlapping candidates are still
        // examined, as the regex engine's scan would.
        from = start + 1;
        if !matches!(line[..start].chars().next_back(), Some(':') | Some('[')) {
            continue;
        }
        let after = line[start + NEEDLE.len()..].chars().next();
        if after
            .map(|character| !python_word(character))
            .unwrap_or(true)
        {
            return true;
        }
    }
    false
}

fn match_endpoint_colon(word: &str) -> Option<String> {
    if let Some(inner_start) = word.strip_prefix('[') {
        if let Some((inside, port)) = inner_start.rsplit_once("]:") {
            if !inside.is_empty()
                && !inside.contains(']')
                && !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Some(inside.to_string());
            }
        }
    }
    if let Some((address, port)) = word.rsplit_once(':') {
        if !address.is_empty()
            && !address.contains(' ')
            && !port.is_empty()
            && port.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Some(address.to_string());
        }
    }
    None
}

fn match_endpoint_bracket(word: &str) -> Option<String> {
    let without_close = word.strip_suffix(']')?;
    let (address, port) = without_close.rsplit_once('[')?;
    if !address.is_empty()
        && !address.contains(' ')
        && !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
    {
        Some(address.to_string())
    } else {
        None
    }
}

fn is_loopback(address: &str) -> Result<bool, ()> {
    let bare = address
        .split_once('%')
        .map(|(head, _)| head)
        .unwrap_or(address);
    // Mirrors CPython `ipaddress.IPv6Address.is_loopback`
    // (`bin/guest-console-agent.py:244`): `self._ip == 1 or
    // (self.ipv4_mapped is not None and self.ipv4_mapped.is_loopback)`. The
    // embedded-IPv4 case is `::ffff:a.b.c.d` only, which is exactly what
    // `Ipv6Addr::to_ipv4_mapped` recognizes.
    match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => Ok(address.is_loopback()),
        Ok(std::net::IpAddr::V6(address)) => Ok(address.is_loopback()
            || address
                .to_ipv4_mapped()
                .is_some_and(|mapped| mapped.is_loopback())),
        Err(_) => Err(()),
    }
}

/// Read and verify the live PF policy.
pub fn mac_isolation() -> Result<Value, Unavailable> {
    let status_text = pf_read(&["-s", "info"])?;
    let rules_text = pf_read(&["-s", "rules"])?;
    let interfaces_text = pf_read(&["-v", "-s", "Interfaces"])?;
    let states_text = pf_read(&["-s", "states"])?;
    let interface_names: Vec<String> = run_text(&["/sbin/ifconfig", "-l"], None, &[0])?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let nat_text = pf_read(&["-s", "nat"])?;
    verify_pf(PfReadings {
        status_text: &status_text,
        rules_text: &rules_text,
        interfaces_text: &interfaces_text,
        states_text: &states_text,
        interface_names: &interface_names,
        nat_text: &nat_text,
    })
}

/// Confirm the loopback Screen Sharing endpoint is the Apple daemon.
pub fn mac_endpoint() -> Result<(), Unavailable> {
    // Apple's ARDAgent launch condition is a reason to reject Remote
    // Management, not a marker that can establish Screen Sharing readiness.
    if remote_management_present() {
        return Err(Unavailable::new("remote_management_not_supported"));
    }

    let processes = run_text(&["/bin/ps", "-axo", "comm="], None, &[0])?;
    let ard = "/System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/MacOS/ARDAgent";
    if crate::python::splitlines(&processes)
        .into_iter()
        .any(|line| python_strip(line) == ard)
    {
        return Err(Unavailable::new("remote_management_not_supported"));
    }

    let job = run_text(
        &["/bin/launchctl", "print", "system/com.apple.screensharing"],
        None,
        &[0],
    )?;
    let program_line = format!("program = {MAC_SERVER}");
    // Python matches `^\s*program = <path>\s*$` with `re.MULTILINE`, whose
    // anchors only recognise `\n`, so `lines()` is the correct splitter here;
    // the whitespace either side is still Python's `\s`.
    if !job.lines().any(|line| python_strip(line) == program_line) {
        return Err(Unavailable::new("screensharing_service_unverified"));
    }

    let listing = run_text(
        &[
            "/usr/bin/sudo",
            "-n",
            "/usr/sbin/lsof",
            "-nP",
            "-iTCP:5900",
            "-sTCP:LISTEN",
            "-Fpcun",
        ],
        None,
        &[0],
    )?;
    let mut owners: Vec<HashMap<String, String>> = Vec::new();
    let mut owner: HashMap<String, String> = HashMap::new();
    for line in crate::python::splitlines(&listing) {
        if let Some(rest) = line.strip_prefix('p') {
            owner = HashMap::new();
            owner.insert("pid".to_string(), rest.to_string());
        } else if let Some(rest) = line.strip_prefix('c') {
            owner.insert("command".to_string(), rest.to_string());
        } else if let Some(rest) = line.strip_prefix('u') {
            owner.insert("uid".to_string(), rest.to_string());
        } else if let Some(rest) = line.strip_prefix('n') {
            let mut record = owner.clone();
            record.insert("address".to_string(), rest.to_string());
            owners.push(record);
        }
    }
    if owners.is_empty() {
        return Err(Unavailable::new("screensharing_listener_unavailable"));
    }
    for owner in &owners {
        let uid = owner.get("uid").map(String::as_str).unwrap_or("");
        let address = owner.get("address").map(String::as_str).unwrap_or("");
        if uid != "0" || !address.ends_with(":5900") {
            return Err(Unavailable::new("screensharing_listener_unverified"));
        }
        let pid = owner.get("pid").map(String::as_str).unwrap_or("");
        let command = owner.get("command").map(String::as_str).unwrap_or("");
        if pid == "1" && command == "launchd" {
            continue;
        }
        if !is_decimal(pid) || command != "screensharingd" {
            return Err(Unavailable::new("screensharing_listener_unverified"));
        }
        let comm = run_text(&["/bin/ps", "-p", pid, "-o", "comm="], None, &[0])?;
        if comm.trim() != MAC_SERVER {
            return Err(Unavailable::new("screensharing_listener_unverified"));
        }
    }
    Ok(())
}

/// Full macOS readiness: session, then isolation, then endpoint.
pub fn mac_ready() -> Result<(Value, Value), Unavailable> {
    let session = mac_session()?;
    let isolation = mac_isolation()?;
    mac_endpoint()?;
    Ok((session, isolation))
}
