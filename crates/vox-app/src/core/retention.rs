//! Retention and cache hygiene (persistence.md §5.10, §6.10; shell.md §3.2 step 7, §5.2).
//!
//! - Startup (`app::run` step 5): `history.apply_retention(days, now)`, then `reconcile_cache`: records with `cached`
//!   but no entry → `cached = false`; preview-only records whose manifest still has text → `cache.redact_text`.
//! - Core startup work (before any queued `Cli`, bridge or selection command is handled): `cache.remove_orphans(all
//!   history ids)` on the **core thread** — index-only and fast (the I/O worker deletes the folders); its candidate set
//!   is the index at call time, so a read saved later (a cold-start `--read-file`) is never deleted as an orphan.
//! - Every 6 h unless a retention change is pending; 60 s after the last `retentionDays` change (applies the
//!   then-current value). Both cancelled by shutdown. Pure schedule: [`Schedule`] (tested: `RetentionInterval` is
//!   skipped while `RetentionChange` is pending; `RetentionChange` re-arms on every change and fires once, 60 s after
//!   the last one, with the then-current value).

use std::collections::HashSet;

use vox_core::constants::{RETENTION_CHANGE_DELAY_MS, RETENTION_INTERVAL_MS};
use vox_core::history::ReadRecord;
use vox_store::{AudioCache, HistoryStore};

/// `applyRetention()`: deletes non-favourite reads older than `days` (nothing when `days == 0`, keep forever); logs
/// `retention removed N records`. Returns N.
pub fn apply_retention(history: &HistoryStore, days: u32, now_ms: f64) -> usize {
    if days == 0 {
        return 0;
    }
    let n = history.apply_retention(days, now_ms);
    if n > 0 {
        tracing::info!(target: "history", "retention removed {n} records");
    }
    n
}

/// `reconcileCache()` at startup (`app::run` step 5): stale `cached` flags cleared, preview-only text redacted from the
/// manifests (queued on the cache's I/O worker). Orphan removal is the core's startup work.
pub fn reconcile_cache(history: &HistoryStore, cache: &AudioCache) {
    let cached_ids: HashSet<String> = cache.ids().into_iter().collect();
    let with_text: HashSet<String> = cached_ids.iter().filter(|id| cache.has_text(id)).cloned().collect();
    let (stale, redact) = history.with_all(|records| reconcile_plan(records, &cached_ids, &with_text));
    if !stale.is_empty() {
        tracing::info!(target: "cache", "cleared {} stale cached flags", stale.len());
        history.set_cached(&stale, false);
    }
    for id in &redact {
        cache.redact_text(id);
    }
    if !redact.is_empty() {
        tracing::info!(target: "cache", "redacting text of {} preview-only cached reads", redact.len());
    }
}

/// The `cached = false` fixes and the ids to redact (pure).
pub fn reconcile_plan(records: &[ReadRecord], cached_ids: &HashSet<String>, ids_with_text: &HashSet<String>) -> (Vec<String>, Vec<String>) {
    let stale: Vec<String> = records.iter().filter(|r| r.cached && !cached_ids.contains(&r.id)).map(|r| r.id.clone()).collect();
    let redact: Vec<String> = records.iter().filter(|r| r.text_truncated && ids_with_text.contains(&r.id)).map(|r| r.id.clone()).collect();
    (stale, redact)
}

/// A retention run the core must perform now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionRun {
    /// The 6-hourly run (skipped while a change is pending).
    Interval,
    /// 60 s after the last `retentionDays` change, with the then-current value.
    AfterChange,
}

/// The retention timers as a pure schedule over monotonic milliseconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schedule {
    pub next_interval_ms: Option<f64>,
    pub change_due_ms: Option<f64>,
}

impl Schedule {
    /// Core startup at `now_ms`: the 6-hourly runs start (a fixed interval from startup).
    pub fn start(&mut self, now_ms: f64) {
        self.next_interval_ms = Some(now_ms + RETENTION_INTERVAL_MS as f64);
    }
    /// `retentionDays` changed at `now_ms`: (re)arm the 60 s change run.
    pub fn on_change(&mut self, now_ms: f64) {
        self.change_due_ms = Some(now_ms + RETENTION_CHANGE_DELAY_MS as f64);
    }
    /// The run due at `now_ms`, if any (re-arms the interval).
    pub fn due(&mut self, now_ms: f64) -> Option<RetentionRun> {
        if self.change_due_ms.is_some_and(|at| now_ms >= at) {
            self.change_due_ms = None;
            // The pending change applies itself; the interval keeps its own rhythm.
            self.rearm_interval(now_ms);
            return Some(RetentionRun::AfterChange);
        }
        if self.next_interval_ms.is_some_and(|at| now_ms >= at) {
            self.rearm_interval(now_ms);
            if self.change_due_ms.is_some() {
                // "A pending change applies itself; don't let the periodic run cut its grace period short."
                return None;
            }
            return Some(RetentionRun::Interval);
        }
        None
    }
    /// The earliest pending deadline (the core arms the two timers separately; kept for inspection and tests).
    #[allow(dead_code)]
    pub fn next_deadline_ms(&self) -> Option<f64> {
        match (self.next_interval_ms, self.change_due_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    /// Moves an elapsed interval deadline past `now_ms` in whole periods (a missed run is not repeated).
    fn rearm_interval(&mut self, now_ms: f64) {
        let period = RETENTION_INTERVAL_MS as f64;
        if let Some(mut at) = self.next_interval_ms {
            while at <= now_ms {
                at += period;
            }
            self.next_interval_ms = Some(at);
        }
    }
    /// Shutdown: cancel both.
    pub fn cancel(&mut self) {
        self.next_interval_ms = None;
        self.change_due_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use vox_core::history::ReadSource;

    use super::*;

    const H6: f64 = RETENTION_INTERVAL_MS as f64;
    const M1: f64 = RETENTION_CHANGE_DELAY_MS as f64;

    #[test]
    fn interval_runs_every_six_hours() {
        let mut s = Schedule::default();
        s.start(0.0);
        assert_eq!(s.due(H6 - 1.0), None);
        assert_eq!(s.due(H6), Some(RetentionRun::Interval));
        assert_eq!(s.next_interval_ms, Some(2.0 * H6));
        assert_eq!(s.due(H6 + 5.0), None);
    }

    #[test]
    fn interval_is_skipped_while_a_change_is_pending() {
        let mut s = Schedule::default();
        s.start(0.0);
        s.on_change(H6 - 10.0);
        assert_eq!(s.due(H6), None, "the pending change keeps its grace period");
        assert_eq!(s.due(H6 - 10.0 + M1), Some(RetentionRun::AfterChange));
        assert_eq!(s.change_due_ms, None);
    }

    #[test]
    fn change_rearms_and_fires_once_after_the_last_change() {
        let mut s = Schedule::default();
        s.start(0.0);
        s.on_change(1_000.0);
        s.on_change(30_000.0);
        assert_eq!(s.due(1_000.0 + M1), None, "re-armed by the second change");
        assert_eq!(s.due(30_000.0 + M1), Some(RetentionRun::AfterChange));
        assert_eq!(s.due(30_000.0 + M1 + 1.0), None, "fires once");
        assert_eq!(s.next_deadline_ms(), Some(H6));
    }

    #[test]
    fn shutdown_cancels_both() {
        let mut s = Schedule::default();
        s.start(0.0);
        s.on_change(0.0);
        s.cancel();
        assert_eq!(s.next_deadline_ms(), None);
        assert_eq!(s.due(10.0 * H6), None);
    }

    #[test]
    fn reconcile_plan_fixes_stale_flags_and_redacts_preview_only_text() {
        let mut a = ReadRecord::new("a".into(), 3.0, ReadSource::Manual);
        a.cached = true; // no entry → stale
        let mut b = ReadRecord::new("b".into(), 2.0, ReadSource::Manual);
        b.cached = true;
        b.text_truncated = true; // entry with text → redact
        let c = ReadRecord::new("c".into(), 1.0, ReadSource::Manual); // entry, not cached: kept as is
        let cached: HashSet<String> = ["b", "c"].into_iter().map(String::from).collect();
        let with_text: HashSet<String> = ["b", "c"].into_iter().map(String::from).collect();
        let (stale, redact) = reconcile_plan(&[a, b, c], &cached, &with_text);
        assert_eq!(stale, ["a"]);
        assert_eq!(redact, ["b"]);
    }
}
