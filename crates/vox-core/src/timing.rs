//! Alignment → character timing (reader-playback.md §5, tts.md §7). Pure maths shared by the player (vox-audio)
//! and the UI glue. Offsets are UTF-16 units; times are seconds.

/// Index of the last element `<= x` in an ascending slice, or `None`.
pub fn last_index_at_or_before<T: PartialOrd + Copy>(arr: &[T], x: T) -> Option<usize> {
    let n = arr.partition_point(|v| *v <= x);
    n.checked_sub(1)
}

/// Timing view of one chunk.
#[derive(Debug, Clone, Copy)]
pub struct ChunkTiming<'a> {
    pub text_start: u32,
    pub text_end: u32,
    /// Chunk duration (seconds) used by the linear fallback.
    pub duration_sec: f64,
    /// Alignment starts, if any.
    pub starts: Option<&'a [f64]>,
}

impl<'a> ChunkTiming<'a> {
    fn len(&self) -> i64 {
        self.text_end as i64 - self.text_start as i64
    }
    /// `usableStarts`: non-empty and (`n == len` or (`n > 1` and `len > 1`)).
    fn usable_starts(&self) -> Option<&'a [f64]> {
        let len = self.len();
        let s = self.starts?;
        let n = s.len() as i64;
        (n > 0 && (n == len || (n > 1 && len > 1))).then_some(s)
    }

    /// Absolute character spoken at `t` seconds into the chunk.
    pub fn char_at_time(&self, t: f64) -> u32 {
        let len = self.len();
        if len <= 0 {
            return self.text_start;
        }
        let local: i64 = match self.usable_starts() {
            Some(starts) => {
                let n = starts.len() as i64;
                let i = last_index_at_or_before(starts, t).unwrap_or(0) as i64;
                if n == len {
                    i
                } else {
                    (i * (len - 1)) / (n - 1)
                }
            }
            None => {
                if self.duration_sec > 0.0 {
                    ((t / self.duration_sec) * len as f64 + 1e-9).floor() as i64
                } else {
                    0
                }
            }
        };
        self.text_start + local.clamp(0, len - 1) as u32
    }

    /// Inverse: the start time of character `ch` (guarantees `char_at_time(time_at_char(c)) >= c`).
    pub fn time_at_char(&self, ch: u32) -> f64 {
        let len = self.len();
        if len <= 0 {
            return 0.0;
        }
        let local = (ch as i64 - self.text_start as i64).clamp(0, len - 1);
        match self.usable_starts() {
            Some(starts) => {
                let n = starts.len() as i64;
                let i = if n == len {
                    local
                } else {
                    let num = local * (n - 1);
                    let den = len - 1;
                    ((num + den - 1) / den).min(n - 1)
                };
                starts[i as usize]
            }
            None => self.duration_sec * (local as f64 / len as f64),
        }
    }
}

/// The sentence containing `char_index`: `(index, start, end)` with `end = next start or text_len`.
pub fn sentence_at(sentences: &[u32], text_len: u32, char_index: u32) -> (usize, u32, u32) {
    let index = last_index_at_or_before(sentences, char_index).unwrap_or(0);
    let start = sentences.get(index).copied().unwrap_or(0);
    let end = sentences.get(index + 1).copied().unwrap_or(text_len);
    (index, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_index() {
        assert_eq!(last_index_at_or_before(&[1.0, 2.0, 3.0], 0.5), None);
        assert_eq!(last_index_at_or_before(&[1.0, 2.0, 3.0], 2.0), Some(1));
        assert_eq!(last_index_at_or_before(&[1u32, 2, 3], 9), Some(2));
    }

    #[test]
    fn exact_alignment() {
        let starts = [0.0, 0.2, 0.4];
        let c = ChunkTiming { text_start: 10, text_end: 13, duration_sec: 0.6, starts: Some(&starts) };
        assert_eq!(c.char_at_time(0.0), 10);
        assert_eq!(c.char_at_time(0.25), 11);
        assert_eq!(c.char_at_time(9.0), 12);
        assert_eq!(c.time_at_char(11), 0.2);
        for ch in 10..13 {
            assert!(c.char_at_time(c.time_at_char(ch)) >= ch);
        }
    }

    #[test]
    fn proportional_and_linear() {
        // 5 alignment entries over 9 chars
        let starts = [0.0, 1.0, 2.0, 3.0, 4.0];
        let c = ChunkTiming { text_start: 0, text_end: 9, duration_sec: 5.0, starts: Some(&starts) };
        assert_eq!(c.char_at_time(4.0), 8);
        assert_eq!(c.char_at_time(1.0), 2);
        for ch in 0..9 {
            assert!(c.char_at_time(c.time_at_char(ch)) >= ch, "ch {ch}");
        }
        let lin = ChunkTiming { text_start: 0, text_end: 10, duration_sec: 10.0, starts: None };
        assert_eq!(lin.char_at_time(3.0), 3);
        assert_eq!(lin.time_at_char(3), 3.0);
        for ch in 0..10 {
            assert_eq!(lin.char_at_time(lin.time_at_char(ch)), ch);
        }
    }

    #[test]
    fn sentences() {
        assert_eq!(sentence_at(&[0, 10, 20], 30, 15), (1, 10, 20));
        assert_eq!(sentence_at(&[0, 10, 20], 30, 25), (2, 20, 30));
        assert_eq!(sentence_at(&[0], 5, 0), (0, 0, 5));
    }
}
