//! Python-semantics helpers for the guest agent.
//!
//! The agent is a separate binary that cannot depend on `vm-service-core`, so
//! the few CPython behaviours it needs are reproduced here. The expectations in
//! the tests come from CPython 3.14.7.

/// Split text the way Python's `str.splitlines()` does.
///
/// `str::lines()` breaks only on `\n` and drops a trailing `\r`. Python also
/// breaks on `\r`, `\v`, `\f`, U+001C through U+001E, U+0085, U+2028 and
/// U+2029, and treats `\r\n` as a single break. U+001F is not a break. This
/// matters for the PF state check, where a missed boundary can hide the second
/// endpoint of a state and turn a rejection into an acceptance.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < text.len() {
        let character = text[index..]
            .chars()
            .next()
            .expect("index stays on a character boundary");
        let mut width = character.len_utf8();
        let boundary = matches!(
            character,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if boundary {
            if character == '\r' && text[index + width..].starts_with('\n') {
                width += 1;
            }
            lines.push(&text[start..index]);
            index += width;
            start = index;
        } else {
            index += width;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::splitlines;

    /// Every expectation below is CPython 3.14.7's `str.splitlines()`.
    #[test]
    fn splitlines_matches_python() {
        let cases: [(&str, &[&str]); 20] = [
            ("", &[]),
            ("a", &["a"]),
            ("a\n", &["a"]),
            ("a\n\n", &["a", ""]),
            ("\n", &[""]),
            ("a\r\nb", &["a", "b"]),
            ("a\rb", &["a", "b"]),
            ("a\u{b}b", &["a", "b"]),
            ("a\u{c}b", &["a", "b"]),
            ("a\u{1c}b", &["a", "b"]),
            ("a\u{1d}b", &["a", "b"]),
            ("a\u{1e}b", &["a", "b"]),
            // U+001F is not a Python line boundary.
            ("a\u{1f}b", &["a\u{1f}b"]),
            ("a\u{85}b", &["a", "b"]),
            ("a\u{2028}b", &["a", "b"]),
            ("a\u{2029}b", &["a", "b"]),
            ("a\r\n\r\nb", &["a", "", "b"]),
            ("\r", &[""]),
            ("\r\n", &[""]),
            ("a\n\rb", &["a", "", "b"]),
        ];
        for (text, expected) in cases {
            assert_eq!(splitlines(text), expected, "input {text:?}");
        }
    }
}
