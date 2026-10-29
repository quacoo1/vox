//! The core thread (docs/ARCHITECTURE.md §3.1): owns the read controller and every service handle, receives
//! [`Command`]s on one channel and publishes [`UiUpdate`]s. It never blocks on the network or on long disk work:
//! catalog requests, key validation, previews, exports, clipboard reads and file extraction run on short-lived worker
//! threads that report back with commands; audio-cache saves, loads and deletions run on the cache's own I/O worker
//! (`AudioCache::{save_async, load_async}`, index-only removals). Timers (retention, hotkey-suspension safety,
//! credits, the history-changed throttle, the retention-change debounce, the output preload) live in
//! [`timers::Timers`] and drive the loop's `select!` timeout.
//!
//! Core-owned state besides the services (each module documents its part): the reader state and tracked sessions,
//! the credits state machine (+ wake throttle), the toast router (`SurfaceState`, `ui_ready`, deferred routes), the
//! hotkey suspension state, `hook_status: Option<bool>` (`None` until the first `SelectionEvent::HookStatus`;
//! `AppInfo.native_hook_available = vox_core::app::native_hook_available(hook_status)`), the catalog's
//! `key_generation`, the retention schedule, the deferred `Cli` queue (until the startup work finished) and the
//! last proxy check time.
//!
//! **Panics** (D-ARCH-3): `dispatch` runs each command under `catch_unwind`. A panicking command is logged (`panic`
//! scope); if it carried a `RequestId` the core still answers it with its failure variant ([`owed_answer`] taken
//! before dispatch, [`failure_update`] after the panic), so no spinner or skeleton waits forever (R-Home). The owed
//! answer is one-shot: `publish` notes when the handler already published the answer for that request id (e.g.
//! `SettingsSaved{Ok}` before the reactions ran), and then no failure follows it. Independent steps of one handler
//! (each startup step, each settings reaction, the tail of a key set/clear) run through [`Core::guarded`], so one
//! panicking step is logged and the steps after it still run.
//!
//! | Module | Spec |
//! |---|---|
//! | [`reader`] | reader-playback.md §2: `read`, `readClipboard`, `replay`, CLI actions, `togglePlayback`, `command`, engine events, cache saves |
//! | [`sessions`] | reader-playback.md §2.12, contracts.md §6.4: tracked sessions and the listening fold-back (pure, tested) |
//! | [`engine_sink`] | the `TtsSink` that forwards to the reading `Player`; engine/player events → `Command` |
//! | [`toasts`] | reader-playback.md §2.11, shell.md §5.3: toast routing (app, widget, OS notification) |
//! | [`reactions`] | shell.md §5.1: what reacts to each settings change |
//! | [`hotkeys`] | shell.md §8: registration plan, OS registration, failure toasts, suspension |
//! | [`selection`] | shell.md §4.1: selection events → reads / toasts; hook status |
//! | [`bridge`] | shell.md §4.2, integrations.md §1.3: `BridgeHandler`, `sync_server` |
//! | [`integrations`] | shell.md §4.3–§4.4: Explorer verb sync, launch at login |
//! | [`retention`] | persistence.md §5.10, shell.md §5.2: startup / 6-hourly / 60 s-after-change runs, cache reconciliation |
//! | [`catalog`] | contracts.md §5.3: voices / models / account / key validation / previews, demo fallbacks |
//! | [`credits`] | ui-main.md §1.5: the credits state machine (moved from the UI store to the core) |
//! | [`media`] | reader-playback.md §9: SMTC state from player reports; media actions |
//! | [`queries`] | history list/export/clear, analytics, cache stats/clear, app info |
//! | [`lifecycle`] | shell.md §3.5–§3.7: quit sequence, session end, close requests, `persistPlayback` |
//! | [`timers`] | the deadline heap |

pub mod bridge;
pub mod catalog;
pub mod credits;
pub mod engine_sink;
pub mod hotkeys;
pub mod integrations;
pub mod lifecycle;
pub mod media;
pub mod notices;
pub mod queries;
pub mod reactions;
pub mod reader;
pub mod retention;
pub mod selection;
pub mod sessions;
pub mod timers;
pub mod toasts;

#[cfg(test)]
mod core_tests;

use std::cell::Cell;
use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use vox_audio::Player;
use vox_bridge::BridgeServer;
use vox_core::app::{AppRoute, Toast, ToastKind};
use vox_core::player::PlayerCommand;
use vox_core::read::{ReadRequest, ReadResult};
use vox_core::settings::{resolve_theme, Settings};
use vox_core::voices::Voice;
use vox_store::{AudioCache, HistoryStore, SettingsStore};
use vox_tts::{ElevenLabsClient, ProxyConfig, TtsEngine};
use vox_win::hotkeys::HotkeyService;
use vox_win::media::MediaSession;
use vox_win::selection::{SelectionConfig, SelectionEvent, SelectionService};
use vox_win::system::SystemEvent;

use crate::command::{BridgeCall, Command, CoreHandle, RequestId, TrayAction, WindowEvent, WindowKind};
use crate::runtime::Runtime;
use crate::update::{RefreshReason, UiSink, UiUpdate, WidgetIntent};

use self::credits::CreditsMachine;
use self::hotkeys::HotkeyState;
use self::reader::{Reader, ReaderState};
use self::timers::{TimerId, Timers};
use self::toasts::{ToastRoute, ToastRouter};

/// Every service handle the core owns (built by `app::run`, moved into the core thread).
pub struct Services {
    pub runtime: Runtime,
    pub settings: SettingsStore,
    pub history: HistoryStore,
    pub cache: AudioCache,
    pub client: ElevenLabsClient,
    pub engine: TtsEngine,
    /// The reading player.
    pub player: Player,
    /// A second player instance for voice samples (one-chunk sessions), so samples never disturb a read's session.
    pub preview_player: Player,
    pub selection: SelectionService,
    /// `None` when the hotkey thread could not start (hotkeys then report every action as in-use).
    pub hotkeys: Option<HotkeyService>,
    pub media: Option<MediaSession>,
    pub bridge: Arc<BridgeServer>,
    /// The `/health` "playing" flag shared with [`bridge::BridgeGlue`] (set on every reading-player report).
    pub bridge_playing: Arc<AtomicBool>,
}

/// The core's state.
pub struct Core {
    services: Services,
    rx: Receiver<Command>,
    handle: CoreHandle,
    ui: UiSink,
    /// Monotonic origin of [`Core::mono_ms`].
    origin: Instant,
    timers: Timers,
    reader: ReaderState,
    router: ToastRouter,
    credits: CreditsMachine,
    hotkey_state: HotkeyState,
    /// `None` until the first `SelectionEvent::HookStatus` (no install attempt yet).
    hook_status: Option<bool>,
    /// Bumped on every API-key change; stamped on catalog jobs (D-USW-23).
    key_generation: u64,
    retention: retention::Schedule,
    /// The main window's last reported `(visible, focused)`.
    main_window: (bool, bool),
    /// When the Windows proxy was last read (D-NET-1).
    last_proxy_check: Option<Instant>,
    /// The last voices list (for sample URLs).
    voices: Option<Arc<Vec<Voice>>>,
    /// The voice sample in progress.
    preview: Option<catalog::PreviewState>,
    /// The quit sequence started (later `Quit`s are ignored).
    quitting: bool,
    /// The loop ends after the current command.
    exit: bool,
    /// The request the command being dispatched owes an answer to, and whether `publish` already sent that answer
    /// (D-ARCH-3: the failure answer after a panic is sent only when it was not).
    owed: Cell<Option<(RequestId, bool)>>,
}

impl Core {
    /// Spawns the core thread; returns its join handle (joined after the event loop ends). The startup work runs
    /// first on that thread, in this order (ARCHITECTURE.md §3.6, app.rs step 8): publish `Settings`, `Theme`, `Motion`
    /// and `AppInfo`; apply `playbackRate`/`volume` to the reading player once (reaction #12 at startup);
    /// `cache.remove_orphans(all history ids)` (index-only); register hotkeys and start the selection service (not in
    /// shots mode); sync the bridge, the Explorer verb (non-forced, not demo) and launch-at-login (not demo); first
    /// credits refresh; arm timers (incl. `PreloadOutput` 1 s, unless `null_output`). Only then are the deferred
    /// `Cli` commands handled, FIFO.
    ///
    /// Panics only if the OS refuses to create a thread; `app::run` uses [`Core::try_spawn`].
    #[allow(dead_code)] // the documented entry point; app::run prefers the fallible form
    pub fn spawn(services: Services, rx: Receiver<Command>, handle: CoreHandle, ui: UiSink) -> JoinHandle<()> {
        match Self::try_spawn(services, rx, handle, ui) {
            Ok(h) => h,
            Err(e) => panic!("cannot start the core thread: {e}"),
        }
    }

    /// [`Core::spawn`] reporting a thread-creation failure.
    pub fn try_spawn(services: Services, rx: Receiver<Command>, handle: CoreHandle, ui: UiSink) -> std::io::Result<JoinHandle<()>> {
        std::thread::Builder::new().name("vox-core".into()).spawn(move || {
            let mut core = Core::new(services, rx, handle, ui);
            if catch_unwind(AssertUnwindSafe(|| core.startup())).is_err() {
                tracing::error!(target: "panic", "core startup panicked; continuing");
            }
            core.run();
        })
    }

    /// The core's state before its startup work (tests drive it directly).
    fn new(services: Services, rx: Receiver<Command>, handle: CoreHandle, ui: UiSink) -> Core {
        Core {
            services,
            rx,
            handle,
            ui,
            origin: Instant::now(),
            timers: Timers::default(),
            reader: ReaderState::default(),
            router: ToastRouter::default(),
            credits: CreditsMachine::default(),
            hotkey_state: HotkeyState::default(),
            hook_status: None,
            key_generation: 0,
            retention: retention::Schedule::default(),
            main_window: (false, false),
            last_proxy_check: Some(Instant::now()),
            voices: None,
            preview: None,
            quitting: false,
            exit: false,
            owed: Cell::new(None),
        }
    }

    /// The startup work (see [`Core::spawn`]). Each step is [`Core::guarded`]: a panic in one never skips the later
    /// ones (the first credits refresh, the timers).
    fn startup(&mut self) {
        let settings = self.settings();
        let shots = self.shots();
        self.guarded("initial state", |core| {
            core.publish(UiUpdate::Settings(Arc::new(settings.clone())));
            core.publish(UiUpdate::Theme { dark: core.resolved_dark(&settings) });
            core.publish(UiUpdate::Motion(vox_win::window::reduce_motion()));
        });
        self.guarded("app info", |core| core.publish_app_info());
        // Reaction #12 once at startup (reader-playback.md §7, D-RP-13).
        self.guarded("player levels", |core| {
            core.services.player.command(PlayerCommand::Rate { rate: settings.playback_rate });
            core.services.player.command(PlayerCommand::Volume { volume: settings.volume });
        });
        // Orphaned cache entries: index-only on this thread, before any queued read can save a new entry (D-P-17).
        self.guarded("cache orphans", |core| {
            let keep: HashSet<String> = core.services.history.with_all(|records| records.iter().map(|r| r.id.clone()).collect());
            let orphans = core.services.cache.remove_orphans(&keep);
            if !orphans.is_empty() {
                tracing::info!(target: "app", "removed {} orphaned audio cache entries", orphans.len());
            }
        });
        if !shots {
            self.guarded("hotkeys", Core::apply_hotkeys);
            self.guarded("selection", |core| core.services.selection.start(SelectionConfig::from(&settings)));
        }
        self.guarded("media keys", |core| {
            if let Some(media) = &core.services.media {
                media.set_enabled(settings.triggers.media_keys);
            }
        });
        self.guarded("bridge", Core::sync_bridge);
        self.guarded("explorer menu", |core| core.sync_explorer_menu(false));
        self.guarded("launch at login", Core::apply_launch_at_login);
        self.guarded("credits", |core| core.refresh_credits(false));
        self.guarded("timers", |core| {
            let now = core.mono_ms();
            core.retention.start(now);
            core.sync_retention_timers();
            core.arm_midnight();
            core.credits.throttle.last_wake_ms = now;
            if !shots {
                core.timers.set(TimerId::PreloadOutput, Instant::now() + Duration::from_millis(vox_core::constants::PRELOAD_OUTPUT_DELAY_MS));
            }
        });
    }

    /// Runs one independent step under `catch_unwind` (D-ARCH-3): a panic is logged (`core step <name> panicked`)
    /// and the caller goes on with its next step.
    fn guarded(&mut self, step: &'static str, f: impl FnOnce(&mut Core)) {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            #[cfg(test)]
            if core_tests::take_step_panic(step) {
                panic!("test: a core step panicked");
            }
            f(self)
        }));
        if outcome.is_err() {
            tracing::error!(target: "panic", "core step {step} panicked");
        }
    }

    /// The loop: `select!` over the command channel and the next timer deadline; dispatch; exit after the quit
    /// sequence published `UiUpdate::Quit`.
    fn run(&mut self) {
        while !self.exit {
            let next = match self.timers.next_deadline() {
                Some(at) => match self.rx.recv_deadline(at) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match self.rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => break,
                },
            };
            if let Some(cmd) = next {
                self.dispatch(cmd);
            }
            if !self.exit {
                self.fire_due_timers();
            }
        }
        tracing::info!(target: "app", "core stopped");
    }

    /// One command, under `catch_unwind` (module docs "Panics").
    fn dispatch(&mut self, cmd: Command) {
        let owed = owed_answer(&cmd);
        let name = command_name(&cmd);
        self.owed.set(owed.map(|o| (o.req(), false)));
        let outcome = catch_unwind(AssertUnwindSafe(|| self.handle(cmd)));
        let answered = matches!(self.owed.take(), Some((_, true)));
        if let Err(payload) = outcome {
            // Only static panic messages are logged: a formatted one could carry read text.
            let detail = payload.downcast_ref::<&'static str>().copied().unwrap_or("(message not logged)");
            tracing::error!(target: "panic", "core command {name} panicked: {detail}");
            match owed {
                Some(_) if answered => tracing::debug!(target: "panic", "core command {name} was already answered"),
                Some(owed) => self.publish(failure_update(owed)),
                None => {}
            }
        }
    }

    /// Runs every due timer (each under `catch_unwind`).
    fn fire_due_timers(&mut self) {
        for id in self.timers.take_due(Instant::now()) {
            if catch_unwind(AssertUnwindSafe(|| self.on_timer(id))).is_err() {
                tracing::error!(target: "panic", "core timer {id:?} panicked");
            }
        }
    }

    fn handle(&mut self, cmd: Command) {
        if self.quitting {
            // Only the quit sequence's own drain reaches the handlers after the quit started.
            return;
        }
        #[cfg(test)]
        if core_tests::take_panic_request() {
            panic!("test: a request handler panicked");
        }
        #[cfg(test)]
        let panic_after = core_tests::take_panic_after_request();
        self.handle_command(cmd);
        #[cfg(test)]
        if panic_after {
            panic!("test: a request handler panicked after answering");
        }
    }

    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            // ── reading ──
            Command::ReadText { req, text, options } => {
                let result = self.read(options.into_request(&text));
                self.answer_read(req, result);
            }
            Command::ReadClipboard { req } => self.read_clipboard(req),
            Command::ClipboardText { req, text } => self.on_clipboard_text(req, text),
            Command::Replay { req, read_id } => self.replay(req, &read_id, crate::command::ReplayOrigin::Request),
            Command::ReplayLoaded { req, read_id, origin, chunks } => self.on_replay_loaded(req, &read_id, origin, chunks),
            Command::ReadSelection(source) => self.services.selection.read_selection(source),
            Command::ReadPrepared(request) => {
                self.read(request);
            }
            Command::TogglePlayback => self.toggle_playback(),
            Command::Player(cmd) => self.command(cmd),
            Command::CycleRate => self.cycle_rate(),

            // ── settings ──
            Command::UpdateSettings { req, patch } => self.update_settings(req, &patch),
            Command::SetApiKey { req, key } => self.set_api_key(req, key),
            Command::ClearApiKey { req } => self.clear_api_key(req),
            Command::ConnectProvider { req, provider, creds } => self.connect_provider(req, provider, creds),
            Command::DisconnectProvider { req, provider } => self.disconnect_provider(req, provider),
            Command::SuspendHotkeys(on) => self.suspend_hotkeys(on),

            // ── catalog / credits / previews ──
            Command::FetchVoices { refresh } => self.fetch_voices(refresh),
            Command::FetchModels { refresh } => self.fetch_models(refresh),
            Command::RefreshCredits { force } => self.refresh_credits(force),
            Command::PreviewVoice { voice_id } => self.preview_voice(voice_id),
            Command::StopPreview => self.stop_preview(),

            // ── history / analytics / cache ──
            Command::QueryHistory { req, query, with_grand_total } => self.query_history(req, &query, with_grand_total),
            Command::QueryHome { req } => self.query_home(req),
            Command::QueryAnalytics { req, range } => self.query_analytics(req, range),
            Command::RemoveReads { req, ids } => self.remove_reads(req, &ids),
            Command::ClearHistory { req } => self.clear_history(req),
            Command::ToggleFavorite { req, id } => self.toggle_favorite(req, &id),
            Command::ExportHistory { req, owner } => self.export_history(req, owner),
            Command::CountExpiring { req, days } => self.count_expiring(req, days),
            Command::QueryCacheStats { req } => self.publish(UiUpdate::CacheStats { req, result: Ok(self.services.cache.stats()) }),
            Command::ClearCache { req } => self.clear_cache(req),
            Command::CopyText { req, text } => self.copy_text(req, text),
            Command::SaveAnalyticsRange(range) => {
                vox_store::ui_state::save_ui_state(&self.services.runtime.profile, &vox_store::ui_state::UiState { analytics_range: range })
            }
            Command::SaveWindowState(state) => vox_store::window_state::save_window_state(&self.services.runtime.profile, &state),

            // ── app shell ──
            Command::ShowMain(route) => {
                let route = route.map(|r| AppRoute::validated(r.page, r.section.as_deref()));
                self.publish(UiUpdate::ShowMain(route));
            }
            Command::HideMain => self.publish(UiUpdate::HideMain),
            Command::OpenExternal { url, req } => self.open_external(url, req),
            Command::RevealPath(target) => self.reveal_path(target),
            Command::QueryAppInfo => self.publish_app_info(),
            Command::Quit => self.shutdown(),

            // ── sources ──
            Command::Window(event) => self.on_window_event(event),
            Command::Tray(action) => self.on_tray(action),
            Command::Hotkey(action) => self.on_hotkey(action),
            Command::Selection(event) => self.on_selection(event),
            Command::Media(action) => self.command(media::media_command(action)),
            Command::Bridge(BridgeCall::Read { request, reply }) => {
                let foreground = vox_win::foreground::foreground_info();
                let result = self.read(request.into_request(foreground.as_ref()));
                let _ = reply.send(result);
            }
            Command::Cli { argv, cwd, from_second_instance } => self.on_cli(&argv, &cwd, from_second_instance),
            Command::System(event) => self.on_system(event),
            Command::SessionEnd(reply) => {
                self.session_end();
                let _ = reply.send(());
            }
            Command::Toast(toast) => self.toast(toast),

            // ── internal ──
            Command::Engine(event) => self.on_engine_event(event),
            Command::CacheSaved { read_id, result } => self.on_cache_saved(&read_id, result),
            Command::PlayerReport(snapshot) => self.on_player_report(snapshot),
            Command::PreviewReport(snapshot) => self.on_preview_report(&snapshot),
            Command::PreviewFailed { message } => self.on_preview_failed(&message),
            Command::PlayerFailed { session_id, message } => self.on_player_failed(session_id, &message),
            Command::HistoryChanged => {
                if !self.timers.is_set(TimerId::HistoryChangedThrottle) {
                    self.timers.set(
                        TimerId::HistoryChangedThrottle,
                        Instant::now() + Duration::from_millis(vox_core::constants::HISTORY_CHANGED_THROTTLE_MS),
                    );
                }
            }
            Command::Catalog(result) => self.on_catalog(result),
        }
    }

    fn on_timer(&mut self, id: TimerId) {
        match id {
            TimerId::RetentionInterval | TimerId::RetentionChange => {
                if let Some(run) = self.retention.due(self.mono_ms()) {
                    let days = self.settings().library.retention_days;
                    tracing::debug!(target: "app", "retention run ({run:?})");
                    retention::apply_retention(&self.services.history, days, now_unix_ms());
                }
                self.sync_retention_timers();
            }
            TimerId::HotkeySuspend => {
                let effects = self.hotkey_state.on_timer();
                self.apply_hotkey_effects(effects);
            }
            TimerId::HistoryChangedThrottle => self.publish(UiUpdate::HistoryChanged),
            TimerId::CreditsAfterRead => self.refresh_credits(true),
            TimerId::CreditsReset => self.check_credits_reset(),
            TimerId::Midnight => {
                self.publish(UiUpdate::Refresh(RefreshReason::Day));
                self.arm_midnight();
            }
            TimerId::MediaTick => self.media_tick(),
            TimerId::PreloadOutput => self.services.player.preload_output(),
        }
    }

    // ───────────────────────────── shared helpers ─────────────────────────────

    /// Milliseconds since the core started (monotonic).
    fn mono_ms(&self) -> f64 {
        self.origin.elapsed().as_secs_f64() * 1000.0
    }

    fn settings(&self) -> Settings {
        self.services.settings.get()
    }

    fn shots(&self) -> bool {
        self.services.runtime.flags.shots()
    }

    fn demo(&self) -> bool {
        self.services.runtime.flags.demo
    }

    fn publish(&self, update: UiUpdate) {
        if let Some((req, false)) = self.owed.get() {
            if answered_request(&update) == Some(req) {
                self.owed.set(Some((req, true)));
            }
        }
        self.ui.send(update);
    }

    /// The `ReadStarted` answer of a read request (nothing without a request id).
    fn answer_read(&self, req: Option<RequestId>, result: ReadResult) {
        if let Some(req) = req {
            self.publish(UiUpdate::ReadStarted { req, result });
        }
    }

    /// The resolved main-window theme (`system` follows `AppsUseLightTheme`).
    fn resolved_dark(&self, settings: &Settings) -> bool {
        resolve_theme(settings.app.theme, vox_win::registry::apps_use_light_theme() == Some(false)).is_dark()
    }

    fn publish_app_info(&self) {
        let providers = queries::provider_statuses(&self.services.engine);
        let info = queries::build_app_info(&self.services.runtime, &self.services.settings, &self.services.bridge, self.hook_status, providers);
        self.publish(UiUpdate::AppInfo(Arc::new(info)));
    }

    /// A core toast (reader-playback.md §2.11): published once for the app stack and the widget, then routed.
    fn toast(&mut self, toast: Toast) {
        self.publish(UiUpdate::Toast(toast.clone()));
        let widget_enabled = self.settings().widget.enabled;
        if let Some(route) = self.router.on_toast(&toast, widget_enabled) {
            self.apply_toast_route(&toast, route);
        }
    }

    fn apply_toast_route(&mut self, toast: &Toast, route: ToastRoute) {
        match route {
            ToastRoute::AppOnly | ToastRoute::Nowhere => {}
            ToastRoute::ShowWidget => self.publish(UiUpdate::Widget(WidgetIntent::Show { reposition: false })),
            ToastRoute::OsNotification => {
                let shown = catch_unwind(|| vox_win::shell::show_notification(vox_core::constants::APP_NAME, &toast.message));
                let error = match shown {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(_) => Some("the notification call panicked".to_string()),
                };
                if let Some(e) = error {
                    if !self.router.notify_failure_logged {
                        self.router.notify_failure_logged = true;
                        tracing::warn!(target: "notify", "could not show a Windows notification: {e}");
                    }
                }
            }
        }
    }

    /// Re-reads the Windows proxy when the last check is ≥ 30 s old (or `force`), applying a change (D-NET-1).
    fn maybe_recheck_proxy(&mut self, force: bool) {
        let stale = self.last_proxy_check.is_none_or(|t| t.elapsed() >= Duration::from_millis(vox_core::constants::PROXY_RECHECK_MS));
        if !force && !stale {
            return;
        }
        self.last_proxy_check = Some(Instant::now());
        let proxy = system_proxy_config();
        if proxy != self.services.client.proxy() {
            tracing::info!(target: "net", "system proxy changed ({})", if proxy.is_some() { "proxy" } else { "direct" });
            self.services.client.set_proxy(proxy);
        }
    }

    // ───────────────────────────── settings ─────────────────────────────

    /// `UpdateSettings` (D-ARCH-4): `Settings` always, then `SettingsSaved{req}`, then the reactions of an effective
    /// change.
    fn update_settings(&mut self, req: Option<RequestId>, patch: &serde_json::Value) {
        if !patch.is_object() {
            tracing::warn!(target: "settings", "ignored a settings patch that is not an object");
            if let Some(req) = req {
                self.publish(UiUpdate::SettingsSaved { req, result: Err("Invalid settings patch".into()) });
            }
            return;
        }
        let prev = self.settings();
        let next = self.services.settings.update(patch);
        self.publish(UiUpdate::Settings(Arc::new(next.clone())));
        if let Some(req) = req {
            self.publish(UiUpdate::SettingsSaved { req, result: Ok(()) });
        }
        if next != prev {
            self.react(&next, &prev, false);
        }
    }

    /// The settings-change dispatcher (shell.md §5.1; reactions.rs). Every reaction is [`Core::guarded`], so a panic
    /// in one (a service call) never skips the others (e.g. `Widget(Hide)` or the key-change refetches).
    fn react(&mut self, next: &Settings, prev: &Settings, key_replaced: bool) {
        let r = reactions::diff(next, prev, key_replaced, self.shots());
        if r.hotkeys {
            self.guarded("hotkeys reaction", Core::apply_hotkeys);
        }
        if r.selection {
            self.guarded("selection reaction", |core| core.services.selection.apply_settings(SelectionConfig::from(next)));
        }
        if r.bridge {
            self.guarded("bridge reaction", Core::sync_bridge);
        }
        if r.explorer_menu {
            self.guarded("explorer menu reaction", |core| core.sync_explorer_menu(true));
        }
        if r.launch_at_login {
            self.guarded("launch at login reaction", Core::apply_launch_at_login);
        }
        if r.theme {
            self.guarded("theme reaction", |core| core.publish(UiUpdate::Theme { dark: core.resolved_dark(next) }));
        }
        if r.hide_widget {
            self.guarded("widget reaction", |core| core.publish(UiUpdate::Widget(WidgetIntent::Hide)));
        } else if r.reposition_widget {
            self.guarded("widget reaction", |core| core.publish(UiUpdate::Widget(WidgetIntent::Reposition)));
        }
        if r.retention {
            self.guarded("retention reaction", |core| {
                core.retention.on_change(core.mono_ms());
                core.sync_retention_timers();
            });
        }
        if r.prune_cache {
            self.guarded("cache limit reaction", |core| {
                let evicted = core.prune_cache(&[]);
                if !evicted.is_empty() {
                    tracing::info!(target: "app", "cache limit lowered: evicted {} cached reads", evicted.len());
                }
            });
        }
        if r.media_keys {
            self.guarded("media keys reaction", |core| {
                if let Some(media) = &core.services.media {
                    media.set_enabled(next.triggers.media_keys);
                    media.update(media::media_state(&core.services.player.snapshot(), next.triggers.media_keys));
                }
            });
        }
        if r.player_rate {
            self.guarded("rate reaction", |core| core.services.player.command(PlayerCommand::Rate { rate: next.playback_rate }));
        }
        if r.player_volume {
            self.guarded("volume reaction", |core| {
                core.services.player.command(PlayerCommand::Volume { volume: next.volume });
                if core.preview.is_some() {
                    core.services.preview_player.command(PlayerCommand::Volume { volume: next.volume });
                }
            });
        }
        if r.key_changed {
            // The generation first: older catalog answers are dropped even if a refetch below panics.
            self.key_generation += 1;
            self.guarded("credits reaction", |core| core.refresh_credits(true));
            self.guarded("voices reaction", |core| core.fetch_voices(false));
            self.guarded("models reaction", |core| core.fetch_models(false));
            self.guarded("app info reaction", |core| core.publish_app_info());
        }
    }

    // ───────────────────────────── hotkeys ─────────────────────────────

    /// Registration (shell.md §8.5): not in shots mode, not while suspended; failures → warn line + toast.
    fn apply_hotkeys(&mut self) {
        if self.shots() || self.hotkey_state.suspended {
            return;
        }
        let settings = self.settings();
        let plan = vox_core::hotkey::plan_registration(&settings.triggers, vox_win::hotkeys::alt_gr_character);
        let failures = match &self.services.hotkeys {
            Some(service) => service.register(&plan),
            None => hotkeys::failures_without_service(&plan),
        };
        if let Some(message) = vox_core::hotkey::describe_failures(&failures) {
            tracing::warn!(target: "shortcuts", "hotkeys not registered: {}", vox_core::hotkey::failures_log_line(&failures));
            self.toast(Toast::error(message).with_settings_action("triggers"));
        } else if settings.triggers.hotkeys_enabled {
            tracing::info!(target: "shortcuts", "global hotkeys registered");
        }
    }

    fn suspend_hotkeys(&mut self, on: bool) {
        let effects = if on { self.hotkey_state.suspend() } else { self.hotkey_state.resume() };
        if on && effects.unregister {
            tracing::debug!(target: "shortcuts", "hotkeys suspended while recording");
        }
        self.apply_hotkey_effects(effects);
    }

    fn apply_hotkey_effects(&mut self, effects: hotkeys::HotkeyEffects) {
        if effects.unregister {
            if let Some(service) = &self.services.hotkeys {
                service.unregister_all();
            }
        }
        if effects.arm_timer {
            self.timers.set(TimerId::HotkeySuspend, Instant::now() + Duration::from_millis(vox_core::constants::SHORTCUT_SUSPEND_MAX_MS));
        }
        if effects.cancel_timer {
            self.timers.cancel(TimerId::HotkeySuspend);
        }
        if effects.register {
            self.apply_hotkeys();
        }
        if effects.notify_resumed {
            self.publish(UiUpdate::HotkeysResumed);
        }
    }

    fn on_hotkey(&mut self, action: vox_core::hotkey::HotkeyAction) {
        use vox_core::hotkey::HotkeyAction;
        match action {
            HotkeyAction::ReadSelection => self.services.selection.read_selection(vox_core::history::ReadSource::Hotkey),
            HotkeyAction::ReadClipboard => self.read_clipboard(None),
            HotkeyAction::PlayPause => self.toggle_playback(),
            HotkeyAction::Stop => self.command(PlayerCommand::Stop),
        }
    }

    // ───────────────────────────── sources ─────────────────────────────

    fn on_window_event(&mut self, event: WindowEvent) {
        match event {
            WindowEvent::MainState { visible, focused } => {
                let (was_visible, was_focused) = self.main_window;
                self.main_window = (visible, focused);
                let widget_enabled = self.settings().widget.enabled;
                for (toast, route) in self.router.on_main_state(visible, focused, widget_enabled) {
                    self.apply_toast_route(&toast, route);
                }
                // Wake (ui-main.md §1.5): hidden → visible, or unfocused → focused while visible.
                if visible && (!was_visible || (focused && !was_focused)) {
                    self.on_main_window_wake();
                }
            }
            WindowEvent::WidgetVisible(visible) => self.router.on_widget_visible(visible),
            WindowEvent::WidgetDragged { x, y } => {
                let patch = serde_json::json!({ "widget": { "position": "remember", "customX": x, "customY": y } });
                self.update_settings(None, &patch);
            }
            WindowEvent::CloseRequested(kind) => {
                let name = match kind {
                    WindowKind::Main => "main",
                    WindowKind::Widget => "widget",
                    WindowKind::Chip => "chip",
                };
                if kind == WindowKind::Main {
                    // The main window's close is decided on the UI thread (close to tray / Quit).
                    tracing::debug!(target: "windows", "ignored a main-window close request forwarded to the core");
                    return;
                }
                tracing::info!(target: "windows", "{name} window got a close request; quitting");
                self.session_end();
                self.shutdown();
            }
        }
    }

    fn on_tray(&mut self, action: TrayAction) {
        let set = |path: &str, value: bool| vox_core::settings::patch_at(path, serde_json::Value::Bool(value));
        match action {
            TrayAction::ReadClipboard => self.read_clipboard(None),
            TrayAction::TogglePause => self.command(PlayerCommand::Toggle),
            TrayAction::Stop => self.command(PlayerCommand::Stop),
            TrayAction::SetRightClickChip(on) => self.update_settings(None, &set("triggers.rightClickChip", on)),
            TrayAction::SetWidgetEnabled(on) => self.update_settings(None, &set("widget.enabled", on)),
            TrayAction::SetHotkeysEnabled(on) => self.update_settings(None, &set("triggers.hotkeysEnabled", on)),
            TrayAction::Quit => self.shutdown(),
            // Handled on the UI thread (the tray click's foreground permission); a stray one just shows the window.
            TrayAction::Open => self.publish(UiUpdate::ShowMain(None)),
            TrayAction::OpenPage(page) => self.publish(UiUpdate::ShowMain(Some(AppRoute::page(page)))),
        }
    }

    fn on_selection(&mut self, event: SelectionEvent) {
        match event {
            SelectionEvent::Read { text, source, foreground } => {
                self.read(selection::request_for(text, source, foreground.as_ref()));
            }
            SelectionEvent::NoSelection(_) => self.toast(Toast::info(vox_core::copy::SELECT_SOME_TEXT)),
            SelectionEvent::Notice { kind, message } => {
                let kind = match kind {
                    vox_win::selection::NoticeKind::Info => ToastKind::Info,
                    vox_win::selection::NoticeKind::Error => ToastKind::Error,
                };
                self.toast(Toast::new(kind, message));
            }
            SelectionEvent::HookStatus(available) => {
                if available {
                    tracing::info!(target: "selection", "native input hook available");
                } else {
                    tracing::warn!(target: "selection", "native input hook unavailable: chips are disabled, hotkeys/extension/tray still work");
                }
                self.hook_status = Some(available);
                self.publish_app_info();
            }
            // Normally routed straight to the UI by app.rs; forwarded if one arrives here anyway.
            SelectionEvent::ChipShow { x, y, mode } => self.publish(UiUpdate::ChipShow { x, y, mode }),
            SelectionEvent::ChipHide => self.publish(UiUpdate::ChipHide),
        }
    }

    /// `handleArgs` (shell.md §2.2): one action (`--read-text` > `--read-clipboard` > `--read-file`); a second instance
    /// that acted on nothing and is not `--hidden` shows the main window.
    fn on_cli(&mut self, argv: &[String], cwd: &std::path::Path, from_second_instance: bool) {
        use vox_core::cli::CliAction;
        let args = vox_core::cli::parse_cli_args(argv);
        let (action, show_main) = vox_core::cli::cli_outcome(&args, from_second_instance);
        match action {
            Some(CliAction::ReadText(text)) => {
                self.read(ReadRequest::new(text, vox_core::history::ReadSource::Cli));
            }
            Some(CliAction::ReadClipboard) => self.read_clipboard(None),
            Some(CliAction::ReadFile(path)) => self.read_file(&path, cwd),
            None => {}
        }
        if show_main {
            self.publish(UiUpdate::ShowMain(None));
        }
    }

    fn on_system(&mut self, event: SystemEvent) {
        match event {
            SystemEvent::DisplaysChanged => {
                self.publish(UiUpdate::ChipHide);
                self.publish(UiUpdate::Widget(WidgetIntent::Reposition));
                self.publish(UiUpdate::DisplaysChanged);
            }
            SystemEvent::ThemeChanged => {
                let settings = self.settings();
                if settings.app.theme == vox_core::settings::ThemeMode::System {
                    self.publish(UiUpdate::Theme { dark: self.resolved_dark(&settings) });
                }
            }
            SystemEvent::LocaleChanged => {
                vox_core::locale::set_process_locale(vox_win::system::user_locale());
                self.publish(UiUpdate::Refresh(RefreshReason::Locale));
            }
            SystemEvent::MotionChanged => self.publish(UiUpdate::Motion(vox_win::window::reduce_motion())),
            SystemEvent::ClockChanged | SystemEvent::Resumed => {
                if event == SystemEvent::Resumed {
                    self.maybe_recheck_proxy(true);
                }
                self.arm_midnight();
                self.check_credits_reset();
                self.publish(UiUpdate::Refresh(RefreshReason::Day));
            }
            SystemEvent::CloseRequested => {
                tracing::info!(target: "app", "system window got a close request; quitting");
                self.session_end();
                self.shutdown();
            }
        }
    }

    // ───────────────────────────── timers ─────────────────────────────

    fn sync_retention_timers(&mut self) {
        let at = |core: &Core, ms: f64| core.origin + Duration::from_secs_f64(ms.max(0.0) / 1000.0);
        match self.retention.next_interval_ms {
            Some(ms) => self.timers.set(TimerId::RetentionInterval, at(self, ms)),
            None => self.timers.cancel(TimerId::RetentionInterval),
        }
        match self.retention.change_due_ms {
            Some(ms) => self.timers.set(TimerId::RetentionChange, at(self, ms)),
            None => self.timers.cancel(TimerId::RetentionChange),
        }
    }

    /// Local midnight + 1 s → `Refresh(Day)` (re-armed after it fires, a clock change or a resume).
    fn arm_midnight(&mut self) {
        let now = chrono::Local::now();
        let tomorrow = now.date_naive().succ_opt().unwrap_or(now.date_naive());
        let midnight = vox_core::time::local_midnight(&chrono::Local, tomorrow);
        let wait = (midnight - now).to_std().unwrap_or(Duration::ZERO) + Duration::from_secs(1);
        self.timers.set(TimerId::Midnight, Instant::now() + wait);
    }

    fn media_tick(&mut self) {
        let snapshot = self.services.player.snapshot();
        if let Some(media) = &self.services.media {
            media.update(media::media_state(&snapshot, self.settings().triggers.media_keys));
        }
        if snapshot.state.status == vox_core::player::PlayerStatus::Playing {
            self.timers.set(TimerId::MediaTick, Instant::now() + Duration::from_secs(1));
        }
    }
}

/// Epoch milliseconds now (wall clock).
fn now_unix_ms() -> f64 {
    vox_core::time::to_ms(&chrono::Utc::now())
}

/// Runs `job` on a short-lived, named core worker thread (panics are caught and logged; a job that owes an answer
/// catches its own panics and answers with its failure variant). `false` when the OS refused the thread (logged).
pub(crate) fn spawn_worker(name: &'static str, job: impl FnOnce() + Send + 'static) -> bool {
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(job)).is_err() {
            tracing::error!(target: "panic", "core worker {name} panicked");
        }
    });
    match spawned {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(target: "app", "cannot start the {name} worker: {e}");
            false
        }
    }
}

/// The Windows proxy as the client's config (D-NET-1).
pub(crate) fn system_proxy_config() -> Option<ProxyConfig> {
    vox_win::registry::system_proxy().map(|p| ProxyConfig { url: p.url, bypass: p.bypass, bypass_local: p.bypass_local })
}

/// A short name for logs (never a payload).
fn command_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::ReadText { .. } => "ReadText",
        Command::ReadClipboard { .. } => "ReadClipboard",
        Command::ClipboardText { .. } => "ClipboardText",
        Command::Replay { .. } => "Replay",
        Command::ReplayLoaded { .. } => "ReplayLoaded",
        Command::ReadSelection(_) => "ReadSelection",
        Command::ReadPrepared(_) => "ReadPrepared",
        Command::TogglePlayback => "TogglePlayback",
        Command::Player(_) => "Player",
        Command::CycleRate => "CycleRate",
        Command::UpdateSettings { .. } => "UpdateSettings",
        Command::SetApiKey { .. } => "SetApiKey",
        Command::ClearApiKey { .. } => "ClearApiKey",
        Command::ConnectProvider { .. } => "ConnectProvider",
        Command::DisconnectProvider { .. } => "DisconnectProvider",
        Command::SuspendHotkeys(_) => "SuspendHotkeys",
        Command::FetchVoices { .. } => "FetchVoices",
        Command::FetchModels { .. } => "FetchModels",
        Command::RefreshCredits { .. } => "RefreshCredits",
        Command::PreviewVoice { .. } => "PreviewVoice",
        Command::StopPreview => "StopPreview",
        Command::QueryHistory { .. } => "QueryHistory",
        Command::QueryHome { .. } => "QueryHome",
        Command::QueryAnalytics { .. } => "QueryAnalytics",
        Command::RemoveReads { .. } => "RemoveReads",
        Command::ClearHistory { .. } => "ClearHistory",
        Command::ToggleFavorite { .. } => "ToggleFavorite",
        Command::ExportHistory { .. } => "ExportHistory",
        Command::CountExpiring { .. } => "CountExpiring",
        Command::QueryCacheStats { .. } => "QueryCacheStats",
        Command::ClearCache { .. } => "ClearCache",
        Command::CopyText { .. } => "CopyText",
        Command::SaveAnalyticsRange(_) => "SaveAnalyticsRange",
        Command::SaveWindowState(_) => "SaveWindowState",
        Command::ShowMain(_) => "ShowMain",
        Command::HideMain => "HideMain",
        Command::OpenExternal { .. } => "OpenExternal",
        Command::RevealPath(_) => "RevealPath",
        Command::QueryAppInfo => "QueryAppInfo",
        Command::Quit => "Quit",
        Command::Window(_) => "Window",
        Command::Tray(_) => "Tray",
        Command::Hotkey(_) => "Hotkey",
        Command::Selection(_) => "Selection",
        Command::Media(_) => "Media",
        Command::Bridge(_) => "Bridge",
        Command::Cli { .. } => "Cli",
        Command::System(_) => "System",
        Command::SessionEnd(_) => "SessionEnd",
        Command::Toast(_) => "Toast",
        Command::Engine(_) => "Engine",
        Command::CacheSaved { .. } => "CacheSaved",
        Command::PlayerReport(_) => "PlayerReport",
        Command::PreviewReport(_) => "PreviewReport",
        Command::PreviewFailed { .. } => "PreviewFailed",
        Command::PlayerFailed { .. } => "PlayerFailed",
        Command::HistoryChanged => "HistoryChanged",
        Command::Catalog(_) => "Catalog",
    }
}

/// The answer a request command is owed (D-ARCH-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwedAnswer {
    ReadStarted(RequestId),
    Home(RequestId),
    Analytics(RequestId),
    HistoryPage(RequestId),
    SettingsSaved(RequestId),
    ApiKey(RequestId),
    Done(RequestId),
    Exported(RequestId),
    CacheStats(RequestId),
    ExpiringCount(RequestId),
    FavoriteToggled(RequestId),
}

impl OwedAnswer {
    /// The request this answer is for.
    pub fn req(self) -> RequestId {
        match self {
            OwedAnswer::ReadStarted(r)
            | OwedAnswer::Home(r)
            | OwedAnswer::Analytics(r)
            | OwedAnswer::HistoryPage(r)
            | OwedAnswer::SettingsSaved(r)
            | OwedAnswer::ApiKey(r)
            | OwedAnswer::Done(r)
            | OwedAnswer::Exported(r)
            | OwedAnswer::CacheStats(r)
            | OwedAnswer::ExpiringCount(r)
            | OwedAnswer::FavoriteToggled(r) => r,
        }
    }
}

/// The request id an update answers (`None` for updates that answer no request).
pub fn answered_request(update: &UiUpdate) -> Option<RequestId> {
    match update {
        UiUpdate::SettingsSaved { req, .. }
        | UiUpdate::HistoryPage { req, .. }
        | UiUpdate::Home { req, .. }
        | UiUpdate::Analytics { req, .. }
        | UiUpdate::ReadStarted { req, .. }
        | UiUpdate::ApiKey { req, .. }
        | UiUpdate::Done { req, .. }
        | UiUpdate::CacheStats { req, .. }
        | UiUpdate::Exported { req, .. }
        | UiUpdate::ExpiringCount { req, .. }
        | UiUpdate::FavoriteToggled { req, .. } => Some(*req),
        _ => None,
    }
}

/// Which answer `cmd` owes its requester (`None` for commands without a request id). Taken before dispatch. A key
/// validation owes `ApiKey{req}` on both hops: `SetApiKey` (the length guard, the worker start) and the worker's
/// `Catalog(ApiKey{req})` (saving the key, the reactions, the answer).
pub fn owed_answer(cmd: &Command) -> Option<OwedAnswer> {
    use Command as C;
    use OwedAnswer as A;
    Some(match cmd {
        C::ReadText { req: Some(r), .. }
        | C::ReadClipboard { req: Some(r) }
        | C::ClipboardText { req: Some(r), .. }
        | C::Replay { req: Some(r), .. }
        | C::ReplayLoaded { req: Some(r), .. } => A::ReadStarted(*r),
        C::QueryHome { req } => A::Home(*req),
        C::QueryAnalytics { req, .. } => A::Analytics(*req),
        C::QueryHistory { req, .. } => A::HistoryPage(*req),
        C::UpdateSettings { req: Some(r), .. } => A::SettingsSaved(*r),
        C::SetApiKey { req, .. } | C::Catalog(crate::command::CatalogResult::ApiKey { req, .. }) => A::ApiKey(*req),
        C::ConnectProvider { req, .. } | C::DisconnectProvider { req, .. } | C::Catalog(crate::command::CatalogResult::Provider { req, .. }) => A::Done(*req),
        C::ClearApiKey { req } | C::RemoveReads { req, .. } | C::ClearHistory { req } | C::CopyText { req, .. } => A::Done(*req),
        C::OpenExternal { req: Some(r), .. } => A::Done(*r),
        C::ExportHistory { req, .. } => A::Exported(*req),
        C::QueryCacheStats { req } | C::ClearCache { req } => A::CacheStats(*req),
        C::CountExpiring { req, .. } => A::ExpiringCount(*req),
        C::ToggleFavorite { req, .. } => A::FavoriteToggled(*req),
        _ => return None,
    })
}

/// The failure answer for a command whose handler panicked (`Something went wrong`; reads answer
/// `ReadResult::internal_error()`).
pub fn failure_update(owed: OwedAnswer) -> UiUpdate {
    let msg = || vox_core::copy::SOMETHING_WENT_WRONG.to_string();
    match owed {
        OwedAnswer::ReadStarted(req) => UiUpdate::ReadStarted { req, result: ReadResult::internal_error() },
        OwedAnswer::Home(req) => UiUpdate::Home { req, result: Err(msg()) },
        OwedAnswer::Analytics(req) => UiUpdate::Analytics { req, result: Err(msg()) },
        OwedAnswer::HistoryPage(req) => UiUpdate::HistoryPage { req, result: Err(msg()) },
        OwedAnswer::SettingsSaved(req) => UiUpdate::SettingsSaved { req, result: Err(msg()) },
        OwedAnswer::ApiKey(req) => UiUpdate::ApiKey {
            req,
            result: vox_core::voices::ApiKeyResult { ok: false, error: Some(msg()), warning: None, account: None },
        },
        OwedAnswer::Done(req) => UiUpdate::Done { req, result: Err(msg()) },
        OwedAnswer::Exported(req) => UiUpdate::Exported { req, result: Err(msg()) },
        OwedAnswer::CacheStats(req) => UiUpdate::CacheStats { req, result: Err(msg()) },
        OwedAnswer::ExpiringCount(req) => UiUpdate::ExpiringCount { req, result: Err(msg()) },
        OwedAnswer::FavoriteToggled(req) => UiUpdate::FavoriteToggled { req, result: Err(msg()) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_commands_owe_their_failure_variant() {
        let r = RequestId(7);
        let owed = owed_answer(&Command::QueryHome { req: r }).unwrap();
        assert!(matches!(failure_update(owed), UiUpdate::Home { req, result: Err(_) } if req == r));
        let owed = owed_answer(&Command::ReadClipboard { req: Some(r) }).unwrap();
        match failure_update(owed) {
            UiUpdate::ReadStarted { req, result } => {
                assert_eq!(req, r);
                assert_eq!(result.code, Some(vox_core::read::ReadErrorCode::Unknown));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(owed_answer(&Command::ReadClipboard { req: None }).is_none());
        assert!(owed_answer(&Command::HistoryChanged).is_none());
        assert_eq!(owed_answer(&Command::CountExpiring { req: r, days: 7 }), Some(OwedAnswer::ExpiringCount(r)));
    }

    #[test]
    fn both_hops_of_a_key_validation_owe_the_api_key_answer() {
        let r = RequestId(9);
        assert_eq!(owed_answer(&Command::SetApiKey { req: r, key: "k".into() }), Some(OwedAnswer::ApiKey(r)));
        let validated = crate::command::CatalogResult::ApiKey {
            req: r,
            key: "k".into(),
            result: vox_core::voices::ApiKeyResult { ok: true, error: None, warning: None, account: None },
        };
        let owed = owed_answer(&Command::Catalog(validated)).expect("the second hop owes the answer too");
        assert_eq!(owed, OwedAnswer::ApiKey(r));
        assert!(matches!(failure_update(owed), UiUpdate::ApiKey { req, result } if req == r && !result.ok));
        // Other catalog results answer no request.
        let voices = crate::command::CatalogResult::Voices { generation: 0, refresh: false, result: Err("x".into()) };
        assert_eq!(owed_answer(&Command::Catalog(voices)), None);
    }

    #[test]
    fn every_failure_variant_answers_its_own_request() {
        let r = RequestId(3);
        for owed in [
            OwedAnswer::ReadStarted(r),
            OwedAnswer::Home(r),
            OwedAnswer::Analytics(r),
            OwedAnswer::HistoryPage(r),
            OwedAnswer::SettingsSaved(r),
            OwedAnswer::ApiKey(r),
            OwedAnswer::Done(r),
            OwedAnswer::Exported(r),
            OwedAnswer::CacheStats(r),
            OwedAnswer::ExpiringCount(r),
            OwedAnswer::FavoriteToggled(r),
        ] {
            assert_eq!(owed.req(), r);
            assert_eq!(answered_request(&failure_update(owed)), Some(r), "{owed:?}");
        }
        assert_eq!(answered_request(&UiUpdate::HistoryChanged), None);
    }
}
