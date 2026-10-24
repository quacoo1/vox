//! Tracked sessions and the listening fold-back (reader-playback.md §2.12, contracts.md §6.4). Pure: the reader
//! feeds player reports and engine events in, and gets history patches out. Unit-test it with the spec's rules
//! (first report writes; status changes write; ≥ 2000 ms between other writes; ledger delta to the current hour;
//! `maxPositionSec` never decreases; partial reads never complete; ≤ 16 sessions, oldest evicted; reports with
//! `session_id: None` — the sessionless report after an explicit stop — are ignored).
//!
//! Partial guard (ARCHITECTURE.md §4.14): besides `Stopped{error}` (which the engine emits before `sink.done`, so it
//! is processed before the player's `ended`), a snapshot of a non-cached session that carries a `warning` marks the
//! session `partial` **before** `ended` is evaluated — the non-fatal warning travels on the player channel before
//! `done`. Test: an `ended` snapshot with a warning never sets `completed`.

use vox_core::constants::{HISTORY_WRITE_INTERVAL_MS, MAX_TRACKED_SESSIONS};
use vox_core::history::{ReadRecord, RecordPatch};
use vox_core::js::round1;
use vox_core::listen_log::add_to_listen_log;
use vox_core::player::{PlayerSnapshot, PlayerState, PlayerStatus};

/// One engine session the reader started.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackedSession {
    pub session_id: String,
    pub read_id: String,
    pub record_saved: bool,
    pub from_cache: bool,
    pub base_listened_sec: f64,
    pub base_completed: bool,
    pub listened_sec: f64,
    pub completed: bool,
    pub max_position_sec: f64,
    pub partial: bool,
    pub last_status: Option<PlayerStatus>,
    pub last_write_ms: f64,
    pub written: Option<Written>,
}

/// The values last written for a session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Written {
    pub listened_sec: f64,
    pub completed: bool,
    pub max_position_sec: f64,
}

/// Up to 16 sessions in insertion order.
#[derive(Debug, Default)]
pub struct Sessions {
    list: Vec<TrackedSession>,
}

impl Sessions {
    /// `track(...)` right after the engine accepted a session (before any report can be processed).
    pub fn track(&mut self, session_id: String, read_id: String, record_saved: bool, from_cache: bool, base_listened_sec: f64, base_completed: bool) {
        self.list.retain(|s| s.session_id != session_id);
        while self.list.len() >= MAX_TRACKED_SESSIONS {
            self.list.remove(0);
        }
        let base_listened_sec = if base_listened_sec.is_finite() { base_listened_sec.max(0.0) } else { 0.0 };
        self.list.push(TrackedSession {
            session_id,
            read_id,
            record_saved,
            from_cache,
            base_listened_sec,
            base_completed,
            listened_sec: 0.0,
            completed: false,
            max_position_sec: 0.0,
            partial: false,
            last_status: None,
            last_write_ms: 0.0,
            written: None,
        });
    }

    /// `latestSessionFor(readId)`.
    pub fn latest_for(&mut self, read_id: &str) -> Option<&mut TrackedSession> {
        self.list.iter_mut().rev().find(|s| s.read_id == read_id)
    }

    /// The tracked session with this id (inspection: tests).
    #[allow(dead_code)]
    pub fn get(&self, session_id: &str) -> Option<&TrackedSession> {
        self.list.iter().find(|s| s.session_id == session_id)
    }

    /// Number of tracked sessions (inspection: tests).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.list.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Folds one player report (`snap.state` for the session `snap.state.session_id`; `snap.warning` → `partial` for a
    /// non-cached session, module docs); returns a [`PendingWrite`] when a history write is due. `record` is looked up
    /// by the caller (the patch needs the current ledger and `maxPositionSec`).
    pub fn on_state(&mut self, snap: &PlayerSnapshot, now_ms: f64) -> Option<PendingWrite> {
        let sid = snap.state.session_id.as_deref()?;
        let sess = self.list.iter_mut().find(|s| s.session_id == sid)?;
        if !fold(sess, snap) {
            return None;
        }
        let status_changed = sess.last_status != Some(snap.state.status);
        sess.last_status = Some(snap.state.status);
        if !status_changed && now_ms - sess.last_write_ms < HISTORY_WRITE_INTERVAL_MS as f64 {
            return None;
        }
        pending_write(sess)
    }

    /// `persistPlayback()` (quit / log-off, D-RP-6): first folds `current` (the player's exact snapshot at that moment)
    /// into the session matching `current.state.session_id` — same rules as [`Sessions::on_state`] but without the
    /// 2 s throttle — then returns a write for every saved session that has seen at least one report.
    pub fn persist_all(&mut self, now_ms: f64, current: Option<&PlayerSnapshot>) -> Vec<PendingWrite> {
        let _ = now_ms;
        if let Some(snap) = current {
            if let Some(sid) = snap.state.session_id.as_deref() {
                if let Some(sess) = self.list.iter_mut().find(|s| s.session_id == sid) {
                    if fold(sess, snap) {
                        sess.last_status = Some(snap.state.status);
                    }
                }
            }
        }
        self.list.iter().filter(|s| s.record_saved && s.last_status.is_some()).filter_map(pending_write).collect()
    }
}

/// `handlePlayerState`'s fold into one session. Returns `false` when the session has no record to write into.
fn fold(sess: &mut TrackedSession, snap: &PlayerSnapshot) -> bool {
    if snap.warning.is_some() && !sess.from_cache {
        sess.partial = true;
    }
    if !sess.record_saved {
        return false;
    }
    fold_state(sess, &snap.state);
    true
}

fn fold_state(sess: &mut TrackedSession, state: &PlayerState) {
    if state.listened_sec.is_finite() {
        sess.listened_sec = sess.listened_sec.max(state.listened_sec);
    }
    if matches!(state.status, PlayerStatus::Playing | PlayerStatus::Paused | PlayerStatus::Ended) {
        let pos = if state.status == PlayerStatus::Ended { state.duration_sec } else { state.position_sec };
        if pos.is_finite() {
            sess.max_position_sec = sess.max_position_sec.max(pos);
        }
    }
    if state.status == PlayerStatus::Ended && !sess.partial {
        sess.completed = true;
    }
}

/// `writeSession`'s "what would be written": `None` when it equals what was written last.
fn pending_write(sess: &TrackedSession) -> Option<PendingWrite> {
    let next = Written {
        listened_sec: round1(sess.base_listened_sec + sess.listened_sec),
        completed: sess.base_completed || sess.completed,
        max_position_sec: round1(sess.max_position_sec),
    };
    if sess.written == Some(next) {
        return None;
    }
    Some(PendingWrite {
        session_id: sess.session_id.clone(),
        read_id: sess.read_id.clone(),
        next,
        previous_listened_sec: sess.written.map(|w| w.listened_sec).unwrap_or(sess.base_listened_sec),
    })
}

/// A due write for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingWrite {
    pub session_id: String,
    pub read_id: String,
    pub next: Written,
    /// `written.listened_sec` or the base: the ledger delta is `next.listened_sec − previous`.
    pub previous_listened_sec: f64,
}

impl PendingWrite {
    /// `writeSession` patch for the current record: `{listenedSec, completed}` + the ledger delta at `hour_key` (when
    /// positive) + `maxPositionSec` when it exceeds the record's. The caller then marks the write done
    /// ([`Sessions::mark_written`]); a vanished record means nothing is remembered (retried next report).
    pub fn patch(&self, record: &ReadRecord, hour_key: &str) -> RecordPatch {
        let mut patch =
            RecordPatch { listened_sec: Some(self.next.listened_sec), completed: Some(self.next.completed), ..RecordPatch::default() };
        let delta = self.next.listened_sec - self.previous_listened_sec;
        if delta > 0.0 {
            patch.listen_log = Some(add_to_listen_log(record.listen_log.as_ref(), hour_key, delta));
        }
        if self.next.max_position_sec > record.max_position_sec.unwrap_or(0.0) {
            patch.max_position_sec = Some(self.next.max_position_sec);
        }
        patch
    }
}

impl Sessions {
    pub fn mark_written(&mut self, session_id: &str, written: Written, now_ms: f64) {
        if let Some(sess) = self.list.iter_mut().find(|s| s.session_id == session_id) {
            sess.written = Some(written);
            sess.last_write_ms = now_ms;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vox_core::history::ReadSource;
    use vox_core::player::{SessionInfo, SessionWarning};
    use vox_core::read::ReadErrorCode;

    use super::*;

    fn snap(sid: Option<&str>, status: PlayerStatus, pos: f64, dur: f64, listened: f64) -> PlayerSnapshot {
        let mut s = PlayerSnapshot::default();
        s.state.status = status;
        s.state.session_id = sid.map(str::to_string);
        s.state.read_id = sid.map(|x| format!("r-{x}"));
        s.state.position_sec = pos;
        s.state.duration_sec = dur;
        s.state.listened_sec = listened;
        s.session = sid.map(|x| {
            Arc::new(SessionInfo {
                id: x.into(),
                read_id: format!("r-{x}"),
                text: Arc::from("t"),
                title: "t".into(),
                voice_name: "v".into(),
                voice_id: "vid".into(),
                source_app: None,
                from_cache: false,
                highlight_words: true,
                sentences: Arc::from(vec![0u32]),
            })
        });
        s
    }

    fn record() -> ReadRecord {
        ReadRecord::new("r-a".into(), 1.0, ReadSource::Manual)
    }

    #[test]
    fn first_report_writes_then_throttles_until_status_changes() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, false, 0.0, false);
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Loading, 0.0, 10.0, 0.0), 1_000.0).expect("first report writes");
        assert_eq!(w.next, Written { listened_sec: 0.0, completed: false, max_position_sec: 0.0 });
        s.mark_written("a", w.next, 1_000.0);
        // Status change → write even 100 ms later.
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Playing, 0.2, 10.0, 0.1), 1_100.0).expect("status change writes");
        s.mark_written("a", w.next, 1_100.0);
        // Same status within 2 s → nothing.
        assert!(s.on_state(&snap(Some("a"), PlayerStatus::Playing, 1.0, 10.0, 0.9), 2_000.0).is_none());
        // ≥ 2 s later → write.
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Playing, 2.0, 10.0, 1.95), 3_100.0).expect("throttle elapsed");
        assert_eq!(w.next.listened_sec, 2.0);
        assert_eq!(w.next.max_position_sec, 2.0);
        assert_eq!(w.previous_listened_sec, 0.1);
    }

    #[test]
    fn unchanged_values_are_not_rewritten_and_do_not_reset_the_throttle() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, false, 0.0, false);
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Paused, 1.0, 10.0, 1.0), 0.0).unwrap();
        s.mark_written("a", w.next, 0.0);
        assert!(s.on_state(&snap(Some("a"), PlayerStatus::Paused, 1.0, 10.0, 1.0), 5_000.0).is_none());
        assert_eq!(s.get("a").unwrap().last_write_ms, 0.0);
    }

    #[test]
    fn max_position_never_decreases_and_ended_credits_the_duration() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, false, 0.0, false);
        s.on_state(&snap(Some("a"), PlayerStatus::Playing, 5.0, 10.0, 5.0), 0.0);
        s.on_state(&snap(Some("a"), PlayerStatus::Paused, 2.0, 10.0, 5.0), 10.0); // seek back
        assert_eq!(s.get("a").unwrap().max_position_sec, 5.0);
        // idle / loading / error never move it.
        s.on_state(&snap(Some("a"), PlayerStatus::Loading, 9.0, 10.0, 5.0), 20.0);
        assert_eq!(s.get("a").unwrap().max_position_sec, 5.0);
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Ended, 9.5, 10.25, 9.0), 30.0).unwrap();
        assert_eq!(w.next.max_position_sec, 10.3);
        assert!(w.next.completed);
    }

    #[test]
    fn partial_sessions_never_complete() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, false, 0.0, false);
        s.latest_for("r-a").unwrap().partial = true;
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Ended, 3.0, 3.0, 3.0), 0.0).unwrap();
        assert!(!w.next.completed);
    }

    #[test]
    fn an_ended_snapshot_with_a_warning_never_completes() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, false, 0.0, false);
        let mut ended = snap(Some("a"), PlayerStatus::Ended, 3.0, 3.0, 3.0);
        ended.warning = Some(SessionWarning { message: "x".into(), code: ReadErrorCode::Quota });
        let w = s.on_state(&ended, 0.0).unwrap();
        assert!(!w.next.completed);
        assert!(s.get("a").unwrap().partial);
        // A cached replay's warning does not make it partial.
        s.track("b".into(), "r-b".into(), true, true, 0.0, false);
        let mut ended = snap(Some("b"), PlayerStatus::Ended, 3.0, 3.0, 3.0);
        ended.warning = Some(SessionWarning { message: "x".into(), code: ReadErrorCode::Quota });
        assert!(s.on_state(&ended, 0.0).unwrap().next.completed);
    }

    #[test]
    fn sessionless_and_unknown_reports_are_ignored() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, false, 0.0, false);
        assert!(s.on_state(&snap(None, PlayerStatus::Idle, 0.0, 0.0, 0.0), 0.0).is_none());
        assert!(s.on_state(&snap(Some("zzz"), PlayerStatus::Playing, 1.0, 2.0, 1.0), 0.0).is_none());
        assert_eq!(s.get("a").unwrap().last_status, None);
    }

    #[test]
    fn unsaved_sessions_write_nothing_but_still_track_partial() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), false, false, 0.0, false);
        let mut playing = snap(Some("a"), PlayerStatus::Playing, 1.0, 2.0, 1.0);
        playing.warning = Some(SessionWarning { message: "x".into(), code: ReadErrorCode::Network });
        assert!(s.on_state(&playing, 0.0).is_none());
        assert!(s.get("a").unwrap().partial);
        assert!(s.persist_all(0.0, Some(&playing)).is_empty());
    }

    #[test]
    fn replays_add_on_top_of_the_base_values() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, true, 12.0, true);
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Playing, 1.0, 5.0, 2.04), 0.0).unwrap();
        assert_eq!(w.next.listened_sec, 14.0);
        assert!(w.next.completed, "baseCompleted is sticky");
        assert_eq!(w.previous_listened_sec, 12.0);
    }

    #[test]
    fn sixteen_sessions_oldest_evicted() {
        let mut s = Sessions::default();
        for i in 0..17 {
            s.track(format!("s{i}"), format!("r{i}"), true, false, 0.0, false);
        }
        assert_eq!(s.len(), 16);
        assert!(s.get("s0").is_none());
        assert!(s.get("s16").is_some());
        // A late report for the evicted session is dropped harmlessly.
        assert!(s.on_state(&snap(Some("s0"), PlayerStatus::Playing, 1.0, 2.0, 1.0), 0.0).is_none());
    }

    #[test]
    fn latest_for_finds_the_newest_session_of_a_read() {
        let mut s = Sessions::default();
        s.track("a".into(), "r".into(), true, false, 0.0, false);
        s.track("b".into(), "r".into(), true, true, 0.0, false);
        assert_eq!(s.latest_for("r").unwrap().session_id, "b");
        assert!(s.latest_for("nope").is_none());
    }

    #[test]
    fn patch_writes_ledger_delta_and_max_position_only_when_higher() {
        let w = PendingWrite {
            session_id: "a".into(),
            read_id: "r-a".into(),
            next: Written { listened_sec: 7.5, completed: false, max_position_sec: 4.0 },
            previous_listened_sec: 5.0,
        };
        let mut rec = record();
        rec.max_position_sec = Some(6.0);
        let p = w.patch(&rec, "2026-10-17T09");
        assert_eq!(p.listened_sec, Some(7.5));
        assert_eq!(p.completed, Some(false));
        assert_eq!(p.listen_log.unwrap().get("2026-10-17T09"), Some(2.5));
        assert_eq!(p.max_position_sec, None, "never decreases");
        rec.max_position_sec = None;
        let p = w.patch(&rec, "2026-10-17T09");
        assert_eq!(p.max_position_sec, Some(4.0));
        // No listening delta → no ledger.
        let w = PendingWrite { previous_listened_sec: 7.5, ..w };
        assert!(w.patch(&rec, "2026-10-17T09").listen_log.is_none());
    }

    #[test]
    fn ledger_delta_adds_to_the_existing_hour() {
        let w = PendingWrite {
            session_id: "a".into(),
            read_id: "r-a".into(),
            next: Written { listened_sec: 3.0, completed: false, max_position_sec: 0.0 },
            previous_listened_sec: 1.0,
        };
        let mut rec = record();
        rec.listen_log = Some(add_to_listen_log(None, "2026-10-17T09", 1.0));
        let p = w.patch(&rec, "2026-10-17T09");
        assert_eq!(p.listen_log.unwrap().get("2026-10-17T09"), Some(3.0));
    }

    #[test]
    fn persist_all_folds_the_exact_snapshot_without_throttle() {
        let mut s = Sessions::default();
        s.track("a".into(), "r-a".into(), true, false, 0.0, false);
        s.track("b".into(), "r-b".into(), true, false, 0.0, false); // never reported: skipped
        let w = s.on_state(&snap(Some("a"), PlayerStatus::Playing, 1.0, 10.0, 1.0), 0.0).unwrap();
        s.mark_written("a", w.next, 0.0);
        // 300 ms later (inside the throttle) the player is at 1.3 s.
        let writes = s.persist_all(300.0, Some(&snap(Some("a"), PlayerStatus::Playing, 1.3, 10.0, 1.3)));
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].read_id, "r-a");
        assert_eq!(writes[0].next.listened_sec, 1.3);
        assert_eq!(writes[0].next.max_position_sec, 1.3);
        // Nothing new → nothing to write; a sessionless current snapshot changes nothing.
        s.mark_written("a", writes[0].next, 300.0);
        assert!(s.persist_all(400.0, Some(&snap(None, PlayerStatus::Idle, 0.0, 0.0, 0.0))).is_empty());
    }
}
