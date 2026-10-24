//! `cleanText` (tts.md §3.1–§3.6): normalises selected text for speech. Paragraph breaks are kept as `"\n\n"`,
//! structural line breaks as `"\n"`; wrap hyphens are joined; inline Markdown is stripped; URLs / e-mails are removed
//! per settings. After the normalisation steps only `' '` and `'\n'` remain from the JS whitespace set.
//!
//! Implementation notes: the original regexes use look-around and backreferences (use `fancy-regex` or hand-written
//! scanners), JS `\s`/`\w`/`\d` semantics (see `vox_core::js`), and `String.replace(str, str)` replaces only the
//! first match. The golden tables in tts.md §3.6 are the test suite, with four rows changed by the Product rulings
//! ("keep the old rules for parity, but fix clear bugs"; ARCHITECTURE.md §8 D-TXT-1..4):
//!
//! | # | Fix | Where | Golden row now |
//! |---|---|---|---|
//! | D-TXT-1 | decode HTML entities — **only `;`-terminated references**: named entries of the integrations.md §5.5 table written `&name;`, `&#N;` and `&#xH;` (same C1 / 0 / surrogate mapping as §5.5). Semicolon-less legacy forms (`&lt`, `&copy`, `&#65`) stay literal: they mostly occur in pasted URL query strings, where decoding `&lt=2` to `<=2` would end `URL_RE` early | new step 1b, before step 2 (so `&#8203;` is then removed and `&#160;` becomes a space) | `"foo&#160;bar"` → `"foo bar"`; `"see https://x.com/?a=1&lt=2 ok"` → `"see ok"`; `"&#65 B"` unchanged; `"a &copy=1"` unchanged |
//! | D-TXT-2 | keep the hyphen of a line-wrapped compound when both halves are words | `joinLines` hyphen case: let `a` = the letter run before the hyphen, `b` = the letter run starting the next line; if `a` and `b` (lower-cased) are both in the embedded English word list (`data/en-words.txt`, a permissively licensed SCOWL-derived list, `include_str!`, binary-searched) **and** `a+b` is not, keep the hyphen and join without a space; otherwise the old behaviour (drop the hyphen) | `"well-\nknown fact"` → `"well-known fact"`; `"infor-\nmation"` → `"information"`; `"some-\nthing"` → `"something"` |
//! | D-TXT-3 | strip `mailto:` case-insensitively | `EMAIL_RE` prefix `(?i:mailto:)?` (the rest stays case-sensitive) | `"MAILTO:foo@bar.com"` → `""` |
//! | D-TXT-4 | drop autolink brackets | `closeHoles` pass 0 (new, before pass 1): `<` … `>` holding only holes collapses, with pass 1's shape. Pass 1 keeps its original classes, so mixed pairs (`(`…`>`, `<`…`)`) are untouched and `(<url>)` collapses fully | `"See <https://x.com> now"` → `"See now"`; `"Ask (<a@b.com>) today"` → `"Ask today"`; `"x <https://x.com) y"` → `"x <) y"` (old) |
//!
//! How the port reproduces the JS regexes (all linear-time, no backtracking engine):
//! - Line patterns, links/images, e-mails and `closeHoles` use the `regex` crate with the JS whitespace set spelled
//!   out (`crate::util::WS`); its leftmost-first semantics give the same matches and captures as V8 for patterns
//!   without look-around. `EMAIL_RE`'s trailing look-ahead can never fail after the greedy TLD, so it is dropped.
//!   Its `(?<!EMAIL_LOCAL)` look-behind matters only after a previous match: V8 resumes at that match's end and the
//!   look-behind sees the address's last character, so an address glued to the previous one (`a@b.com-c@d.org`,
//!   `a@b.cc1x@d.ee`) is not removed. `remove_emails` emulates it on top of the `regex` crate's matches (see
//!   there); without `mailto:` a match's local part ends at its `@`, so a rejected start skips straight to the `@`
//!   and the scan stays linear.
//! - `RULE`'s backreference, the bold/emphasis patterns (look-ahead), and `URL_RE` (`\b` + look-behind) are
//!   hand-written scanners that follow the backtracking order of the original patterns exactly.
//! - D-TXT-2's word list is the SCOWL-derived `data/en-words.txt` (93 823 words including inflected forms, so
//!   `well-\nbehaved` keeps its hyphen too); only `-` and U+2010 are kept as hyphens, a `¬` wrap mark is always
//!   dropped.

use std::sync::{LazyLock, OnceLock};

use regex::{Captures, Regex};
use vox_core::js::js_trim;
use vox_core::settings::TextSettings;
use vox_core::text16::utf16_len;

use crate::extract::{decode_entities, is_thematic_break};
use crate::util::{is_ascii_word, is_letter, is_line_terminator, is_lower, is_number, is_upper, is_ws, WS};

/// The `settings.text` switches `cleanText` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CleanOptions {
    pub skip_urls: bool,
    pub skip_emails: bool,
}

impl From<&TextSettings> for CleanOptions {
    fn from(t: &TextSettings) -> Self {
        Self { skip_urls: t.skip_urls, skip_emails: t.skip_emails }
    }
}

/// Private-use placeholder marking where a URL / e-mail was removed, so the clean-up only touches those spots.
const HOLE: char = '\u{E000}';

/// Cleans `text` for speech (tts.md §3.1 steps 1–11). Must not panic on any input.
pub fn clean_text(text: &str, opts: CleanOptions) -> String {
    // 1. Empty input.
    if text.is_empty() {
        return String::new();
    }
    // 1b. (D-TXT-1) `;`-terminated HTML entities.
    let t = decode_entities(text, true);
    // 2–5. Invisible characters, line breaks, paragraph separators, odd spaces.
    let t = normalize_chars(&t);
    // 6. Hard-wrapped lines, lists, headings.
    let t = join_lines(&t);
    // 7. Inline Markdown.
    let t = strip_inline_markdown(&t);
    // 8–9. URLs and e-mail addresses.
    let t = if opts.skip_urls { remove_urls(&t) } else { t };
    let t = if opts.skip_emails { remove_emails(&t) } else { t };
    // 10. Close the holes (also triggered by a literal U+E000 in the input).
    let t = if t.contains(HOLE) { close_holes(&t) } else { t };
    // 11. Final collapse.
    final_collapse(&t)
}

// ───────────────────────────── steps 2–5 ─────────────────────────────

/// `INVISIBLE = [\u00AD\u180E\u200B-\u200F\u202A-\u202E\u2060-\u2064\uFEFF]`.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{AD}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}

/// `ODD_SPACES = [\t\v\f\u00A0\u1680\u2000-\u200A\u202F\u205F\u3000]`.
fn is_odd_space(c: char) -> bool {
    matches!(c, '\t' | '\u{B}' | '\u{C}' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// Steps 2–5: remove `INVISIBLE`; `\r\n?`, U+2028, U+0085 → `\n`; U+2029 → `\n\n`; `ODD_SPACES` → `' '`.
fn normalize_chars(text: &str) -> String {
    let visible: Vec<char> = text.chars().filter(|&c| !is_invisible(c)).collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < visible.len() {
        let c = visible[i];
        match c {
            '\r' => {
                out.push('\n');
                if visible.get(i + 1) == Some(&'\n') {
                    i += 1;
                }
            }
            '\u{2028}' | '\u{85}' => out.push('\n'),
            '\u{2029}' => out.push_str("\n\n"),
            c if is_odd_space(c) => out.push(' '),
            c => out.push(c),
        }
        i += 1;
    }
    out
}

// ───────────────────────────── step 6: joinLines ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Text,
    Heading,
    Item,
}

#[derive(Debug)]
struct Line {
    text: String,
    kind: LineKind,
    indented: bool,
}

/// A code fence delimiter line: ``` / ~~~ plus at most an info string (`js`, `c++`, `{r}`, `js title="a.js"`).
static FENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^[{WS}]*(?:`{{3,}}|~{{3,}})[{WS}]*(?:[^{WS}`~]+(?:[{WS}]+(?:[^{WS}`~={{}}]+=[^{WS}`~{{]*|\{{[^`~{{}}\n]*\}}))*)?[{WS}]*$"
    ))
    .expect("valid regex")
});
static HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^[{WS}]{{0,3}}#{{1,6}}[{WS}]+(.*?)(?:[{WS}]+#+)?[{WS}]*$")).expect("valid regex")
});
static QUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^[{WS}]*(?:>[{WS}]?)+")).expect("valid regex"));
/// Bullets: `- * + • ◦ ▪ ‣ · ● ○ ■ □ –`.
static BULLET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^[{WS}]*[-*+\x{{2022}}\x{{25E6}}\x{{25AA}}\x{{2023}}\x{{B7}}\x{{25CF}}\x{{25CB}}\x{{25A0}}\x{{25A1}}\x{{2013}}][{WS}]+(.*)$"
    ))
    .expect("valid regex")
});
static ORDERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^[{WS}]*(?:[0-9]{{1,3}}[.)]|[a-z]\))[{WS}]+[^{WS}]")).expect("valid regex")
});

/// `classify(raw)` (tts.md §3.3): `None` is a "blank" line.
fn classify(raw: &str) -> Option<Line> {
    // `/^\s{2,}\S/` on the raw line.
    let leading = raw.chars().take_while(|&c| is_ws(c)).count();
    let indented = leading >= 2 && raw.chars().nth(leading).is_some();
    let line = QUOTE.replacen(raw, 1, "");
    if js_trim(&line).is_empty() || is_thematic_break(&line) || is_setext(&line) || FENCE.is_match(&line) {
        return None;
    }
    if let Some(caps) = HEADING.captures(&line) {
        let text = caps.get(1).map_or("", |m| m.as_str());
        if text.is_empty() {
            return None;
        }
        return Some(Line { text: js_trim(text).to_owned(), kind: LineKind::Heading, indented });
    }
    if let Some(caps) = BULLET.captures(&line) {
        let text = caps.get(1).map_or("", |m| m.as_str());
        return Some(Line { text: js_trim(text).to_owned(), kind: LineKind::Item, indented });
    }
    let text = js_trim(&line);
    let kind = if ORDERED.is_match(text) { LineKind::Item } else { LineKind::Text };
    Some(Line { text: text.to_owned(), kind, indented })
}

/// `SETEXT = /^\s*=+\s*$/`.
fn is_setext(line: &str) -> bool {
    let t = js_trim(line);
    !t.is_empty() && t.chars().all(|c| c == '=')
}

/// `CJK` (BMP ranges, tested per char).
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{2E80}'..='\u{2FFF}' | '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FFEF}')
}

/// `ENDS_WITH_PUNCT = /[.!?:;,…。！？、，；：)"”’'\]]$/`.
fn ends_with_punct(s: &str) -> bool {
    s.chars().last().is_some_and(|c| {
        matches!(
            c,
            '.' | '!' | '?' | ':' | ';' | ',' | '\u{2026}' | '\u{3002}' | '\u{FF01}' | '\u{FF1F}' | '\u{3001}'
                | '\u{FF0C}' | '\u{FF1B}' | '\u{FF1A}' | ')' | '"' | '\u{201D}' | '\u{2019}' | '\'' | ']'
        )
    })
}

/// `keepBreak(prev, cur)`.
fn keep_break(prev: &Line, cur: &Line) -> bool {
    if cur.kind == LineKind::Heading || prev.kind == LineKind::Heading {
        return true;
    }
    if cur.kind == LineKind::Item {
        return true;
    }
    if prev.kind == LineKind::Item {
        return !cur.indented;
    }
    // A short title-like line without punctuation followed by a capitalised line ("Abstract\nWe propose…").
    utf16_len(&prev.text) < 40
        && !ends_with_punct(&prev.text)
        && !prev.text.chars().any(is_cjk)
        && cur.text.chars().next().is_some_and(|c| is_upper(c) || is_number(c))
}

/// The wrap-hyphen case: `LINE_HYPHEN` (`\p{L}[-‐¬]$`) on `last` and `^\p{Ll}` on `next`. `Some(keep)`, where `keep`
/// is D-TXT-2's verdict for a real compound.
fn wrap_hyphen(last: &str, next: &str) -> Option<bool> {
    let mut rev = last.chars().rev();
    let hyphen = rev.next()?;
    if !matches!(hyphen, '-' | '\u{2010}' | '\u{AC}') {
        return None;
    }
    if !rev.next().is_some_and(is_letter) || !next.chars().next().is_some_and(is_lower) {
        return None;
    }
    if hyphen == '\u{AC}' {
        return Some(false);
    }
    let before: String = {
        let mut run: Vec<char> = last.chars().rev().skip(1).take_while(|&c| is_letter(c)).collect();
        run.reverse();
        run.into_iter().collect()
    };
    let after: String = next.chars().take_while(|&c| is_letter(c)).collect();
    let (a, b) = (before.to_lowercase(), after.to_lowercase());
    Some(is_word(&a) && is_word(&b) && !is_word(&format!("{a}{b}")))
}

/// `joinLines(t)` (tts.md §3.3).
fn join_lines(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut prev: Option<Line> = None;
    let mut paragraph_break = false;
    for raw in t.split('\n') {
        let Some(line) = classify(raw) else {
            if prev.is_some() {
                paragraph_break = true; // leading blank lines are ignored
            }
            continue;
        };
        if let Some(p) = &prev {
            if paragraph_break {
                out.push_str("\n\n");
            } else if keep_break(p, &line) {
                out.push('\n');
            } else if let Some(keep) = wrap_hyphen(&p.text, &line.text) {
                // "infor-\nmation" → "information" (the output ends with `p.text`, whose last char is the hyphen).
                if !keep {
                    out.pop();
                }
            } else if !(p.text.chars().last().is_some_and(is_cjk) && line.text.chars().next().is_some_and(is_cjk)) {
                out.push(' '); // CJK lines join with no space
            }
        }
        out.push_str(&line.text);
        prev = Some(line);
        paragraph_break = false;
    }
    out
}

// ───────────────────────────── D-TXT-2 word list ─────────────────────────────

static WORD_DATA: &str = include_str!("../data/en-words.txt");

/// The provenance and licence notice of the embedded D-TXT-2 word list (the comment header of `data/en-words.txt`,
/// without the `# ` prefixes): the SCOWL, WordNet, UKACD and Ispell terms, which ask for the notice to be shipped and
/// displayed with the binary (e.g. in a third-party licences file or an About → Licences view).
pub fn word_list_notice() -> String {
    WORD_DATA
        .lines()
        .take_while(|l| l.starts_with('#'))
        .map(|l| {
            let l = l.trim_start_matches('#');
            l.strip_prefix(' ').unwrap_or(l).trim_end()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Line offsets of the sorted word list (built once, on first use).
fn word_offsets() -> &'static [(u32, u32)] {
    static OFFSETS: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    OFFSETS.get_or_init(|| {
        let mut out = Vec::with_capacity(100_000);
        let mut pos = 0usize;
        for line in WORD_DATA.split_inclusive('\n') {
            let start = pos;
            pos += line.len();
            let word = line.trim_end_matches(['\n', '\r']);
            if word.is_empty() || word.starts_with('#') {
                continue;
            }
            let (s, e) = (u32::try_from(start).unwrap_or(0), u32::try_from(start + word.len()).unwrap_or(0));
            out.push((s, e));
        }
        // The file is sorted; sorting again keeps the lookup correct if someone edits it by hand.
        out.sort_by(|a, b| word_at(*a).cmp(word_at(*b)));
        out.dedup_by(|a, b| word_at(*a) == word_at(*b));
        out
    })
}

fn word_at((s, e): (u32, u32)) -> &'static str {
    WORD_DATA.get(s as usize..e as usize).unwrap_or("")
}

/// True when `word` (already lower-cased) is in the embedded English word list.
fn is_word(word: &str) -> bool {
    !word.is_empty() && word_offsets().binary_search_by(|&o| word_at(o).cmp(word)).is_ok()
}

// ───────────────────────────── step 7: inline Markdown ─────────────────────────────

/// Row 1: image → alt text.
static MD_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"!\[([^\[\]\n]*)\]\([^){WS}]*(?:[{WS}]+"[^"]*")?\)"#)).expect("valid regex")
});
/// Row 2: link → link text (the URL may contain one level of balanced parentheses).
static MD_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"\[([^\[\]\n]+)\]\((?:[^(){WS}]|\([^(){WS}]*\))*(?:[{WS}]+"[^"]*")?\)"#))
        .expect("valid regex")
});
/// Row 8: `~~strike~~` and decorative `~~~ Chapter One ~~~`.
static MD_TILDES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~{2,}").expect("valid regex"));

/// `stripInlineMarkdown` (tts.md §3.4), in the original order.
fn strip_inline_markdown(t: &str) -> String {
    let t = MD_IMAGE.replace_all(t, "${1}");
    let t = MD_LINK.replace_all(&t, "${1}");
    let t = strip_strong(&t, '*');
    let t = strip_strong(&t, '_');
    let t = strip_em(&t, '*');
    let t = strip_em(&t, '_');
    let t = t.replace('`', "");
    MD_TILDES.replace_all(&t, "").into_owned()
}

/// Multiline `^` at char index `p`.
fn at_line_start(c: &[char], p: usize) -> bool {
    p == 0 || is_line_terminator(c[p - 1])
}

/// Rows 3–4: `/(^|[^A-Za-z0-9D])DD(?=\S)((?:(?!DD)[^\n])*?\S)DD(?![A-Za-z0-9D])/gm → "$1$2"` for `D` = `*` or `_`.
/// Bold applies only when not glued to a letter or digit (`2**10 and 3**4` survive); the content cannot run past
/// another delimiter, which keeps the scan linear.
fn strip_strong(t: &str, d: char) -> String {
    let pair: String = [d, d].iter().collect();
    if !t.contains(&pair) {
        return t.to_owned();
    }
    let c: Vec<char> = t.chars().collect();
    let n = c.len();
    let glued = |ch: char| ch.is_ascii_alphanumeric() || ch == d;
    // The match end and the content range for an opener at `q`.
    let try_at = |q: usize| -> Option<(usize, usize, usize)> {
        if c.get(q) != Some(&d) || c.get(q + 1) != Some(&d) {
            return None;
        }
        let start = q + 2;
        if start >= n || is_ws(c[start]) {
            return None;
        }
        let mut k = start;
        loop {
            // Candidate: content = c[start..=k].
            if k >= n {
                return None;
            }
            if !is_ws(c[k]) && c.get(k + 1) == Some(&d) && c.get(k + 2) == Some(&d) && !c.get(k + 3).is_some_and(|&a| glued(a)) {
                return Some((start, k + 1, k + 3));
            }
            // Extend the lazy part by c[k]: `(?!DD)[^\n]`.
            if c[k] == '\n' || (c[k] == d && c.get(k + 1) == Some(&d)) {
                return None;
            }
            k += 1;
        }
    };
    let mut out = String::with_capacity(t.len());
    let mut p = 0;
    while p < n {
        if at_line_start(&c, p) {
            if let Some((s, e, end)) = try_at(p) {
                out.extend(&c[s..e]);
                p = end;
                continue;
            }
        }
        if !glued(c[p]) {
            if let Some((s, e, end)) = try_at(p + 1) {
                out.push(c[p]);
                out.extend(&c[s..e]);
                p = end;
                continue;
            }
        }
        out.push(c[p]);
        p += 1;
    }
    out
}

/// Rows 5–6: `/(^|[\s(])D(?=\S)([^D\n]*?\S)D(?=$|[\s).,;:!?])/gm → "$1$2"`: single `*em*` / `_em_` only when delimited
/// by spaces or punctuation (`2 * 3` and `snake_case` survive).
fn strip_em(t: &str, d: char) -> String {
    if !t.contains(d) {
        return t.to_owned();
    }
    let c: Vec<char> = t.chars().collect();
    let n = c.len();
    let try_at = |q: usize| -> Option<(usize, usize, usize)> {
        if c.get(q) != Some(&d) {
            return None;
        }
        let start = q + 1;
        if start >= n || is_ws(c[start]) {
            return None;
        }
        let mut k = start;
        loop {
            if k >= n {
                return None;
            }
            if !is_ws(c[k]) && c.get(k + 1) == Some(&d) {
                let ok = match c.get(k + 2) {
                    None => true,
                    Some(&a) => is_ws(a) || is_line_terminator(a) || matches!(a, ')' | '.' | ',' | ';' | ':' | '!' | '?'),
                };
                if ok {
                    return Some((start, k + 1, k + 2));
                }
            }
            // Extend `[^D\n]*?` by c[k].
            if c[k] == d || c[k] == '\n' {
                return None;
            }
            k += 1;
        }
    };
    let mut out = String::with_capacity(t.len());
    let mut p = 0;
    while p < n {
        if at_line_start(&c, p) {
            if let Some((s, e, end)) = try_at(p) {
                out.extend(&c[s..e]);
                p = end;
                continue;
            }
        }
        if is_ws(c[p]) || c[p] == '(' {
            if let Some((s, e, end)) = try_at(p + 1) {
                out.push(c[p]);
                out.extend(&c[s..e]);
                p = end;
                continue;
            }
        }
        out.push(c[p]);
        p += 1;
    }
    out
}

// ───────────────────────────── steps 8–10: URLs, e-mails, holes ─────────────────────────────

/// The URL body: runs to whitespace, a quote or angle bracket, or the first CJK, kana, Hangul, Thai-family or
/// full-width character (`\p{Script=…}` = the Script property, as in JS).
static URL_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"^[^{WS}<>"'`\x{{3000}}-\x{{30FF}}\x{{FF00}}-\x{{FFEF}}\p{{Script=Han}}\p{{Script=Hangul}}\p{{Script=Thai}}\p{{Script=Lao}}\p{{Script=Khmer}}\p{{Script=Myanmar}}]+"#
    ))
    .expect("valid regex")
});

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.as_bytes().get(..prefix.len()).is_some_and(|b| b.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// The prefix length of `URL_RE` at `rest`: `\bhttps?:\/\/`, `\bftp:\/\/` (ASCII word boundary) or `www.` not
/// preceded by `[@\w.-]` (case-insensitive).
fn url_prefix(rest: &str, prev: Option<char>) -> Option<usize> {
    if !prev.is_some_and(is_ascii_word) {
        for p in ["https://", "http://", "ftp://"] {
            if starts_with_ignore_ascii_case(rest, p) {
                return Some(p.len());
            }
        }
    }
    let blocked = prev.is_some_and(|c| is_ascii_word(c) || matches!(c, '@' | '.' | '-'));
    (!blocked && starts_with_ignore_ascii_case(rest, "www.")).then_some(4)
}

/// `URL_TRAILING = /[.,;:!?'"”’»)\]}>]+$/`.
fn is_url_trailing(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '\u{201D}' | '\u{2019}' | '\u{BB}' | ')' | ']' | '}' | '>')
}

/// `urlTrailing(match)`: trailing punctuation that belongs to the sentence; a `)` closing a `(` inside the URL
/// (Wikipedia links) stays with the URL, and only one `)` is ever given back to it.
fn url_trailing(m: &str) -> String {
    let body_len = m.trim_end_matches(is_url_trailing).len();
    let (body, trail) = m.split_at(body_len);
    if !trail.contains(')') {
        return trail.to_owned();
    }
    let open = body.matches('(').count();
    let close = body.matches(')').count();
    if open > close {
        trail.replacen(')', "", 1)
    } else {
        trail.to_owned()
    }
}

/// Step 8: every `URL_RE` match → `HOLE + urlTrailing(match)`.
fn remove_urls(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut copied = 0;
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < t.len() {
        let rest = &t[i..];
        if let Some(prefix) = url_prefix(rest, prev) {
            if let Some(body) = URL_BODY.find(&rest[prefix..]) {
                let end = i + prefix + body.end();
                let whole = &t[i..end];
                out.push_str(&t[copied..i]);
                out.push(HOLE);
                out.push_str(&url_trailing(whole));
                copied = end;
                prev = whole.chars().last();
                i = end;
                continue;
            }
        }
        let Some(c) = rest.chars().next() else {
            break;
        };
        prev = Some(c);
        i += c.len_utf8();
    }
    out.push_str(&t[copied..]);
    out
}

/// `EMAIL_LETTERS`: local part, labels and TLD are limited to spaced alphabets (Latin, Cyrillic, Greek) so an address
/// glued to CJK or Thai text does not take its neighbours along.
const EMAIL_LETTERS: &str = r"\p{Script=Latin}\p{Script=Cyrillic}\p{Script=Greek}";

/// Step 9: `EMAIL_RE` without its look-around (emulated by [`remove_emails`]), with D-TXT-3's case-insensitive
/// `mailto:`.
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    let letters = EMAIL_LETTERS;
    Regex::new(&format!(
        r"(?:(?i:mailto:))?[{letters}\p{{N}}._%+\-]+@[{letters}\p{{N}}\-]+(?:\.[{letters}\p{{N}}\-]+)*\.[{letters}]{{2,}}"
    ))
    .expect("valid regex")
});

/// One `EMAIL_LOCAL` character: `[EMAIL_LETTERS \p{N} . _ % + -]`.
static EMAIL_LOCAL_CHAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^[{EMAIL_LETTERS}\p{{N}}._%+\-]$")).expect("valid regex"));

fn is_email_local(c: char) -> bool {
    let mut buf = [0u8; 4];
    EMAIL_LOCAL_CHAR.is_match(c.encode_utf8(&mut buf))
}

/// Step 9: every `EMAIL_RE` match → `HOLE`, as V8's global `replace` finds them.
///
/// The original pattern is `(?:mailto:)?(?<!EMAIL_LOCAL)EMAIL_LOCAL+@…`. At any start position the `regex` crate's
/// leftmost-first match equals V8's, except that V8 rejects a match without `mailto:` whose start follows an
/// `EMAIL_LOCAL` character (the look-behind sits after the optional `mailto:`, where it always sees `:`). Such a
/// match contains no `:`, and its greedy local part ends exactly at its `@` (`@` is not `EMAIL_LOCAL`). Every later
/// start up to that `@` also follows an `EMAIL_LOCAL` character, and none can begin a `mailto:` (its `:` would have to
/// lie inside the run or at the `@`), so V8 finds nothing before the `@` and the search resumes there.
fn remove_emails(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut copied = 0;
    let mut pos = 0;
    while let Some(m) = EMAIL_RE.find_at(t, pos) {
        let found = m.as_str();
        let glued = !found.contains(':') && t[..m.start()].chars().next_back().is_some_and(is_email_local);
        if glued {
            // The local part is non-empty, so this always moves forward.
            pos = m.start() + found.find('@').unwrap_or(found.len());
            continue;
        }
        out.push_str(&t[copied..m.start()]);
        out.push(HOLE);
        copied = m.end();
        pos = m.end();
    }
    out.push_str(&t[copied..]);
    out
}

/// Pass 0 (D-TXT-4): autolink brackets `<…>` holding only removed items collapse (same shape as pass 1).
static HOLES_AUTOLINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"<[{WS}]*\x{{E000}}(?:[{WS},;\x{{3001}}\x{{FF0C}}]*\x{{E000}})*[{WS}]*>")).expect("valid regex")
});
/// Pass 1: brackets holding only removed items collapse.
static HOLES_BRACKETS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"[(\[\x{{FF08}}\x{{FF3B}}][{WS}]*\x{{E000}}(?:[{WS},;\x{{3001}}\x{{FF0C}}]*\x{{E000}})*[{WS}]*[)\]\x{{FF09}}\x{{FF3D}}]"
    ))
    .expect("valid regex")
});
/// Pass 2: several removed items in a row become one (`and`/`or` case-sensitive, no word boundary).
static HOLES_RUN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x{E000}(?:[ \t]*(?:[,;/|\x{3001}\x{FF0C}]|and|or)?[ \t]*\x{E000})+").expect("valid regex")
});
/// Pass 3: one hole with its punctuation.
static HOLES_ONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"([,;:]?)[ \t]*\x{E000}[ \t]*([,.;:!?)\]\x{3001}\x{3002}\x{FF0C}\x{FF0E}\x{FF01}\x{FF1F}\x{FF1B}\x{FF1A}\x{FF09}\x{FF3D}\x{300D}\x{300F}]?)",
    )
    .expect("valid regex")
});

/// `UNSPACED = [\u2E80-\u9FFF\uF900-\uFAFF\uFF00-\uFFEF]`: scripts written without spaces.
fn is_unspaced(c: char) -> bool {
    matches!(c, '\u{2E80}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FFEF}')
}

/// `closeHoles(t)` (tts.md §3.5), with D-TXT-4's pass 0.
fn close_holes(t: &str) -> String {
    let t = HOLES_AUTOLINK.replace_all(t, HOLE.to_string());
    let t = HOLES_BRACKETS.replace_all(&t, HOLE.to_string());
    let t = HOLES_RUN.replace_all(&t, HOLE.to_string());
    let s: &str = &t;
    HOLES_ONE
        .replace_all(s, |caps: &Captures| -> String {
            let pre = caps.get(1).map_or("", |m| m.as_str());
            let post = caps.get(2).map_or("", |m| m.as_str());
            if post.chars().next().is_some_and(|c| {
                matches!(
                    c,
                    '.' | '!' | '?' | ')' | ']' | '\u{3002}' | '\u{FF0E}' | '\u{FF01}' | '\u{FF1F}' | '\u{FF09}'
                        | '\u{FF3D}' | '\u{300D}' | '\u{300F}'
                )
            }) {
                // "see <url>." → "see."   "(see <url>)" → "(see)"   "詳細は<url>。" → "詳細は。"
                return post.to_owned();
            }
            if !post.is_empty() {
                // "at <email>, or" → "at, or"
                return if pre.is_empty() { post } else { pre }.to_owned();
            }
            if !pre.is_empty() {
                return format!("{pre} ");
            }
            // "詳細は<url>を参照" → "詳細はを参照": no space between characters written without spaces.
            let (start, end) = caps.get(0).map_or((0, 0), |m| (m.start(), m.end()));
            let before = s[..start].chars().next_back();
            let after = s[end..].chars().next();
            if before.is_some_and(is_unspaced) && after.is_some_and(is_unspaced) {
                String::new()
            } else {
                " ".to_owned()
            }
        })
        .into_owned()
}

// ───────────────────────────── step 11 ─────────────────────────────

static SPACE_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ ]{2,}").expect("valid regex"));
static SPACES_AROUND_NEWLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" *\n *").expect("valid regex"));
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").expect("valid regex"));

/// `/[ ]{2,}/g → " "`, `/ *\n */g → "\n"`, `/\n{3,}/g → "\n\n"`, JS `trim()`.
fn final_collapse(t: &str) -> String {
    let t = SPACE_RUNS.replace_all(t, " ");
    let t = SPACES_AROUND_NEWLINE.replace_all(&t, "\n");
    let t = BLANK_LINES.replace_all(&t, "\n\n");
    js_trim(&t).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: CleanOptions = CleanOptions { skip_urls: true, skip_emails: true };
    const NONE: CleanOptions = CleanOptions { skip_urls: false, skip_emails: false };
    const DEFAULTS: CleanOptions = CleanOptions { skip_urls: true, skip_emails: false };

    fn check(cases: &[(&str, &str)], opts: CleanOptions) {
        for (input, expected) in cases {
            assert_eq!(clean_text(input, opts), *expected, "{input:?}");
        }
    }

    /// tts.md §3.6, first table (skipUrls = skipEmails = true), with the D-TXT rows updated.
    #[test]
    fn golden_table_one() {
        check(
            &[
                ("Hello   world", "Hello world"),
                ("This is infor-\nmation that wraps\nacross lines.", "This is information that wraps across lines."),
                ("Abstract\nWe propose a method.", "Abstract\nWe propose a method."),
                (
                    "# Title\nSome **bold** and _em_ and `code`.\n\n- item one\n- item two\n  continued\nAfter list.",
                    "Title\nSome bold and em and code.\n\nitem one\nitem two continued\nAfter list.",
                ),
                ("## Title ##\nbody text", "Title\nbody text"),
                ("1. First\n2. Second\ncontinued", "1. First\n2. Second\ncontinued"),
                ("Chapter 1\nThe beginning", "Chapter 1\nThe beginning"),
                ("Line one\nline two continues", "Line one line two continues"),
                // D-TXT-2 (was "wellknown fact").
                ("well-\nknown fact", "well-known fact"),
                ("中文第一行\n中文第二行", "中文第一行中文第二行"),
                ("> quoted line\n> more quote", "quoted line more quote"),
                ("```js\nconst x = 1\n```\nDone.", "const x = 1\n\nDone."),
                ("***\n\nPara two", "Para two"),
                ("a  \n\n\n\n  b", "a\n\nb"),
                ("Text\u{2029}Para", "Text\n\nPara"),
                ("Hello\u{200B}world\u{A0}there\r\nnext", "Helloworld there next"),
                ("[link text](https://x.com \"title\") and ![alt](img.png)", "link text and alt"),
                (
                    "2**10 and 3**4 and snake_case_name and 2 * 3 * 4",
                    "2**10 and 3**4 and snake_case_name and 2 * 3 * 4",
                ),
                ("~~strike~~ text ~~~ Chapter One ~~~", "strike text Chapter One"),
                ("see https://example.com/docs.", "see."),
                ("Visit https://x.com today", "Visit today"),
                ("(see https://x.com)", "(see)"),
                ("Read (https://en.wikipedia.org/wiki/Foo_(bar)). Next.", "Read. Next."),
                ("Results (https://a.com, https://b.com) are in", "Results are in"),
                ("Links: https://a.com, https://b.com and https://c.com.", "Links."),
                ("Look here: https://a.com/x?y=1;", "Look here:"),
                ("www.example.com/path is it", "is it"),
                ("https://x.com", ""),
                ("Mail me at foo@example.com, or call.", "Mail me at, or call."),
                ("Contact: foo@x.com", "Contact:"),
                ("user@www.example.com", ""),
                // D-TXT-3 (was "MAILTO:").
                ("MAILTO:foo@bar.com", ""),
                ("詳細はhttps://x.com/docsを参照してください。", "詳細はを参照してください。"),
                ("連絡先はfoo@example.comまで", "連絡先はまで"),
                // D-TXT-1 (was unchanged).
                ("foo&#160;bar", "foo bar"),
            ],
            BOTH,
        );
    }

    /// tts.md §3.6, second table, with the D-TXT rows updated.
    #[test]
    fn golden_table_two() {
        check(
            &[
                ("- \nnext", "next"),
                ("####\nbody", "#### body"),
                ("Title\n=====\nBody", "Title\n\nBody"),
                ("> # Quoted heading\n>\n> para", "Quoted heading\n\npara"),
                ("item:\n  - nested\n  - nested2", "item:\nnested\nnested2"),
                ("Some text\n    indented code line\nmore", "Some text indented code line more"),
                ("Short\nlowercase next", "Short lowercase next"),
                ("Intro:\nThe body", "Intro: The body"),
                ("1) one\n2) two", "1) one\n2) two"),
                ("a) alpha\nb) beta", "a) alpha\nb) beta"),
                ("\u{2022} bullet\n\u{2022} two", "bullet\ntwo"),
                ("\u{B7} mid", "mid"),
                ("\u{2013} dash item", "dash item"),
                ("A\tB", "A B"),
                ("x\u{85}y", "x y"),
                ("\u{FEFF}start", "start"),
                ("a\u{3000}b", "a b"),
                ("\u{FF28}\u{FF45}\u{FF4C}\u{FF4C}\u{FF4F}\n\u{FF37}\u{FF4F}\u{FF52}\u{FF4C}\u{FF44}", "\u{FF28}\u{FF45}\u{FF4C}\u{FF4C}\u{FF4F}\u{FF37}\u{FF4F}\u{FF52}\u{FF4C}\u{FF44}"),
                ("```npm install``` then go", "npm install then go"),
                ("~~~\ncode\n~~~", "code"),
                ("```js title=\"a.js\"\nx\n```", "x"),
                ("See [a](b) [c](d)", "See a c"),
                ("HTTPS://X.COM/Y ok", "ok"),
                ("ftp://files.example.com/a.zip done", "done"),
                ("see (https://x.com/a_(b)_(c)) ok", "see ok"),
                ("x https://a.com/(b)) c", "x) c"),
                ("A: https://a.com/b) c", "A) c"),
                // D-TXT-4 (was "See < > now").
                ("See <https://x.com> now", "See now"),
                ("http://\u{4F8B}\u{3048}.jp/\u{30D1}\u{30B9} is", "http://\u{4F8B}\u{3048}.jp/\u{30D1}\u{30B9} is"),
                ("see http://x.com/a\u{A0}b", "see b"),
                ("mailto:foo@bar.com", ""),
                ("email:foo@bar.com", "email:"),
                ("Email foo@bar.co.uk.", "Email."),
                ("trailing\n\n\n", "trailing"),
            ],
            BOTH,
        );
        check(&[("Go to [docs](https://x.com) now", "Go to docs now"), ("a \u{E000} b", "a b")], NONE);
        // Inline examples of §3.2 and §3.5.
        check(
            &[
                ("a\u{200B}b", "ab"),
                ("\u{8A73}\u{7D30}\u{306F}https://x.com\u{3002}", "\u{8A73}\u{7D30}\u{306F}\u{3002}"),
                ("see https://x.com.", "see."),
                ("Write to foo@bar.com, or not", "Write to, or not"),
            ],
            BOTH,
        );
        check(&[("Mail foo@example.com see https://x.com.", "Mail foo@example.com see.")], DEFAULTS);
    }

    /// ARCHITECTURE.md §8 D-TXT-1..4 rows and §9's extra vectors.
    #[test]
    fn product_ruling_fixes() {
        check(
            &[
                ("see https://x.com/?a=1&lt=2 ok", "see ok"),
                ("&#65 B", "&#65 B"),
                ("a &copy=1", "a &copy=1"),
                ("Tom &amp; Jerry &lt;3 &hellip; &#8220;hi&#8221;", "Tom & Jerry <3 … \u{201C}hi\u{201D}"),
                ("zero&#8203;width", "zerowidth"),
                ("soft&shy;hyphen", "softhyphen"),
                ("&#x1F600; ok &#0; &#xD800;", "😀 ok \u{FFFD} \u{FFFD}"),
                ("&notreal; stays", "&notreal; stays"),
                ("infor-\nmation", "information"),
                ("some-\nthing", "something"),
                ("well-\nknown fact", "well-known fact"),
                ("A well-\nbehaved dog", "A well-behaved dog"),
                ("Well\u{2010}\nknown", "Well\u{2010}known"),
                ("well\u{AC}\nknown", "wellknown"),
                ("x-\nray", "x-ray"),
                ("mailto:a@b.com MAILTO:c@d.org Mailto:e@f.net", ""),
                ("See <https://x.com> now", "See now"),
                ("Write <foo@bar.com> today", "Write today"),
                // An autolink inside brackets: pass 0 collapses `<…>`, then pass 1 the outer pair.
                ("(<https://x.com>) next", "next"),
                ("Ask (<a@b.com>) today", "Ask today"),
                ("(https://x.com) <b@c.de> ok", "ok"),
                ("< https://a.com , https://b.com > end", "end"),
            ],
            BOTH,
        );
        // Without the hole machinery, autolink brackets are untouched.
        check(&[("See <https://x.com> now", "See <https://x.com> now")], NONE);
    }

    /// D-TXT-4 covers only `<…>`: mixed pairs keep the original output (computed with the original `text.ts`).
    #[test]
    fn mixed_brackets_keep_old_output() {
        check(
            &[
                ("Docs (https://x.com> here", "Docs ( > here"),
                ("x <https://x.com) y", "x <) y"),
                ("See [https://x.com> now", "See [ > now"),
                ("if a < https://a.com) then b", "if a <) then b"),
                ("see (https://a.com> now", "see ( > now"),
                ("x <https://a.com]. y", "x <]. y"),
            ],
            BOTH,
        );
    }

    /// `EMAIL_RE`'s look-behind after a previous match (outputs computed with the original `text.ts`).
    #[test]
    fn glued_addresses_keep_old_output() {
        check(
            &[
                ("foo@bar.com-x@y.org", "-x@y.org"),
                ("Mail a@b.co-x@y.org now", "Mail -x@y.org now"),
                ("a@b.cc1x@d.ee", "1x@d.ee"),
                ("x foo@bar.com-baz@qux.org y", "x -baz@qux.org y"),
                ("a@b.com.c@d.org", ".c@d.org"),
                ("foo@bar.comx__userfoo@bar.comFig. 3.org\u{2026}", "__userfoo@bar.comFig. 3.org\u{2026}"),
                // `mailto:` right after an address: the look-behind sees `:` and the address goes.
                ("a@b.com-mailto:c@d.org z", "- z"),
                ("a@b.comxmailto:c@d.org z", ": z"),
                ("x@y.com mailto:a@b.com", ""),
                ("a@b.co,c@d.org", ""),
                ("a@b.co c@d.org", ""),
                ("x@y.org@z.com", "@z.com"),
                ("a@b.c.d@e.fg", "a@"),
            ],
            BOTH,
        );
    }

    /// The straightforward reading of the original `EMAIL_RE` (`(?:mailto:)?(?<!LOCAL)LOCAL+@…`): try every start
    /// from the previous match's end, `mailto:` first, then the bare form if the look-behind allows. Quadratic; tests
    /// only.
    fn remove_emails_reference(t: &str) -> String {
        static ANCHORED: LazyLock<(Regex, Regex)> = LazyLock::new(|| {
            let anchored = |prefix: &str| {
                let letters = EMAIL_LETTERS;
                Regex::new(&format!(
                    r"^{prefix}[{letters}\p{{N}}._%+\-]+@[{letters}\p{{N}}\-]+(?:\.[{letters}\p{{N}}\-]+)*\.[{letters}]{{2,}}"
                ))
                .expect("valid regex")
            };
            (anchored("(?i:mailto:)"), anchored(""))
        });
        let (with_mailto, bare) = (&ANCHORED.0, &ANCHORED.1);
        let mut out = String::new();
        let mut copied = 0;
        let mut p = 0;
        while p < t.len() {
            let rest = &t[p..];
            let behind_ok = !t[..p].chars().next_back().is_some_and(is_email_local);
            let found = with_mailto.find(rest).or_else(|| bare.find(rest).filter(|_| behind_ok));
            if let Some(m) = found {
                out.push_str(&t[copied..p]);
                out.push(HOLE);
                p += m.end();
                copied = p;
            } else {
                p += rest.chars().next().map_or(1, char::len_utf8);
            }
        }
        out.push_str(&t[copied..]);
        out
    }

    #[test]
    fn email_scan_matches_reference() {
        const PARTS: &[&str] = &[
            "a", "x", "foo", "1", "9", "é", "ж", "λ", "中", ".", "-", "_", "%", "+", "@", ":", " ", ",", "mailto:",
            "MailTo:", "mailto", "com", "co", "b.c", "@b.com", "a@b.cc", "x@y.org", "\u{E000}", "😀",
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..4000 {
            let text: String = (0..1 + rng.below(16)).map(|_| PARTS[rng.below(PARTS.len())]).collect();
            assert_eq!(remove_emails(&text), remove_emails_reference(&text), "{text:?}");
        }
    }

    #[test]
    fn markdown_and_lines_edge_cases() {
        check(
            &[
                ("**a***", "a*"),
                ("x **bold** y", "x bold y"),
                ("**a** **b**", "a b"),
                ("f(**a) and g(**b)", "f(**a) and g(**b)"),
                ("(*em*) and (_em_)", "(em) and (em)"),
                ("__init__ stays", "init stays"),
                // Lines are joined first, so the bold pair meets on one line; list items keep their break.
                ("**bold\nacross** lines", "bold across lines"),
                ("- **bold\n- across** lines", "**bold\nacross** lines"),
                ("![](pic.png) caption", "caption"),
                ("[a [b](c)", "[a b"),
                ("[link](https://a.com/x_(y)) ok", "link ok"),
                ("####### seven", "####### seven"),
                ("#hashtag here", "#hashtag here"),
                ("# ", ""),
                // Four digits are not an ordered marker, but the title rule keeps a break before a digit.
                ("2024. was a year\n2025. too", "2024. was a year\n2025. too"),
                ("2024. was a year\nand more", "2024. was a year and more"),
                ("A) alpha\nB) beta", "A) alpha\nB) beta"),
                ("item\n> > nested quote", "item nested quote"),
                ("- a\n\n\n- b", "a\n\nb"),
                ("lead\n\n\n\n", "lead"),
                ("\n\n\nlead", "lead"),
                ("one\r\rtwo", "one\n\ntwo"),
                ("x\u{2028}y", "x y"),
            ],
            BOTH,
        );
    }

    #[test]
    fn url_and_email_edges() {
        check(
            &[
                ("xhttp://a.com y", "xhttp://a.com y"),
                ("a.www.example.com b", "a.www.example.com b"),
                ("go www.x.org! now", "go! now"),
                ("WWW.EXAMPLE.COM ok", "ok"),
                ("https:// alone", "https:// alone"),
                ("x \"https://q.com\" y", "x \" \" y"),
                ("see https://x.com/wiki/Москва ok", "see ok"),
                // Pass 2 merges "<url>, <url>"; "; and" is not a joiner, so the `;` survives.
                ("a https://x.com, https://y.com; and https://z.com b", "a; and b"),
                ("Visit https://x.com: it", "Visit: it"),
                ("foo@bar.com1 x", "1 x"),
                ("foo@bar@baz.com", "foo@"),
                ("a.b-c+d@sub.example.co.uk ok", "ok"),
                ("ïñtl@exämple.de ok", "ok"),
                ("no@tld", "no@tld"),
            ],
            BOTH,
        );
        // E-mails survive with skipEmails off; URLs with skipUrls off.
        check(&[("foo@bar.com and https://x.com", "foo@bar.com and https://x.com")], NONE);
    }

    #[test]
    fn word_list_lookup() {
        assert!(is_word("well") && is_word("known") && !is_word("wellknown"));
        assert!(is_word("a") && is_word("something") && !is_word("infor"));
        assert!(!is_word("") && !is_word("#"));
        assert!(word_offsets().len() > 90_000);
        // Sorted and unique.
        assert!(word_offsets().windows(2).all(|w| word_at(w[0]) < word_at(w[1])));
    }

    #[test]
    fn word_list_notice_is_the_header() {
        let notice = word_list_notice();
        for needle in ["SCOWL", "Kevin Atkinson", "Princeton University", "J Ross Beresford", "Geoff Kuenning"] {
            assert!(notice.contains(needle), "{needle}");
        }
        assert!(notice.starts_with("en-words.txt:") && notice.ends_with("M Cooper <thegrendel@theriver.com>"));
        assert!(!notice.lines().any(|l| l.starts_with('#')) && !notice.contains("\naachen"));
    }

    #[test]
    fn options_from_settings() {
        let s = TextSettings { skip_urls: false, skip_emails: true, ..TextSettings::default() };
        assert_eq!(CleanOptions::from(&s), CleanOptions { skip_urls: false, skip_emails: true });
        assert_eq!(CleanOptions::from(&TextSettings::default()), DEFAULTS);
    }

    /// A small deterministic generator (xorshift) for the no-panic loop.
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

    const FRAGMENTS: &[&str] = &[
        "a", "Word", " ", "  ", "\n", "\n\n", "\r", "\r\n", "\t", "-", "\u{2010}", "\u{AC}", "*", "**", "_", "__", "`",
        "~~", "#", "## ", "> ", "- ", "1. ", "a) ", "[", "]", "(", ")", "![", "](", "\"", "'", "<", ">", "&", "&amp;",
        "&#160;", "&#xD800;", "&lt", ";", "https://", "http://x.com/", "www.", "ftp://", "@", "foo@bar.com", "mailto:",
        ".", ",", ":", "!", "?", "\u{E000}", "\u{200B}", "\u{FEFF}", "\u{2028}", "\u{2029}", "\u{85}", "\u{A0}",
        "\u{3000}", "中", "。", "、", "（", "）", "😀", "𝐀", "é", "Москва", "```", "~~~", "***", "===", "and", "or",
        "Dr.", "\u{0}", "\u{1}", "\u{FFFD}",
    ];

    #[test]
    fn no_panics_on_random_input() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        for round in 0..1500 {
            let len = 1 + rng.below(if round % 50 == 0 { 400 } else { 60 });
            let mut text = String::new();
            for _ in 0..len {
                if rng.below(8) == 0 {
                    // Arbitrary code points, including astral ones.
                    let cp = match rng.below(3) {
                        0 => rng.below(0x80) as u32,
                        1 => rng.below(0x1_0000) as u32,
                        _ => rng.below(0x11_0000) as u32,
                    };
                    if let Some(c) = char::from_u32(cp) {
                        text.push(c);
                    }
                } else {
                    text.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
                }
            }
            for opts in [BOTH, NONE, DEFAULTS] {
                let out = clean_text(&text, opts);
                // The whitespace invariant: only ' ' and '\n' survive, trimmed, no runs of 3+ newlines.
                assert!(out.chars().all(|c| !is_ws(c) || c == ' ' || c == '\n'), "{text:?} → {out:?}");
                assert_eq!(js_trim(&out), out);
                assert!(!out.contains("\n\n\n") && !out.contains("  ") && !out.contains(HOLE), "{text:?} → {out:?}");
                // Downstream consumers of cleaned text must not panic either.
                let _ = crate::chunk::chunk_text_default(&out);
                let _ = crate::segment::sentence_starts(&out);
            }
        }
    }
}
