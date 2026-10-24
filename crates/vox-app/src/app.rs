//! Bootstrap and the process lifecycle (shell.md §3; docs/ARCHITECTURE.md §3.1, §3.6 and §4.17–§4.19).
//!
//! `run()` does, in order (each step's failure mode in brackets):
//! 1. `Runtime::detect()` (env flags; demo → `%APPDATA%\Vox-Demo`; argv via `args_os` + `to_string_lossy`) [fatal →
//!    exit 1]; `set_per_monitor_dpi_aware`; not in shots mode: write `<profile>\vox-icon.png` from the embedded app
//!    icon (`assets/vox-256.png`) when missing or different, and `vox_win::shell::register_app_user_model(APP_ID,
//!    APP_NAME, png)` (toasts of an unpackaged app need a registered AUMID; failure or panic logged later, never
//!    fatal); `set_app_user_model_id(APP_ID)`;
//!    `vox_core::locale::set_process_locale(vox_win::system::user_locale())`. Create the `Command` channel (crossbeam
//!    unbounded) and the `CoreHandle` here.
//! 2. Single instance (`vox_win::single_instance::acquire(runtime.instance_key())`, per-user names; the primary owns
//!    the first pipe instance): secondary → `forward_to_primary` (argv + cwd; the server's user SID is checked, then
//!    `AllowSetForegroundWindow`) and exit 0, before touching any store [pipe failure → exit 0 after logging to
//!    stderr]. Primary, right away: (a) enqueue `Command::Cli{argv: own, cwd, from_second_instance: false}` (not in
//!    shots mode — the initial arguments are never handled there, shell.md §3.2 step 16); (b)
//!    `InstanceServer::listen(|a| core.send(Command::Cli{argv: a.argv, cwd: a.cwd, from_second_instance: true}))` —
//!    the unbounded channel buffers until the core spawns, so a secondary launched during a slow startup is never
//!    lost, and the primary's own `Cli` stays first (shell.md §3.3).
//! 3. **Old-version guard** (BRIEF.md "Do not run both builds at once"): `electron_instance_running(profile root)` →
//!    `vox_win::shell::message_box(copy::old_version_running_title(), copy::old_version_running_body())`, exit 0.
//! 4. Logging (`vox_store::log::init_logging`), first line `Vox <version> starting (rust <rustc>, slint 1.18.1,
//!    build <hash>, demo=<bool>, mockTts=<bool>)`. In shots mode a tracing layer also collects WARN/ERROR events for
//!    `report.json` (`shots` module).
//! 5. Stores: `SettingsStore::open(profile, ELEVENLABS_API_KEY)` (key load + Electron migration → `key_status()`),
//!    then `HistoryStore::open` ∥ `AudioCache::open` (two threads, joined; the cache starts its I/O worker);
//!    `history.on_removed` → `cache.remove` (index-only + queued deletions, fast on the core thread); demo seeding
//!    (`generate_demo_records(Local::now(), 120)` when empty — shots mode clears the history first so the captures
//!    always show a history seeded as of now; `onboarded = true`); retention now;
//!    `retention::reconcile_cache` (stale `cached` flags, redactions). Orphan removal happens in the core's startup
//!    work (step 8), not here. [never fatal]
//! 6. The `UiSink` (buffers until the UI attaches, so toasts raised before the UI exists are not lost; chip events
//!    are dropped while detached).
//! 7. Services: `ElevenLabsClient::new(key source = settings.api_key, ClientConfig { proxy:
//!    vox_win::registry::system_proxy() → ProxyConfig, .. })` (re-read later, D-NET-1); the reading `Player` and the
//!    preview `Player` (vox-audio; `null_output` in shots mode); `TtsEngine` (sink → reading player; events →
//!    `Command::Engine`; mock = `flags.mock_tts`); `SelectionService` (events: chip show/hide → `UiSink` directly, the
//!    rest → core); `HotkeyService` (`None` on failure: every hotkey then reports `InUse`); `MediaSession` (`None` on
//!    failure, logged); `SystemWatcher` (events → core; session end → `Command::SessionEnd(reply)`, wait ≤ 5 s; after
//!    2.5 s without a reply the handler calls `settings.flush_sync()` + `history.flush_sync()` itself — it holds clones
//!    of both stores); `BridgeServer` (handler = `core::bridge::BridgeGlue`).
//! 8. Spawn the core thread (`core::Core::spawn`). Its startup work, in order (`Core::spawn` docs): publish
//!    `Settings`, `Theme`, `Motion`, `AppInfo`; send `Rate{playbackRate}` and `Volume{volume}` to the reading player
//!    once; `cache.remove_orphans(all history ids)`; register hotkeys and start the selection service (not in shots
//!    mode); sync the bridge, the Explorer verb (non-forced, not demo) and launch-at-login (not demo); first credits
//!    refresh; arm timers (incl. `TimerId::PreloadOutput` 1 s, not with `null_output`); then the deferred `Cli`
//!    commands, FIFO.
//! 9. On the main thread: install the Slint backend with `slint::BackendSelector::new().backend_name("winit")
//!    .with_winit_window_attributes_hook(..)` (`windowing::window_attributes`: overlays `with_active(false)`,
//!    skip-taskbar, always-on-top, not resizable, no buttons, off-screen; the main window gets its theme, placement and
//!    maximized state up front). Build `ui::UiInit` from the stores (settings, resolved theme `resolve_theme(mode,
//!    apps_use_light_theme() == Some(false))`, `AppInfo`, startup route — `onboarded ? home : onboarding`, or
//!    `settings/account` when the key migration failed and the user is onboarded (D-KEY-3) —, the startup visibility
//!    rule [`should_show_main_at_start`], `main_window::initial_placement(load_window_state(..), &monitors())`,
//!    `load_ui_state(..).analytics_range`, `reduce_motion()`, the profile); `ui::AppUi::create(init, …)` (three
//!    windows, everything applied **before** any show: Theme/Brand/Motion, every controller seeded from
//!    `init.settings`; the overlays are pre-created off-screen and get their subclass; then the main window is shown
//!    if `init.show_main`, `WindowEvent::MainState` is sent, and the `UiSink` attaches); create the tray; shots mode →
//!    `shots::start(out_dir, core)`; then `slint::run_event_loop_until_quit()`.
//! 10. After the loop: join the core (it finished the quit sequence), drop the log guard, exit 0 (or the shots
//!     harness code from the cell `shots::start` returned).
//!
//! Quit (shell.md §3.5): `Command::Quit` → the core runs `core::lifecycle::shutdown` (watchdog, unregister hotkeys,
//! stop the selection service, persist playback, stop synthesis + drain its `Stopped`, flush settings/history/
//! selection/cache/bridge in parallel ≤ 3 s, publish `UiUpdate::Quit`, then stop the players and SMTC on bounded
//! helpers). The UI, on `Quit`, saves `window-state.json` synchronously, destroys the tray, hides the windows and quits
//! the event loop; a 4 s hard-exit timer (`std::process::exit(shots::hard_exit_code())`, 0 outside the harness) is
//! armed when it receives `Quit`. Before the UI itself sends `Command::Quit` (tray Quit, main close with close-to-tray
//! off, shots end) it commits a pending History undo batch so its `RemoveReads` precedes `Quit` in the FIFO (D-UM-32).

use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;
use vox_audio::{Player, PlayerConfig};
use vox_bridge::BridgeServer;
use vox_core::constants::{APP_ID, APP_NAME, SESSION_END_FALLBACK_MS, SESSION_END_WAIT_MS};
use vox_store::{AudioCache, HistoryStore, SettingsStore};
use vox_tts::{ClientConfig, ElevenLabsClient, TtsEngine};
use vox_win::hotkeys::HotkeyService;
use vox_win::media::MediaSession;
use vox_win::selection::{SelectionEvent, SelectionService};
use vox_win::single_instance::{ForwardedArgs, InstanceListener, InstanceRole};
use vox_win::system::SystemWatcher;

use crate::command::{Command, CoreHandle};
use crate::core::bridge::BridgeGlue;
use crate::core::engine_sink::{engine_events, player_events, preview_events, PlayerSink};
use crate::core::{queries, retention, Core, Services};
use crate::runtime::Runtime;
use crate::update::{UiSink, UiUpdate};

/// The whole program.
pub fn run() -> anyhow::Result<ExitCode> {
    // ── 1. Runtime, process identity, locale, the command channel ──
    let runtime = Runtime::detect()?;
    let shots = runtime.flags.shots();
    vox_win::shell::set_per_monitor_dpi_aware();
    // Logging is not up yet: problems found before it are logged right after it starts.
    let mut early_warnings: Vec<String> = Vec::new();
    if !shots {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| register_notification_identity(&runtime))) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => early_warnings.push(format!("could not register the notification identity: {e}")),
            Err(_) => early_warnings.push("registering the notification identity panicked".to_string()),
        }
    }
    vox_win::shell::set_app_user_model_id(APP_ID);
    vox_core::locale::set_process_locale(vox_win::system::user_locale());
    let (tx, rx) = crossbeam_channel::unbounded::<Command>();
    let core = CoreHandle::new(tx);

    // ── 2. Single instance (before any store opens) ──
    let instance_key = runtime.instance_key();
    let role = match vox_win::single_instance::acquire(&instance_key) {
        Ok(role) => Some(role),
        Err(e) => {
            early_warnings.push(format!("single-instance lock unavailable; running without it: {e}"));
            None
        }
    };
    let server = match role {
        Some(InstanceRole::Secondary) => {
            let args = ForwardedArgs { argv: runtime.argv.clone(), cwd: runtime.cwd.clone() };
            if let Err(e) = vox_win::single_instance::forward_to_primary(&instance_key, &args) {
                eprintln!("vox: could not hand the arguments to the running Vox: {e}");
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(InstanceRole::Primary(server)) => Some(server),
        None => None,
    };
    // The primary's own arguments go first (never in shots mode, shell.md §3.2 step 16); the listener starts after.
    if !shots {
        core.send(Command::Cli { argv: runtime.argv.clone(), cwd: runtime.cwd.clone(), from_second_instance: false });
    }
    let listener: Option<InstanceListener> = server.and_then(|server| {
        let forward = core.clone();
        let on_args = Arc::new(move |a: ForwardedArgs| forward.send(Command::Cli { argv: a.argv, cwd: a.cwd, from_second_instance: true }));
        match server.listen(on_args) {
            Ok(listener) => Some(listener),
            Err(e) => {
                early_warnings.push(format!("instance pipe listener failed; later launches cannot forward: {e}"));
                None
            }
        }
    });

    // ── 3. Old-version guard (BRIEF.md "Do not run both builds at once") ──
    if vox_win::single_instance::electron_instance_running(runtime.profile.root()) {
        vox_win::shell::message_box(&vox_core::copy::old_version_running_title(), &vox_core::copy::old_version_running_body());
        return Ok(ExitCode::SUCCESS);
    }

    // ── 4. Logging ──
    let log_guard = vox_store::log::init_logging(&runtime.profile, runtime.flags.debug);
    install_panic_logging();
    tracing::info!(
        target: "app",
        "{APP_NAME} {} starting (rust {}, slint 1.18.1, build {}, demo={}, mockTts={})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_RUST_VERSION"),
        runtime.build_hash,
        runtime.flags.demo,
        runtime.flags.mock_tts
    );
    for warning in early_warnings {
        tracing::warn!(target: "app", "{warning}");
    }

    // ── 5. Stores ──
    let (settings, history, cache) = open_stores(&runtime, &core);

    // ── 6. The UI publisher (buffers until the UI attaches) ──
    let ui_sink = UiSink::new(crate::ui::AppUi::apply);

    // ── 7. Services ──
    let key_source = {
        let settings = settings.clone();
        Arc::new(move || settings.api_key())
    };
    let client = ElevenLabsClient::new(key_source, ClientConfig { proxy: crate::core::system_proxy_config(), ..ClientConfig::default() });
    let player = Player::spawn(PlayerConfig { null_output: shots, name: "player", ..PlayerConfig::default() }, player_events(core.clone()));
    let preview_player =
        Player::spawn(PlayerConfig { null_output: shots, name: "preview", ..PlayerConfig::default() }, preview_events(core.clone()));
    let engine = TtsEngine::new(client.clone(), Arc::new(PlayerSink { player: player.clone() }), engine_events(core.clone()), runtime.flags.mock_tts);
    // Other voice providers: the Windows voices always, the rest when credentials were saved (Settings → Voice
    // providers).
    {
        let proxy = crate::core::system_proxy_config();
        let ua = vox_tts::user_agent();
        let windows = vox_core::providers::Provider::Windows;
        engine.set_provider(windows, vox_tts::connect_provider(windows, Default::default(), proxy.as_ref(), &ua));
        for (provider, creds) in vox_store::provider_secrets::load_all(&runtime.profile) {
            engine.set_provider(provider, vox_tts::connect_provider(provider, creds, proxy.as_ref(), &ua));
        }
    }
    let selection = {
        let sink = ui_sink.clone();
        let core = core.clone();
        SelectionService::new(Arc::new(move |event| match event {
            // Low latency, no core hop; dropped while the UI is not attached (UiSink docs).
            SelectionEvent::ChipShow { x, y, mode } => sink.send(UiUpdate::ChipShow { x, y, mode }),
            SelectionEvent::ChipHide => sink.send(UiUpdate::ChipHide),
            other => core.send(Command::Selection(other)),
        }))
    };
    let hotkeys = if shots {
        None
    } else {
        let core = core.clone();
        match HotkeyService::spawn(Arc::new(move |action| core.send(Command::Hotkey(action)))) {
            Ok(service) => Some(service),
            Err(e) => {
                tracing::warn!(target: "shortcuts", "hotkey service unavailable: {e}");
                None
            }
        }
    };
    let media = if shots {
        None
    } else {
        let core = core.clone();
        match MediaSession::spawn(Arc::new(move |action| core.send(Command::Media(action)))) {
            Ok(session) => Some(session),
            Err(e) => {
                tracing::warn!(target: "media", "media session unavailable: {e}");
                None
            }
        }
    };
    let watcher = {
        let on_event = {
            let core = core.clone();
            Arc::new(move |event| core.send(Command::System(event)))
        };
        let on_session_end = {
            let core = core.clone();
            let settings = settings.clone();
            let history = history.clone();
            Arc::new(move || wait_for_session_end_save(&core, &settings, &history))
        };
        match SystemWatcher::spawn(on_event, on_session_end) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                tracing::warn!(target: "app", "system watcher unavailable: {e}");
                None
            }
        }
    };
    let bridge_playing = Arc::new(AtomicBool::new(false));
    let bridge = Arc::new(BridgeServer::new(Arc::new(BridgeGlue { core: core.clone(), playing: bridge_playing.clone() })));

    // What the first frame needs (ARCHITECTURE.md §3.6 #5), read before the services move to the core.
    let current = settings.get();
    let dark = vox_core::settings::resolve_theme(current.app.theme, vox_win::registry::apps_use_light_theme() == Some(false)).is_dark();
    let args = vox_core::cli::parse_cli_args(&runtime.argv);
    let show_main = should_show_main_at_start(&runtime, &args, &current);
    let route = startup_route(&current, settings.key_status().problem.is_some());
    let app_info = queries::build_app_info(&runtime, &settings, &bridge, None, queries::provider_statuses(&engine));
    let main_placement = crate::windowing::main_window::initial_placement(
        vox_store::window_state::load_window_state(&runtime.profile).as_ref(),
        &vox_win::window::monitors(),
    );
    let analytics_range = vox_store::ui_state::load_ui_state(&runtime.profile).analytics_range;
    let reduce_motion = vox_win::window::reduce_motion();

    // ── 8. The core thread ──
    let services = Services {
        runtime: runtime.clone(),
        settings,
        history,
        cache,
        client,
        engine,
        player: player.clone(),
        preview_player,
        selection: selection.clone(),
        hotkeys,
        media,
        bridge,
        bridge_playing,
    };
    let core_thread = Core::try_spawn(services, rx, core.clone(), ui_sink.clone()).map_err(|e| anyhow::anyhow!("cannot start the core thread: {e}"))?;

    // ── 9. The UI (main thread) ──
    let ui_started = crate::windowing::install_backend(crate::windowing::AttributesInit { dark, main_placement }).and_then(|()| {
        let init = crate::ui::UiInit {
            settings: Arc::new(current),
            dark,
            reduce_motion,
            app_info: Arc::new(app_info),
            route,
            show_main,
            main_placement,
            analytics_range,
            profile: runtime.profile.clone(),
            demo: runtime.flags.demo,
        };
        crate::ui::AppUi::create(init, core.clone(), player, selection, &ui_sink)
    });
    if let Err(e) = ui_started {
        tracing::error!(target: "app", "the user interface could not start: {e:#}");
        core.send(Command::Quit);
        join_core(core_thread);
        stop_after_core(watcher, listener);
        drop(log_guard);
        return Err(e);
    }
    let shots_code = runtime.flags.shots_dir.clone().map(|dir| crate::shots::start(dir, core.clone()));
    if let Err(e) = slint::run_event_loop_until_quit() {
        tracing::error!(target: "app", "the event loop failed: {e}");
    }

    // ── 10. After the loop ──
    // A no-op when the quit sequence already ran (the usual way the loop ends).
    core.send(Command::Quit);
    join_core(core_thread);
    stop_after_core(watcher, listener);
    tracing::info!(target: "app", "{APP_NAME} stopped");
    drop(log_guard);
    let code = shots_code.and_then(|cell| cell.get()).unwrap_or(0);
    Ok(ExitCode::from(code))
}

/// Step 5: settings, then history ∥ audio cache; the removal hook, the change notification, demo seeding, retention
/// and cache reconciliation. Never fatal.
fn open_stores(runtime: &Runtime, core: &CoreHandle) -> (SettingsStore, HistoryStore, AudioCache) {
    let env_key = std::env::var("ELEVENLABS_API_KEY").ok().filter(|k| !vox_core::js::js_trim(k).is_empty());
    let settings = SettingsStore::open(&runtime.profile, env_key);
    let profile = &runtime.profile;
    let (history, cache) = std::thread::scope(|scope| {
        let history = scope.spawn(|| HistoryStore::open(profile));
        let cache = AudioCache::open(profile);
        let history = match history.join() {
            Ok(history) => history,
            Err(_) => {
                tracing::error!(target: "history", "opening the history panicked; retrying on the main thread");
                HistoryStore::open(profile)
            }
        };
        (history, cache)
    });
    {
        let cache = cache.clone();
        history.on_removed(Box::new(move |ids: &[String]| {
            for id in ids {
                cache.remove(id);
            }
        }));
    }
    {
        let core = core.clone();
        history.on_change(Box::new(move || core.send(Command::HistoryChanged)));
    }
    if runtime.flags.demo {
        // The shots harness captures Home / Analytics: a demo history seeded days ago would show empty "today" and
        // week figures, so shots mode re-seeds it as of now (deterministic captures; the demo profile is a sandbox).
        if runtime.flags.shots() && !history.is_empty() {
            let removed = history.clear();
            tracing::info!(target: "app", "shots: replaced {removed} demo records");
        }
        if history.is_empty() {
            let records = vox_store::demo::generate_demo_records(chrono::Local::now(), 120);
            let n = records.len();
            for record in records {
                history.add(record);
            }
            tracing::info!(target: "app", "seeded {n} demo records");
        }
        if !settings.get().onboarded {
            settings.update(&serde_json::json!({ "onboarded": true }));
        }
    }
    let now = vox_core::time::to_ms(&chrono::Utc::now());
    retention::apply_retention(&history, settings.get().library.retention_days, now);
    retention::reconcile_cache(&history, &cache);
    (settings, history, cache)
}

/// The system watcher's `SessionEndHandler` (ARCHITECTURE.md §2.6): ask the core for its synchronous save and wait
/// ≤ 5 s; after 2.5 s without a reply (or with the core gone) flush settings and history here.
fn wait_for_session_end_save(core: &CoreHandle, settings: &SettingsStore, history: &HistoryStore) {
    let (reply, answer) = crossbeam_channel::bounded::<()>(1);
    core.send(Command::SessionEnd(reply));
    let fallback = Duration::from_millis(SESSION_END_FALLBACK_MS);
    match answer.recv_timeout(fallback) {
        Ok(()) => {}
        Err(RecvTimeoutError::Timeout) => {
            tracing::warn!(target: "app", "the core did not answer the session end in time; saving directly");
            settings.flush_sync();
            history.flush_sync();
            let _ = answer.recv_timeout(Duration::from_millis(SESSION_END_WAIT_MS.saturating_sub(SESSION_END_FALLBACK_MS)));
        }
        Err(RecvTimeoutError::Disconnected) => {
            settings.flush_sync();
            history.flush_sync();
        }
    }
}

/// Joins the core thread (it ends after the quit sequence; the quit watchdog bounds it).
fn join_core(core_thread: std::thread::JoinHandle<()>) {
    if core_thread.join().is_err() {
        tracing::error!(target: "panic", "the core thread ended with a panic");
    }
}

/// The threads `app::run` owns besides the core.
fn stop_after_core(watcher: Option<SystemWatcher>, listener: Option<InstanceListener>) {
    if let Some(watcher) = watcher {
        watcher.shutdown();
    }
    if let Some(listener) = listener {
        listener.shutdown();
    }
}

/// Logs the location of every panic (the message may carry user text, so it is not logged); the default hook still
/// runs (stderr in dev builds).
fn install_panic_logging() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_else(|| "unknown location".into());
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        tracing::error!(target: "panic", "panic on thread {thread} at {location}");
        default_hook(info);
    }));
}

/// The app icon (D-SH-20: `assets/vox-256.png`, the AUMID icon source), embedded in the exe.
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/vox-256.png");

/// Step 1: an unpackaged app needs a registered AUMID (with an icon file) for its toasts to show (D-SH-19).
/// `<profile>\vox-icon.png` is (re)written from the embedded [`APP_ICON_PNG`] whenever it is missing or differs, so a
/// stale icon from an older build (the procedural stand-in art) is replaced. Runs before logging and the panic hook
/// exist: plain file I/O plus one registry write, and `run` calls it under `catch_unwind`.
fn register_notification_identity(runtime: &Runtime) -> Result<(), String> {
    let png = runtime.profile.root().join("vox-icon.png");
    write_if_changed(&png, APP_ICON_PNG).map_err(|e| e.to_string())?;
    vox_win::shell::register_app_user_model(APP_ID, APP_NAME, &png).map_err(|e| e.to_string())
}

/// Writes `bytes` to `path` (creating its folder) unless the file already holds exactly these bytes. `true` = written.
fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes)?;
    Ok(true)
}

/// Startup visibility (shell.md §3.4): not shots mode, no `--hidden`, no read action, and (`!startMinimized` or not
/// onboarded). Route: onboarded ? home : onboarding.
pub fn should_show_main_at_start(runtime: &Runtime, args: &vox_core::cli::CliArgs, settings: &vox_core::settings::Settings) -> bool {
    !runtime.flags.shots() && !args.hidden && !args.has_read_action() && (!settings.app.start_minimized || !settings.onboarded)
}

/// The route the main window opens on (D-KEY-3): onboarding when not onboarded; `settings/account` when onboarded but
/// a stored key could not be migrated/read and no key is available; else home.
pub fn startup_route(settings: &vox_core::settings::Settings, key_problem: bool) -> vox_core::app::AppRoute {
    use vox_core::app::{AppPage, AppRoute};
    if !settings.onboarded {
        AppRoute::page(AppPage::Onboarding)
    } else if key_problem && !settings.has_api_key {
        AppRoute::settings("account")
    } else {
        AppRoute::page(AppPage::Home)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use vox_core::app::{AppPage, AppRoute, RuntimeFlags};
    use vox_core::cli::parse_cli_args;
    use vox_core::settings::Settings;
    use vox_store::Profile;

    use super::*;

    fn runtime(shots: bool) -> Runtime {
        Runtime {
            flags: RuntimeFlags { demo: shots, mock_tts: shots, shots_dir: shots.then(|| PathBuf::from(r"C:\shots")), debug: false },
            profile: Profile::at(r"C:\p", shots),
            exe_path: PathBuf::from(r"C:\x\vox.exe"),
            extension_dir: PathBuf::from(r"C:\x\browser-extension"),
            is_packaged: false,
            runtime_label: crate::runtime::runtime_label(),
            build_hash: "dev".into(),
            argv: vec![],
            cwd: PathBuf::from(r"C:\x"),
        }
    }

    fn settings(onboarded: bool, start_minimized: bool) -> Settings {
        let defaults = Settings::default();
        Settings { onboarded, app: vox_core::settings::AppSettings { start_minimized, ..defaults.app.clone() }, ..defaults }
    }

    #[test]
    fn startup_visibility_rule() {
        let none = parse_cli_args::<&str>(&["vox.exe"]);
        assert!(should_show_main_at_start(&runtime(false), &none, &settings(true, false)));
        assert!(!should_show_main_at_start(&runtime(true), &none, &settings(true, false)), "never in shots mode");
        assert!(!should_show_main_at_start(&runtime(false), &parse_cli_args(&["vox.exe", "--hidden"]), &settings(true, false)));
        assert!(!should_show_main_at_start(&runtime(false), &parse_cli_args(&["vox.exe", "--read-clipboard"]), &settings(false, false)));
        assert!(!should_show_main_at_start(&runtime(false), &parse_cli_args(&["vox.exe", "--read-file", "a.txt"]), &settings(true, false)));
        assert!(!should_show_main_at_start(&runtime(false), &none, &settings(true, true)), "start minimized");
        assert!(should_show_main_at_start(&runtime(false), &none, &settings(false, true)), "onboarding always shows");
    }

    #[test]
    fn the_aumid_icon_is_the_app_icon_and_stale_copies_are_replaced() {
        // The embedded icon is the 256 px app icon PNG (D-SH-20), not procedural art.
        assert!(APP_ICON_PNG.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&APP_ICON_PNG[16..24], &[0, 0, 1, 0, 0, 0, 1, 0], "IHDR 256 × 256");
        let dir = std::env::temp_dir().join(format!("vox-icon-test-{}", vox_core::ids::new_uuid()));
        let png = dir.join("profile").join("vox-icon.png");
        assert!(write_if_changed(&png, APP_ICON_PNG).unwrap(), "missing → written (folder created)");
        assert!(!write_if_changed(&png, APP_ICON_PNG).unwrap(), "identical → left alone");
        std::fs::write(&png, b"an older placeholder").unwrap();
        assert!(write_if_changed(&png, APP_ICON_PNG).unwrap(), "different → replaced");
        assert_eq!(std::fs::read(&png).unwrap(), APP_ICON_PNG);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_routes() {
        assert_eq!(startup_route(&settings(false, false), true), AppRoute::page(AppPage::Onboarding));
        assert_eq!(startup_route(&settings(true, false), false), AppRoute::page(AppPage::Home));
        assert_eq!(startup_route(&settings(true, false), true), AppRoute::settings("account"), "D-KEY-3");
        let mut with_key = settings(true, false);
        with_key.has_api_key = true;
        assert_eq!(startup_route(&with_key, true), AppRoute::page(AppPage::Home), "an environment key is in use");
    }
}
