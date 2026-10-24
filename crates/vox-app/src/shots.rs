//! The in-app screenshot harness (shell.md §9; ARCHITECTURE.md §8 D-SH-17), active when `VOX_SHOTS_DIR` is set (implies
//! demo + mock TTS; no hotkeys, no hook, no bridge, no AUMID registration; the main window is not shown at start; the
//! initial CLI arguments are never handled). Decision: Slint windows are captured with `slint::Window::take_snapshot()`
//! instead of Chromium captures; the sequence (app pages × themes at 1280×820, widget playing / compact / loading /
//! error, chip context / selection), the 100 s watchdog, the blank-image test and `report.json` keep the original
//! shape (`app.electron` becomes `app.runtime`; `displays[].id` = the `HMONITOR`, `MonitorInfo::id`). Static page
//! previews use `tools/vox-shot` with JSON fixtures instead.
//!
//! Mechanics:
//! - [`start`] runs the sequence as `slint::spawn_local(async ..)` on the UI thread, awaiting a `slint::Timer`-based
//!   `sleep(ms)` future between steps (rendered frames happen while it waits), reaching `AppUi` through the UI
//!   thread-local between awaits. It returns a cell that holds the exit code once the run finished; `app::run`
//!   returns that value after the event loop (0 when unset).
//! - Watchdog: a std thread that after 100 s writes `report.json` with the failure `watchdog: run exceeded 100 s`
//!   (plus everything recorded so far) and calls `std::process::exit(2)` — unless the run already finished (the
//!   "always: clear the watchdog" rule: [`finish`] marks the report finished and the watchdog then does nothing).
//! - Exit code: besides the cell `app::run` reads after the event loop, the code is kept in a process-wide value
//!   ([`exit_code`]) so the hard-exit paths of the quit (the UI's 4 s hard exit; the core's quit watchdog should do the
//!   same) exit with it instead of 0.
//! - Mock read: `Command::ReadText{req: Some(id), text: DEMO_TEXT, options{source: manual, sourceApp: "Chrome",
//!   sourceTitle: "Essay"}}`; `AppUi::apply` forwards every `ReadStarted` to [`on_read_started`], which keeps the one
//!   whose `req` matches (failure → `mock read failed: <code> <error>`).
//! - Loading / error states: `AppUi.player.start_session(TtsSessionStart{session_id: "shots-loading", read_id:
//!   "shots-loading", text: LOADING_TEXT, chunk_count: 2, ..})` and `player.fail_session(TtsSessionError{code: Quota,
//!   message: "Your ElevenLabs character quota is used up", fatal: true})` directly on the reading player.
//! - Console entries: the WARN / ERROR lines this run appended to `<profile>\logs\main.log` (`main` records the log's
//!   length before `app::run` starts — [`note_log_start_from_env`] — so startup errors count too; without that mark the
//!   harness falls back to the length when it starts; a rotation during the run restarts at the top of the new file),
//!   each as `{window: scope, level: "error"|"warning", message ≤ 2000, source: scope, line: 0}` for
//!   `consoleErrors` / `consoleWarnings`.
//! - Runner (P-APP-INT): `scripts/shots.ps1` (not under `tools/`, which is frozen) deletes `report.json`, sets
//!   `VOX_SHOTS_DIR` / `VOX_DEMO` / `VOX_MOCK_TTS`, runs `vox.exe`, kills it after 120 s, prints the summary and exits
//!   0 (ok) / 1 (failures, timeout or no report) / 2 (the exe is missing).

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};
use slint::ComponentHandle;
use vox_core::app::{AppPage, AppRoute, ChipMode};
use vox_core::history::ReadSource;
use vox_core::player::{PlayerCommand, PlayerStatus};
use vox_core::read::{ReadErrorCode, ReadResult, ReadTextOptions};
use vox_core::tts::{TtsSessionError, TtsSessionStart};
use vox_win::window::PhysRect;

use crate::command::{Command, CoreHandle, RequestId};
use crate::update::UiUpdate;

/// The fixed demo paragraph the harness reads (old `shots.ts` `DEMO_TEXT`).
pub const DEMO_TEXT: &str = "The quiet revolution in personal computing was never about faster chips. It was about the moment software began to fit around people instead of the other way round. Good tools disappear into the work: you select a paragraph, and a calm voice reads it back while you look out of the window and think.";

/// The licence clause of the synthetic loading session (old `shots.ts` `LOADING_TEXT`).
pub const LOADING_TEXT: &str = "Section 4.2: The licensee shall not sublicense, assign, or otherwise transfer any rights granted under this agreement without prior written consent of the licensor.";

/// The whole run's limit (shell.md §9 step 2).
const WATCHDOG: Duration = Duration::from_secs(100);
/// The app window's content size for the captures (logical px).
const APP_SIZE: (f32, f32) = (1280.0, 820.0);
const PAGES: [(AppPage, &str); 6] = [
    (AppPage::Home, "home"),
    (AppPage::History, "history"),
    (AppPage::Analytics, "analytics"),
    (AppPage::Voices, "voices"),
    (AppPage::Settings, "settings"),
    (AppPage::Onboarding, "onboarding"),
];
const THEMES: [&str; 2] = ["paper", "night"];
/// Console message cut (characters).
const CONSOLE_MAX: usize = 2000;
/// After the harness pushed a widget card itself: the card's enter animation (180 ms) must finish before a capture.
const CARD_SETTLE_MS: u64 = 400;
/// The longest wait for outstanding art before an app-page capture (`settle_art`; a debug backdrop takes ~4 s).
const ART_SETTLE_MAX_MS: u64 = 6_000;

/// One captured file in `report.json`.
#[derive(Debug, Clone, PartialEq)]
struct Shot {
    file: String,
    width: u32,
    height: u32,
    bytes: usize,
    window_bounds: Option<PhysRect>,
    blank: bool,
}

/// What the report holds (shared with the watchdog thread).
#[derive(Debug, Default)]
struct ReportData {
    out_dir: PathBuf,
    started: Option<Instant>,
    shots: Vec<Shot>,
    failures: Vec<String>,
    windows: [Option<PhysRect>; 3],
    displays: Vec<Value>,
    log_file: Option<PathBuf>,
    log_offset: u64,
    packaged: bool,
    /// The run wrote its final report: the watchdog stands down.
    finished: bool,
}

/// The harness's exit code once known (`-1` before).
static EXIT_CODE: AtomicI32 = AtomicI32::new(-1);
/// `main.log` and its length before `app::run` started (shots mode only).
static LOG_START: OnceLock<(PathBuf, u64)> = OnceLock::new();

/// The harness's exit code (0 ok, 1 failures) once the run finished; `None` outside shots mode or before.
pub fn exit_code() -> Option<u8> {
    u8::try_from(EXIT_CODE.load(Ordering::SeqCst)).ok()
}

/// The code a hard exit should use: the harness's code in shots mode, else 0.
pub fn hard_exit_code() -> i32 {
    exit_code().map(i32::from).unwrap_or(0)
}

/// Records where this run's log lines start (called by `main` before `app::run`, in shots mode only).
pub fn note_log_start(log: &Path) {
    let len = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
    let _ = LOG_START.set((log.to_path_buf(), len));
}

/// [`note_log_start`] for the profile `app::run` will open when `VOX_SHOTS_DIR` is set (shots mode implies demo).
pub fn note_log_start_from_env() {
    let flags = vox_core::app::RuntimeFlags::from_lookup(|k| std::env::var(k).ok());
    if !flags.shots() {
        return;
    }
    if let Ok(profile) = vox_store::Profile::resolve(flags.demo) {
        note_log_start(&profile.log_file());
    }
}

thread_local! {
    /// The harness's mock-read request and its answer.
    static READ: RefCell<(Option<RequestId>, Option<ReadResult>)> = const { RefCell::new((None, None)) };
}

/// `AppUi::apply` forwards every `ReadStarted`; the harness keeps the answer to its own request.
pub fn on_read_started(req: RequestId, result: &ReadResult) {
    READ.with(|r| {
        let mut r = r.borrow_mut();
        if r.0 == Some(req) {
            r.1 = Some(result.clone());
        }
    });
}

/// Starts the harness on the UI thread (module docs). The cell receives the exit code (0 ok, 1 failures) when the run
/// finished and the quit was requested; the watchdog exits with 2 on its own.
pub fn start(out_dir: PathBuf, core: CoreHandle) -> Rc<Cell<Option<u8>>> {
    let cell = Rc::new(Cell::new(None));
    let data = Arc::new(Mutex::new(ReportData {
        out_dir: out_dir.clone(),
        started: Some(Instant::now()),
        packaged: !cfg!(debug_assertions),
        ..ReportData::default()
    }));
    spawn_watchdog(data.clone());
    let task_cell = cell.clone();
    let task = async move {
        let ok = run(&out_dir, &core, &data).await;
        let code = if ok { 0 } else { 1 };
        EXIT_CODE.store(i32::from(code), Ordering::SeqCst);
        task_cell.set(Some(code));
        crate::ui::with_app("shots quit", |app| app.quit_from_ui());
    };
    if let Err(e) = slint::spawn_local(task) {
        tracing::error!(target: "shots", "the screenshot harness could not start: {e}");
        EXIT_CODE.store(1, Ordering::SeqCst);
        cell.set(Some(1));
        core_quit_later();
    }
    cell
}

fn core_quit_later() {
    crate::ui::with_app_later(|app| app.quit_from_ui());
}

fn spawn_watchdog(data: Arc<Mutex<ReportData>>) {
    let spawned = std::thread::Builder::new().name("vox-shots-watchdog".into()).spawn(move || {
        std::thread::sleep(WATCHDOG);
        {
            let mut d = data.lock();
            if !watchdog_fires(&d) {
                return;
            }
            d.failures.push("watchdog: run exceeded 100 s".into());
            write_report(&d);
        }
        std::process::exit(2);
    });
    if let Err(e) = spawned {
        tracing::warn!(target: "shots", "watchdog could not start: {e}");
    }
}

/// Whether the watchdog acts (pure; tested): only while the run has not finished.
fn watchdog_fires(d: &ReportData) -> bool {
    !d.finished
}

/// The sequence (shell.md §9 step 2). Returns whether the report says `ok`.
async fn run(out_dir: &Path, core: &CoreHandle, data: &Arc<Mutex<ReportData>>) -> bool {
    if let Err(e) = prepare_dir(out_dir) {
        data.lock().failures.push(format!("harness error: cannot prepare {}: {e}", out_dir.display()));
    }
    // Where this run's log lines start: the mark `main` took before startup, else now.
    if let Some(log) = crate::ui::with_app_now(|app| app.init.profile.log_file()) {
        let offset = match LOG_START.get() {
            Some((marked, len)) if *marked == log => *len,
            _ => std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0),
        };
        let mut d = data.lock();
        d.log_file = Some(log);
        d.log_offset = offset;
    }
    data.lock().displays = displays();
    let Some(saved) = crate::ui::with_app_now(|app| app.settings.clone()) else {
        data.lock().failures.push("harness error: the UI does not exist".into());
        return finish(data);
    };

    // Wait for the overlays (the native windows exist and are styled).
    for _ in 0..100 {
        if crate::ui::with_app_now(|app| app.overlays_ready).unwrap_or(false) {
            break;
        }
        sleep(50).await;
    }

    app_step(out_dir, core, data).await;
    widget_step(out_dir, core, data).await;
    chip_step(out_dir, data).await;

    // Always: restore what the run changed.
    core.send(Command::UpdateSettings {
        req: None,
        patch: json!({
            "app": { "theme": saved.app.theme },
            "widget": {
                "enabled": saved.widget.enabled,
                "compact": saved.widget.compact,
                "position": saved.widget.position,
                "autoHide": saved.widget.auto_hide,
            },
            "library": { "saveHistory": saved.library.save_history },
        }),
    });
    sleep(100).await;
    finish(data)
}

fn finish(data: &Arc<Mutex<ReportData>>) -> bool {
    let mut d = data.lock();
    d.windows = crate::ui::with_app_now(|app| {
        [
            crate::windowing::hwnd_of(app.main.window()).and_then(vox_win::window::window_rect),
            crate::windowing::hwnd_of(app.widget.window()).and_then(vox_win::window::window_rect),
            crate::windowing::hwnd_of(app.chip.window()).and_then(vox_win::window::window_rect),
        ]
    })
    .unwrap_or_default();
    let ok = write_report(&d);
    d.finished = true;
    ok
}

/// App pages × themes.
async fn app_step(out_dir: &Path, core: &CoreHandle, data: &Arc<Mutex<ReportData>>) {
    let placed = crate::ui::with_app_now(|app| {
        app.main.window().set_size(slint::LogicalSize::new(APP_SIZE.0, APP_SIZE.1));
        if let Some(m) = vox_win::window::primary_monitor() {
            let scale = if m.scale > 0.0 { m.scale } else { 1.0 };
            let (w, h) = ((APP_SIZE.0 as f64 * scale).round() as i32, (APP_SIZE.1 as f64 * scale).round() as i32);
            let margin = (24.0 * scale).round() as i32;
            let wa = m.work_area;
            let x = (wa.x + wa.width - w - margin).max(wa.x);
            let y = (wa.y + wa.height - h - margin).max(wa.y);
            app.main.window().set_position(slint::PhysicalPosition::new(x, y));
        }
        app.main.show().is_ok()
    });
    if placed != Some(true) {
        data.lock().failures.push("app window missing".into());
        return;
    }
    sleep(800).await;
    for theme in THEMES {
        core.send(Command::UpdateSettings { req: None, patch: json!({ "app": { "theme": theme } }) });
        sleep(500).await;
        for (page, name) in PAGES {
            crate::ui::with_app_now(|app| app.navigate(AppRoute::page(page)));
            sleep(900).await;
            let shown = crate::ui::with_app_now(|app| app.shell.shown_page()).flatten();
            if shown != Some(page) {
                crate::ui::with_app_now(|app| app.navigate(AppRoute::page(page)));
                sleep(700).await;
            }
            settle_art().await;
            capture(out_dir, data, &format!("app-{name}-{theme}.png"), Which::Main);
        }
    }
    crate::ui::with_app_now(|app| {
        let _ = app.main.hide();
    });
}

/// Widget playing / compact / loading / error.
async fn widget_step(out_dir: &Path, core: &CoreHandle, data: &Arc<Mutex<ReportData>>) {
    core.send(Command::UpdateSettings {
        req: None,
        patch: json!({
            "widget": { "enabled": true, "compact": false, "position": "bottom-right", "autoHide": false },
            "library": { "saveHistory": false },
        }),
    });
    sleep(300).await;
    let req = core.request_id();
    READ.with(|r| *r.borrow_mut() = (Some(req), None));
    core.send(Command::ReadText {
        req: Some(req),
        text: DEMO_TEXT.to_string(),
        options: ReadTextOptions {
            source: Some(ReadSource::Manual),
            source_app: Some("Chrome".into()),
            source_title: Some("Essay".into()),
            ..ReadTextOptions::default()
        },
    });
    // The answer to the mock read.
    let mut answer = None;
    for _ in 0..100 {
        answer = READ.with(|r| r.borrow().1.clone());
        if answer.is_some() {
            break;
        }
        sleep(50).await;
    }
    match answer {
        Some(r) if r.ok => {}
        Some(r) => data.lock().failures.push(format!(
            "mock read failed: {} {}",
            r.code.map(ReadErrorCode::as_str).unwrap_or("unknown"),
            r.error.unwrap_or_default()
        )),
        None => data.lock().failures.push("mock read failed: unknown no answer".into()),
    }
    // Up to 12 s for `playing` with position ≥ 1.5 s.
    let mut status = PlayerStatus::Idle;
    let mut reached = false;
    for _ in 0..120 {
        if let Some(s) = crate::ui::with_app_now(|app| app.player.snapshot()) {
            status = s.state.status;
            if status == PlayerStatus::Playing && s.state.position_sec >= 1.5 {
                reached = true;
                break;
            }
        }
        sleep(100).await;
    }
    if !reached {
        data.lock().failures.push(format!("widget never reached playing (status={})", status.as_str()));
    }
    let shown = crate::ui::with_app_now(|app| {
        app.harness_poll();
        app.harness_show_widget();
        app.widget_window.visible
    });
    if shown != Some(true) {
        data.lock().failures.push("widget window missing".into());
        return;
    }
    sleep(300).await;
    capture(out_dir, data, "widget-playing.png", Which::Widget);
    core.send(Command::UpdateSettings { req: None, patch: json!({ "widget": { "compact": true } }) });
    sleep(800).await;
    capture(out_dir, data, "widget-compact.png", Which::Widget);
    core.send(Command::UpdateSettings { req: None, patch: json!({ "widget": { "compact": false } }) });
    core.send(Command::Player(PlayerCommand::Stop));
    sleep(600).await;

    // Loading and error are driven on the player directly (the mock is too fast to catch them).
    crate::ui::with_app_now(|app| {
        let voice_name = app.settings.voice_name.clone();
        let voice_id = app.settings.voice_id.clone();
        let len16 = vox_core::text16::utf16_len(LOADING_TEXT) as f64;
        app.player.start_session(TtsSessionStart {
            session_id: "shots-loading".into(),
            read_id: "shots-loading".into(),
            text: Arc::from(LOADING_TEXT),
            title: vox_text::preview_title(LOADING_TEXT, 64),
            voice_name,
            voice_id,
            source_app: Some("Word".into()),
            estimated_duration_sec: vox_core::js::js_round(len16 / 15.0),
            chunk_count: 2,
            rate: 1.0,
            volume: 1.0,
            highlight_words: true,
            from_cache: false,
            sentences: Arc::from(vox_text::sentence_starts(LOADING_TEXT)),
        });
        app.harness_poll();
        app.harness_show_widget();
    });
    sleep(900).await;
    crate::ui::with_app_now(|app| app.harness_poll());
    // A card that first appears in this poll replays its enter animation (opacity 0 → 1 in 180 ms).
    sleep(CARD_SETTLE_MS).await;
    capture(out_dir, data, "widget-loading.png", Which::Widget);
    crate::ui::with_app_now(|app| {
        app.player.fail_session(TtsSessionError {
            session_id: "shots-loading".into(),
            message: "Your ElevenLabs character quota is used up".into(),
            code: ReadErrorCode::Quota,
            fatal: true,
        });
    });
    sleep(800).await;
    crate::ui::with_app_now(|app| app.harness_poll());
    sleep(CARD_SETTLE_MS).await;
    capture(out_dir, data, "widget-error.png", Which::Widget);
    crate::ui::with_app_now(|app| app.player.command(PlayerCommand::Stop));
    sleep(200).await;
    crate::ui::with_app_now(|app| {
        app.harness_poll();
        app.harness_hide_widget();
    });
}

/// Chip context / selection at the primary work area's `(right − 320, bottom − 280)`.
async fn chip_step(out_dir: &Path, data: &Arc<Mutex<ReportData>>) {
    let Some(m) = vox_win::window::primary_monitor() else {
        data.lock().failures.push("chip window missing".into());
        return;
    };
    let wa = m.work_area;
    let (x, y) = (wa.x + wa.width - 320, wa.y + wa.height - 280);
    for (mode, file) in [(ChipMode::Context, "chip-context.png"), (ChipMode::Selection, "chip-selection.png")] {
        crate::ui::AppUi::apply(UiUpdate::ChipShow { x, y, mode });
        sleep(700).await;
        capture(out_dir, data, file, Which::Chip);
    }
    crate::ui::AppUi::apply(UiUpdate::ChipHide);
    sleep(200).await;
}

#[derive(Debug, Clone, Copy)]
enum Which {
    Main,
    Widget,
    Chip,
}

/// Captures one window to `out_dir/file` (shell.md §9 step 3): the PNG is written and listed even when empty/blank.
/// Waits (≤ [`ART_SETTLE_MAX_MS`]) until the art cache has no render outstanding, so a capture shows the real voice
/// art and the backdrop of the current theme rather than placeholders (a theme switch re-renders the backdrop, which
/// takes seconds in a debug build), then lets one more frame render.
async fn settle_art() {
    let mut waited = 0;
    while waited < ART_SETTLE_MAX_MS && crate::ui::with_app_now(|app| app.art.busy()).unwrap_or(false) {
        sleep(100).await;
        waited += 100;
    }
    if waited > 0 {
        sleep(150).await;
    }
}

fn capture(out_dir: &Path, data: &Arc<Mutex<ReportData>>, file: &str, which: Which) {
    let taken = crate::ui::with_app_now(|app| {
        let window = match which {
            Which::Main => app.main.window(),
            Which::Widget => app.widget.window(),
            Which::Chip => app.chip.window(),
        };
        let bounds = crate::windowing::hwnd_of(window).and_then(vox_win::window::window_rect);
        (window.take_snapshot(), bounds)
    });
    let Some((snapshot, bounds)) = taken else {
        data.lock().failures.push(format!("{file}: window missing"));
        return;
    };
    let buffer = match snapshot {
        Ok(b) => b,
        Err(e) => {
            data.lock().failures.push(format!("{file}: {e}"));
            return;
        }
    };
    let (width, height) = (buffer.width(), buffer.height());
    let rgba: Vec<u8> = buffer.as_bytes().to_vec();
    let empty = width == 0 || height == 0 || rgba.is_empty();
    let blank = looks_blank(&rgba, width, height);
    let png = if empty {
        Vec::new()
    } else {
        match (vox_art::Art { width, height, rgba }).encode_png() {
            Ok(p) => p,
            Err(e) => {
                data.lock().failures.push(format!("{file}: {e}"));
                return;
            }
        }
    };
    if let Err(e) = std::fs::write(out_dir.join(file), &png) {
        data.lock().failures.push(format!("{file}: {e}"));
        return;
    }
    let mut d = data.lock();
    if empty {
        d.failures.push(format!("{file}: empty capture"));
    } else if blank {
        d.failures.push(format!("{file}: capture looks blank"));
    } else {
        tracing::info!(target: "shots", "captured {file} {width}x{height}");
    }
    d.shots.push(Shot { file: file.to_string(), width, height, bytes: png.len(), window_bounds: bounds, blank });
}

/// Creates the output folder and deletes old captures and the old report.
fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_old_output(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// `^(app|widget|chip)-.*\.png$` or `report.json` (pure; tested).
fn is_old_output(name: &str) -> bool {
    name == "report.json" || (name.ends_with(".png") && (name.starts_with("app-") || name.starts_with("widget-") || name.starts_with("chip-")))
}

fn rect_json(r: &PhysRect) -> Value {
    json!({ "x": r.x, "y": r.y, "width": r.width, "height": r.height })
}

fn displays() -> Vec<Value> {
    vox_win::window::monitors()
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "scaleFactor": m.scale,
                "bounds": rect_json(&m.bounds),
                "workArea": rect_json(&m.work_area),
                "primary": m.primary,
            })
        })
        .collect()
}

/// The WARN / ERROR lines appended to `main.log` since `offset` (module docs).
fn console_entries(log: &Path, offset: u64) -> (Vec<Value>, Vec<Value>) {
    let Ok(bytes) = std::fs::read(log) else { return (Vec::new(), Vec::new()) };
    let start = if (offset as usize) <= bytes.len() { offset as usize } else { 0 };
    let text = String::from_utf8_lossy(&bytes[start..]);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for line in text.lines() {
        if let Some((level, scope, message)) = parse_log_line(line) {
            let entry = json!({ "window": scope, "level": level, "message": message, "source": scope, "line": 0 });
            if level == "error" {
                errors.push(entry);
            } else {
                warnings.push(entry);
            }
        }
    }
    (errors, warnings)
}

/// `ISO LEVEL [scope] text` → `("error" | "warning", scope, text ≤ 2000 chars)` for WARN / ERROR lines (pure; tested).
fn parse_log_line(line: &str) -> Option<(&'static str, String, String)> {
    let mut parts = line.splitn(2, ' ');
    let _timestamp = parts.next()?;
    let rest = parts.next()?.trim_start();
    let (level, rest) = rest.split_once(' ')?;
    let level = match level {
        "ERROR" => "error",
        "WARN" => "warning",
        _ => return None,
    };
    let rest = rest.trim_start();
    let (scope, message) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((scope, message)) => (scope.to_string(), message.trim_start()),
        None => (String::new(), rest),
    };
    let message: String = message.chars().take(CONSOLE_MAX).collect();
    Some((level, scope, message))
}

/// Writes `report.json`; returns `ok` (no failures and no console errors).
fn write_report(d: &ReportData) -> bool {
    let (errors, warnings) = match &d.log_file {
        Some(log) => console_entries(log, d.log_offset),
        None => (Vec::new(), Vec::new()),
    };
    let ok = d.failures.is_empty() && errors.is_empty();
    let duration_ms = d.started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
    let window = |r: &Option<PhysRect>| r.as_ref().map(rect_json).unwrap_or(Value::Null);
    let report = json!({
        "ok": ok,
        "generatedAt": vox_core::time::iso_utc_ms(crate::windowing::now_ms()),
        "durationMs": duration_ms,
        "app": {
            "version": vox_core::constants::APP_VERSION,
            "runtime": crate::runtime::runtime_label(),
            "packaged": d.packaged,
        },
        "displays": d.displays,
        "windows": { "main": window(&d.windows[0]), "widget": window(&d.windows[1]), "chip": window(&d.windows[2]) },
        "shots": d.shots.iter().map(|s| json!({
            "file": s.file,
            "width": s.width,
            "height": s.height,
            "bytes": s.bytes,
            "windowBounds": window(&s.window_bounds),
            "blank": s.blank,
        })).collect::<Vec<_>>(),
        "failures": d.failures,
        "consoleErrors": errors,
        "consoleWarnings": warnings,
    });
    let path = d.out_dir.join("report.json");
    let text = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into());
    if let Err(e) = std::fs::create_dir_all(&d.out_dir).and_then(|()| std::fs::write(&path, text)) {
        tracing::error!(target: "shots", "report.json could not be written: {e}");
    }
    tracing::info!(target: "shots", "report written: {} shots, {} failures, {} console errors", d.shots.len(), d.failures.len(), errors.len());
    ok
}

/// A `slint::Timer`-based sleep for the harness's async sequence.
fn sleep(ms: u64) -> Sleep {
    Sleep { ms, state: Rc::new(RefCell::new(SleepState::default())), started: false, timer: None }
}

#[derive(Default)]
struct SleepState {
    done: bool,
    waker: Option<Waker>,
}

struct Sleep {
    ms: u64,
    state: Rc<RefCell<SleepState>>,
    started: bool,
    timer: Option<slint::Timer>,
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        {
            let mut st = self.state.borrow_mut();
            if st.done {
                return Poll::Ready(());
            }
            st.waker = Some(cx.waker().clone());
        }
        if !self.started {
            self.started = true;
            let state = self.state.clone();
            let timer = slint::Timer::default();
            timer.start(slint::TimerMode::SingleShot, Duration::from_millis(self.ms), move || {
                let waker = {
                    let mut st = state.borrow_mut();
                    st.done = true;
                    st.waker.take()
                };
                if let Some(w) = waker {
                    w.wake();
                }
            });
            self.timer = Some(timer);
        }
        Poll::Pending
    }
}

/// The blank test (shell.md §9 step 3): zero size → blank; else sample every `max(1, floor(w·h / 4000))`-th pixel of
/// the straight RGBA8 buffer, quantise each channel to its top 4 bits, and count distinct values; more than 3 distinct
/// → not blank. Pure; tested.
pub fn looks_blank(rgba: &[u8], width: u32, height: u32) -> bool {
    let pixels = (width as usize).saturating_mul(height as usize);
    if pixels == 0 {
        return true;
    }
    let step = (pixels / 4000).max(1);
    let mut seen: Vec<u16> = Vec::with_capacity(4);
    let mut i = 0;
    while i < pixels {
        let o = i * 4;
        let Some(px) = rgba.get(o..o + 4) else { break };
        let q = (u16::from(px[0] >> 4) << 12) | (u16::from(px[1] >> 4) << 8) | (u16::from(px[2] >> 4) << 4) | u16::from(px[3] >> 4);
        if !seen.contains(&q) {
            seen.push(q);
            if seen.len() > 3 {
                return false;
            }
        }
        i += step;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        px.iter().copied().cycle().take((w * h * 4) as usize).collect()
    }

    #[test]
    fn blank_test_rules() {
        assert!(looks_blank(&[], 0, 0));
        assert!(looks_blank(&[], 10, 0));
        assert!(looks_blank(&solid(64, 64, [10, 20, 30, 255]), 64, 64));
        // Values that differ only in the low 4 bits quantise together.
        let mut img = solid(8, 8, [16, 16, 16, 255]);
        img[4..8].copy_from_slice(&[31, 17, 20, 255]);
        assert!(looks_blank(&img, 8, 8));
        // Three distinct colours are still blank, four are not.
        let mut img = solid(8, 8, [0, 0, 0, 255]);
        img[4..8].copy_from_slice(&[255, 0, 0, 255]);
        img[8..12].copy_from_slice(&[0, 255, 0, 255]);
        assert!(looks_blank(&img, 8, 8));
        img[12..16].copy_from_slice(&[0, 0, 255, 255]);
        assert!(!looks_blank(&img, 8, 8));
        // Large images are sampled every floor(w·h / 4000)-th pixel: 200×200 → every 10th.
        let mut img = solid(200, 200, [0, 0, 0, 0]);
        for (n, i) in [1usize, 2, 3, 4].iter().enumerate() {
            let o = i * 4;
            img[o..o + 4].copy_from_slice(&[n as u8 * 60 + 60, 0, 0, 255]);
        }
        assert!(looks_blank(&img, 200, 200), "unsampled pixels do not count");
        for (n, i) in [10usize, 20, 30, 40].iter().enumerate() {
            let o = i * 4;
            img[o..o + 4].copy_from_slice(&[n as u8 * 60 + 60, 0, 0, 255]);
        }
        assert!(!looks_blank(&img, 200, 200));
        // A short buffer never panics.
        assert!(looks_blank(&[1, 2, 3], 10, 10));
    }

    #[test]
    fn old_outputs() {
        assert!(is_old_output("report.json"));
        assert!(is_old_output("app-home-night.png"));
        assert!(is_old_output("widget-error.png"));
        assert!(is_old_output("chip-context.png"));
        assert!(!is_old_output("chip-context.jpg"));
        assert!(!is_old_output("notes.png"));
        assert!(!is_old_output("report.json.bak"));
    }

    #[test]
    fn log_lines() {
        assert_eq!(
            parse_log_line("2026-10-17T10:00:00.000Z ERROR [tray] tray icon missing"),
            Some(("error", "tray".to_string(), "tray icon missing".to_string()))
        );
        assert_eq!(
            parse_log_line("2026-10-17T10:00:00.000Z WARN  [window] could not show the widget: x"),
            Some(("warning", "window".to_string(), "could not show the widget: x".to_string()))
        );
        assert_eq!(parse_log_line("2026-10-17T10:00:00.000Z INFO  [app] started"), None);
        assert_eq!(parse_log_line("garbage"), None);
        let long = format!("2026-10-17T10:00:00.000Z ERROR [x] {}", "é".repeat(3000));
        assert_eq!(parse_log_line(&long).unwrap().2.chars().count(), CONSOLE_MAX);
    }

    #[test]
    fn console_entries_start_at_the_offset() {
        let dir = std::env::temp_dir().join(format!("vox-shots-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("main.log");
        let before = "2026-10-17T10:00:00.000Z ERROR [old] before the run\n";
        std::fs::write(&log, format!("{before}2026-10-17T10:00:01.000Z WARN  [ui] w\n2026-10-17T10:00:02.000Z ERROR [ui] e\n")).unwrap();
        let (errors, warnings) = console_entries(&log, before.len() as u64);
        assert_eq!(errors.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert_eq!(errors[0]["window"], "ui");
        // A rotated (shorter) log is read from the top.
        let (errors, _) = console_entries(&log, 1_000_000);
        assert_eq!(errors.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_watchdog_stands_down_after_the_run() {
        let mut d = ReportData::default();
        assert!(watchdog_fires(&d));
        d.finished = true;
        assert!(!watchdog_fires(&d));
        // No code before a run finished: hard exits keep 0.
        assert_eq!(hard_exit_code(), exit_code().map(i32::from).unwrap_or(0));
    }

    #[test]
    fn report_shape() {
        let dir = std::env::temp_dir().join(format!("vox-shots-report-{}", std::process::id()));
        let d = ReportData {
            out_dir: dir.clone(),
            shots: vec![Shot { file: "app-home-night.png".into(), width: 10, height: 10, bytes: 5, window_bounds: None, blank: false }],
            failures: vec!["chip window missing".into()],
            ..ReportData::default()
        };
        assert!(!write_report(&d));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["shots"][0]["file"], "app-home-night.png");
        assert_eq!(v["shots"][0]["windowBounds"], Value::Null);
        assert_eq!(v["failures"][0], "chip window missing");
        assert!(v["app"]["runtime"].as_str().unwrap().contains("Slint"));
        assert!(v["windows"]["main"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
