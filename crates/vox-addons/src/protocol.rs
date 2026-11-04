//! The helper protocol, version 1 (docs/addons-protocol.md): one compact UTF-8 JSON object per line, in both
//! directions. Pure: no processes and no threads, so the framing rules are unit-tested on bytes.
//!
//! - Vox → helper ([`HostMsg`]): `hello`, `voices`, `synthesize`, `cancel`, `quit`. Every message carries an `id`,
//!   increasing per process; `cancel` and `quit` get no reply.
//! - Helper → Vox ([`HelperMsg`]): `hello`, `voices`, `progress`, `audio`, `error`, each echoing the request's `id`.
//!   Unknown fields are ignored, and an unknown `type` parses as [`HelperBody::Other`] with its id kept, because it
//!   still proves the helper is alive.
//! - A line that is not a JSON object, has no `type`, is longer than its cap or ends without a newline is a protocol
//!   violation: the caller kills the helper.

use std::io::{self, BufRead};

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol majors this Vox offers in `hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

/// The reserved `synthesize.options` object (speed, language… in later versions); v1 always sends `{}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SynthOptions {}

/// A Vox → helper message.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMsg<'a> {
    Hello { id: u64, protocol: ProtocolRange, vox: &'a str, os: &'a str, arch: &'a str },
    Voices { id: u64 },
    Synthesize { id: u64, voice: &'a str, text: &'a str, word_timing: bool, options: SynthOptions },
    Cancel { id: u64, target: u64 },
    Quit { id: u64 },
}

/// One line, newline included.
pub fn encode(msg: &HostMsg<'_>) -> Vec<u8> {
    // Serialising these plain structs cannot fail.
    let mut line = serde_json::to_vec(msg).unwrap_or_default();
    line.push(b'\n');
    line
}

/// How a helper counts the `from`/`to` offsets of its word cues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OffsetUnit {
    /// UTF-8 bytes (the default).
    #[default]
    Utf8,
    /// Unicode scalar values.
    Codepoint,
    /// UTF-16 code units.
    Utf16,
}

impl OffsetUnit {
    pub fn parse(s: &str) -> Option<OffsetUnit> {
        match s {
            "utf8" => Some(OffsetUnit::Utf8),
            "codepoint" => Some(OffsetUnit::Codepoint),
            "utf16" => Some(OffsetUnit::Utf16),
            _ => None,
        }
    }
}

/// A helper → Vox message.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperMsg {
    /// The request it answers; `None` when the helper sent none.
    pub id: Option<u64>,
    pub body: HelperBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HelperBody {
    Hello(HelloReply),
    /// The raw list; `vox_core::addons::parse_voices` validates it.
    Voices(Value),
    Progress,
    Audio(AudioReply),
    Error(ErrorReply),
    /// A `type` this Vox does not know.
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HelloReply {
    /// `None` when missing or not a number.
    pub protocol: Option<u64>,
    pub name: String,
    pub version: String,
    /// `None`: the default (UTF-8), or a unit this Vox does not know (then word cues are not used).
    pub offsets: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioReply {
    pub format: String,
    /// Base64 of one complete RIFF/WAVE file.
    pub wav: String,
    /// Overrides the `hello` unit.
    pub offsets: Option<String>,
    /// The cues, unchecked (at most [`MAX_CUES`]); checked one by one when the result is built.
    pub words: Option<Vec<Cue>>,
}

/// One word cue as the helper sent it: each field is `None` when missing or not a number.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cue {
    pub from: Option<f64>,
    pub to: Option<f64>,
    pub start: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorReply {
    pub code: String,
    /// Untrusted: logged, never shown.
    pub message: String,
}

/// Cues kept from one `audio` reply (a chunk of 4096 bytes has fewer words); the rest are skipped unread.
pub const MAX_CUES: usize = 4096;
/// Voice entries kept from one `voices` reply: one more than a valid list may have, so an over-long list still fails.
const MAX_VOICE_ENTRIES: usize = 1001;

/// Every field any helper message may carry, read in one pass (a long chunk's audio line is megabytes). No field is
/// read into a general JSON tree: a tree costs up to ~60 times its text, so a 96 MiB line of `[0,0,…]` would need
/// gigabytes. Each field is a string, a number, or a bounded list of those, and a value of any other shape is skipped
/// without being stored (and is not a violation).
#[derive(Deserialize)]
struct RawMsg {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Loose,
    #[serde(default)]
    protocol: Loose,
    #[serde(default)]
    name: Loose,
    #[serde(default)]
    version: Loose,
    #[serde(default)]
    offsets: Loose,
    #[serde(default)]
    voices: Option<Voices>,
    #[serde(default)]
    format: Loose,
    #[serde(default)]
    wav: Loose,
    #[serde(default)]
    words: Option<Words>,
    #[serde(default)]
    code: Loose,
    #[serde(default)]
    message: Loose,
}

/// A scalar read without trusting its type.
#[derive(Debug, Clone, Default, PartialEq)]
enum Loose {
    #[default]
    Other,
    Str(String),
    Int(u64),
    Num(f64),
}

impl Loose {
    fn string(self) -> Option<String> {
        match self {
            Loose::Str(s) => Some(s),
            _ => None,
        }
    }

    fn text(self) -> String {
        self.string().unwrap_or_default()
    }

    fn u64(&self) -> Option<u64> {
        match *self {
            Loose::Int(n) => Some(n),
            _ => None,
        }
    }

    fn f64(&self) -> Option<f64> {
        match *self {
            Loose::Int(n) => Some(n as f64),
            Loose::Num(n) => Some(n),
            _ => None,
        }
    }
}

/// `visit_*` for the shapes a visitor does not want: each gives `$default`, and lists and objects are skipped without
/// being stored.
macro_rules! skip_other_shapes {
    ($default:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($default)
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($default)
        }
    };
}

fn skip_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn skip_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

impl<'de> Deserialize<'de> for Loose {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Loose;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Loose, E> {
                Ok(Loose::Other)
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Loose, E> {
                Ok(Loose::Num(n as f64))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<Loose, E> {
                Ok(Loose::Int(n))
            }
            fn visit_f64<E: de::Error>(self, n: f64) -> Result<Loose, E> {
                Ok(Loose::Num(n))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Loose, E> {
                Ok(Loose::Str(s.to_string()))
            }
            fn visit_string<E: de::Error>(self, s: String) -> Result<Loose, E> {
                Ok(Loose::Str(s))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Loose, E> {
                Ok(Loose::Other)
            }
            fn visit_none<E: de::Error>(self) -> Result<Loose, E> {
                Ok(Loose::Other)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Loose, D::Error> {
                Loose::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Loose, A::Error> {
                skip_seq(seq).map(|()| Loose::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Loose, A::Error> {
                skip_map(map).map(|()| Loose::Other)
            }
        }
        d.deserialize_any(V)
    }
}

/// A list of at most `max` items; the rest, and a value that is not a list, are skipped.
fn bounded_list<'de, A: SeqAccess<'de>, T: Deserialize<'de>>(mut seq: A, max: usize) -> Result<Vec<T>, A::Error> {
    let mut out = Vec::new();
    while out.len() < max {
        match seq.next_element::<T>()? {
            Some(item) => out.push(item),
            None => return Ok(out),
        }
    }
    skip_seq(seq).map(|()| out)
}

/// The fields of one object, each read as [`Loose`]; `keep` names the ones worth storing.
fn loose_fields<'de, A: MapAccess<'de>>(mut map: A, keep: &[&str]) -> Result<Vec<(String, Loose)>, A::Error> {
    let mut out = Vec::new();
    while let Some(key) = map.next_key::<String>()? {
        if keep.contains(&key.as_str()) && !out.iter().any(|(k, _)| *k == key) {
            out.push((key, map.next_value::<Loose>()?));
        } else {
            map.next_value::<IgnoredAny>()?;
        }
    }
    Ok(out)
}

struct Words(Vec<Cue>);

impl<'de> Deserialize<'de> for Words {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Words;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of word cues")
            }
            skip_other_shapes!(Words(Vec::new()));
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Words, A::Error> {
                bounded_list(seq, MAX_CUES).map(Words)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Words, A::Error> {
                skip_map(map).map(|()| Words(Vec::new()))
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for Cue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Cue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a word cue")
            }
            skip_other_shapes!(Cue::default());
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Cue, A::Error> {
                skip_seq(seq).map(|()| Cue::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Cue, A::Error> {
                let fields = loose_fields(map, &["from", "to", "start"])?;
                let get = |k: &str| fields.iter().find(|(f, _)| f == k).and_then(|(_, v)| v.f64());
                Ok(Cue { from: get("from"), to: get("to"), start: get("start") })
            }
        }
        d.deserialize_any(V)
    }
}

/// A `voices` list as plain objects of string fields (what `vox_core::addons::parse_voices` reads); `None` when it
/// was not a list.
struct Voices(Option<Vec<VoiceEntry>>);

/// One voice entry; `None` when it was not an object.
struct VoiceEntry(Option<Vec<(String, String)>>);

const VOICE_FIELDS: [&str; 7] = ["key", "name", "gender", "language", "accent", "use_case", "description"];

impl<'de> Deserialize<'de> for Voices {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Voices;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of voices")
            }
            skip_other_shapes!(Voices(None));
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Voices, A::Error> {
                bounded_list(seq, MAX_VOICE_ENTRIES).map(|v| Voices(Some(v)))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Voices, A::Error> {
                skip_map(map).map(|()| Voices(None))
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for VoiceEntry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = VoiceEntry;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a voice")
            }
            skip_other_shapes!(VoiceEntry(None));
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<VoiceEntry, A::Error> {
                skip_seq(seq).map(|()| VoiceEntry(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<VoiceEntry, A::Error> {
                let fields = loose_fields(map, &VOICE_FIELDS)?;
                Ok(VoiceEntry(Some(fields.into_iter().filter_map(|(k, v)| Some((k, v.string()?))).collect())))
            }
        }
        d.deserialize_any(V)
    }
}

impl Voices {
    /// The JSON shape `parse_voices` checks: a list of objects with string fields (a non-object entry stays `null`,
    /// which it refuses).
    fn into_value(self) -> Value {
        let Some(entries) = self.0 else { return Value::Null };
        Value::Array(
            entries
                .into_iter()
                .map(|e| match e.0 {
                    Some(fields) => Value::Object(fields.into_iter().map(|(k, v)| (k, Value::String(v))).collect()),
                    None => Value::Null,
                })
                .collect(),
        )
    }
}

/// Parses one helper line (without its newline). `Err` is a protocol violation, with what was wrong for the log.
pub fn parse_helper_line(line: &[u8]) -> Result<HelperMsg, String> {
    let raw: RawMsg = serde_json::from_slice(line).map_err(|e| format!("not a protocol message: {e}"))?;
    let id = raw.id.u64();
    let body = match raw.kind.as_str() {
        "hello" => HelperBody::Hello(HelloReply {
            protocol: raw.protocol.u64(),
            name: raw.name.text(),
            version: raw.version.text(),
            offsets: raw.offsets.string(),
        }),
        "voices" => HelperBody::Voices(raw.voices.map_or(Value::Null, Voices::into_value)),
        "progress" => HelperBody::Progress,
        "audio" => HelperBody::Audio(AudioReply {
            format: raw.format.text(),
            // A missing or non-string `wav` is caught when the audio is checked (bad audio, not a violation).
            wav: raw.wav.text(),
            offsets: raw.offsets.string(),
            words: raw.words.map(|w| w.0),
        }),
        "error" => HelperBody::Error(ErrorReply { code: raw.code.text(), message: raw.message.text() }),
        other => HelperBody::Other(other.to_string()),
    };
    Ok(HelperMsg { id, body })
}

/// Parses a `words` value the way an `audio` reply's is read (for tests of the cue checks).
#[doc(hidden)]
pub fn words_from_json(v: Value) -> Vec<Cue> {
    serde_json::from_value::<Words>(v).map(|w| w.0).unwrap_or_default()
}

/// Why a line could not be read.
#[derive(Debug)]
pub enum LineError {
    /// Longer than the cap; the rest of the stream is not usable.
    TooLong,
    /// The stream ended in the middle of a line.
    Truncated,
    Io(io::Error),
}

/// Reads `\n`-terminated lines of at most `cap` bytes (a trailing `\r` is stripped and not counted), never buffering
/// more than the cap plus one read.
pub struct LineReader<R> {
    inner: R,
    cap: usize,
}

impl<R: BufRead> LineReader<R> {
    pub fn new(inner: R, cap: usize) -> Self {
        LineReader { inner, cap }
    }

    /// The next line; `Ok(None)` at a clean end of stream.
    pub fn next_line(&mut self) -> Result<Option<Vec<u8>>, LineError> {
        let mut line = Vec::new();
        loop {
            let buf = match self.inner.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(LineError::Io(e)),
            };
            if buf.is_empty() {
                return if line.is_empty() { Ok(None) } else { Err(LineError::Truncated) };
            }
            // One byte of slack while collecting, for the `\r` of a CRLF.
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    if line.len() + i > self.cap + 1 {
                        self.inner.consume(i + 1);
                        return Err(LineError::TooLong);
                    }
                    line.extend_from_slice(&buf[..i]);
                    self.inner.consume(i + 1);
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    if line.len() > self.cap {
                        return Err(LineError::TooLong);
                    }
                    return Ok(Some(line));
                }
                None => {
                    let n = buf.len();
                    if line.len() + n > self.cap + 1 {
                        self.inner.consume(n);
                        return Err(LineError::TooLong);
                    }
                    line.extend_from_slice(buf);
                    self.inner.consume(n);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], cap: usize, chunk: usize) -> Vec<Result<Vec<u8>, String>> {
        // A reader that hands out `chunk` bytes per read, so lines straddle reads.
        let reader = io::BufReader::with_capacity(chunk.max(1), input);
        let mut r = LineReader::new(reader, cap);
        let mut out = Vec::new();
        loop {
            match r.next_line() {
                Ok(Some(l)) => out.push(Ok(l)),
                Ok(None) => break,
                Err(e) => {
                    out.push(Err(format!("{e:?}")));
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn lines_split_and_crlf_is_stripped() {
        for chunk in [1, 2, 3, 7, 64] {
            let got = lines(b"a\nbb\r\n\nccc\r\n", 8, chunk);
            assert_eq!(got, [Ok(b"a".to_vec()), Ok(b"bb".to_vec()), Ok(Vec::new()), Ok(b"ccc".to_vec())], "chunk {chunk}");
        }
    }

    #[test]
    fn the_cap_counts_the_line_not_its_crlf() {
        for chunk in [1, 3, 64] {
            assert_eq!(lines(b"abcd\r\n", 4, chunk), [Ok(b"abcd".to_vec())], "chunk {chunk}");
            assert_eq!(lines(b"abcd\n", 4, chunk), [Ok(b"abcd".to_vec())]);
            assert_eq!(lines(b"abcde\n", 4, chunk), [Err("TooLong".to_string())], "chunk {chunk}");
            assert_eq!(lines(b"abcde\r\n", 4, chunk), [Err("TooLong".to_string())], "chunk {chunk}");
        }
    }

    #[test]
    fn an_unterminated_last_line_is_truncated() {
        assert_eq!(lines(b"ok\nhalf", 16, 4), [Ok(b"ok".to_vec()), Err("Truncated".to_string())]);
        assert_eq!(lines(b"", 16, 4), []);
    }

    #[test]
    fn a_long_line_is_refused_without_reading_it_all() {
        let input = vec![b'x'; 1 << 20];
        let reader = io::BufReader::with_capacity(4096, input.as_slice());
        let mut r = LineReader::new(reader, 100);
        assert!(matches!(r.next_line(), Err(LineError::TooLong)));
    }

    /// Deterministic pseudo-random bytes (xorshift): the reader and the parser never panic, and no line exceeds the cap.
    #[test]
    fn random_input_never_panics_or_exceeds_the_cap() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet: &[u8] = b"{}[]\":,\n\r abc0123456789typeidwav\\\x00\xff\xc3\xa9";
        for _ in 0..10_000 {
            let len = (next() % 200) as usize;
            let input: Vec<u8> = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
            let cap = (next() % 40) as usize;
            let chunk = 1 + (next() % 16) as usize;
            for l in lines(&input, cap, chunk).into_iter().flatten() {
                assert!(l.len() <= cap);
                let _ = parse_helper_line(&l);
            }
        }
    }

    #[test]
    fn host_messages_encode_as_one_line() {
        let range = ProtocolRange { min: 1, max: 1 };
        let hello = encode(&HostMsg::Hello { id: 1, protocol: range, vox: "0.1.0", os: "windows", arch: "x86_64" });
        assert_eq!(
            String::from_utf8(hello).unwrap(),
            "{\"type\":\"hello\",\"id\":1,\"protocol\":{\"min\":1,\"max\":1},\"vox\":\"0.1.0\",\"os\":\"windows\",\"arch\":\"x86_64\"}\n"
        );
        let synth = encode(&HostMsg::Synthesize { id: 3, voice: "bella", text: "Line\nbreak \u{1F44B}", word_timing: true, options: SynthOptions {} });
        let s = String::from_utf8(synth).unwrap();
        assert_eq!(s.matches('\n').count(), 1, "a newline in the text is escaped");
        let v: Value = serde_json::from_str(s.trim_end()).unwrap();
        assert_eq!(v["type"], "synthesize");
        assert_eq!(v["text"], "Line\nbreak \u{1F44B}");
        assert_eq!(v["options"], serde_json::json!({}));
        assert_eq!(String::from_utf8(encode(&HostMsg::Cancel { id: 4, target: 3 })).unwrap(), "{\"type\":\"cancel\",\"id\":4,\"target\":3}\n");
        assert_eq!(String::from_utf8(encode(&HostMsg::Quit { id: 5 })).unwrap(), "{\"type\":\"quit\",\"id\":5}\n");
    }

    #[test]
    fn helper_messages_parse() {
        let m = parse_helper_line(br#"{"id":1,"type":"hello","protocol":1,"name":"fake","version":"2.0.0","offsets":"utf16","word_timing":"exact","x":[1]}"#).unwrap();
        assert_eq!(m.id, Some(1));
        assert_eq!(
            m.body,
            HelperBody::Hello(HelloReply { protocol: Some(1), name: "fake".into(), version: "2.0.0".into(), offsets: Some("utf16".into()) })
        );
        let m = parse_helper_line(br#"{"id":3,"type":"progress","stage":"loading","fraction":0.4}"#).unwrap();
        assert_eq!(m.body, HelperBody::Progress);
        let m = parse_helper_line(br#"{"id":3,"type":"audio","format":"wav","wav":"UklG","words":[{"from":0,"to":5,"start":0}]}"#).unwrap();
        let HelperBody::Audio(a) = m.body else { panic!() };
        assert_eq!(a.wav, "UklG");
        assert_eq!(a.offsets, None);
        assert_eq!(a.words.unwrap(), [Cue { from: Some(0.0), to: Some(5.0), start: Some(0.0) }]);
        let m = parse_helper_line(br#"{"id":3,"type":"error","code":"unknown_voice","message":"no voice"}"#).unwrap();
        assert_eq!(m.body, HelperBody::Error(ErrorReply { code: "unknown_voice".into(), message: "no voice".into() }));
        let m = parse_helper_line(br#"{"id":9,"type":"telemetry"}"#).unwrap();
        assert_eq!((m.id, m.body), (Some(9), HelperBody::Other("telemetry".into())));
        // Loose optional fields: a wrong type is not a violation.
        let m = parse_helper_line(br#"{"id":"x","type":"hello","protocol":"1","name":5}"#).unwrap();
        assert_eq!(m.id, None);
        assert_eq!(m.body, HelperBody::Hello(HelloReply { protocol: None, name: String::new(), version: String::new(), offsets: None }));
    }

    #[test]
    fn odd_shapes_are_skipped_not_stored() {
        // Big lists and objects in any field are read past without being kept, and are not violations.
        let zeros = format!("[{}0]", "0,".repeat(200_000));
        let line = format!(r#"{{"id":3,"type":"progress","words":{zeros},"voices":{zeros},"name":{zeros},"extra":{{"a":{zeros}}}}}"#);
        assert_eq!(parse_helper_line(line.as_bytes()).unwrap().body, HelperBody::Progress);
        // Words: at most MAX_CUES are kept; odd entries read as empty cues.
        let many = format!(r#"{{"id":3,"type":"audio","format":"wav","wav":"x","words":[{}{{"from":1}}]}}"#, r#""x","#.repeat(MAX_CUES + 10));
        let HelperBody::Audio(a) = parse_helper_line(many.as_bytes()).unwrap().body else { panic!() };
        let words = a.words.unwrap();
        assert_eq!(words.len(), MAX_CUES);
        assert!(words.iter().all(|c| *c == Cue::default()));
        let odd = br#"{"id":3,"type":"audio","wav":[1],"words":{"from":0},"offsets":7}"#;
        let HelperBody::Audio(a) = parse_helper_line(odd).unwrap().body else { panic!() };
        assert_eq!((a.wav.as_str(), a.offsets, a.words), ("", None, Some(Vec::new())));
        let cue = br#"{"id":3,"type":"audio","words":[{"from":"0","to":[1],"start":-0.5,"from":9}]}"#;
        let HelperBody::Audio(a) = parse_helper_line(cue).unwrap().body else { panic!() };
        assert_eq!(a.words.unwrap(), [Cue { from: None, to: None, start: Some(-0.5) }]);
    }

    #[test]
    fn voice_lists_keep_string_fields_only() {
        let line = br#"{"id":2,"type":"voices","voices":[{"key":"a","name":"A","gender":["x"],"extra":{"big":[1,2]}},7]}"#;
        let HelperBody::Voices(v) = parse_helper_line(line).unwrap().body else { panic!() };
        assert_eq!(v, serde_json::json!([{"key": "a", "name": "A"}, null]));
        let HelperBody::Voices(v) = parse_helper_line(br#"{"id":2,"type":"voices","voices":{"key":"a"}}"#).unwrap().body else { panic!() };
        assert_eq!(v, Value::Null);
        let HelperBody::Voices(v) = parse_helper_line(br#"{"id":2,"type":"voices"}"#).unwrap().body else { panic!() };
        assert_eq!(v, Value::Null);
        let over = format!(r#"{{"id":2,"type":"voices","voices":[{}{{}}]}}"#, "{},".repeat(5000));
        let HelperBody::Voices(Value::Array(v)) = parse_helper_line(over.as_bytes()).unwrap().body else { panic!() };
        assert_eq!(v.len(), MAX_VOICE_ENTRIES, "kept one past the limit, so the list is still refused");
    }

    #[test]
    fn violations() {
        for bad in [&b"not json"[..], b"[]", b"42", b"{}", br#"{"id":1}"#, br#"{"type":3}"#, b"{\"type\":\"hello\"", b"\xff\xfe"] {
            assert!(parse_helper_line(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn offset_units() {
        assert_eq!(OffsetUnit::parse("utf8"), Some(OffsetUnit::Utf8));
        assert_eq!(OffsetUnit::parse("codepoint"), Some(OffsetUnit::Codepoint));
        assert_eq!(OffsetUnit::parse("utf16"), Some(OffsetUnit::Utf16));
        assert_eq!(OffsetUnit::parse("UTF-8"), None);
    }
}
