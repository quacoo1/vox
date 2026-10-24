//! `Command`: everything any source asks of the core thread (docs/ARCHITECTURE.md §3.2). Every source — UI
//! callbacks, tray, hotkeys, the selection service, SMTC, the bridge, second instances, the system watcher, the TTS
//! engine, the players, the audio cache's I/O worker, core workers and the stores' change notifications — sends one of
//! these on one unbounded crossbeam channel. The core processes them strictly in order on its thread; nothing here may
//! block the sender.
//!
//! Requests whose answer the UI shows carry a [`RequestId`]; the answer comes back as a `UiUpdate` with the same id
//! (the UI drops stale ids). A request command is **always** answered, even when its handler panics (D-ARCH-3: the
//! core answers with the failure variant, `core::failure_update`). Only blocking callers on other threads (the
//! bridge, session end) use reply channels.
//!
//! Validation (contracts.md §5.3 "UI surfaces are treated as untrusted input"): the in-process UI is trusted code,
//! but every value that came from a text field, a slider, the OS (SMTC seeks), the bridge or a second instance is
//! still validated by the core handler with the vox-core validators (`player::sanitize_player_command` for every
//! `Player` command, `AppRoute::validated`, `ids::is_valid_read_id` + the 128-unit cap for ids, `is_http_url` for
//! `OpenExternal`, a JSON-object check for `UpdateSettings` → `SettingsSaved{Err("Invalid settings patch")}`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::Sender;
use vox_core::analytics::AnalyticsRange;
use vox_core::app::{AppPage, AppRoute, Toast};
use vox_core::history::HistoryQuery;
use vox_core::hotkey::HotkeyAction;
use vox_core::player::{PlayerCommand, PlayerSnapshot};
use vox_core::read::{BridgeReadRequest, ReadResult, ReadTextOptions};
use vox_core::tts::{AudioBytes, CachedChunk};
use vox_core::voices::{AccountInfo, ApiKeyResult, TtsModel, Voice};
use vox_tts::EngineEvent;
use vox_win::media::MediaAction;
use vox_win::selection::SelectionEvent;
use vox_win::system::SystemEvent;

/// Correlates a UI request with its `UiUpdate` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RequestId(pub u64);

/// Which folder to open in Explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealTarget {
    UserData,
    Extension,
}

/// Tray menu actions (shell.md §7.3). `Open` and `OpenPage` are handled **on the UI thread** inside the tray event
/// handler (navigate + `windowing::main_window::show`, which needs the short-lived foreground permission of the tray
/// click); they never reach the core. The rest arrive as `Command::Tray`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Left click, double click or keyboard select (Enter/Space) on the icon, or "Open Vox" (BRIEF.md "Tray").
    Open,
    ReadClipboard,
    /// "Pause"/"Resume": a plain toggle (not the hotkey's replay behaviour).
    TogglePause,
    Stop,
    OpenPage(AppPage),
    /// Checkbox items write the negation of the value seen when the menu was built.
    SetRightClickChip(bool),
    SetWidgetEnabled(bool),
    SetHotkeysEnabled(bool),
    Quit,
}

/// Which native window an event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Main,
    Widget,
    Chip,
}

/// Facts the UI thread reports about the native windows.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowEvent {
    /// The main window became visible/hidden or gained/lost focus (toast routing, credits "wake"). `AppUi::create`
    /// sends the first one right after the startup visibility decision (the toast router's `ui_ready`).
    MainState { visible: bool, focused: bool },
    /// The widget became visible/hidden (toast routing, auto-hide bookkeeping).
    WidgetVisible(bool),
    /// A user drag of the widget ended (window top-left in DIP, already rounded) → `position: remember`.
    WidgetDragged { x: i32, y: i32 },
    /// A close request reached an **overlay** (`WM_CLOSE` from taskkill without /f, an installer…; Alt+F4 is impossible
    /// without focus): `Widget` | `Chip` only. The core logs `<widget|chip> window got a close request; quitting`, runs
    /// `session_end()` (synchronous persist + flush) and quits. The main window's close is handled on the UI thread
    /// (close to tray, or `Command::Quit`).
    CloseRequested(WindowKind),
}

/// A call from a bridge connection thread.
#[derive(Debug)]
pub enum BridgeCall {
    /// `POST /read`: the core answers on `reply` (the bridge waits until its deadline, then answers 504).
    Read { request: BridgeReadRequest, reply: Sender<ReadResult> },
}

/// Results of catalog / account work done on worker threads. `generation` = the core's key generation when the job
/// was started (`core::catalog`); results from an older generation (the key was replaced or removed meanwhile) are
/// dropped.
#[derive(Debug, Clone)]
pub enum CatalogResult {
    Voices { generation: u64, refresh: bool, result: Result<Arc<Vec<Voice>>, String> },
    Models { generation: u64, result: Result<Arc<Vec<TtsModel>>, String> },
    /// `Ok(None)` = the key cannot read account info (restricted). `seq` = the credits state machine's request number
    /// (`core::credits::CreditsMachine::start`): only the latest request settles the view.
    Account { generation: u64, forced: bool, seq: u64, result: Result<Option<AccountInfo>, String> },
    ApiKey { req: RequestId, key: String, result: ApiKeyResult },
    Preview { voice_id: String, result: Result<AudioBytes, String> },
}

/// Who asked for a replay (decides the failure feedback when the cached audio turns out to be unusable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOrigin {
    /// `Command::Replay` (UI): the answer is `ReadStarted{req}`; the UI toasts failures.
    Request,
    /// The play/pause hotkey's replay branch (`toggle_playback`): a miss re-synthesizes when the full text exists,
    /// else the `Nothing is playing…` toast.
    TogglePlayback,
}

/// Everything the core thread handles.
#[derive(Debug)]
pub enum Command {
    // ── reading (reader-playback.md §2.2) ──
    /// Typed/pasted text (Quick read, onboarding sample, Test voice, widget retry fallback).
    ReadText { req: Option<RequestId>, text: String, options: ReadTextOptions },
    /// Read clipboard (Home, tray, hotkey, CLI): the core spawns a worker that runs
    /// `vox_win::clipboard::read_clipboard_text()` (may block on delayed rendering) and answers `ClipboardText`.
    ReadClipboard { req: Option<RequestId> },
    /// The clipboard worker's result: blank/`None` → info toast `Copy some text first` + `ReadStarted{req,
    /// clipboard_empty()}`; else `reader::read(ReadRequest::new(text, Clipboard))` → `ReadStarted{req}`.
    ClipboardText { req: Option<RequestId>, text: Option<String> },
    Replay { req: Option<RequestId>, read_id: String },
    /// The cache I/O worker's answer to a replay's `load_async` (`None` = missing/broken entry) → reader-playback.md
    /// §2.6 step 2 (play cached) or step 3 (miss path).
    ReplayLoaded { req: Option<RequestId>, read_id: String, origin: ReplayOrigin, chunks: Option<Vec<CachedChunk>> },
    /// The read-selection hotkey / chip activation path → selection service capture with this source. (Contract
    /// variant: today the hotkey handler and the chip call the selection service directly.)
    #[allow(dead_code)]
    ReadSelection(vox_core::history::ReadSource),
    /// A fully prepared request from a core worker (`--read-file` extraction: source `explorer`, sourceApp
    /// `Explorer`, sourceProcess `explorer.exe`, sourceTitle = file name) → `reader::read`. Extraction failures come
    /// back as `Toast` instead.
    ReadPrepared(vox_core::read::ReadRequest),
    /// The play/pause hotkey (`togglePlayback`: toggle, or replay the newest cached read, or toast). (Contract
    /// variant: today `Hotkey(PlayPause)` calls the handler directly.)
    #[allow(dead_code)]
    TogglePlayback,
    /// Any transport command (sanitized first); `Stop` also stops synthesis and, when a session existed, hides the
    /// widget (`UiUpdate::Widget(Hide)`) for every source.
    Player(PlayerCommand),
    /// Widget speed pill: `next_rate(player.snapshot().state.rate)` now + persist `playbackRate`.
    CycleRate,

    // ── settings ──
    /// JSON merge patch (contracts.md §4.3). Special keys are handled by the UI controllers before sending
    /// (model change → language reset; rate/volume also send `Player`). Answered by `UiUpdate::Settings` (always,
    /// changed or not) and, when `req` is set, `UiUpdate::SettingsSaved{req}` right after it.
    UpdateSettings { req: Option<RequestId>, patch: serde_json::Value },
    SetApiKey { req: RequestId, key: String },
    ClearApiKey { req: RequestId },
    /// The hotkey recorder is recording (suspend all global hotkeys; 30 s safety resume → `UiUpdate::HotkeysResumed`).
    SuspendHotkeys(bool),

    // ── catalog / credits / previews ──
    FetchVoices { refresh: bool },
    FetchModels { refresh: bool },
    /// Retry / Refresh buttons and Analytics mount. Main-window wake is derived by the core from `MainState`.
    RefreshCredits { force: bool },
    /// Toggle the sample of a voice (stop if it is the one playing).
    PreviewVoice { voice_id: String },
    StopPreview,

    // ── history / analytics / cache ──
    QueryHistory { req: RequestId, query: HistoryQuery, with_grand_total: bool },
    /// Home data: analytics `7d` + the 5 newest reads.
    QueryHome { req: RequestId },
    QueryAnalytics { req: RequestId, range: AnalyticsRange },
    RemoveReads { req: RequestId, ids: Vec<String> },
    ClearHistory { req: RequestId },
    ToggleFavorite { req: RequestId, id: String },
    /// `owner` = the main window's HWND (`windowing::hwnd_of`), so the save dialog is modal to it.
    ExportHistory { req: RequestId, owner: Option<isize> },
    CountExpiring { req: RequestId, days: u32 },
    QueryCacheStats { req: RequestId },
    /// On the core thread: `cache.clear()` (index-only) → `history.set_cached(<records with cached>, false)` →
    /// `CacheStats{req}`.
    ClearCache { req: RequestId },
    /// Put text on the clipboard (History "Copy text", copy chips). Answered with `Done{req}`; the core never toasts
    /// for it (the requesting controller toasts its own copy).
    CopyText { req: RequestId, text: String },
    /// Persist the Analytics range (ui-state.json).
    SaveAnalyticsRange(AnalyticsRange),
    /// Persist the main window placement (window-state.json); sent by the UI on hide-to-tray and 1 s after the last
    /// move/resize (debounced on the UI thread). The quit-time save is done by the UI itself (`AppUi::apply(Quit)`).
    SaveWindowState(vox_store::window_state::WindowState),

    // ── app shell ──
    ShowMain(Option<AppRoute>),
    /// Header "Send to tray" (D-UM-28): save the window state, then `UiUpdate::HideMain`.
    HideMain,
    /// `is_http_url` else ignored (logged). With `req`: answered `Done{req, Err}` and the requester toasts (Account,
    /// onboarding: id `open-external`, `copy::open_link_failed`); without: failures are only logged (About links).
    OpenExternal { url: String, req: Option<RequestId> },
    RevealPath(RevealTarget),
    QueryAppInfo,
    Quit,

    // ── sources ──
    Window(WindowEvent),
    Tray(TrayAction),
    Hotkey(HotkeyAction),
    Selection(SelectionEvent),
    Media(MediaAction),
    Bridge(BridgeCall),
    /// Our own argv at startup, or a second instance's (`from_second_instance`). Deferred until the core's startup
    /// work finished, then handled FIFO (the primary's own `Cli` is always first: `app::run` enqueues it before the
    /// instance listener starts). Never sent in shots mode.
    Cli { argv: Vec<String>, cwd: PathBuf, from_second_instance: bool },
    System(SystemEvent),
    /// Log-off / shutdown: persist playback + flush settings and history synchronously, then reply.
    SessionEnd(Sender<()>),
    /// A toast raised by a core worker (`--read-file` extraction failure, …): routed by `core::toasts` exactly like a
    /// core-raised toast (`UiUpdate::Toast`, then `Widget(Show)` or an OS notification).
    Toast(Toast),

    // ── internal ──
    Engine(EngineEvent),
    /// The audio cache's I/O worker finished a `save_async` for a completed read (`Err` = message for the log).
    CacheSaved { read_id: String, result: Result<(), String> },
    /// A report from the reading player.
    PlayerReport(PlayerSnapshot),
    /// A report from the voice-preview player.
    PreviewReport(PlayerSnapshot),
    /// The preview player failed (and restarted): end the sample with `PreviewUpdate::Failed`.
    PreviewFailed { message: String },
    /// The reading player failed and restarted itself (`vox_audio::PlayerEvent::Failed`): stop the engine when
    /// `session_id` is the engine's session, log, and show the widget (error card) when enabled.
    PlayerFailed { session_id: Option<String>, message: String },
    /// Debounced history-store change notification.
    HistoryChanged,
    Catalog(CatalogResult),
}

/// Cloneable sender used by every source.
#[derive(Clone)]
pub struct CoreHandle {
    tx: Sender<Command>,
    next_id: Arc<AtomicU64>,
}

impl CoreHandle {
    pub fn new(tx: Sender<Command>) -> Self {
        Self { tx, next_id: Arc::new(AtomicU64::new(1)) }
    }
    /// Enqueues a command (dropped silently after the core exited).
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
    /// A fresh request id.
    pub fn request_id(&self) -> RequestId {
        RequestId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}
