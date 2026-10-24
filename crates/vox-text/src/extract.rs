//! File → plain text for the Explorer verb / `--read-file` (integrations.md §5). Reads at most 5 MiB, decodes
//! (BOMs, UTF-16 sniffing, strict UTF-8, lossy UTF-8 vs cp1252 heuristics), rejects binary, converts `.html/.htm/
//! .xhtml`, `.rtf` (when it starts with `{\rtf`) and `.md/.markdown`, keeps other text verbatim (newlines normalised,
//! NULs dropped, trimmed), then truncates to `max_chars` UTF-16 units at a whitespace boundary.
//!
//! Golden vectors: integrations.md §5.10. Decision (ARCHITECTURE.md §8): RTF code pages 932/936/950 map to
//! Shift_JIS/GBK/Big5 via `encoding_rs` (an improvement over the original's cp1252 fallback; the 932 golden row
//! changes accordingly).
//!
//! Porting notes: the scanners run over the same units as the original where lengths matter (the RTF parser and
//! `\uN` work on UTF-16 code units; the truncation and binary sample count UTF-16 units); JS `\s`/`trim()` use the
//! ECMAScript whitespace set; JS `\b` is the ASCII word boundary.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

use encoding_rs::{Encoding, BIG5, GBK, SHIFT_JIS, WINDOWS_1252};
use regex::{Captures, Regex};
use vox_core::constants::MAX_FILE_BYTES;
use vox_core::js::{js_trim, js_trim_end};

use crate::util::{is_ascii_word, is_high_surrogate, is_line_terminator, is_space_unit, is_ws, string_of, units, WS};

/// Why a file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("Not a regular file")]
    NotAFile,
    #[error("This file does not look like text")]
    Binary,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// `readTextFile(path, maxChars)`. The caller passes `settings.text.maxChars + 1000`.
pub fn read_text_file(path: &Path, max_chars: usize) -> Result<String, ExtractError> {
    // A directory may not even open on Windows; report it as "not a file" rather than an I/O error.
    if std::fs::metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(ExtractError::NotAFile);
    }
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(ExtractError::NotAFile);
    }
    let size = meta.len();
    let truncated = size > MAX_FILE_BYTES;
    let cap = size.min(MAX_FILE_BYTES);
    let mut bytes = Vec::with_capacity(usize::try_from(cap).unwrap_or(0));
    // Reads until full or EOF and keeps what was read (a file that shrank meanwhile is fine).
    file.take(cap).read_to_end(&mut bytes)?;

    let text = decode_text_buffer(&bytes, truncated);
    if looks_binary(&text) {
        return Err(ExtractError::Binary);
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let text = match ext.as_str() {
        "html" | "htm" | "xhtml" => html_to_text(&text),
        "rtf" if starts_like_rtf(&text) => rtf_to_text(&text),
        "md" | "markdown" => markdown_to_text(&text),
        _ => plain_text(&text),
    };
    Ok(truncate_extracted(&text, max_chars))
}

/// `/^\s*\{\\rtf/`.
fn starts_like_rtf(text: &str) -> bool {
    text.trim_start_matches(is_ws).starts_with("{\\rtf")
}

/// `normalizeNewlines(t).replace(/\0/g, '').trim()`: JSON/CSV/logs verbatim otherwise.
fn plain_text(text: &str) -> String {
    let t = normalize_newlines(text).replace('\0', "");
    js_trim(&t).to_owned()
}

// ───────────────────────────── decoding ─────────────────────────────

/// `decodeTextBuffer(bytes, truncated)` (integrations.md §5.2). `truncated` = the bytes are a prefix of a longer file.
pub fn decode_text_buffer(bytes: &[u8], truncated: bool) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return decode_utf8(rest, truncated);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, false);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, true);
    }
    match sniff_utf16(bytes) {
        Some(big_endian) => decode_utf16(bytes, big_endian),
        None => decode_utf8(bytes, truncated),
    }
}

/// `TextDecoder('utf-8')` removes one leading U+FEFF (strict and lossy alike).
fn strip_one_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

fn decode_utf8(data: &[u8], truncated: bool) -> String {
    let data = if truncated { trim_incomplete_utf8(data) } else { data };
    match std::str::from_utf8(data) {
        Ok(s) => strip_one_bom(s).to_owned(),
        Err(_) => {
            // A legacy ANSI file makes nearly every high byte invalid; a UTF-8 file with a stray byte only a few.
            let loose = String::from_utf8_lossy(data);
            let replacements = loose.chars().filter(|&c| c == '\u{FFFD}').count();
            let high_bytes = data.iter().filter(|&&b| b >= 0x80).count();
            if replacements * 4 < high_bytes {
                strip_one_bom(&loose).to_owned()
            } else {
                decode_windows_1252(data)
            }
        }
    }
}

/// Drops a multi-byte sequence cut short by the 5 MiB cap.
fn trim_incomplete_utf8(buf: &[u8]) -> &[u8] {
    let mut i = buf.len();
    let mut continuation = 0;
    while i > 0 && continuation < 3 && buf[i - 1] & 0xC0 == 0x80 {
        i -= 1;
        continuation += 1;
    }
    if i == 0 {
        return buf;
    }
    let lead = buf[i - 1];
    let need = if lead >= 0xF0 {
        3
    } else if lead >= 0xE0 {
        2
    } else if lead >= 0xC0 {
        1
    } else {
        0
    };
    if need > continuation {
        &buf[..i - 1]
    } else {
        buf
    }
}

fn decode_utf16(buf: &[u8], big_endian: bool) -> String {
    // A trailing odd byte is dropped.
    let (pairs, _) = buf.as_chunks::<2>();
    let mut u: Vec<u16> =
        pairs.iter().map(|&p| if big_endian { u16::from_be_bytes(p) } else { u16::from_le_bytes(p) }).collect();
    if u.last().copied().is_some_and(is_high_surrogate) {
        u.pop();
    }
    // Other lone surrogates become U+FFFD (JS kept them; they now count toward `looksBinary`).
    string_of(&u)
}

/// BOM-less UTF-16 by the pattern of zero bytes in mostly-ASCII text: `Some(true)` = big endian.
fn sniff_utf16(buf: &[u8]) -> Option<bool> {
    let n = buf.len().min(4096) & !1;
    if n < 4 {
        return None;
    }
    let (mut even_zeros, mut odd_zeros) = (0usize, 0usize);
    for pair in buf[..n].as_chunks::<2>().0 {
        even_zeros += usize::from(pair[0] == 0);
        odd_zeros += usize::from(pair[1] == 0);
    }
    let pairs = (n / 2) as f64;
    let (even, odd) = (even_zeros as f64, odd_zeros as f64);
    if odd > pairs * 0.4 && even < pairs * 0.05 {
        Some(false)
    } else if even > pairs * 0.4 && odd < pairs * 0.05 {
        Some(true)
    } else {
        None
    }
}

/// WHATWG windows-1252 (= the original's table: 0x80–0x9F mapped, `81 8D 8F 90 9D` kept as C1 controls).
fn decode_windows_1252(bytes: &[u8]) -> String {
    WINDOWS_1252.decode_without_bom_handling(bytes).0.into_owned()
}

/// The cp1252 meaning of a C1 control code point (HTML numeric references in 0x80..=0x9F).
fn cp1252_c1(cp: u32) -> u32 {
    match u8::try_from(cp) {
        Ok(b @ 0x80..=0x9F) => decode_windows_1252(&[b]).chars().next().map_or(cp, u32::from),
        _ => cp,
    }
}

/// `decodeCodepage(bytes, cp)` for RTF `\'hh` runs.
fn decode_codepage(bytes: &[u8], codepage: i64) -> String {
    match codepage {
        0 | 1252 => decode_windows_1252(bytes),
        65001 => strip_one_bom(&String::from_utf8_lossy(bytes)).to_owned(),
        // D-INT-4: not WHATWG labels, so the original fell back to cp1252 mojibake.
        932 => SHIFT_JIS.decode_without_bom_handling(bytes).0.into_owned(),
        936 => GBK.decode_without_bom_handling(bytes).0.into_owned(),
        950 => BIG5.decode_without_bom_handling(bytes).0.into_owned(),
        cp => match Encoding::for_label(format!("windows-{cp}").as_bytes()) {
            Some(enc) => enc.decode_without_bom_handling(bytes).0.into_owned(),
            None => decode_windows_1252(bytes),
        },
    }
}

/// `looksBinary(text)` (integrations.md §5.3).
pub fn looks_binary(text: &str) -> bool {
    let mut len = 0usize;
    let mut control = 0usize;
    for u in text.encode_utf16().take(8192) {
        len += 1;
        if (u < 0x20 && !matches!(u, 0x09 | 0x0A | 0x0C | 0x0D)) || u == 0xFFFD {
            control += 1;
        }
    }
    len > 0 && control as f64 / len as f64 > 0.1
}

// ───────────────────────────── HTML ─────────────────────────────

/// Elements whose content is never prose.
const HTML_SKIP: &[&str] =
    &["script", "style", "noscript", "template", "svg", "math", "iframe", "object", "canvas", "title", "select", "head"];
/// Elements that break lines.
const HTML_BLOCK: &[&str] = &[
    "address", "article", "aside", "blockquote", "body", "caption", "dd", "details", "dialog", "div", "dl", "dt",
    "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "html",
    "li", "main", "nav", "ol", "p", "pre", "section", "summary", "table", "tbody", "td", "tfoot", "th", "thead", "tr",
    "ul",
];
/// Elements that break paragraphs.
const HTML_PARAGRAPH: &[&str] =
    &["p", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote", "pre", "table", "ul", "ol", "section", "article"];

/// The output of the HTML scanner: line breaks requested by block tags are held back until the next real text, so
/// nested blocks produce one break instead of a pile of blank lines (1 = line, 2 = paragraph, more via `<br>`).
struct HtmlOut {
    out: String,
    pending: usize,
}

impl HtmlOut {
    fn push_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.pending > 0 && s.chars().any(|c| !is_ws(c)) {
            self.out.extend(std::iter::repeat_n('\n', self.pending));
            self.pending = 0;
        }
        self.out.push_str(s);
    }

    fn break_line(&mut self, level: usize) {
        self.pending = self.pending.max(level);
    }
}

/// `htmlToText` including entities and tidy (integrations.md §5.4–§5.5). Linear-time; entities are decoded after
/// the tags are gone (so `&lt;b&gt;` stays literal text).
pub fn html_to_text(html: &str) -> String {
    let b = html.as_bytes();
    let n = b.len();
    let mut o = HtmlOut { out: String::with_capacity(n), pending: 0 };
    let mut head_unclosed_from: Option<usize> = None;
    let mut i = 0usize;
    while i < n {
        let Some(lt) = find_byte(b, b'<', i) else {
            o.push_text(&html[i..]);
            break;
        };
        if lt > i {
            o.push_text(&html[i..lt]);
        }
        if b[lt..].starts_with(b"<!--") {
            i = find_bytes(b, b"-->", lt + 4).map_or(n, |e| e + 3);
            continue;
        }
        // `/<(\/?)([a-zA-Z][a-zA-Z0-9:-]*)/y`
        let closing = b.get(lt + 1) == Some(&b'/');
        let name_start = lt + 1 + usize::from(closing);
        if !b.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            if matches!(b.get(lt + 1), Some(b'!' | b'?')) {
                // <!DOCTYPE>, <![CDATA[, <?xml?>: dropped up to the first '>'.
                i = find_byte(b, b'>', lt).map_or(n, |e| e + 1);
            } else {
                o.push_text("<"); // a literal "<" in text ("3 < 5")
                i = lt + 1;
            }
            continue;
        }
        let mut j = name_start;
        while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b':' || b[j] == b'-') {
            j += 1;
        }
        let name = html[name_start..j].to_ascii_lowercase();
        // The tag's closing '>' honouring quoted attribute values (which may contain '>').
        let mut quote: Option<u8> = None;
        let mut prev = 0u8;
        while j < n {
            let c = b[j];
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
            } else if (c == b'"' || c == b'\'') && prev == b'=' {
                quote = Some(c);
            } else if c == b'>' {
                break;
            }
            if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                prev = c;
            }
            j += 1;
        }
        let self_closing = b[j - 1] == b'/';
        let tag_end = j; // index of '>' (or n)
        i = j + 1;

        if !closing && !self_closing && HTML_SKIP.contains(&name.as_str()) {
            if name == "head" {
                // <head> may legally lack its end tag: only skip it when one exists. Once a search found none,
                // no later one can (this keeps a page of unclosed <head> tags linear).
                if head_unclosed_from.is_some_and(|from| i >= from) {
                    continue;
                }
                match find_closing_tag(b, &name, i) {
                    Some(e) => i = e,
                    None => head_unclosed_from = Some(i),
                }
            } else {
                let end = find_closing_tag(b, &name, i);
                // An unclosed <script> & co drops the rest of the document.
                i = end.unwrap_or(n);
            }
            continue;
        }
        match name.as_str() {
            "br" => {
                if o.pending > 0 {
                    o.break_line(o.pending + 1);
                } else {
                    o.out.push('\n');
                }
            }
            // Cell separator, pushed directly (it does not flush a pending break); td/th never break lines.
            "td" | "th" => {
                if closing {
                    o.out.push('\t');
                }
            }
            "img" if !closing => {
                if let Some(alt) = img_alt(&html[lt..tag_end.min(n)]) {
                    let alt = js_trim(alt);
                    if !alt.is_empty() {
                        o.push_text(&format!(" {alt} "));
                    }
                }
            }
            other => {
                if HTML_PARAGRAPH.contains(&other) {
                    o.break_line(2);
                } else if HTML_BLOCK.contains(&other) {
                    o.break_line(1);
                }
                // Every other (inline) tag is removed with no space inserted ("foo<span>bar</span>" → "foobar").
            }
        }
    }
    tidy_whitespace(&decode_entities(&o.out, false))
}

fn find_byte(b: &[u8], needle: u8, from: usize) -> Option<usize> {
    b.get(from..)?.iter().position(|&c| c == needle).map(|p| p + from)
}

fn find_bytes(b: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    b.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

/// Index just past the first `/<\/name\b[^>]*>/i` at or after `from`.
fn find_closing_tag(b: &[u8], name: &str, from: usize) -> Option<usize> {
    let name = name.as_bytes();
    let mut p = from;
    while let Some(lt) = find_bytes(b, b"</", p) {
        let after = lt + 2 + name.len();
        let name_ok = b.get(lt + 2..after).is_some_and(|s| s.eq_ignore_ascii_case(name));
        let boundary = b.get(after).is_none_or(|&c| !is_ascii_word(char::from(c)));
        if name_ok && boundary {
            // `[^>]*>`: if no '>' follows this candidate, none follows a later one either.
            return find_byte(b, b'>', after).map(|e| e + 1);
        }
        p = lt + 1;
    }
    None
}

/// `/\balt\s*=\s*("([^"]*)"|'([^']*)')/i` (ASCII `\b`, so `data-alt=` matches too); quoted values only.
fn img_alt(tag: &str) -> Option<&str> {
    let b = tag.as_bytes();
    let mut from = 0;
    while let Some(p) = (from..b.len().saturating_sub(2)).find(|&p| b[p..p + 3].eq_ignore_ascii_case(b"alt")) {
        from = p + 1;
        if p > 0 && is_ascii_word(char::from(b[p - 1])) {
            continue;
        }
        let rest = &tag[p + 3..];
        let rest = rest.trim_start_matches(is_ws);
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start_matches(is_ws);
        let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        if let Some(close) = rest[1..].find(q) {
            return Some(&rest[1..1 + close]);
        }
    }
    None
}

/// Legacy entities browsers accept without the trailing semicolon.
const SEMICOLON_OPTIONAL: &[&str] = &["amp", "lt", "gt", "quot", "nbsp", "copy", "reg"];

/// The named-entity table of integrations.md §5.5 (case-sensitive).
fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" | "ensp" | "emsp" | "thinsp" => " ",
        "ndash" => "\u{2013}",
        "mdash" => "\u{2014}",
        "hellip" => "\u{2026}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "sbquo" => "\u{201A}",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "bdquo" => "\u{201E}",
        "laquo" => "\u{AB}",
        "raquo" => "\u{BB}",
        "lsaquo" => "\u{2039}",
        "rsaquo" => "\u{203A}",
        "bull" => "\u{2022}",
        "middot" => "\u{B7}",
        "copy" => "\u{A9}",
        "reg" => "\u{AE}",
        "trade" => "\u{2122}",
        "deg" => "\u{B0}",
        "plusmn" => "\u{B1}",
        "times" => "\u{D7}",
        "divide" => "\u{F7}",
        "minus" => "\u{2212}",
        "frac12" => "\u{BD}",
        "frac14" => "\u{BC}",
        "frac34" => "\u{BE}",
        "euro" => "\u{20AC}",
        "pound" => "\u{A3}",
        "yen" => "\u{A5}",
        "cent" => "\u{A2}",
        "sect" => "\u{A7}",
        "para" => "\u{B6}",
        "dagger" => "\u{2020}",
        "Dagger" => "\u{2021}",
        "prime" => "\u{2032}",
        "Prime" => "\u{2033}",
        "larr" => "\u{2190}",
        "rarr" => "\u{2192}",
        "uarr" => "\u{2191}",
        "darr" => "\u{2193}",
        "harr" => "\u{2194}",
        "shy" | "zwj" | "zwnj" | "lrm" | "rlm" => "",
        "iexcl" => "\u{A1}",
        "iquest" => "\u{BF}",
        "micro" => "\u{B5}",
        "alpha" => "\u{3B1}",
        "beta" => "\u{3B2}",
        "gamma" => "\u{3B3}",
        "delta" => "\u{3B4}",
        "pi" => "\u{3C0}",
        "mu" => "\u{3BC}",
        "sigma" => "\u{3C3}",
        "le" => "\u{2264}",
        "ge" => "\u{2265}",
        "ne" => "\u{2260}",
        "asymp" => "\u{2248}",
        "infin" => "\u{221E}",
        "check" => "\u{2713}",
        "star" => "\u{2606}",
        "aacute" => "\u{E1}",
        "eacute" => "\u{E9}",
        "iacute" => "\u{ED}",
        "oacute" => "\u{F3}",
        "uacute" => "\u{FA}",
        "Aacute" => "\u{C1}",
        "Eacute" => "\u{C9}",
        "Iacute" => "\u{CD}",
        "Oacute" => "\u{D3}",
        "Uacute" => "\u{DA}",
        "agrave" => "\u{E0}",
        "egrave" => "\u{E8}",
        "igrave" => "\u{EC}",
        "ograve" => "\u{F2}",
        "ugrave" => "\u{F9}",
        "Agrave" => "\u{C0}",
        "Egrave" => "\u{C8}",
        "acirc" => "\u{E2}",
        "ecirc" => "\u{EA}",
        "icirc" => "\u{EE}",
        "ocirc" => "\u{F4}",
        "ucirc" => "\u{FB}",
        "auml" => "\u{E4}",
        "euml" => "\u{EB}",
        "iuml" => "\u{EF}",
        "ouml" => "\u{F6}",
        "uuml" => "\u{FC}",
        "yuml" => "\u{FF}",
        "Auml" => "\u{C4}",
        "Ouml" => "\u{D6}",
        "Uuml" => "\u{DC}",
        "szlig" => "\u{DF}",
        "ccedil" => "\u{E7}",
        "Ccedil" => "\u{C7}",
        "ntilde" => "\u{F1}",
        "Ntilde" => "\u{D1}",
        "atilde" => "\u{E3}",
        "otilde" => "\u{F5}",
        "aring" => "\u{E5}",
        "Aring" => "\u{C5}",
        "aelig" => "\u{E6}",
        "AElig" => "\u{C6}",
        "oslash" => "\u{F8}",
        "Oslash" => "\u{D8}",
        "oelig" => "\u{153}",
        "OElig" => "\u{152}",
        _ => return None,
    })
}

/// `&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z][a-zA-Z0-9]{1,31})(;?)` (the digit counts include leading zeros).
static ENTITY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z][a-zA-Z0-9]{1,31})(;?)").expect("valid regex")
});
/// The same references, `;`-terminated only (D-TXT-1).
static ENTITY_STRICT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z][a-zA-Z0-9]{1,31})(;)").expect("valid regex")
});

/// `decodeHtmlEntities`; with `semicolon_required` only `;`-terminated references are decoded (cleanText's D-TXT-1:
/// `&lt=2` in a pasted URL query stays literal).
pub(crate) fn decode_entities(text: &str, semicolon_required: bool) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let re = if semicolon_required { &ENTITY_STRICT_RE } else { &ENTITY_RE };
    re.replace_all(text, |caps: &Captures| -> String {
        let whole = caps.get(0).map_or("", |m| m.as_str());
        let body = caps.get(1).map_or("", |m| m.as_str());
        let semi = caps.get(2).is_some_and(|m| !m.as_str().is_empty());
        if let Some(num) = body.strip_prefix('#') {
            let cp = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16),
                None => num.parse::<u32>(),
            }
            .unwrap_or(0);
            let cp = cp1252_c1(cp);
            return match char::from_u32(cp) {
                Some(c) if cp != 0 => c.to_string(),
                _ => "\u{FFFD}".to_owned(),
            };
        }
        match named_entity(body) {
            Some(v) if semi || SEMICOLON_OPTIONAL.contains(&body) => v.to_owned(),
            _ => whole.to_owned(),
        }
    })
    .into_owned()
}

// ───────────────────────────── RTF ─────────────────────────────

/// Destinations whose content is metadata, not body text.
const RTF_SKIP: &[&str] = &[
    "fonttbl", "colortbl", "stylesheet", "info", "pict", "object", "objdata", "header", "headerl", "headerr", "headerf",
    "footer", "footerl", "footerr", "footerf", "footnote", "annotation", "fldinst", "xmlnstbl", "listtable",
    "listoverridetable", "rsidtbl", "generator", "themedata", "colorschememapping", "datastore", "latentstyles",
    "pgdsctbl", "filetbl", "revtbl", "bkmkstart", "bkmkend", "nonshppict", "shpinst", "pntext", "pntxta", "pntxtb",
    "private", "userprops", "docvar", "xe", "tc", "template", "ftnsep", "ftnsepc", "ftncn", "aftnsep", "aftnsepc",
    "aftncn", "mmathPr", "wgrffmtfilter", "listpicture", "protusertbl", "defchp", "defpap",
];

/// Control words that stand for text (exact keys only).
fn rtf_symbol(word: &str) -> Option<&'static str> {
    Some(match word {
        "par" | "line" | "row" => "\n",
        "sect" | "page" => "\n\n",
        "tab" | "cell" => "\t",
        "emdash" => "\u{2014}",
        "endash" => "\u{2013}",
        "bullet" => "\u{2022}",
        "lquote" => "\u{2018}",
        "rquote" => "\u{2019}",
        "ldblquote" => "\u{201C}",
        "rdblquote" => "\u{201D}",
        "emspace" | "enspace" | "qmspace" => " ",
        "zwj" | "zwnj" | "zwbo" | "zwnbo" => "",
        _ => return None,
    })
}

#[derive(Clone, Copy)]
struct RtfGroup {
    skip: bool,
    uc: i64,
}

impl Default for RtfGroup {
    fn default() -> Self {
        Self { skip: false, uc: 1 }
    }
}

struct RtfOut {
    out: Vec<u16>,
    group: RtfGroup,
    pending_bytes: Vec<u8>,
    codepage: i64,
}

impl RtfOut {
    fn emit_units(&mut self, s: &[u16]) {
        if !self.group.skip {
            self.out.extend_from_slice(s);
        }
    }

    fn emit(&mut self, s: &str) {
        if !self.group.skip {
            self.out.extend(s.encode_utf16());
        }
    }

    /// Decodes the pending `\'hh` bytes in the document code page (subject to the current group's skip).
    fn flush_bytes(&mut self) {
        if !self.pending_bytes.is_empty() {
            let text = decode_codepage(&self.pending_bytes, self.codepage);
            self.pending_bytes.clear();
            self.emit(&text);
        }
    }
}

fn is_ascii_letter_unit(u: Option<&u16>) -> bool {
    u.is_some_and(|&c| c < 0x80 && (c as u8).is_ascii_alphabetic())
}

fn is_digit_unit(u: Option<&u16>) -> bool {
    u.is_some_and(|&c| c < 0x80 && (c as u8).is_ascii_digit())
}

fn hex_value(u: Option<&u16>) -> Option<u8> {
    let c = *u?;
    if c >= 0x80 {
        return None;
    }
    char::from(c as u8).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

/// `rtfToText` (integrations.md §5.6): drops control words and metadata groups, keeps paragraphs, `\uN` and `\'hh`
/// characters. Runs over UTF-16 units like the original (`\uN` emits one unit; astral characters arrive as two).
pub fn rtf_to_text(rtf: &str) -> String {
    let s = units(rtf);
    let n = s.len();
    let mut stack: Vec<RtfGroup> = Vec::new();
    let mut o = RtfOut { out: Vec::with_capacity(n), group: RtfGroup::default(), pending_bytes: Vec::new(), codepage: 1252 };
    let mut uc_skip: i64 = 0;
    let mut i = 0usize;
    let unit = |c: char| c as u16;

    while i < n {
        let c = s[i];
        if c == unit('{') {
            o.flush_bytes();
            stack.push(o.group);
            uc_skip = 0;
            i += 1;
        } else if c == unit('}') {
            o.flush_bytes();
            o.group = stack.pop().unwrap_or_default();
            uc_skip = 0;
            i += 1;
        } else if c == unit('\\') {
            let Some(&next) = s.get(i + 1) else {
                break;
            };
            if next == unit('\'') {
                // \'hh: one byte in the ANSI code page (two hex digits, or one followed by a non-hex char).
                let byte = hex_value(s.get(i + 2)).map(|hi| match hex_value(s.get(i + 3)) {
                    Some(lo) => hi * 16 + lo,
                    None => hi,
                });
                i += 4;
                if uc_skip > 0 {
                    uc_skip -= 1; // it is the ANSI fallback of a preceding \uN
                } else if let Some(byte) = byte {
                    o.pending_bytes.push(byte); // consecutive \'hh accumulate (DBCS pairs decode together)
                }
                continue;
            }
            o.flush_bytes();
            if is_ascii_letter_unit(Some(&next)) {
                // Control word: ≤ 32 letters, an optional parameter of ≤ 11 chars including '-', one delimiter space.
                let mut j = i + 1;
                while j < n && j - i <= 32 && is_ascii_letter_unit(s.get(j)) {
                    j += 1;
                }
                let word = string_of(&s[i + 1..j]);
                let mut k = j;
                if s.get(k) == Some(&unit('-')) && is_digit_unit(s.get(k + 1)) {
                    k += 1;
                }
                let param: Option<i64> = if is_digit_unit(s.get(k)) {
                    while k < n && k - j < 11 && is_digit_unit(s.get(k)) {
                        k += 1;
                    }
                    string_of(&s[j..k]).parse::<i64>().ok()
                } else {
                    k = j;
                    None
                };
                if s.get(k) == Some(&unit(' ')) {
                    k += 1;
                }
                i = k;
                uc_skip = 0;

                if word == "bin" {
                    // Skip the raw binary payload.
                    let skip = usize::try_from(param.unwrap_or(0).max(0)).unwrap_or(usize::MAX);
                    i = i.saturating_add(skip);
                } else if RTF_SKIP.contains(&word.as_str()) {
                    o.group.skip = true;
                } else if word == "ansicpg" && param.is_some() {
                    o.codepage = param.unwrap_or(1252); // \ansicpg0 → 0 → treated as 1252
                } else if word == "uc" {
                    o.group.uc = param.unwrap_or(1).max(0);
                } else if let ("u", Some(p)) = (word.as_str(), param) {
                    // \u0 counts too (U+0000 is removed by tidyWhitespace).
                    let u = p.rem_euclid(65536) as u16;
                    o.emit_units(&[u]);
                    uc_skip = o.group.uc;
                } else if let Some(sym) = rtf_symbol(&word) {
                    o.emit(sym);
                }
                // Every other control word is ignored.
                continue;
            }
            // Control symbols.
            i += 2;
            let literal = match next {
                x if x == unit('\\') || x == unit('{') || x == unit('}') => Some(x),
                x if x == unit('~') => Some(unit(' ')),
                x if x == unit('_') => Some(unit('-')),
                _ => None,
            };
            if let Some(l) = literal {
                if uc_skip > 0 {
                    uc_skip -= 1;
                } else {
                    o.emit_units(&[l]);
                }
            } else if next == unit('*') {
                o.group.skip = true; // an ignorable destination we do not understand
            } else if next == unit('\n') || next == unit('\r') {
                o.emit("\n"); // backslash-newline = \par
            }
            // Others (\- optional hyphen, \: …) are ignored and leave uc_skip alone.
        } else if c == unit('\r') || c == unit('\n') {
            i += 1; // raw line breaks are not content in RTF
        } else {
            let mut j = i;
            while j < n && ![unit('\\'), unit('{'), unit('}'), unit('\r'), unit('\n')].contains(&s[j]) {
                j += 1;
            }
            o.flush_bytes();
            let mut run = &s[i..j];
            i = j;
            if uc_skip > 0 {
                let skip = usize::try_from(uc_skip).unwrap_or(usize::MAX).min(run.len());
                run = &run[skip..];
                uc_skip -= i64::try_from(skip).unwrap_or(0);
            }
            if !run.is_empty() {
                o.emit_units(run);
            }
        }
    }
    o.flush_bytes();
    tidy_whitespace(&string_of(&o.out))
}

// ───────────────────────────── Markdown ─────────────────────────────

static MD_FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^[{WS}]*(?:```|~~~)")).expect("valid regex"));
static MD_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^[{WS}]{{0,3}}#{{1,6}}[{WS}]+")).expect("valid regex"));
static MD_CLOSING_HASHES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"[{WS}]+#+[{WS}]*$")).expect("valid regex"));
static MD_QUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^[{WS}]*>[{WS}]?")).expect("valid regex"));
static MD_TASK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^([{WS}]*)[-*+][{WS}]+\[[ xX]\][{WS}]+")).expect("valid regex"));
static MD_BULLET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^([{WS}]*)[-*+][{WS}]+")).expect("valid regex"));
static MD_IMAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\([^)]*\)").expect("valid regex"));
static MD_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]*\)").expect("valid regex"));
static MD_REF_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\[[^\]]*\]").expect("valid regex"));
static MD_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").expect("valid regex"));
static MD_LINK_DEF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^[{WS}]*\[[^\]]+\]:[{WS}]*[^{WS}]+")).expect("valid regex"));

/// `markdownToText` (integrations.md §5.7): light cleanup so syntax characters are not read aloud; prose untouched.
pub fn markdown_to_text(md: &str) -> String {
    let normalized = normalize_newlines(md);
    let mut kept: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw in normalized.split('\n') {
        if MD_FENCE.is_match(raw) {
            in_fence = !in_fence; // ``` and ~~~ close each other
            continue;
        }
        if in_fence {
            kept.push(raw.to_owned());
            continue;
        }
        if is_thematic_break(raw) {
            kept.push(String::new());
            continue;
        }
        let line = MD_HEADING.replacen(raw, 1, "");
        let line = MD_CLOSING_HASHES.replacen(&line, 1, "");
        let line = MD_QUOTE.replacen(&line, 1, "");
        let line = MD_TASK.replacen(&line, 1, "${1}");
        let line = MD_BULLET.replacen(&line, 1, "${1}");
        let line = MD_IMAGE.replace_all(&line, "${1}");
        let line = MD_LINK.replace_all(&line, "${1}");
        let line = MD_REF_LINK.replace_all(&line, "${1}");
        let line = MD_CODE.replace_all(&line, "${1}");
        let line = md_bold(&line);
        let line = md_italic(&line);
        let line = md_strike(&line);
        if MD_LINK_DEF.is_match(&line) {
            continue; // link reference definitions
        }
        kept.push(line);
    }
    tidy_whitespace(&kept.join("\n"))
}

/// `^\s*([-*_])(\s*\1){2,}\s*$`: three or more of the same marker, whitespace anywhere.
pub(crate) fn is_thematic_break(line: &str) -> bool {
    let mut marks = line.chars().filter(|c| !is_ws(*c));
    let Some(first) = marks.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut count = 1;
    for c in marks {
        if c != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

/// JS `.` (no `s` flag): anything but a line terminator.
fn is_dot(c: char) -> bool {
    !is_line_terminator(c)
}

/// `/(D)(?=\S)(.+?)(?<=\S)D/g → "$1"` for a two-char delimiter `D` (patterns 10 and 12).
///
/// The lazy content is the shortest run of `.` chars ending in `\S` and followed by `D`. Unlike the cleanText
/// patterns it may contain `D` itself, so a naive scan per opener is quadratic on lines full of unclosed openers
/// (V8 is too); the first closing candidate and the first line terminator at or after every position are
/// precomputed instead, which gives the same match in linear time.
fn md_delimited(line: &str, delims: &[[char; 2]]) -> String {
    let c: Vec<char> = line.chars().collect();
    let n = c.len();
    let mut next_non_dot = vec![n; n + 1];
    for i in (0..n).rev() {
        next_non_dot[i] = if is_dot(c[i]) { next_non_dot[i + 1] } else { i };
    }
    let next_close: Vec<Vec<usize>> = delims
        .iter()
        .map(|d| {
            let mut v = vec![n; n + 1];
            for k in (0..n).rev() {
                let closes = !is_ws(c[k]) && c.get(k + 1) == Some(&d[0]) && c.get(k + 2) == Some(&d[1]);
                v[k] = if closes { k } else { v[k + 1] };
            }
            v
        })
        .collect();
    let mut out = String::with_capacity(line.len());
    let mut p = 0;
    'outer: while p < n {
        for (d, close) in delims.iter().zip(&next_close) {
            if p + 1 < n && c[p] == d[0] && c[p + 1] == d[1] {
                let start = p + 2;
                if start < n && !is_ws(c[start]) {
                    let k = close[start];
                    if k < n && k < next_non_dot[start] {
                        out.extend(&c[start..=k]);
                        p = k + 3;
                        continue 'outer;
                    }
                }
            }
        }
        out.push(c[p]);
        p += 1;
    }
    out
}

/// Pattern 10: `/(\*\*|__)(?=\S)(.+?)(?<=\S)\1/g → "$2"`.
fn md_bold(line: &str) -> String {
    if !line.contains("**") && !line.contains("__") {
        return line.to_owned();
    }
    md_delimited(line, &[['*', '*'], ['_', '_']])
}

/// Pattern 12: `/~~(?=\S)(.+?)(?<=\S)~~/g → "$1"`.
fn md_strike(line: &str) -> String {
    if !line.contains("~~") {
        return line.to_owned();
    }
    md_delimited(line, &[['~', '~']])
}

/// Pattern 11: `/(^|[\s(])[*_](?=\S)([^*_\n]+?)(?<=\S)[*_](?=[\s).,!?:;]|$)/g → "$1$2"` (open and close need not
/// match; `^`/`$` are the line's ends).
fn md_italic(line: &str) -> String {
    if !line.contains(['*', '_']) {
        return line.to_owned();
    }
    let c: Vec<char> = line.chars().collect();
    let n = c.len();
    // The end of a match whose opener is at `q`.
    let try_at = |q: usize| -> Option<usize> {
        if !matches!(c.get(q), Some('*' | '_')) {
            return None;
        }
        let start = q + 1;
        if start >= n || is_ws(c[start]) {
            return None;
        }
        let mut k = start;
        while k < n && !matches!(c[k], '*' | '_' | '\n') {
            if !is_ws(c[k]) && matches!(c.get(k + 1), Some('*' | '_')) {
                let after = c.get(k + 2);
                if after.is_none_or(|&a| is_ws(a) || matches!(a, ')' | '.' | ',' | '!' | '?' | ':' | ';')) {
                    return Some(k + 2);
                }
            }
            k += 1;
        }
        None
    };
    let mut out = String::with_capacity(line.len());
    let mut p = 0;
    while p < n {
        if p == 0 {
            if let Some(end) = try_at(0) {
                out.extend(&c[1..end - 1]);
                p = end;
                continue;
            }
        }
        if is_ws(c[p]) || c[p] == '(' {
            if let Some(end) = try_at(p + 1) {
                out.push(c[p]);
                out.extend(&c[p + 2..end - 1]);
                p = end;
                continue;
            }
        }
        out.push(c[p]);
        p += 1;
    }
    out
}

// ───────────────────────────── shared ─────────────────────────────

/// `/\r\n?/g → "\n"`.
pub(crate) fn normalize_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

static TIDY_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\x0C\x0B\x{A0}]+").expect("valid regex"));
static TIDY_AROUND_NEWLINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" *\n *").expect("valid regex"));
static TIDY_BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").expect("valid regex"));

/// `tidyWhitespace`: collapse runs of spaces/tabs/FF/VT/NBSP, trim around line breaks, keep at most one blank line,
/// drop NULs, JS-trim.
fn tidy_whitespace(text: &str) -> String {
    let t = normalize_newlines(text).replace('\0', "");
    let t = TIDY_SPACES.replace_all(&t, " ");
    let t = TIDY_AROUND_NEWLINE.replace_all(&t, "\n");
    let t = TIDY_BLANK_LINES.replace_all(&t, "\n\n");
    js_trim(&t).to_owned()
}

/// The extractor's own truncation (integrations.md §5.9; differs from the reader's `truncate_text`): cut at
/// `max_chars` UTF-16 units, backed up to the last whitespace within the final `min(200, end / 10)` units, never inside
/// a surrogate pair, then `trimEnd`. `0` means no limit.
pub fn truncate_extracted(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return text.to_owned();
    }
    let u = units(text);
    if u.len() <= max_chars {
        return text.to_owned();
    }
    let mut end = max_chars;
    if is_high_surrogate(u[end - 1]) {
        end -= 1;
    }
    let window = (end / 10).min(200);
    let w = &u[end - window..end];
    // `w.search(/\s\S*$/)`: the last whitespace unit (only non-whitespace follows it); 0 does not move the cut.
    if let Some(space) = w.iter().rposition(|&c| is_space_unit(c)) {
        if space > 0 {
            end = end - window + space;
        }
    }
    js_trim_end(&string_of(&u[..end])).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_golden_vectors() {
        let cases: &[(&str, &str)] = &[
            ("<p>Hello <b>world</b></p><p>Second</p>", "Hello world\n\nSecond"),
            ("<ul><li>One</li><li>Two</li></ul>after", "One\nTwo\n\nafter"),
            ("a<br>b<br><br>c", "a\nb\n\nc"),
            ("<div>x</div><br><div>y</div>", "x\n\ny"),
            ("<table><tr><td>A</td><td>B</td></tr><tr><td>C</td><td>D</td></tr></table>", "A B\nC D"),
            ("<script>var a=\"<p>\";</script>Visible<style>p{}</style>", "Visible"),
            ("<head><title>T</title></head><body>Body</body>", "Body"),
            ("<head><meta charset=utf-8><p>No end head", "No end head"),
            (
                "<!-- c --><!DOCTYPE html>x &lt;b&gt; &amp;amp; &nbsp;y &copy &hellip &#8217; &#x1F600; &#150; &#0;",
                "x <b> &amp; y © &hellip ’ 😀 – \u{FFFD}",
            ),
            ("<img alt=\"A cat\" src=x>and <img alt=dog> <img alt='Bird'/>", "A cat and Bird"),
            ("<a title=\"x>y\" href=\"/\">link</a> 3 < 5 and 5>3", "link 3 < 5 and 5>3"),
            ("foo<span>bar</span> baz", "foobar baz"),
            ("<h1>Title</h1>Text<h2>Sub</h2>", "Title\n\nText\n\nSub"),
            ("<svg><text>no</text></svg>yes<svg/>end", "yesend"),
            ("<script>never closed", ""),
            ("<img alt=\"A &amp; B\">", "A & B"),
            ("<img data-alt=\"x\" src=1>y", "x y"),
            ("<script>a</script-x>b", "b"),
            ("<script/>visible", "visible"),
            ("<a href=a>b>c</a>", "b>c"),
            ("<p>x</p><td>a</td>b", "x\n\na b"),
            ("a</br>b", "a\nb"),
            ("<dl><dt>T</dt><dd>D</dd></dl>", "T\nD"),
            ("a<!-->b-->c", "ac"),
            ("a<![CDATA[x]]>b", "ab"),
            ("a<?xml x?>b", "ab"),
            ("<ul><li>a<ul><li>b</li></ul></li></ul>", "a\n\nb"),
            ("<div class=\"a\"", ""),
            // §5.4 notes.
            ("<a title= \"x>y\">z</a>", "z"),
            ("<pre>  a\n    b</pre>", "a\nb"),
            ("<script>a</scripts>b</script>c", "c"),
            ("<SCRIPT>a</Script >b", "b"),
            ("x<p>", "x"),
            ("é<b>ü</b> 中", "éü 中"),
        ];
        for (input, expected) in cases {
            assert_eq!(html_to_text(input), *expected, "{input:?}");
        }
    }

    #[test]
    fn entity_vectors() {
        let cases: &[(&str, &str)] = &[
            ("&amp", "&"),
            ("&ampx", "&ampx"),
            ("&notanentity;", "&notanentity;"),
            ("&Hellip;", "&Hellip;"),
            ("&#x110000;", "\u{FFFD}"),
            ("&#xD800;", "\u{FFFD}"),
            ("&#128;", "€"),
            ("&#x9F;", "Ÿ"),
            ("&#65", "A"),
            ("&lt&gt", "<>"),
            ("&#x81;", "\u{81}"),
            ("&#00000065;", "\u{6}5;"),
            ("&a;", "&a;"),
            ("&copy &reg &quot &nbsp|", "© ® \"  |"),
            ("&hellip &hellip;", "&hellip …"),
            ("&shy;x&zwj;", "x"),
            ("&star;&check;&minus;&prime;&Prime;", "\u{2606}\u{2713}\u{2212}\u{2032}\u{2033}"),
        ];
        for (input, expected) in cases {
            assert_eq!(decode_entities(input, false), *expected, "{input:?}");
        }
        // The strict variant (cleanText, D-TXT-1) needs the semicolon.
        assert_eq!(decode_entities("&lt=2 &lt; &#65 &#65; &copy=1", true), "&lt=2 < &#65 A &copy=1");
        assert_eq!(decode_entities("&amp;amp;", true), "&amp;");
        assert_eq!(decode_entities("&#12345678;", true), "&#12345678;");
    }

    #[test]
    fn markdown_golden_vectors() {
        let cases: &[(&str, &str)] = &[
            (
                "# Title #\n\nSome **bold** and *italic* and _under_ and `code`.",
                "Title\n\nSome bold and italic and under and code.",
            ),
            (
                "- item one\n* item two\n+ item three\n- [x] done task\n1. numbered",
                "item one\nitem two\nitem three\ndone task\n1. numbered",
            ),
            ("> quote\n>> nested", "quote\n> nested"),
            ("[link](http://x) ![alt](img.png) [ref][1]\n\n[1]: http://example.com", "link alt ref"),
            ("```js\nconst a = **1**;\n```\nafter", "const a = **1**;\nafter"),
            ("---\n***\n- - -", ""),
            ("snake_case_name and 2*3*4 and C #", "snake_case_name and 2*3*4 and C"),
            ("~~strike~~ and __bold__", "strike and bold"),
            ("Line   with    spaces\n\n\n\nnext", "Line with spaces\n\nnext"),
            // §5.7 "more verified outputs".
            ("C #tag", "C #tag"),
            ("Title ##  ", "Title"),
            ("#hashtag", "#hashtag"),
            ("####### x", "####### x"),
            ("    # not heading", "# not heading"),
            ("> > a", "> a"),
            ("a*b*c", "a*b*c"),
            ("(*x_ y", "(x y"),
            ("[](http://x)", "[](http://x)"),
            ("  - [ ] todo", "todo"),
            ("  [x]: http://a", ""),
            ("```\ncode\n~~~\nafter", "code\nafter"),
            ("**a **b** c**", "a **b c**"),
            ("~~ not~~ ~~yes~~", "~~ not~~ yes"),
            ("*em*, done", "em, done"),
            ("x *a* y", "x a y"),
        ];
        for (input, expected) in cases {
            assert_eq!(markdown_to_text(input), *expected, "{input:?}");
        }
    }

    #[test]
    fn rtf_golden_vectors() {
        let cases: &[(&str, &str)] = &[
            (
                r"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0 Arial;}}{\colortbl;\red0\green0\blue0;}\f0\fs24 Hello \b world\b0 !\par Second line\par}",
                "Hello world!\nSecond line",
            ),
            (r"{\rtf1 Caf\'e9 \u8212? dash \uc2\u8364\'80\'80 euro\uc1  \u-10179?\u-8704? emoji}", "Café — dash € euro 😀 emoji"),
            (
                r"{\rtf1{\*\generator Riched20;}{\info{\title T}}Body\line next\tab tab \emdash  \~nbsp\_hyphen \{brace\}}",
                "Body\nnext tab — nbsp-hyphen {brace}",
            ),
            (r"{\rtf1\ansi\ansicpg1251 \'cf\'f0\'e8\'e2\'e5\'f2}", "Привет"),
            // D-INT-4: 932/936/950 decode as Shift_JIS / GBK / Big5.
            (r"{\rtf1\ansi\ansicpg932 \'82\'a0 kana}", "あ kana"),
            (r"{\rtf1\ansi\ansicpg936 \'c4\'e3}", "你"),
            (r"{\rtf1\ansi\ansicpg950 \'a7\'41}", "你"),
            (r#"{\rtf1{\field{\*\fldinst HYPERLINK "http://x"}{\fldrslt link text}}}"#, "link text"),
            ("{\\rtf1 a\\\nb \\bin3 XYZafter}", "a\nb after"),
            (r"{\rtf1 a\u0?b}", "ab"),
            (r"{\rtf1 x\u8212\~y}", "x—y"),
            (r"{\rtf1 x\u8212\_y}", "x—y"),
            (r"{\rtf1 x\u8212\-?y}", "x—y"),
            (r"{\rtf1\uc0 \u8212 x}", "—x"),
            (r"{\rtf1 \u8212{}?}", "—?"),
            (r"{\rtf1\ansicpg949 \'b0\'a1}", "가"),
            (r"{\rtf1\ansicpg65001 \'c3\'a9}", "é"),
            (r"{\rtf1\ansicpg0 \'e9}", "é"),
            (r"{\rtf1 {\*\foo hidden\par}shown}", "shown"),
            // §5.6 edge cases.
            (r"{\rtf1 a\constructor b}", "ab"),
            (r"{\rtf1 \bin-5 x}", "x"),
            (r"{\rtf1 \u-4064? \uN}", "\u{F020}"),
            (r"{\rtf1 \'zz ok}", "ok"),
            (r"{\rtf1 trailing\", "trailing"),
        ];
        for (input, expected) in cases {
            assert_eq!(rtf_to_text(input), *expected, "{input:?}");
        }
        // A 40-letter control word is cut after 32 letters; the rest is text.
        let long = format!("\\{} x", "a".repeat(40));
        assert_eq!(rtf_to_text(&long), "aaaaaaaa x");
        // Parameter digits are limited to 11 characters including the '-': `\u-1234567890` (= U+FD2E as a UTF-16
        // unit), then "12x" is text whose first unit is the skipped `\uN` fallback.
        assert_eq!((-1_234_567_890i64).rem_euclid(65536), 0xFD2E);
        assert_eq!(rtf_to_text(r"{\rtf1 \u-123456789012x}"), "\u{FD2E}2x");
    }

    #[test]
    fn decode_golden_vectors() {
        let cases: &[(&[u8], bool, &str)] = &[
            (&[0xEF, 0xBB, 0xBF, 0x68, 0x69], false, "hi"),
            (&[0xFF, 0xFE, 0x68, 0x00, 0x69, 0x00], false, "hi"),
            (&[0xFE, 0xFF, 0x00, 0x68, 0x00, 0x69], false, "hi"),
            (&[0x68, 0x00, 0x69, 0x00, 0x21, 0x00, 0x21, 0x00], false, "hi!!"),
            (&[0x43, 0x61, 0x66, 0xE9, 0x20, 0x93, 0x71, 0x94], false, "Café “q”"),
            (&[0x61, 0xE2, 0x82], true, "a"),
            (&[0x61, 0xE2, 0x82], false, "aâ‚"),
            (&[0xFF, 0xFE, 0x68, 0x00, 0x69], false, "h"),
            (&[0xFF, 0xFE, 0x68, 0x00, 0x3D, 0xD8], false, "h"),
            (&[0xEF, 0xBB, 0xBF, 0xEF, 0xBB, 0xBF, 0x41], false, "A"),
            (&[0xEF, 0xBB, 0xBF, 0xEF, 0xBB, 0xBF, 0x41, 0xE9, 0xE9], false, "ï»¿Aéé"),
            (&[0xFF, 0xFE, 0xFF, 0xFE, 0x41, 0x00], false, "\u{FEFF}A"),
            (&[0x41, 0x00, 0x42], false, "A\u{0}B"),
            (&[0x81, 0x8D], false, "\u{81}\u{8D}"),
            (&[], false, ""),
            // Big-endian sniffing; lone surrogates → U+FFFD.
            (&[0x00, 0x68, 0x00, 0x69, 0x00, 0x21, 0x00, 0x21], false, "hi!!"),
            (&[0xFF, 0xFE, 0x41, 0x00, 0x00, 0xDC, 0x42, 0x00], false, "A\u{FFFD}B"),
        ];
        for (bytes, truncated, expected) in cases {
            assert_eq!(decode_text_buffer(bytes, *truncated), *expected, "{bytes:02X?}");
        }
        // Lossy UTF-8 is kept when only a few high bytes are invalid.
        let mut bytes = "héllo wörld ünïcödé".as_bytes().to_vec();
        bytes.push(0xFF);
        assert_eq!(decode_text_buffer(&bytes, false), "héllo wörld ünïcödé\u{FFFD}");
        // A truncated buffer drops a cut 4-byte sequence but keeps complete ones.
        let mut bytes = "ok😀".as_bytes().to_vec();
        assert_eq!(decode_text_buffer(&bytes, true), "ok😀");
        bytes.pop();
        assert_eq!(decode_text_buffer(&bytes, true), "ok");
    }

    #[test]
    fn binary_check() {
        assert!(!looks_binary(""));
        assert!(!looks_binary("plain text\twith\r\nbreaks\x0C"));
        assert!(looks_binary("\u{0}\u{1}\u{2}abc"));
        // Exactly 10 % is not binary; more is.
        assert!(!looks_binary(&format!("\u{1}{}", "a".repeat(9))));
        assert!(looks_binary(&format!("\u{FFFD}\u{FFFD}{}", "a".repeat(9))));
        // Only the first 8192 units are sampled.
        assert!(!looks_binary(&format!("{}{}", "a".repeat(8192), "\u{0}".repeat(5000))));
    }

    #[test]
    fn truncation_vectors() {
        let text = format!("{}end", "word ".repeat(50));
        assert_eq!(truncate_extracted(&text, 100), vec!["word"; 20].join(" "));
        assert_eq!(truncate_extracted("abcdef", 2), "ab");
        assert_eq!(truncate_extracted(&"x".repeat(50), 30), "x".repeat(30));
        assert_eq!(truncate_extracted("ab😀cd", 3), "ab");
        assert_eq!(truncate_extracted("any text", 0), "any text");
        assert_eq!(truncate_extracted("aaaa\tbbbb", 6), "aaaa\tb");
        let text = format!("aaaaa {}", "b".repeat(14));
        assert_eq!(truncate_extracted(&text, 19), format!("aaaaa {}", "b".repeat(13)));
        assert_eq!(truncate_extracted("short", 99), "short");
    }

    #[test]
    fn plain_and_rtf_detection() {
        assert_eq!(plain_text("  a\r\nb\rc\0d  \n"), "a\nb\ncd");
        assert!(starts_like_rtf("  \n{\\rtf1 x}"));
        assert!(!starts_like_rtf("x{\\rtf1}"));
        assert!(is_thematic_break(" - - - "));
        assert!(is_thematic_break("___"));
        assert!(!is_thematic_break("--"));
        assert!(!is_thematic_break("-*-"));
        assert!(!is_thematic_break("--- x"));
    }

    #[test]
    fn read_text_file_pipeline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).expect("write fixture");
            p
        };
        let p = write("a.txt", b"  Hello\r\n\r\n  world  \0 ");
        assert_eq!(read_text_file(&p, 21_000).expect("txt"), "Hello\n\n  world");
        let p = write("page.HTML", b"<p>Hi <b>there</b></p><p>Next</p>");
        assert_eq!(read_text_file(&p, 21_000).expect("html"), "Hi there\n\nNext");
        let p = write("doc.rtf", br"{\rtf1 Caf\'e9\par ok}");
        assert_eq!(read_text_file(&p, 21_000).expect("rtf"), "Café\nok");
        let p = write("fake.rtf", b"not rtf  \r\n");
        assert_eq!(read_text_file(&p, 21_000).expect("plain rtf"), "not rtf");
        let p = write("notes.md", b"# T\n\n- **a**");
        assert_eq!(read_text_file(&p, 21_000).expect("md"), "T\n\na");
        let p = write("data.json", b"{\"a\":  1}");
        assert_eq!(read_text_file(&p, 21_000).expect("json"), "{\"a\":  1}");
        let p = write(".bashrc", b"x  y");
        assert_eq!(read_text_file(&p, 21_000).expect("dotfile"), "x  y");
        let p = write("long.txt", format!("{}end", "word ".repeat(50)).as_bytes());
        assert_eq!(read_text_file(&p, 100).expect("long"), vec!["word"; 20].join(" "));
        let p = write("utf16.txt", &[0xFF, 0xFE, 0x68, 0x00, 0x69, 0x00]);
        assert_eq!(read_text_file(&p, 100).expect("utf16"), "hi");

        let p = write("bin.dat", &[0, 1, 2, 3, 4, 5, 6, 7, 8, 65]);
        assert!(matches!(read_text_file(&p, 100), Err(ExtractError::Binary)));
        assert!(matches!(read_text_file(dir.path(), 100), Err(ExtractError::NotAFile)));
        let missing = dir.path().join("missing.txt");
        assert!(matches!(read_text_file(&missing, 100), Err(ExtractError::Io(_))));
        assert_eq!(ExtractError::Binary.to_string(), "This file does not look like text");
        assert_eq!(ExtractError::NotAFile.to_string(), "Not a regular file");
    }

    #[test]
    fn large_files_are_capped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("big.txt");
        // 5 MiB of "é" (2 bytes each) plus one more: the cap cuts the last "é" in half, which is dropped.
        let mut bytes = "é".repeat((MAX_FILE_BYTES as usize) / 2).into_bytes();
        bytes.insert(0, b'a');
        std::fs::write(&p, &bytes).expect("write big file");
        let text = read_text_file(&p, 10_000_000).expect("big");
        assert!(text.starts_with("aé"));
        assert_eq!(text.chars().count(), 1 + (MAX_FILE_BYTES as usize - 1) / 2);
        assert!(!text.contains('\u{FFFD}'));
    }
}
