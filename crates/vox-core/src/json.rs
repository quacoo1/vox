//! JSON compatibility with the Electron app's files (persistence.md §2.4, §11).
//!
//! - Reading: strip a UTF-8 BOM, repair lone UTF-16 surrogate escapes (the old app could write `"\ud83d"` when a
//!   160-unit preview split a pair; `serde_json` rejects those), reject empty files.
//! - Writing: `JSON.stringify` output. `serde_json`'s pretty printer with a 2-space indent produces the same layout
//!   as `JSON.stringify(v, null, 2)`; numbers must be serialized with [`js_f64`] so integral values have no `.0`.
//! - `serde_json` is built with `preserve_order`, so a parsed [`Value`] keeps the file's key order (needed for the
//!   "rewrite only if different" rule of settings.json).

use std::borrow::Cow;

use serde::Serialize;
use serde_json::Value;

/// Strips one leading U+FEFF (Notepad's BOM).
pub fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

/// Replaces `\uD800`–`\uDFFF` escapes that are not part of a valid high+low pair with `�`.
/// Escaped backslashes (`\\u…`) are left alone. Returns the input unchanged (borrowed) when nothing needed fixing.
pub fn repair_lone_surrogates(s: &str) -> Cow<'_, str> {
    let bytes = s.as_bytes();
    if !s.contains("\\u") {
        return Cow::Borrowed(s);
    }
    let hex4 = |at: usize| -> Option<u16> {
        let h = bytes.get(at..at + 4)?;
        let text = std::str::from_utf8(h).ok()?;
        u16::from_str_radix(text, 16).ok()
    };
    let mut out: Option<String> = None;
    let mut i = 0;
    let mut copied_to = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) != Some(&b'u') {
            i += 2; // some other escape (\\, \", \n …): skip both bytes
            continue;
        }
        let Some(unit) = hex4(i + 2) else {
            i += 2;
            continue;
        };
        let mut bad = false;
        let mut len = 6;
        if (0xD800..=0xDBFF).contains(&unit) {
            let low = if bytes.get(i + 6) == Some(&b'\\') && bytes.get(i + 7) == Some(&b'u') { hex4(i + 8) } else { None };
            match low {
                Some(l) if (0xDC00..=0xDFFF).contains(&l) => len = 12,
                _ => bad = true,
            }
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            bad = true;
        }
        if bad {
            let o = out.get_or_insert_with(|| String::with_capacity(s.len()));
            o.push_str(&s[copied_to..i]);
            o.push_str("\\ufffd");
            copied_to = i + 6;
        }
        i += len;
    }
    match out {
        Some(mut o) => {
            o.push_str(&s[copied_to..]);
            Cow::Owned(o)
        }
        None => Cow::Borrowed(s),
    }
}

/// Error from [`parse_lenient`].
#[derive(Debug)]
pub enum LenientParseError {
    /// Empty or whitespace-only text ("file is empty").
    Empty,
    /// Not valid JSON.
    Invalid(serde_json::Error),
}

impl std::fmt::Display for LenientParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "file is empty"),
            Self::Invalid(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LenientParseError {}

/// How [`parse_lenient_with`] treats number literals outside the f64 range (`1e400`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutOfRange {
    /// Replace them with the strings `"Infinity"` / `"-Infinity"` (files and ElevenLabs bodies: a sanitizer rejects
    /// the field exactly like JS `±Infinity`).
    #[default]
    InfinityString,
    /// Replace them with `null` (the browser bridge, integrations.md §1.4 step 12 / §7 #15, ARCHITECTURE.md §8
    /// D-INT-15): every old outcome is reproduced — `"text":1e400` → `"text" must be a string`, a `title`/`url` is
    /// dropped, a numeric field → `must be a finite number`, a top-level `1e400` → `Expected a JSON object`.
    Null,
}

/// Parses JSON text the way the old `readJsonFile` / bridge body parser did: BOM stripped, empty → error, lone
/// surrogate escapes repaired, and number literals outside the f64 range (`1e400`) replaced by the strings
/// `"Infinity"` / `"-Infinity"` ([`quote_out_of_range_numbers`]) instead of failing the whole document — JS
/// `JSON.parse` yields ±Infinity there, which every sanitizer rejects as "not a finite number", and a string is
/// rejected the same way (persistence.md §2.4, integrations.md §7 #15; ARCHITECTURE.md §8 D-P-4).
/// Equals `parse_lenient_with(text, OutOfRange::InfinityString)`.
pub fn parse_lenient(text: &str) -> Result<Value, LenientParseError> {
    parse_lenient_with(text, OutOfRange::InfinityString)
}

/// [`parse_lenient`] with an explicit [`OutOfRange`] policy. The ElevenLabs client parses every body (2xx and error
/// bodies) with the default, so an out-of-range alignment time becomes a non-number and `map_alignment` returns
/// `None` exactly as with JS `Infinity` (ARCHITECTURE.md §2.4); the bridge uses [`OutOfRange::Null`].
pub fn parse_lenient_with(text: &str, mode: OutOfRange) -> Result<Value, LenientParseError> {
    let t = strip_bom(text);
    if t.trim().is_empty() {
        return Err(LenientParseError::Empty);
    }
    let repaired = repair_lone_surrogates(t);
    let numbers = replace_out_of_range_numbers(&repaired, mode);
    serde_json::from_str(&numbers).map_err(LenientParseError::Invalid)
}

/// Replaces number literals (outside strings) whose value overflows `f64` with `"Infinity"` / `"-Infinity"`.
/// Returns the input borrowed when there is none.
pub fn quote_out_of_range_numbers(s: &str) -> Cow<'_, str> {
    replace_out_of_range_numbers(s, OutOfRange::InfinityString)
}

/// Replaces number literals (outside strings) whose value overflows `f64` according to `mode`. Returns the input
/// borrowed when there is none.
pub fn replace_out_of_range_numbers(s: &str, mode: OutOfRange) -> Cow<'_, str> {
    let b = s.as_bytes();
    let mut out: Option<String> = None;
    let mut copied_to = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            i += 1;
            while i < b.len() {
                match b[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            continue;
        }
        if c == b'-' || c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_digit() || matches!(b[i], b'.' | b'e' | b'E' | b'+' | b'-')) {
                i += 1;
            }
            if let Ok(v) = s[start..i].parse::<f64>() {
                if v.is_infinite() {
                    let o = out.get_or_insert_with(|| String::with_capacity(s.len()));
                    o.push_str(&s[copied_to..start]);
                    o.push_str(match mode {
                        OutOfRange::Null => "null",
                        OutOfRange::InfinityString if v < 0.0 => "\"-Infinity\"",
                        OutOfRange::InfinityString => "\"Infinity\"",
                    });
                    copied_to = i;
                }
            }
            continue;
        }
        i += 1;
    }
    match out {
        Some(mut o) => {
            o.push_str(&s[copied_to..]);
            Cow::Owned(o)
        }
        None => Cow::Borrowed(s),
    }
}

/// `JSON.stringify(v, null, 2)`: 2-space indent, `"key": value`, no trailing newline.
pub fn to_js_pretty<T: Serialize + ?Sized>(v: &T) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    v.serialize(&mut ser).expect("serializing to memory cannot fail");
    String::from_utf8(buf).expect("serde_json writes UTF-8")
}

/// `JSON.stringify(v)`: compact.
pub fn to_js_compact<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).expect("serializing to memory cannot fail")
}

/// Serializes an `f64` like `JSON.stringify`: integral values (|x| < 2^53) as integers, non-finite as `null`.
pub fn serialize_js_number<S: serde::Serializer>(v: f64, s: S) -> Result<S::Ok, S::Error> {
    if !v.is_finite() {
        s.serialize_unit()
    } else if v == v.trunc() && v.abs() < 9_007_199_254_740_992.0 {
        s.serialize_i64(v as i64)
    } else {
        s.serialize_f64(v)
    }
}

/// `#[serde(serialize_with = "vox_core::json::js_f64")]`
pub fn js_f64<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    serialize_js_number(*v, s)
}

/// `#[serde(serialize_with = "vox_core::json::js_opt_f64")]` (use with `skip_serializing_if = "Option::is_none"`).
pub fn js_opt_f64<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(x) => serialize_js_number(*x, s),
        None => s.serialize_none(),
    }
}

/// `#[serde(serialize_with = "vox_core::json::js_f64_vec")]`
pub fn js_f64_vec<S: serde::Serializer>(v: &[f64], s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = s.serialize_seq(Some(v.len()))?;
    for x in v {
        seq.serialize_element(&JsNumber(*x))?;
    }
    seq.end()
}

/// Wrapper that serializes an `f64` with [`serialize_js_number`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JsNumber(pub f64);

impl Serialize for JsNumber {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_js_number(self.0, s)
    }
}

/// Lenient accessors over untrusted JSON (contracts.md §3: "finite" = a JSON number; booleans must be JSON booleans).
pub mod lenient {
    use serde_json::{Map, Value};

    /// `v` as an object (`isObj`: not null, not an array).
    pub fn obj(v: &Value) -> Option<&Map<String, Value>> {
        v.as_object()
    }

    /// The child object at `key`, or an empty map when absent / not an object (sanitize's `rec()`).
    pub fn child<'a>(v: &'a Value, key: &str) -> Cow<'a> {
        match v.get(key) {
            Some(Value::Object(m)) => Cow::Borrowed(m),
            _ => Cow::Empty,
        }
    }

    /// Borrowed-or-empty object view.
    pub enum Cow<'a> {
        Borrowed(&'a Map<String, Value>),
        Empty,
    }

    impl<'a> Cow<'a> {
        pub fn get(&self, key: &str) -> Option<&'a Value> {
            match self {
                Cow::Borrowed(m) => m.get(key),
                Cow::Empty => None,
            }
        }
    }

    /// A JSON number (always finite in JSON).
    pub fn number(v: Option<&Value>) -> Option<f64> {
        match v {
            Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
            _ => None,
        }
    }

    /// A JSON boolean.
    pub fn boolean(v: Option<&Value>) -> Option<bool> {
        match v {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// A JSON string.
    pub fn string(v: Option<&Value>) -> Option<&str> {
        match v {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bom_and_empty() {
        assert!(matches!(parse_lenient("  \n"), Err(LenientParseError::Empty)));
        assert_eq!(parse_lenient("\u{FEFF}{\"a\":1}").unwrap(), json!({"a": 1}));
    }

    #[test]
    fn lone_surrogates_are_repaired() {
        let v = parse_lenient(r#"{"text":"ab\ud83d","ok":"😀","low":"\ude00x","esc":"\\ud83d"}"#).unwrap();
        assert_eq!(v["text"], "ab\u{FFFD}");
        assert_eq!(v["ok"], "\u{1F600}");
        assert_eq!(v["low"], "\u{FFFD}x");
        assert_eq!(v["esc"], "\\ud83d");
    }

    #[test]
    fn out_of_range_numbers_do_not_fail_the_document() {
        let v = parse_lenient(r#"{"a":1e400,"b":"1e400","c":-1e999,"d":1.5,"e":[2,1E+309],"f":"x\"1e400"}"#).unwrap();
        assert_eq!(v["a"], "Infinity");
        assert_eq!(v["b"], "1e400");
        assert_eq!(v["c"], "-Infinity");
        assert_eq!(v["d"], 1.5);
        assert_eq!(v["e"], json!([2, "Infinity"]));
        assert_eq!(v["f"], "x\"1e400");
        assert!(lenient::number(v.get("a")).is_none());
        assert!(matches!(quote_out_of_range_numbers("[1,2]"), Cow::Borrowed(_)));
    }

    #[test]
    fn bridge_mode_turns_out_of_range_numbers_into_null() {
        let v = parse_lenient_with(r#"{"text":1e400,"title":-1e400,"rate":1E+309,"s":"1e400","ok":2}"#, OutOfRange::Null).unwrap();
        assert_eq!(v, json!({"text": null, "title": null, "rate": null, "s": "1e400", "ok": 2}));
        assert_eq!(parse_lenient_with("1e400", OutOfRange::Null).unwrap(), Value::Null);
        assert_eq!(parse_lenient_with("[1e400]", OutOfRange::InfinityString).unwrap(), json!(["Infinity"]));
        // An ElevenLabs body with a lone surrogate and an out-of-range time still parses (tts.md §8; D-NET-2).
        let body = parse_lenient(r#"{"audio_base64":"AA==","normalized_alignment":{"characters":["\ud800"],"character_start_times_seconds":[1e400]}}"#)
            .unwrap();
        assert_eq!(body["normalized_alignment"]["characters"][0], "\u{FFFD}");
        assert!(lenient::number(body["normalized_alignment"]["character_start_times_seconds"].get(0)).is_none());
    }

    #[test]
    fn js_numbers() {
        #[derive(Serialize)]
        struct T {
            #[serde(serialize_with = "js_f64")]
            a: f64,
            #[serde(serialize_with = "js_f64")]
            b: f64,
            #[serde(serialize_with = "js_f64_vec")]
            c: Vec<f64>,
        }
        let t = T { a: 1.0, b: 0.75, c: vec![0.0, 0.192, -0.0] };
        assert_eq!(to_js_compact(&t), r#"{"a":1,"b":0.75,"c":[0,0.192,0]}"#);
    }

    #[test]
    fn pretty_matches_json_stringify() {
        let v = json!({"version": 1, "list": [], "o": {"x": null}});
        assert_eq!(to_js_pretty(&v), "{\n  \"version\": 1,\n  \"list\": [],\n  \"o\": {\n    \"x\": null\n  }\n}");
    }
}
