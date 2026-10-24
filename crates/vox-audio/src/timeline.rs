//! Pure timeline maths (reader-playback.md §4.8). A session's chunks are contiguous; `dur(c)` is the decoded or
//! engine duration, else an estimate from the chunk's character range. Uses `vox_core::timing` for in-chunk lookups.
//!
//! All offsets are UTF-16 units of the session text; all times are audio seconds (independent of the playback
//! rate). Nothing here panics on any input (out-of-range indexes and non-finite times are clamped).

use vox_core::constants::{END_GUARD_SEC, ESTIMATED_CHARS_PER_SECOND, MIN_CHUNK_DURATION_SEC};
use vox_core::timing::ChunkTiming;

/// What the timeline needs per chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineChunk {
    pub text_start: u32,
    pub text_end: u32,
    /// 0 = unknown.
    pub duration_sec: f64,
    pub alignment_starts: Option<Vec<f64>>,
    pub failed: bool,
}

/// Session-level inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    pub text_len: u32,
    pub estimated_duration_sec: f64,
    pub chunks: Vec<TimelineChunk>,
    pub done: bool,
}

/// `clampNum`: NaN → `min`.
fn clamp_num(n: f64, min: f64, max: f64) -> f64 {
    if n.is_nan() {
        min
    } else {
        n.max(min).min(max)
    }
}

impl Timeline {
    /// An empty timeline for a session text of `text_len` units with the engine's estimate.
    pub fn new(text_len: u32, estimated_duration_sec: f64) -> Self {
        Self { text_len, estimated_duration_sec, chunks: Vec::new(), done: false }
    }

    /// Seconds per character used for chunks without a known duration.
    fn per_char(&self) -> f64 {
        if self.text_len > 0 {
            self.estimated_duration_sec / self.text_len as f64
        } else {
            1.0 / ESTIMATED_CHARS_PER_SECOND
        }
    }

    /// `dur(c)`.
    pub fn dur(&self, i: usize) -> f64 {
        let Some(c) = self.chunks.get(i) else { return 0.0 };
        if c.duration_sec > 0.0 && c.duration_sec.is_finite() {
            c.duration_sec
        } else {
            let len = c.text_end.saturating_sub(c.text_start) as f64;
            let est = len * self.per_char();
            if est.is_finite() {
                est.max(MIN_CHUNK_DURATION_SEC)
            } else {
                MIN_CHUNK_DURATION_SEC
            }
        }
    }

    /// Σ `dur` over delivered chunks.
    pub fn available_sec(&self) -> f64 {
        (0..self.chunks.len()).map(|i| self.dur(i)).sum()
    }

    /// Start time of chunk `i` (Σ `dur` of the chunks before it; `i` past the end → available).
    pub fn chunk_start(&self, i: usize) -> f64 {
        (0..i.min(self.chunks.len())).map(|k| self.dur(k)).sum()
    }

    /// Position for current chunk `cur` at in-chunk time `t` (`ended` → available).
    pub fn position_sec(&self, cur: usize, t: f64, ended: bool) -> f64 {
        if ended {
            return self.available_sec();
        }
        let n = self.chunks.len();
        let mut pos = self.chunk_start(cur.min(n));
        if cur < n {
            pos += clamp_num(t, 0.0, self.dur(cur));
        }
        pos
    }

    /// Duration: exact when done, else available + remaining chars × observed seconds per char.
    pub fn duration_sec(&self) -> f64 {
        let avail = self.available_sec();
        if self.done {
            return avail;
        }
        let delivered = self.chunks.last().map(|c| c.text_end).unwrap_or(0);
        let remaining = self.text_len.saturating_sub(delivered) as f64;
        let sec_per_char = if delivered > 0 && avail > 0.0 { avail / delivered as f64 } else { self.per_char() };
        avail + remaining * sec_per_char
    }

    /// The bounded-look-ahead share: `done ? (chunks ? 1 : 0) : clamp(available / duration, 0, 1)`.
    pub fn buffered_fraction(&self) -> f64 {
        if self.done {
            return if self.chunks.is_empty() { 0.0 } else { 1.0 };
        }
        let duration = self.duration_sec();
        if duration > 0.0 {
            clamp_num(self.available_sec() / duration, 0.0, 1.0)
        } else {
            0.0
        }
    }

    fn timing(&self, i: usize) -> Option<ChunkTiming<'_>> {
        let c = self.chunks.get(i)?;
        Some(ChunkTiming {
            text_start: c.text_start,
            text_end: c.text_end,
            duration_sec: self.dur(i),
            starts: c.alignment_starts.as_deref(),
        })
    }

    /// The char index for current chunk `cur` at in-chunk time `t`.
    pub fn char_index(&self, cur: usize, t: f64, ended: bool) -> u32 {
        if ended {
            return self.text_len;
        }
        match self.timing(cur) {
            Some(timing) => timing.char_at_time(if t.is_finite() { t } else { 0.0 }),
            None => {
                if cur > 0 {
                    self.chunks.get(cur - 1).or(self.chunks.last()).map(|c| c.text_end).unwrap_or(0)
                } else {
                    0
                }
            }
        }
    }

    /// Session seconds of char `ch`; `f64::INFINITY` when its audio has not arrived.
    pub fn time_at_char(&self, ch: u32) -> f64 {
        if self.chunks.is_empty() {
            return f64::INFINITY;
        }
        // `lastIndexAtOrBefore` over the chunk starts (ascending).
        let Some(i) = self.chunks.partition_point(|c| c.text_start <= ch).checked_sub(1) else { return 0.0 };
        let last = self.chunks.len() - 1;
        if i == last && ch >= self.chunks[last].text_end {
            return if self.done { self.available_sec() } else { f64::INFINITY };
        }
        let start = self.chunk_start(i);
        if ch >= self.chunks[i].text_end {
            // In a gap between chunks.
            return start + self.dur(i);
        }
        match self.timing(i) {
            Some(timing) => start + timing.time_at_char(ch),
            None => start,
        }
    }

    /// Chunk index and offset for a session time (seek), clamped to available − 0.05 s. The last chunk absorbs any
    /// remainder; the offset is clamped to `dur − 0.05`. An empty timeline yields `(0, 0)`.
    pub fn locate(&self, sec: f64) -> (usize, f64) {
        let n = self.chunks.len();
        if n == 0 {
            return (0, 0.0);
        }
        let target = clamp_num(sec, 0.0, (self.available_sec() - END_GUARD_SEC).max(0.0));
        let mut i = 0;
        let mut start = 0.0;
        while i + 1 < n {
            let d = self.dur(i);
            if target < start + d {
                break;
            }
            start += d;
            i += 1;
        }
        let offset = clamp_num(target - start, 0.0, (self.dur(i) - END_GUARD_SEC).max(0.0));
        (i, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(start: u32, end: u32, dur: f64) -> TimelineChunk {
        TimelineChunk { text_start: start, text_end: end, duration_sec: dur, alignment_starts: None, failed: false }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn dur_uses_known_duration_else_estimate_with_floor() {
        let mut t = Timeline::new(300, 20.0); // 1/15 s per char
        t.chunks.push(chunk(0, 150, 9.5));
        t.chunks.push(chunk(150, 300, 0.0));
        t.chunks.push(chunk(300, 300, 0.0)); // empty range → 0.1 s floor
        assert_eq!(t.dur(0), 9.5);
        assert!(close(t.dur(1), 10.0));
        assert_eq!(t.dur(2), MIN_CHUNK_DURATION_SEC);
        assert_eq!(t.dur(9), 0.0);
        // Non-finite / negative durations count as unknown.
        t.chunks[0].duration_sec = f64::NAN;
        assert!(close(t.dur(0), 10.0));
        t.chunks[0].duration_sec = -1.0;
        assert!(close(t.dur(0), 10.0));
        // Empty text → 1/15 s per char.
        let mut e = Timeline::new(0, 0.0);
        e.chunks.push(chunk(0, 30, 0.0));
        assert!(close(e.dur(0), 2.0));
    }

    #[test]
    fn available_start_and_position() {
        let mut t = Timeline::new(30, 3.0);
        t.chunks.push(chunk(0, 10, 1.0));
        t.chunks.push(chunk(10, 20, 2.0));
        assert!(close(t.available_sec(), 3.0));
        assert!(close(t.chunk_start(0), 0.0));
        assert!(close(t.chunk_start(1), 1.0));
        assert!(close(t.chunk_start(5), 3.0));
        assert!(close(t.position_sec(1, 0.5, false), 1.5));
        // In-chunk time clamped to the chunk.
        assert!(close(t.position_sec(1, 9.0, false), 3.0));
        assert!(close(t.position_sec(0, -4.0, false), 0.0));
        assert!(close(t.position_sec(0, f64::NAN, false), 0.0));
        // Past the last chunk (waiting for synthesis) → available; ended → available.
        assert!(close(t.position_sec(2, 0.7, false), 3.0));
        assert!(close(t.position_sec(0, 0.0, true), 3.0));
    }

    #[test]
    fn duration_estimate_refinement() {
        // 300 chars, engine estimate 20 s (15 chars/s).
        let mut t = Timeline::new(300, 20.0);
        assert!(close(t.duration_sec(), 20.0));
        // First chunk: 100 chars took 10 s → observed 0.1 s/char → 10 + 200 × 0.1 = 30.
        t.chunks.push(chunk(0, 100, 10.0));
        assert!(close(t.duration_sec(), 30.0));
        assert!(close(t.buffered_fraction(), 10.0 / 30.0));
        // Done → exact.
        t.done = true;
        assert!(close(t.duration_sec(), 10.0));
        assert_eq!(t.buffered_fraction(), 1.0);
        // Done without chunks → 0 buffered.
        let mut empty = Timeline::new(10, 1.0);
        empty.done = true;
        assert_eq!(empty.buffered_fraction(), 0.0);
        assert_eq!(empty.duration_sec(), 0.0);
    }

    #[test]
    fn char_index_rules() {
        let mut t = Timeline::new(30, 3.0);
        t.chunks.push(chunk(0, 10, 1.0));
        t.chunks.push(chunk(10, 20, 2.0));
        // Linear fallback inside a chunk.
        assert_eq!(t.char_index(0, 0.55, false), 5);
        assert_eq!(t.char_index(1, 1.0, false), 15);
        // Past the available chunks: the previous chunk's end.
        assert_eq!(t.char_index(2, 0.0, false), 20);
        // No chunks at all.
        assert_eq!(Timeline::new(30, 3.0).char_index(0, 0.0, false), 0);
        // Ended → text length.
        assert_eq!(t.char_index(0, 0.0, true), 30);
        // With alignment.
        t.chunks[0].alignment_starts = Some(vec![0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9]);
        assert_eq!(t.char_index(0, 0.35, false), 3);
    }

    #[test]
    fn time_at_char_gaps_and_infinity() {
        let mut t = Timeline::new(40, 4.0);
        assert_eq!(t.time_at_char(0), f64::INFINITY);
        t.chunks.push(chunk(5, 10, 1.0)); // text before the first chunk
        t.chunks.push(chunk(12, 20, 2.0)); // a gap 10..12
                                           // Before the first chunk start → 0.
        assert_eq!(t.time_at_char(2), 0.0);
        // Linear inside chunk 0: char 7 → 2/5 of 1 s.
        assert!(close(t.time_at_char(7), 0.4));
        // In the gap → end of chunk 0.
        assert!(close(t.time_at_char(11), 1.0));
        // Inside chunk 1.
        assert!(close(t.time_at_char(16), 1.0 + 2.0 * 4.0 / 8.0));
        // Beyond the last delivered chunk: ∞ until done, then the available total.
        assert_eq!(t.time_at_char(25), f64::INFINITY);
        t.done = true;
        assert!(close(t.time_at_char(25), 3.0));
    }

    #[test]
    fn locate_clamps() {
        let mut t = Timeline::new(30, 3.0);
        t.chunks.push(chunk(0, 10, 1.0));
        t.chunks.push(chunk(10, 20, 2.0));
        assert_eq!(t.locate(0.0), (0, 0.0));
        let (i, off) = t.locate(0.5);
        assert_eq!(i, 0);
        assert!(close(off, 0.5));
        // Exactly at a chunk boundary → the next chunk at 0.
        let (i, off) = t.locate(1.0);
        assert_eq!(i, 1);
        assert!(close(off, 0.0));
        // Past the available audio → clamped to available − 0.05, in the last chunk.
        let (i, off) = t.locate(99.0);
        assert_eq!(i, 1);
        assert!(close(off, 1.95));
        // Negative and NaN → 0.
        assert_eq!(t.locate(-3.0), (0, 0.0));
        assert_eq!(t.locate(f64::NAN), (0, 0.0));
        // The offset inside a chunk never lands closer than 0.05 s to its end.
        let mut one = Timeline::new(10, 1.0);
        one.chunks.push(chunk(0, 10, 0.03));
        assert_eq!(one.locate(5.0), (0, 0.0));
        // Empty timeline.
        assert_eq!(Timeline::new(10, 1.0).locate(3.0), (0, 0.0));
    }
}
