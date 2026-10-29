//! The UI thread (docs/ARCHITECTURE.md §3.4, §7). Holds the three Slint windows, applies [`UiUpdate`]s and wires
//! every Slint callback to a controller method. Page **controllers** keep page-local state (filters, expanded rows,
//! undo batches, request ids, debounces with `slint::Timer`) and build the Slint models from domain data with
//! vox-core's format helpers — the `.slint` files never format. No business logic and no blocking calls on this
//! thread (one exception: the synchronous `window-state.json` save when `UiUpdate::Quit` arrives).
//!
//! | Module | Builds / owns |
//! |---|---|
//! | [`generated`] | `slint::include_modules!()` — `MainWindow`, `WidgetWindow`, `ChipWindow`, `Theme`, `Motion`, model structs |
//! | [`art`] | the art cache (voice / app / backdrop images rendered by vox-art at 2× on a worker thread) |
//! | [`convert`] | small converters: options → `[ChoiceItem]`, keycaps, `CreditsData` per surface, `ToastData`, images |
//! | [`shell`] | `ShellController`: routes + back/forward, page enter/leave/wake, sidebar & mini player, the toast stack (timers, hover pause, exits, removal events, UI-local actions), dialog slot |
//! | [`home`] | `HomeData` (ui-main.md §3) |
//! | [`history`] | `HistoryData` (ui-main.md §4: loading cycle, reconcile, load more, delete-with-undo, clear dialog) |
//! | [`analytics`] | `AnalyticsData` (ui-main.md §5: range, bucketing via `vox_core::charts` at the reported plot width) |
//! | [`voices`] | `VoicesData` (ui-settings-widget.md §4) |
//! | [`settings`] | `SettingsData` (ui-settings-widget.md §2: optimistic overlay, retention guard, port field, copy chips) |
//! | [`onboarding`] | `OnboardingData` (ui-settings-widget.md §5) |
//! | [`widget`] | `WidgetData` + the widget controller (toast slot, auto-hide 4 s / hold retry 1.5 s, hover holds, close) |
//! | [`chip`] | `ChipData` + show/hide sequencing with the exit animation |
//! | [`hotkey_recorder`] | the Settings recorder fed by winit key events (`vox_core::hotkey::record_key`) |
//! | [`tray_view`] | the tray's icon state, tooltip and menu key from settings + player state |
//! | [`stable`] | stable list identity for every struct pushed into Slint (rows are updated in place, never re-created) |
//!
//! **Controller interface** (ARCHITECTURE.md §7.3; final): every page controller implements [`PageController`]
//! and gets a [`UiCtx`] — the shared app state (settings, player snapshot, credits, app info, models, `now`) plus
//! sinks for toasts, navigation, the dialog slot and the main window's in-out text fields. [`AppUi::apply`] first
//! updates the shared state from an update, then fans it out to the shell and the **active** page controller
//! (inactive pages get only `Refresh` and the answers to their own requests), then applies what the sinks collected
//! (toasts → `ShellController::toasts`, navigation → `ShellController::navigate` + `on_leave`/`on_enter`, dialog
//! changes, field writes) and pushes the rebuilt structs. Slint callbacks call the controller methods the same way.
//! Every push goes through [`stable::stabilize`]: the lists of the rebuilt struct are synced **in place** into the
//! lists Slint already shows (a new list identity would make Slint drop and re-create every row: lost clicks, focus
//! and hover while the 30 Hz poll rebuilds the sidebar and Home), and a struct that did not change is not set at all.
//!
//! Fan-out details fixed here: answers carry request ids the UI cannot attribute, so **every** request answer
//! (`SettingsSaved`, `HistoryPage`, `Home`, `Analytics`, `ReadStarted`, `ApiKey`, `Done`, `CacheStats`, `Exported`,
//! `ExpiringCount`, `FavoriteToggled`) and the catalog answers (`Voices`, `Models`, `Preview`) go to every page
//! controller, which drop ids they do not own; `Refresh` and `HotkeysResumed` go to every page too. Only the active
//! page is rebuilt after an update; a page is rebuilt when it is entered. The widget controller sees every update
//! (`ui::widget` integration contract); the chip controller is driven per `ui::chip`'s contract.
//!
//! **Startup** (ARCHITECTURE.md §3.6 guarantee 5): [`AppUi::create`] receives a [`UiInit`] built by `app::run` from the
//! stores, so Theme, Brand, Motion, the onboarding gate and every controller are set **before** the first show — the
//! wrong theme or empty skeletons never flash. The winit backend creates every window's native HWND (hidden) as soon as
//! the event loop runs; on its first turn `AppUi` (a) applies the main window's caption colour, saved placement, icon,
//! close handler and native event dispatcher, (b) installs `vox_win::window::install_overlay_subclass` on both overlays,
//! pre-shows them off-screen and hides them again, then sets `overlays_ready` (`ui::chip` ignores `ChipShow` and reports
//! no bounds until then), (c) shows the main window when `init.show_main`, sends the first `WindowEvent::MainState`, and
//! only then (d) attaches the `UiSink` — so no queued update (a startup toast's `Widget(Show)`) can reach an overlay that
//! is not styled yet — and creates the tray (`app::run` creates none).
//!
//! **Native events** (one dispatcher per window, `slint::winit_030::WinitWindowAccessor::on_winit_window_event`; the
//! main window's is registered by `windowing::main_window::on_created`): the hotkey recorder first (may return
//! `PreventDefault` while recording), then geometry tracking, then mouse X1/X2 → `ShellController::back/forward`
//! (ignored during onboarding). An event that arrives while the app is busy (re-entrant: e.g. `Focused` sent
//! synchronously by the `SetForegroundWindow` of `main_window::show`) is replayed on the next turn instead of being
//! dropped (keyboard events excepted). The main window's focus reported to the core is always read from the window
//! (`main_window::has_focus`), never assumed. The overlays' dispatchers (`style_native_windows`) handle
//! `ScaleFactorChanged` (a visible widget is re-placed like `WidgetIntent::Reposition`, a visible chip is hidden) and,
//! for the widget, `RedrawRequested` (its first-frame reveal, `windowing::reveal`).
//! Close requests: `window.on_close_requested` on all three windows always returns `KeepWindowShown`; main →
//! close-to-tray on the UI thread (save state + hide) or `Command::Quit`; widget/chip → `Command::Window(
//! WindowEvent::CloseRequested(kind))` (through the overlay subclass); an outside `WM_CLOSE` on winit's visible event
//! target window (`taskkill` without `/F`, installers) → `Command::System(SystemEvent::CloseRequested)` (the same
//! subclass on that window; `windowing` docs "Outside WM_CLOSE").
//!
//! **Quit**: before the UI sends `Command::Quit` it calls `history.flush(cx)` (commits a pending undo batch, so its
//! `RemoveReads` precede `Quit`). On `UiUpdate::Quit` it saves `window-state.json` synchronously
//! (`vox_store::window_state::save_window_state(&init.profile, ..)`), hides the windows, destroys the tray, arms the
//! 4 s hard exit (exit code 0, or the shots harness's code) and quits the event loop.
//!
//! **Re-entrancy**: the `AppUi` lives in a thread-local `RefCell`. Every Slint callback, timer and native event goes
//! through [`with_app`], which runs at once when the app is free and otherwise defers to the next event-loop turn;
//! updates from the `UiSink` are queued in order and drained the same way, so nothing is applied out of order. Every
//! entry point runs under `catch_unwind` (`ui callback panicked`).

pub mod analytics;
pub mod art;
pub mod chip;
pub mod convert;
pub mod generated;
pub mod history;
pub mod home;
pub mod hotkey_recorder;
pub mod onboarding;
pub mod settings;
pub mod shell;
pub mod stable;
pub mod tray_view;
pub mod voices;
pub mod widget;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent as WinitEvent};
use slint::winit_030::EventResult;
use slint::{ComponentHandle, SharedString};
use vox_core::analytics::AnalyticsRange;
use vox_core::app::{AppInfo, AppPage, AppRoute, Toast};
use vox_core::constants::{APP_NAME, HOVER_POLL_MS, READ_WITH_LABEL, UI_FRAME_MS};
use vox_core::player::{PlayerSnapshot, PlayerStatus};
use vox_core::settings::Settings;
use vox_core::voices::TtsModel;
use vox_store::Profile;
use vox_win::window::PhysRect;

use crate::command::{Command, CoreHandle, WindowEvent, WindowKind};
use crate::update::{CreditsUpdate, CreditsView, UiSink, UiUpdate};
use crate::windowing::{self, main_window, widget_window};

pub use generated::{ChipWindow, MainWindow, WidgetWindow};
use generated::{Brand, Motion, PlayerAction, Theme};

/// Everything the UI needs before the first frame (built by `app::run` from the stores; ARCHITECTURE.md §3.6 #5).
#[derive(Debug, Clone)]
pub struct UiInit {
    pub settings: Arc<Settings>,
    /// The resolved main-window theme (`resolve_theme(settings.app.theme, os_dark)`); widget and chip are always dark.
    pub dark: bool,
    /// OS reduce motion (`vox_win::window::reduce_motion()`).
    pub reduce_motion: bool,
    pub app_info: Arc<AppInfo>,
    /// The startup route (`app::startup_route`).
    pub route: AppRoute,
    /// The startup visibility decision (`app::should_show_main_at_start`).
    pub show_main: bool,
    /// The main window's normal rect and maximized flag (`windowing::main_window::initial_placement`).
    pub main_placement: (PhysRect, bool),
    /// The saved Analytics range (`load_ui_state(..).analytics_range`; 30d when absent — the Electron localStorage
    /// value is not migrated).
    pub analytics_range: AnalyticsRange,
    /// For the quit-time `window-state.json` save.
    pub profile: Profile,
    pub demo: bool,
}

/// A UI-local toast action: what the shell does when a toast without a route is actioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiToastAction {
    /// Forward to that page's controller (`PageController::on_toast_action`); dropped when the page is not active.
    Page(AppPage),
}

/// Toasts raised by controllers during one call; the shell pushes them afterwards.
#[derive(Debug, Default)]
pub struct ToastSink {
    pub pushes: Vec<(Toast, Option<UiToastAction>)>,
    pub dismisses: Vec<String>,
}

impl ToastSink {
    /// Push (or replace by id) a toast in the app stack; `local` = a UI-local action (Undo, Retry).
    pub fn push(&mut self, toast: Toast, local: Option<UiToastAction>) {
        self.pushes.push((toast, local));
    }
    /// Programmatic dismiss: the toast is absent at once (no exit animation; ui-main.md §1.7).
    pub fn dismiss(&mut self, id: &str) {
        self.dismisses.push(id.to_string());
    }
}

/// How a navigation was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NavIntent {
    #[default]
    None,
    /// The sidebar search submitted this text: History applies it at once (no 220 ms debounce).
    Search(String),
}

/// A navigation requested by a controller during one call.
#[derive(Debug, Default)]
pub struct NavSink {
    pub target: Option<(AppRoute, NavIntent)>,
}

impl NavSink {
    pub fn navigate(&mut self, route: AppRoute) {
        self.target = Some((route, NavIntent::None));
    }
    pub fn navigate_with(&mut self, route: AppRoute, intent: NavIntent) {
        self.target = Some((route, intent));
    }
}

/// The one modal dialog (ids: `history-clear` → History; `remove-key`, `retention`, `library-clear-history` →
/// Settings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogSpec {
    pub id: String,
    pub owner: AppPage,
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub cancel_label: String,
    pub danger: bool,
    pub busy: bool,
}

/// Dialog changes requested by a controller during one call.
#[derive(Debug, Default)]
pub struct DialogSink {
    pub open: Option<DialogSpec>,
    pub busy: Option<(String, bool)>,
    pub close: Option<String>,
}

impl DialogSink {
    pub fn open(&mut self, spec: DialogSpec) {
        self.open = Some(spec);
    }
    pub fn set_busy(&mut self, id: &str, busy: bool) {
        self.busy = Some((id.to_string(), busy));
    }
    pub fn close(&mut self, id: &str) {
        self.close = Some(id.to_string());
    }
}

/// Writes to the main window's in-out text fields (the field keeps its own text while the user types; Rust writes it
/// only through these: ARCHITECTURE.md §7.1 "Text fields").
#[derive(Debug, Default)]
pub struct FieldWrites {
    pub settings_port_draft: Option<String>,
    pub settings_exclude_draft: Option<String>,
    pub voices_search_text: Option<String>,
    pub history_search_text: Option<String>,
    pub sidebar_search_text: Option<String>,
    pub onboarding_key_draft: Option<String>,
    pub settings_key_draft: Option<String>,
}

/// The shared state and sinks every controller call receives.
pub struct UiCtx<'a> {
    pub core: &'a CoreHandle,
    pub settings: &'a Arc<Settings>,
    /// The last reading-player snapshot (discrete reports; the 30 Hz poll updates it while active).
    pub player: &'a PlayerSnapshot,
    pub credits: &'a CreditsUpdate,
    pub app_info: &'a Arc<AppInfo>,
    /// The last `Ok` model list (kept when the key goes away).
    pub models: &'a Arc<Vec<TtsModel>>,
    pub now_ms: f64,
    pub demo: bool,
    /// OS "show animations" off (the `.slint` files read the `Motion` global; this is for Rust-side timing).
    #[allow(dead_code)] // part of the final controller interface (ARCHITECTURE.md §7.3); no page reads it yet
    pub reduce_motion: bool,
    pub art: &'a mut art::ArtCache,
    pub toasts: &'a mut ToastSink,
    pub nav: &'a mut NavSink,
    pub dialogs: &'a mut DialogSink,
    pub fields: &'a mut FieldWrites,
}

/// The page controller contract (ARCHITECTURE.md §7.3). Default methods do nothing.
#[allow(unused_variables)]
pub trait PageController {
    type Data;
    /// The page became active (route changed to it, or re-announced with a section/intent). Pages are re-created on
    /// entry in the spec: reset page-local UI state here (Home word count; History filters, expanded rows; …).
    fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent) {}
    /// The page is being left: History commits its undo batch and dismisses `history-remove`; Home and History bump
    /// their request sequences (in-flight answers are dropped).
    fn on_leave(&mut self, cx: &mut UiCtx) {}
    /// An update (after the shared state was updated); returns whether the page must be rebuilt.
    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        false
    }
    /// The 60 s "now" tick (relative times, greetings).
    fn on_tick(&mut self, cx: &mut UiCtx) {}
    /// The main window woke (focused / became visible), at most once per 30 s per page since its mount: Home and
    /// Analytics re-query (freshness key).
    fn on_wake(&mut self, cx: &mut UiCtx) {}
    /// A toast left the stack (History: commit the undo batch when no `history-remove` is present any more).
    fn on_toast_removed(&mut self, cx: &mut UiCtx, id: &str, why: shell::ToastRemoval) {}
    /// A UI-local toast action of this page (`Undo`, `Retry`); true when handled.
    fn on_toast_action(&mut self, cx: &mut UiCtx, id: &str) -> bool {
        false
    }
    /// The dialog this page owns was confirmed / cancelled; true when handled.
    fn on_dialog(&mut self, cx: &mut UiCtx, id: &str, confirmed: bool) -> bool {
        false
    }
    /// Builds the page struct from the controller state and the shared state.
    fn build(&self, cx: &mut UiCtx) -> Self::Data;
}

/// The sinks behind every [`UiCtx`].
#[derive(Debug, Default)]
struct Sinks {
    toasts: ToastSink,
    nav: NavSink,
    dialogs: DialogSink,
    fields: FieldWrites,
}

/// The pages in fan-out order.
const PAGES: [AppPage; 6] = [AppPage::Home, AppPage::History, AppPage::Analytics, AppPage::Voices, AppPage::Settings, AppPage::Onboarding];
/// How many times one flush re-processes sinks that produced more work (a navigation whose `on_enter` navigates…).
const MAX_SINK_ROUNDS: usize = 8;
/// The 60 s "now" tick.
const TICK_MS: u64 = 60_000;
/// Main-window geometry save debounce (ARCHITECTURE.md §3.7).
const GEOMETRY_SAVE_MS: u64 = 1_000;
/// Backdrop re-render debounce after a resize.
const BACKDROP_DEBOUNCE_MS: u64 = 250;
/// Drag-end poll (shell.md §6.3.5).
const DRAG_POLL_MS: u64 = 50;
/// The hard exit armed by `UiUpdate::Quit`.
const HARD_EXIT_MS: u64 = 4_000;
/// The main window's small icon (title bar) and big icon (taskbar, Alt+Tab; = `assets/vox-256.png`), D-SH-20.
const WINDOW_ICON_PX: u32 = 48;
const TASKBAR_ICON_PX: u32 = 256;
/// Startup retries while the native windows do not exist yet (20 ms apart).
const NATIVE_STARTUP_TRIES: u32 = 100;

thread_local! {
    static APP: RefCell<Option<AppUi>> = const { RefCell::new(None) };
    /// Updates waiting to be applied in order (the app was busy when they arrived).
    static PENDING: RefCell<VecDeque<UiUpdate>> = const { RefCell::new(VecDeque::new()) };
}

// ───────────────────────────── entry points (UI thread) ─────────────────────────────

/// Runs `f`, catching a panic so it never unwinds into Slint or a window procedure (ARCHITECTURE.md §3.4).
pub(crate) fn guard<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    shell::guarded(what, f)
}

/// Runs `f` with the app: at once when it is free, else on the next event-loop turn (module docs "Re-entrancy").
pub(crate) fn with_app(what: &'static str, f: impl FnOnce(&mut AppUi) + 'static) {
    let mut f = Some(f);
    let ran = APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => {
            if let (Some(app), Some(f)) = (slot.as_mut(), f.take()) {
                guard(what, || f(app));
            }
            true
        }
        Err(_) => false,
    });
    if !ran {
        if let Some(f) = f {
            slint::Timer::single_shot(Duration::ZERO, move || with_app(what, f));
        }
    }
}

/// Runs `f` with the app on the next event-loop turn (never inside the current call stack).
pub(crate) fn with_app_later(f: impl FnOnce(&mut AppUi) + 'static) {
    slint::Timer::single_shot(Duration::ZERO, move || with_app("deferred", f));
}

/// Runs `f` with the app now and returns its result; `None` when the app does not exist or is busy.
pub(crate) fn with_app_now<R>(f: impl FnOnce(&mut AppUi) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut slot = cell.try_borrow_mut().ok()?;
        let app = slot.as_mut()?;
        guard("ui call", || f(app))
    })
}

/// The main window's native event dispatcher (registered once by `windowing::main_window::on_created`). While the app
/// is busy the event is replayed on the next turn (module docs "Native events").
pub fn main_window_event(event: &WinitEvent) -> EventResult {
    let consumed = APP.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            if replays_when_busy(event) {
                let event = event.clone();
                with_app_later(move |app| {
                    let _ = app.on_main_event(&event);
                });
            }
            return false;
        };
        let Some(app) = slot.as_mut() else { return false };
        guard("main window event", || app.on_main_event(event)).unwrap_or(false)
    });
    if consumed {
        EventResult::PreventDefault
    } else {
        EventResult::Propagate
    }
}

/// Main-window events worth replaying when they arrive while the app is busy (keyboard input is not: the recorder
/// can only consume it synchronously).
fn replays_when_busy(event: &WinitEvent) -> bool {
    matches!(
        event,
        WinitEvent::Moved(_)
            | WinitEvent::Resized(_)
            | WinitEvent::ScaleFactorChanged { .. }
            | WinitEvent::Focused(_)
            | WinitEvent::Occluded(_)
            | WinitEvent::MouseInput { .. }
    )
}

/// An overlay's native event dispatcher (`style_native_windows`): `ScaleFactorChanged` (module docs) and, for the
/// widget, `RedrawRequested` for the first-frame reveal (`windowing::reveal`). Never consumes an event.
fn overlay_window_event(kind: WindowKind, window: &slint::Window, event: &WinitEvent) -> EventResult {
    match event {
        WinitEvent::ScaleFactorChanged { .. } => with_app_later(move |app| app.on_overlay_scale_changed(kind)),
        WinitEvent::RedrawRequested if kind == WindowKind::Widget => {
            let _ = guard("widget reveal", || windowing::reveal::redraw_requested(window));
        }
        _ => {}
    }
    EventResult::Propagate
}

/// The art worker finished renders (posted by `ui::art`).
pub fn art_ready() {
    with_app("art ready", |app| app.on_art_ready());
}

/// Tray activation / `Open Vox` / a page item: navigate (if a route), then show, raise and focus the main window —
/// synchronously, while the tray click's foreground permission is valid (D-SH-18).
pub fn tray_open(route: Option<AppRoute>) {
    with_app("tray open", move |app| app.show_main_from_tray(route));
}

/// Tray `Quit Vox`: commit a pending History undo batch, then `Command::Quit`.
pub fn tray_quit() {
    with_app("tray quit", |app| app.quit_from_ui());
}

// ───────────────────────────── the app ─────────────────────────────

/// Builds a [`UiCtx`] from disjoint `AppUi` fields (so a controller field can be borrowed beside it).
macro_rules! cx {
    ($s:ident) => {
        UiCtx {
            core: &$s.core,
            settings: &$s.settings,
            player: &$s.snapshot,
            credits: &$s.credits,
            app_info: &$s.app_info,
            models: &$s.models,
            now_ms: windowing::now_ms(),
            demo: $s.init.demo,
            reduce_motion: $s.reduce_motion,
            art: &mut $s.art,
            toasts: &mut $s.sinks.toasts,
            nav: &mut $s.sinks.nav,
            dialogs: &mut $s.sinks.dialogs,
            fields: &mut $s.sinks.fields,
        }
    };
}

/// Runs `$body` with the controller of `$page` bound to `$c` and a fresh context bound to `$cx`.
macro_rules! on_page {
    ($s:ident, $page:expr, |$c:ident, $cx:ident| $body:expr) => {
        match $page {
            AppPage::Home => {
                let mut $cx = cx!($s);
                let $c = &mut $s.home;
                $body
            }
            AppPage::History => {
                let mut $cx = cx!($s);
                let $c = &mut $s.history;
                $body
            }
            AppPage::Analytics => {
                let mut $cx = cx!($s);
                let $c = &mut $s.analytics;
                $body
            }
            AppPage::Voices => {
                let mut $cx = cx!($s);
                let $c = &mut $s.voices;
                $body
            }
            AppPage::Settings => {
                let mut $cx = cx!($s);
                let $c = &mut $s.settings_page;
                $body
            }
            AppPage::Onboarding => {
                let mut $cx = cx!($s);
                let $c = &mut $s.onboarding;
                $body
            }
        }
    };
}

/// A callback helper per controller: runs `f(controller, cx)`, then flushes the sinks and rebuilds the page.
macro_rules! page_callback {
    ($name:ident, $field:ident, $ty:ty, $page:expr) => {
        fn $name(&mut self, f: impl FnOnce(&mut $ty, &mut UiCtx)) {
            {
                let mut cx = cx!(self);
                f(&mut self.$field, &mut cx);
            }
            self.after_page_callback($page);
        }
    };
}

/// Everything on the UI thread (kept in a thread-local; `UiSink` drains into [`AppUi::apply`]).
pub struct AppUi {
    pub main: MainWindow,
    pub widget: WidgetWindow,
    pub chip: ChipWindow,
    pub core: CoreHandle,
    /// The reading player: polled with `snapshot()` at ~30 Hz while a read is active (ARCHITECTURE.md §3.3).
    pub player: vox_audio::Player,
    /// Chip presses go straight to the selection service (`chip_activated` / `chip_dismissed`, non-blocking) and
    /// the chip's real bounds are reported with `set_chip_bounds`.
    pub selection: vox_win::selection::SelectionService,
    pub init: UiInit,
    // ── shared state ──
    pub settings: Arc<Settings>,
    pub snapshot: PlayerSnapshot,
    pub credits: CreditsUpdate,
    pub app_info: Arc<AppInfo>,
    pub models: Arc<Vec<TtsModel>>,
    pub dark: bool,
    pub reduce_motion: bool,
    pub art: art::ArtCache,
    /// Both overlays exist, are styled and subclassed (chip shows are ignored until then).
    pub overlays_ready: bool,
    // ── controllers ──
    pub shell: shell::ShellController,
    pub home: home::HomeController,
    pub history: history::HistoryController,
    pub analytics: analytics::AnalyticsController,
    pub voices: voices::VoicesController,
    pub settings_page: settings::SettingsController,
    pub onboarding: onboarding::OnboardingController,
    pub widget_ctl: widget::WidgetController,
    pub chip_ctl: chip::ChipController,
    pub recorder: hotkey_recorder::Recorder,
    pub widget_window: crate::windowing::widget_window::WidgetWindowState,
    pub tray: Option<crate::tray::Tray>,
    // ── private ──
    sinks: Sinks,
    sink: UiSink,
    /// The last `(visible, focused)` reported to the core (`None` before the first `MainState`).
    main_state: Option<(bool, bool)>,
    /// winit's last `Focused` for the main window.
    main_focused: bool,
    poll_timer: slint::Timer,
    poll_key: Option<PollKey>,
    hover_timer: slint::Timer,
    drag_timer: slint::Timer,
    geometry_timer: slint::Timer,
    backdrop_timer: slint::Timer,
    tick_timer: slint::Timer,
    quitting: bool,
}

impl AppUi {
    /// Creates the three windows (hidden), applies `init` (Brand, `Theme.dark` on the main window and `true` on the
    /// widget and chip, `Motion.reduce` on all three, the onboarding gate, every controller seeded from
    /// `init.settings`), wires every callback and stores `self` in the thread-local. On the event loop's first turn
    /// (module docs "Startup") it styles the native windows, pre-creates and subclasses the overlays, shows the main
    /// window when `init.show_main` (with its placement and caption colour applied first), sends the first
    /// `WindowEvent::MainState`, attaches `sink` and creates the tray.
    pub fn create(
        init: UiInit,
        core: CoreHandle,
        player: vox_audio::Player,
        selection: vox_win::selection::SelectionService,
        sink: &crate::update::UiSink,
    ) -> anyhow::Result<()> {
        let main = windowing::creating(WindowKind::Main, MainWindow::new).map_err(|e| anyhow::anyhow!("could not create the main window: {e}"))?;
        let widget = windowing::creating(WindowKind::Widget, WidgetWindow::new).map_err(|e| anyhow::anyhow!("could not create the widget window: {e}"))?;
        let chip = windowing::creating(WindowKind::Chip, ChipWindow::new).map_err(|e| anyhow::anyhow!("could not create the chip window: {e}"))?;

        // Globals before anything is shown.
        main.global::<Brand>().set_app_name(APP_NAME.into());
        main.global::<Brand>().set_read_with_label(READ_WITH_LABEL.into());
        widget.global::<Brand>().set_app_name(APP_NAME.into());
        widget.global::<Brand>().set_read_with_label(READ_WITH_LABEL.into());
        chip.global::<Brand>().set_app_name(APP_NAME.into());
        chip.global::<Brand>().set_read_with_label(READ_WITH_LABEL.into());
        main.global::<Theme>().set_dark(init.dark);
        widget.global::<Theme>().set_dark(true);
        chip.global::<Theme>().set_dark(true);
        // The renderer gives the overlays no per-pixel alpha (Skia on DirectX 12): an opaque panel under the card /
        // pill keeps their animations and the translucent panel from blending over black.
        widget.set_opaque_backing(true);
        chip.set_opaque_backing(true);
        main.global::<Motion>().set_reduce(init.reduce_motion);
        widget.global::<Motion>().set_reduce(init.reduce_motion);
        chip.global::<Motion>().set_reduce(init.reduce_motion);
        main.set_theme_dark(init.dark);

        let snapshot = player.snapshot();
        let credits = CreditsUpdate { view: if init.settings.has_api_key { CreditsView::Loading } else { CreditsView::NoKey }, refreshing: false };
        let mut app = AppUi {
            main,
            widget,
            chip,
            core,
            player,
            selection,
            settings: init.settings.clone(),
            snapshot,
            credits,
            app_info: init.app_info.clone(),
            models: Arc::new(Vec::new()),
            dark: init.dark,
            reduce_motion: init.reduce_motion,
            art: art::ArtCache::default(),
            overlays_ready: false,
            shell: shell::ShellController::default(),
            home: home::HomeController::default(),
            history: history::HistoryController::default(),
            analytics: analytics::AnalyticsController::new(init.analytics_range),
            voices: voices::VoicesController::default(),
            settings_page: settings::SettingsController::default(),
            onboarding: onboarding::OnboardingController::default(),
            widget_ctl: widget::WidgetController::default(),
            chip_ctl: chip::ChipController::default(),
            recorder: hotkey_recorder::Recorder::default(),
            widget_window: widget_window::WidgetWindowState::default(),
            tray: None,
            sinks: Sinks::default(),
            sink: sink.clone(),
            main_state: None,
            main_focused: false,
            poll_timer: slint::Timer::default(),
            poll_key: None,
            hover_timer: slint::Timer::default(),
            drag_timer: slint::Timer::default(),
            geometry_timer: slint::Timer::default(),
            backdrop_timer: slint::Timer::default(),
            tick_timer: slint::Timer::default(),
            quitting: false,
            init,
        };

        // Seed every controller with the stored settings and app info, then open the startup route.
        let seed = [UiUpdate::Settings(app.settings.clone()), UiUpdate::AppInfo(app.app_info.clone())];
        for update in &seed {
            {
                let mut cx = cx!(app);
                let _ = app.shell.on_update(&mut cx, update);
            }
            for page in PAGES {
                let _ = on_page!(app, page, |c, cx| c.on_update(&mut cx, update));
            }
        }
        app.sinks.nav.target = None;
        let nav = app.shell.navigate(app.init.route.clone(), NavIntent::None, app.settings.onboarded);
        app.apply_navigation(nav);
        app.flush_sinks();
        app.push_shell();
        app.refresh_backdrop();
        app.chip_ctl.init(&app.chip, &mut app.art, &app.settings);
        let data = app.widget_ctl.build(&app.snapshot, &app.settings, &mut app.art);
        app.widget.set_data(data);

        wire_main(&app.main);
        wire_widget(&app.widget);
        wire_chip(&app.chip);
        app.start_tick();

        APP.with(|cell| *cell.borrow_mut() = Some(app));
        slint::Timer::single_shot(Duration::ZERO, || native_startup(0));
        Ok(())
    }

    /// Applies one update (called on the UI thread by the `UiSink` drain; module docs "Controller interface").
    pub fn apply(update: UiUpdate) {
        PENDING.with(|p| p.borrow_mut().push_back(update));
        drain_pending();
    }

    // ── updates ──

    fn apply_update(&mut self, update: UiUpdate) {
        if self.quitting {
            return;
        }
        let now = windowing::mono_ms();
        // 1. shared state (and the window-level parts of a few updates)
        match &update {
            UiUpdate::Settings(s) => self.settings = s.clone(),
            UiUpdate::Player(s) => {
                self.snapshot = s.clone();
                self.poll_key = Some(PollKey::of(s));
            }
            UiUpdate::Credits(c) => self.credits = c.clone(),
            UiUpdate::AppInfo(a) => self.app_info = a.clone(),
            UiUpdate::Models { result: Ok(m), .. } => self.models = m.clone(),
            UiUpdate::Theme { dark } => self.set_theme(*dark),
            UiUpdate::Motion(reduce) => self.set_motion(*reduce),
            _ => {}
        }
        // 2. the shell, then the pages
        let shell_dirty = {
            let mut cx = cx!(self);
            self.shell.on_update(&mut cx, &update)
        };
        let active = self.shell.shown_page();
        let to_all = reaches_every_page(&update);
        let mut active_dirty = false;
        for page in PAGES {
            if Some(page) == active {
                active_dirty = on_page!(self, page, |c, cx| c.on_update(&mut cx, &update));
            } else if to_all {
                let _ = on_page!(self, page, |c, cx| c.on_update(&mut cx, &update));
            }
        }
        // 3. the widget
        let effect = self.widget_ctl.on_update(&self.core, &update, &self.snapshot, &self.settings, now);
        self.apply_widget_effect(effect);
        if touches_widget(&update) {
            self.push_widget();
            self.arm_widget_timer();
        }
        // 4. native windows, tray, chip, harness
        match &update {
            UiUpdate::Settings(_) => {
                self.update_tray();
                self.update_poll_timer();
                // The chip's dot art is asked for by the chip only: a new voice queues its render now, so the next
                // chip already has it.
                self.chip_ctl.refresh_art(&self.chip, &mut self.art, &self.settings);
            }
            UiUpdate::Player(_) => {
                self.update_tray();
                self.update_poll_timer();
            }
            UiUpdate::ChipShow { x, y, mode } => {
                if self.overlays_ready {
                    self.chip_ctl.show(&self.chip, &self.selection, &mut self.art, &self.settings, *mode, *x, *y);
                }
            }
            UiUpdate::ChipHide => self.chip_ctl.hide(&self.chip, &self.selection),
            UiUpdate::DisplaysChanged => {
                if let Some(tray) = self.tray.as_mut() {
                    tray.refresh_dpi();
                }
                self.chip_ctl.hide(&self.chip, &self.selection);
                if self.widget_window.visible {
                    self.show_widget(true);
                }
            }
            UiUpdate::ReadStarted { req, result } => crate::shots::on_read_started(*req, result),
            UiUpdate::Refresh(_) => {
                self.push_shell();
            }
            _ => {}
        }
        // 5. sinks (navigation for Navigate / ShowMain happens here), then the main window's own updates
        self.flush_sinks();
        if shell_dirty {
            self.push_sidebar();
        }
        if active_dirty {
            if let Some(page) = self.shell.shown_page() {
                self.push_page(page);
            }
        }
        match update {
            UiUpdate::ShowMain(_) => {
                main_window::show(&self.main);
                self.schedule_main_state_check();
            }
            UiUpdate::HideMain => {
                main_window::hide(&self.main);
                self.schedule_main_state_check();
            }
            UiUpdate::Quit => self.quit_now(),
            _ => {}
        }
    }

    // ── sinks, navigation, pushing models ──

    /// Applies what the controllers collected (toasts, dialogs, field writes, navigation), repeatedly while that
    /// produced more work, then pushes the toast stack / dialog when they changed.
    fn flush_sinks(&mut self) {
        let mut toasts_changed = false;
        let mut dialog_changed = false;
        for _ in 0..MAX_SINK_ROUNDS {
            let mut work = false;
            if !self.sinks.toasts.pushes.is_empty() || !self.sinks.toasts.dismisses.is_empty() {
                let removed = self.shell.apply_toast_sink(&mut self.sinks.toasts);
                self.fan_out_removals(removed);
                toasts_changed = true;
                work = true;
            }
            let d = &self.sinks.dialogs;
            if d.open.is_some() || d.busy.is_some() || d.close.is_some() {
                self.shell.apply_dialog_sink(&mut self.sinks.dialogs);
                dialog_changed = true;
                work = true;
            }
            self.apply_field_writes();
            if let Some((route, intent)) = self.sinks.nav.target.take() {
                let nav = self.shell.navigate(route, intent, self.settings.onboarded);
                self.apply_navigation(nav);
                dialog_changed = true;
                work = true;
            }
            if !work {
                break;
            }
        }
        if toasts_changed {
            self.push_toasts();
        }
        if dialog_changed {
            self.main.set_dialog(self.shell.dialog_data());
        }
    }

    /// Toast removals go to the active page (History's undo batch).
    fn fan_out_removals(&mut self, removed: Vec<(String, shell::ToastRemoval)>) {
        if removed.is_empty() {
            return;
        }
        let Some(page) = self.shell.shown_page() else { return };
        for (id, why) in removed {
            on_page!(self, page, |c, cx| c.on_toast_removed(&mut cx, &id, why));
        }
    }

    fn apply_field_writes(&mut self) {
        let f = &mut self.sinks.fields;
        if let Some(t) = f.settings_port_draft.take() {
            self.main.set_settings_port_draft(t.into());
        }
        if let Some(t) = f.settings_exclude_draft.take() {
            self.main.set_settings_exclude_draft(t.into());
        }
        if let Some(t) = f.voices_search_text.take() {
            self.main.set_voices_search_text(t.into());
        }
        if let Some(t) = f.history_search_text.take() {
            self.main.set_history_search_text(t.into());
        }
        if let Some(t) = f.sidebar_search_text.take() {
            self.main.set_sidebar_search_text(t.into());
        }
        if let Some(t) = f.onboarding_key_draft.take() {
            self.main.set_onboarding_key_draft(t.into());
        }
        if let Some(t) = f.settings_key_draft.take() {
            self.main.set_settings_key_draft(t.into());
        }
    }

    /// A navigation the shell decided: `on_leave` / `on_enter`, then the main window's page and models.
    fn apply_navigation(&mut self, nav: shell::Navigation) {
        if let Some(left) = nav.left {
            on_page!(self, left, |c, cx| c.on_leave(&mut cx));
        }
        if let Some(entered) = nav.entered {
            on_page!(self, entered, |c, cx| c.on_enter(&mut cx, &nav.route, &nav.intent));
        }
        if let Some(page) = self.shell.shown_page() {
            self.push_page(page);
        }
        self.push_shell();
        self.update_poll_timer();
    }

    /// The shell's properties: page, gate, title, sidebar, toasts, dialog, settings scroll.
    fn push_shell(&mut self) {
        if let Some(page) = self.shell.shown_page() {
            self.main.set_page(shell::page_id(page));
        }
        self.main.set_onboarding(self.shell.onboarding());
        self.main.set_page_title(self.shell.page_title().into());
        let (to, nonce) = self.shell.settings_scroll();
        self.main.set_settings_scroll_to(to);
        self.main.set_settings_scroll_nonce(nonce);
        self.push_sidebar();
        self.push_toasts();
        self.main.set_dialog(self.shell.dialog_data());
    }

    /// The sidebar (nav, mini player, credits, hint) — set only when it changed, lists synced in place.
    fn push_sidebar(&mut self) {
        let data = {
            let mut cx = cx!(self);
            self.shell.build_sidebar(&mut cx)
        };
        if let Some(d) = stable::stabilize(data, &self.main.get_sidebar()) {
            self.main.set_sidebar(d);
        }
    }

    fn push_toasts(&mut self) {
        if let Some(m) = stable::stabilize_model(&self.shell.build_toasts(), &self.main.get_toasts()) {
            self.main.set_toasts(m);
        }
        self.arm_toast_timer();
    }

    /// Rebuilds one page's struct (set only when it changed, lists synced in place: module docs).
    fn push_page(&mut self, page: AppPage) {
        let mut cx = cx!(self);
        match page {
            AppPage::Home => {
                let d = self.home.build(&mut cx);
                if let Some(d) = stable::stabilize(d, &self.main.get_home()) {
                    self.main.set_home(d);
                }
            }
            AppPage::History => {
                let d = self.history.build(&mut cx);
                if let Some(d) = stable::stabilize(d, &self.main.get_history()) {
                    self.main.set_history(d);
                }
            }
            AppPage::Analytics => {
                let d = self.analytics.build(&mut cx);
                if let Some(d) = stable::stabilize(d, &self.main.get_analytics()) {
                    self.main.set_analytics(d);
                }
            }
            AppPage::Voices => {
                let d = self.voices.build(&mut cx);
                if let Some(d) = stable::stabilize(d, &self.main.get_voices()) {
                    self.main.set_voices(d);
                }
            }
            AppPage::Settings => {
                let d = self.settings_page.build(&mut cx);
                if let Some(d) = stable::stabilize(d, &self.main.get_settings()) {
                    self.main.set_settings(d);
                }
            }
            AppPage::Onboarding => {
                let d = self.onboarding.build(&mut cx);
                if let Some(d) = stable::stabilize(d, &self.main.get_onboarding_data()) {
                    self.main.set_onboarding_data(d);
                }
            }
        }
    }

    /// After a page callback: flush the sinks and rebuild the page when it is the one shown.
    fn after_page_callback(&mut self, page: AppPage) {
        self.flush_sinks();
        if self.shell.shown_page() == Some(page) {
            self.push_page(page);
        }
    }

    page_callback!(home_cb, home, home::HomeController, AppPage::Home);
    page_callback!(history_cb, history, history::HistoryController, AppPage::History);
    page_callback!(analytics_cb, analytics, analytics::AnalyticsController, AppPage::Analytics);
    page_callback!(voices_cb, voices, voices::VoicesController, AppPage::Voices);
    page_callback!(settings_cb, settings_page, settings::SettingsController, AppPage::Settings);
    page_callback!(onboarding_cb, onboarding, onboarding::OnboardingController, AppPage::Onboarding);

    /// A shell callback: runs `f(shell, cx)`, flushes and pushes the shell.
    fn shell_cb(&mut self, f: impl FnOnce(&mut shell::ShellController, &mut UiCtx)) {
        {
            let mut cx = cx!(self);
            f(&mut self.shell, &mut cx);
        }
        self.flush_sinks();
        self.push_shell();
    }

    // ── toasts and dialogs ──

    fn arm_toast_timer(&mut self) {
        self.shell.arm_toast_timer(windowing::mono_ms(), || with_app("toast timer", |app| app.on_toast_timer()));
    }

    fn on_toast_timer(&mut self) {
        let removed = self.shell.tick_toasts(windowing::mono_ms());
        self.fan_out_removals(removed);
        self.flush_sinks();
        self.push_toasts();
    }

    fn toast_hover(&mut self, id: &str, hovered: bool) {
        if self.shell.toast_hover(id, hovered, windowing::mono_ms()) {
            self.push_toasts();
        }
    }

    fn toast_dismiss(&mut self, id: &str) {
        if self.shell.toast_dismiss(id, windowing::mono_ms()) {
            self.push_toasts();
        }
    }

    fn toast_action(&mut self, id: &str) {
        let Some(target) = self.shell.toast_action(id, windowing::mono_ms()) else { return };
        match target {
            shell::ToastActionTarget::Route(route) => self.sinks.nav.navigate(route),
            shell::ToastActionTarget::Page(page) => {
                if self.shell.shown_page() == Some(page) {
                    let _ = on_page!(self, page, |c, cx| c.on_toast_action(&mut cx, id));
                    self.flush_sinks();
                    self.push_page(page);
                }
            }
            shell::ToastActionTarget::Nothing => {}
        }
        self.flush_sinks();
        self.push_toasts();
    }

    fn dialog(&mut self, id: &str, confirmed: bool) {
        let owner = if confirmed { self.shell.dialog_confirm(id) } else { self.shell.dialog_cancel(id) };
        let Some(page) = owner else { return };
        let handled = on_page!(self, page, |c, cx| c.on_dialog(&mut cx, id, confirmed));
        if !handled && !confirmed {
            self.shell.close_dialog(id);
        }
        self.flush_sinks();
        self.main.set_dialog(self.shell.dialog_data());
        if self.shell.shown_page() == Some(page) {
            self.push_page(page);
        }
    }

    // ── main window ──

    /// The main window's native events (module docs "Native events"). True = consumed.
    fn on_main_event(&mut self, event: &WinitEvent) -> bool {
        if hotkey_recorder::on_winit_event(&mut self.recorder, event) {
            return true;
        }
        match event {
            WinitEvent::Moved(_) => self.schedule_geometry_save(),
            WinitEvent::Resized(_) => {
                self.schedule_geometry_save();
                self.schedule_backdrop();
                self.schedule_main_state_check();
            }
            WinitEvent::ScaleFactorChanged { .. } => self.schedule_backdrop(),
            WinitEvent::Focused(focused) => {
                self.main_focused = *focused;
                self.schedule_main_state_check();
            }
            WinitEvent::Occluded(_) => self.schedule_main_state_check(),
            WinitEvent::MouseInput { state: ElementState::Released, button, .. } => {
                let nav = match button {
                    MouseButton::Back => self.shell.back(self.settings.onboarded),
                    MouseButton::Forward => self.shell.forward(self.settings.onboarded),
                    _ => None,
                };
                if let Some(nav) = nav {
                    self.apply_navigation(nav);
                    self.flush_sinks();
                }
            }
            _ => {}
        }
        false
    }

    /// The title-bar X / Alt+F4 / an outside `WM_CLOSE` (shell.md §3.7): close to tray or quit.
    pub(crate) fn on_main_close(&mut self) {
        if self.settings.app.close_to_tray {
            if let Some(state) = main_window::current_state(&self.main) {
                self.core.send(Command::SaveWindowState(state));
            }
            main_window::hide(&self.main);
            self.schedule_main_state_check();
        } else {
            self.quit_from_ui();
        }
    }

    /// Quit asked by the UI (tray Quit, main close with close-to-tray off, shots end): History commits its pending
    /// undo batch first so its `RemoveReads` precede `Quit` (D-UM-32).
    pub(crate) fn quit_from_ui(&mut self) {
        {
            let mut cx = cx!(self);
            self.history.flush(&mut cx);
        }
        self.flush_sinks();
        self.core.send(Command::Quit);
    }

    fn show_main_from_tray(&mut self, route: Option<AppRoute>) {
        if let Some(route) = route {
            self.sinks.nav.navigate(route);
            self.flush_sinks();
        }
        main_window::show(&self.main);
        self.schedule_main_state_check();
    }

    fn schedule_main_state_check(&mut self) {
        with_app_later(|app| app.check_main_state());
    }

    /// The main window's real focus (winit's state; the last `Focused` event when the window does not exist).
    fn main_has_focus(&mut self) -> bool {
        if let Some(focused) = main_window::has_focus(&self.main) {
            self.main_focused = focused;
        }
        self.main_focused
    }

    /// Reports `MainState{visible, focused}` when it changed; wakes the active page (≤ once per 30 s).
    fn check_main_state(&mut self) {
        let visible = main_window::is_visible(&self.main);
        let focused = visible && self.main_has_focus();
        if self.main_state == Some((visible, focused)) {
            return;
        }
        self.main_state = Some((visible, focused));
        self.core.send(Command::Window(WindowEvent::MainState { visible, focused }));
        if let Some(page) = self.shell.on_main_window_activity(focused, visible, windowing::mono_ms()) {
            on_page!(self, page, |c, cx| c.on_wake(&mut cx));
            self.flush_sinks();
            self.push_page(page);
        }
        self.update_poll_timer();
    }

    fn schedule_geometry_save(&mut self) {
        self.geometry_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(GEOMETRY_SAVE_MS), || {
            with_app("geometry save", |app| {
                if main_window::is_visible(&app.main) {
                    if let Some(state) = main_window::current_state(&app.main) {
                        app.core.send(Command::SaveWindowState(state));
                    }
                }
            });
        });
    }

    fn schedule_backdrop(&mut self) {
        self.backdrop_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(BACKDROP_DEBOUNCE_MS), || {
            with_app("backdrop", |app| app.refresh_backdrop());
        });
    }

    /// The backdrop at the main window's physical size (the startup placement until the window exists).
    fn refresh_backdrop(&mut self) {
        let size = self.main.window().size();
        let (w, h) = if size.width > 0 && size.height > 0 {
            (size.width, size.height)
        } else {
            let r = self.init.main_placement.0;
            (r.width.max(1) as u32, r.height.max(1) as u32)
        };
        let img = self.art.backdrop(w, h, self.dark);
        self.main.set_backdrop(img);
    }

    fn set_theme(&mut self, dark: bool) {
        self.dark = dark;
        self.main.global::<Theme>().set_dark(dark);
        self.main.set_theme_dark(dark);
        main_window::set_caption(&self.main, dark);
        self.refresh_backdrop();
    }

    fn set_motion(&mut self, reduce: bool) {
        self.reduce_motion = reduce;
        self.main.global::<Motion>().set_reduce(reduce);
        self.widget.global::<Motion>().set_reduce(reduce);
        self.chip.global::<Motion>().set_reduce(reduce);
    }

    // ── widget ──

    fn push_widget(&mut self) {
        if self.widget_window.visible {
            let data = self.widget_ctl.build(&self.snapshot, &self.settings, &mut self.art);
            self.widget.set_data(data);
        }
    }

    fn arm_widget_timer(&mut self) {
        self.widget_ctl.arm_timer(windowing::mono_ms(), || with_app("widget timer", |app| app.on_widget_timer()));
    }

    fn on_widget_timer(&mut self) {
        let effect = self.widget_ctl.on_timer(&self.snapshot, &self.settings, windowing::mono_ms());
        self.apply_widget_effect(effect);
        self.push_widget();
        self.arm_widget_timer();
    }

    fn apply_widget_effect(&mut self, effect: widget::WidgetEffect) {
        match effect {
            widget::WidgetEffect::None => {}
            widget::WidgetEffect::Show { reposition } => self.show_widget(reposition),
            widget::WidgetEffect::Hide => self.hide_widget(),
            widget::WidgetEffect::Reposition => {
                if self.widget_window.visible {
                    self.show_widget(true);
                }
            }
        }
    }

    /// Shows the widget without activating it (placed first when hidden or `reposition`; shell.md §6.3.4).
    fn show_widget(&mut self, reposition: bool) {
        if !self.overlays_ready || self.quitting {
            return;
        }
        let was_visible = self.widget_window.visible;
        if !was_visible {
            // The shared snapshot may lag the player by up to one poll (a read's show follows the player's report of
            // the new session, but other shows do not): read the player now so the card is built (and sized) for
            // what it plays.
            self.refresh_snapshot();
        }
        let data = self.widget_ctl.build(&self.snapshot, &self.settings, &mut self.art);
        self.widget.set_data(data);
        if !was_visible {
            // The card's size for the data just set (the window does not bind its own size).
            let (w, h) = (self.widget.get_card_width(), self.widget.get_card_height());
            widget_window::resize(&self.widget, &mut self.widget_window, f64::from(w), f64::from(h));
        }
        widget_window::show(&self.widget, &mut self.widget_window, &self.settings, reposition);
        if !was_visible && self.widget_window.visible {
            self.core.send(Command::Window(WindowEvent::WidgetVisible(true)));
            self.start_hover_poll();
        }
        self.update_poll_timer();
    }

    /// The shared snapshot from the player now (the poll's change key is reset so the next poll republishes).
    fn refresh_snapshot(&mut self) {
        let s = self.player.snapshot();
        if self.poll_key.as_ref() == Some(&PollKey::of(&s)) {
            return;
        }
        self.poll_key = None;
        self.snapshot = s.clone();
        self.widget_ctl.on_snapshot(&s, &self.settings, windowing::mono_ms());
    }

    /// `ScaleFactorChanged` on an overlay (module docs "Native events").
    fn on_overlay_scale_changed(&mut self, kind: WindowKind) {
        match kind {
            WindowKind::Widget if self.widget_window.visible => self.show_widget(true),
            WindowKind::Chip if !self.chip_ctl.hiding => self.chip_ctl.hide(&self.chip, &self.selection),
            _ => {}
        }
    }

    fn hide_widget(&mut self) {
        if !self.widget_window.visible {
            return;
        }
        widget_window::hide(&self.widget, &mut self.widget_window);
        self.hover_timer.stop();
        self.drag_timer.stop();
        self.widget_ctl.set_hovered(false);
        self.core.send(Command::Window(WindowEvent::WidgetVisible(false)));
        self.update_poll_timer();
    }

    fn start_hover_poll(&mut self) {
        if let Some(h) = widget_window::poll_hover(&self.widget, &mut self.widget_window) {
            self.widget_ctl.set_hovered(h);
        }
        self.hover_timer.start(slint::TimerMode::Repeated, Duration::from_millis(HOVER_POLL_MS), || {
            with_app("hover poll", |app| {
                if let Some(h) = widget_window::poll_hover(&app.widget, &mut app.widget_window) {
                    app.widget_ctl.set_hovered(h);
                }
                if !app.widget_window.visible {
                    app.hover_timer.stop();
                }
            });
        });
    }

    fn widget_action(&mut self, action: PlayerAction) {
        let effect = self.widget_ctl.action(&self.core, &self.snapshot, action);
        self.apply_widget_effect(effect);
        self.push_widget();
        self.arm_widget_timer();
    }

    fn widget_toast_action(&mut self) {
        let effect = self.widget_ctl.toast_action(&self.core, &self.snapshot);
        self.apply_widget_effect(effect);
        self.push_widget();
        self.arm_widget_timer();
    }

    fn widget_drag_start(&mut self) {
        widget_window::start_drag(&self.widget, &mut self.widget_window);
        if self.widget_window.drag_start.is_none() {
            return;
        }
        self.drag_timer.start(slint::TimerMode::Repeated, Duration::from_millis(DRAG_POLL_MS), || {
            with_app("drag poll", |app| match widget_window::poll_drag(&app.widget, &mut app.widget_window, windowing::mono_ms()) {
                widget_window::DragPoll::Pending => {}
                widget_window::DragPoll::Done => app.drag_timer.stop(),
                widget_window::DragPoll::Moved { x, y } => {
                    app.drag_timer.stop();
                    app.core.send(Command::Window(WindowEvent::WidgetDragged { x, y }));
                }
            });
        });
    }

    // ── 30 Hz snapshot poll (ARCHITECTURE.md §3.3) ──

    /// Runs the poll while a read is playing / loading and a player surface is visible.
    fn update_poll_timer(&mut self) {
        let active = matches!(self.snapshot.state.status, PlayerStatus::Playing | PlayerStatus::Loading);
        let main_visible = self.main_state.map(|(v, _)| v).unwrap_or(false) && !self.shell.onboarding();
        if active && (self.widget_window.visible || main_visible) && !self.quitting {
            if !self.poll_timer.running() {
                self.poll_timer.start(slint::TimerMode::Repeated, Duration::from_millis(UI_FRAME_MS), || {
                    with_app("snapshot poll", |app| app.on_poll());
                });
            }
        } else {
            self.poll_timer.stop();
        }
    }

    fn on_poll(&mut self) {
        let s = self.player.snapshot();
        let key = PollKey::of(&s);
        if self.poll_key.as_ref() == Some(&key) {
            return;
        }
        self.poll_key = Some(key);
        self.snapshot = s.clone();
        let now = windowing::mono_ms();
        self.widget_ctl.on_snapshot(&s, &self.settings, now);
        self.push_widget();
        self.arm_widget_timer();
        let update = UiUpdate::Player(s);
        let shell_dirty = {
            let mut cx = cx!(self);
            self.shell.on_update(&mut cx, &update)
        };
        if shell_dirty {
            self.push_sidebar();
        }
        if self.shell.shown_page() == Some(AppPage::Home) {
            let dirty = {
                let mut cx = cx!(self);
                self.home.on_update(&mut cx, &update)
            };
            self.flush_sinks();
            if dirty {
                self.push_page(AppPage::Home);
            }
        }
        self.update_poll_timer();
    }

    // ── tray, art, tick, quit ──

    fn update_tray(&mut self) {
        if let Some(tray) = self.tray.as_mut() {
            tray.update(tray_view::TrayView::build(&self.settings, &self.snapshot.state));
        }
    }

    fn on_art_ready(&mut self) {
        if !self.art.take_ready() {
            return;
        }
        self.refresh_backdrop();
        self.push_sidebar();
        if let Some(page) = self.shell.shown_page() {
            self.push_page(page);
        }
        self.push_widget();
        self.chip_ctl.refresh_art(&self.chip, &mut self.art, &self.settings);
    }

    fn start_tick(&mut self) {
        self.tick_timer.start(slint::TimerMode::Repeated, Duration::from_millis(TICK_MS), || {
            with_app("tick", |app| {
                if let Some(page) = app.shell.shown_page() {
                    on_page!(app, page, |c, cx| c.on_tick(&mut cx));
                    app.flush_sinks();
                    app.push_page(page);
                }
                app.push_sidebar();
            });
        });
    }

    /// `UiUpdate::Quit` (shell.md §3.5): save the window state synchronously, destroy the tray, hide every window, arm
    /// the 4 s hard exit and leave the event loop.
    fn quit_now(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        if let Some(state) = main_window::current_state(&self.main) {
            vox_store::window_state::save_window_state(&self.init.profile, &state);
        }
        if let Some(mut tray) = self.tray.take() {
            tray.destroy();
        }
        for t in [&self.poll_timer, &self.hover_timer, &self.drag_timer, &self.geometry_timer, &self.backdrop_timer, &self.tick_timer] {
            t.stop();
        }
        let _ = self.chip.hide();
        let _ = self.widget.hide();
        let _ = self.main.hide();
        let spawned = std::thread::Builder::new().name("vox-hard-exit".into()).spawn(|| {
            std::thread::sleep(Duration::from_millis(HARD_EXIT_MS));
            tracing::warn!(target: "app", "event loop did not end in time; exiting");
            std::process::exit(crate::shots::hard_exit_code());
        });
        if let Err(e) = spawned {
            tracing::warn!(target: "app", "hard-exit timer could not start: {e}");
        }
        if let Err(e) = slint::quit_event_loop() {
            tracing::warn!(target: "app", "could not quit the event loop: {e}");
        }
    }

    // ── shots harness helpers ──

    /// Navigates the main window (the shots harness).
    pub(crate) fn navigate(&mut self, route: AppRoute) {
        self.sinks.nav.navigate(route);
        self.flush_sinks();
    }

    /// Shows the widget as `Widget(Show)` would (the shots harness).
    pub(crate) fn harness_show_widget(&mut self) {
        let effect = self.widget_ctl.on_update(&self.core, &UiUpdate::Widget(crate::update::WidgetIntent::Show { reposition: true }), &self.snapshot, &self.settings, windowing::mono_ms());
        self.apply_widget_effect(effect);
    }

    /// Hides the widget as `Widget(Hide)` would (the shots harness).
    pub(crate) fn harness_hide_widget(&mut self) {
        let effect = self.widget_ctl.on_update(&self.core, &UiUpdate::Widget(crate::update::WidgetIntent::Hide), &self.snapshot, &self.settings, windowing::mono_ms());
        self.apply_widget_effect(effect);
    }

    /// Refreshes the shared snapshot from the player now (the shots harness drives the player directly).
    pub(crate) fn harness_poll(&mut self) {
        let s = self.player.snapshot();
        self.poll_key = None;
        self.snapshot = s.clone();
        self.widget_ctl.on_snapshot(&s, &self.settings, windowing::mono_ms());
        self.push_widget();
    }
}

/// Drains the pending updates in order while the app is free (module docs "Re-entrancy").
fn drain_pending() {
    loop {
        let applied = APP.with(|cell| {
            let mut slot = cell.try_borrow_mut().ok()?;
            let app = slot.as_mut()?;
            let next = PENDING.with(|p| p.borrow_mut().pop_front())?;
            guard("ui update", || app.apply_update(next));
            Some(())
        });
        match applied {
            Some(()) => continue,
            None => break,
        }
    }
    let left = PENDING.with(|p| !p.borrow().is_empty());
    let exists = APP.with(|cell| cell.try_borrow().map(|s| s.is_some()).unwrap_or(true));
    if left && exists {
        slint::Timer::single_shot(Duration::ZERO, drain_pending);
    }
}

/// Updates every page controller sees (answers to their own requests, catalog answers, refreshes).
fn reaches_every_page(update: &UiUpdate) -> bool {
    matches!(
        update,
        UiUpdate::SettingsSaved { .. }
            | UiUpdate::HistoryPage { .. }
            | UiUpdate::Home { .. }
            | UiUpdate::Analytics { .. }
            | UiUpdate::Voices { .. }
            | UiUpdate::Models { .. }
            | UiUpdate::Preview(_)
            | UiUpdate::ReadStarted { .. }
            | UiUpdate::ApiKey { .. }
            | UiUpdate::Done { .. }
            | UiUpdate::CacheStats { .. }
            | UiUpdate::Exported { .. }
            | UiUpdate::ExpiringCount { .. }
            | UiUpdate::FavoriteToggled { .. }
            | UiUpdate::Refresh(_)
            | UiUpdate::HotkeysResumed
    )
}

/// Updates after which the widget's data / timer may have changed.
fn touches_widget(update: &UiUpdate) -> bool {
    matches!(
        update,
        UiUpdate::Player(_) | UiUpdate::Settings(_) | UiUpdate::Toast(_) | UiUpdate::Widget(_) | UiUpdate::Refresh(_) | UiUpdate::Motion(_)
    )
}

/// What the 30 Hz poll compares (reader-playback.md §8.9): `status | session | wantsPlay | buffering | starting |
/// warning | partialText | charIndex | floor(position×10) | round(duration×10) | estimate | round(buffered×1000) | rate |
/// volume | error`.
#[derive(Debug, Clone, PartialEq)]
pub struct PollKey {
    status: PlayerStatus,
    session: Option<String>,
    wants_play: bool,
    buffering: bool,
    starting: bool,
    warning: Option<String>,
    partial_text: bool,
    char_index: u32,
    position: i64,
    duration: i64,
    estimate: bool,
    buffered: i64,
    rate: u64,
    volume: u64,
    error: Option<String>,
}

impl PollKey {
    /// The change key of a snapshot (pure; tested).
    pub fn of(s: &PlayerSnapshot) -> PollKey {
        let st = &s.state;
        let finite = |v: f64| if v.is_finite() { v } else { 0.0 };
        PollKey {
            status: st.status,
            session: s.session.as_ref().map(|x| x.id.clone()).or_else(|| st.session_id.clone()),
            wants_play: s.wants_play,
            buffering: s.buffering,
            starting: s.starting,
            warning: s.warning.as_ref().map(|w| w.message.clone()),
            partial_text: s.partial_text,
            char_index: st.char_index,
            position: (finite(st.position_sec) * 10.0).floor() as i64,
            duration: (finite(st.duration_sec) * 10.0).round() as i64,
            estimate: st.duration_is_estimate,
            buffered: (finite(st.buffered_fraction) * 1000.0).round() as i64,
            rate: st.rate.to_bits(),
            volume: st.volume.to_bits(),
            error: st.error.clone(),
        }
    }
}

// ───────────────────────────── startup on the event loop ─────────────────────────────

/// The first turns of the event loop (module docs "Startup"): retried until the native windows exist.
fn native_startup(attempt: u32) {
    let ready = with_app_now(|app| {
        let hwnds = (windowing::hwnd_of(app.main.window()), windowing::hwnd_of(app.widget.window()), windowing::hwnd_of(app.chip.window()));
        match hwnds {
            (Some(_), Some(widget), Some(chip)) => {
                app.style_native_windows(widget, chip);
                true
            }
            _ => false,
        }
    });
    match ready {
        Some(true) => {
            // Let the off-screen pre-show render a frame, then hide the overlays and finish.
            slint::Timer::single_shot(Duration::from_millis(30), || with_app("startup", |app| app.finish_startup()));
        }
        Some(false) if attempt < NATIVE_STARTUP_TRIES => {
            slint::Timer::single_shot(Duration::from_millis(20), move || native_startup(attempt + 1));
        }
        Some(false) => {
            tracing::error!(target: "window", "native windows were not created; continuing without overlay styling");
            with_app("startup", |app| app.finish_startup());
        }
        // The app is busy (re-entrant) or gone: try again shortly.
        None if attempt < NATIVE_STARTUP_TRIES => {
            slint::Timer::single_shot(Duration::from_millis(20), move || native_startup(attempt + 1));
        }
        None => {}
    }
}

impl AppUi {
    /// Main window: caption, placement, icon, dispatcher, close; overlays: subclass + off-screen pre-show.
    fn style_native_windows(&mut self, widget_hwnd: isize, chip_hwnd: isize) {
        main_window::on_created(&self.main, self.dark);
        main_window::apply_placement(&self.main, self.init.main_placement);
        set_window_icon(&self.main);
        for (kind, hwnd) in [(WindowKind::Widget, widget_hwnd), (WindowKind::Chip, chip_hwnd)] {
            let core = self.core.clone();
            let on_close: vox_win::window::OverlayCloseHandler = Arc::new(move || core.send(Command::Window(WindowEvent::CloseRequested(kind))));
            if let Err(e) = vox_win::window::install_overlay_subclass(hwnd, on_close) {
                tracing::error!(target: "window", "overlay styles could not be installed on the {kind:?} window: {e}");
            }
            let size = vox_win::window::window_rect(hwnd).map(|r| (r.width.max(1), r.height.max(1))).unwrap_or((1, 1));
            vox_win::window::set_bounds(hwnd, PhysRect { x: windowing::OFFSCREEN, y: windowing::OFFSCREEN, width: size.0, height: size.1 });
        }
        if let Err(e) = self.widget.show() {
            tracing::debug!(target: "window", "widget pre-show failed: {e}");
        }
        if let Err(e) = self.chip.show() {
            tracing::debug!(target: "window", "chip pre-show failed: {e}");
        }
        for w in [self.widget.window(), self.chip.window()] {
            w.on_close_requested(|| slint::CloseRequestResponse::KeepWindowShown);
        }
        {
            use slint::winit_030::WinitWindowAccessor;
            self.widget.window().on_winit_window_event(|w, event| overlay_window_event(WindowKind::Widget, w, event));
            self.chip.window().on_winit_window_event(|w, event| overlay_window_event(WindowKind::Chip, w, event));
        }
        // The widget's first-frame reveal (`windowing::reveal`, fed by its dispatcher; the chip shows in place).
        windowing::reveal::install(widget_hwnd);
        self.catch_outside_close();
    }

    /// Subclasses winit's visible event target window so an outside `WM_CLOSE` quits gracefully (`windowing` docs
    /// "Outside WM_CLOSE").
    fn catch_outside_close(&mut self) {
        let targets = windowing::native::thread_windows_of_class(windowing::native::WINIT_EVENT_TARGET_CLASS);
        // Linux: outside close requests arrive as SIGTERM (vox_win::system), not as a window message.
        if targets.is_empty() && cfg!(windows) {
            tracing::warn!(target: "window", "winit's event target window was not found; an outside close request may not quit cleanly");
        }
        for hwnd in targets {
            let core = self.core.clone();
            let on_close: vox_win::window::OverlayCloseHandler =
                Arc::new(move || core.send(Command::System(vox_win::system::SystemEvent::CloseRequested)));
            if let Err(e) = vox_win::window::install_overlay_subclass(hwnd, on_close) {
                tracing::warn!(target: "window", "outside close requests are not caught on winit's event target window: {e}");
            }
        }
    }

    /// Hides the pre-shown overlays, marks them ready, shows the main window if asked, reports the first `MainState`,
    /// attaches the sink and creates the tray.
    fn finish_startup(&mut self) {
        let _ = self.widget.hide();
        let _ = self.chip.hide();
        self.overlays_ready = true;
        // Seed the sizes the windows reported during construction (their `init` ran before the callbacks existed).
        let (w, h) = (self.widget.get_card_width(), self.widget.get_card_height());
        widget_window::resize(&self.widget, &mut self.widget_window, f64::from(w), f64::from(h));
        let (cw, ch) = (self.chip.get_pill_width(), self.chip.get_pill_height());
        self.chip_ctl.size_changed(&self.chip, &self.selection, cw, ch);
        if self.init.show_main {
            main_window::show(&self.main);
        }
        // Read, not assumed: Windows may refuse the foreground (focus stolen during a slow start, a login launch).
        let visible = self.init.show_main && main_window::is_visible(&self.main);
        let focused = visible && self.main_has_focus();
        self.main_state = Some((visible, focused));
        self.core.send(Command::Window(WindowEvent::MainState { visible, focused }));
        let _ = self.shell.on_main_window_activity(focused, visible, windowing::mono_ms());
        if self.tray.is_none() {
            match crate::tray::Tray::create(self.core.clone()) {
                Ok(tray) => self.tray = Some(tray),
                Err(e) => tracing::error!(target: "tray", "{e:#}"),
            }
        }
        self.update_tray();
        self.refresh_backdrop();
        self.sink.attach();
        if visible {
            self.schedule_main_state_check();
        }
    }
}

/// The main window's icons (D-SH-20): the title bar's small icon at 48 px, and the big icon (taskbar, Alt+Tab) at
/// 256 px, the pixels of `assets/vox-256.png` (both come from [`crate::tray::app_icon_rgba`], which generated the
/// asset), so Windows only ever scales them down.
fn set_window_icon(main: &MainWindow) {
    #[cfg(windows)]
    use slint::winit_030::winit::platform::windows::WindowExtWindows;
    use slint::winit_030::winit::window::Icon;
    let icon = |px: u32| Icon::from_rgba(crate::tray::app_icon_rgba(px), px, px);
    match (icon(WINDOW_ICON_PX), icon(TASKBAR_ICON_PX)) {
        (Ok(small), Ok(big)) => {
            windowing::with_winit(main.window(), |w| {
                #[cfg(windows)]
                {
                    w.set_window_icon(Some(small));
                    w.set_taskbar_icon(Some(big));
                }
                // X11: one `_NET_WM_ICON`; the big one scales down best.
                #[cfg(not(windows))]
                {
                    drop(small);
                    w.set_window_icon(Some(big));
                }
            });
        }
        (Err(e), _) | (_, Err(e)) => tracing::debug!(target: "window", "window icon rejected: {e}"),
    }
}

// ───────────────────────────── callback wiring ─────────────────────────────

/// `$win.$setter(|args| with_app(.., |app| app.$helper(|c, cx| body)))`.
macro_rules! on {
    ($win:expr, $setter:ident, $helper:ident, |$c:ident, $cx:ident $(, $arg:ident)*| $body:expr) => {
        $win.$setter(move |$($arg),*| {
            with_app(stringify!($setter), move |app| app.$helper(move |$c, $cx| {
                $body;
            }));
        });
    };
}

/// Every `MainWindow` callback (ARCHITECTURE.md §7.3 table).
fn wire_main(m: &MainWindow) {
    // ── shell ──
    on!(m, on_navigate, shell_cb, |c, cx, page| c.nav_clicked(cx, page));
    on!(m, on_sidebar_search, shell_cb, |c, cx, text| c.sidebar_search(cx, &text));
    on!(m, on_mini_action, shell_cb, |c, cx, action| c.mini_action(cx, action));
    on!(m, on_mini_open, shell_cb, |c, cx| c.mini_open(cx));
    on!(m, on_credits_open, shell_cb, |c, cx| c.credits_open(cx));
    m.on_header_send_to_tray(|| {
        with_app("header-send-to-tray", |app| {
            let state = main_window::current_state(&app.main);
            app.shell_cb(move |c, cx| c.send_to_tray(cx, state));
        });
    });
    m.on_header_toggle_theme(|| {
        with_app("header-toggle-theme", |app| {
            let dark = app.dark;
            app.shell_cb(move |c, cx| c.toggle_theme(cx, dark));
        });
    });
    m.on_toast_action(|id| with_app("toast-action", move |app| app.toast_action(&id)));
    m.on_toast_dismiss(|id| with_app("toast-dismiss", move |app| app.toast_dismiss(&id)));
    m.on_toast_hover(|id, hovered| with_app("toast-hover", move |app| app.toast_hover(&id, hovered)));
    m.on_dialog_confirm(|id| with_app("dialog-confirm", move |app| app.dialog(&id, true)));
    m.on_dialog_cancel(|id| with_app("dialog-cancel", move |app| app.dialog(&id, false)));

    // ── home ──
    on!(m, on_home_quick_read, home_cb, |c, cx, text| c.quick_read(cx, &text));
    on!(m, on_home_quick_read_edited, home_cb, |c, cx, text| c.quick_read_edited(cx, &text));
    on!(m, on_home_read_clipboard, home_cb, |c, cx| c.read_clipboard(cx));
    on!(m, on_home_player_action, home_cb, |c, cx, action| c.player_action(cx, action));
    on!(m, on_home_seek, home_cb, |c, cx, fraction| c.seek(cx, fraction));
    on!(m, on_home_replay, home_cb, |c, cx, id| c.replay(cx, &id));
    on!(m, on_home_replay_last, home_cb, |c, cx| c.replay_last(cx));
    on!(m, on_home_pause_live, home_cb, |c, cx| c.pause_live(cx));
    on!(m, on_home_resume_live, home_cb, |c, cx| c.resume_live(cx));
    on!(m, on_home_open_page, home_cb, |c, cx, page| c.open_page(cx, page));
    on!(m, on_home_open_settings, home_cb, |c, cx, section| c.open_settings(cx, &section));
    on!(m, on_home_credits_retry, home_cb, |c, cx| c.credits_retry(cx));
    on!(m, on_home_retry, home_cb, |c, cx| c.retry(cx));

    // ── history ──
    on!(m, on_history_search_edited, history_cb, |c, cx, text| c.search_edited(cx, &text));
    on!(m, on_history_set_favorites_only, history_cb, |c, cx, on| c.set_favorites_only(cx, on));
    on!(m, on_history_set_source, history_cb, |c, cx, value| c.set_source(cx, &value));
    on!(m, on_history_clear_filters, history_cb, |c, cx| c.clear_filters(cx));
    on!(m, on_history_toggle_row, history_cb, |c, cx, id| c.toggle_row(cx, &id));
    on!(m, on_history_replay, history_cb, |c, cx, id| c.replay(cx, &id));
    on!(m, on_history_copy_text, history_cb, |c, cx, id| c.copy_text(cx, &id));
    on!(m, on_history_toggle_favorite, history_cb, |c, cx, id| c.toggle_favorite(cx, &id));
    on!(m, on_history_delete, history_cb, |c, cx, id| c.delete(cx, &id));
    on!(m, on_history_open_url, history_cb, |c, cx, id| c.open_url(cx, &id));
    on!(m, on_history_load_more, history_cb, |c, cx| c.load_more(cx));
    m.on_history_export(|| {
        with_app("history-export", |app| {
            let owner = windowing::hwnd_of(app.main.window());
            app.history_cb(move |c, cx| c.export(cx, owner));
        });
    });
    on!(m, on_history_clear_all, history_cb, |c, cx| c.clear_all(cx));
    on!(m, on_history_retry, history_cb, |c, cx| c.retry(cx));
    on!(m, on_history_paste_and_read, history_cb, |c, cx| c.paste_and_read(cx));

    // ── analytics ──
    on!(m, on_analytics_set_range, analytics_cb, |c, cx, value| c.set_range(cx, &value));
    on!(m, on_analytics_retry, analytics_cb, |c, cx| c.retry(cx));
    on!(m, on_analytics_refresh_credits, analytics_cb, |c, cx| c.refresh_credits(cx));
    on!(m, on_analytics_add_key, analytics_cb, |c, cx| c.add_key(cx));
    on!(m, on_analytics_daily_width, analytics_cb, |c, cx, width| c.set_plot_width(cx, width));

    // ── voices ──
    on!(m, on_voices_search_edited, voices_cb, |c, cx, text| c.search_edited(cx, &text));
    on!(m, on_voices_set_gender, voices_cb, |c, cx, value| c.set_gender(cx, &value));
    on!(m, on_voices_set_accent, voices_cb, |c, cx, value| c.set_accent(cx, &value));
    on!(m, on_voices_set_category, voices_cb, |c, cx, value| c.set_category(cx, &value));
    on!(m, on_voices_clear_filters, voices_cb, |c, cx| c.clear_filters(cx));
    on!(m, on_voices_refresh, voices_cb, |c, cx| c.refresh(cx));
    on!(m, on_voices_preview, voices_cb, |c, cx, id| c.preview(cx, &id));
    on!(m, on_voices_use_voice, voices_cb, |c, cx, id| c.use_voice(cx, &id));
    on!(m, on_voices_add_key, voices_cb, |c, cx| c.add_key(cx));

    // ── settings ──
    on!(m, on_settings_section_selected, settings_cb, |c, cx, section| c.section_selected(cx, &section));
    on!(m, on_settings_set_bool, settings_cb, |c, cx, key, value| c.set_bool(cx, &key, value));
    on!(m, on_settings_set_number, settings_cb, |c, cx, key, value| c.set_number(cx, &key, value));
    on!(m, on_settings_set_choice, settings_cb, |c, cx, key, value| c.set_choice(cx, &key, &value));
    m.on_settings_format_number(|key, value| format_number(&key, value));
    on!(m, on_settings_save_key, settings_cb, |c, cx, key| c.save_key(cx, &key));
    on!(m, on_settings_key_edited, settings_cb, |c, cx| c.key_edited(cx));
    on!(m, on_settings_provider_connect, settings_cb, |c, cx, id, key, secret, region| c.provider_connect(cx, &id, &key, &secret, region));
    on!(m, on_settings_provider_disconnect, settings_cb, |c, cx, id| c.provider_disconnect(cx, &id));
    on!(m, on_settings_provider_edited, settings_cb, |c, cx, id| c.provider_edited(cx, &id));
    on!(m, on_settings_open_key_form, settings_cb, |c, cx| c.open_key_form(cx));
    on!(m, on_settings_cancel_key_form, settings_cb, |c, cx| c.cancel_key_form(cx));
    on!(m, on_settings_remove_key, settings_cb, |c, cx| c.remove_key(cx));
    on!(m, on_settings_get_key, settings_cb, |c, cx| c.get_key(cx));
    on!(m, on_settings_credits_retry, settings_cb, |c, cx| c.credits_retry(cx));
    on!(m, on_settings_test_voice, settings_cb, |c, cx| c.test_voice(cx));
    on!(m, on_settings_browse_voices, settings_cb, |c, cx| c.browse_voices(cx));
    on!(m, on_settings_reset_voice_settings, settings_cb, |c, cx| c.reset_voice_settings(cx));
    on!(m, on_settings_hotkey_start, settings_cb, |c, cx, action| c.hotkey_start(cx, &action));
    on!(m, on_settings_hotkey_stop, settings_cb, |c, cx| c.hotkey_stop(cx));
    on!(m, on_settings_hotkey_clear, settings_cb, |c, cx, action| c.hotkey_clear(cx, &action));
    on!(m, on_settings_add_excluded, settings_cb, |c, cx, name| c.add_excluded(cx, &name));
    on!(m, on_settings_remove_excluded, settings_cb, |c, cx, name| c.remove_excluded(cx, &name));
    on!(m, on_settings_excluded_edited, settings_cb, |c, cx| c.excluded_edited(cx));
    on!(m, on_settings_clear_cache, settings_cb, |c, cx| c.clear_cache(cx));
    m.on_settings_export_history(|| {
        with_app("settings-export-history", |app| {
            let owner = windowing::hwnd_of(app.main.window());
            app.settings_cb(move |c, cx| c.export_history(cx, owner));
        });
    });
    on!(m, on_settings_clear_history, settings_cb, |c, cx| c.clear_history(cx));
    on!(m, on_settings_port_edited, settings_cb, |c, cx, text| c.port_edited(cx, &text));
    on!(m, on_settings_commit_port, settings_cb, |c, cx, text| c.commit_port(cx, &text));
    on!(m, on_settings_revert_port, settings_cb, |c, cx| c.revert_port(cx));
    on!(m, on_settings_copy_text, settings_cb, |c, cx, text| c.copy_text(cx, &text));
    on!(m, on_settings_show_extension_folder, settings_cb, |c, cx| c.show_extension_folder(cx));
    on!(m, on_settings_open_data_folder, settings_cb, |c, cx| c.open_data_folder(cx));
    on!(m, on_settings_open_notices, settings_cb, |c, cx| c.open_notices(cx));
    on!(m, on_settings_open_link, settings_cb, |c, cx, which| c.open_link(cx, &which));

    // ── onboarding ──
    on!(m, on_onboarding_go_to, onboarding_cb, |c, cx, step| c.go_to(cx, step));
    on!(m, on_onboarding_next, onboarding_cb, |c, cx| c.next(cx));
    on!(m, on_onboarding_back, onboarding_cb, |c, cx| c.back(cx));
    on!(m, on_onboarding_skip, onboarding_cb, |c, cx| c.skip(cx));
    on!(m, on_onboarding_connect_key, onboarding_cb, |c, cx, draft| c.connect_key(cx, &draft));
    on!(m, on_onboarding_key_edited, onboarding_cb, |c, cx| c.key_edited(cx));
    on!(m, on_onboarding_get_key, onboarding_cb, |c, cx| c.get_key(cx));
    on!(m, on_onboarding_replace_key, onboarding_cb, |c, cx| c.replace_key(cx));
    on!(m, on_onboarding_cancel_replace, onboarding_cb, |c, cx| c.cancel_replace(cx));
    on!(m, on_onboarding_choose_voice, onboarding_cb, |c, cx, id| c.choose_voice(cx, &id));
    on!(m, on_onboarding_preview_voice, onboarding_cb, |c, cx, id| c.preview_voice(cx, &id));
    on!(m, on_onboarding_retry_voices, onboarding_cb, |c, cx| c.retry_voices(cx));
    on!(m, on_onboarding_read_sample, onboarding_cb, |c, cx| c.read_sample(cx));
}

/// The `settings-format-number` pure callback: the Settings controller's live slider label (a plain number when the
/// app is busy, which only a re-entrant evaluation could cause).
fn format_number(key: &str, value: f32) -> SharedString {
    let label = APP.with(|cell| {
        let slot = cell.try_borrow().ok()?;
        let app = slot.as_ref()?;
        guard("settings-format-number", || app.settings_page.format_number(key, value))
    });
    label.unwrap_or_else(|| vox_core::js::number_to_string(f64::from(value))).into()
}

/// Every `WidgetWindow` callback (`ui::widget` integration contract).
fn wire_widget(w: &WidgetWindow) {
    w.on_action(|action| with_app("widget action", move |app| app.widget_action(action)));
    w.on_seek(|fraction| {
        with_app("widget seek", move |app| {
            app.widget_ctl.seek(&app.core, &app.snapshot, fraction);
        })
    });
    w.on_error_action(|| {
        with_app("widget error-action", |app| {
            app.widget_ctl.error_action(&app.core, &app.snapshot);
            app.push_widget();
        })
    });
    w.on_warning_clicked(|| with_app("widget warning", |app| app.widget_ctl.warning_clicked(&app.core)));
    w.on_toast_action(|| with_app("widget toast-action", |app| app.widget_toast_action()));
    w.on_drag_start(|| with_app("widget drag", |app| app.widget_drag_start()));
    w.on_size_changed(|width, height| {
        with_app("widget size", move |app| {
            widget_window::resize(&app.widget, &mut app.widget_window, f64::from(width), f64::from(height));
        })
    });
}

/// Every `ChipWindow` callback (`ui::chip` integration contract).
fn wire_chip(c: &ChipWindow) {
    c.on_pressed(|left| {
        with_app("chip press", move |app| {
            let _ = app.chip_ctl.pressed(&app.chip, &app.selection, left);
        })
    });
    c.on_size_changed(|width, height| {
        with_app("chip size", move |app| app.chip_ctl.size_changed(&app.chip, &app.selection, width, height));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::player::{PlayerState, SessionWarning};

    fn snap(status: PlayerStatus, position: f64) -> PlayerSnapshot {
        PlayerSnapshot {
            state: PlayerState { status, position_sec: position, duration_sec: 20.0, ..PlayerState::idle() },
            ..PlayerSnapshot::default()
        }
    }

    #[test]
    fn poll_key_ignores_sub_tenth_position_changes() {
        let a = PollKey::of(&snap(PlayerStatus::Playing, 1.21));
        let b = PollKey::of(&snap(PlayerStatus::Playing, 1.29));
        let c = PollKey::of(&snap(PlayerStatus::Playing, 1.30));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, PollKey::of(&snap(PlayerStatus::Paused, 1.21)));
    }

    #[test]
    fn poll_key_tracks_every_listed_field() {
        let base = snap(PlayerStatus::Playing, 3.0);
        let k = PollKey::of(&base);
        let mut s = base.clone();
        s.state.char_index = 5;
        assert_ne!(PollKey::of(&s), k);
        let mut s = base.clone();
        s.state.buffered_fraction = 0.0004;
        assert_eq!(PollKey::of(&s), k, "below a thousandth");
        s.state.buffered_fraction = 0.002;
        assert_ne!(PollKey::of(&s), k);
        let mut s = base.clone();
        s.warning = Some(SessionWarning { message: "quota".into(), code: vox_core::read::ReadErrorCode::Quota });
        assert_ne!(PollKey::of(&s), k);
        let mut s = base.clone();
        s.state.rate = 1.25;
        assert_ne!(PollKey::of(&s), k);
        let mut s = base.clone();
        s.state.duration_sec = f64::NAN;
        assert_eq!(PollKey::of(&s).duration, 0, "non-finite values never panic");
        let mut s = base;
        s.wants_play = !s.wants_play;
        assert_ne!(PollKey::of(&s), k);
    }

    #[test]
    fn fan_out_rules() {
        assert!(reaches_every_page(&UiUpdate::Refresh(crate::update::RefreshReason::Day)));
        assert!(reaches_every_page(&UiUpdate::Done { req: crate::command::RequestId(1), result: Ok(()) }));
        assert!(reaches_every_page(&UiUpdate::HotkeysResumed));
        assert!(!reaches_every_page(&UiUpdate::HistoryChanged));
        assert!(!reaches_every_page(&UiUpdate::Player(PlayerSnapshot::default())));
        assert!(!reaches_every_page(&UiUpdate::Toast(Toast::info("x"))));
        assert!(touches_widget(&UiUpdate::Toast(Toast::info("x"))));
        assert!(!touches_widget(&UiUpdate::HistoryChanged));
    }

    #[test]
    fn busy_replay_rules() {
        use slint::winit_030::winit::dpi::PhysicalPosition;
        assert!(replays_when_busy(&WinitEvent::Focused(true)));
        assert!(replays_when_busy(&WinitEvent::Moved(PhysicalPosition::new(1, 2))));
        assert!(!replays_when_busy(&WinitEvent::CloseRequested));
        assert!(!replays_when_busy(&WinitEvent::Destroyed));
    }

    #[test]
    fn sinks_collect_in_order() {
        let mut t = ToastSink::default();
        t.push(Toast::info("a"), None);
        t.push(Toast::info("b"), Some(UiToastAction::Page(AppPage::History)));
        t.dismiss("x");
        assert_eq!(t.pushes.len(), 2);
        assert_eq!(t.dismisses, vec!["x".to_string()]);
        let mut n = NavSink::default();
        n.navigate(AppRoute::page(AppPage::Home));
        n.navigate_with(AppRoute::page(AppPage::History), NavIntent::Search("q".into()));
        assert_eq!(n.target, Some((AppRoute::page(AppPage::History), NavIntent::Search("q".into()))));
        let mut d = DialogSink::default();
        d.set_busy("history-clear", true);
        d.close("history-clear");
        assert_eq!(d.busy, Some(("history-clear".into(), true)));
        assert_eq!(d.close.as_deref(), Some("history-clear"));
    }
}
