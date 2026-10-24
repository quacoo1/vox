//! # vox-text
//!
//! Pure text algorithms (tts.md §3, reader-playback.md §2.4/§5, integrations.md §5). Every public offset and length is
//! in **UTF-16 code units** (see `vox_core::text16`); inputs and outputs are UTF-8 `&str`/`String`. Nothing here
//! panics on any input, does I/O (except [`read_text_file`]) or logs.
//!
//! | Module | Spec |
//! |---|---|
//! | [`clean`] | tts.md §3.1–§3.6 (`cleanText`) + ARCHITECTURE.md §8 D-TXT-1..4 |
//! | [`segment`] | tts.md §3.7 (`sentenceSpans`), reader-playback.md §5 (`sentenceStarts`, `softSplits`, `wordSpans`) |
//! | [`chunk`] | tts.md §3.8 (`chunkText`, `splitSpan`) |
//! | [`reader`] | reader-playback.md §2.4 (`preCut`, `truncateText`, `previewTitle`), tts.md §4.5 (`contextBefore/After`) |
//! | [`highlight`] | reader-playback.md §8.7, ui-settings-widget.md §6.10 (the widget's word highlighting) |
//! | [`extract`] | integrations.md §5 (`readTextFile` and its decoders / converters) + D-INT-4 |
//!
//! Golden vectors from the specs (tts.md §3.6/§3.7/§3.8/§4.5, reader-playback.md §2.4/§5, integrations.md §5.10) are
//! unit tests in each module. Data: `data/en-words.txt` (SCOWL-derived word list for D-TXT-2; licence in its header,
//! which [`clean::word_list_notice`] returns for shipping with the binary).

pub mod chunk;
pub mod clean;
pub mod extract;
pub mod highlight;
pub mod reader;
pub mod segment;
mod util;

pub use chunk::{chunk_text, chunk_text_default};
pub use clean::{clean_text, CleanOptions};
pub use extract::{read_text_file, ExtractError};
pub use highlight::{sentence_view, SentenceView, WordState};
pub use reader::{context_after, context_before, pre_cut, preview_title, truncate_text};
pub use segment::{sentence_spans, sentence_starts, word_spans};

#[cfg(test)]
mod fuzz {
    //! The no-panic loop of ARCHITECTURE.md §9: every public function over random UTF-8 strings biased towards
    //! UTF-16 edge cases (astral characters, offsets inside surrogate pairs, BOMs, lone-surrogate-looking bytes)
    //! and over random bytes for the decoders.

    use std::time::Instant;

    use vox_core::text16::{splits_pair, utf16_len, Utf16Index};
    use vox_core::tts::TextSpan;

    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    const PIECES: &[&str] = &[
        "a", "Word", "x", " ", " ", "\n", "\r\n", "\t", ".", ". ", "! ", "? ", "...", ",", ";", ":", "—", "–", "-",
        "😀", "𝐀", "中", "。", "、", "é", "\u{A0}", "\u{3000}", "\u{FEFF}", "\u{200B}", "\u{2028}", "\u{2029}",
        "\u{85}", "\u{E000}", "\u{0}", "\u{FFFD}", "<", ">", "</", "<p>", "<br>", "<script>", "<!--", "-->", "\"", "'",
        "=", "&", "&amp;", "&#x1F600;", "&#9999999;", "{", "}", "\\", "\\'", "\\u", "\\u-", "\\uc0", "\\bin", "\\par ",
        "\\*", "{\\rtf1 ", "\\'e9", "#", "## ", "* ", "**", "_", "__", "~~", "`", "```", "[", "](", ")", "![",
        "https://", "www.", "@", "foo@bar.com", "Dr. ", "e.g. ", "Fig. 3", "1. ", "a) ",
    ];

    fn random_text(rng: &mut Rng, max_pieces: usize) -> String {
        let mut s = String::new();
        for _ in 0..rng.below(max_pieces) + 1 {
            if rng.below(6) == 0 {
                let cp = match rng.below(4) {
                    0 => rng.below(0x80) as u32,
                    1 => rng.below(0x800) as u32,
                    2 => rng.below(0x1_0000) as u32,
                    _ => rng.below(0x11_0000) as u32,
                };
                if let Some(c) = char::from_u32(cp) {
                    s.push(c);
                }
            } else {
                s.push_str(PIECES[rng.below(PIECES.len())]);
            }
        }
        s
    }

    #[test]
    fn no_panics_anywhere() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for round in 0..1200 {
            let text = random_text(&mut rng, if round % 40 == 0 { 600 } else { 80 });
            let len = utf16_len(&text);
            let a = rng.below(len + 3);
            let b = rng.below(len + 3);
            let (a32, b32) = (a as u32, b as u32);

            for opts in [CleanOptions { skip_urls: true, skip_emails: true }, CleanOptions::default()] {
                let _ = clean_text(&text, opts);
            }
            let _ = segment::fallback_segments(&text);
            for s in sentence_spans(&text) {
                assert!(s.start < s.end && (s.end as usize) <= len);
                assert!(!splits_pair(&text, s.start as usize) && !splits_pair(&text, s.end as usize));
            }
            let starts = sentence_starts(&text);
            assert!(!starts.is_empty() && starts.windows(2).all(|w| w[0] < w[1]));
            let _ = segment::soft_splits(&text, a32, b32);
            for w in word_spans(&text, a32, b32) {
                assert!(w.start < w.end && (w.end as usize) <= len);
            }
            for c in chunk_text(&text, a, b) {
                assert_eq!(vox_core::text16::slice16(&text, c.start as usize, c.end as usize), c.text);
            }
            let limit = match round % 4 {
                0 => usize::MAX - rng.below(100),
                1 => usize::MAX / 2 + rng.below(3),
                _ => rng.below(30),
            };
            let span = TextSpan::new(a32.min(b32), a32.max(b32));
            let (head, rest) = chunk::split_span(&text, span, limit);
            assert!(head.start <= head.end && rest.is_none_or(|r| head.end <= r.start && r.start <= r.end));

            for (out, cut) in [pre_cut(&text, a), truncate_text(&text, a)] {
                assert!(utf16_len(&out) <= len.max(a));
                assert!(!cut || utf16_len(&out) <= a);
            }
            assert!(utf16_len(&preview_title(&text, a)) <= a.max(len) + 1);
            let _ = context_before(Some(&text));
            let _ = context_after(Some(&text));

            let index = Utf16Index::new(&text);
            for window in [0, 1, 40, 96] {
                let _ = sentence_view(&text, &index, &starts, a32, round % 2 == 0, window);
            }
            let _ = sentence_view(&text, &index, &[b32, a32], a32, true, 7);

            let _ = extract::html_to_text(&text);
            let _ = extract::rtf_to_text(&text);
            let _ = extract::markdown_to_text(&text);
            let _ = extract::looks_binary(&text);
            let truncated = extract::truncate_extracted(&text, a);
            assert!(a == 0 || utf16_len(&truncated) <= a.max(1));

            let bytes: Vec<u8> = (0..rng.below(64)).map(|_| rng.next() as u8).collect();
            let _ = extract::decode_text_buffer(&bytes, round % 3 == 0);
            let _ = extract::decode_text_buffer(text.as_bytes(), true);
        }
    }

    /// Every public function taking a number, with extreme values (overflow would panic in test builds).
    #[test]
    fn extreme_numeric_arguments() {
        let texts = ["", "a", "😀", "Hello world, this is a test. 😀 Another sentence here; with clauses. Dr. Who."];
        let sizes = [0, 1, usize::MAX / 3 + 1, usize::MAX / 2, usize::MAX / 2 + 1, usize::MAX - 1, usize::MAX];
        let offsets = [0, 1, u32::MAX / 2, u32::MAX / 2 + 1, u32::MAX - 1, u32::MAX];
        for text in texts {
            let index = Utf16Index::new(text);
            let starts = sentence_starts(text);
            for &n in &sizes {
                let _ = chunk_text(text, n, n);
                let _ = chunk_text(text, n, 0);
                let _ = chunk_text(text, 0, n);
                let _ = pre_cut(text, n);
                let _ = truncate_text(text, n);
                let _ = preview_title(text, n);
                let _ = extract::truncate_extracted(text, n);
                for &o in &offsets {
                    let (head, rest) = chunk::split_span(text, TextSpan::new(o.min(3), o), n);
                    assert!(head.start <= head.end && rest.is_none_or(|r| head.end <= r.start));
                    let view = sentence_view(text, &index, &starts, o, true, n);
                    let _ = view.word_state(n);
                    let _ = sentence_view(text, &index, &[o, 0, u32::MAX], o, false, n);
                }
            }
            for &a in &offsets {
                for &b in &offsets {
                    let _ = segment::soft_splits(text, a, b);
                    let _ = word_spans(text, a, b);
                }
            }
        }
    }

    /// Pathological inputs stay linear (a quadratic scan would not finish in debug builds).
    #[test]
    fn large_inputs_stay_fast() {
        let mut big = String::new();
        let mut rng = Rng(7);
        while big.len() < 300_000 {
            big.push_str(&random_text(&mut rng, 40));
            big.push_str(" Plain prose sentence number ");
        }
        let started = Instant::now();
        for text in [
            big.clone(),
            " **a".repeat(60_000),
            " *a".repeat(60_000),
            "(_a".repeat(60_000),
            "~~a ".repeat(60_000),
            "a".repeat(250_000),
            format!("**{}", "x ".repeat(120_000)),
            "https://x.com/".repeat(20_000),
            "a@".repeat(100_000),
            // Glued addresses: each rejected start skips to its `@` (EMAIL_RE's look-behind).
            "a@b.cc1".repeat(40_000),
            format!("x@y.com-{}@b.com ", "a".repeat(100_000)).repeat(2),
            "x@y.com-".repeat(40_000),
            "word-\n".repeat(40_000),
            "> ".repeat(100_000),
            "<head>x".repeat(40_000),
            "<img alt=\"a".repeat(20_000),
            "{\\rtf1 \\u8212".repeat(20_000),
            "*a_".repeat(60_000),
        ] {
            let cleaned = clean_text(&text, CleanOptions { skip_urls: true, skip_emails: true });
            let _ = chunk_text_default(&cleaned);
            let _ = sentence_starts(&cleaned);
            let _ = extract::markdown_to_text(&text);
            let _ = extract::html_to_text(&text);
            let _ = extract::rtf_to_text(&text);
        }
        // Generous: debug builds take a few seconds; a quadratic path would take minutes.
        assert!(started.elapsed().as_secs() < 120, "took {:?}", started.elapsed());
    }
}
