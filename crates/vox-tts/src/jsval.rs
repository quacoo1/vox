//! Private JS-semantics helpers over untrusted JSON and header strings (tts.md §2.5 `str`/`num`, §8 `Number()` and
//! `String()`).

use serde_json::Value;
use vox_core::js::{js_trim, number_to_string};

/// `str(x)`: a non-empty JSON string.
pub(crate) fn str_of(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// `num(x)`: a finite JSON number (strings are not coerced).
pub(crate) fn num_of(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

/// JS `String(value)` for a JSON value: strings as is, numbers JS-style, `null` → `"null"`, booleans, arrays joined
/// with `,` (`null` elements → `""`), objects → `"[object Object]"`.
pub(crate) fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(x) => number_to_string(x),
            None => n.to_string(),
        },
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JS `Number(string)`: trims JS whitespace, `""` → 0, `0x`/`0o`/`0b` prefixes (unsigned), `[+-]Infinity`, decimal
/// literals with an optional exponent; anything else → NaN.
pub(crate) fn js_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = match t.get(..2) {
        Some("0x") | Some("0X") => 16,
        Some("0o") | Some("0O") => 8,
        Some("0b") | Some("0B") => 2,
        _ => 10,
    };
    if radix != 10 {
        let digits = &t[2..];
        if digits.is_empty() {
            return f64::NAN;
        }
        let mut acc = 0.0f64;
        for c in digits.chars() {
            match c.to_digit(radix) {
                Some(d) => acc = acc * f64::from(radix) + f64::from(d),
                None => return f64::NAN,
            }
        }
        return acc;
    }
    if !is_decimal_literal(t) {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `[+-]? (digits [. digits?] | . digits) ([eE] [+-]? digits)?`
fn is_decimal_literal(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+') | Some(b'-')) {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - frac_start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

/// A JSON number from an `f64` the way `JSON.stringify` prints it: integral values without a fraction.
pub(crate) fn js_num(x: f64) -> Value {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    if x.is_finite() && x.fract() == 0.0 && x.abs() <= MAX_SAFE {
        // Exact: |x| ≤ 2^53 − 1 and integral.
        Value::from(x as i64)
    } else {
        serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn number_follows_js_rules() {
        assert_eq!(js_number(""), 0.0);
        assert_eq!(js_number("  "), 0.0);
        assert_eq!(js_number(" 0x10 "), 16.0);
        assert_eq!(js_number("0X1f"), 31.0);
        assert_eq!(js_number("0b11"), 3.0);
        assert_eq!(js_number("0o17"), 15.0);
        assert!(js_number("-0x10").is_nan());
        assert!(js_number("0x").is_nan());
        assert!(js_number("0xg").is_nan());
        assert_eq!(js_number(" 1e3 "), 1000.0);
        assert_eq!(js_number("1.5"), 1.5);
        assert_eq!(js_number(".5"), 0.5);
        assert_eq!(js_number("5."), 5.0);
        assert_eq!(js_number("+2"), 2.0);
        assert_eq!(js_number("\u{FEFF}\u{00A0}7\u{3000}"), 7.0);
        assert_eq!(js_number("Infinity"), f64::INFINITY);
        assert_eq!(js_number("-Infinity"), f64::NEG_INFINITY);
        for bad in ["3, 3", "inf", "nan", "NaN", "infinity", "1e", "e5", ".", "1_000", "Wed, 21 Oct 2015 07:28:00 GMT", "12px"] {
            assert!(js_number(bad).is_nan(), "{bad}");
        }
    }

    #[test]
    fn string_follows_js_rules() {
        assert_eq!(js_string(&json!("a")), "a");
        assert_eq!(js_string(&json!(1)), "1");
        assert_eq!(js_string(&json!(1.5)), "1.5");
        assert_eq!(js_string(&json!(null)), "null");
        assert_eq!(js_string(&json!(true)), "true");
        assert_eq!(js_string(&json!([1, null, "x", [2, 3]])), "1,,x,2,3");
        assert_eq!(js_string(&json!({"a": 1})), "[object Object]");
    }

    #[test]
    fn str_and_num_are_strict() {
        let v = json!({"s": "x", "e": "", "n": 3, "ns": "3", "f": 1.25});
        assert_eq!(str_of(v.get("s")), Some("x"));
        assert_eq!(str_of(v.get("e")), None);
        assert_eq!(str_of(v.get("n")), None);
        assert_eq!(num_of(v.get("n")), Some(3.0));
        assert_eq!(num_of(v.get("ns")), None);
        assert_eq!(num_of(v.get("f")), Some(1.25));
        assert_eq!(num_of(v.get("missing")), None);
    }

    #[test]
    fn js_num_prints_integers_without_fraction() {
        assert_eq!(serde_json::to_string(&js_num(1.0)).unwrap(), "1");
        assert_eq!(serde_json::to_string(&js_num(0.0)).unwrap(), "0");
        assert_eq!(serde_json::to_string(&js_num(1.2)).unwrap(), "1.2");
        assert_eq!(serde_json::to_string(&js_num(0.75)).unwrap(), "0.75");
        assert_eq!(serde_json::to_string(&js_num(f64::NAN)).unwrap(), "null");
    }
}
