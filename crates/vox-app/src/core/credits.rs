//! The ElevenLabs credits state machine (ui-main.md §1.5), owned by the core so every page shows the same numbers.
//!
//! States: loading · no-key · ready(account) · restricted · error. `refresh(force)`: latest call wins (sequence
//! number); no key and not demo → no-key; from no-key/error → loading (ready/restricted keep showing old numbers);
//! `client.get_account(force)` on a worker; `Some` → ready, `None` → restricted; failure keeps the last good numbers
//! (error only if never loaded). Every start publishes `UiUpdate::Credits(CreditsUpdate{view, refreshing: true})`
//! and the settle of the latest request publishes `refreshing: false` — even when the view is unchanged (D-UM-20).
//! Triggers: first settings, `hasApiKey` change / key replaced (forced), a read settled (forced, 1.5 s debounce),
//! main-window wake, the reset timer (`nextResetUnix` + 5 s, re-checked on resume), Analytics mount, explicit Retry /
//! Refresh (forced).
//!
//! **Wake** is derived by the core alone from `WindowEvent::MainState` transitions (the UI never sends a refresh for
//! it): hidden → visible, or unfocused → focused while visible. [`WakeThrottle`] runs the §1.5 `onWake` algorithm:
//! at most once per 30 s (`lastWake` initialised when the core starts), except that a passed `nextResetUnix` forces a
//! refresh once per reset value — the `forcedReset` memory is shared with the `CreditsReset` timer path. Tested: the
//! throttle, the passed-reset bypass, and the shared `forcedReset` with the timer.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use vox_core::constants::CREDITS_WAKE_THROTTLE_MS;
use vox_core::copy::SOMETHING_WENT_WRONG;
use vox_core::player::PlayerStatus;
use vox_core::voices::AccountInfo;

use super::timers::TimerId;
use super::{now_unix_ms, spawn_worker, Core};
use crate::command::{CatalogResult, Command};
use crate::update::{CreditsUpdate, CreditsView, UiUpdate};

/// The longest single wait of the reset timer; longer waits are re-checked (a sleeping PC can skip the moment, and
/// the wall clock may be changed meanwhile).
const RESET_RECHECK_MAX: Duration = Duration::from_secs(3600);

impl Core {
    /// `refreshCredits(force)` (module docs): publishes `refreshing: true` (or the no-key view at once), then the
    /// account fetch runs on a worker and settles through `CatalogResult::Account`.
    pub(super) fn refresh_credits(&mut self, force: bool) {
        let has_key = self.services.settings.has_api_key();
        let demo = self.demo();
        let (update, seq) = self.credits.start(has_key, demo);
        self.publish(UiUpdate::Credits(update));
        let Some(seq) = seq else {
            self.after_credits_change();
            return;
        };
        if demo && !has_key {
            self.settle_credits(seq, Ok(Some(vox_store::demo::demo_account(now_unix_ms()))));
            return;
        }
        self.maybe_recheck_proxy(false);
        let generation = self.key_generation;
        let client = self.services.client.clone();
        let core = self.handle.clone();
        let spawned = spawn_worker("vox-credits", move || {
            let result = catch_unwind(AssertUnwindSafe(|| client.get_account(force).map_err(|e| e.message)))
                .unwrap_or_else(|_| Err(SOMETHING_WENT_WRONG.to_string()));
            core.send(Command::Catalog(CatalogResult::Account { generation, forced: force, seq, result }));
        });
        if !spawned {
            self.settle_credits(seq, Err(SOMETHING_WENT_WRONG.to_string()));
        }
    }

    /// The fetch for `seq` finished: publish the settled view (stale results are dropped) and re-arm the reset timer.
    pub(super) fn settle_credits(&mut self, seq: u64, result: Result<Option<AccountInfo>, String>) {
        if let Some(update) = self.credits.settle(seq, result) {
            self.publish(UiUpdate::Credits(update));
            self.after_credits_change();
        }
    }

    /// `armReset` after every credits change.
    fn after_credits_change(&mut self) {
        if self.credits.rearm() {
            self.check_credits_reset();
        }
    }

    /// The reset timer's tick (also on resume and clock changes): wait, force a refresh once, or disarm.
    pub(super) fn check_credits_reset(&mut self) {
        match self.credits.check_reset(now_unix_ms()) {
            ResetCheck::Idle | ResetCheck::Done => self.timers.cancel(TimerId::CreditsReset),
            ResetCheck::Wait(ms) => self.timers.set(TimerId::CreditsReset, Instant::now() + reset_wait(ms)),
            ResetCheck::Refresh => {
                self.timers.cancel(TimerId::CreditsReset);
                self.refresh_credits(true);
            }
        }
    }

    /// The main window woke (hidden → visible, or focused while visible): §1.5 `onWake`.
    pub(super) fn on_main_window_wake(&mut self) {
        let next_reset = self.credits.next_reset_unix();
        match self.credits.throttle.on_wake(self.mono_ms(), now_unix_ms() / 1000.0, next_reset) {
            Some(CreditsRefresh::Forced) => self.refresh_credits(true),
            Some(CreditsRefresh::Normal) => self.refresh_credits(false),
            None => {}
        }
    }
}

/// The reset timer's next wait for a [`ResetCheck::Wait`] of `ms`: capped at [`RESET_RECHECK_MAX`] **before** the
/// conversion (`nextResetUnix` comes from the API: a finite but huge value, or one whose `× 1000` overflows to
/// infinity, would make `Duration::from_secs_f64` panic); NaN or ≤ 0 → no wait.
pub fn reset_wait(ms: f64) -> Duration {
    let secs = ms / 1000.0;
    if secs.is_nan() || secs <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(secs.min(RESET_RECHECK_MAX.as_secs_f64()))
}

/// A read settled: previous status loading/playing/paused and next idle/ended/error.
pub fn read_settled(prev: PlayerStatus, next: PlayerStatus) -> bool {
    prev.is_active() && next.is_settled()
}

/// What a wake / reset check asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditsRefresh {
    /// A normal (non-forced) refresh.
    Normal,
    /// A forced refresh (a passed reset).
    Forced,
}

/// The §1.5 wake throttle and the shared `forcedReset` memory (pure; monotonic ms and epoch seconds injected).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WakeThrottle {
    pub last_wake_ms: f64,
    /// The `nextResetUnix` value a forced refresh was already made for (timer or wake).
    pub forced_reset: Option<f64>,
}

impl WakeThrottle {
    /// A `MainState` transition counted as a wake at `now_ms`; `next_reset_unix` from the current account.
    pub fn on_wake(&mut self, now_ms: f64, now_unix: f64, next_reset_unix: Option<f64>) -> Option<CreditsRefresh> {
        let passed = next_reset_unix.filter(|r| *r > 0.0 && *r <= now_unix && Some(*r) != self.forced_reset);
        if passed.is_none() && now_ms - self.last_wake_ms < CREDITS_WAKE_THROTTLE_MS as f64 {
            return None;
        }
        self.last_wake_ms = now_ms;
        match passed {
            Some(r) => {
                self.forced_reset = Some(r);
                Some(CreditsRefresh::Forced)
            }
            None => Some(CreditsRefresh::Normal),
        }
    }
    /// The `CreditsReset` timer fired for `next_reset_unix`.
    pub fn on_reset_timer(&mut self, next_reset_unix: f64) -> Option<CreditsRefresh> {
        if self.forced_reset == Some(next_reset_unix) {
            return None;
        }
        self.forced_reset = Some(next_reset_unix);
        Some(CreditsRefresh::Forced)
    }
}

/// What a reset-timer check asks the core to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResetCheck {
    /// No account reset known: nothing armed.
    Idle,
    /// Wait this many milliseconds (then check again).
    Wait(f64),
    /// The reset passed and was not forced yet: refresh (forced) now.
    Refresh,
    /// The reset passed and was already forced (by the timer or a wake).
    Done,
}

/// The credits state machine (ui-main.md §1.5 `refreshCredits`, the reset timer's `armReset`/`tick`). Pure: the core
/// runs the account fetch on a worker and feeds the result back with the sequence number it got from
/// [`CreditsMachine::start`]; only the latest request settles the view (stale results are dropped).
#[derive(Debug, Clone, PartialEq)]
pub struct CreditsMachine {
    pub view: CreditsView,
    pub refreshing: bool,
    seq: u64,
    /// The `nextResetUnix` the reset timer is armed for.
    armed_for: Option<f64>,
    pub throttle: WakeThrottle,
}

impl Default for CreditsMachine {
    fn default() -> Self {
        Self { view: CreditsView::Loading, refreshing: false, seq: 0, armed_for: None, throttle: WakeThrottle::default() }
    }
}

impl CreditsMachine {
    /// The current published form.
    pub fn update(&self) -> CreditsUpdate {
        CreditsUpdate { view: self.view.clone(), refreshing: self.refreshing }
    }

    /// `refreshCredits` steps 2–4. Returns the update to publish and, when an account fetch must run, its sequence
    /// number (pass it to [`CreditsMachine::settle`]).
    pub fn start(&mut self, has_key: bool, demo: bool) -> (CreditsUpdate, Option<u64>) {
        self.seq += 1;
        if !has_key && !demo {
            self.view = CreditsView::NoKey;
            self.refreshing = false;
            return (self.update(), None);
        }
        if matches!(self.view, CreditsView::NoKey | CreditsView::Error(_)) {
            self.view = CreditsView::Loading;
        }
        self.refreshing = true;
        (self.update(), Some(self.seq))
    }

    /// Steps 6–7: the fetch for `seq` finished. `None` = stale (a newer request is in flight or the state was reset);
    /// else the update to publish — always with `refreshing: false`, even when the numbers did not change.
    pub fn settle(&mut self, seq: u64, result: Result<Option<AccountInfo>, String>) -> Option<CreditsUpdate> {
        if seq != self.seq {
            return None;
        }
        match result {
            Ok(Some(account)) => self.view = CreditsView::Ready(account),
            Ok(None) => self.view = CreditsView::Restricted,
            Err(message) => {
                // Keep the last good numbers; error only when nothing was ever loaded.
                if !matches!(self.view, CreditsView::Ready(_) | CreditsView::Restricted) {
                    self.view = CreditsView::Error(message);
                }
            }
        }
        self.refreshing = false;
        Some(self.update())
    }

    /// The validated account of a freshly connected key becomes the first `Ready` value (ARCHITECTURE.md §4.11).
    pub fn seed(&mut self, account: AccountInfo) {
        self.view = CreditsView::Ready(account);
    }

    /// The current account's `nextResetUnix`.
    pub fn next_reset_unix(&self) -> Option<f64> {
        match &self.view {
            CreditsView::Ready(a) => a.next_reset_unix.filter(|r| r.is_finite() && *r > 0.0),
            _ => None,
        }
    }

    /// `armReset` after every credits change: `true` when the armed reset value changed (the caller re-checks with
    /// [`CreditsMachine::check_reset`]; a disarm cancels the timer).
    pub fn rearm(&mut self) -> bool {
        let r = self.next_reset_unix();
        if r == self.armed_for {
            return false;
        }
        self.armed_for = r;
        true
    }

    /// The reset timer's `tick()` at `now_unix_ms` (wall clock).
    pub fn check_reset(&mut self, now_unix_ms: f64) -> ResetCheck {
        let Some(r) = self.armed_for else { return ResetCheck::Idle };
        let wait = r * 1000.0 - now_unix_ms;
        if wait > 0.0 {
            return ResetCheck::Wait(wait + 5_000.0);
        }
        match self.throttle.on_reset_timer(r) {
            Some(_) => ResetCheck::Refresh,
            None => ResetCheck::Done,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(reset: Option<f64>) -> AccountInfo {
        AccountInfo { tier: "creator".into(), character_count: 10.0, character_limit: 100.0, next_reset_unix: reset, status: None }
    }

    #[test]
    fn settled_detection() {
        assert!(read_settled(PlayerStatus::Playing, PlayerStatus::Ended));
        assert!(read_settled(PlayerStatus::Loading, PlayerStatus::Error));
        assert!(read_settled(PlayerStatus::Paused, PlayerStatus::Idle));
        assert!(!read_settled(PlayerStatus::Idle, PlayerStatus::Idle));
        assert!(!read_settled(PlayerStatus::Ended, PlayerStatus::Idle));
        assert!(!read_settled(PlayerStatus::Playing, PlayerStatus::Paused));
    }

    #[test]
    fn no_key_without_demo() {
        let mut m = CreditsMachine::default();
        let (u, seq) = m.start(false, false);
        assert_eq!(u, CreditsUpdate { view: CreditsView::NoKey, refreshing: false });
        assert_eq!(seq, None);
        // Demo mode counts as having a key (demo account fallback).
        let (u, seq) = m.start(false, true);
        assert_eq!(u, CreditsUpdate { view: CreditsView::Loading, refreshing: true });
        assert!(seq.is_some());
    }

    #[test]
    fn stale_results_are_dropped_and_the_latest_settles() {
        let mut m = CreditsMachine::default();
        let (_, first) = m.start(true, false);
        let (_, second) = m.start(true, false);
        assert_eq!(m.settle(first.unwrap(), Ok(Some(account(None)))), None, "older request dropped");
        assert!(m.refreshing);
        let u = m.settle(second.unwrap(), Ok(None)).unwrap();
        assert_eq!(u, CreditsUpdate { view: CreditsView::Restricted, refreshing: false });
    }

    #[test]
    fn refreshing_ends_even_with_unchanged_numbers() {
        let mut m = CreditsMachine::default();
        let (_, s) = m.start(true, false);
        m.settle(s.unwrap(), Ok(Some(account(None))));
        let (u, s) = m.start(true, false);
        assert_eq!(u, CreditsUpdate { view: CreditsView::Ready(account(None)), refreshing: true }, "stale-while-revalidate");
        let u = m.settle(s.unwrap(), Ok(Some(account(None)))).unwrap();
        assert_eq!(u, CreditsUpdate { view: CreditsView::Ready(account(None)), refreshing: false });
    }

    #[test]
    fn failures_keep_the_last_good_numbers() {
        let mut m = CreditsMachine::default();
        let (_, s) = m.start(true, false);
        assert_eq!(m.settle(s.unwrap(), Err("offline".into())).unwrap().view, CreditsView::Error("offline".into()));
        // From error a refresh shows loading again.
        let (u, s) = m.start(true, false);
        assert_eq!(u.view, CreditsView::Loading);
        m.settle(s.unwrap(), Ok(Some(account(None))));
        let (_, s) = m.start(true, false);
        assert_eq!(m.settle(s.unwrap(), Err("offline".into())).unwrap().view, CreditsView::Ready(account(None)));
        // No key → no-key, and an in-flight answer from before is dropped.
        let (_, s) = m.start(true, false);
        m.start(false, false);
        assert_eq!(m.settle(s.unwrap(), Ok(Some(account(None)))), None);
        assert_eq!(m.view, CreditsView::NoKey);
    }

    #[test]
    fn wake_throttle() {
        let mut t = WakeThrottle { last_wake_ms: 0.0, forced_reset: None };
        assert_eq!(t.on_wake(10_000.0, 1_000.0, None), None, "within 30 s of the core start");
        assert_eq!(t.on_wake(30_000.0, 1_000.0, None), Some(CreditsRefresh::Normal));
        assert_eq!(t.on_wake(40_000.0, 1_000.0, None), None);
        assert_eq!(t.on_wake(40_000.0, 1_000.0, Some(2_000.0)), None, "reset not passed yet");
    }

    #[test]
    fn a_passed_reset_bypasses_the_throttle_once() {
        let mut t = WakeThrottle { last_wake_ms: 0.0, forced_reset: None };
        assert_eq!(t.on_wake(1_000.0, 5_000.0, Some(4_000.0)), Some(CreditsRefresh::Forced));
        assert_eq!(t.forced_reset, Some(4_000.0));
        assert_eq!(t.on_wake(2_000.0, 5_000.0, Some(4_000.0)), None, "already forced for that reset value");
        // The timer path shares the memory.
        assert_eq!(t.on_reset_timer(4_000.0), None);
        assert_eq!(t.on_reset_timer(9_000.0), Some(CreditsRefresh::Forced));
        assert_eq!(t.on_wake(3_000.0, 10_000.0, Some(9_000.0)), None);
    }

    #[test]
    fn reset_timer_arms_waits_and_forces_once() {
        let mut m = CreditsMachine::default();
        assert!(!m.rearm(), "nothing to arm");
        assert_eq!(m.check_reset(0.0), ResetCheck::Idle);
        m.seed(account(Some(100.0)));
        assert!(m.rearm());
        assert!(!m.rearm(), "unchanged value → no re-arm");
        assert_eq!(m.check_reset(90_000.0), ResetCheck::Wait(15_000.0));
        assert_eq!(m.check_reset(100_000.0), ResetCheck::Refresh);
        assert_eq!(m.check_reset(101_000.0), ResetCheck::Done);
        // A wake after the timer forced it does not force again.
        assert_eq!(m.throttle.on_wake(0.0, 200.0, Some(100.0)), None);
        // An account without a reset disarms.
        m.seed(account(None));
        assert!(m.rearm());
        assert_eq!(m.check_reset(0.0), ResetCheck::Idle);
    }

    #[test]
    fn implausible_reset_values_never_panic_the_timer() {
        assert_eq!(reset_wait(15_000.0), Duration::from_secs(15));
        assert_eq!(reset_wait(10.0 * 3_600_000.0), RESET_RECHECK_MAX, "long waits are re-checked hourly");
        assert_eq!(reset_wait(1e303), RESET_RECHECK_MAX, "finite, but past Duration's range");
        assert_eq!(reset_wait(f64::INFINITY), RESET_RECHECK_MAX);
        assert_eq!(reset_wait(f64::NAN), Duration::ZERO);
        assert_eq!(reset_wait(-5.0), Duration::ZERO);
        // Through the machine: `nextResetUnix` 1e300 (and 1.8e306, whose × 1000 overflows) → a capped wait.
        for reset in [1e300, 1.8e306] {
            let mut m = CreditsMachine::default();
            m.seed(account(Some(reset)));
            assert!(m.rearm());
            match m.check_reset(1_700_000_000_000.0) {
                ResetCheck::Wait(ms) => assert_eq!(reset_wait(ms), RESET_RECHECK_MAX),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn an_already_past_reset_forces_immediately() {
        let mut m = CreditsMachine::default();
        m.seed(account(Some(50.0)));
        m.rearm();
        assert_eq!(m.check_reset(60_000.0), ResetCheck::Refresh);
    }
}
