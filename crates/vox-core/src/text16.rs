//! UTF-16 offset helpers: the one place where Rust's UTF-8 strings meet the specs' UTF-16 offsets.
//!
//! Policy (ARCHITECTURE.md §5.6): every text offset or length that is persisted (cache manifests, `charCount`,
//! previews), crosses a crate boundary (`TextChunk`, `TtsChunk.textStart/End`, `PlayerState.charIndex/totalChars`,
//! sentence starts) or is compared with a spec limit (`maxChars`, 160-unit preview, 64-unit title, 220/900 chunk
//! limits, 300-unit context) is measured in **UTF-16 code units**, exactly like the Electron app. Code that needs a
//! `&str` slice for such a range converts here. Cuts never split a surrogate pair: an offset that lands inside a
//! pair is moved down to the start of that character ("floor").

/// Number of UTF-16 code units in `s` (JS `s.length`).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Byte index in `s` of UTF-16 offset `off16`, clamped to the end; an offset inside a surrogate pair maps to the
/// start of that character.
pub fn byte_index(s: &str, off16: usize) -> usize {
    if off16 == 0 {
        return 0;
    }
    let mut units = 0usize;
    for (b, c) in s.char_indices() {
        let next = units + c.len_utf16();
        if next > off16 {
            return b;
        }
        units = next;
        if units == off16 {
            return b + c.len_utf8();
        }
    }
    s.len()
}

/// UTF-16 offset of byte index `b` (which must be a char boundary; clamped to the end).
pub fn utf16_offset(s: &str, b: usize) -> usize {
    let b = b.min(s.len());
    utf16_len(&s[..floor_char_boundary(s, b)])
}

fn floor_char_boundary(s: &str, mut b: usize) -> usize {
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// `s.slice(start, end)` in UTF-16 units, both ends floored to character starts. `start > end` gives `""`.
pub fn slice16(s: &str, start: usize, end: usize) -> &str {
    let a = byte_index(s, start);
    let b = byte_index(s, end);
    if a >= b {
        ""
    } else {
        &s[a..b]
    }
}

/// The first `max` UTF-16 units of `s`, never splitting a surrogate pair (JS `slice(0, max)` minus a dangling
/// high surrogate).
pub fn prefix16(s: &str, max: usize) -> &str {
    &s[..byte_index(s, max)]
}

/// True if UTF-16 offset `off16` falls between the two halves of a surrogate pair in `s`.
pub fn splits_pair(s: &str, off16: usize) -> bool {
    let mut units = 0usize;
    for c in s.chars() {
        if units == off16 {
            return false;
        }
        let next = units + c.len_utf16();
        if next > off16 {
            return true;
        }
        units = next;
    }
    false
}

/// Precomputed UTF-16 ↔ byte mapping for one long text (a session text is converted many times per second when the
/// widget highlights words). ASCII-only texts use the identity mapping.
#[derive(Debug, Clone)]
pub struct Utf16Index {
    len16: usize,
    /// `(utf16_start, byte_start)` of every non-ASCII-run boundary; empty for pure ASCII.
    marks: Vec<(u32, u32)>,
    byte_len: usize,
}

impl Utf16Index {
    /// Builds the index for `s` in one pass.
    pub fn new(s: &str) -> Self {
        let byte_len = s.len();
        if s.is_ascii() {
            return Self { len16: byte_len, marks: Vec::new(), byte_len };
        }
        let mut marks = Vec::with_capacity(s.len() / 2);
        let mut u = 0u32;
        for (b, c) in s.char_indices() {
            marks.push((u, b as u32));
            u += c.len_utf16() as u32;
        }
        Self { len16: u as usize, marks, byte_len }
    }

    /// UTF-16 length of the indexed text.
    pub fn len16(&self) -> usize {
        self.len16
    }

    /// Byte index of UTF-16 offset `off16` (floored to a char start, clamped to the end).
    pub fn byte_of(&self, off16: usize) -> usize {
        if self.marks.is_empty() {
            return off16.min(self.byte_len);
        }
        if off16 >= self.len16 {
            return self.byte_len;
        }
        let off = off16 as u32;
        match self.marks.binary_search_by(|(u, _)| u.cmp(&off)) {
            Ok(i) => self.marks[i].1 as usize,
            Err(i) => self.marks[i.saturating_sub(1)].1 as usize,
        }
    }

    /// UTF-16 offset of byte index `b` (floored to a char start).
    pub fn utf16_of(&self, b: usize) -> usize {
        if self.marks.is_empty() {
            return b.min(self.byte_len);
        }
        if b >= self.byte_len {
            return self.len16;
        }
        let bb = b as u32;
        match self.marks.binary_search_by(|(_, x)| x.cmp(&bb)) {
            Ok(i) => self.marks[i].0 as usize,
            Err(i) => self.marks[i.saturating_sub(1)].0 as usize,
        }
    }

    /// `text[start16..end16]` using this index (`text` must be the string the index was built from).
    pub fn slice<'a>(&self, text: &'a str, start16: usize, end16: usize) -> &'a str {
        let a = self.byte_of(start16);
        let b = self.byte_of(end16);
        if a >= b {
            ""
        } else {
            &text[a..b]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_and_cuts() {
        let s = "ab😀cd";
        assert_eq!(utf16_len(s), 6);
        assert_eq!(prefix16(s, 3), "ab"); // would split the pair: floored
        assert_eq!(prefix16(s, 4), "ab😀");
        assert_eq!(slice16(s, 2, 6), "😀cd");
        assert_eq!(slice16(s, 3, 6), "😀cd");
        assert!(splits_pair(s, 3));
        assert!(!splits_pair(s, 4));
        assert_eq!(utf16_offset(s, 6), 4);
    }

    #[test]
    fn index_matches_linear_helpers() {
        for s in ["hello world", "é😀 x 中文 ok", ""] {
            let idx = Utf16Index::new(s);
            assert_eq!(idx.len16(), utf16_len(s));
            for off in 0..=utf16_len(s) + 1 {
                assert_eq!(idx.byte_of(off), byte_index(s, off), "{s:?} @ {off}");
                assert_eq!(idx.slice(s, 0, off), prefix16(s, off));
            }
        }
    }
}
