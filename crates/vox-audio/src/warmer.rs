//! The output warmer (reader-playback.md §6): keeps codecs / Bluetooth awake with an inaudible ±1 LSB square wave
//! (flipping every 10 ms) and gates speech until the device has rendered 1 s of it since it last started (measured
//! on the audio clock = rendered frames), at most 3 s per warm-up; release 30 s after end/stop/fail, 60 s after pause;
//! after a failed open, no retry for 60 s. Pure state machine; the player drives it with "now" and the output's
//! rendered frames.
//!
//! Mapping to the original (`output-warmer.ts`): the player owns the stream (the "context"): it opens it lazily when
//! [`OutputWarmer::may_open`] allows, resumes it before [`OutputWarmer::prepare`], and suspends it when
//! [`OutputWarmer::should_suspend`] fires. `running` = the stream exists and is not suspended. "Rendered" is measured
//! from the stream's real start: the frames the device consumed since the stream was last resumed, which the output
//! counts itself (reader-playback.md §6: "Measuring from the stream's real start (frames consumed since `Start()`)
//! is equivalent or better" than the original's lazy `startedAt`). So a stream that was reopened (device change or
//! loss) is warm one second after it started, whether or not anything asked the warmer in between.

use vox_core::constants::{WARM_MAX_WAIT_MS, WARM_MS, WARM_RETRY_MS};

/// Warmer decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmState {
    /// Start speech now.
    Ready,
    /// Wait (re-check every 25 ms).
    Waiting,
}

#[derive(Debug, Clone, Default)]
pub struct OutputWarmer {
    deadline_ms: Option<f64>,
    gave_up: bool,
    release_at_ms: Option<f64>,
    failed_at_ms: Option<f64>,
}

impl OutputWarmer {
    /// Seconds rendered since the output last started (`rendered()`; `rendered_frames` counts from the last resume).
    fn rendered_sec(rendered_frames: u64, sample_rate: u32, running: bool) -> f64 {
        if !running || sample_rate == 0 {
            return 0.0;
        }
        rendered_frames as f64 / sample_rate as f64
    }

    fn is_warm(&self, rendered_frames: u64, sample_rate: u32, running: bool) -> bool {
        Self::rendered_sec(rendered_frames, sample_rate, running) * 1000.0 >= WARM_MS as f64
    }

    /// `prepare()`: cancels a pending release; returns `Ready` when warm, given up or impossible. The player calls it
    /// after making sure the stream exists and is resumed; when the stream could not be created it does not call it
    /// (warming impossible → start at once).
    pub fn prepare(&mut self, now_ms: f64, rendered_frames: u64, sample_rate: u32, running: bool) -> WarmState {
        self.release_at_ms = None;
        if self.gave_up || self.is_warm(rendered_frames, sample_rate, running) {
            return WarmState::Ready;
        }
        if self.deadline_ms.is_none() {
            self.deadline_ms = Some(now_ms + WARM_MAX_WAIT_MS as f64);
        }
        // The shared wait polls immediately when it is created.
        self.poll(now_ms, rendered_frames, sample_rate, running)
    }

    /// Polled while `Waiting`.
    pub fn poll(&mut self, now_ms: f64, rendered_frames: u64, sample_rate: u32, running: bool) -> WarmState {
        let Some(deadline) = self.deadline_ms else { return WarmState::Ready };
        if self.is_warm(rendered_frames, sample_rate, running) {
            self.deadline_ms = None;
            return WarmState::Ready;
        }
        if now_ms >= deadline {
            // This warm-up timed out: nothing waits again until the output was released and restarted.
            self.gave_up = true;
            self.deadline_ms = None;
            return WarmState::Ready;
        }
        WarmState::Waiting
    }

    /// A warm-up wait is pending (the player keeps ticking).
    pub fn is_waiting(&self) -> bool {
        self.deadline_ms.is_some()
    }

    /// `release(afterMs)`: replaces any earlier scheduled release (last call wins); does not affect an in-flight
    /// warm-up wait.
    pub fn release(&mut self, now_ms: f64, after_ms: u64) {
        self.release_at_ms = Some(now_ms + after_ms as f64);
    }

    /// Cancels a scheduled release without waiting for anything (outputs that need no warm-up).
    pub fn cancel_release(&mut self) {
        self.release_at_ms = None;
    }

    /// When the scheduled release fires (for the player's wake-up deadline).
    pub fn release_at(&self) -> Option<f64> {
        self.release_at_ms
    }

    /// True when the scheduled release time has passed (the player suspends the stream). Clears `gaveUp`, so the
    /// next start warms afresh.
    pub fn should_suspend(&mut self, now_ms: f64) -> bool {
        match self.release_at_ms {
            Some(at) if now_ms >= at => {
                self.release_at_ms = None;
                self.gave_up = false;
                true
            }
            _ => false,
        }
    }

    /// The stream could not be created.
    pub fn open_failed(&mut self, now_ms: f64) {
        self.failed_at_ms = Some(now_ms);
    }

    /// The stream was created (clears a previous failure).
    pub fn opened(&mut self) {
        self.failed_at_ms = None;
    }

    /// May the player try to open the stream now (60 s after a failure)?
    pub fn may_open(&self, now_ms: f64) -> bool {
        match self.failed_at_ms {
            Some(at) => now_ms - at >= WARM_RETRY_MS as f64,
            None => true,
        }
    }
}

/// The keep-alive sample at frame `i` for `sample_rate` (`±1/32768`, period 20 ms).
pub fn keep_alive_sample(i: u64, sample_rate: u32) -> f32 {
    let half = ((sample_rate as f64 / 100.0).round() as u64).max(1);
    if (i / half).is_multiple_of(2) {
        1.0 / 32768.0
    } else {
        -1.0 / 32768.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    #[test]
    fn ready_after_one_second_rendered() {
        let mut w = OutputWarmer::default();
        // Stream just resumed: first observation at 0 frames.
        assert_eq!(w.prepare(0.0, 0, SR, true), WarmState::Waiting);
        assert!(w.is_waiting());
        assert_eq!(w.poll(500.0, 24_000, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(1000.0, 47_999, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(1010.0, 48_000, SR, true), WarmState::Ready);
        assert!(!w.is_waiting());
        // Warm now: later prepares start at once.
        assert_eq!(w.prepare(2000.0, 96_000, SR, true), WarmState::Ready);
    }

    #[test]
    fn measured_from_the_stream_start_not_from_the_first_look() {
        let mut w = OutputWarmer::default();
        // A stream that has rendered 10 s since it (re)started — e.g. one reopened after a device change while a read
        // played, which nothing asked the warmer about since — is warm at the first look.
        assert_eq!(w.prepare(0.0, 480_000, SR, true), WarmState::Ready);
        assert!(!w.is_waiting());
        // Half a second after a restart: waits for the other half.
        let mut w = OutputWarmer::default();
        assert_eq!(w.prepare(0.0, 24_000, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(400.0, 43_200, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(500.0, 48_000, SR, true), WarmState::Ready);
    }

    #[test]
    fn gives_up_after_three_seconds_until_released() {
        let mut w = OutputWarmer::default();
        // A stream that exists but never renders (broken device).
        assert_eq!(w.prepare(0.0, 0, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(2999.0, 0, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(3000.0, 0, SR, true), WarmState::Ready);
        // Given up: every later prepare starts at once …
        assert_eq!(w.prepare(4000.0, 0, SR, true), WarmState::Ready);
        assert!(!w.is_waiting());
        // … until a release fires, which clears gaveUp.
        w.release(5000.0, 30_000);
        assert!(!w.should_suspend(34_999.0));
        assert!(w.should_suspend(35_000.0));
        assert!(!w.should_suspend(36_000.0), "a release fires once");
        assert_eq!(w.prepare(40_000.0, 0, SR, false), WarmState::Waiting);
    }

    #[test]
    fn not_running_is_not_warm() {
        let mut w = OutputWarmer::default();
        assert_eq!(w.prepare(0.0, 1_000_000, SR, false), WarmState::Waiting);
        // A suspended stream is never warm, whatever its counter says.
        assert_eq!(w.poll(100.0, 2_000_000, SR, false), WarmState::Waiting);
        // Resumed: its counter restarted at 0.
        assert_eq!(w.poll(200.0, 0, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(1200.0, 48_000, SR, true), WarmState::Ready);
    }

    #[test]
    fn prepare_cancels_release_and_release_replaces() {
        let mut w = OutputWarmer::default();
        w.release(0.0, 60_000);
        assert_eq!(w.release_at(), Some(60_000.0));
        // Last call wins.
        w.release(1000.0, 30_000);
        assert_eq!(w.release_at(), Some(31_000.0));
        // A new start (prepare) cancels it.
        let _ = w.prepare(2000.0, 0, SR, true);
        assert_eq!(w.release_at(), None);
        assert!(!w.should_suspend(100_000.0));
    }

    #[test]
    fn release_does_not_affect_a_pending_wait() {
        let mut w = OutputWarmer::default();
        assert_eq!(w.prepare(0.0, 0, SR, true), WarmState::Waiting);
        w.release(10.0, 60_000);
        assert!(w.is_waiting());
        assert_eq!(w.poll(1000.0, 48_000, SR, true), WarmState::Ready);
    }

    #[test]
    fn retry_after_open_failure() {
        let mut w = OutputWarmer::default();
        assert!(w.may_open(0.0));
        w.open_failed(1000.0);
        assert!(!w.may_open(1000.0));
        assert!(!w.may_open(60_999.0));
        assert!(w.may_open(61_000.0));
        w.opened();
        assert!(w.may_open(61_001.0));
    }

    #[test]
    fn suspend_restart_warms_afresh() {
        let mut w = OutputWarmer::default();
        assert_eq!(w.prepare(0.0, 0, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(1000.0, 48_000, SR, true), WarmState::Ready);
        w.release(1000.0, 30_000);
        assert!(w.should_suspend(31_000.0));
        // The stream restarted: its counter restarted too.
        assert_eq!(w.prepare(40_000.0, 0, SR, true), WarmState::Waiting);
        assert_eq!(w.poll(41_000.0, 48_000, SR, true), WarmState::Ready);
    }

    #[test]
    fn keep_alive_square_wave() {
        // 48 kHz: half period 480 frames.
        assert_eq!(keep_alive_sample(0, 48_000), 1.0 / 32768.0);
        assert_eq!(keep_alive_sample(479, 48_000), 1.0 / 32768.0);
        assert_eq!(keep_alive_sample(480, 48_000), -1.0 / 32768.0);
        assert_eq!(keep_alive_sample(960, 48_000), 1.0 / 32768.0);
        // 44.1 kHz: 441 frames.
        assert_eq!(keep_alive_sample(441, 44_100), -1.0 / 32768.0);
        // Degenerate rates never divide by zero.
        assert_eq!(keep_alive_sample(5, 0), -1.0 / 32768.0);
        // Not digital silence, far below hearing (≈ −90 dBFS).
        let db = 20.0 * (keep_alive_sample(0, 48_000) as f64).log10();
        assert!((-91.0..-89.0).contains(&db), "{db}");
    }
}
