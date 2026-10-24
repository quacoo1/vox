//! A small deadline heap for the core loop. Timers are identified by [`TimerId`]; re-arming an id replaces its
//! deadline (debounce semantics).

use std::time::Instant;

/// Every core timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerId {
    /// 6-hourly retention.
    RetentionInterval,
    /// 60 s after the last `retentionDays` change.
    RetentionChange,
    /// 30 s safety resume of suspended hotkeys.
    HotkeySuspend,
    /// 150 ms `history:changed` throttle.
    HistoryChangedThrottle,
    /// 1.5 s after a read settled: forced credits refresh.
    CreditsAfterRead,
    /// The account's `nextResetUnix` + 5 s.
    CreditsReset,
    /// Midnight + 1 s: day rollover for Home / Analytics / History labels.
    Midnight,
    /// Player-report-independent SMTC position refresh (1 s while playing).
    MediaTick,
    /// 1 s after core startup (whatever the window visibility; not with `null_output`): `player.preload_output()`.
    PreloadOutput,
}

/// The heap.
#[derive(Debug, Default)]
pub struct Timers {
    entries: Vec<(TimerId, Instant)>,
}

impl Timers {
    /// Arms (or re-arms) `id` at `at`.
    pub fn set(&mut self, id: TimerId, at: Instant) {
        self.entries.retain(|(t, _)| *t != id);
        self.entries.push((id, at));
    }
    pub fn cancel(&mut self, id: TimerId) {
        self.entries.retain(|(t, _)| *t != id);
    }
    pub fn is_set(&self, id: TimerId) -> bool {
        self.entries.iter().any(|(t, _)| *t == id)
    }
    /// The earliest deadline.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.iter().map(|(_, at)| *at).min()
    }
    /// Removes and returns every timer due at `now`.
    pub fn take_due(&mut self, now: Instant) -> Vec<TimerId> {
        let due: Vec<TimerId> = self.entries.iter().filter(|(_, at)| *at <= now).map(|(t, _)| *t).collect();
        self.entries.retain(|(_, at)| *at > now);
        due
    }
}
