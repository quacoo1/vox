//! The reader's text helpers (reader-playback.md §2.4) and the engine's context windows (tts.md §4.5). All lengths
//! are UTF-16 units; cuts never leave a lone high surrogate (D-C-17: where the original could, the dangling unit is
//! dropped, which may make a result one unit shorter than the old app's).

use vox_core::constants::CONTEXT_CHARS;
use vox_core::js::{collapse_whitespace, js_trim, js_trim_end};

use crate::util::{is_high_surrogate, is_low_surrogate, is_space_unit, string_of, units};

/// The units `[0, end)` as a string, dropping a dangling high surrogate at the cut.
fn head_string(u: &[u16], end: usize) -> String {
    let mut end = end.min(u.len());
    if end > 0 && is_high_surrogate(u[end - 1]) {
        end -= 1;
    }
    string_of(&u[..end])
}

/// Index of the match of `/\s\S*$/` in `u` (the last whitespace unit), if any.
fn last_space(u: &[u16]) -> Option<usize> {
    u.iter().rposition(|&c| is_space_unit(c))
}

/// `preCut(text, max)`: cut unbounded raw input before cleaning — at `max`, backed up to the last `\n` if it lies in
/// the final tenth; never ends with a lone high surrogate; not trimmed. Returns `(text, was_cut)`.
pub fn pre_cut(text: &str, max: usize) -> (String, bool) {
    let u = units(text);
    if u.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    if let Some(nl) = u[..max].iter().rposition(|&c| c == u16::from(b'\n')) {
        if nl as f64 > max as f64 * 0.9 {
            end = nl;
        }
    }
    (head_string(&u, end), true)
}

/// `truncateText(text, max)`: final cut to `maxChars` — back up to the last whitespace only if it lies in the final
/// 10 % (`> max × 0.9`), never split a surrogate pair, `trimEnd`. Returns `(text, was_truncated)`.
pub fn truncate_text(text: &str, max: usize) -> (String, bool) {
    let u = units(text);
    if u.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    if let Some(sp) = last_space(&u[..max]) {
        if sp as f64 > max as f64 * 0.9 {
            end = sp;
        }
    }
    (js_trim_end(&head_string(&u, end)).to_owned(), true)
}

/// `previewTitle(text, max = 64)`: whitespace collapsed; longer than `max` → cut at the last space if it lies beyond
/// `max × 0.6`, strip trailing `[\s,.;:\-–—]`, append `…`.
pub fn preview_title(text: &str, max: usize) -> String {
    let flat = collapse_whitespace(text);
    let u = units(&flat);
    if u.len() <= max {
        return flat;
    }
    let cut = &u[..max];
    let end = match cut.iter().rposition(|&c| c == u16::from(b' ')) {
        Some(space) if space as f64 > max as f64 * 0.6 => space,
        _ => max,
    };
    let base = head_string(&u, end);
    let base = base.trim_end_matches(|c: char| {
        crate::util::is_ws(c) || matches!(c, ',' | '.' | ';' | ':' | '-' | '\u{2013}' | '\u{2014}')
    });
    format!("{base}…")
}

/// `contextBefore(prevChunkText)`: up to ~300 units from the end of the previous chunk, starting at a word boundary
/// (the first whitespace within the first 60 units of the 300-unit tail), trimmed; `None` if empty. A text of at most
/// 300 units is passed through unchanged (untrimmed, like the original).
pub fn context_before(prev: Option<&str>) -> Option<String> {
    let t = prev.filter(|t| !t.is_empty())?;
    let u = units(t);
    if u.len() <= CONTEXT_CHARS {
        return Some(t.to_owned());
    }
    let tail_start = u.len() - CONTEXT_CHARS;
    let tail = &u[tail_start..];
    // Indices are relative to the 300-unit tail exactly as in the original; only the output skips a lone low
    // surrogate when the tail starts inside a pair.
    let from = match tail.iter().position(|&c| is_space_unit(c)) {
        Some(sp) if sp < 60 => tail_start + sp + 1,
        _ => tail_start,
    };
    let from = if from < u.len() && is_low_surrogate(u[from]) { from + 1 } else { from };
    let r = string_of(&u[from..]);
    let r = js_trim(&r);
    (!r.is_empty()).then(|| r.to_owned())
}

/// `contextAfter(nextChunkText)`: up to ~300 units from the start of the next chunk, ending at a word boundary (the
/// last whitespace after unit 240 of the 300-unit head), trimmed; `None` if empty. A text of at most 300 units is
/// passed through unchanged.
pub fn context_after(next: Option<&str>) -> Option<String> {
    let t = next.filter(|t| !t.is_empty())?;
    let u = units(t);
    if u.len() <= CONTEXT_CHARS {
        return Some(t.to_owned());
    }
    let head = &u[..CONTEXT_CHARS];
    let end = match last_space(head) {
        Some(sp) if sp > CONTEXT_CHARS - 60 => sp,
        _ => CONTEXT_CHARS,
    };
    let r = head_string(&u, end);
    let r = js_trim(&r);
    (!r.is_empty()).then(|| r.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::constants::TITLE_CHARS;
    use vox_core::text16::utf16_len;

    #[test]
    fn pre_cut_rules() {
        assert_eq!(pre_cut("short", 10), ("short".to_owned(), false));
        assert_eq!(pre_cut("exactly10!", 10), ("exactly10!".to_owned(), false));
        // A line break in the final tenth (index > 0.9 × max) is preferred.
        let text = format!("{}\n{}", "a".repeat(95), "b".repeat(20));
        assert_eq!(pre_cut(&text, 100), ("a".repeat(95), true));
        // …but not one earlier than that.
        let text = format!("{}\n{}", "a".repeat(90), "b".repeat(20));
        assert_eq!(pre_cut(&text, 100), (format!("{}\n{}", "a".repeat(90), "b".repeat(9)), true));
        // Never ends with a lone high surrogate; no trimming.
        let text = format!("{}😀tail", "x".repeat(9));
        assert_eq!(pre_cut(&text, 10), ("x".repeat(9), true));
        assert_eq!(pre_cut("ab  cd", 4), ("ab  ".to_owned(), true));
        assert_eq!(pre_cut("abc", 0), (String::new(), true));
    }

    #[test]
    fn truncate_text_rules() {
        assert_eq!(truncate_text("hello", 5), ("hello".to_owned(), false));
        // Last whitespace in the final 10 % → cut there, then trimEnd.
        let text = format!("{} {}", "a".repeat(95), "b".repeat(20));
        assert_eq!(truncate_text(&text, 100), ("a".repeat(95), true));
        // Whitespace earlier than 90 % → hard cut.
        let text = format!("{} {}", "a".repeat(80), "b".repeat(40));
        assert_eq!(truncate_text(&text, 100), (format!("{} {}", "a".repeat(80), "b".repeat(19)), true));
        // Never splits a surrogate pair.
        let text = format!("{}😀😀", "x".repeat(9));
        assert_eq!(truncate_text(&text, 10), ("x".repeat(9), true));
        // trimEnd after a hard cut.
        assert_eq!(truncate_text("abc   def", 5), ("abc".to_owned(), true));
        assert_eq!(truncate_text("abc", 0), (String::new(), true));
        // The space must be strictly beyond 0.9 × max (18 is not > 18.0; 19 is).
        let text = format!("{} {}", "a".repeat(18), "b".repeat(10));
        assert_eq!(truncate_text(&text, 20), (format!("{} b", "a".repeat(18)), true));
        let text = format!("{} {}", "a".repeat(19), "b".repeat(10));
        assert_eq!(truncate_text(&text, 20), ("a".repeat(19), true));
    }

    #[test]
    fn preview_title_rules() {
        assert_eq!(preview_title("  Hello \n\t world  ", TITLE_CHARS), "Hello world");
        assert_eq!(preview_title("", TITLE_CHARS), "");
        // Cut at the last space beyond 0.6 × max, trailing punctuation stripped.
        let text = "The quick brown fox jumps over the lazy dog, again and again, until the end of time.";
        assert_eq!(preview_title(text, TITLE_CHARS), "The quick brown fox jumps over the lazy dog, again and again…");
        // No space beyond 38.4 → hard cut at 64.
        let text = format!("{} {}", "a".repeat(10), "b".repeat(80));
        assert_eq!(preview_title(&text, 64), format!("{} {}…", "a".repeat(10), "b".repeat(53)));
        // All punctuation past the cut → just the ellipsis.
        assert_eq!(preview_title(&".".repeat(80), 64), "…");
        assert_eq!(preview_title("one two three — four", 16), "one two three…");
        // Exactly max units is kept whole.
        assert_eq!(preview_title(&"x".repeat(64), 64), "x".repeat(64));
        // A pair at the cut is dropped, never split.
        let text = format!("{}😀{}", "y".repeat(63), "z".repeat(5));
        assert_eq!(preview_title(&text, 64), format!("{}…", "y".repeat(63)));
    }

    #[test]
    fn context_vectors() {
        // tts.md §4.5 verified vectors.
        let prev = format!("{} {}", "a".repeat(250), "b".repeat(100));
        assert_eq!(context_before(Some(&prev)), Some(format!("{} {}", "a".repeat(199), "b".repeat(100))));
        let next = format!("{} {}", "c".repeat(100), "d".repeat(250));
        let head: String = next.chars().take(300).collect();
        assert_eq!(context_after(Some(&next)), Some(head));

        // ≤ 300 units pass through unchanged (not even trimmed).
        assert_eq!(context_before(Some("  short  ")), Some("  short  ".to_owned()));
        assert_eq!(context_after(Some(" x ")), Some(" x ".to_owned()));
        let exact = "e".repeat(300);
        assert_eq!(context_before(Some(&exact)), Some(exact.clone()));
        assert_eq!(context_before(None), None);
        assert_eq!(context_before(Some("")), None);
        assert_eq!(context_after(None), None);
        assert_eq!(context_after(Some("")), None);

        // Word boundary: the first whitespace within the first 60 units of the tail.
        let prev = format!("{} {} {}", "x".repeat(100), "y".repeat(10), "z".repeat(250));
        assert_eq!(utf16_len(&prev), 362);
        // tail = prev[62..]: "x"×38 + " " + "y"×10 + " " + "z"×250 → first space at 38 < 60 → skip "x"×38 + " ".
        assert_eq!(context_before(Some(&prev)), Some(format!("{} {}", "y".repeat(10), "z".repeat(250))));
        // The last whitespace after unit 240 of the head ends contextAfter.
        let next = format!("{} {}", "p".repeat(250), "q".repeat(100));
        assert_eq!(context_after(Some(&next)), Some("p".repeat(250)));
        // Whitespace-only windows → None.
        let blank = format!("{}{}", "w".repeat(10), " ".repeat(400));
        assert_eq!(context_before(Some(&blank)), None);
        let blank = format!("{}{}", " ".repeat(400), "w");
        assert_eq!(context_after(Some(&blank)), None);
    }

    #[test]
    fn context_cuts_are_surrogate_safe() {
        // 301 units: the 300-unit tail starts inside the first pair.
        let prev = format!("😀{}", "a".repeat(299));
        assert_eq!(utf16_len(&prev), 301);
        assert_eq!(context_before(Some(&prev)), Some("a".repeat(299)));
        // The 300-unit head ends inside a pair.
        let next = format!("{}😀b", "c".repeat(299));
        assert_eq!(context_after(Some(&next)), Some("c".repeat(299)));
        // Emoji-only text never yields a lone surrogate.
        let emoji = "😀".repeat(200);
        let before = context_before(Some(&emoji)).unwrap_or_default();
        let after = context_after(Some(&emoji)).unwrap_or_default();
        assert_eq!(utf16_len(&before), 300);
        assert_eq!(utf16_len(&after), 300);
        let odd = format!("x{}", "😀".repeat(200));
        assert_eq!(utf16_len(&context_before(Some(&odd)).unwrap_or_default()), 300);
        assert_eq!(utf16_len(&context_after(Some(&odd)).unwrap_or_default()), 299);
    }
}
