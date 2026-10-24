//! Main-window shell (ui-main.md §2; DESIGN.md "Screens"): routing with an explicit back/forward stack (mouse X
//! buttons via the native event dispatcher), the onboarding gate (`!onboarded || page == onboarding`; a navigation to
//! `settings/account` while onboarding jumps to the key step), page enter/leave and wake, the sidebar (nav, search →
//! History, now-playing row, credits line / add-key, read-selection hint), Ctrl+1..5, the app toast stack and the
//! single dialog slot.
//!
//! Toast stack rules (ui-main.md §1.7; ARCHITECTURE.md §7.4): durations per `Toast::app_duration_ms`, hover pause with
//! `max(800, remaining)`, max 4 (a 5th push removes the oldest **at once**: `PushedOut`). A push with an id already in
//! the stack **replaces it atomically**: same row, timer reset, exit cancelled, moved to the newest position, a new
//! `key` (the enter animation replays) — and **no removal is reported** (the id is never absent; deleting a second
//! History row while `Removed from history` shows must not commit the first batch). Leaving by timer, close button or
//! action starts a 180 ms exit (`leaving = true`; the toast still counts as present, clicks ignored), after which the
//! row is removed and the removal reported. A programmatic dismiss (`ToastSink::dismiss`) removes at once.
//! Actions: a toast whose `action.route` is set navigates there; else a `UiToastAction::Page(p)` entry calls
//! `controllers[p].on_toast_action(id)` (dropped when `p` is not active); then the exit starts.
//!
//! Dialog slot: on a page change the shell closes an open dialog the leaving page owns (`history-clear` → History;
//! `remove-key`, `retention`, `library-clear-history` → Settings); a busy request still completes and its controller
//! still handles the answer (the `History cleared` / `Couldn’t clear history` toasts are app-level). While a dialog is
//! open, Ctrl+1..5, Ctrl+K and the sidebar nav are ignored (the rest of the window is inert, ui-main.md §4.11).
//! Programmatic navigation (a toast action, `UiUpdate::Navigate`/`ShowMain`) closes the owning dialog the same way.
//!
//! Wake (ui-main.md §1.4): [`ShellController::on_main_window_activity`] keeps one 30 s throttle per page, reset when
//! the page is entered, and returns the active page to wake (Home / Analytics re-query). Credits wake is the core's.
//! Wake marks and toast deadlines use the monotonic clock (`windowing::mono_ms`), never the wall clock.
//!
//! ## How `AppUi` drives the shell (integration contract of this module)
//!
//! - **Fan-out**: `ShellController` implements [`PageController`] with `Data = SidebarData` (`build` =
//!   [`ShellController::build_sidebar`]). Its `on_update` (call it for **every** update, before the active page):
//!   `Settings` re-evaluates the onboarding gate (a flip of `onboarded` pushes the current route into `cx.nav`);
//!   `Toast` pushes the core toast into `cx.toasts` (the stack); `Navigate(route)` / `ShowMain(Some(route))` push the
//!   route into `cx.nav` (showing the window stays with `windowing::main_window`); `SettingsSaved` of the header theme
//!   toggle toasts a failure; `Player` / `Credits` / `Settings` / `Refresh` / `AppInfo` return dirty (sidebar).
//! - **Sinks**: toasts → [`ShellController::apply_toast_sink`] (returns the `PushedOut` / `Dismissed` removals to fan
//!   out to the active page's `on_toast_removed`); navigation → [`ShellController::navigate`] then `on_leave` /
//!   `on_enter`; dialogs → [`ShellController::apply_dialog_sink`].
//! - **Toast timers** are time-driven ([`ToastStackState::tick`] / [`ToastStackState::next_deadline`]): after any stack
//!   change call [`ShellController::arm_toast_timer`] with a closure that re-enters `AppUi`, calls
//!   [`ShellController::tick_toasts`], fans the returned removals out (`on_toast_removed`), pushes
//!   [`ShellController::build_toasts`] and re-arms. Slint callbacks: `toast-hover` → [`ShellController::toast_hover`],
//!   `toast-dismiss` → [`ShellController::toast_dismiss`], `toast-action` → [`ShellController::toast_action`] (route →
//!   navigate; `Page(p)` → `p`'s `on_toast_action` when active).
//! - **Dialog callbacks**: `dialog-confirm` / `dialog-cancel` → [`ShellController::dialog_confirm`] /
//!   [`ShellController::dialog_cancel`] return the owning page (ignored while busy) → its `on_dialog(id, confirmed)`;
//!   an unhandled cancel → [`ShellController::close_dialog`].
//! - **Other shell callbacks**: `navigate(PageId)` → [`ShellController::nav_clicked`], `sidebar-search` →
//!   [`ShellController::sidebar_search`], `mini-action` → [`ShellController::mini_action`], `mini-open` →
//!   [`ShellController::mini_open`], `credits-open` → [`ShellController::credits_open`], `header-send-to-tray` →
//!   [`ShellController::send_to_tray`], `header-toggle-theme` → [`ShellController::toggle_theme`].
//! - **Main-window properties**: `page` = [`page_id`] of [`ShellController::shown_page`], `onboarding` =
//!   [`ShellController::onboarding`], `page-title` = [`ShellController::page_title`], `sidebar` = `build_sidebar`,
//!   `toasts` = `build_toasts`, `dialog` = [`ShellController::dialog_data`], `settings-scroll-to/-nonce` =
//!   [`ShellController::settings_scroll`].

use std::collections::HashMap;
use std::time::Duration;

use slint::{ModelRc, SharedString, VecModel};
use vox_core::app::{AppPage, AppRoute, Toast};
use vox_core::constants::{CREDITS_WAKE_THROTTLE_MS, MAX_TOASTS, READ_WITH_LABEL, TOAST_EXIT_MS};
use vox_core::format;
use vox_core::player::{PlayerState, PlayerStatus};
use vox_core::settings::Settings;

use super::art::ArtSize;
use super::generated::{DialogData, MiniPlayerData, NavItemData, PageId, PlayerAction, PlayerPhase, SidebarData, ToastData};
use super::{DialogSink, DialogSpec, NavIntent, PageController, ToastSink, UiCtx, UiToastAction};
use crate::command::{Command, RequestId};
use crate::update::{CreditsUpdate, CreditsView, UiUpdate};

/// How many routes the back/forward stack keeps (the window lives in the tray for days).
const MAX_ROUTE_HISTORY: usize = 100;
/// A paused toast resumes with at least this much time left (ui-main.md §1.7).
const TOAST_MIN_RESUME_MS: u64 = 800;
/// Page wake throttle (ui-main.md §1.4 `useWakeCount(30_000)`).
const WAKE_THROTTLE_MS: f64 = CREDITS_WAKE_THROTTLE_MS as f64;

// ───────────────────────────── small shared helpers ─────────────────────────────

/// Runs `f`, catching a panic so it never unwinds into Slint (ARCHITECTURE.md §3.4); the panic is logged as
/// `ui callback panicked` (scope `panic`).
pub(crate) fn guarded<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            tracing::error!(target: "panic", "ui callback panicked: {what}: {msg}");
            None
        }
    }
}

/// `AppPage` → the Slint `PageId`.
pub fn page_id(p: AppPage) -> PageId {
    match p {
        AppPage::Home => PageId::Home,
        AppPage::History => PageId::History,
        AppPage::Analytics => PageId::Analytics,
        AppPage::Voices => PageId::Voices,
        AppPage::Settings => PageId::Settings,
        AppPage::Onboarding => PageId::Onboarding,
    }
}

/// The Slint `PageId` → `AppPage`.
pub fn app_page(p: PageId) -> AppPage {
    match p {
        PageId::Home => AppPage::Home,
        PageId::History => AppPage::History,
        PageId::Analytics => AppPage::Analytics,
        PageId::Voices => AppPage::Voices,
        PageId::Settings => AppPage::Settings,
        PageId::Onboarding => AppPage::Onboarding,
    }
}

/// Player status as the UI sees it.
pub fn phase(status: PlayerStatus) -> PlayerPhase {
    match status {
        PlayerStatus::Idle => PlayerPhase::Idle,
        PlayerStatus::Loading => PlayerPhase::Loading,
        PlayerStatus::Playing => PlayerPhase::Playing,
        PlayerStatus::Paused => PlayerPhase::Paused,
        PlayerStatus::Ended => PlayerPhase::Ended,
        PlayerStatus::Error => PlayerPhase::Error,
    }
}

/// A string list model.
pub(crate) fn string_model(items: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    let v: Vec<SharedString> = items.into_iter().map(SharedString::from).collect();
    ModelRc::new(VecModel::from(v))
}

/// `[a, b, …]` joined with ` · `, empty parts dropped.
pub(crate) fn join_dot<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" \u{00B7} ")
}

/// The read-selection hotkey shown in hints (ui-main.md §1.8): empty when hotkeys are off or it is unset.
pub(crate) fn read_selection_hotkey(settings: &Settings) -> &str {
    if settings.triggers.hotkeys_enabled {
        settings.triggers.hotkeys.read_selection.trim()
    } else {
        ""
    }
}

/// One single-shot `slint::Timer` re-armed to the next deadline of a time-driven state machine (the toast stack, the
/// widget's auto-hide / toast slot). The callback runs inside [`guarded`].
#[derive(Default)]
pub struct DeadlineTimer {
    timer: slint::Timer,
    armed_for: Option<f64>,
}

impl std::fmt::Debug for DeadlineTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeadlineTimer").field("armed_for", &self.armed_for).finish()
    }
}

impl DeadlineTimer {
    /// Arms the timer for `deadline_ms` (epoch ms; `None` cancels). Re-arming for the same deadline while it is
    /// pending does nothing.
    pub fn arm(&mut self, deadline_ms: Option<f64>, now_ms: f64, fire: impl FnOnce() + 'static) {
        let Some(at) = deadline_ms.filter(|d| d.is_finite()) else {
            self.cancel();
            return;
        };
        if self.armed_for == Some(at) && self.timer.running() {
            return;
        }
        let delay = (at - now_ms).max(0.0).ceil() as u64;
        let mut fire = Some(fire);
        self.armed_for = Some(at);
        self.timer.start(slint::TimerMode::SingleShot, Duration::from_millis(delay), move || {
            if let Some(f) = fire.take() {
                guarded("deadline timer", f);
            }
        });
    }
    pub fn cancel(&mut self) {
        self.timer.stop();
        self.armed_for = None;
    }
}

// ───────────────────────────── routes ─────────────────────────────

/// An empty section is no section (`toRoute`, ui-main.md §2.1).
fn normalized(mut route: AppRoute) -> AppRoute {
    if route.section.as_deref().is_some_and(str::is_empty) {
        route.section = None;
    }
    route
}

/// Back/forward route history.
#[derive(Debug, Default)]
pub struct RouteHistory {
    stack: Vec<AppRoute>,
    index: usize,
}

impl RouteHistory {
    /// Pushes a route (drops forward entries); an identical route with a section re-announces it.
    pub fn navigate(&mut self, route: AppRoute) -> bool {
        let route = normalized(route);
        if let Some(cur) = self.current() {
            if *cur == route {
                return route.section.is_some();
            }
            self.stack.truncate(self.index + 1);
        }
        self.stack.push(route);
        if self.stack.len() > MAX_ROUTE_HISTORY {
            let extra = self.stack.len() - MAX_ROUTE_HISTORY;
            self.stack.drain(..extra);
        }
        self.index = self.stack.len() - 1;
        true
    }
    /// The previous route, skipping the onboarding gate (ui-main.md §2.3: the gate is never a back/forward target; the
    /// startup `onboarding` route stays in the stack after onboarding finishes). `None` (index unchanged) when there
    /// is none.
    pub fn back(&mut self) -> Option<&AppRoute> {
        let i = (0..self.index.min(self.stack.len())).rev().find(|&i| self.stack[i].page != AppPage::Onboarding)?;
        self.index = i;
        self.stack.get(i)
    }
    /// The next route, skipping the onboarding gate (see [`RouteHistory::back`]).
    pub fn forward(&mut self) -> Option<&AppRoute> {
        let i = (self.index + 1..self.stack.len()).find(|&i| self.stack[i].page != AppPage::Onboarding)?;
        self.index = i;
        self.stack.get(i)
    }
    pub fn current(&self) -> Option<&AppRoute> {
        self.stack.get(self.index)
    }
}

// ───────────────────────────── toast stack ─────────────────────────────

/// Why a toast left the stack. A same-id replacement is **not** a removal (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastRemoval {
    TimedOut,
    Dismissed,
    Action,
    PushedOut,
}

/// One toast in the app stack.
#[derive(Debug, Clone, PartialEq)]
pub struct StackEntry {
    pub toast: Toast,
    pub local: Option<UiToastAction>,
    /// In its 180 ms exit (still present; clicks ignored).
    pub leaving: bool,
    pub remaining_ms: u64,
    pub paused: bool,
    /// Unique per push (`ToastData.key`): a replacement gets a new key so the enter animation replays.
    pub key: u64,
}

/// The clock of one entry (keyed by `StackEntry::key`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ToastClock {
    /// When the countdown (re)started; `None` = not started yet (starts at the next `tick` / `next_deadline`) or
    /// paused.
    since: Option<f64>,
    /// When the exit animation ends; `None` while leaving = not started yet.
    exit_at: Option<f64>,
    /// Why it is leaving.
    why: Option<ToastRemoval>,
}

/// The app window's toast stack state (pure; tested: a same-id push reports no removal; a 5th push yields
/// `PushedOut` for the oldest; an exit keeps the toast present until `finish_exit`).
///
/// Time is injected: a pushed toast's countdown starts at the next [`ToastStackState::tick`] /
/// [`ToastStackState::next_deadline`] (both take `now_ms`), hover pauses it ([`ToastStackState::set_hover`]).
#[derive(Debug, Default)]
pub struct ToastStackState {
    entries: Vec<StackEntry>,
    next_key: u64,
    clocks: HashMap<u64, ToastClock>,
}

impl ToastStackState {
    /// Pushes, or replaces the same id in place (timer reset, exit cancelled, moved to newest, new key); returns the
    /// removals to report (only `PushedOut`).
    pub fn push(&mut self, toast: Toast, local: Option<UiToastAction>) -> Vec<(String, ToastRemoval)> {
        self.next_key += 1;
        let key = self.next_key;
        if let Some(i) = self.entries.iter().position(|e| e.toast.id == toast.id) {
            let old = self.entries.remove(i);
            self.clocks.remove(&old.key);
        }
        let remaining_ms = toast.app_duration_ms();
        self.entries.push(StackEntry { toast, local, leaving: false, remaining_ms, paused: false, key });
        self.clocks.insert(key, ToastClock::default());
        let mut removed = Vec::new();
        while self.entries.len() > MAX_TOASTS {
            let old = self.entries.remove(0);
            self.clocks.remove(&old.key);
            removed.push((old.toast.id, ToastRemoval::PushedOut));
        }
        removed
    }
    /// Starts the 180 ms exit (timer, close button, action) with the clock started by the next `tick`. False when
    /// absent or already leaving. (Production code passes the clock: [`ToastStackState::begin_exit_at`].)
    #[cfg(test)]
    pub fn begin_exit(&mut self, id: &str, why: ToastRemoval) -> bool {
        self.begin_exit_inner(id, why, None)
    }
    /// [`ToastStackState::begin_exit`] with the clock: the exit ends at `now_ms + 180`.
    pub fn begin_exit_at(&mut self, id: &str, why: ToastRemoval, now_ms: f64) -> bool {
        self.begin_exit_inner(id, why, Some(now_ms))
    }
    fn begin_exit_inner(&mut self, id: &str, why: ToastRemoval, now_ms: Option<f64>) -> bool {
        let Some(e) = self.entries.iter_mut().find(|e| e.toast.id == id) else { return false };
        if e.leaving {
            return false;
        }
        e.leaving = true;
        let clock = self.clocks.entry(e.key).or_default();
        clock.why = Some(why);
        clock.since = None;
        clock.exit_at = now_ms.map(|n| n + TOAST_EXIT_MS as f64);
        true
    }
    /// The exit finished: removes the row and returns the removal to report.
    pub fn finish_exit(&mut self, id: &str) -> Option<(String, ToastRemoval)> {
        let i = self.entries.iter().position(|e| e.toast.id == id && e.leaving)?;
        let e = self.entries.remove(i);
        let why = self.clocks.remove(&e.key).and_then(|c| c.why).unwrap_or(ToastRemoval::Dismissed);
        Some((e.toast.id, why))
    }
    /// Programmatic dismiss: removed at once (reported as `Dismissed`).
    pub fn dismiss_now(&mut self, id: &str) -> Option<(String, ToastRemoval)> {
        let i = self.entries.iter().position(|e| e.toast.id == id)?;
        let e = self.entries.remove(i);
        self.clocks.remove(&e.key);
        Some((e.toast.id, ToastRemoval::Dismissed))
    }
    /// Present (a leaving toast counts as present).
    #[cfg(test)]
    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|e| e.toast.id == id)
    }
    pub fn entries(&self) -> &[StackEntry] {
        &self.entries
    }
    /// Pointer / focus entered (`true`) or left a toast: pausing keeps `max(800, remaining)`; resuming restarts the
    /// countdown for that remaining time. Returns whether anything changed (leaving toasts are not paused).
    pub fn set_hover(&mut self, id: &str, hovered: bool, now_ms: f64) -> bool {
        let Some(e) = self.entries.iter_mut().find(|e| e.toast.id == id) else { return false };
        if e.leaving {
            return false;
        }
        let clock = self.clocks.entry(e.key).or_default();
        if hovered && !e.paused {
            if let Some(since) = clock.since {
                let left = since + e.remaining_ms as f64 - now_ms;
                e.remaining_ms = left.max(TOAST_MIN_RESUME_MS as f64).ceil() as u64;
            } else {
                e.remaining_ms = e.remaining_ms.max(TOAST_MIN_RESUME_MS);
            }
            e.paused = true;
            clock.since = None;
            true
        } else if !hovered && e.paused {
            e.paused = false;
            clock.since = Some(now_ms);
            true
        } else {
            false
        }
    }
    /// Starts every countdown / exit that has not started yet.
    fn start_clocks(&mut self, now_ms: f64) {
        for e in &self.entries {
            let clock = self.clocks.entry(e.key).or_default();
            if e.leaving {
                if clock.exit_at.is_none() {
                    clock.exit_at = Some(now_ms + TOAST_EXIT_MS as f64);
                }
            } else if !e.paused && clock.since.is_none() {
                clock.since = Some(now_ms);
            }
        }
    }
    /// Advances the clock: timed-out toasts start their exit, finished exits are removed. Returns the removals to
    /// report (in stack order).
    pub fn tick(&mut self, now_ms: f64) -> Vec<(String, ToastRemoval)> {
        self.start_clocks(now_ms);
        let due: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !e.leaving && !e.paused)
            .filter(|e| self.clocks.get(&e.key).and_then(|c| c.since).is_some_and(|s| now_ms >= s + e.remaining_ms as f64))
            .map(|e| e.toast.id.clone())
            .collect();
        for id in due {
            self.begin_exit_at(&id, ToastRemoval::TimedOut, now_ms);
        }
        let done: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.leaving)
            .filter(|e| self.clocks.get(&e.key).and_then(|c| c.exit_at).is_some_and(|t| now_ms >= t))
            .map(|e| e.toast.id.clone())
            .collect();
        done.iter().filter_map(|id| self.finish_exit(id)).collect()
    }
    /// The next moment [`ToastStackState::tick`] has work (starts pending clocks at `now_ms`); `None` when nothing
    /// runs (empty stack, or every toast paused).
    pub fn next_deadline(&mut self, now_ms: f64) -> Option<f64> {
        self.start_clocks(now_ms);
        self.entries
            .iter()
            .filter_map(|e| {
                let c = self.clocks.get(&e.key)?;
                if e.leaving {
                    c.exit_at
                } else if e.paused {
                    None
                } else {
                    c.since.map(|s| s + e.remaining_ms as f64)
                }
            })
            .min_by(|a, b| a.total_cmp(b))
    }
}

// ───────────────────────────── dialog ─────────────────────────────

/// The open dialog (one slot).
#[derive(Debug, Default)]
pub struct DialogSlot {
    pub open: Option<super::DialogSpec>,
}

impl DialogSlot {
    fn get(&self, id: &str) -> Option<&DialogSpec> {
        self.open.as_ref().filter(|d| d.id == id)
    }
}

// ───────────────────────────── navigation ─────────────────────────────

/// What a navigation changed: the page left (its controller gets `on_leave`) and the page entered (`on_enter` with
/// the route and intent). Both `None` when nothing changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Navigation {
    pub left: Option<AppPage>,
    pub entered: Option<AppPage>,
    pub route: AppRoute,
    pub intent: NavIntent,
}

/// What a toast's action button does (the exit has already started).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastActionTarget {
    /// Navigate the main window there (core toasts with a route).
    Route(AppRoute),
    /// Forward to that page's controller (`on_toast_action(id)`), only when it is the active page.
    Page(AppPage),
    /// An action without a route: the toast just leaves.
    Nothing,
}

/// The shell controller (owned by `AppUi`).
#[derive(Debug, Default)]
pub struct ShellController {
    pub routes: RouteHistory,
    pub toasts: ToastStackState,
    pub dialog: DialogSlot,
    /// Per page: when it last woke (or was entered), for the 30 s wake throttle.
    wake_marks: HashMap<AppPage, f64>,
    /// The page on screen (the effective page of the current route).
    shown: Option<AppPage>,
    /// The last main-window `(focused, visible)`.
    activity: (bool, bool),
    /// `onboarded` at the last navigation (the gate is re-evaluated when settings flip it).
    onboarded: Option<bool>,
    /// The header theme toggle's pending `UpdateSettings`.
    theme_req: Option<RequestId>,
    /// `settings-scroll-to` / `settings-scroll-nonce`.
    scroll_to: String,
    scroll_nonce: i32,
    toast_timer: DeadlineTimer,
}

impl ShellController {
    /// Navigates (effective page via [`effective_page`]); closes a dialog owned by the leaving page. `AppUi` then calls
    /// `on_leave` / `on_enter` on the controllers named in the result.
    pub fn navigate(&mut self, route: AppRoute, intent: NavIntent, onboarded: bool) -> Navigation {
        self.navigate_at(route, intent, onboarded, crate::windowing::mono_ms())
    }
    /// [`ShellController::navigate`] with an injected clock (the entered page's wake mark).
    pub fn navigate_at(&mut self, route: AppRoute, intent: NavIntent, onboarded: bool, now_ms: f64) -> Navigation {
        let route = normalized(route);
        let announce = self.routes.navigate(route.clone());
        self.transition(route, intent, onboarded, announce, now_ms)
    }
    fn transition(&mut self, route: AppRoute, intent: NavIntent, onboarded: bool, announce: bool, now_ms: f64) -> Navigation {
        self.onboarded = Some(onboarded);
        let page = effective_page(&route, onboarded);
        let page_changed = self.shown != Some(page);
        let left = if page_changed { self.shown } else { None };
        let entered = (page_changed || announce || intent != NavIntent::None).then_some(page);
        if let Some(l) = left {
            if self.dialog.open.as_ref().is_some_and(|d| d.owner == l) {
                self.dialog.open = None;
            }
        }
        if page_changed {
            self.shown = Some(page);
            self.wake_marks.insert(page, now_ms);
        }
        if entered == Some(AppPage::Settings) {
            match route.settings_section() {
                Some(section) => {
                    self.scroll_to = section.to_string();
                    self.scroll_nonce = self.scroll_nonce.wrapping_add(1);
                }
                None if page_changed => self.scroll_to.clear(),
                None => {}
            }
        }
        Navigation { left, entered, route, intent }
    }
    /// Mouse X1 / Alt+Left equivalents (ignored during onboarding: not onboarded, or the gate still on screen).
    pub fn back(&mut self, onboarded: bool) -> Option<Navigation> {
        if !onboarded || self.onboarding() || self.dialog.open.is_some() {
            return None;
        }
        let route = self.routes.back()?.clone();
        Some(self.transition(route, NavIntent::None, onboarded, true, crate::windowing::mono_ms()))
    }
    pub fn forward(&mut self, onboarded: bool) -> Option<Navigation> {
        if !onboarded || self.onboarding() || self.dialog.open.is_some() {
            return None;
        }
        let route = self.routes.forward()?.clone();
        Some(self.transition(route, NavIntent::None, onboarded, true, crate::windowing::mono_ms()))
    }
    /// Main window focus / visibility changed: the active page to wake (≤ once per 30 s per page since its entry).
    pub fn on_main_window_activity(&mut self, focused: bool, visible: bool, now_ms: f64) -> Option<AppPage> {
        let (was_focused, was_visible) = self.activity;
        self.activity = (focused, visible);
        if !visible {
            return None;
        }
        let woke = (focused && !was_focused) || !was_visible;
        if !woke {
            return None;
        }
        let page = self.shown?;
        if self.wake_marks.get(&page).is_some_and(|m| now_ms - m < WAKE_THROTTLE_MS) {
            return None;
        }
        self.wake_marks.insert(page, now_ms);
        Some(page)
    }
    /// The sidebar struct (nav, mini player, credits line via `convert::credits_data(.., Sidebar, ..)`, hint).
    pub fn build_sidebar(&self, cx: &mut UiCtx) -> SidebarData {
        let settings: &Settings = cx.settings;
        let voice_id = cx.player.session.as_ref().map(|s| s.voice_id.as_str()).unwrap_or(settings.voice_id.as_str());
        let art = cx.art.image(voice_id, ArtSize::Compact);
        let credits = credits_line(cx.credits, settings.has_api_key);
        let hint = read_selection_hint(settings);
        SidebarData {
            nav: nav_items(),
            now: mini_player(&cx.player.state, art),
            credits_visible: credits.visible,
            credits_line: credits.line.into(),
            credits_fraction: credits.fraction as f32,
            credits_low: credits.low,
            add_key_visible: credits.add_key,
            hint_label: hint.label.into(),
            hint_keys: string_model(hint.keys),
            hint_spoken: hint.spoken.into(),
        }
    }

    // ── state for the main window ──

    /// The page on screen.
    pub fn shown_page(&self) -> Option<AppPage> {
        self.shown
    }
    /// `MainWindow.onboarding`.
    pub fn onboarding(&self) -> bool {
        self.shown == Some(AppPage::Onboarding)
    }
    /// `MainWindow.page-title`.
    pub fn page_title(&self) -> &'static str {
        self.shown.map(AppPage::title).unwrap_or("")
    }
    /// `MainWindow.settings-scroll-to` / `settings-scroll-nonce`.
    pub fn settings_scroll(&self) -> (SharedString, i32) {
        (self.scroll_to.as_str().into(), self.scroll_nonce)
    }

    // ── toasts ──

    /// Applies what controllers collected: pushes (in order), then programmatic dismisses (removed at once).
    /// Returns every removal to report (`PushedOut`, `Dismissed`).
    pub fn apply_toast_sink(&mut self, sink: &mut ToastSink) -> Vec<(String, ToastRemoval)> {
        let mut removed = Vec::new();
        for (toast, local) in sink.pushes.drain(..) {
            removed.extend(self.toasts.push(toast, local));
        }
        for id in sink.dismisses.drain(..) {
            removed.extend(self.toasts.dismiss_now(&id));
        }
        removed
    }
    /// The toast timer fired (or any time): timeouts start exits, finished exits are removed and returned.
    pub fn tick_toasts(&mut self, now_ms: f64) -> Vec<(String, ToastRemoval)> {
        self.toasts.tick(now_ms)
    }
    /// (Re)arms the one toast timer for the stack's next deadline; `fire` re-enters `AppUi` (module docs).
    pub fn arm_toast_timer(&mut self, now_ms: f64, fire: impl FnOnce() + 'static) {
        let deadline = self.toasts.next_deadline(now_ms);
        self.toast_timer.arm(deadline, now_ms, fire);
    }
    /// `toast-hover(id, hovered)`.
    pub fn toast_hover(&mut self, id: &str, hovered: bool, now_ms: f64) -> bool {
        self.toasts.set_hover(id, hovered, now_ms)
    }
    /// `toast-dismiss(id)` (close button): starts the exit (ignored while leaving).
    pub fn toast_dismiss(&mut self, id: &str, now_ms: f64) -> bool {
        self.toasts.begin_exit_at(id, ToastRemoval::Dismissed, now_ms)
    }
    /// `toast-action(id)`: what the action does, then the exit starts. `None` when the toast is gone or leaving
    /// (clicks during the exit are ignored).
    pub fn toast_action(&mut self, id: &str, now_ms: f64) -> Option<ToastActionTarget> {
        let e = self.toasts.entries().iter().find(|e| e.toast.id == id && !e.leaving)?;
        let target = match (e.toast.action.as_ref().and_then(|a| a.route.clone()), e.local) {
            (Some(route), _) => ToastActionTarget::Route(route),
            (None, Some(UiToastAction::Page(p))) => ToastActionTarget::Page(p),
            (None, None) => ToastActionTarget::Nothing,
        };
        self.toasts.begin_exit_at(id, ToastRemoval::Action, now_ms);
        Some(target)
    }
    /// `MainWindow.toasts` (newest last).
    pub fn build_toasts(&self) -> ModelRc<ToastData> {
        let v: Vec<ToastData> = self.toasts.entries().iter().map(|e| super::convert::toast_data(&e.toast, e.leaving, e.key)).collect();
        ModelRc::new(VecModel::from(v))
    }

    // ── dialog ──

    /// Applies what controllers asked for: open, busy flag, close (by id).
    pub fn apply_dialog_sink(&mut self, sink: &mut DialogSink) {
        if let Some(spec) = sink.open.take() {
            self.dialog.open = Some(spec);
        }
        if let Some((id, busy)) = sink.busy.take() {
            if let Some(d) = self.dialog.open.as_mut().filter(|d| d.id == id) {
                d.busy = busy;
            }
        }
        if let Some(id) = sink.close.take() {
            self.close_dialog(&id);
        }
    }
    /// Closes the dialog when it is the open one.
    pub fn close_dialog(&mut self, id: &str) {
        if self.dialog.get(id).is_some() {
            self.dialog.open = None;
        }
    }
    /// `dialog-confirm(id)`: the owning page (→ `on_dialog(id, true)`); `None` when not open or busy.
    pub fn dialog_confirm(&self, id: &str) -> Option<AppPage> {
        self.dialog.get(id).filter(|d| !d.busy).map(|d| d.owner)
    }
    /// `dialog-cancel(id)` (Esc, scrim, close glyph, Cancel): the owning page (→ `on_dialog(id, false)`); ignored
    /// while busy. A cancel the owner does not handle closes the dialog ([`ShellController::close_dialog`]).
    pub fn dialog_cancel(&self, id: &str) -> Option<AppPage> {
        self.dialog.get(id).filter(|d| !d.busy).map(|d| d.owner)
    }
    /// `MainWindow.dialog`.
    pub fn dialog_data(&self) -> DialogData {
        match &self.dialog.open {
            Some(d) => DialogData {
                open: true,
                id: d.id.as_str().into(),
                title: d.title.as_str().into(),
                body: d.body.as_str().into(),
                confirm_label: d.confirm_label.as_str().into(),
                cancel_label: d.cancel_label.as_str().into(),
                danger: d.danger,
                busy: d.busy,
            },
            None => DialogData::default(),
        }
    }

    // ── shell callbacks ──

    /// `navigate(PageId)` (sidebar nav, Ctrl+1..5): drops any section; ignored while a dialog is open.
    pub fn nav_clicked(&mut self, cx: &mut UiCtx, page: PageId) {
        if self.dialog.open.is_some() {
            return;
        }
        cx.nav.navigate(AppRoute::page(app_page(page)));
    }
    /// `sidebar-search(text)` (Enter in "Search history"): History with that search, applied at once; the field is
    /// cleared. A blank search just opens History.
    pub fn sidebar_search(&mut self, cx: &mut UiCtx, text: &str) {
        if self.dialog.open.is_some() {
            return;
        }
        let t = vox_core::js::js_trim(text);
        let intent = if t.is_empty() { NavIntent::None } else { NavIntent::Search(t.to_string()) };
        cx.nav.navigate_with(AppRoute::page(AppPage::History), intent);
        cx.fields.sidebar_search_text = Some(String::new());
    }
    /// `mini-action(a)`: toggle (only while playing / paused) and stop; everything else is ignored.
    pub fn mini_action(&mut self, cx: &mut UiCtx, action: PlayerAction) {
        let status = cx.player.state.status;
        match action {
            PlayerAction::Toggle if matches!(status, PlayerStatus::Playing | PlayerStatus::Paused) => {
                cx.core.send(Command::Player(vox_core::player::PlayerCommand::Toggle));
            }
            PlayerAction::Play if status == PlayerStatus::Paused => cx.core.send(Command::Player(vox_core::player::PlayerCommand::Play)),
            PlayerAction::Pause if status == PlayerStatus::Playing => cx.core.send(Command::Player(vox_core::player::PlayerCommand::Pause)),
            PlayerAction::Stop | PlayerAction::Close => cx.core.send(Command::Player(vox_core::player::PlayerCommand::Stop)),
            _ => {}
        }
    }
    /// `mini-open()` (title button / rail "Open player"): Home.
    pub fn mini_open(&mut self, cx: &mut UiCtx) {
        if self.dialog.open.is_none() {
            cx.nav.navigate(AppRoute::page(AppPage::Home));
        }
    }
    /// `credits-open()` (credits line / add-key button): Settings → Account.
    pub fn credits_open(&mut self, cx: &mut UiCtx) {
        if self.dialog.open.is_none() {
            cx.nav.navigate(AppRoute::settings("account"));
        }
    }
    /// `header-send-to-tray()` (D-UM-28): saves the placement first (`state` from
    /// `windowing::main_window::current_state`), then `Command::HideMain`.
    pub fn send_to_tray(&mut self, cx: &mut UiCtx, state: Option<vox_store::window_state::WindowState>) {
        if let Some(state) = state {
            cx.core.send(Command::SaveWindowState(state));
        }
        cx.core.send(Command::HideMain);
    }
    /// `header-toggle-theme()`: `paper` when the resolved theme is dark, else `night` (never `system`, D-UM-14).
    /// A refused write toasts `Couldn’t change the theme: {message}` when its `SettingsSaved` arrives.
    pub fn toggle_theme(&mut self, cx: &mut UiCtx, dark: bool) {
        let req = cx.core.request_id();
        self.theme_req = Some(req);
        let theme = if dark { "paper" } else { "night" };
        cx.core.send(Command::UpdateSettings { req: Some(req), patch: serde_json::json!({ "app": { "theme": theme } }) });
    }
}

impl PageController for ShellController {
    type Data = SidebarData;

    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        match update {
            UiUpdate::Settings(s) => {
                // The onboarding gate: finishing (or resetting) onboarding changes the page for the same route.
                if self.onboarded.is_some_and(|o| o != s.onboarded) {
                    if let Some(route) = self.routes.current().cloned() {
                        cx.nav.navigate(route);
                    }
                }
                true
            }
            UiUpdate::SettingsSaved { req, result } if self.theme_req == Some(*req) => {
                self.theme_req = None;
                if let Err(e) = result {
                    cx.toasts.push(Toast::error(format!("Couldn\u{2019}t change the theme: {}", format::clean_error(e))).with_id("theme"), None);
                }
                false
            }
            UiUpdate::Toast(t) => {
                cx.toasts.push(t.clone(), None);
                false
            }
            UiUpdate::Navigate(route) | UiUpdate::ShowMain(Some(route)) => {
                cx.nav.navigate(route.clone());
                false
            }
            UiUpdate::Player(_) | UiUpdate::Credits(_) | UiUpdate::AppInfo(_) | UiUpdate::Refresh(_) => true,
            _ => false,
        }
    }

    fn build(&self, cx: &mut UiCtx) -> SidebarData {
        self.build_sidebar(cx)
    }
}

/// The page shown for a route given the onboarding state.
pub fn effective_page(route: &AppRoute, onboarded: bool) -> AppPage {
    if !onboarded || route.page == AppPage::Onboarding {
        AppPage::Onboarding
    } else {
        route.page
    }
}

// ───────────────────────────── sidebar pieces (pure) ─────────────────────────────

/// The nav list in `AppPage::NAV` order (Lucide names the sidebar maps: house, history, chart-column, audio-lines,
/// settings).
fn nav_items() -> ModelRc<NavItemData> {
    let v: Vec<NavItemData> = AppPage::NAV
        .iter()
        .map(|&p| NavItemData {
            page: page_id(p),
            label: p.title().into(),
            icon: match p {
                AppPage::Home => "house",
                AppPage::History => "history",
                AppPage::Analytics => "chart-column",
                AppPage::Voices => "audio-lines",
                _ => "settings",
            }
            .into(),
        })
        .collect();
    ModelRc::new(VecModel::from(v))
}

/// The sidebar's credits footer (ui-main.md §2.6).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CreditsLine {
    pub visible: bool,
    pub line: String,
    pub fraction: f64,
    pub low: bool,
    pub add_key: bool,
}

/// `ready` → `39% credits left` + meter (warning below 15 %); else without a key the add-key button (also in demo mode
/// while loading / failed: only `ready` suppresses it); else nothing.
pub(crate) fn credits_line(credits: &CreditsUpdate, has_api_key: bool) -> CreditsLine {
    match &credits.view {
        CreditsView::Ready(_) => {
            // The sidebar surface of the shared credits block (`39%`, the fraction, the low tone below 15 %).
            let c = super::convert::credits_data(credits, super::convert::CreditsSurface::Sidebar, false, 0.0);
            CreditsLine { visible: true, line: format!("{} credits left", c.percent), fraction: f64::from(c.fraction), low: c.low, add_key: false }
        }
        _ => CreditsLine { visible: false, line: String::new(), fraction: 0.0, low: false, add_key: !has_api_key },
    }
}

/// The footer hint (ui-main.md §2.6).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hint {
    pub label: String,
    pub keys: Vec<String>,
    pub spoken: String,
}

/// `Read selection` + keycaps, or `Right-click → Read with Vox` when there is no hotkey.
pub(crate) fn read_selection_hint(settings: &Settings) -> Hint {
    let accel = read_selection_hotkey(settings);
    let keys = vox_core::hotkey::display_labels(accel);
    if keys.is_empty() {
        Hint {
            label: format!("Right-click \u{2192} {READ_WITH_LABEL}"),
            keys: Vec::new(),
            spoken: format!("Right-click, then choose {READ_WITH_LABEL}"),
        }
    } else {
        Hint { label: "Read selection".into(), keys, spoken: format!("Read selection: {}", vox_core::hotkey::spoken(accel)) }
    }
}

/// `{about }{m:ss} left` (spoken; floored, ui-main.md §11.1).
pub(crate) fn time_left_spoken(state: &PlayerState) -> String {
    let clock = format::format_clock((state.duration_sec - state.position_sec).max(0.0));
    if state.duration_is_estimate {
        format!("about {clock} left")
    } else {
        format!("{clock} left")
    }
}

/// The sidebar's now-playing row (ui-main.md §2.7): visible while loading / playing / paused / error.
pub(crate) fn mini_player(state: &PlayerState, art: slint::Image) -> MiniPlayerData {
    let status = state.status;
    let has_duration = state.duration_sec > 0.0;
    let progress = if has_duration { (state.position_sec / state.duration_sec).clamp(0.0, 1.0) } else { 0.0 };
    MiniPlayerData {
        visible: matches!(status, PlayerStatus::Loading | PlayerStatus::Playing | PlayerStatus::Paused | PlayerStatus::Error),
        art,
        status: format::mini_status_label(status).into(),
        title: if state.title.is_empty() { "Untitled selection".into() } else { state.title.as_str().into() },
        phase: phase(status),
        playing: status == PlayerStatus::Playing,
        can_toggle: matches!(status, PlayerStatus::Playing | PlayerStatus::Paused),
        progress: progress as f32,
        buffered: state.buffered_fraction.clamp(0.0, 1.0) as f32,
        time: if has_duration { format::remaining_clock(state.position_sec, state.duration_sec, state.duration_is_estimate).into() } else { SharedString::new() },
        time_accessible: if has_duration { time_left_spoken(state).into() } else { SharedString::new() },
        error: if status == PlayerStatus::Error {
            state.error.as_deref().filter(|e| !e.is_empty()).unwrap_or("Playback stopped.").into()
        } else {
            SharedString::new()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::art::ArtCache;
    use vox_core::app::ToastKind;
    use vox_core::player::PlayerSnapshot;
    use vox_core::voices::AccountInfo;

    fn toast(id: &str) -> Toast {
        Toast::info("hello").with_id(id)
    }

    #[test]
    fn route_history_back_forward() {
        let mut r = RouteHistory::default();
        assert!(r.navigate(AppRoute::page(AppPage::Home)));
        assert!(!r.navigate(AppRoute::page(AppPage::Home)), "identical route without section changes nothing");
        assert!(r.navigate(AppRoute::page(AppPage::History)));
        assert!(r.navigate(AppRoute::settings("account")));
        assert!(r.navigate(AppRoute::settings("account")), "identical route with a section re-announces");
        assert_eq!(r.back(), Some(&AppRoute::page(AppPage::History)));
        assert_eq!(r.back(), Some(&AppRoute::page(AppPage::Home)));
        assert_eq!(r.back(), None);
        assert_eq!(r.forward(), Some(&AppRoute::page(AppPage::History)));
        // A push drops the forward entries.
        assert!(r.navigate(AppRoute::page(AppPage::Voices)));
        assert_eq!(r.forward(), None);
        assert_eq!(r.current(), Some(&AppRoute::page(AppPage::Voices)));
        // An empty section is no section.
        assert!(!r.navigate(AppRoute { page: AppPage::Voices, section: Some(String::new()) }));
    }

    #[test]
    fn route_history_is_capped() {
        let mut r = RouteHistory::default();
        for i in 0..250 {
            r.navigate(AppRoute::settings(&format!("s{i}")));
        }
        assert_eq!(r.stack.len(), MAX_ROUTE_HISTORY);
        assert_eq!(r.current().and_then(|c| c.section.clone()).as_deref(), Some("s249"));
    }

    #[test]
    fn same_id_push_is_not_a_removal() {
        let mut s = ToastStackState::default();
        assert!(s.push(toast("a"), None).is_empty());
        assert!(s.push(toast("b"), None).is_empty());
        let key_a = s.entries()[0].key;
        // Leaving, then replaced: exit cancelled, moved to newest, new key, no removal.
        assert!(s.begin_exit("a", ToastRemoval::Dismissed));
        let removed = s.push(Toast::error("again").with_id("a"), None);
        assert!(removed.is_empty());
        assert_eq!(s.entries().len(), 2);
        let a = &s.entries()[1];
        assert_eq!(a.toast.id, "a");
        assert!(!a.leaving);
        assert_ne!(a.key, key_a);
        assert_eq!(a.remaining_ms, 6_500);
        // The old exit timer firing later must not remove the replacement.
        assert_eq!(s.finish_exit("a"), None);
        assert!(s.contains("a"));
    }

    #[test]
    fn fifth_push_pushes_the_oldest_out() {
        let mut s = ToastStackState::default();
        for id in ["a", "b", "c", "d"] {
            assert!(s.push(toast(id), None).is_empty());
        }
        let removed = s.push(toast("e"), None);
        assert_eq!(removed, vec![("a".to_string(), ToastRemoval::PushedOut)]);
        assert!(!s.contains("a"));
        assert_eq!(s.entries().iter().map(|e| e.toast.id.as_str()).collect::<Vec<_>>(), ["b", "c", "d", "e"]);
    }

    #[test]
    fn exit_keeps_the_toast_present_until_finished() {
        let mut s = ToastStackState::default();
        s.push(toast("a"), Some(UiToastAction::Page(AppPage::History)));
        assert!(s.begin_exit("a", ToastRemoval::Action));
        assert!(!s.begin_exit("a", ToastRemoval::Dismissed), "already leaving");
        assert!(s.contains("a"));
        assert_eq!(s.finish_exit("a"), Some(("a".to_string(), ToastRemoval::Action)));
        assert!(!s.contains("a"));
        // Programmatic dismiss removes at once.
        s.push(toast("b"), None);
        assert_eq!(s.dismiss_now("b"), Some(("b".to_string(), ToastRemoval::Dismissed)));
        assert_eq!(s.dismiss_now("b"), None);
    }

    #[test]
    fn timers_hover_and_exits() {
        let mut s = ToastStackState::default();
        s.push(toast("a"), None); // info: 4000 ms
        assert_eq!(s.next_deadline(1_000.0), Some(5_000.0));
        assert!(s.tick(4_999.0).is_empty());
        // Hover at 4 000: 1 000 left → paused with max(800, 1000).
        assert!(s.set_hover("a", true, 4_000.0));
        assert_eq!(s.entries()[0].remaining_ms, 1_000);
        assert_eq!(s.next_deadline(4_000.0), None);
        assert!(s.tick(60_000.0).is_empty());
        // Resume at 60 000 → times out at 61 000, exit ends at 61 180.
        assert!(s.set_hover("a", false, 60_000.0));
        assert!(s.tick(60_999.0).is_empty());
        assert!(s.tick(61_000.0).is_empty());
        assert!(s.entries()[0].leaving && s.contains("a"));
        assert_eq!(s.next_deadline(61_000.0), Some(61_180.0));
        assert!(!s.set_hover("a", true, 61_050.0), "a leaving toast is not paused");
        assert_eq!(s.tick(61_180.0), vec![("a".to_string(), ToastRemoval::TimedOut)]);
        assert!(s.entries().is_empty());
        // A short remainder resumes with at least 800 ms.
        s.push(toast("b"), None);
        s.tick(0.0);
        assert!(s.set_hover("b", true, 3_900.0));
        assert_eq!(s.entries()[0].remaining_ms, 800);
    }

    #[test]
    fn close_button_exit_with_clock() {
        let mut s = ToastStackState::default();
        s.push(Toast::error("x").with_id("e").with_action("Retry", None), None);
        s.tick(0.0);
        assert!(s.begin_exit_at("e", ToastRemoval::Dismissed, 100.0));
        assert!(s.tick(279.0).is_empty());
        assert_eq!(s.tick(280.0), vec![("e".to_string(), ToastRemoval::Dismissed)]);
    }

    #[test]
    fn toast_actions() {
        let mut shell = ShellController::default();
        shell.toasts.push(Toast::error("No key").with_id("k").with_action("Add key", Some(AppRoute::settings("account"))), None);
        shell.toasts.push(Toast::info("Removed").with_id("history-remove").with_action("Undo", None), Some(UiToastAction::Page(AppPage::History)));
        shell.toasts.push(Toast::info("Just text").with_id("t").with_action("OK", None), None);
        assert_eq!(shell.toast_action("k", 0.0), Some(ToastActionTarget::Route(AppRoute::settings("account"))));
        assert_eq!(shell.toast_action("k", 10.0), None, "clicks during the exit are ignored");
        assert_eq!(shell.toast_action("history-remove", 0.0), Some(ToastActionTarget::Page(AppPage::History)));
        assert_eq!(shell.toast_action("t", 0.0), Some(ToastActionTarget::Nothing));
        assert!(shell.toasts.entries().iter().all(|e| e.leaving));
        let removed = shell.tick_toasts(180.0);
        assert_eq!(removed.len(), 3);
        assert!(removed.iter().all(|(_, why)| *why == ToastRemoval::Action));
    }

    #[test]
    fn toast_sink_pushes_then_dismisses() {
        let mut shell = ShellController::default();
        for id in ["a", "b", "c", "d"] {
            shell.toasts.push(toast(id), None);
        }
        let mut sink = ToastSink::default();
        sink.push(toast("e"), None);
        sink.dismiss("c");
        let removed = shell.apply_toast_sink(&mut sink);
        assert_eq!(removed, vec![("a".to_string(), ToastRemoval::PushedOut), ("c".to_string(), ToastRemoval::Dismissed)]);
        assert!(sink.pushes.is_empty() && sink.dismisses.is_empty());
    }

    #[test]
    fn effective_pages() {
        assert_eq!(effective_page(&AppRoute::page(AppPage::History), true), AppPage::History);
        assert_eq!(effective_page(&AppRoute::settings("account"), false), AppPage::Onboarding);
        assert_eq!(effective_page(&AppRoute::page(AppPage::Onboarding), true), AppPage::Onboarding);
    }

    fn spec(id: &str, owner: AppPage) -> DialogSpec {
        DialogSpec {
            id: id.into(),
            owner,
            title: "Clear all history?".into(),
            body: "b".into(),
            confirm_label: "Clear history".into(),
            cancel_label: "Cancel".into(),
            danger: true,
            busy: false,
        }
    }

    #[test]
    fn navigation_enter_leave_and_dialog() {
        let mut shell = ShellController::default();
        let n = shell.navigate_at(AppRoute::page(AppPage::Home), NavIntent::None, true, 0.0);
        assert_eq!((n.left, n.entered), (None, Some(AppPage::Home)));
        // Same route again: nothing.
        let n = shell.navigate_at(AppRoute::page(AppPage::Home), NavIntent::None, true, 0.0);
        assert_eq!((n.left, n.entered), (None, None));
        let n = shell.navigate_at(AppRoute::page(AppPage::History), NavIntent::None, true, 0.0);
        assert_eq!((n.left, n.entered), (Some(AppPage::Home), Some(AppPage::History)));
        // A search intent on the same page re-enters without leaving.
        let n = shell.navigate_at(AppRoute::page(AppPage::History), NavIntent::Search("x".into()), true, 0.0);
        assert_eq!((n.left, n.entered), (None, Some(AppPage::History)));
        // The History dialog closes when History is left; a Settings dialog would not.
        let mut sink = DialogSink::default();
        sink.open(spec("history-clear", AppPage::History));
        shell.apply_dialog_sink(&mut sink);
        assert!(shell.dialog_data().open);
        assert_eq!(shell.back(true), None, "back is ignored while a dialog is open");
        let n = shell.navigate_at(AppRoute::settings("library"), NavIntent::None, true, 0.0);
        assert_eq!((n.left, n.entered), (Some(AppPage::History), Some(AppPage::Settings)));
        assert!(!shell.dialog_data().open);
        assert_eq!(shell.settings_scroll(), (SharedString::from("library"), 1));
        // Re-announce of the same section scrolls again.
        let n = shell.navigate_at(AppRoute::settings("library"), NavIntent::None, true, 0.0);
        assert_eq!((n.left, n.entered), (None, Some(AppPage::Settings)));
        assert_eq!(shell.settings_scroll().1, 2);
        // Back / forward.
        let n = shell.back(true).expect("back");
        assert_eq!((n.left, n.entered), (Some(AppPage::Settings), Some(AppPage::History)));
        assert_eq!(shell.back(false), None, "ignored during onboarding");
        let n = shell.forward(true).expect("forward");
        assert_eq!(n.entered, Some(AppPage::Settings));
        assert_eq!(shell.page_title(), "Settings");
    }

    #[test]
    fn onboarding_gate() {
        let mut shell = ShellController::default();
        let n = shell.navigate_at(AppRoute::page(AppPage::Home), NavIntent::None, false, 0.0);
        assert_eq!(n.entered, Some(AppPage::Onboarding));
        assert!(shell.onboarding());
        // A deep link while onboarding re-enters onboarding (the key step) without leaving it.
        let n = shell.navigate_at(AppRoute::settings("account"), NavIntent::None, false, 0.0);
        assert_eq!((n.left, n.entered), (None, Some(AppPage::Onboarding)));
        // Finishing onboarding: the same route now shows Settings.
        let n = shell.navigate_at(AppRoute::settings("account"), NavIntent::None, true, 0.0);
        assert_eq!((n.left, n.entered), (Some(AppPage::Onboarding), Some(AppPage::Settings)));
    }

    #[test]
    fn dialog_busy_ignores_cancel() {
        let mut shell = ShellController::default();
        let mut sink = DialogSink::default();
        sink.open(spec("remove-key", AppPage::Settings));
        shell.apply_dialog_sink(&mut sink);
        assert_eq!(shell.dialog_cancel("remove-key"), Some(AppPage::Settings));
        assert_eq!(shell.dialog_cancel("other"), None);
        sink.set_busy("remove-key", true);
        shell.apply_dialog_sink(&mut sink);
        assert!(shell.dialog_data().busy);
        assert_eq!(shell.dialog_cancel("remove-key"), None);
        assert_eq!(shell.dialog_confirm("remove-key"), None);
        sink.close("remove-key");
        shell.apply_dialog_sink(&mut sink);
        assert!(!shell.dialog_data().open);
    }

    #[test]
    fn wake_throttle() {
        let mut shell = ShellController::default();
        shell.navigate_at(AppRoute::page(AppPage::Home), NavIntent::None, true, 0.0);
        // The startup show within 30 s of entering does not count.
        assert_eq!(shell.on_main_window_activity(true, true, 1_000.0), None);
        assert_eq!(shell.on_main_window_activity(false, true, 2_000.0), None, "focus loss is not a wake");
        assert_eq!(shell.on_main_window_activity(true, true, 40_000.0), Some(AppPage::Home));
        assert_eq!(shell.on_main_window_activity(false, true, 41_000.0), None);
        assert_eq!(shell.on_main_window_activity(true, true, 50_000.0), None, "throttled to once per 30 s");
        // Hidden: ignored; shown again later: wake.
        assert_eq!(shell.on_main_window_activity(false, false, 60_000.0), None);
        assert_eq!(shell.on_main_window_activity(false, true, 80_000.0), Some(AppPage::Home));
        // Entering a page resets its mark.
        shell.navigate_at(AppRoute::page(AppPage::Analytics), NavIntent::None, true, 200_000.0);
        assert_eq!(shell.on_main_window_activity(false, false, 201_000.0), None);
        assert_eq!(shell.on_main_window_activity(true, true, 210_000.0), None);
        assert_eq!(shell.on_main_window_activity(false, false, 240_000.0), None);
        assert_eq!(shell.on_main_window_activity(true, true, 240_001.0), Some(AppPage::Analytics));
    }

    #[test]
    fn sidebar_credits_and_hint() {
        let account = AccountInfo { tier: "creator".into(), character_count: 61_234.0, character_limit: 100_000.0, next_reset_unix: None, status: None };
        let ready = CreditsUpdate { view: CreditsView::Ready(account), refreshing: false };
        let c = credits_line(&ready, true);
        assert_eq!((c.visible, c.line.as_str(), c.low, c.add_key), (true, "39% credits left", false, false));
        let loading = CreditsUpdate { view: CreditsView::Loading, refreshing: true };
        assert!(credits_line(&loading, false).add_key);
        assert!(!credits_line(&loading, true).add_key);
        let low = CreditsUpdate {
            view: CreditsView::Ready(AccountInfo { tier: String::new(), character_count: 95_000.0, character_limit: 100_000.0, next_reset_unix: None, status: None }),
            refreshing: false,
        };
        assert!(credits_line(&low, true).low);

        let mut s = Settings::default();
        let h = read_selection_hint(&s);
        assert_eq!(h.label, "Read selection");
        assert_eq!(h.keys, ["Ctrl", "Alt", "R"]);
        assert_eq!(h.spoken, "Read selection: Ctrl plus Alt plus R");
        s.triggers.hotkeys_enabled = false;
        let h = read_selection_hint(&s);
        assert_eq!(h.label, "Right-click \u{2192} Read with Vox");
        assert!(h.keys.is_empty());
    }

    #[test]
    fn mini_player_rows() {
        let mut st = PlayerState { status: PlayerStatus::Playing, title: "Hello".into(), position_sec: 63.4, duration_sec: 263.0, duration_is_estimate: false, buffered_fraction: 0.5, ..PlayerState::idle() };
        let m = mini_player(&st, slint::Image::default());
        assert!(m.visible && m.playing && m.can_toggle);
        assert_eq!(m.status, "Now reading");
        assert_eq!(m.time, "-3:20");
        assert_eq!(m.time_accessible, "3:19 left");
        st.duration_is_estimate = true;
        assert_eq!(mini_player(&st, slint::Image::default()).time_accessible, "about 3:19 left");
        st.status = PlayerStatus::Error;
        st.error = None;
        st.title.clear();
        let m = mini_player(&st, slint::Image::default());
        assert_eq!((m.error.as_str(), m.title.as_str(), m.can_toggle), ("Playback stopped.", "Untitled selection", false));
        assert_eq!(m.status, "Couldn\u{2019}t read");
        st.status = PlayerStatus::Ended;
        assert!(!mini_player(&st, slint::Image::default()).visible);
        st.status = PlayerStatus::Idle;
        st.duration_sec = 0.0;
        let m = mini_player(&st, slint::Image::default());
        assert!(!m.visible && m.time.is_empty());
    }

    /// A `UiCtx` for controller tests.
    struct World {
        core: crate::command::CoreHandle,
        rx: crossbeam_channel::Receiver<Command>,
        settings: std::sync::Arc<Settings>,
        player: PlayerSnapshot,
        credits: CreditsUpdate,
        app_info: std::sync::Arc<vox_core::app::AppInfo>,
        models: std::sync::Arc<Vec<vox_core::voices::TtsModel>>,
        art: ArtCache,
        toasts: ToastSink,
        nav: super::super::NavSink,
        dialogs: DialogSink,
        fields: super::super::FieldWrites,
    }

    impl World {
        fn new() -> Self {
            let (tx, rx) = crossbeam_channel::unbounded();
            World {
                core: crate::command::CoreHandle::new(tx),
                rx,
                settings: std::sync::Arc::new(Settings { onboarded: true, ..Settings::default() }),
                player: PlayerSnapshot::default(),
                credits: CreditsUpdate { view: CreditsView::Loading, refreshing: false },
                app_info: std::sync::Arc::default(),
                models: std::sync::Arc::default(),
                art: ArtCache::default(),
                toasts: ToastSink::default(),
                nav: super::super::NavSink::default(),
                dialogs: DialogSink::default(),
                fields: super::super::FieldWrites::default(),
            }
        }
        fn cx(&mut self) -> UiCtx<'_> {
            UiCtx {
                core: &self.core,
                settings: &self.settings,
                player: &self.player,
                credits: &self.credits,
                app_info: &self.app_info,
                models: &self.models,
                now_ms: 0.0,
                demo: false,
                reduce_motion: false,
                art: &mut self.art,
                toasts: &mut self.toasts,
                nav: &mut self.nav,
                dialogs: &mut self.dialogs,
                fields: &mut self.fields,
            }
        }
    }

    #[test]
    fn on_update_fan_out() {
        let mut w = World::new();
        let mut shell = ShellController::default();
        shell.navigate_at(AppRoute::page(AppPage::History), NavIntent::None, true, 0.0);
        // Core toasts go to the stack; Navigate / ShowMain(route) navigate.
        assert!(!shell.on_update(&mut w.cx(), &UiUpdate::Toast(toast("core"))));
        assert_eq!(w.toasts.pushes.len(), 1);
        shell.on_update(&mut w.cx(), &UiUpdate::Navigate(AppRoute::settings("account")));
        assert_eq!(w.nav.target.take().map(|(r, _)| r), Some(AppRoute::settings("account")));
        shell.on_update(&mut w.cx(), &UiUpdate::ShowMain(None));
        assert!(w.nav.target.is_none());
        // The onboarding gate: flipping `onboarded` re-navigates the current route.
        let not_onboarded = std::sync::Arc::new(Settings { onboarded: false, ..Settings::default() });
        assert!(shell.on_update(&mut w.cx(), &UiUpdate::Settings(not_onboarded)));
        assert_eq!(w.nav.target.take().map(|(r, _)| r), Some(AppRoute::page(AppPage::History)));
        let same = std::sync::Arc::new(Settings { onboarded: true, ..Settings::default() });
        shell.on_update(&mut w.cx(), &UiUpdate::Settings(same));
        assert!(w.nav.target.is_none(), "unchanged gate: no navigation");
        assert!(shell.on_update(&mut w.cx(), &UiUpdate::Player(PlayerSnapshot::default())));
    }

    #[test]
    fn header_and_sidebar_callbacks() {
        let mut w = World::new();
        let mut shell = ShellController::default();
        shell.navigate_at(AppRoute::page(AppPage::Home), NavIntent::None, true, 0.0);
        // Theme toggle writes paper/night; a refused write toasts.
        shell.toggle_theme(&mut w.cx(), true);
        let req = match w.rx.try_recv() {
            Ok(Command::UpdateSettings { req: Some(req), patch }) => {
                assert_eq!(patch, serde_json::json!({ "app": { "theme": "paper" } }));
                req
            }
            _ => panic!("expected UpdateSettings"),
        };
        shell.on_update(&mut w.cx(), &UiUpdate::SettingsSaved { req, result: Err("SettingsError: disk full".into()) });
        let t = w.toasts.pushes.pop().map(|(t, _)| t).expect("toast");
        assert_eq!(t.message, "Couldn\u{2019}t change the theme: disk full");
        // Send to tray.
        shell.send_to_tray(&mut w.cx(), None);
        assert!(matches!(w.rx.try_recv(), Ok(Command::HideMain)));
        // Sidebar search: History with the trimmed search, field cleared.
        shell.sidebar_search(&mut w.cx(), "  invoice ");
        assert_eq!(w.nav.target.take(), Some((AppRoute::page(AppPage::History), NavIntent::Search("invoice".into()))));
        assert_eq!(w.fields.sidebar_search_text.take().as_deref(), Some(""));
        // Mini player: toggle only while playing / paused; stop always.
        shell.mini_action(&mut w.cx(), PlayerAction::Toggle);
        assert!(w.rx.try_recv().is_err());
        w.player.state.status = PlayerStatus::Paused;
        shell.mini_action(&mut w.cx(), PlayerAction::Toggle);
        assert!(matches!(w.rx.try_recv(), Ok(Command::Player(vox_core::player::PlayerCommand::Toggle))));
        shell.mini_action(&mut w.cx(), PlayerAction::Stop);
        assert!(matches!(w.rx.try_recv(), Ok(Command::Player(vox_core::player::PlayerCommand::Stop))));
        // While a dialog is open the nav, search and credits links are inert.
        let mut sink = DialogSink::default();
        sink.open(spec("history-clear", AppPage::History));
        shell.apply_dialog_sink(&mut sink);
        shell.nav_clicked(&mut w.cx(), PageId::Voices);
        shell.sidebar_search(&mut w.cx(), "x");
        shell.credits_open(&mut w.cx());
        shell.mini_open(&mut w.cx());
        assert!(w.nav.target.is_none());
        shell.close_dialog("history-clear");
        shell.credits_open(&mut w.cx());
        assert_eq!(w.nav.target.take().map(|(r, _)| r), Some(AppRoute::settings("account")));
        shell.nav_clicked(&mut w.cx(), PageId::Voices);
        assert_eq!(w.nav.target.take().map(|(r, _)| r), Some(AppRoute::page(AppPage::Voices)));
    }

    #[test]
    fn back_never_lands_on_the_onboarding_gate() {
        let mut shell = ShellController::default();
        // Startup while not onboarded: the gate route is the first entry.
        let nav = shell.navigate_at(AppRoute::page(AppPage::Onboarding), NavIntent::None, false, 0.0);
        assert_eq!(nav.entered, Some(AppPage::Onboarding));
        // A toast's `settings/account` during onboarding still shows the gate; mouse back/forward are ignored there.
        shell.navigate_at(AppRoute::settings("account"), NavIntent::None, false, 0.0);
        assert!(shell.onboarding());
        assert!(shell.back(true).is_none(), "the gate is on screen: no back even if settings already say onboarded");
        // Finish: Home with `onboarded` true.
        let nav = shell.navigate_at(AppRoute::page(AppPage::Home), NavIntent::None, true, 0.0);
        assert_eq!(nav.entered, Some(AppPage::Home));
        // Back goes to the real page pushed during onboarding, never to the gate.
        let nav = shell.back(true).expect("settings/account is a real page");
        assert_eq!(nav.entered, Some(AppPage::Settings));
        assert!(shell.back(true).is_none(), "only the gate is left behind");
        assert_eq!(shell.shown_page(), Some(AppPage::Settings));
        assert_eq!(shell.forward(true).and_then(|n| n.entered), Some(AppPage::Home));

        // The pure stack skips gate entries in both directions.
        let mut r = RouteHistory::default();
        r.navigate(AppRoute::page(AppPage::Home));
        r.navigate(AppRoute::page(AppPage::Onboarding));
        r.navigate(AppRoute::page(AppPage::History));
        assert_eq!(r.back(), Some(&AppRoute::page(AppPage::Home)));
        assert_eq!(r.forward(), Some(&AppRoute::page(AppPage::History)));
        r.navigate(AppRoute::page(AppPage::Onboarding));
        assert_eq!(r.forward(), None);
        assert_eq!(r.current(), Some(&AppRoute::page(AppPage::Onboarding)), "a failed step keeps the index");
    }

    #[test]
    fn page_ids_round_trip() {
        for p in AppPage::ALL {
            assert_eq!(app_page(page_id(p)), p);
        }
        assert_eq!(phase(PlayerStatus::Ended), PlayerPhase::Ended);
        assert_eq!(join_dot(["George", "", "Outlook"]), "George \u{b7} Outlook");
        assert_eq!(ToastKind::Error.duration_ms(), 6_500);
    }
}
