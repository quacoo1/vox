//! The widget controller (ui-settings-widget.md §6, reader-playback.md §7–§8, shell.md §6.3.7). Runs on the UI thread
//! because it is pure presentation timing:
//!
//! - View selection: toast standalone / no session → toast card; `error` → error card; no session → idle; compact →
//!   pill; else full card. "No session" includes the sessionless `idle` report the player sends after an explicit stop.
//!   The controller **never infers a hide from an `idle` snapshot** (a supersede reports `idle` for the old session
//!   after the core already showed the widget for the new read); hides come from Close, auto-hide, toast dismissal and
//!   the core's `WidgetIntent::Hide` (stop with a session, `widget.enabled` off).
//! - Compact pill (D-USW-21): spec §6.5 — art dot (DESIGN.md) on the left, play/pause, then in the middle the compact
//!   toast, the ended-early note, the `Synthesizing` loader or the small progress bar (no sentence excerpt), time,
//!   close.
//! - Body: sentence with word highlighting (`vox_text::sentence_view` over the session text with a cached
//!   `Utf16Index`; shown when highlight is on, session exists, not ended, not partial text) or the title.
//! - Labels: `format::status_label`, `widget_time_label`, `rate_label`, `progress_accessible` (`progress-accessible`,
//!   the progress bar's value), warning lead/reason/link, `warning-dot-tooltip` (`Stops early: {message}` for the
//!   compact red dot), error title/detail and action (`Open settings` for settings errors, `Try again` for retry errors
//!   with a session).
//! - `card-key` (replays the card's enter animation): bumped on a new session id, on a full↔compact switch, and each
//!   time the error card or the standalone toast card appears; status changes do not bump it. `Motion.reduce` turns the
//!   enter / sentence-fade / scroll / toast animations into 0 ms (reduce motion, reader-playback.md §8.7).
//! - Toast slot ([`ToastSlot`], pure): one toast; `revealed` / `standalone` / `hides_on_dismiss` rules; 2.5 s (5 s with
//!   an action), re-check every 1 s while hovered; a same-id toast restarts its timer.
//! - Auto-hide ([`AutoHide`], pure): `ended && autoHide && !warning` → hide after 4 s; holds (hover, toast) retry every
//!   1.5 s; a new session cancels; stopped-early reads are never auto-hidden.
//! - Close: hide locally first, then `Command::Player(Stop)` (the core's `Hide` that follows is idempotent). Speed
//!   pill: `Command::CycleRate`. `partialText` shows the title and makes previous/next skip ±10 s (the player maps
//!   the sentence skip itself).
//! - Retry: `Command::Replay`; if it fails with `unknown`/`not-cached` → `Command::ReadText(session text, sourceApp)`.
//! - The widget window is always dark (`Theme.dark = true`, D-USW-20).
//!
//! ## How `AppUi` drives the widget (integration contract of this module)
//!
//! - **Updates**: call [`WidgetController::on_update`] for **every** `UiUpdate` (after the shared state was updated;
//!   `snapshot` / `settings` are the shared ones). It handles `Player` (new-session detection, auto-hide), `Settings`
//!   (auto-hide switched on/off), `Toast` (the slot), `Widget(intent)` and `ReadStarted` (the Try-again fallback), and
//!   returns a [`WidgetEffect`] for the native window: `Show{reposition}` → `windowing::widget_window::show`, `Hide` →
//!   `windowing::widget_window::hide` (when visible), `Reposition` → `show(.., true)` only while visible.
//! - **30 Hz poll**: feed every polled snapshot through [`WidgetController::on_snapshot`] before rebuilding.
//! - **Data**: after any of the above (and after a callback), push [`WidgetController::build`] into
//!   `WidgetWindow.data` — the controller keeps `card-key` and the sentence cache itself, so `build` may be called as
//!   often as needed.
//! - **Timer**: after every call that may change a deadline (updates, callbacks, the timer itself) call
//!   [`WidgetController::arm_timer`] with a closure that re-enters `AppUi`, calls [`WidgetController::on_timer`], applies
//!   its effect, pushes the data and re-arms (the same pattern as the shell's toast timer).
//! - **Hover**: `windowing::widget_window::poll_hover` changes → [`WidgetController::set_hovered`].
//! - **Callbacks**: `action(a)` → [`WidgetController::action`] (apply the returned effect: `close` hides at once);
//!   `seek(f)` → [`WidgetController::seek`]; `error-action` → [`WidgetController::error_action`]; `warning-clicked` →
//!   [`WidgetController::warning_clicked`]; `toast-action` → [`WidgetController::toast_action`] (may hide);
//!   `drag-start` / `size-changed` stay with `windowing::widget_window`.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use slint::SharedString;
use vox_core::app::{AppRoute, Toast};
use vox_core::constants::{AUTO_HIDE_MS, HOLD_RETRY_MS, WIDGET_TOAST_HOVER_RECHECK_MS};
use vox_core::format;
use vox_core::player::{PlayerCommand, PlayerSnapshot, PlayerState, PlayerStatus, SessionInfo};
use vox_core::read::{ReadErrorCode, ReadResult, ReadTextOptions};
use vox_core::settings::Settings;
use vox_core::text16::Utf16Index;

use super::art::{ArtCache, ArtSize};
use super::generated::{MainAction, PlayerAction, ToastData, WidgetBody, WidgetData, WidgetView};
use super::shell::{phase, DeadlineTimer};
use crate::command::{Command, CoreHandle, RequestId};
use crate::update::{UiUpdate, WidgetIntent};

/// The sentence window handed to `vox_text::sentence_view` (UTF-16 units). The card scrolls a long sentence itself
/// (widget-window.slint measures the line of the current word), so only pathological sentences without any soft-split
/// point are windowed — they would otherwise make every frame lay out thousands of characters.
const SENTENCE_WINDOW_UNITS: usize = 480;
/// Pending Try-again requests kept for the fallback (the button can be pressed repeatedly).
const MAX_RETRIES: usize = 8;

/// The session id of a snapshot (`session.id`, else `state.sessionId`).
fn session_id(snapshot: &PlayerSnapshot) -> Option<&str> {
    snapshot.session.as_ref().map(|s| s.id.as_str()).or(snapshot.state.session_id.as_deref())
}

/// `SETTINGS_ERRORS`: codes the user fixes in Settings → Account.
pub fn is_settings_error(code: ReadErrorCode) -> bool {
    matches!(code, ReadErrorCode::NoApiKey | ReadErrorCode::Unauthorized | ReadErrorCode::Quota)
}

/// `RETRY_ERRORS`: codes the error card offers "Try again" for (with a session).
pub fn is_retry_error(code: ReadErrorCode) -> bool {
    matches!(code, ReadErrorCode::Network | ReadErrorCode::RateLimit | ReadErrorCode::Unknown)
}

/// The error card's button for a code (ui-settings-widget.md §6.6): `Open settings`, `Try again` (retry codes with a
/// session) or none.
pub fn error_action_label(code: ReadErrorCode, has_session: bool) -> &'static str {
    if is_settings_error(code) {
        "Open settings"
    } else if is_retry_error(code) && has_session {
        "Try again"
    } else {
        ""
    }
}

// ───────────────────────────── toast slot ─────────────────────────────

/// The toast currently in the widget's slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownToast {
    pub toast: Toast,
    /// The window had hidden itself when the toast arrived (the core showed it just for the toast).
    pub revealed: bool,
    pub session_id: Option<String>,
}

/// `hidesOnDismiss`.
pub fn hides_on_dismiss(shown: &ShownToast, snapshot: &PlayerSnapshot) -> bool {
    use vox_core::player::PlayerStatus::*;
    let sid = session_id(snapshot);
    snapshot.state.status == Idle || (snapshot.state.status == Ended && shown.revealed && shown.session_id.as_deref() == sid)
}

/// The widget's toast slot (pure; injected milliseconds; tested: the `revealed` chain, a same-id toast restarts its
/// timer, dismissal hides only per `hides_on_dismiss`).
#[derive(Debug, Default)]
pub struct ToastSlot {
    pub shown: Option<ShownToast>,
    pub deadline_ms: Option<f64>,
    /// Bumped on every push (`ToastData.key`: the banner / compact toast fade in again).
    pub key: u64,
}

impl ToastSlot {
    /// A toast arrived (`hidden` = the widget's own hidden flag, module docs): it replaces the current one,
    /// `revealed = hidden || (previous.revealed && previous.sessionId == sessionId)`, and the timer restarts (also for a
    /// same-id toast).
    pub fn push(&mut self, toast: Toast, hidden: bool, snapshot: &PlayerSnapshot, now_ms: f64) {
        let sid = session_id(snapshot).map(str::to_string);
        let inherited = self.shown.as_ref().is_some_and(|prev| prev.revealed && prev.session_id == sid);
        let duration = toast.widget_duration_ms();
        self.shown = Some(ShownToast { toast, revealed: hidden || inherited, session_id: sid });
        self.deadline_ms = Some(now_ms + duration as f64);
        self.key = self.key.wrapping_add(1);
    }
    /// The timer fired: `Some(hide_window)` when the toast is dismissed now, `None` when it stays (hovered → re-check
    /// in 1 s; not due yet; nothing shown).
    pub fn on_timer(&mut self, hovered: bool, snapshot: &PlayerSnapshot, now_ms: f64) -> Option<bool> {
        let deadline = self.deadline_ms?;
        if self.shown.is_none() {
            self.deadline_ms = None;
            return None;
        }
        if now_ms < deadline {
            return None;
        }
        if hovered {
            self.deadline_ms = Some(now_ms + WIDGET_TOAST_HOVER_RECHECK_MS as f64);
            return None;
        }
        self.dismiss(snapshot)
    }
    /// Dismisses the toast now (timer, action click): `Some(hide_window)` evaluated with the status and session **at
    /// dismissal**; `None` when nothing was shown.
    pub fn dismiss(&mut self, snapshot: &PlayerSnapshot) -> Option<bool> {
        self.deadline_ms = None;
        let shown = self.shown.take()?;
        Some(hides_on_dismiss(&shown, snapshot))
    }
    /// `toastStandalone = sessionId == null || hidesOnDismiss(..)` (false when no toast is shown).
    pub fn standalone(&self, snapshot: &PlayerSnapshot) -> bool {
        self.shown.as_ref().is_some_and(|s| session_id(snapshot).is_none() || hides_on_dismiss(s, snapshot))
    }
}

// ───────────────────────────── auto-hide ─────────────────────────────

/// The auto-hide machine (pure; tested: stopped-early reads are never auto-hidden; holds retry every 1.5 s).
#[derive(Debug, Default)]
pub struct AutoHide {
    pub deadline_ms: Option<f64>,
}

impl AutoHide {
    /// `shouldAutoHide = status == ended && autoHide && !warning` (a read that stopped early keeps its card up).
    pub fn should(snapshot: &PlayerSnapshot, auto_hide: bool) -> bool {
        snapshot.state.status == PlayerStatus::Ended && auto_hide && snapshot.warning.is_none()
    }
    /// A new snapshot (or an `autoHide` change): arm the 4 s timer when none is pending — repeated notifications never
    /// postpone the hide — or cancel it when the rule no longer holds.
    pub fn on_snapshot(&mut self, snapshot: &PlayerSnapshot, auto_hide: bool, now_ms: f64) {
        if Self::should(snapshot, auto_hide) {
            if self.deadline_ms.is_none() {
                self.deadline_ms = Some(now_ms + AUTO_HIDE_MS as f64);
            }
        } else {
            self.deadline_ms = None;
        }
    }
    /// The timer fired: true = hide now; false = a hold is active (re-armed for 1.5 s), not due yet, or not armed.
    pub fn on_timer(&mut self, hovered: bool, toast_shown: bool, now_ms: f64) -> bool {
        let Some(deadline) = self.deadline_ms else { return false };
        if now_ms < deadline {
            return false;
        }
        if hovered || toast_shown {
            self.deadline_ms = Some(now_ms + HOLD_RETRY_MS as f64);
            return false;
        }
        self.deadline_ms = None;
        true
    }
    pub fn cancel(&mut self) {
        self.deadline_ms = None;
    }
}

// ───────────────────────────── view selection ─────────────────────────────

/// Which card to render (ui-settings-widget.md §6.2, first match).
pub fn select_view(snapshot: &PlayerSnapshot, slot: &ToastSlot, compact: bool) -> WidgetView {
    let has_session = snapshot.session.is_some();
    if slot.shown.is_some() && (!has_session || slot.standalone(snapshot)) {
        WidgetView::Toast
    } else if snapshot.state.status == PlayerStatus::Error {
        WidgetView::Error
    } else if !has_session {
        WidgetView::Idle
    } else if compact {
        WidgetView::Compact
    } else {
        WidgetView::Full
    }
}

/// What a card's enter animation is keyed by (module docs: `card-key`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CardIdentity {
    Idle,
    Toast(u64),
    Error(Option<String>),
    Player { session: Option<String>, compact: bool },
}

fn card_identity(view: WidgetView, snapshot: &PlayerSnapshot, slot: &ToastSlot) -> CardIdentity {
    let session = session_id(snapshot).map(str::to_string);
    match view {
        WidgetView::Idle => CardIdentity::Idle,
        WidgetView::Toast => CardIdentity::Toast(slot.key),
        WidgetView::Error => CardIdentity::Error(session),
        WidgetView::Compact => CardIdentity::Player { session, compact: true },
        WidgetView::Full => CardIdentity::Player { session, compact: false },
    }
}

/// The time label, its estimate flag and its spoken form (`about 0:31 left`, `0:27 left`; the total when ended).
pub(crate) fn time_strings(state: &PlayerState) -> (String, bool, String) {
    let (label, estimate) = format::widget_time_label(state);
    if state.status == PlayerStatus::Ended {
        return (label.clone(), false, label);
    }
    let clock = label.trim_start_matches(['\u{2248}', '\u{2212}']);
    let spoken = if estimate { format!("about {clock} left") } else { format!("{clock} left") };
    (label, estimate, spoken)
}

// ───────────────────────────── the controller ─────────────────────────────

/// What `AppUi` must do with the native widget window (module docs).
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetEffect {
    None,
    /// Show without activating (positioned first if hidden, or if `reposition`).
    Show { reposition: bool },
    /// Hide the window (when visible).
    Hide,
    /// Re-place it if visible (placement or display change).
    Reposition,
}

/// A Try-again request whose answer may need the direct-read fallback.
#[derive(Debug, Clone)]
struct Retry {
    req: RequestId,
    text: Arc<str>,
    source_app: Option<String>,
}

/// The session text and its UTF-16 index (built once per session text).
struct TextIndex {
    text: Arc<str>,
    index: Utf16Index,
}

/// Widget state.
pub struct WidgetController {
    pub slot: ToastSlot,
    pub auto_hide: AutoHide,
    /// The widget's own hidden flag (ui-settings-widget.md §6.9): starts `true`, becomes `true` on every widget-side
    /// hide (auto-hide, Close, a toast dismissal that hides) and on the core's `Hide`, and `false` **only** when a new
    /// read session starts. The core showing the window for a toast does not clear it.
    pub hidden: bool,
    /// The pointer is over the card (hover hold; from the 100 ms poll).
    pub hovered: bool,
    /// `WidgetData.card-key` (module docs), tracked by `build`.
    card_key: Cell<i32>,
    last_card: RefCell<Option<CardIdentity>>,
    /// The last session id seen (a different one = a new read session).
    last_session: Option<String>,
    retries: Vec<Retry>,
    index: RefCell<Option<TextIndex>>,
    timer: DeadlineTimer,
}

impl Default for WidgetController {
    fn default() -> Self {
        Self {
            slot: ToastSlot::default(),
            auto_hide: AutoHide::default(),
            hidden: true,
            hovered: false,
            card_key: Cell::new(0),
            last_card: RefCell::new(None),
            last_session: None,
            retries: Vec::new(),
            index: RefCell::new(None),
            timer: DeadlineTimer::default(),
        }
    }
}

impl WidgetController {
    /// The current `card-key` (tests; the window reads it from the built data).
    #[cfg(test)]
    pub fn card_key(&self) -> i32 {
        self.card_key.get()
    }

    /// Every `UiUpdate` (module docs): returns what to do with the window.
    pub fn on_update(&mut self, core: &CoreHandle, update: &UiUpdate, snapshot: &PlayerSnapshot, settings: &Settings, now_ms: f64) -> WidgetEffect {
        match update {
            UiUpdate::Player(s) => {
                self.on_snapshot(s, settings, now_ms);
                WidgetEffect::None
            }
            UiUpdate::Settings(s) => {
                self.auto_hide.on_snapshot(snapshot, s.widget.auto_hide, now_ms);
                WidgetEffect::None
            }
            UiUpdate::Toast(t) => {
                self.on_toast(t.clone(), snapshot, now_ms);
                WidgetEffect::None
            }
            UiUpdate::Widget(WidgetIntent::Show { reposition }) => WidgetEffect::Show { reposition: *reposition },
            UiUpdate::Widget(WidgetIntent::Hide) => {
                self.mark_hidden();
                WidgetEffect::Hide
            }
            UiUpdate::Widget(WidgetIntent::Reposition) => WidgetEffect::Reposition,
            UiUpdate::ReadStarted { req, result } => {
                self.on_read_started(core, *req, result);
                WidgetEffect::None
            }
            _ => WidgetEffect::None,
        }
    }

    /// A player snapshot (discrete report or the 30 Hz poll): a new session clears the hidden flag and the pending
    /// hide; auto-hide follows the `ended` rule.
    pub fn on_snapshot(&mut self, snapshot: &PlayerSnapshot, settings: &Settings, now_ms: f64) {
        if let Some(sid) = session_id(snapshot) {
            if self.last_session.as_deref() != Some(sid) {
                self.last_session = Some(sid.to_string());
                self.hidden = false;
                self.auto_hide.cancel();
            }
        }
        self.auto_hide.on_snapshot(snapshot, settings.widget.auto_hide, now_ms);
    }

    /// A core toast (every toast reaches the widget slot as well as the app stack). A toast with neither a title nor
    /// a message has nothing to show and is ignored.
    pub fn on_toast(&mut self, toast: Toast, snapshot: &PlayerSnapshot, now_ms: f64) {
        if toast.message.is_empty() && toast.title.as_deref().unwrap_or("").is_empty() {
            return;
        }
        self.slot.push(toast, self.hidden, snapshot, now_ms);
    }

    /// The hover poll changed (hover holds auto-hide and toast dismissal).
    pub fn set_hovered(&mut self, hovered: bool) {
        self.hovered = hovered;
    }

    /// The earliest pending deadline (toast slot, auto-hide).
    pub fn next_deadline(&self) -> Option<f64> {
        match (self.slot.deadline_ms, self.auto_hide.deadline_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// (Re)arms the one widget timer for [`WidgetController::next_deadline`]; `fire` re-enters `AppUi` (module docs).
    pub fn arm_timer(&mut self, now_ms: f64, fire: impl FnOnce() + 'static) {
        let deadline = self.next_deadline();
        self.timer.arm(deadline, now_ms, fire);
    }

    /// The timer fired: the toast slot first (a dismissal may hide), then auto-hide (re-checked against the current
    /// snapshot and setting; holds: hover, a toast still shown).
    pub fn on_timer(&mut self, snapshot: &PlayerSnapshot, settings: &Settings, now_ms: f64) -> WidgetEffect {
        let mut hide = self.slot.on_timer(self.hovered, snapshot, now_ms).unwrap_or(false);
        if !AutoHide::should(snapshot, settings.widget.auto_hide) {
            self.auto_hide.cancel();
        }
        if self.auto_hide.on_timer(self.hovered, self.slot.shown.is_some(), now_ms) {
            hide = true;
        }
        if hide {
            self.mark_hidden();
            WidgetEffect::Hide
        } else {
            WidgetEffect::None
        }
    }

    /// A widget-side (or core) hide: the hidden flag and the hover hold.
    fn mark_hidden(&mut self) {
        self.hidden = true;
        self.hovered = false;
    }

    // ── callbacks ──

    /// `action(a)`: transport and chrome (ui-settings-widget.md §6.14). `close` hides locally first (returns `Hide`),
    /// then stops (`Command::Player(Stop)`; the core's `Hide` that follows is idempotent).
    pub fn action(&mut self, core: &CoreHandle, snapshot: &PlayerSnapshot, action: PlayerAction) -> WidgetEffect {
        let st = &snapshot.state;
        let has_audio = snapshot.session.is_some() && st.buffered_fraction > 0.0;
        let ended = st.status == PlayerStatus::Ended;
        match action {
            PlayerAction::Toggle => core.send(Command::Player(PlayerCommand::Toggle)),
            PlayerAction::Play => core.send(Command::Player(PlayerCommand::Play)),
            PlayerAction::Pause => core.send(Command::Player(PlayerCommand::Pause)),
            PlayerAction::Previous if has_audio => core.send(Command::Player(PlayerCommand::previous_sentence())),
            PlayerAction::Next if has_audio && !ended => core.send(Command::Player(PlayerCommand::next_sentence())),
            PlayerAction::CycleRate if snapshot.session.is_some() && st.status != PlayerStatus::Error => core.send(Command::CycleRate),
            PlayerAction::Stop => core.send(Command::Player(PlayerCommand::Stop)),
            PlayerAction::Close => {
                self.mark_hidden();
                core.send(Command::Player(PlayerCommand::Stop));
                return WidgetEffect::Hide;
            }
            _ => {}
        }
        WidgetEffect::None
    }

    /// `seek(f)`: the progress bar released at a 0..1 fraction (only with audio; the core sanitizes).
    pub fn seek(&mut self, core: &CoreHandle, snapshot: &PlayerSnapshot, fraction: f32) {
        if snapshot.session.is_none() || snapshot.state.buffered_fraction <= 0.0 || !fraction.is_finite() {
            return;
        }
        let fraction = vox_core::settings::quantize(fraction, 0.0001).clamp(0.0, 1.0);
        core.send(Command::Player(PlayerCommand::SeekFraction { fraction }));
    }

    /// `error-action()`: `Open settings` → the app on Settings → Account; `Try again` → replay the session's read
    /// (the answer may fall back to reading the session text directly, [`WidgetController::on_update`]).
    pub fn error_action(&mut self, core: &CoreHandle, snapshot: &PlayerSnapshot) {
        if snapshot.state.status != PlayerStatus::Error {
            return;
        }
        let code = snapshot.state.error_code.unwrap_or(ReadErrorCode::Unknown);
        if is_settings_error(code) {
            core.send(Command::ShowMain(Some(AppRoute::settings("account"))));
            return;
        }
        let Some(session) = snapshot.session.as_deref().filter(|_| is_retry_error(code)) else { return };
        self.retry(core, session);
    }

    fn retry(&mut self, core: &CoreHandle, session: &SessionInfo) {
        let req = core.request_id();
        if self.retries.len() >= MAX_RETRIES {
            self.retries.remove(0);
        }
        self.retries.push(Retry { req, text: Arc::clone(&session.text), source_app: session.source_app.clone() });
        core.send(Command::Replay { req: Some(req), read_id: session.read_id.clone() });
    }

    /// The answer to a Try-again replay: not-ok with `unknown` / `not-cached` (e.g. not in history because history is
    /// off) → read the session text again directly with the same `sourceApp` and no source. Other failures do nothing
    /// more (the core toasts read failures).
    fn on_read_started(&mut self, core: &CoreHandle, req: RequestId, result: &ReadResult) {
        let Some(i) = self.retries.iter().position(|r| r.req == req) else { return };
        let retry = self.retries.remove(i);
        if result.ok || !matches!(result.code, Some(ReadErrorCode::Unknown | ReadErrorCode::NotCached)) {
            return;
        }
        if retry.text.trim().is_empty() {
            return;
        }
        core.send(Command::ReadText {
            req: None,
            text: retry.text.to_string(),
            options: ReadTextOptions { source: None, source_app: retry.source_app, ..ReadTextOptions::default() },
        });
    }

    /// `warning-clicked()`: the settings-fixable warning link → Settings → Account.
    pub fn warning_clicked(&mut self, core: &CoreHandle) {
        core.send(Command::ShowMain(Some(AppRoute::settings("account"))));
    }

    /// `toast-action()`: open the app window at the action's route (or just show it when the action has no route),
    /// then dismiss — which hides the widget per `hides_on_dismiss`.
    pub fn toast_action(&mut self, core: &CoreHandle, snapshot: &PlayerSnapshot) -> WidgetEffect {
        let Some(shown) = self.slot.shown.as_ref() else { return WidgetEffect::None };
        if let Some(action) = &shown.toast.action {
            core.send(Command::ShowMain(action.route.clone()));
        }
        if self.slot.dismiss(snapshot) == Some(true) {
            self.mark_hidden();
            WidgetEffect::Hide
        } else {
            WidgetEffect::None
        }
    }

    // ── data ──

    /// The highlighted sentence of `session` at the snapshot's position (index cached per session text).
    fn sentence(&self, session: &SessionInfo, state: &PlayerState) -> vox_text::SentenceView {
        let mut cache = self.index.borrow_mut();
        if !cache.as_ref().is_some_and(|c| Arc::ptr_eq(&c.text, &session.text)) {
            *cache = Some(TextIndex { text: Arc::clone(&session.text), index: Utf16Index::new(&session.text) });
        }
        let started = state.position_sec > 0.0 || state.status == PlayerStatus::Playing;
        match cache.as_ref() {
            Some(c) => vox_text::sentence_view(&c.text, &c.index, &session.sentences, state.char_index, started, SENTENCE_WINDOW_UNITS),
            None => vox_text::SentenceView::default(),
        }
    }

    /// `WidgetData` from the snapshot, the settings and the slot (ui-settings-widget.md §6; module docs). The art is
    /// the session voice's (else the configured voice's).
    pub fn build(&self, snapshot: &PlayerSnapshot, settings: &Settings, art: &mut ArtCache) -> WidgetData {
        let voice_id = snapshot.session.as_deref().map(|s| s.voice_id.as_str()).filter(|v| !v.is_empty()).unwrap_or(settings.voice_id.as_str());
        let image = art.image(voice_id, ArtSize::Compact);
        self.build_data(snapshot, settings, image)
    }

    /// [`WidgetController::build`] with the art image already resolved (pure apart from the `card-key` / sentence
    /// caches; tested).
    pub fn build_data(&self, snapshot: &PlayerSnapshot, settings: &Settings, art: slint::Image) -> WidgetData {
        let st = &snapshot.state;
        let session = snapshot.session.as_deref();
        let has_session = session.is_some();
        let status = st.status;
        let ended = status == PlayerStatus::Ended;
        let has_audio = has_session && st.buffered_fraction > 0.0;
        let view = select_view(snapshot, &self.slot, settings.widget.compact);

        // card-key: a new card identity replays the enter animation (the idle card never animates).
        let identity = card_identity(view, snapshot, &self.slot);
        {
            let mut last = self.last_card.borrow_mut();
            if last.as_ref() != Some(&identity) {
                if identity != CardIdentity::Idle {
                    self.card_key.set(self.card_key.get().wrapping_add(1));
                }
                *last = Some(identity);
            }
        }

        let toast_visible = self.slot.shown.is_some();
        let standalone = self.slot.standalone(snapshot);
        let highlight = settings.widget.highlight_words;
        let body = if toast_visible && !standalone {
            WidgetBody::Banner
        } else if highlight && has_session && !ended && !snapshot.partial_text {
            WidgetBody::Sentence
        } else {
            WidgetBody::Title
        };

        let voice = if !st.voice_name.is_empty() {
            st.voice_name.clone()
        } else {
            session.map(|s| s.voice_name.clone()).unwrap_or_default()
        };
        let title = if !st.title.is_empty() {
            st.title.clone()
        } else {
            session.map(|s| s.title.clone()).unwrap_or_default()
        };

        let (time, time_estimate, time_accessible) = if has_session { time_strings(st) } else { (String::new(), false, String::new()) };
        let progress = if ended {
            1.0
        } else if st.duration_sec > 0.0 {
            (st.position_sec / st.duration_sec).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let (warning_lead, warning_reason, warning_link, warning_tooltip, warning_dot_tooltip) = match &snapshot.warning {
            Some(w) => {
                let link = is_settings_error(w.code);
                (
                    format::warning_lead(ended).to_string(),
                    format::warning_reason(w.code).to_string(),
                    link,
                    if link { format!("{}\nOpen settings", w.message) } else { w.message.clone() },
                    if ended { String::new() } else { format!("Stops early: {}", w.message) },
                )
            }
            None => Default::default(),
        };

        let (error_title, error_detail, error_action) = if status == PlayerStatus::Error {
            let code = st.error_code.unwrap_or(ReadErrorCode::Unknown);
            (format::error_title(code).to_string(), format::error_detail(st), error_action_label(code, has_session))
        } else {
            (String::new(), String::new(), "")
        };

        let mut data = WidgetData {
            view,
            body,
            art,
            source: if has_session { format::widget_source_label(snapshot).into() } else { SharedString::new() },
            voice: voice.into(),
            phase: phase(status),
            status: if view == WidgetView::Idle { "Ready".into() } else { format::status_label(snapshot).into() },
            status_loading: view != WidgetView::Idle && status == PlayerStatus::Loading,
            rate: format::rate_label(if st.rate > 0.0 { st.rate } else { 1.0 }).into(),
            show_rate: has_session && status != PlayerStatus::Error,
            title: title.into(),
            time: time.into(),
            time_estimate,
            time_accessible: time_accessible.into(),
            progress: progress as f32,
            buffered: st.buffered_fraction.clamp(0.0, 1.0) as f32,
            progress_accessible: if has_session { format::progress_accessible(st).into() } else { SharedString::new() },
            seekable: has_audio && status != PlayerStatus::Error,
            previous_enabled: has_audio,
            next_enabled: has_audio && !ended,
            previous_label: if snapshot.partial_text { "Back 10 seconds" } else { "Previous sentence" }.into(),
            next_label: if snapshot.partial_text { "Forward 10 seconds" } else { "Next sentence" }.into(),
            main_action: if ended {
                MainAction::PlayAgain
            } else if snapshot.wants_play {
                MainAction::Pause
            } else {
                MainAction::Play
            },
            compact_loading: view == WidgetView::Compact && status == PlayerStatus::Loading && st.buffered_fraction <= 0.0,
            warning_visible: snapshot.warning.is_some(),
            warning_lead: warning_lead.into(),
            warning_reason: warning_reason.into(),
            warning_link,
            warning_tooltip: warning_tooltip.into(),
            warning_dot_tooltip: warning_dot_tooltip.into(),
            error_title: error_title.into(),
            error_detail: error_detail.into(),
            error_action: error_action.into(),
            toast: match &self.slot.shown {
                Some(s) => super::convert::toast_data(&s.toast, false, self.slot.key),
                None => ToastData::default(),
            },
            toast_visible,
            card_key: self.card_key.get(),
            ..WidgetData::default()
        };
        if body == WidgetBody::Sentence {
            if let Some(session) = session {
                let s = self.sentence(session, st);
                data.spoken = s.spoken.into();
                data.current = s.current.into();
                data.upcoming = s.upcoming.into();
                data.sentence_key = i32::try_from(s.sentence_index).unwrap_or(i32::MAX);
            }
        }
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::player::SessionWarning;

    fn session(id: &str, text: &str) -> Arc<SessionInfo> {
        Arc::new(SessionInfo {
            id: id.into(),
            read_id: format!("read-{id}"),
            text: text.into(),
            title: "The quiet revolution".into(),
            voice_name: "George".into(),
            voice_id: "voice-george".into(),
            source_app: Some("Chrome".into()),
            from_cache: false,
            highlight_words: true,
            sentences: vox_text::sentence_starts(text).into(),
        })
    }

    fn snap(status: PlayerStatus, sid: Option<&str>) -> PlayerSnapshot {
        let text = "The quiet revolution in personal computing was never about faster chips. It was about the moment software began to listen.";
        PlayerSnapshot {
            state: PlayerState {
                status,
                session_id: sid.map(str::to_string),
                read_id: sid.map(|s| format!("read-{s}")),
                title: "The quiet revolution".into(),
                voice_name: "George".into(),
                position_sec: 14.0,
                duration_sec: 41.0,
                buffered_fraction: 0.72,
                rate: 1.0,
                duration_is_estimate: false,
                char_index: 33,
                ..PlayerState::idle()
            },
            session: sid.map(|s| session(s, text)),
            wants_play: status == PlayerStatus::Playing,
            ..PlayerSnapshot::default()
        }
    }

    fn core() -> (CoreHandle, crossbeam_channel::Receiver<Command>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (CoreHandle::new(tx), rx)
    }

    #[test]
    fn toast_slot_revealed_chain() {
        let mut slot = ToastSlot::default();
        let ended = snap(PlayerStatus::Ended, Some("s1"));
        // The window had hidden itself: revealed.
        slot.push(Toast::info("a").with_id("a"), true, &ended, 0.0);
        assert!(slot.shown.as_ref().is_some_and(|s| s.revealed));
        // A replacement in the same session inherits it even though the flag is now false.
        slot.push(Toast::info("b").with_id("b"), false, &ended, 100.0);
        assert!(slot.shown.as_ref().is_some_and(|s| s.revealed));
        // A replacement for another session does not.
        let other = snap(PlayerStatus::Ended, Some("s2"));
        slot.push(Toast::info("c").with_id("c"), false, &other, 200.0);
        assert!(slot.shown.as_ref().is_some_and(|s| !s.revealed));
        // After a dismissal nothing is inherited.
        slot.push(Toast::info("d"), true, &other, 300.0);
        assert_eq!(slot.dismiss(&other), Some(true));
        slot.push(Toast::info("e"), false, &other, 400.0);
        assert!(slot.shown.as_ref().is_some_and(|s| !s.revealed));
    }

    #[test]
    fn toast_slot_timer_restarts_and_hover_rechecks() {
        let mut slot = ToastSlot::default();
        let playing = snap(PlayerStatus::Playing, Some("s1"));
        slot.push(Toast::info("x").with_id("t"), false, &playing, 0.0);
        assert_eq!(slot.deadline_ms, Some(2_500.0));
        // Same id again: the timer restarts.
        slot.push(Toast::info("x").with_id("t"), false, &playing, 2_000.0);
        assert_eq!(slot.deadline_ms, Some(4_500.0));
        let k = slot.key;
        // An action makes it 5 s.
        slot.push(Toast::error("y").with_action("Add key", Some(AppRoute::settings("account"))), false, &playing, 3_000.0);
        assert_eq!(slot.deadline_ms, Some(8_000.0));
        assert_eq!(slot.key, k + 1);
        // Not due yet.
        assert_eq!(slot.on_timer(false, &playing, 7_999.0), None);
        // Hovered: re-check in 1 s.
        assert_eq!(slot.on_timer(true, &playing, 8_000.0), None);
        assert_eq!(slot.deadline_ms, Some(9_000.0));
        // Dismissed while a read is active: the window stays.
        assert_eq!(slot.on_timer(false, &playing, 9_000.0), Some(false));
        assert!(slot.shown.is_none() && slot.deadline_ms.is_none());
        assert_eq!(slot.on_timer(false, &playing, 10_000.0), None);
    }

    #[test]
    fn hides_on_dismiss_rules() {
        let shown = |revealed: bool, sid: Option<&str>| ShownToast { toast: Toast::info("m"), revealed, session_id: sid.map(str::to_string) };
        let idle = snap(PlayerStatus::Idle, None);
        assert!(hides_on_dismiss(&shown(false, None), &idle));
        let ended = snap(PlayerStatus::Ended, Some("s1"));
        assert!(hides_on_dismiss(&shown(true, Some("s1")), &ended));
        assert!(!hides_on_dismiss(&shown(false, Some("s1")), &ended), "a finished card the user kept");
        assert!(!hides_on_dismiss(&shown(true, Some("s0")), &ended), "another session");
        assert!(!hides_on_dismiss(&shown(true, Some("s1")), &snap(PlayerStatus::Playing, Some("s1"))));
        assert!(!hides_on_dismiss(&shown(true, Some("s1")), &snap(PlayerStatus::Error, Some("s1"))));
    }

    #[test]
    fn auto_hide_rules() {
        let mut a = AutoHide::default();
        let ended = snap(PlayerStatus::Ended, Some("s1"));
        a.on_snapshot(&ended, true, 0.0);
        assert_eq!(a.deadline_ms, Some(4_000.0));
        // Repeated notifications never postpone it.
        a.on_snapshot(&ended, true, 1_000.0);
        assert_eq!(a.deadline_ms, Some(4_000.0));
        assert!(!a.on_timer(false, false, 3_999.0));
        // Holds retry every 1.5 s.
        assert!(!a.on_timer(true, false, 4_000.0));
        assert_eq!(a.deadline_ms, Some(5_500.0));
        assert!(!a.on_timer(false, true, 5_500.0));
        assert_eq!(a.deadline_ms, Some(7_000.0));
        assert!(a.on_timer(false, false, 7_000.0));
        assert_eq!(a.deadline_ms, None);
        // autoHide off, or playing again, cancels.
        a.on_snapshot(&ended, true, 10_000.0);
        a.on_snapshot(&ended, false, 10_500.0);
        assert_eq!(a.deadline_ms, None);
        a.on_snapshot(&ended, true, 11_000.0);
        a.on_snapshot(&snap(PlayerStatus::Playing, Some("s1")), true, 11_500.0);
        assert_eq!(a.deadline_ms, None);
        // A read that stopped early is never auto-hidden.
        let mut early = ended.clone();
        early.warning = Some(SessionWarning { message: "Your ElevenLabs quota is used up.".into(), code: ReadErrorCode::Quota });
        a.on_snapshot(&early, true, 20_000.0);
        assert_eq!(a.deadline_ms, None);
        assert!(!a.on_timer(false, false, 99_000.0));
        // Paused and error cards never auto-hide.
        a.on_snapshot(&snap(PlayerStatus::Paused, Some("s1")), true, 0.0);
        a.on_snapshot(&snap(PlayerStatus::Error, Some("s1")), true, 0.0);
        assert_eq!(a.deadline_ms, None);
    }

    #[test]
    fn view_selection() {
        let mut slot = ToastSlot::default();
        let playing = snap(PlayerStatus::Playing, Some("s1"));
        assert_eq!(select_view(&playing, &slot, false), WidgetView::Full);
        assert_eq!(select_view(&playing, &slot, true), WidgetView::Compact);
        assert_eq!(select_view(&snap(PlayerStatus::Error, Some("s1")), &slot, true), WidgetView::Error);
        // The sessionless idle report after an explicit stop.
        let stopped = snap(PlayerStatus::Idle, None);
        assert_eq!(select_view(&stopped, &slot, false), WidgetView::Idle);
        // A toast with no session is standalone; on an active read it is a banner.
        slot.push(Toast::info("Select some text first"), true, &stopped, 0.0);
        assert_eq!(select_view(&stopped, &slot, false), WidgetView::Toast);
        assert_eq!(select_view(&playing, &slot, false), WidgetView::Full);
        assert!(!slot.standalone(&playing));
        // A finished read whose window had hidden itself: standalone.
        let ended = snap(PlayerStatus::Ended, Some("s1"));
        slot.push(Toast::info("x"), true, &ended, 0.0);
        assert_eq!(select_view(&ended, &slot, false), WidgetView::Toast);
        slot.push(Toast::info("y"), false, &snap(PlayerStatus::Ended, Some("s9")), 0.0);
        assert_eq!(select_view(&ended, &slot, false), WidgetView::Full);
    }

    #[test]
    fn time_labels() {
        let mut st = snap(PlayerStatus::Playing, Some("s1")).state;
        st.position_sec = 14.0;
        st.duration_sec = 41.0;
        assert_eq!(time_strings(&st), ("\u{2212}0:27".to_string(), false, "0:27 left".to_string()));
        st.duration_is_estimate = true;
        st.position_sec = 10.0;
        assert_eq!(time_strings(&st), ("\u{2248}0:31".to_string(), true, "about 0:31 left".to_string()));
        st.status = PlayerStatus::Ended;
        st.duration_is_estimate = false;
        assert_eq!(time_strings(&st), ("0:41".to_string(), false, "0:41".to_string()));
    }

    #[test]
    fn new_session_clears_hidden_and_close_hides() {
        let (core, rx) = core();
        let mut w = WidgetController::default();
        let settings = Settings::default();
        assert!(w.hidden, "the window is created hidden");
        w.on_snapshot(&snap(PlayerStatus::Loading, Some("s1")), &settings, 0.0);
        assert!(!w.hidden);
        // Close: local hide + Stop.
        assert_eq!(w.action(&core, &snap(PlayerStatus::Playing, Some("s1")), PlayerAction::Close), WidgetEffect::Hide);
        assert!(w.hidden);
        assert!(matches!(rx.try_recv(), Ok(Command::Player(PlayerCommand::Stop))));
        // The same session again does not clear the flag; a new one does.
        w.on_snapshot(&snap(PlayerStatus::Ended, Some("s1")), &settings, 0.0);
        assert!(w.hidden);
        w.on_snapshot(&snap(PlayerStatus::Loading, Some("s2")), &settings, 0.0);
        assert!(!w.hidden);
        // The core's Hide marks it hidden too.
        let fx = w.on_update(&core, &UiUpdate::Widget(WidgetIntent::Hide), &snap(PlayerStatus::Idle, None), &settings, 0.0);
        assert_eq!(fx, WidgetEffect::Hide);
        assert!(w.hidden);
        assert_eq!(
            w.on_update(&core, &UiUpdate::Widget(WidgetIntent::Show { reposition: true }), &snap(PlayerStatus::Idle, None), &settings, 0.0),
            WidgetEffect::Show { reposition: true }
        );
    }

    #[test]
    fn transport_actions() {
        let (core, rx) = core();
        let mut w = WidgetController::default();
        let playing = snap(PlayerStatus::Playing, Some("s1"));
        assert_eq!(w.action(&core, &playing, PlayerAction::Toggle), WidgetEffect::None);
        assert!(matches!(rx.try_recv(), Ok(Command::Player(PlayerCommand::Toggle))));
        let _ = w.action(&core, &playing, PlayerAction::Next);
        assert!(matches!(rx.try_recv(), Ok(Command::Player(PlayerCommand::Skip { .. }))));
        let _ = w.action(&core, &playing, PlayerAction::CycleRate);
        assert!(matches!(rx.try_recv(), Ok(Command::CycleRate)));
        // Without audio: no skips, no seek.
        let mut waiting = snap(PlayerStatus::Loading, Some("s1"));
        waiting.state.buffered_fraction = 0.0;
        let _ = w.action(&core, &waiting, PlayerAction::Previous);
        w.seek(&core, &waiting, 0.5);
        assert!(rx.try_recv().is_err());
        // Ended: next is disabled.
        let _ = w.action(&core, &snap(PlayerStatus::Ended, Some("s1")), PlayerAction::Next);
        assert!(rx.try_recv().is_err());
        w.seek(&core, &playing, 0.25);
        match rx.try_recv() {
            Ok(Command::Player(PlayerCommand::SeekFraction { fraction })) => assert!((fraction - 0.25).abs() < 1e-9),
            other => panic!("unexpected {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn error_card_actions_and_retry_fallback() {
        let (core, rx) = core();
        let mut w = WidgetController::default();
        let settings = Settings::default();
        let mut err = snap(PlayerStatus::Error, Some("s1"));
        err.state.error_code = Some(ReadErrorCode::Quota);
        w.error_action(&core, &err);
        assert!(matches!(rx.try_recv(), Ok(Command::ShowMain(Some(r))) if r == AppRoute::settings("account")));
        // Network → Try again → Replay; a not-in-history answer reads the session text directly.
        err.state.error_code = Some(ReadErrorCode::Network);
        w.error_action(&core, &err);
        let req = match rx.try_recv() {
            Ok(Command::Replay { req: Some(req), read_id }) => {
                assert_eq!(read_id, "read-s1");
                req
            }
            _ => panic!("expected a replay"),
        };
        let _ = w.on_update(&core, &UiUpdate::ReadStarted { req, result: ReadResult::not_in_history() }, &err, &settings, 0.0);
        match rx.try_recv() {
            Ok(Command::ReadText { req: None, text, options }) => {
                assert!(text.starts_with("The quiet revolution"));
                assert_eq!(options.source_app.as_deref(), Some("Chrome"));
                assert_eq!(options.source, None);
            }
            _ => panic!("expected the direct read"),
        }
        // Other failures do nothing more; unknown requests are ignored.
        w.error_action(&core, &err);
        let req = match rx.try_recv() {
            Ok(Command::Replay { req: Some(req), .. }) => req,
            _ => panic!("expected a replay"),
        };
        let _ = w.on_update(&core, &UiUpdate::ReadStarted { req, result: ReadResult::no_api_key() }, &err, &settings, 0.0);
        assert!(rx.try_recv().is_err());
        // Codes without a button do nothing.
        err.state.error_code = Some(ReadErrorCode::EmptyText);
        w.error_action(&core, &err);
        assert!(rx.try_recv().is_err());
        assert_eq!(error_action_label(ReadErrorCode::Unknown, false), "");
        assert_eq!(error_action_label(ReadErrorCode::Unknown, true), "Try again");
        assert_eq!(error_action_label(ReadErrorCode::NoApiKey, false), "Open settings");
    }

    #[test]
    fn toast_action_opens_the_route_and_hides_per_rule() {
        let (core, rx) = core();
        let mut w = WidgetController::default();
        let idle = snap(PlayerStatus::Idle, None);
        w.on_toast(Toast::error("Add your ElevenLabs API key to start reading").with_action("Add key", Some(AppRoute::settings("account"))), &idle, 0.0);
        assert_eq!(w.toast_action(&core, &idle), WidgetEffect::Hide);
        assert!(matches!(rx.try_recv(), Ok(Command::ShowMain(Some(_)))));
        // On an active read the dismissal keeps the window.
        let playing = snap(PlayerStatus::Playing, Some("s1"));
        w.on_snapshot(&playing, &Settings::default(), 0.0);
        w.on_toast(Toast::info("Reading the first 20,000 characters"), &playing, 0.0);
        assert_eq!(w.toast_action(&core, &playing), WidgetEffect::None);
        assert!(rx.try_recv().is_err(), "no action, no window");
        assert_eq!(w.toast_action(&core, &playing), WidgetEffect::None);
    }

    #[test]
    fn timer_dismisses_then_auto_hides() {
        let mut w = WidgetController::default();
        let settings = Settings::default();
        let ended = snap(PlayerStatus::Ended, Some("s1"));
        w.on_snapshot(&snap(PlayerStatus::Playing, Some("s1")), &settings, 0.0);
        w.on_snapshot(&ended, &settings, 1_000.0);
        assert_eq!(w.next_deadline(), Some(5_000.0));
        w.on_toast(Toast::info("Saved"), &ended, 4_000.0);
        assert_eq!(w.next_deadline(), Some(5_000.0));
        // At 5 s the toast hold re-arms auto-hide for 1.5 s.
        assert_eq!(w.on_timer(&ended, &settings, 5_000.0), WidgetEffect::None);
        assert_eq!(w.next_deadline(), Some(6_500.0));
        // The toast leaves at 6.5 s (not revealed: the window stays), then auto-hide hides.
        assert_eq!(w.on_timer(&ended, &settings, 6_500.0), WidgetEffect::Hide);
        assert!(w.hidden);
        assert_eq!(w.next_deadline(), None);
        // Hovered: nothing hides.
        w.on_snapshot(&snap(PlayerStatus::Playing, Some("s2")), &settings, 10_000.0);
        w.on_snapshot(&snap(PlayerStatus::Ended, Some("s2")), &settings, 10_000.0);
        w.set_hovered(true);
        assert_eq!(w.on_timer(&snap(PlayerStatus::Ended, Some("s2")), &settings, 14_000.0), WidgetEffect::None);
        assert_eq!(w.next_deadline(), Some(15_500.0));
    }

    #[test]
    fn full_card_data() {
        let w = WidgetController::default();
        let settings = Settings::default();
        let mut s = snap(PlayerStatus::Playing, Some("s1"));
        let d = w.build_data(&s, &settings, slint::Image::default());
        assert_eq!(d.view, WidgetView::Full);
        assert_eq!(d.body, WidgetBody::Sentence);
        assert_eq!(d.source, "Chrome");
        assert_eq!(d.voice, "George");
        assert_eq!(d.status, "Reading");
        assert_eq!(d.rate, "1\u{d7}");
        assert!(d.show_rate && d.seekable && d.previous_enabled && d.next_enabled);
        assert_eq!(d.time, "\u{2212}0:27");
        assert_eq!(d.time_accessible, "0:27 left");
        assert_eq!(d.progress_accessible, "0:14 of 0:41");
        assert_eq!(d.main_action, MainAction::Pause);
        assert_eq!(d.spoken, "The quiet revolution in personal");
        assert_eq!(d.current, "computing");
        assert_eq!(d.upcoming, "was never about faster chips.");
        assert_eq!(d.sentence_key, 0);
        let first_key = d.card_key;
        assert_eq!(first_key, 1);
        // A status change keeps the card; the second sentence bumps the sentence key.
        s.state.status = PlayerStatus::Paused;
        s.wants_play = false;
        s.state.char_index = 73;
        let d = w.build_data(&s, &settings, slint::Image::default());
        assert_eq!((d.card_key, d.main_action, d.status.as_str()), (first_key, MainAction::Play, "Paused"));
        assert_eq!(d.sentence_key, 1);
        assert_eq!(d.spoken, "");
        assert_eq!(d.current, "It");
        // Ended: title body, total time, next disabled, play again.
        s.state.status = PlayerStatus::Ended;
        let d = w.build_data(&s, &settings, slint::Image::default());
        assert_eq!((d.body, d.time.as_str(), d.next_enabled, d.main_action), (WidgetBody::Title, "0:41", false, MainAction::PlayAgain));
        assert_eq!(d.progress, 1.0);
        // Full ↔ compact switch replays the enter animation.
        let mut compact = settings.clone();
        compact.widget.compact = true;
        let d = w.build_data(&s, &compact, slint::Image::default());
        assert_eq!(d.view, WidgetView::Compact);
        assert_eq!(d.card_key, first_key + 1);
    }

    #[test]
    fn warning_and_error_data() {
        let w = WidgetController::default();
        let settings = Settings::default();
        let mut s = snap(PlayerStatus::Playing, Some("s1"));
        s.warning = Some(SessionWarning { message: "The connection to ElevenLabs was reset.".into(), code: ReadErrorCode::Network });
        let d = w.build_data(&s, &settings, slint::Image::default());
        assert!(d.warning_visible && !d.warning_link);
        assert_eq!((d.warning_lead.as_str(), d.warning_reason.as_str()), ("Stops early", "connection lost"));
        assert_eq!(d.warning_tooltip, "The connection to ElevenLabs was reset.");
        assert_eq!(d.warning_dot_tooltip, "Stops early: The connection to ElevenLabs was reset.");
        s.state.status = PlayerStatus::Ended;
        s.warning = Some(SessionWarning { message: "Your ElevenLabs quota is used up.".into(), code: ReadErrorCode::Quota });
        let d = w.build_data(&s, &settings, slint::Image::default());
        assert_eq!(d.status, "Stopped early");
        assert!(d.warning_link);
        assert_eq!(d.warning_tooltip, "Your ElevenLabs quota is used up.\nOpen settings");
        assert_eq!(d.warning_dot_tooltip, "");

        let mut e = snap(PlayerStatus::Error, Some("s2"));
        e.state.error_code = Some(ReadErrorCode::Network);
        e.state.error = Some("The connection timed out.".into());
        let before = w.card_key();
        let d = w.build_data(&e, &settings, slint::Image::default());
        assert_eq!(d.view, WidgetView::Error);
        assert_eq!((d.error_title.as_str(), d.error_detail.as_str(), d.error_action.as_str()), ("Can\u{2019}t reach ElevenLabs", "The connection timed out.", "Try again"));
        assert!(!d.show_rate && !d.seekable);
        assert_eq!(d.status, "Error");
        assert_eq!(d.card_key, before + 1);
        // Synthesizing: no audio yet.
        let mut l = snap(PlayerStatus::Loading, Some("s3"));
        l.state.buffered_fraction = 0.0;
        l.state.position_sec = 0.0;
        l.state.duration_is_estimate = true;
        l.wants_play = true;
        let d = w.build_data(&l, &settings, slint::Image::default());
        assert_eq!((d.status.as_str(), d.status_loading, d.seekable, d.previous_enabled), ("Synthesizing", true, false, false));
        assert_eq!(d.current, "", "every word upcoming before playback starts");
        assert!(d.upcoming.starts_with("The quiet"));
        let mut compact = settings.clone();
        compact.widget.compact = true;
        assert!(w.build_data(&l, &compact, slint::Image::default()).compact_loading);
    }

    #[test]
    fn idle_toast_and_partial_data() {
        let mut w = WidgetController::default();
        let settings = Settings::default();
        let idle = snap(PlayerStatus::Idle, None);
        let d = w.build_data(&idle, &settings, slint::Image::default());
        assert_eq!((d.view, d.status.as_str(), d.source.as_str(), d.time.as_str()), (WidgetView::Idle, "Ready", "", ""));
        assert_eq!(d.card_key, 0, "the idle card never animates");
        w.on_toast(Toast::error("Add your ElevenLabs API key to start reading").with_action("Add key", None), &idle, 0.0);
        let d = w.build_data(&idle, &settings, slint::Image::default());
        assert_eq!(d.view, WidgetView::Toast);
        assert!(d.toast_visible);
        assert_eq!(d.toast.action_label, "Add key");
        assert_eq!(d.card_key, 1);
        // A new toast is a new appearance.
        w.on_toast(Toast::info("Copy some text first"), &idle, 10.0);
        assert_eq!(w.build_data(&idle, &settings, slint::Image::default()).card_key, 2);
        // On an active read it becomes a banner.
        let mut p = snap(PlayerStatus::Playing, Some("s1"));
        let d = w.build_data(&p, &settings, slint::Image::default());
        assert_eq!((d.view, d.body), (WidgetView::Full, WidgetBody::Banner));
        // Partial text: the title, ±10 s labels.
        w.slot.shown = None;
        p.partial_text = true;
        p.session = Some(Arc::new(SessionInfo { from_cache: true, source_app: None, ..(*session("s1", "Preview…")).clone() }));
        let d = w.build_data(&p, &settings, slint::Image::default());
        assert_eq!(d.body, WidgetBody::Title);
        assert_eq!((d.previous_label.as_str(), d.next_label.as_str()), ("Back 10 seconds", "Forward 10 seconds"));
        assert_eq!(d.source, "Replay");
        // Highlighting off: the title.
        p.partial_text = false;
        let mut off = settings.clone();
        off.widget.highlight_words = false;
        assert_eq!(w.build_data(&p, &off, slint::Image::default()).body, WidgetBody::Title);
    }
}
