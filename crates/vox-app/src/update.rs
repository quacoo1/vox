//! `UiUpdate`: everything the core publishes to the UI thread (docs/ARCHITECTURE.md §3.3), and [`UiSink`], the
//! publisher. Updates carry **domain data** (vox-core types, `Send`); the UI thread's page controllers turn them into
//! Slint models with vox-core's format helpers. High-rate playback is not pushed per frame: the UI polls
//! `vox_audio::Player::snapshot()` at ~30 Hz while a read is active and uses [`UiUpdate::Player`] for discrete changes.
//!
//! Ordering guarantees the controllers rely on (ARCHITECTURE.md §3.3): for an API-key set or clear the core publishes
//! `Settings` and `AppInfo` **before** `ApiKey{req}` / `Done{req}`; for an `UpdateSettings{req}` it publishes
//! `Settings` then `SettingsSaved{req}`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use vox_core::analytics::AnalyticsSummary;
use vox_core::app::{AppInfo, AppRoute, ChipMode, Toast};
use vox_core::cache::CacheStats;
use vox_core::history::{HistoryPage, ReadRecord};
use vox_core::player::PlayerSnapshot;
use vox_core::read::ReadResult;
use vox_core::settings::Settings;
use vox_core::voices::{AccountInfo, ApiKeyResult, TtsModel, Voice};

use crate::command::RequestId;

/// The credits state machine's output (ui-main.md §1.5).
#[derive(Debug, Clone, PartialEq)]
pub enum CreditsView {
    Loading,
    NoKey,
    /// Latest known numbers (stale-while-revalidate keeps showing them during a refresh).
    Ready(AccountInfo),
    /// The key works but cannot read account info.
    Restricted,
    Error(String),
}

/// What `core::credits` publishes: the view plus whether a refresh is in flight (D-UM-20: the Analytics `Refresh
/// balance` button spins while `refreshing`). Published when any refresh request starts (`refreshing = true`) and
/// when the latest one settles (`false`) — success, restricted, failure or dropped as stale — **even when the view is
/// unchanged**, so a forced refresh returning identical numbers still ends the spinner.
#[derive(Debug, Clone, PartialEq)]
pub struct CreditsUpdate {
    pub view: CreditsView,
    pub refreshing: bool,
}

/// What the widget window must do (core decides showing; the UI's widget controller decides hiding, except for the
/// core-side stop rule below).
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetIntent {
    /// Show without activating; position it first if hidden (or if `reposition`).
    Show { reposition: bool },
    /// Hide. Published by the core when `widget.enabled` turns off, and by `reader::command(Stop)` whenever a session
    /// existed before the stop — for every source (UI, tray, hotkey, SMTC, bridge). Idempotent (the widget's own Close
    /// already hid locally). The UI never infers a hide from an `idle` snapshot (a supersede reports `idle` for the old
    /// session after the core already showed the widget for the new read).
    Hide,
    /// Placement settings changed or displays changed: re-place if visible.
    Reposition,
}

/// Sample-preview state for a voice id (one at a time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewUpdate {
    Loading(String),
    Playing(String),
    Stopped,
    /// The sample could not be played (download or playback failure): the Voices / onboarding controller pushes its
    /// UI-local toast id `voice-preview` (`Couldn’t play this sample. Check your connection.` for network errors, else
    /// `Couldn’t play this sample.`). Implies `Stopped`. Never a core toast (it must not show the widget or an OS
    /// notification up to 15 s after the click). `message` is that ready-to-show toast text, chosen by the core
    /// ([`PREVIEW_FAILED_CONNECTION`] when the download failed, [`PREVIEW_FAILED`] when playback failed).
    Failed { voice_id: String, message: String },
}

/// `PreviewUpdate::Failed` text when the sample could not be downloaded (ui-settings-widget.md §4.6).
pub const PREVIEW_FAILED_CONNECTION: &str = "Couldn\u{2019}t play this sample. Check your connection.";
/// `PreviewUpdate::Failed` text when the sample downloaded but could not be played.
pub const PREVIEW_FAILED: &str = "Couldn\u{2019}t play this sample.";

/// Why every page model must be rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshReason {
    /// The Windows number/date conventions changed (`vox_core::locale` was re-installed).
    Locale,
    /// Local midnight passed, or the clock / time zone changed, or the PC resumed (day keys, greetings, relative
    /// times, "Today" groups).
    Day,
}

/// Every update.
#[derive(Debug, Clone)]
pub enum UiUpdate {
    /// Every effective settings change (also API-key set/clear), and the answer to every `UpdateSettings`.
    Settings(Arc<Settings>),
    /// The answer to `UpdateSettings{req: Some}` (published right after the `Settings` it produced). `Err` = a patch
    /// that is not an object, or a write the store refused (`settings-save-error` toast; onboarding finish:
    /// `Couldn't save your setup: …`).
    SettingsSaved { req: RequestId, result: Result<(), String> },
    /// Resolved theme for the **main window** (`system` follows the OS, live). The widget and chip are always dark
    /// (D-USW-20).
    Theme { dark: bool },
    /// The OS reduce-motion setting (`Motion.reduce` on all three windows).
    Motion(bool),
    /// Every player report (status change, discrete change, 500 ms while playing, the sessionless report after an
    /// explicit stop).
    Player(PlayerSnapshot),
    Preview(PreviewUpdate),
    Credits(CreditsUpdate),
    /// Startup, `QueryAppInfo`, hook status change, and at the end of **every** bridge / Explorer-menu /
    /// launch-at-login reaction even when it skipped work (demo mode, same port), so the Settings re-check window
    /// always ends; key set/clear.
    AppInfo(Arc<AppInfo>),
    /// A core toast: shown by the main window's stack **and** the widget (contracts.md §5.3 `app:toast`).
    Toast(Toast),
    /// Navigate the main window (does not show it). (Contract variant: the core currently navigates only through
    /// `ShowMain(Some(route))`; the startup route travels in `UiInit`.)
    #[allow(dead_code)]
    Navigate(AppRoute),
    /// Show (create if needed), restore, raise and focus the main window; navigate if a route is given.
    ShowMain(Option<AppRoute>),
    HideMain,
    Widget(WidgetIntent),
    /// Chip show at the cursor (physical px) / hide (exit animation). Sent straight from the selection service's
    /// callback to keep latency low; **dropped** (not queued) while the UI is not attached.
    ChipShow { x: i32, y: i32, mode: ChipMode },
    ChipHide,
    /// Displays / work areas / DPI changed: the tray re-picks its icon size, the widget/chip re-place.
    DisplaysChanged,
    /// The 30 s hotkey-suspension safety timer fired: the recorder stops recording and clears its preview.
    HotkeysResumed,
    /// History changed (coalesced: ≥ 150 ms apart, every change followed by one).
    HistoryChanged,
    HistoryPage { req: RequestId, result: Result<(HistoryPage, usize), String> },
    Home { req: RequestId, result: Result<(Arc<AnalyticsSummary>, Vec<ReadRecord>), String> },
    Analytics { req: RequestId, result: Result<Arc<AnalyticsSummary>, String> },
    /// Voices list; `refresh` = answer to a forced refresh (a failed refresh toasts `Couldn’t refresh voices` and keeps
    /// the list; a failed first load shows the error state).
    Voices { refresh: bool, result: Result<Arc<Vec<Voice>>, String> },
    /// Models list (a failed answer keeps the shown list; no refresh distinction: nothing toasts for models).
    Models { result: Result<Arc<Vec<TtsModel>>, String> },
    /// A read started (or failed to start) for a UI request (spinners end; some failures toast in the UI).
    ReadStarted { req: RequestId, result: ReadResult },
    ApiKey { req: RequestId, result: ApiKeyResult },
    /// Generic completion of a UI action (`Err` = message for the requester's own error toast).
    Done { req: RequestId, result: Result<(), String> },
    CacheStats { req: RequestId, result: Result<CacheStats, String> },
    Exported { req: RequestId, result: Result<Option<PathBuf>, String> },
    /// `Err` = the count failed (worker failure or a caught panic): the retention guard reverts and toasts
    /// `Couldn’t check your history` (id `library-retention`).
    ExpiringCount { req: RequestId, result: Result<usize, String> },
    /// A favourite toggled: `Ok(Some(r))` applies `r.favorite`; `Ok(None)` (the record vanished) keeps the optimistic
    /// value; `Err` reverts and toasts `Couldn’t update favorites: …` (id `history-favorite`).
    FavoriteToggled { req: RequestId, result: Result<Option<ReadRecord>, String> },
    /// Rebuild every page model from the data the controllers hold; Home/Analytics also re-query.
    Refresh(RefreshReason),
    /// Close every window and leave the event loop (end of the quit sequence). The UI saves the window state
    /// synchronously first.
    Quit,
}

/// Publishes updates to the Slint event loop. Updates are queued and drained in one `invoke_from_event_loop` per
/// batch; consecutive `Player` updates in a batch collapse to the newest. **Before [`UiSink::attach`]** (the UI does
/// not exist yet) updates are only queued — nothing is dropped, so toasts raised during startup appear once the UI
/// is up (BRIEF.md "toasts raised before the UI is up are not lost"); `attach` drains the queue in order. Exception:
/// `ChipShow` / `ChipHide` are **dropped** while not attached (a replayed chip would appear at a stale cursor position
/// for a gesture that is long over).
///
/// Every drained update is applied under `catch_unwind` (a panicking UI handler is logged, scope `ui`, and the rest
/// of the batch still runs). After the event loop is gone (`invoke_from_event_loop` fails at quit) updates are simply
/// dropped.
#[derive(Clone)]
pub struct UiSink {
    inner: Arc<Mutex<SinkInner>>,
    apply: fn(UiUpdate),
}

struct SinkInner {
    queue: VecDeque<UiUpdate>,
    /// `attach()` was called: drains go through the Slint event loop.
    attached: bool,
    /// A drain is already posted to the event loop (one per batch).
    scheduled: bool,
}

impl UiSink {
    /// `apply` is called on the UI thread for every drained update (in order), once attached.
    pub fn new(apply: fn(UiUpdate)) -> Self {
        Self { inner: Arc::new(Mutex::new(SinkInner { queue: VecDeque::new(), attached: false, scheduled: false })), apply }
    }

    /// Called once on the UI thread at the end of `AppUi::create`: from now on queued updates are drained through
    /// `slint::invoke_from_event_loop`.
    pub fn attach(&self) {
        let schedule = {
            let mut inner = self.inner.lock();
            inner.attached = true;
            let schedule = !inner.queue.is_empty() && !inner.scheduled;
            if schedule {
                inner.scheduled = true;
            }
            schedule
        };
        if schedule {
            self.schedule_drain();
        }
    }

    pub fn send(&self, update: UiUpdate) {
        let schedule = {
            let mut inner = self.inner.lock();
            if !inner.attached && matches!(update, UiUpdate::ChipShow { .. } | UiUpdate::ChipHide) {
                return;
            }
            inner.queue.push_back(update);
            let schedule = inner.attached && !inner.scheduled;
            if schedule {
                inner.scheduled = true;
            }
            schedule
        };
        if schedule {
            self.schedule_drain();
        }
    }

    /// Posts one drain of the queue to the Slint event loop.
    fn schedule_drain(&self) {
        let sink = self.clone();
        if let Err(e) = slint::invoke_from_event_loop(move || sink.drain()) {
            // The event loop is gone (quit) or never existed: nothing can show these updates any more.
            let mut inner = self.inner.lock();
            inner.scheduled = false;
            inner.queue.clear();
            tracing::debug!(target: "ui", "ui updates dropped: {e}");
        }
    }

    /// Runs on the UI thread: applies the queued batch in order.
    fn drain(&self) {
        let batch: Vec<UiUpdate> = {
            let mut inner = self.inner.lock();
            inner.scheduled = false;
            inner.queue.drain(..).collect()
        };
        for update in collapse_player_updates(batch) {
            let apply = self.apply;
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(update))).is_err() {
                tracing::error!(target: "ui", "ui update handler panicked");
            }
        }
    }

    /// Test helper: everything queued so far (the sink stays detached in core tests).
    #[cfg(test)]
    pub fn take_queued(&self) -> Vec<UiUpdate> {
        self.inner.lock().queue.drain(..).collect()
    }
}

/// Consecutive `Player` updates collapse to the newest one (the UI only needs the latest snapshot).
fn collapse_player_updates(batch: Vec<UiUpdate>) -> Vec<UiUpdate> {
    let mut out: Vec<UiUpdate> = Vec::with_capacity(batch.len());
    for update in batch {
        if matches!(update, UiUpdate::Player(_)) && matches!(out.last(), Some(UiUpdate::Player(_))) {
            out.pop();
        }
        out.push(update);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignore(_: UiUpdate) {}

    #[test]
    fn detached_sink_queues_everything_but_chip_events() {
        let sink = UiSink::new(ignore);
        sink.send(UiUpdate::HistoryChanged);
        sink.send(UiUpdate::ChipShow { x: 1, y: 2, mode: ChipMode::Context });
        sink.send(UiUpdate::ChipHide);
        sink.send(UiUpdate::Toast(Toast::info("x")));
        let queued = sink.take_queued();
        assert_eq!(queued.len(), 2);
        assert!(matches!(queued[0], UiUpdate::HistoryChanged));
        assert!(matches!(queued[1], UiUpdate::Toast(_)));
    }

    #[test]
    fn consecutive_player_updates_collapse_to_the_newest() {
        let snap = |rate: f64| {
            let mut s = PlayerSnapshot::default();
            s.state.rate = rate;
            UiUpdate::Player(s)
        };
        let out = collapse_player_updates(vec![snap(1.0), snap(1.5), UiUpdate::HistoryChanged, snap(2.0), snap(0.75)]);
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[0], UiUpdate::Player(s) if s.state.rate == 1.5));
        assert!(matches!(out[1], UiUpdate::HistoryChanged));
        assert!(matches!(&out[2], UiUpdate::Player(s) if s.state.rate == 0.75));
    }
}
