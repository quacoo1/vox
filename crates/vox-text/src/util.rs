//! Private helpers shared by the text modules: UTF-16 buffers, the JS whitespace set as code units, Unicode general
//! category tests, and the JS whitespace class spelled for the `regex` crate.

use std::sync::LazyLock;

use regex::Regex;

/// The ECMAScript `\s` set (also `trim()`'s set and text.ts' `isSpace`), as the body of a `regex` character class.
/// Rust's `\s` is Unicode `White_Space`, which adds U+0085 and drops U+FEFF, so patterns spell the JS set out.
pub(crate) const WS: &str =
    r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// `text` as UTF-16 code units (JS string indices).
pub(crate) fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// `String.fromCharCode(...)` of a unit range, lone surrogates (which a Rust `String` cannot hold) → U+FFFD.
pub(crate) fn string_of(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// text.ts `isSpace(code)` = the JS whitespace set tested on one UTF-16 code unit (a surrogate is never a space).
pub(crate) fn is_space_unit(u: u16) -> bool {
    matches!(
        u,
        9..=13 | 32 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

/// A UTF-16 high (leading) surrogate.
pub(crate) fn is_high_surrogate(u: u16) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}

/// A UTF-16 low (trailing) surrogate.
pub(crate) fn is_low_surrogate(u: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&u)
}

/// JS `\s` on a char.
pub(crate) fn is_ws(c: char) -> bool {
    vox_core::js::is_js_whitespace(c)
}

/// The JS line terminators `^`/`$` stop at in multiline mode and `.` never matches.
pub(crate) fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// ASCII `\w` (`[A-Za-z0-9_]`): JS word characters, also with the `u` flag.
pub(crate) fn is_ascii_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn category(re: &Regex, c: char) -> bool {
    let mut buf = [0u8; 4];
    re.is_match(c.encode_utf8(&mut buf))
}

static LETTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{L}$").expect("valid regex"));
static UPPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{Lu}$").expect("valid regex"));
static LOWER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{Ll}$").expect("valid regex"));
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{N}$").expect("valid regex"));

/// `\p{L}` (general category Letter; `char::is_alphabetic` is the wider Alphabetic property).
pub(crate) fn is_letter(c: char) -> bool {
    category(&LETTER, c)
}

/// `\p{Lu}` (general category; `char::is_uppercase` is the wider Uppercase property).
pub(crate) fn is_upper(c: char) -> bool {
    category(&UPPER, c)
}

/// `\p{Ll}`.
pub(crate) fn is_lower(c: char) -> bool {
    category(&LOWER, c)
}

/// `\p{N}`.
pub(crate) fn is_number(c: char) -> bool {
    category(&NUMBER, c)
}

/// The char starting at UTF-16 offset `i` as JS sees `text[i]`: a lone surrogate (half of a pair) is `None`.
pub(crate) fn bmp_char_at(units: &[u16], i: usize) -> Option<char> {
    let u = *units.get(i)?;
    if (0xD800..=0xDFFF).contains(&u) {
        None
    } else {
        char::from_u32(u32::from(u))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        assert!(is_letter('é') && is_letter('中') && !is_letter('1') && !is_letter('.'));
        assert!(is_upper('A') && is_upper('É') && !is_upper('a') && !is_upper('Ⓐ'));
        assert!(is_lower('a') && is_lower('ß') && !is_lower('A'));
        assert!(is_number('3') && is_number('٣') && is_number('½') && !is_number('x'));
    }

    #[test]
    fn whitespace_units_match_js_set() {
        for c in ['\t', '\n', '\u{B}', '\u{C}', '\r', ' ', '\u{A0}', '\u{1680}', '\u{2000}', '\u{200A}', '\u{2028}'] {
            assert!(is_space_unit(c as u16), "{c:?}");
            assert!(is_ws(c));
        }
        for c in ['\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}', '\u{FEFF}'] {
            assert!(is_space_unit(c as u16), "{c:?}");
            assert!(is_ws(c));
        }
        assert!(!is_space_unit(0x85) && !is_ws('\u{85}'));
        assert!(!is_space_unit(0x200B));
        assert!(!is_space_unit(0xD800));
    }

    #[test]
    fn units_round_trip() {
        let s = "a😀b";
        let u = units(s);
        assert_eq!(u.len(), 4);
        assert_eq!(string_of(&u), s);
        assert_eq!(string_of(&u[..2]), "a\u{FFFD}");
        assert_eq!(bmp_char_at(&u, 0), Some('a'));
        assert_eq!(bmp_char_at(&u, 1), None);
        assert_eq!(bmp_char_at(&u, 9), None);
        assert!(is_high_surrogate(u[1]) && is_low_surrogate(u[2]));
    }
}
