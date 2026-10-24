//! `chunkText` (tts.md §3.8): splits cleaned text into synthesis chunks on sentence boundaries. The first chunk is
//! ≤ `first_max` (fast first audio), later ones ≤ `max`; over-long sentences split at clause punctuation, then at
//! spaces, then hard (never inside a surrogate pair). Offsets index the input exactly (`text[start..end] ==
//! chunk.text` in UTF-16 units); whitespace between chunks belongs to neither chunk. `""`/whitespace → no chunks.

use vox_core::constants::{CHUNK_MAX, FIRST_CHUNK_MAX};
use vox_core::tts::{TextChunk, TextSpan};

use crate::segment::{sentence_spans_units, trim_span};
use crate::util::{is_high_surrogate, is_low_surrogate, is_space_unit, string_of, units};

/// Chunks `text`; `first_max` is raised to ≥ 20 and `max` to ≥ `first_max`.
pub fn chunk_text(text: &str, first_max: usize, max: usize) -> Vec<TextChunk> {
    let first_max = first_max.max(20);
    let max = max.max(first_max);
    let u = units(text);
    let mut stack: Vec<(usize, usize)> = sentence_spans_units(&u);
    stack.reverse(); // pop() yields the sentences in order
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut cur: Option<(usize, usize)> = None;

    while let Some(unit) = stack.pop() {
        // The chunk being built is the first one until one is emitted.
        let limit = if out.is_empty() { first_max } else { max };
        match cur {
            None => {
                if unit.1 - unit.0 <= limit {
                    cur = Some(unit);
                } else {
                    let (head, rest) = split_units(&u, unit, limit);
                    out.push(head);
                    // The remainder is considered again (and may merge with the following sentences).
                    if let Some(rest) = rest {
                        stack.push(rest);
                    }
                }
            }
            // The merged length includes the whitespace between the sentences.
            Some(c) if unit.1 - c.0 <= limit => cur = Some((c.0, unit.1)),
            Some(c) => {
                out.push(c);
                cur = None;
                stack.push(unit);
            }
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out.into_iter()
        .map(|(s, e)| TextChunk { text: string_of(&u[s..e]), start: to_u32(s), end: to_u32(e) })
        .collect()
}

/// `chunk_text(text, 220, 900)`: what the engine always uses.
pub fn chunk_text_default(text: &str) -> Vec<TextChunk> {
    chunk_text(text, FIRST_CHUNK_MAX, CHUNK_MAX)
}

/// `splitSpan(text, span, limit)`: `(head, rest)` for a span longer than `limit` (tts.md §3.8). The span is clamped to
/// the text. Cuts never fall inside a surrogate pair (with `limit` ≥ 2 the original rules guarantee it; for the
/// degenerate `limit` ≤ 1 on an astral character the cut moves past the whole character).
pub fn split_span(text: &str, span: TextSpan, limit: usize) -> (TextSpan, Option<TextSpan>) {
    let u = units(text);
    let end = (span.end as usize).min(u.len());
    let start = (span.start as usize).min(end);
    let (head, rest) = split_units(&u, (start, end), limit);
    (to_span(head), rest.map(to_span))
}

fn to_u32(x: usize) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

fn to_span((s, e): (usize, usize)) -> TextSpan {
    TextSpan::new(to_u32(s), to_u32(e))
}

/// `CLAUSE_SPACED = [;:,)\]]`: a cut only when followed by whitespace.
fn is_clause_spaced(u: u16) -> bool {
    matches!(u, 0x3B | 0x3A | 0x2C | 0x29 | 0x5D)
}

/// `CLAUSE_TIGHT = [，、；：—–]`: no space needed.
fn is_clause_tight(u: u16) -> bool {
    matches!(u, 0xFF0C | 0x3001 | 0xFF1B | 0xFF1A | 0x2014 | 0x2013)
}

/// `safeCut(text, pos)`: back off one unit when `pos` would leave a lone high surrogate.
fn safe_cut(u: &[u16], pos: usize) -> usize {
    if pos > 0 && u.get(pos - 1).copied().is_some_and(is_high_surrogate) {
        pos - 1
    } else {
        pos
    }
}

/// `splitSpan` over units; `span` must lie within `u`.
fn split_units(u: &[u16], span: (usize, usize), limit: usize) -> ((usize, usize), Option<(usize, usize)>) {
    let (s, e) = span;
    // Saturating: `split_span` is public and any `limit` (up to `usize::MAX`) must work.
    let hi = e.min(s.saturating_add(limit));
    let lo = s.saturating_add((limit as f64 * 0.35).floor() as usize);

    let followed_by_space = |p: usize| u.get(p).is_some_and(|&c| is_space_unit(c));
    let clause = |p: usize| {
        let prev = u[p - 1];
        is_clause_tight(prev) || (is_clause_spaced(prev) && followed_by_space(p))
    };
    // `findCut(from, test)`: the latest `p` in `(from, hi]` passing `test`.
    let find_cut =
        |from: usize, test: &dyn Fn(usize) -> bool| -> Option<usize> { (from.saturating_add(1)..=hi).rev().find(|&p| test(p)) };

    let mut cut = find_cut(lo, &clause) // 1. latest clause boundary in the last 65 % of the window
        .or_else(|| find_cut(lo, &followed_by_space)) // 2. latest space in the last 65 %
        .or_else(|| find_cut(s, &followed_by_space)) // 3. any space
        .unwrap_or_else(|| safe_cut(u, hi)); // 4. hard cut, never inside a surrogate pair
    if cut <= s {
        cut = e.min(s.saturating_add(limit.max(1)));
    }
    // Only reachable with limit ≤ 1 on an astral character: keep the pair whole.
    if cut < e && cut > 0 && is_high_surrogate(u[cut - 1]) && is_low_surrogate(u[cut]) {
        cut += 1;
    }
    let head = trim_span(u, s, cut).unwrap_or((s, cut));
    let rest = trim_span(u, cut, e);
    (head, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::text16::{slice16, splits_pair, utf16_len};

    fn ranges(chunks: &[TextChunk]) -> Vec<(u32, u32)> {
        chunks.iter().map(|c| (c.start, c.end)).collect()
    }

    /// Checks the chunk invariants on any input: offsets index the text exactly, limits hold, pairs stay whole,
    /// chunks are ordered, non-empty and trimmed.
    fn check_invariants(text: &str, chunks: &[TextChunk], first_max: usize, max: usize) {
        let mut prev_end = 0u32;
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(slice16(text, c.start as usize, c.end as usize), c.text, "{text:?} chunk {i}");
            assert_eq!(utf16_len(&c.text), (c.end - c.start) as usize);
            let limit = if i == 0 { first_max } else { max };
            assert!((c.end - c.start) as usize <= limit, "{text:?} chunk {i} too long");
            assert!(c.end > c.start);
            assert!(c.start >= prev_end);
            assert!(!splits_pair(text, c.start as usize) && !splits_pair(text, c.end as usize));
            assert_eq!(vox_core::js::js_trim(&c.text), c.text);
            prev_end = c.end;
        }
    }

    #[test]
    fn forty_sentences() {
        let text: Vec<String> = (0..40).map(|n| format!("Sentence number {n} has a few words in it.")).collect();
        let text = text.join(" ");
        assert_eq!(utf16_len(&text), 1669);
        let chunks = chunk_text_default(&text);
        assert_eq!(ranges(&chunks), vec![(0, 204), (205, 1081), (1082, 1669)]);
        check_invariants(&text, &chunks, 220, 900);

        let text: Vec<String> = (1..=40).map(|n| format!("Sentence number {n} has a few words in it.")).collect();
        let text = text.join(" ");
        assert_eq!(utf16_len(&text), 1670);
        assert_eq!(ranges(&chunk_text_default(&text)), vec![(0, 204), (205, 1082), (1083, 1670)]);
    }

    #[test]
    fn verified_split_examples() {
        let text = format!("{}end, more words {}", "word ".repeat(100), "x".repeat(10));
        assert_eq!(ranges(&chunk_text_default(&text)), vec![(0, 219), (220, 526)]);

        let text = "alpha beta gamma, ".repeat(30);
        let chunks = chunk_text_default(&text);
        assert_eq!(ranges(&chunks), vec![(0, 215), (216, 539)]);
        assert!(chunks[0].text.ends_with("gamma,"));

        let text = "中".repeat(500);
        assert_eq!(ranges(&chunk_text_default(&text)), vec![(0, 220), (220, 500)]);

        let text = "😀".repeat(300);
        let chunks = chunk_text_default(&text);
        assert_eq!(ranges(&chunks), vec![(0, 220), (220, 600)]);
        assert_eq!(chunks[0].text.chars().count(), 110);
        assert_eq!(chunks[1].text.chars().count(), 190);

        assert!(chunk_text_default("").is_empty());
        assert!(chunk_text_default("   ").is_empty());
        assert_eq!(ranges(&chunk_text("Hi.", 5, 3)), vec![(0, 3)]);
    }

    #[test]
    fn chunk_text_merges_and_respects_limits() {
        // Sentences merge into the first chunk while it fits.
        let chunks = chunk_text_default("One. Two. Three.");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "One. Two. Three.");
        // Internal line breaks stay inside chunks; whitespace between chunks belongs to neither.
        let chunks = chunk_text("Para one is here.\n\nPara two is here.", 20, 20);
        assert_eq!(ranges(&chunks), vec![(0, 17), (19, 36)]);
        // The limits are raised: first ≥ 20, max ≥ first.
        let text = "a".repeat(30);
        let chunks = chunk_text(&text, 0, 0);
        assert_eq!(ranges(&chunks), vec![(0, 20), (20, 30)]);
    }

    #[test]
    fn split_span_rules() {
        // Clause cut wins over a later space.
        let text = "aaaa, bbbb cccc";
        let (head, rest) = split_span(text, TextSpan::new(0, 15), 12);
        assert_eq!((head, rest), (TextSpan::new(0, 5), Some(TextSpan::new(6, 15))));
        // Tight clause marks need no space.
        let text = "aaaa—bbbbcccc";
        let (head, rest) = split_span(text, TextSpan::new(0, 13), 8);
        assert_eq!((head, rest), (TextSpan::new(0, 5), Some(TextSpan::new(5, 13))));
        // A `)` or `]` only with a following space.
        let text = "aa)bbbbbbbb cc";
        let (head, _) = split_span(text, TextSpan::new(0, 14), 12);
        assert_eq!(head, TextSpan::new(0, 11));
        // Hard cut never splits a pair.
        let text = "😀😀😀";
        let (head, rest) = split_span(text, TextSpan::new(0, 6), 3);
        assert_eq!((head, rest), (TextSpan::new(0, 2), Some(TextSpan::new(2, 6))));
        // Degenerate limits keep pairs whole and always make progress.
        let (head, rest) = split_span(text, TextSpan::new(0, 6), 1);
        assert_eq!((head, rest), (TextSpan::new(0, 2), Some(TextSpan::new(2, 6))));
        let (head, rest) = split_span("abc", TextSpan::new(0, 3), 0);
        assert_eq!((head, rest), (TextSpan::new(0, 1), Some(TextSpan::new(1, 3))));
        // Out-of-range spans are clamped.
        let (head, rest) = split_span("abc def", TextSpan::new(2, 99), 3);
        assert_eq!((head, rest), (TextSpan::new(2, 3), Some(TextSpan::new(4, 7))));
    }

    /// Regression: `s + limit` overflowed (a panic in debug builds, an inverted head span in release).
    #[test]
    fn split_span_huge_limits() {
        let text = "Hello world, this is a test. 😀 Another sentence here; with clauses.";
        let len = utf16_len(text) as u32;
        for limit in [usize::MAX, usize::MAX - 5, usize::MAX / 2, usize::MAX / 3 + 1] {
            for (s, e) in [(0, len), (1, 11), (10, len), (len - 1, len), (len, len)] {
                let (head, rest) = split_span(text, TextSpan::new(s, e), limit);
                assert!(s <= head.start && head.start <= head.end && head.end <= e, "{limit} ({s}, {e}) → {head:?}");
                if let Some(rest) = rest {
                    assert!(head.end <= rest.start && rest.start < rest.end && rest.end <= e);
                }
                assert!(!splits_pair(text, head.start as usize) && !splits_pair(text, head.end as usize));
            }
        }
        assert_eq!(
            split_span("abc def ghi", TextSpan::new(1, 11), usize::MAX),
            (TextSpan::new(1, 7), Some(TextSpan::new(8, 11)))
        );
    }

    /// A small deterministic generator (xorshift) for the property loops.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const PIECES: &[&str] = &[
        "word", "Word", " ", " ", " ", "  ", ".", ". ", "! ", "? ", ",", ", ", ";", ": ", "—", "–", "\n", "\n\n", "😀",
        "中", "。", "，", "Dr.", "e.g.", "Fig.", "3", "\"", ")", "] ", "\u{A0}", "\u{3000}", "é", "𝐀", "x",
    ];

    #[test]
    fn property_random_texts() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..400 {
            let pieces = 1 + rng.below(if round % 10 == 0 { 2000 } else { 300 });
            let mut text = String::new();
            for _ in 0..pieces {
                text.push_str(PIECES[rng.below(PIECES.len())]);
            }
            let chunks = chunk_text_default(&text);
            check_invariants(&text, &chunks, 220, 900);
            // Every non-whitespace unit is covered by exactly one chunk.
            let covered: usize = chunks.iter().map(|c| c.text.chars().filter(|c| !vox_core::js::is_js_whitespace(*c)).count()).sum();
            let expected = text.chars().filter(|c| !vox_core::js::is_js_whitespace(*c)).count();
            assert_eq!(covered, expected, "{text:?}");
            let small = chunk_text(&text, 20, 40);
            check_invariants(&text, &small, 20, 40);
        }
    }
}
