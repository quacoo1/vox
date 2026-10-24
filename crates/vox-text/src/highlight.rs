//! The widget's sentence-with-word-highlighting view (reader-playback.md §8.7, ui-settings-widget.md §6.10): only the
//! sentence containing `charIndex` is shown, as its `\S+` words joined by single spaces, split into spoken / current
//! / upcoming. Long sentences are windowed so the current word stays within the two visible lines (decision: a
//! character window approximates the original's line scrolling).
//!
//! Windowing model: the words are laid out greedily on lines of `window_units / 2` UTF-16 units (a word longer than
//! a line wraps anywhere, like the original's `overflow-wrap: anywhere`). The view starts at the line holding the
//! current word, or at the second-to-last line when that is earlier — the original's
//! `translateY(-min(currentWord.offsetTop, scrollHeight - viewportHeight))` — so the box scrolls line by line, not
//! word by word. Words before that line are replaced by a leading `…`; words more than four model lines below it are
//! not returned (they could never be visible in the two-line box, and the UI lays the strings out every frame).

use vox_core::text16::Utf16Index;
use vox_core::timing::{last_index_at_or_before, sentence_at};

use crate::util::is_ws;

/// Look of one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordState {
    Spoken,
    Current,
    Upcoming,
}

/// What the widget body shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SentenceView {
    /// Words before the current one, joined by single spaces (empty when none).
    pub spoken: String,
    /// The current word (empty before playback starts: every word is upcoming).
    pub current: String,
    /// Words after the current one.
    pub upcoming: String,
    /// The sentence index (a change triggers the "new sentence" fade).
    pub sentence_index: usize,
    /// True when `spoken` was shortened at its start (a leading `…` is part of `spoken`).
    pub clipped_start: bool,
}

impl SentenceView {
    /// The look of word `i` of the displayed words (`spoken` words, then `current`, then `upcoming`), e.g. for an
    /// accessible description. The leading `…` of a clipped view is not a word.
    pub fn word_state(&self, i: usize) -> WordState {
        let tokens = self.spoken.split(' ').filter(|w| !w.is_empty()).count();
        let spoken = tokens.saturating_sub(usize::from(self.clipped_start));
        if i < spoken {
            WordState::Spoken
        } else if i == spoken && !self.current.is_empty() {
            WordState::Current
        } else {
            WordState::Upcoming
        }
    }
}

/// Model lines below the top visible line that are still returned (the box shows two).
const LINES_AFTER_TOP: usize = 3;

/// Builds the view. `index` must be built from `text`; `sentences` are the session's sentence starts; `started` =
/// `positionSec > 0 || status == playing`; `window_units` caps the visible characters (≈ two lines; the UI passes
/// ~96 for the full card and ~40 for the compact pill): words before the current one are dropped from the start
/// (with a leading `…`) so the current word stays visible. `window_units == 0` disables windowing.
pub fn sentence_view(
    text: &str,
    index: &Utf16Index,
    sentences: &[u32],
    char_index: u32,
    started: bool,
    window_units: usize,
) -> SentenceView {
    // An index built from another string must not make us slice off a char boundary.
    let rebuilt;
    let index = if index_matches(text, index) {
        index
    } else {
        rebuilt = Utf16Index::new(text);
        &rebuilt
    };
    let len16 = u32::try_from(index.len16()).unwrap_or(u32::MAX);
    let (sentence_index, start, end) = sentence_at(sentences, len16, char_index);
    let words = sentence_words(text, index, start as usize, end as usize);
    let starts: Vec<u32> = words.iter().map(|w| w.0).collect();
    let current = if started { last_index_at_or_before(&starts, char_index) } else { None };

    let (first, last, clipped) = window(&words, current, window_units);
    let join = |range: std::ops::Range<usize>| -> String {
        words[range].iter().map(|w| w.2).collect::<Vec<_>>().join(" ")
    };

    let mut view = SentenceView { sentence_index, clipped_start: clipped, ..SentenceView::default() };
    match current {
        Some(c) => {
            let before = join(first..c);
            view.spoken = match (clipped, before.is_empty()) {
                (true, true) => "…".to_owned(),
                (true, false) => format!("… {before}"),
                (false, _) => before,
            };
            view.current = words[c].2.to_owned();
            view.upcoming = join((c + 1).min(last)..last);
        }
        None => view.upcoming = join(first..last),
    }
    view
}

/// A cheap plausibility check (O(log n)); the slicing below re-checks char boundaries, so an index from another
/// string of the same byte length can give odd words but never a panic.
fn index_matches(text: &str, index: &Utf16Index) -> bool {
    index.byte_of(index.len16()) == text.len()
}

/// `(start16, len16, word)` for every `\S+` run of `text[start..end)` (UTF-16 offsets).
fn sentence_words<'a>(text: &'a str, index: &Utf16Index, start: usize, end: usize) -> Vec<(u32, usize, &'a str)> {
    let a = index.byte_of(start).min(text.len());
    let b = index.byte_of(end).min(text.len());
    if a >= b || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
        return Vec::new();
    }
    let slice = &text[a..b];
    let mut out = Vec::new();
    let mut off16 = index.utf16_of(a);
    let mut word: Option<(usize, usize, usize)> = None; // (byte start in slice, start16, len16)
    for (bi, c) in slice.char_indices() {
        if is_ws(c) {
            if let Some((ws, s16, l16)) = word.take() {
                out.push((u32::try_from(s16).unwrap_or(u32::MAX), l16, &slice[ws..bi]));
            }
        } else {
            match word.as_mut() {
                Some(w) => w.2 += c.len_utf16(),
                None => word = Some((bi, off16, c.len_utf16())),
            }
        }
        off16 += c.len_utf16();
    }
    if let Some((ws, s16, l16)) = word {
        out.push((u32::try_from(s16).unwrap_or(u32::MAX), l16, &slice[ws..]));
    }
    out
}

/// `(first, last, clipped)`: the words to return, `[first, last)`.
fn window(words: &[(u32, usize, &str)], current: Option<usize>, window_units: usize) -> (usize, usize, bool) {
    let n = words.len();
    if window_units == 0 || n == 0 {
        return (0, n, false);
    }
    let width = (window_units / 2).max(1);
    // Greedy layout: (first line, last line) of every word.
    let mut lines = Vec::with_capacity(n);
    let (mut line, mut col) = (0usize, 0usize);
    for &(_, len, _) in words {
        let len = len.max(1);
        if col > 0 && col + 1 + len > width {
            line += 1;
            col = 0;
        }
        let first_line = line;
        if col > 0 {
            col += 1;
        }
        if col + len <= width {
            col += len;
        } else {
            // A word longer than a line (col is 0 here) wraps anywhere.
            let extra = (len - 1) / width;
            line += extra;
            col = len - extra * width;
        }
        lines.push((first_line, line));
    }
    let total_lines = line + 1;
    if total_lines <= 2 {
        return (0, n, false);
    }
    let current_line = current.map_or(0, |c| lines[c].0);
    let top = current_line.min(total_lines - 2);
    let first = lines.iter().position(|&(_, l)| l >= top).unwrap_or(0);
    let last = lines.iter().rposition(|&(f, _)| f <= top + LINES_AFTER_TOP).map_or(n, |i| i + 1);
    let last = current.map_or(last, |c| last.max(c + 1));
    (first, last, first > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(text: &str, sentences: &[u32], char_index: u32, started: bool, window: usize) -> SentenceView {
        sentence_view(text, &Utf16Index::new(text), sentences, char_index, started, window)
    }

    #[test]
    fn spoken_current_upcoming() {
        let text = "First sentence here. Second  one\nhas words.";
        let sentences = crate::segment::sentence_starts(text);
        assert_eq!(sentences, vec![0, 21, 33]);
        let v = view(text, &sentences, 7, true, 0);
        assert_eq!(
            v,
            SentenceView {
                spoken: "First".into(),
                current: "sentence".into(),
                upcoming: "here.".into(),
                sentence_index: 0,
                clipped_start: false,
            }
        );
        // Whitespace inside a word's range is not part of a word: the word starting at or before charIndex wins.
        let v = view(text, &sentences, 27, true, 0);
        assert_eq!((v.spoken.as_str(), v.current.as_str(), v.upcoming.as_str()), ("", "Second", "one"));
        assert_eq!(v.sentence_index, 1);
        let v = view(text, &sentences, 29, true, 0);
        assert_eq!((v.spoken.as_str(), v.current.as_str(), v.upcoming.as_str()), ("Second", "one", ""));
        // A line break ends the sentence; the last sentence runs to the end of the text.
        let v = view(text, &sentences, 40, true, 0);
        assert_eq!((v.spoken.as_str(), v.current.as_str(), v.upcoming.as_str()), ("has", "words.", ""));
        assert_eq!(v.sentence_index, 2);
    }

    #[test]
    fn not_started_means_all_upcoming() {
        let text = "One two three. Four.";
        let v = view(text, &[0, 15], 9, false, 0);
        assert_eq!(v.spoken, "");
        assert_eq!(v.current, "");
        assert_eq!(v.upcoming, "One two three.");
        assert_eq!(v.word_state(0), WordState::Upcoming);
        // Started at position 0: the first word is current.
        let v = view(text, &[0, 15], 0, true, 0);
        assert_eq!((v.spoken.as_str(), v.current.as_str()), ("", "One"));
        assert_eq!(v.word_state(0), WordState::Current);
        assert_eq!(v.word_state(1), WordState::Upcoming);
    }

    #[test]
    fn utf16_offsets_and_edges() {
        let text = "😀 ab cd";
        // "😀" = 0..2, "ab" = 3..5, "cd" = 6..8.
        let v = view(text, &[0], 6, true, 0);
        assert_eq!((v.spoken.as_str(), v.current.as_str()), ("😀 ab", "cd"));
        assert_eq!(v.word_state(0), WordState::Spoken);
        assert_eq!(v.word_state(1), WordState::Spoken);
        assert_eq!(v.word_state(2), WordState::Current);
        // Before the first word of the sentence (leading whitespace): nothing is current yet.
        let v = view("  hi there", &[0], 1, true, 0);
        assert_eq!((v.spoken.as_str(), v.current.as_str(), v.upcoming.as_str()), ("", "", "hi there"));
        // Empty text, empty sentence list, out-of-range char index.
        assert_eq!(view("", &[], 0, true, 96), SentenceView::default());
        let v = view("abc def", &[], 99, true, 96);
        assert_eq!((v.spoken.as_str(), v.current.as_str()), ("abc", "def"));
        // An index built from another string does not panic.
        let other = Utf16Index::new("😀😀😀😀 é");
        let v = sentence_view("plain ascii", &other, &[0], 6, true, 0);
        assert_eq!((v.spoken.as_str(), v.current.as_str()), ("plain", "ascii"));
    }

    #[test]
    fn long_sentences_are_windowed() {
        // 30 words of 9 units ("wordNN.." style) → 300 units in one sentence; window 40 → 20-unit lines (2 words).
        let words: Vec<String> = (0..30).map(|i| format!("word{i:02}xyz")).collect();
        let text = words.join(" ");
        let starts: Vec<u32> = (0..30).map(|i| i * 10).collect();

        // Current word in the first lines: no clipping, upcoming capped after four model lines.
        let v = view(&text, &[0], starts[1], true, 40);
        assert!(!v.clipped_start);
        assert_eq!(v.spoken, "word00xyz");
        assert_eq!(v.current, "word01xyz");
        assert_eq!(v.upcoming, words[2..8].join(" "));

        // Current word on model line 5 (words 10, 11): the view starts at that line with a leading ellipsis.
        let v = view(&text, &[0], starts[11], true, 40);
        assert!(v.clipped_start);
        assert_eq!(v.spoken, "… word10xyz");
        assert_eq!(v.current, "word11xyz");
        assert_eq!(v.upcoming, words[12..18].join(" "));
        let v = view(&text, &[0], starts[10], true, 40);
        assert_eq!(v.spoken, "…");
        assert_eq!(v.current, "word10xyz");
        assert_eq!(v.word_state(0), WordState::Current);

        // Near the end the last two lines stay visible (no scrolling past them).
        let v = view(&text, &[0], starts[29], true, 40);
        assert!(v.clipped_start);
        assert_eq!(v.spoken, format!("… {}", words[26..29].join(" ")));
        assert_eq!(v.current, "word29xyz");
        assert_eq!(v.upcoming, "");

        // A sentence that fits in two lines is never clipped.
        let short = "a b c d e";
        let v = view(short, &[0], 8, true, 40);
        assert!(!v.clipped_start);
        assert_eq!(v.spoken, "a b c d");

        // Not started: the view starts at the top.
        let v = view(&text, &[0], starts[20], false, 40);
        assert!(!v.clipped_start);
        assert!(v.upcoming.starts_with("word00xyz"));
    }

    #[test]
    fn long_words_wrap_anywhere() {
        let long = "x".repeat(100);
        let text = format!("start {long} end tail");
        // width 10: "start" line 0, the 100-unit word lines 1..=10, "end tail" line 11.
        let v = view(&text, &[0], 107, true, 20);
        assert_eq!(v.current, "end");
        assert!(v.clipped_start);
        assert_eq!(v.spoken, format!("… {long}"));
        // The long word starts on line 1, so line 0 ("start") scrolls away.
        let v = view(&text, &[0], 6, true, 20);
        assert_eq!(v.current, long);
        assert_eq!(v.spoken, "…");
        // "end tail" sits on model line 11, far below the visible lines 1–2 of the long word.
        assert_eq!(v.upcoming, "");
    }
}
