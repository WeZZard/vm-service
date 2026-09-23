//! Python-compatible JSON number handling.
//!
//! The workspace enables `serde_json`'s `arbitrary_precision` feature so that an
//! integer wider than `u64` keeps its digits, which the Python contract
//! requires. The side effect is that a *float* literal keeps its original text
//! too, and CPython normalizes floats when it loads JSON: it parses them to
//! `float` and writes the value back with `repr`. So `0.10` becomes `0.1`,
//! `1e2` becomes `100.0`, and `1e309` becomes `Infinity`.
//!
//! `normalize_numbers` restores that behaviour at every boundary where parsed
//! JSON can be stored, hashed, or echoed back to a client.

use serde_json::Value;

/// Render a float the way CPython's `repr` does.
///
/// CPython prints the shortest round-tripping digits and switches to exponent
/// form when the decimal point sits more than sixteen digits to the right of
/// the first digit, or four or more places to its left.
pub fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    // Rust's `LowerExp` prints the shortest round-tripping digits as
    // `d[.ddd]e[-]dd`, which yields both the digits and the decimal exponent.
    let scientific = format!("{value:e}");
    let (negative, rest) = match scientific.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, scientific.as_str()),
    };
    let (mantissa, exponent) = rest
        .split_once('e')
        .expect("`{:e}` always emits an exponent");
    let exponent: i32 = exponent.parse().expect("the exponent is an integer");
    let digits: Vec<char> = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    let decimal_point = exponent + 1;

    let text: String = digits.iter().collect();
    let body = if decimal_point <= -4 || decimal_point > 16 {
        let mantissa = if digits.len() > 1 {
            format!("{}.{}", digits[0], digits[1..].iter().collect::<String>())
        } else {
            digits[0].to_string()
        };
        let exponent = decimal_point - 1;
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    } else if decimal_point <= 0 {
        format!("0.{}{text}", "0".repeat((-decimal_point) as usize))
    } else if decimal_point as usize >= digits.len() {
        format!(
            "{text}{}.0",
            "0".repeat(decimal_point as usize - digits.len())
        )
    } else {
        let split = decimal_point as usize;
        format!(
            "{}.{}",
            digits[..split].iter().collect::<String>(),
            digits[split..].iter().collect::<String>()
        )
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// Rewrite every number in a parsed document to CPython's value for the same
/// literal.
///
/// An integer keeps its exact digits; a float literal becomes CPython's
/// `repr` of the parsed value.
pub fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let text = number.to_string();
            if !text.contains(['.', 'e', 'E']) {
                // Python's `json` produced an `int`, and `arbitrary_precision`
                // already preserved its digits exactly.
                return;
            }
            let Ok(parsed) = text.parse::<f64>() else {
                return;
            };
            *number = serde_json::Number::from_string_unchecked(python_float_repr(parsed));
        }
        Value::Array(items) => {
            for item in items {
                normalize_numbers(item);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                normalize_numbers(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_numbers, python_float_repr};
    use serde_json::{json, Value};

    /// Every expectation is `json.dumps(json.loads(literal))` on CPython 3.14.7.
    #[test]
    fn float_repr_matches_python() {
        let cases: [(&str, &str); 18] = [
            ("0.1", "0.1"),
            ("0.10", "0.1"),
            ("1e2", "100.0"),
            ("1E2", "100.0"),
            ("100.0", "100.0"),
            ("1e16", "1e+16"),
            ("1e17", "1e+17"),
            ("1e15", "1000000000000000.0"),
            ("1e-4", "0.0001"),
            ("1e-5", "1e-05"),
            ("0.0001", "0.0001"),
            ("1.5", "1.5"),
            ("2.0", "2.0"),
            ("7.0", "7.0"),
            ("-0.0", "-0.0"),
            ("1.7976931348623157e308", "1.7976931348623157e+308"),
            ("5e-324", "5e-324"),
            ("123456789012345678901234567890.5", "1.2345678901234568e+29"),
        ];
        for (literal, expected) in cases {
            let parsed: f64 = literal.parse().expect("literal parses");
            assert_eq!(python_float_repr(parsed), expected, "literal {literal}");
        }
    }

    #[test]
    fn infinity_matches_python() {
        let overflowing: f64 = "1e309".parse().expect("literal parses");
        assert_eq!(python_float_repr(overflowing), "Infinity");
        assert_eq!(python_float_repr(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(python_float_repr(f64::NAN), "NaN");
    }

    /// `json.dumps(json.loads(text))` for the same input, on CPython 3.14.7.
    #[test]
    fn normalize_matches_python_round_trip() {
        let mut value: Value =
            serde_json::from_str(r#"{"a": 0.10, "b": 1e2, "c": 1, "d": [1.500, "1.5"]}"#)
                .expect("document parses");
        normalize_numbers(&mut value);
        assert_eq!(
            serde_json::to_string(&value).expect("serialize"),
            r#"{"a":0.1,"b":100.0,"c":1,"d":[1.5,"1.5"]}"#
        );
    }

    /// Integer literals wider than `u64` keep their exact digits, because
    /// Python's `json` parses them as an arbitrary-precision `int`.
    #[test]
    fn wide_integers_are_left_exact() {
        let mut value: Value = serde_json::from_str(r#"{"big": 79228162514264337593543950336}"#)
            .expect("document parses");
        normalize_numbers(&mut value);
        assert_eq!(
            value,
            json!({"big": serde_json::from_str::<Value>("79228162514264337593543950336").expect("wide")})
        );
    }
}
