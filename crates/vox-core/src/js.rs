//! JavaScript-compatible primitives the specs depend on (contracts.md §3 conventions, tts.md §8).
//!
//! - [`js_round`] is `Math.round` (halves toward +∞), not Rust's `f64::round` (halves away from zero).
//! - [`is_js_whitespace`] is the ECMAScript `\s` set (includes U+FEFF, excludes U+0085).
//! - [`to_fixed`] is `Number.prototype.toFixed` (round half up on the exact binary value).
//! - [`number_to_string`] is `String(number)` for finite values in the ranges Vox prints.

/// `Math.round(x)`: rounds half toward +∞ (`-2.5 → -2`, `2.5 → 3`). NaN stays NaN.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let r = x.floor();
    if x - r >= 0.5 {
        r + 1.0
    } else {
        r
    }
}

/// `Math.round(x * 10) / 10`.
pub fn round1(x: f64) -> f64 {
    js_round(x * 10.0) / 10.0
}

/// `Math.round(x * 1000) / 1000`.
pub fn round3(x: f64) -> f64 {
    js_round(x * 1000.0) / 1000.0
}

/// `Math.min(max, Math.max(min, x))` with NaN → `min` (reader-playback.md §0 `clampNum`).
pub fn clamp_num(x: f64, min: f64, max: f64) -> f64 {
    if x.is_nan() {
        return min;
    }
    x.max(min).min(max)
}

/// True for the ECMAScript whitespace + line-terminator set matched by `\s` and stripped by `trim()`.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}'
            | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `String.prototype.trimEnd`.
pub fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

/// `String.prototype.trimStart`.
pub fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

/// `s.replace(/\s+/g, ' ').trim()`: every whitespace run becomes one space, ends trimmed.
pub fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.chars() {
        if is_js_whitespace(c) {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// True if the string contains any JS whitespace (`/\s/.test(s)`).
pub fn has_js_whitespace(s: &str) -> bool {
    s.chars().any(is_js_whitespace)
}

/// `Number.prototype.toFixed(digits)` for finite `x` (digits ≤ 20). Ties round up on the exact binary value,
/// e.g. `to_fixed(0.125, 2) == "0.13"`, `to_fixed(1.005, 2) == "1.00"` (1.005 is below the tie in binary).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() {
        return if x.is_nan() { "NaN".into() } else if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let neg = x < 0.0;
    // Rust prints the exact decimal expansion for large precisions; 40 extra digits cover every f64 tie decision.
    let exact = format!("{:.*}", digits + 40, x.abs());
    let (int_part, frac_part) = exact.split_once('.').unwrap_or((exact.as_str(), ""));
    let mut digits_vec: Vec<u8> = int_part.bytes().chain(frac_part.bytes().take(digits)).map(|b| b - b'0').collect();
    let next = frac_part.as_bytes().get(digits).map(|b| b - b'0').unwrap_or(0);
    if next >= 5 {
        // Round half up (any remainder ≥ 0.5 of the last kept digit).
        let mut i = digits_vec.len();
        loop {
            if i == 0 {
                digits_vec.insert(0, 1);
                break;
            }
            i -= 1;
            if digits_vec[i] == 9 {
                digits_vec[i] = 0;
            } else {
                digits_vec[i] += 1;
                break;
            }
        }
    }
    let int_len = digits_vec.len() - digits;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    for d in &digits_vec[..int_len] {
        out.push((b'0' + d) as char);
    }
    if digits > 0 {
        out.push('.');
        for d in &digits_vec[int_len..] {
            out.push((b'0' + d) as char);
        }
    }
    out
}

/// `String(x)` for a finite number: shortest round-trip form, integral values without a fraction, `-0` → `"0"`.
/// (Exponent forms for |x| ≥ 1e21 or < 1e-6 are not reproduced; Vox never prints such values.)
pub fn number_to_string(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return to_fixed(x, 0);
    }
    format!("{x}")
}

/// `Number(x.toFixed(digits))` rendered with [`number_to_string`]: `1.25 → "1.25"`, `1.0 → "1"`.
pub fn fixed_trimmed(x: f64, digits: usize) -> String {
    let fixed = to_fixed(x, digits);
    let parsed: f64 = fixed.parse().unwrap_or(0.0);
    number_to_string(parsed)
}

/// Upper-case the first character, leave the rest (`'abc' → 'Abc'`).
pub fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_round_matches_js() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(-100.5), -100.0);
        assert_eq!(js_round(-1.5), -1.0);
        assert_eq!(js_round(0.49999999999999994), 0.0);
        assert_eq!(js_round(-0.4), 0.0);
        assert_eq!(round1(50.06), 50.1);
        assert_eq!(round1(0.04), 0.0);
        assert_eq!(round3(0.2), 0.2);
    }

    #[test]
    fn whitespace_set() {
        assert!(is_js_whitespace('\u{FEFF}'));
        assert!(!is_js_whitespace('\u{0085}'));
        assert_eq!(js_trim("\u{FEFF} a b \u{3000}"), "a b");
        assert_eq!(collapse_whitespace("  a \n\t b  "), "a b");
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn to_fixed_matches_js() {
        assert_eq!(to_fixed(1.0, 2), "1.00");
        assert_eq!(to_fixed(0.125, 2), "0.13");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(9.995, 2), "9.99");
        assert_eq!(to_fixed(9.996, 2), "10.00");
        assert_eq!(to_fixed(1.6, 1), "1.6");
        assert_eq!(to_fixed(-1.25, 1), "-1.3");
        assert_eq!(to_fixed(-0.001, 1), "-0.0");
        assert_eq!(to_fixed(187.4, 0), "187");
    }

    #[test]
    fn number_strings() {
        assert_eq!(number_to_string(1.0), "1");
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(1.25), "1.25");
        assert_eq!(number_to_string(1792229746902.0), "1792229746902");
        assert_eq!(fixed_trimmed(1.0, 2), "1");
        assert_eq!(fixed_trimmed(1.25, 2), "1.25");
        assert_eq!(fixed_trimmed(0.7499999, 2), "0.75");
        assert_eq!(capitalize_first("creator"), "Creator");
    }
}
