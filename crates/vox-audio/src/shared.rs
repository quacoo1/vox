//! The state the player thread publishes for [`crate::Player::snapshot`] (any thread, ~30 Hz) and for its own
//! reports: one small `parking_lot::Mutex` held only for copies (never by the audio callback). `snapshot()` computes
//! position, duration and char index for "now" from it, extrapolating the in-chunk time while playing and audible —
//! a display-only interpolation (ARCHITECTURE.md §5.4); reports are computed with no extrapolation.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use vox_core::player::{PlayerSnapshot, PlayerState, PlayerStatus, SessionInfo, SessionWarning};
use vox_core::read::ReadErrorCode;
use vox_core::timing::ChunkTiming;
use vox_core::tts::Alignment;

/// What the engine published (see the module docs).
#[derive(Debug, Clone)]
pub(crate) struct Published {
    pub status: PlayerStatus,
    pub session: Option<Arc<SessionInfo>>,
    pub rate: f64,
    pub volume: f64,
    /// Synchronous targets written by the handle and not yet applied by the thread (ARCHITECTURE.md §5.2).
    pub rate_pending: u32,
    pub volume_pending: u32,
    pub wants_play: bool,
    pub buffering: bool,
    pub starting: bool,
    pub warning: Option<SessionWarning>,
    pub partial_text: bool,
    pub error: Option<(String, ReadErrorCode)>,
    pub listened_sec: f64,
    /// In-chunk media time of the current chunk at `at_ms` (device-reported).
    pub t: f64,
    pub at_ms: f64,
    pub extrapolate: bool,
    pub ended: bool,
    /// Start of the current chunk on the session timeline (available when past the last chunk).
    pub position_base: f64,
    pub cur_exists: bool,
    pub cur_dur: f64,
    pub cur_text: (u32, u32),
    pub cur_alignment: Option<Arc<Alignment>>,
    /// `chunks[cur - 1].textEnd` (0 for none), used when the current chunk has not arrived.
    pub prev_text_end: u32,
    pub available: f64,
    pub duration: f64,
    pub done: bool,
    pub buffered: f64,
    pub text_len: u32,
}

impl Default for Published {
    fn default() -> Self {
        Self {
            status: PlayerStatus::Idle,
            session: None,
            rate: 1.0,
            volume: 1.0,
            rate_pending: 0,
            volume_pending: 0,
            wants_play: false,
            buffering: false,
            starting: false,
            warning: None,
            partial_text: false,
            error: None,
            listened_sec: 0.0,
            t: 0.0,
            at_ms: 0.0,
            extrapolate: false,
            ended: false,
            position_base: 0.0,
            cur_exists: false,
            cur_dur: 0.0,
            cur_text: (0, 0),
            cur_alignment: None,
            prev_text_end: 0,
            available: 0.0,
            duration: 0.0,
            done: false,
            buffered: 0.0,
            text_len: 0,
        }
    }
}

/// Shared between the handle(s) and the player thread.
pub(crate) struct Shared {
    pub published: Mutex<Published>,
    base: Instant,
    /// Timer-driven wake-ups of the player thread (the idle-cost test reads it).
    pub ticks: AtomicU64,
    /// Every pass of the player thread's loop (tests: no busy loop).
    #[cfg(test)]
    pub wakeups: AtomicU64,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            published: Mutex::new(Published::default()),
            base: Instant::now(),
            ticks: AtomicU64::new(0),
            #[cfg(test)]
            wakeups: AtomicU64::new(0),
        }
    }

    /// Monotonic milliseconds since the player was created.
    pub fn now_ms(&self) -> f64 {
        self.base.elapsed().as_secs_f64() * 1000.0
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let now = self.now_ms();
        let p = self.published.lock();
        snapshot_from(&p, now)
    }
}

/// The snapshot for `now_ms` (reader-playback.md §4.10 `getState` + `PlayerSnapshot`).
pub(crate) fn snapshot_from(p: &Published, now_ms: f64) -> PlayerSnapshot {
    let Some(info) = p.session.as_ref() else {
        let state = PlayerState { status: p.status, rate: p.rate, volume: p.volume, ..PlayerState::idle() };
        return PlayerSnapshot {
            state,
            session: None,
            wants_play: false,
            buffering: p.buffering,
            starting: false,
            warning: None,
            partial_text: false,
        };
    };
    let mut t = p.t;
    if p.extrapolate && p.cur_exists {
        let dt = ((now_ms - p.at_ms) / 1000.0).max(0.0);
        let rate = if p.rate > 0.0 && p.rate.is_finite() { p.rate } else { 1.0 };
        t = (t + dt * rate).min(p.cur_dur.max(p.t));
    }
    let position = if p.ended {
        p.available
    } else if p.cur_exists {
        p.position_base + t.clamp(0.0, p.cur_dur.max(0.0))
    } else {
        p.position_base
    };
    let char_index = if p.ended {
        p.text_len
    } else if p.cur_exists {
        let starts = p.cur_alignment.as_ref().filter(|a| a.usable_for_timing()).map(|a| &a.starts[..]);
        ChunkTiming { text_start: p.cur_text.0, text_end: p.cur_text.1, duration_sec: p.cur_dur, starts }
            .char_at_time(if t.is_finite() { t } else { 0.0 })
    } else {
        p.prev_text_end
    };
    let duration = p.duration;
    let state = PlayerState {
        status: p.status,
        session_id: Some(info.id.clone()),
        read_id: Some(info.read_id.clone()),
        title: info.title.clone(),
        voice_name: info.voice_name.clone(),
        position_sec: position.min(duration).max(0.0),
        duration_sec: duration,
        duration_is_estimate: !p.done,
        buffered_fraction: p.buffered,
        rate: p.rate,
        volume: p.volume,
        char_index,
        total_chars: p.text_len,
        listened_sec: p.listened_sec,
        source_app: info.source_app.clone(),
        error: p.error.as_ref().map(|e| e.0.clone()),
        error_code: p.error.as_ref().map(|e| e.1),
    };
    PlayerSnapshot {
        state,
        session: Some(info.clone()),
        wants_play: p.wants_play,
        buffering: p.buffering,
        starting: p.starting,
        warning: p.warning.clone(),
        partial_text: p.partial_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> Arc<SessionInfo> {
        Arc::new(SessionInfo {
            id: "s1".into(),
            read_id: "r1".into(),
            text: Arc::from("Hello there world"),
            title: "Hello".into(),
            voice_name: "Ava".into(),
            voice_id: "v".into(),
            source_app: Some("Chrome".into()),
            from_cache: false,
            highlight_words: true,
            sentences: Arc::from(vec![0u32]),
        })
    }

    #[test]
    fn sessionless_snapshot_keeps_rate_and_volume() {
        let p = Published { rate: 1.5, volume: 0.4, ..Published::default() };
        let s = snapshot_from(&p, 0.0);
        assert_eq!(s.state, PlayerState { rate: 1.5, volume: 0.4, ..PlayerState::idle() });
        assert!(s.session.is_none() && !s.wants_play && !s.starting);
    }

    #[test]
    fn extrapolates_only_while_audible_and_caps_at_the_chunk() {
        let mut p = Published {
            status: PlayerStatus::Playing,
            session: Some(info()),
            rate: 2.0,
            t: 1.0,
            at_ms: 1000.0,
            extrapolate: true,
            position_base: 10.0,
            cur_exists: true,
            cur_dur: 1.5,
            cur_text: (0, 17),
            available: 11.5,
            duration: 20.0,
            text_len: 17,
            ..Published::default()
        };
        // 100 ms later at 2× → +0.2 s.
        let s = snapshot_from(&p, 1100.0);
        assert!((s.state.position_sec - 11.2).abs() < 1e-9);
        // Capped at the chunk duration.
        let s = snapshot_from(&p, 9000.0);
        assert!((s.state.position_sec - 11.5).abs() < 1e-9);
        // Not audible: no extrapolation.
        p.extrapolate = false;
        let s = snapshot_from(&p, 9000.0);
        assert!((s.state.position_sec - 11.0).abs() < 1e-9);
        // Position never exceeds the (estimated) duration.
        p.duration = 10.5;
        let s = snapshot_from(&p, 9000.0);
        assert_eq!(s.state.position_sec, 10.5);
        assert_eq!(s.state.source_app.as_deref(), Some("Chrome"));
        assert_eq!(s.state.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn ended_and_missing_chunk_positions() {
        let p = Published {
            status: PlayerStatus::Ended,
            session: Some(info()),
            ended: true,
            available: 7.0,
            duration: 7.0,
            done: true,
            buffered: 1.0,
            text_len: 17,
            ..Published::default()
        };
        let s = snapshot_from(&p, 0.0);
        assert_eq!(s.state.position_sec, 7.0);
        assert_eq!(s.state.char_index, 17);
        assert!(!s.state.duration_is_estimate);
        let p = Published {
            status: PlayerStatus::Loading,
            session: Some(info()),
            position_base: 3.0,
            cur_exists: false,
            prev_text_end: 9,
            available: 3.0,
            duration: 8.0,
            text_len: 17,
            error: Some(("boom".into(), ReadErrorCode::Network)),
            ..Published::default()
        };
        let s = snapshot_from(&p, 0.0);
        assert_eq!(s.state.position_sec, 3.0);
        assert_eq!(s.state.char_index, 9);
        assert!(s.state.duration_is_estimate);
        assert_eq!(s.state.error.as_deref(), Some("boom"));
        assert_eq!(s.state.error_code, Some(ReadErrorCode::Network));
    }
}
