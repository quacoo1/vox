//! Sentence and word segmentation.
//!
//! - [`sentence_spans`] is the engine's splitter (tts.md §3.7): UAX #29 sentence breaks (`icu_segmenter`; every `\n`
//!   ends a sentence), trimmed spans, then abbreviation merging (`HONORIFICS`, `ALWAYS_JOIN`, `BEFORE_NUMBER`, single
//!   capital initials) over a 24-unit window. The verified table in tts.md §3.7 is the test suite.
//! - [`sentence_starts`] is the player's version (reader-playback.md §5): segment starts advanced past whitespace,
//!   empty segments dropped, sentences longer than 280 units soft-split at `, ; : — –` + whitespace (preferring a cut
//!   ≤ 200 units from the sentence start, never within the first 40); never empty (`[0]` for empty text). The engine
//!   puts the result in `TtsSessionStart::sentences`.
//! - [`word_spans`] = every `\S+` run (JS whitespace) inside a range.
//!
//! Every offset is a UTF-16 code unit; the algorithms run over the text's UTF-16 units exactly like the original.

use std::sync::LazyLock;

use icu_segmenter::options::SentenceBreakInvariantOptions;
use icu_segmenter::SentenceSegmenter;
use regex::Regex;
use vox_core::constants::{LONG_SENTENCE, SOFT_TARGET};
use vox_core::tts::TextSpan;

use crate::util::{bmp_char_at, is_number, is_space_unit, is_upper, string_of, units};

/// Abbreviation window: the last ≤ 24 UTF-16 units of the sentence so far (text.ts:250).
const ABBREVIATION_WINDOW: usize = 24;

/// Compared case-sensitively.
const HONORIFICS: &[&str] = &[
    "Mr", "Mrs", "Ms", "Mx", "Messrs", "Mme", "Mlle", "Dr", "Prof", "Rev", "Hon", "St", "Mt", "Ft", "Gen", "Col", "Lt",
    "Sgt", "Capt", "Cmdr", "Adm", "Gov", "Sen", "Rep", "Pres", "Fr", "Supt", "Insp",
];
/// Compared lower-cased; always joined with the next sentence.
const ALWAYS_JOIN: &[&str] = &["e.g", "i.e", "vs", "v", "cf", "viz", "approx", "ca", "resp", "incl"];
/// Compared lower-cased; joined only when the next sentence starts with a digit (`Fig. 3`, `No. 5`, `Jan. 5`).
const BEFORE_NUMBER: &[&str] = &[
    "no", "nos", "fig", "figs", "eq", "eqs", "vol", "vols", "ch", "sec", "pp", "p", "art", "para", "ref", "refs",
    "tab", "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec", "ver", "op", "ed",
];

/// `LAST_TOKEN = /(?:^|[^\p{L}.])(\p{L}+(?:\.\p{L}+)*)\.$/u` (no look-around: the `regex` crate's leftmost-first
/// match is the one V8 finds).
static LAST_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^\p{L}.])(\p{L}+(?:\.\p{L}+)*)\.$").expect("valid regex"));

/// UAX #29 sentence boundaries of `u` (UTF-16 offsets): ascending, starting with 0 and ending with `u.len()`, no
/// duplicates. ICU4X implements the same rules as ICU4C's `Intl.Segmenter` (plain, no abbreviation suppression).
fn uax29_boundaries(u: &[u16]) -> Vec<usize> {
    let segmenter = SentenceSegmenter::new(SentenceBreakInvariantOptions::default());
    let mut out: Vec<usize> = Vec::new();
    out.push(0);
    for b in segmenter.segment_utf16(u) {
        let b = b.min(u.len());
        if b > *out.last().unwrap_or(&0) {
            out.push(b);
        }
    }
    if *out.last().unwrap_or(&0) < u.len() {
        out.push(u.len());
    }
    out
}

/// `trimSpan`: strip `isSpace` units at both ends; `None` when nothing is left.
pub(crate) fn trim_span(u: &[u16], start: usize, end: usize) -> Option<(usize, usize)> {
    let end = end.min(u.len());
    let mut s = start.min(end);
    let mut e = end;
    while s < e && is_space_unit(u[s]) {
        s += 1;
    }
    while e > s && is_space_unit(u[e - 1]) {
        e -= 1;
    }
    (e > s).then_some((s, e))
}

/// `shouldJoin(text, cur, next)` (tts.md §3.7): abbreviation merging between two trimmed sentence spans.
fn should_join(u: &[u16], cur: (usize, usize), next: (usize, usize)) -> bool {
    let from = cur.0.max(cur.1.saturating_sub(ABBREVIATION_WINDOW));
    // A window starting inside a surrogate pair begins with a lone low surrogate in JS; U+FFFD classifies the same
    // way (neither a letter nor a dot).
    let window = string_of(&u[from..cur.1]);
    let Some(caps) = LAST_TOKEN.captures(&window) else {
        return false;
    };
    let word = caps.get(1).map_or("", |m| m.as_str());
    // `text[next.start]` is one UTF-16 unit: an astral first char is a lone surrogate there and never matches.
    let next_char = bmp_char_at(u, next.0);
    let mut chars = word.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if is_upper(c) {
            // Initials ("J. R. R. Tolkien"): this decides the outcome.
            return next_char.is_some_and(is_upper);
        }
    }
    if HONORIFICS.contains(&word) {
        return true;
    }
    let lower = word.to_lowercase();
    if ALWAYS_JOIN.contains(&lower.as_str()) {
        return true;
    }
    if BEFORE_NUMBER.contains(&lower.as_str()) {
        return next_char.is_some_and(is_number);
    }
    false
}

/// Trimmed, abbreviation-merged sentence spans over UTF-16 units (shared with the chunker).
pub(crate) fn sentence_spans_units(u: &[u16]) -> Vec<(usize, usize)> {
    let bounds = uax29_boundaries(u);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for pair in bounds.windows(2) {
        let Some(t) = trim_span(u, pair[0], pair[1]) else {
            continue;
        };
        match spans.last_mut() {
            Some(last) if should_join(u, *last, t) => last.1 = t.1,
            _ => spans.push(t),
        }
    }
    spans
}

fn span(s: usize, e: usize) -> TextSpan {
    TextSpan::new(to_u32(s), to_u32(e))
}

fn to_u32(x: usize) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

/// Trimmed sentence spans with abbreviations merged (UTF-16 offsets into `text`).
pub fn sentence_spans(text: &str) -> Vec<TextSpan> {
    let u = units(text);
    sentence_spans_units(&u).into_iter().map(|(s, e)| span(s, e)).collect()
}

fn is_terminal(u: u16) -> bool {
    // . ! ? 。 ！ ？ ‼ ⁇ ⁈ ⁉
    matches!(u, 0x2E | 0x21 | 0x3F | 0x3002 | 0xFF01 | 0xFF1F | 0x203C | 0x2047 | 0x2048 | 0x2049)
}

fn is_closer(u: u16) -> bool {
    // " ' ” ’ ) ] » 」 』
    matches!(u, 0x22 | 0x27 | 0x201D | 0x2019 | 0x29 | 0x5D | 0xBB | 0x300D | 0x300F)
}

/// The fallback splitter used when no UAX #29 segmenter is available (tts.md §3.7 `fallbackSegments`). Kept for
/// parity tests; production uses ICU4X. Spans are raw (untrimmed) and cover the whole text.
pub fn fallback_segments(text: &str) -> Vec<TextSpan> {
    let u = units(text);
    let n = u.len();
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        let ch = u[i];
        if ch == u16::from(b'\n') {
            spans.push(span(start, i + 1));
            start = i + 1;
            i += 1;
            continue;
        }
        if !is_terminal(ch) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && (is_terminal(u[j]) || is_closer(u[j])) {
            j += 1;
        }
        let cjk = matches!(ch, 0x3002 | 0xFF01 | 0xFF1F);
        if j >= n || cjk || is_space_unit(u[j]) {
            while j < n && u[j] != u16::from(b'\n') && is_space_unit(u[j]) {
                j += 1;
            }
            spans.push(span(start, j));
            start = j;
        }
        // `i = j - 1` then the loop's `i++`.
        i = j.max(i + 1);
    }
    if start < n {
        spans.push(span(start, n));
    }
    spans
}

/// Sentence start offsets for skip/highlight (ascending, never empty).
///
/// The player's `sentenceStarts` (reader-playback.md §5): every UAX #29 segment start advanced past whitespace,
/// empty segments dropped, each followed by its [`soft_splits`]. No abbreviation merging. `[0]` for empty or
/// whitespace-only text.
pub fn sentence_starts(text: &str) -> Vec<u32> {
    let u = units(text);
    let bounds = uax29_boundaries(&u);
    let mut out = Vec::new();
    for pair in bounds.windows(2) {
        let (mut a, b) = (pair[0], pair[1]);
        while a < b && is_space_unit(u[a]) {
            a += 1;
        }
        if a >= b {
            continue;
        }
        out.push(to_u32(a));
        out.extend(soft_splits_units(&u, a, b).into_iter().map(to_u32));
    }
    if out.is_empty() {
        out.push(0);
    }
    out
}

fn is_soft_clause(u: u16) -> bool {
    // , ; : — –
    matches!(u, 0x2C | 0x3B | 0x3A | 0x2014 | 0x2013)
}

fn soft_splits_units(u: &[u16], start: usize, end: usize) -> Vec<usize> {
    let end = end.min(u.len());
    let mut out = Vec::new();
    let mut from = start;
    while end.saturating_sub(from) > LONG_SENTENCE {
        let mut cut: Option<usize> = None;
        let hi = (end - 1).min(from + LONG_SENTENCE);
        let mut p = hi;
        while p > from + 40 {
            if is_soft_clause(u[p - 1]) && u.get(p).is_some_and(|&c| is_space_unit(c)) {
                cut = Some(p);
                if p - from <= SOFT_TARGET {
                    break;
                }
            }
            p -= 1;
        }
        let Some(mut cut) = cut else {
            break;
        };
        while cut < end && is_space_unit(u[cut]) {
            cut += 1;
        }
        if cut >= end {
            break;
        }
        out.push(cut);
        from = cut;
    }
    out
}

/// Soft split points inside `[start, end)` for a sentence longer than 280 units (reader-playback.md §5): the highest
/// clause break (`, ; : — –` followed by whitespace) at ≤ 200 units from the current start, else the lowest one in
/// (200, 280], never within the first 40 units; each cut is advanced past whitespace.
pub fn soft_splits(text: &str, start: u32, end: u32) -> Vec<u32> {
    let u = units(text);
    soft_splits_units(&u, start as usize, end as usize).into_iter().map(to_u32).collect()
}

/// Every non-whitespace run inside `[start, end)` as absolute UTF-16 spans.
pub fn word_spans(text: &str, start: u32, end: u32) -> Vec<TextSpan> {
    let u = units(text);
    word_spans_units(&u, start as usize, end as usize).into_iter().map(|(s, e)| span(s, e)).collect()
}

/// `wordSpans` over units: `text.slice(start, end)` (clamped; `start > end` → nothing) split into `\S+` runs.
pub(crate) fn word_spans_units(u: &[u16], start: usize, end: usize) -> Vec<(usize, usize)> {
    let end = end.min(u.len());
    let mut out = Vec::new();
    let mut i = start.min(end);
    while i < end {
        if is_space_unit(u[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < end && !is_space_unit(u[i]) {
            i += 1;
        }
        out.push((s, i));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::text16::slice16;

    fn sentences(text: &str) -> Vec<&str> {
        sentence_spans(text).iter().map(|s| slice16(text, s.start as usize, s.end as usize)).collect()
    }

    /// tts.md §3.7 verified table (ICU raw boundaries + the merge rules).
    #[test]
    fn sentence_spans_verified_table() {
        let cases: &[(&str, &[&str])] = &[
            (
                "Dr. Smith went to Washington. He arrived at 5 p.m. Then left.",
                &["Dr. Smith went to Washington.", "He arrived at 5 p.m.", "Then left."],
            ),
            ("See Fig. 3 for details. And No. 5 too.", &["See Fig. 3 for details.", "And No. 5 too."]),
            ("J. R. R. Tolkien wrote it. Yes.", &["J. R. R. Tolkien wrote it.", "Yes."]),
            ("e.g. This one. Next.", &["e.g. This one.", "Next."]),
            ("Hello world.\nNext line here", &["Hello world.", "Next line here"]),
            ("Mr. and Mrs. Smith. Next.", &["Mr. and Mrs. Smith.", "Next."]),
            ("He said \"Stop.\" Then left.", &["He said \"Stop.\"", "Then left."]),
            ("Wait... What? Yes!", &["Wait...", "What?", "Yes!"]),
            ("中文句子。第二句。", &["中文句子。", "第二句。"]),
            ("Version 2.0 is out. Next.", &["Version 2.0 is out.", "Next."]),
            ("Hello world.Next sentence.", &["Hello world.Next sentence."]),
            ("vs. them. ok", &["vs. them. ok"]),
            ("Fig. A shows", &["Fig.", "A shows"]),
            ("Plan A. Then we left.", &["Plan A. Then we left."]),
            ("U.S. Army is here. Next.", &["U.S.", "Army is here.", "Next."]),
        ];
        for (text, expected) in cases {
            assert_eq!(sentences(text), *expected, "{text:?}");
        }
    }

    #[test]
    fn sentence_spans_edges() {
        assert!(sentence_spans("").is_empty());
        assert!(sentence_spans("   \n\n  ").is_empty());
        // Trimmed: no leading/trailing whitespace inside a span.
        let t = "  One.   Two.  ";
        assert_eq!(sentences(t), ["One.", "Two."]);
        // UTF-16 offsets.
        let t = "😀 Hi. Yo.";
        assert_eq!(sentence_spans(t), vec![TextSpan::new(0, 6), TextSpan::new(7, 10)]);
        // Merge keeps extending the same output span.
        assert_eq!(sentences("Prof. Dr. Smith spoke. Ok."), ["Prof. Dr. Smith spoke.", "Ok."]);
        assert_eq!(sentences("See p. 12 and pp. 3. Done."), ["See p. 12 and pp. 3.", "Done."]);
        assert_eq!(sentences("It was Jan. 5 already. Yes."), ["It was Jan. 5 already.", "Yes."]);
        assert_eq!(sentences("Compare cf. Smith. Then."), ["Compare cf. Smith.", "Then."]);
        // `."` endings never join, even after an honorific.
        assert_eq!(sentences("He said \"Mr.\" Then left."), ["He said \"Mr.\"", "Then left."]);
    }

    #[test]
    fn fallback_segments_rules() {
        let spans = |t: &str| -> Vec<(u32, u32)> { fallback_segments(t).iter().map(|s| (s.start, s.end)).collect() };
        assert_eq!(spans("One. Two."), vec![(0, 5), (5, 9)]);
        assert_eq!(spans("a\nb"), vec![(0, 2), (2, 3)]);
        assert_eq!(spans("Hi!) Next"), vec![(0, 5), (5, 9)]);
        assert_eq!(spans("2.0 is"), vec![(0, 6)]);
        assert_eq!(spans("中文。第二"), vec![(0, 3), (3, 5)]);
        assert_eq!(spans("End.  \nNext"), vec![(0, 6), (6, 7), (7, 11)]);
        assert_eq!(spans("Wait..."), vec![(0, 7)]);
        assert!(spans("").is_empty());
    }

    #[test]
    fn sentence_starts_player_rules() {
        assert_eq!(sentence_starts(""), vec![0]);
        assert_eq!(sentence_starts("   "), vec![0]);
        assert_eq!(sentence_starts("Hello world. Next one."), vec![0, 13]);
        // No abbreviation merging in the player's version (Mr. breaks before an uppercase word).
        assert_eq!(sentence_starts("Mr. Smith left."), vec![0, 4]);
        // Leading whitespace skipped; line breaks end sentences.
        assert_eq!(sentence_starts("  a.\n\nb"), vec![2, 6]);
        // UTF-16 offsets.
        assert_eq!(sentence_starts("😀 Hi. Yo."), vec![0, 7]);
    }

    #[test]
    fn soft_splits_long_sentences() {
        // A 400-unit sentence with commas every 50 units: highest clause break ≤ 200 from the start.
        let part = format!("{}, ", "a".repeat(48)); // 50 units, comma at 48, space at 49
        let text = part.repeat(8);
        let len = text.encode_utf16().count() as u32;
        let cuts = soft_splits(&text, 0, len);
        assert_eq!(cuts, vec![200]);
        assert_eq!(sentence_starts(&text), vec![0, 200]);

        // Nothing within the first 40 units counts; no clause break → no split.
        let text = format!("{}, {}", "b".repeat(30), "c".repeat(300));
        assert!(soft_splits(&text, 0, text.len() as u32).is_empty());

        // Only a break beyond 200 (the lowest one in (200, 280]).
        let text = format!("{}; {}; {}", "d".repeat(210), "e".repeat(20), "f".repeat(100));
        let n = text.len() as u32;
        assert_eq!(soft_splits(&text, 0, n), vec![212]);

        // Short sentences are never split; ranges are clamped.
        assert!(soft_splits("short, text here", 0, 16).is_empty());
        assert!(soft_splits("abc", 5, 999).is_empty());

        // The em dash and en dash are clause marks too.
        let text = format!("{}— {}", "g".repeat(100), "h".repeat(250));
        assert_eq!(soft_splits(&text, 0, text.encode_utf16().count() as u32), vec![102]);
    }

    #[test]
    fn word_spans_runs() {
        let t = "  Hello,  world!\nok ";
        let w: Vec<(u32, u32)> = word_spans(t, 0, t.len() as u32).iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(w, vec![(2, 8), (10, 16), (17, 19)]);
        // A sub-range cuts words at its edges (absolute offsets).
        let w: Vec<(u32, u32)> = word_spans(t, 4, 12).iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(w, vec![(4, 8), (10, 12)]);
        // UTF-16 units; NBSP and U+3000 are whitespace, U+200B is not.
        let t = "😀a\u{A0}b\u{3000}c\u{200B}d";
        let w: Vec<(u32, u32)> = word_spans(t, 0, 99).iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(w, vec![(0, 3), (4, 5), (6, 9)]);
        assert!(word_spans(t, 5, 2).is_empty());
        assert!(word_spans("", 0, 0).is_empty());
    }
}
