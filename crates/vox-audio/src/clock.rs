//! The listening clock (reader-playback.md §4.9): wall time counted only while `playing` and audible, capped at
//! media progress / rate (device spin-up, stalls and system sleep add nothing).
//!
//! Pure: the player passes "now" (monotonic milliseconds) and the current chunk's media time in source seconds,
//! taken from the device-reported played block (ARCHITECTURE.md §5.4), never from a display extrapolation. The
//! mark belongs to one chunk: the player banks (`sync(false, …)`) before it moves to another chunk.

/// Monotonic milliseconds supplier (injected for tests).
pub type NowMs = fn() -> f64;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListeningClock {
    acc: f64,
    mark: Option<(f64 /* wall ms */, f64 /* media sec */)>,
}

/// `creditSince(m)`: wall seconds since the mark, never more than the media advanced at `rate`.
fn credit_since(mark: Option<(f64, f64)>, now_ms: f64, media_sec: f64, rate: f64) -> f64 {
    let Some((wall0, media0)) = mark else { return 0.0 };
    let wall = (now_ms - wall0) / 1000.0;
    let media = media_sec - media0;
    // Like the original's `!(x > 0)`: NaN, zero and negative intervals credit nothing.
    if wall.is_nan() || media.is_nan() || wall <= 0.0 || media <= 0.0 {
        return 0.0;
    }
    let rate = if rate > 0.0 && rate.is_finite() { rate } else { 1.0 };
    wall.min(media / rate)
}

impl ListeningClock {
    /// `syncClock`: start or stop counting (`running = playing && audible`), with the current media time.
    pub fn sync(&mut self, running: bool, now_ms: f64, media_sec: f64, rate: f64) {
        if running && self.mark.is_none() {
            self.mark = Some((now_ms, finite_or_zero(media_sec)));
        } else if !running && self.mark.is_some() {
            self.acc += credit_since(self.mark, now_ms, media_sec, rate);
            self.mark = None;
        }
    }

    /// `creditListening`: bank and restart the baseline (before rate changes and seeks).
    pub fn credit(&mut self, now_ms: f64, media_sec: f64, rate: f64) {
        if self.mark.is_some() {
            self.acc += credit_since(self.mark, now_ms, media_sec, rate);
            self.mark = Some((now_ms, finite_or_zero(media_sec)));
        }
    }

    /// After a seek: restart the baseline at the new media time without counting the jump.
    pub fn rebase(&mut self, now_ms: f64, media_sec: f64) {
        if self.mark.is_some() {
            self.mark = Some((now_ms, finite_or_zero(media_sec)));
        }
    }

    /// `listenedSec()`.
    pub fn listened_sec(&self, now_ms: f64, media_sec: f64, rate: f64) -> f64 {
        self.acc + credit_since(self.mark, now_ms, media_sec, rate)
    }

    /// True while a mark exists (listening is being counted).
    pub fn is_counting(&self) -> bool {
        self.mark.is_some()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

fn finite_or_zero(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn counts_only_while_running() {
        let mut c = ListeningClock::default();
        c.sync(false, 0.0, 0.0, 1.0);
        assert_eq!(c.listened_sec(1000.0, 1.0, 1.0), 0.0);
        c.sync(true, 1000.0, 0.0, 1.0);
        assert!(c.is_counting());
        assert!(close(c.listened_sec(3000.0, 2.0, 1.0), 2.0));
        c.sync(false, 3000.0, 2.0, 1.0);
        assert!(!c.is_counting());
        // Stopped: time passing adds nothing.
        assert!(close(c.listened_sec(10_000.0, 2.0, 1.0), 2.0));
    }

    #[test]
    fn capped_by_media_progress() {
        // Device spin-up / a stall / system sleep: 10 wall seconds but only 1 s of media → 1 s.
        let mut c = ListeningClock::default();
        c.sync(true, 0.0, 5.0, 1.0);
        assert!(close(c.listened_sec(10_000.0, 6.0, 1.0), 1.0));
        // No media progress at all → nothing.
        assert_eq!(c.listened_sec(10_000.0, 5.0, 1.0), 0.0);
        // Media went backwards (should not happen) → nothing, not negative.
        assert_eq!(c.listened_sec(10_000.0, 4.0, 1.0), 0.0);
    }

    #[test]
    fn rate_divides_media_progress() {
        // At 2×, 4 s of media are 2 wall seconds; wall 3 s → min(3, 2) = 2.
        let mut c = ListeningClock::default();
        c.sync(true, 0.0, 0.0, 2.0);
        assert!(close(c.listened_sec(3000.0, 4.0, 2.0), 2.0));
        // At 0.5×, 1 s of media takes 2 wall seconds; wall 2 s, media 1 s → min(2, 2) = 2.
        let mut c = ListeningClock::default();
        c.sync(true, 0.0, 0.0, 0.5);
        assert!(close(c.listened_sec(2000.0, 1.0, 0.5), 2.0));
    }

    #[test]
    fn rate_banking() {
        // 2 s at 1× (2 s media), then switch to 2×: the first interval is banked at the old rate.
        let mut c = ListeningClock::default();
        c.sync(true, 0.0, 0.0, 1.0);
        c.credit(2000.0, 2.0, 1.0);
        assert!(close(c.listened_sec(2000.0, 2.0, 2.0), 2.0));
        // Then 1 wall second at 2× (2 s media) → +1.
        assert!(close(c.listened_sec(3000.0, 4.0, 2.0), 3.0));
        // Without banking, dividing the whole interval by the new rate would give min(3, 4/2) = 2.
        let mut unbanked = ListeningClock::default();
        unbanked.sync(true, 0.0, 0.0, 1.0);
        assert!(close(unbanked.listened_sec(3000.0, 4.0, 2.0), 2.0));
    }

    #[test]
    fn rebase_on_seek_does_not_count_the_jump() {
        let mut c = ListeningClock::default();
        c.sync(true, 0.0, 0.0, 1.0);
        // Seek forward by 30 s at t = 1 s: bank, jump, rebase.
        c.credit(1000.0, 1.0, 1.0);
        c.rebase(1000.0, 31.0);
        assert!(close(c.listened_sec(2000.0, 32.0, 1.0), 2.0));
        // Seek backwards: the media decrease is not negative listening.
        c.credit(2000.0, 32.0, 1.0);
        c.rebase(2000.0, 0.0);
        assert!(close(c.listened_sec(2500.0, 0.5, 1.0), 2.5));
        // Rebase without a mark does nothing (not counting).
        let mut idle = ListeningClock::default();
        idle.rebase(0.0, 5.0);
        assert!(!idle.is_counting());
        idle.credit(100.0, 5.0, 1.0);
        assert!(!idle.is_counting());
    }

    #[test]
    fn reset_clears_everything() {
        let mut c = ListeningClock::default();
        c.sync(true, 0.0, 0.0, 1.0);
        c.sync(false, 5000.0, 5.0, 1.0);
        c.reset();
        assert_eq!(c, ListeningClock::default());
        assert_eq!(c.listened_sec(9000.0, 9.0, 1.0), 0.0);
    }

    #[test]
    fn non_finite_inputs_are_harmless() {
        let mut c = ListeningClock::default();
        c.sync(true, 0.0, f64::NAN, 1.0);
        assert_eq!(c.listened_sec(1000.0, f64::NAN, 1.0), 0.0);
        assert!(close(c.listened_sec(1000.0, 1.0, f64::NAN), 1.0));
    }
}
