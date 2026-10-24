//! Every cross-cutting constant from the port specs (contracts.md §2, shell.md §11, tts.md §9,
//! reader-playback.md §2.1/§4.1/§6, selection.md §5.1/§8.1, integrations.md §1.1, persistence.md §7.1).
//!
//! Durations are milliseconds (`*_MS`, `u64`) or seconds (`*_SEC`, `f64`). Geometry is DIP (`f64`).
//! Text lengths are UTF-16 code units.

// ───────────────────────────── product ─────────────────────────────

/// The single source of the product name (contracts.md §2.1). UI strings never hard-code "Vox".
pub const APP_NAME: &str = "Vox";
/// App User Model ID (toast attribution, taskbar grouping, installer shortcuts).
pub const APP_ID: &str = "com.vox.reader";
/// Context chip label and Explorer verb label: `Read with Vox`.
pub const READ_WITH_LABEL: &str = "Read with Vox";
/// The selection-popup chip label.
pub const READ_LABEL: &str = "Read";
/// App version shown in About, `/health` and logs (workspace version).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// ───────────────────────────── ElevenLabs ─────────────────────────────

pub const ELEVENLABS_BASE_URL: &str = "https://api.elevenlabs.io";
/// "George", a premade voice.
pub const ELEVENLABS_DEFAULT_VOICE_ID: &str = "JBFqnCBsd6RMkjVDRZzb";
pub const ELEVENLABS_DEFAULT_VOICE_NAME: &str = "George";
pub const ELEVENLABS_DEFAULT_MODEL_ID: &str = "eleven_flash_v2_5";
/// Where "Get a key" / "Manage keys" lead.
pub const ELEVENLABS_API_KEYS_URL: &str = "https://elevenlabs.io/app/settings/api-keys";
pub const ELEVENLABS_DASHBOARD_URL: &str = "https://elevenlabs.io/app";
pub const ELEVENLABS_BILLING_URL: &str = "https://elevenlabs.io/app/subscription";
pub const ELEVENLABS_DOCS_URL: &str = "https://elevenlabs.io/docs";
/// Synthesis timeout = max(SYNTH_TIMEOUT_MS, chars × SYNTH_MS_PER_CHAR) per HTTP request.
pub const SYNTH_TIMEOUT_MS: u64 = 30_000;
pub const SYNTH_MS_PER_CHAR: u64 = 50;
/// Voices / models / subscription / key validation timeout (per page).
pub const LIST_TIMEOUT_MS: u64 = 15_000;
pub const VOICES_TTL_MS: u64 = 600_000;
pub const MODELS_TTL_MS: u64 = 3_600_000;
pub const ACCOUNT_TTL_MS: u64 = 60_000;
pub const MAX_VOICE_PAGES: usize = 25;
pub const VOICE_PAGE_SIZE: usize = 100;
/// Error message cap for non-JSON error bodies and "other HTTP" messages.
pub const ERROR_MESSAGE_CAP: usize = 300;
/// Keys longer than this are rejected before validation (`That doesn't look like an API key`).
pub const MAX_API_KEY_LEN: usize = 256;

// ───────────────────────────── TTS engine ─────────────────────────────

pub const FIRST_CHUNK_MAX: usize = 220;
pub const CHUNK_MAX: usize = 900;
pub const RETRY_DELAYS_MS: [u64; 3] = [1_000, 2_000, 4_000];
pub const MAX_RETRY_AFTER_MS: u64 = 10_000;
pub const MOCK_LATENCY_FIRST_MS: u64 = 350;
pub const MOCK_LATENCY_NEXT_MS: u64 = 120;
pub const CONTEXT_CHARS: usize = 300;
/// Max seconds of delivered-but-unplayed audio before synthesis waits for the listener.
pub const LOOKAHEAD_SEC: f64 = 120.0;
/// Chunks `[0, LOOKAHEAD_FREE_CHUNKS)` are requested without waiting.
pub const LOOKAHEAD_FREE_CHUNKS: usize = 2;
/// Speech-rate guess used before real durations exist.
pub const ESTIMATED_CHARS_PER_SECOND: f64 = 15.0;
pub const MOCK_SAMPLE_RATE: u32 = 22_050;

// ───────────────────────────── reader ─────────────────────────────

/// Text sliced from UI/bridge input before the reader sees it.
pub const MAX_READ_TEXT: usize = 2_000_000;
/// Characters kept on a record when `library.storeText` is off.
pub const PREVIEW_CHARS: usize = 160;
pub const HISTORY_WRITE_INTERVAL_MS: u64 = 2_000;
/// Max length of `previewTitle` (player title, tray tooltip, media overlay).
pub const TITLE_CHARS: usize = 64;
pub const RAW_CUT_FACTOR: usize = 4;
pub const RAW_CUT_MIN: usize = 200_000;
pub const PARTIAL_READ_ERROR: &str = "Only part of the text could be synthesized";
pub const MAX_TRACKED_SESSIONS: usize = 16;
/// Cache limit unit: decimal MB.
pub const CACHE_MB: u64 = 1_000_000;
/// Stand-in for text that was not stored (preview-only cached replays).
pub const HIDDEN_MARK: &str = "\u{2026}";
/// `sourceTitle` stored on a record is trimmed and cut to this many units.
pub const SOURCE_TITLE_MAX: usize = 200;
/// `--read-file` reads this many characters past `text.maxChars` so truncation is detectable.
pub const READ_FILE_EXTRA_CHARS: usize = 1_000;
/// Max file bytes read by the file text extractor.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

// ───────────────────────────── player ─────────────────────────────

/// Client-side rates offered in the UI (defaults.ts:84).
pub const PLAYBACK_RATES: [f64; 6] = [0.75, 1.0, 1.25, 1.5, 1.75, 2.0];
pub const RATE_MIN: f64 = 0.5;
pub const RATE_MAX: f64 = 3.0;
/// Seeks never land closer than this to the end of available audio / of a chunk.
pub const END_GUARD_SEC: f64 = 0.05;
/// "Previous sentence" restarts the current sentence when more than this many wall seconds into it.
pub const BACK_GRACE_SEC: f64 = 1.2;
/// Prev/next step when sentence positions are unknown (partial text).
pub const PARTIAL_SKIP_SEC: f64 = 10.0;
/// Default `skip` amount with `unit: seconds`.
pub const DEFAULT_SKIP_SEC: f64 = 10.0;
pub const IDLE_OUTPUT_HOLD_MS: u64 = 30_000;
pub const PAUSED_OUTPUT_HOLD_MS: u64 = 60_000;
/// Player state report cadence while playing (core-facing; the UI is refreshed at UI_FRAME_MS).
pub const PLAYER_REPORT_MS: u64 = 500;
/// Floor of an estimated chunk duration.
pub const MIN_CHUNK_DURATION_SEC: f64 = 0.1;
/// Output warmer: audio the device must have rendered before speech starts.
pub const WARM_MS: u64 = 1_000;
/// Output warmer: a warm-up never holds playback longer than this.
pub const WARM_MAX_WAIT_MS: u64 = 3_000;
pub const WARM_POLL_MS: u64 = 25;
/// Output warmer: after the stream could not be created, don't retry for this long.
pub const WARM_RETRY_MS: u64 = 60_000;
/// Sentences longer than this are soft-split at clause punctuation (sentence skipping).
pub const LONG_SENTENCE: usize = 280;
pub const SOFT_TARGET: usize = 200;

// ───────────────────────────── widget / UI timing ─────────────────────────────

pub const AUTO_HIDE_MS: u64 = 4_000;
pub const HOLD_RETRY_MS: u64 = 1_500;
pub const WIDGET_TOAST_MS: u64 = 2_500;
pub const WIDGET_TOAST_ACTION_MS: u64 = 5_000;
pub const WIDGET_TOAST_HOVER_RECHECK_MS: u64 = 1_000;
/// App-window toast durations (ui-main.md §1.7).
pub const TOAST_INFO_MS: u64 = 4_000;
pub const TOAST_SUCCESS_MS: u64 = 3_000;
pub const TOAST_ERROR_MS: u64 = 6_500;
pub const TOAST_ACTION_MIN_MS: u64 = 6_000;
pub const MAX_TOASTS: usize = 4;
pub const TOAST_EXIT_MS: u64 = 180;
/// UI refresh cadence for the playing state (≈30 Hz).
pub const UI_FRAME_MS: u64 = 33;
/// Home / History trailing refresh debounce.
pub const PAGE_REFRESH_DEBOUNCE_MS: u64 = 700;
pub const HISTORY_SEARCH_DEBOUNCE_MS: u64 = 220;
pub const HISTORY_PAGE_SIZE: usize = 50;
/// Credits: forced refresh after a read settles; wake throttle.
pub const CREDITS_AFTER_READ_MS: u64 = 1_500;
pub const CREDITS_WAKE_THROTTLE_MS: u64 = 30_000;
/// Settings library: re-measure cache this long after the last history change.
pub const CACHE_REMEASURE_MS: u64 = 2_500;
pub const RETENTION_SETTLE_MS: u64 = 450;
pub const HOTKEY_REJECT_MESSAGE_MS: u64 = 6_000;
pub const HOTKEY_HINT_MS: u64 = 1_800;
pub const COPY_CONFIRM_MS: u64 = 1_400;
pub const BRIDGE_RECHECK_MAX_MS: u64 = 4_000;

// ───────────────────────────── windows ─────────────────────────────

pub const WIDGET_MARGIN: f64 = 24.0;
pub const DEFAULT_WIDGET_SIZE: (f64, f64) = (380.0, 168.0);
pub const WIDGET_SIZE_MIN: (f64, f64) = (40.0, 24.0);
pub const WIDGET_SIZE_MAX: (f64, f64) = (1200.0, 900.0);
pub const HIT_RECT_MIN: f64 = 8.0;
pub const DEFAULT_CHIP_SIZE_CONTEXT: (f64, f64) = (153.0, 38.0);
pub const DEFAULT_CHIP_SIZE_SELECTION: (f64, f64) = (82.0, 38.0);
pub const CHIP_SIZE_MIN: (f64, f64) = (32.0, 24.0);
pub const CHIP_SIZE_MAX: (f64, f64) = (480.0, 160.0);
/// User-drag notifications are ignored for this long after our own move/resize.
pub const OWN_MOVE_GRACE_MS: u64 = 500;
pub const HOVER_POLL_MS: u64 = 100;
/// Chip exit: 120 ms UI animation + a frame.
pub const CHIP_EXIT_MS: u64 = 140;
pub const CONTEXT_MENU_SMALL: (f64, f64) = (220.0, 240.0);
pub const CONTEXT_MENU_GUESS: (f64, f64) = (320.0, 360.0);
pub const CONTEXT_MENU_LARGE: (f64, f64) = (360.0, 480.0);
pub const CHIP_GAP: f64 = 18.0;
pub const CHIP_GAP_V: f64 = 20.0;
pub const SELECTION_CHIP_GAP: f64 = 20.0;
pub const NEAR_CURSOR_DX: f64 = 16.0;
pub const NEAR_CURSOR_DY: f64 = 20.0;
pub const NEAR_CURSOR_FLIP: f64 = 16.0;
pub const MAIN_WINDOW_SIZE: (f64, f64) = (1180.0, 780.0);
pub const MAIN_WINDOW_MIN: (f64, f64) = (940.0, 640.0);
/// Below this main-window width (logical px = DIP) the sidebar collapses to icons: ui-main.md §2.6 `RAIL_QUERY`
/// `max-width: 1099px`. DESIGN.md's 900 px assumed a smaller minimum window; the minimum stays 940, so the rail uses
/// the spec's 1100 (ARCHITECTURE.md §8 D-UM-29).
pub const SIDEBAR_RAIL_BELOW: f64 = 1100.0;
pub const CUSTOM_COORD_LIMIT: f64 = 100_000.0;
/// Tray tooltip limit (UTF-16 units).
pub const TOOLTIP_MAX: usize = 127;

// ───────────────────────────── lifecycle ─────────────────────────────

pub const SHUTDOWN_TIMEOUT_MS: u64 = 3_000;
pub const FORCE_EXIT_MS: u64 = 4_000;
pub const RETENTION_INTERVAL_MS: u64 = 6 * 3_600_000;
pub const RETENTION_CHANGE_DELAY_MS: u64 = 60_000;
pub const SHORTCUT_SUSPEND_MAX_MS: u64 = 30_000;
/// The Settings recorder re-arms the hotkey suspension this often while it records with the main window focused and
/// visible (ARCHITECTURE.md §8 D-USW-18).
pub const HOTKEY_SUSPEND_REARM_MS: u64 = 20_000;
/// Detached watchdog armed by the first quit request: `std::process::exit(0)` if the process is still alive after
/// the bounded flush budget plus the UI's hard exit (shell.md §3.5 worst case ≈ 7 s; ARCHITECTURE.md §4.17).
pub const SHUTDOWN_WATCHDOG_MS: u64 = SHUTDOWN_TIMEOUT_MS + FORCE_EXIT_MS;
/// Each of `player.shutdown()` ×2 and `media.shutdown()` runs on a helper thread joined for at most this long.
pub const SERVICE_SHUTDOWN_JOIN_MS: u64 = 500;
/// The system watcher waits this long for the core's `SessionEnd` reply …
pub const SESSION_END_WAIT_MS: u64 = 5_000;
/// … and flushes settings + history itself (`flush_sync`) when no reply came within this long.
pub const SESSION_END_FALLBACK_MS: u64 = 2_500;
/// The reading player builds its output stream this long after core startup, whether or not a window is shown
/// (reader-playback.md §6 preload; skipped with `null_output`).
pub const PRELOAD_OUTPUT_DELAY_MS: u64 = 1_000;
pub const DAY_MS: f64 = 86_400_000.0;

// ───────────────────────────── persistence ─────────────────────────────

pub const SETTINGS_DEBOUNCE_MS: u64 = 250;
pub const SETTINGS_MAX_WAIT_MS: u64 = 2_000;
pub const HISTORY_DEBOUNCE_MS: u64 = 2_500;
pub const HISTORY_MAX_WAIT_MS: u64 = 10_000;
/// History change notification: store debounce / max wait, then the UI throttle.
pub const HISTORY_NOTIFY_DEBOUNCE_MS: u64 = 100;
pub const HISTORY_NOTIFY_MAX_WAIT_MS: u64 = 400;
pub const HISTORY_CHANGED_THROTTLE_MS: u64 = 150;
/// Windows sharing-violation retry delays (5 retries, 6 attempts).
pub const FILE_RETRY_DELAYS_MS: [u64; 5] = [15, 40, 90, 180, 350];
pub const MAX_WRITE_RETRIES: u32 = 5;
pub const LISTEN_LOG_MAX_KEYS: usize = 400;
pub const LOG_ROTATE_BYTES: u64 = 1_048_576;
pub const LOG_ARG_MAX_CHARS: usize = 2_000;

// ───────────────────────────── analytics ─────────────────────────────

pub const SILENT_READING_WPM: f64 = 238.0;
pub const TOP_N: usize = 6;
/// Largest finite analytics range in days.
pub const MAX_DAILY_DAYS: usize = 365;
/// `all` analytics window cap: `startDay = max(local day of the oldest valid record, today − (MAX_ALL_DAYS − 1))`, so
/// one malformed `createdAt` (0, −1e14) cannot produce a 20 000-entry daily series; older records fall outside the
/// window everywhere (ARCHITECTURE.md §8 D-AN-2).
pub const MAX_ALL_DAYS: usize = 3_653;
/// Daily listening goal on Home (15 minutes).
pub const DAILY_GOAL_SEC: f64 = 900.0;
/// Credits below this share are "low".
pub const LOW_CREDITS_SHARE: f64 = 0.15;

// ───────────────────────────── bridge ─────────────────────────────

pub const LOCAL_SERVER_DEFAULT_PORT: u16 = 47_823;
/// The ElevenLabs client re-reads the Windows proxy before a read / catalog fetch when the last check is older than
/// this (ARCHITECTURE.md §8 D-NET-1).
pub const PROXY_RECHECK_MS: u64 = 30_000;
pub const LOCAL_SERVER_CLIENT_HEADER: &str = "x-vox-client";
pub const LOCAL_SERVER_CLIENT_VALUE: &str = "browser-extension";
pub const BRIDGE_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const BRIDGE_TIMEOUT_MS: u64 = 15_000;
pub const BRIDGE_HEADERS_TIMEOUT_MS: u64 = 10_000;
pub const BRIDGE_KEEP_ALIVE_MS: u64 = 5_000;
pub const BRIDGE_MAX_CONNECTIONS: usize = 64;
pub const BRIDGE_IDLE_SOCKET_MS: u64 = 20_000;
pub const BRIDGE_MAX_HEADER_BYTES: usize = 16 * 1024;
pub const BRIDGE_URL_MAX: usize = 2_048;
pub const BRIDGE_TITLE_MAX: usize = 300;
/// http(s) URLs kept on a record are at most this long.
pub const HTTP_URL_MAX: usize = 4_096;

// ───────────────────────────── selection / capture ─────────────────────────────

pub const MODIFIER_WAIT_MS: u64 = 700;
pub const MODIFIER_POLL_MS: u64 = 10;
pub const CAPTURE_SETTLE_MS: u64 = 40;
pub const CONTEXT_MENU_SETTLE_MS: u64 = 120;
pub const CLIPBOARD_POLL_MS: u64 = 15;
pub const CLIPBOARD_POLL_TIMEOUT_MS: u64 = 450;
pub const CLIPBOARD_TEXT_SETTLE_MS: u64 = 1_500;
pub const ESCAPE_SETTLE_MS: u64 = 80;
pub const CLIPBOARD_RESTORE_DELAY_MS: u64 = 150;
pub const LATE_COPY_GRACE_MS: u64 = 1_500;
pub const LATE_POLL_MS: u64 = 25;
pub const CHIP_ACTIVATION_DEDUPE_MS: u64 = 300;
/// The selection consumer re-installs the hook when it saw no hook event for this long while `GetLastInputInfo`
/// reports newer input (Windows silently removes slow LL hooks; selection.md §14).
pub const HOOK_SILENCE_REINSTALL_MS: u64 = 10_000;
pub const DRAG_THRESHOLD_PX: f64 = 6.0;
pub const MULTI_CLICK_MS: u64 = 500;
pub const MULTI_CLICK_PX: f64 = 4.0;
pub const HINT_TTL_MS: u64 = 60_000;
pub const SELECTION_CHIP_MS: u64 = 4_000;
pub const CONTEXT_CHIP_MS: u64 = 8_000;
/// Window titles are whitespace-collapsed and cut to this many units.
pub const WINDOW_TITLE_MAX: usize = 300;
/// Max excluded apps kept by the settings sanitizer.
pub const EXCLUDED_APPS_MAX: usize = 200;

// ───────────────────────────── shell ─────────────────────────────

/// Explorer verb file types, in registry order.
pub const EXPLORER_EXTENSIONS: [&str; 9] = [".txt", ".md", ".markdown", ".log", ".csv", ".json", ".rtf", ".html", ".htm"];
/// Explorer verb key name (mirrored by the uninstaller).
pub const EXPLORER_VERB: &str = "VoxRead";
/// Launch-at-login value name (dev builds use `Vox (dev)`).
pub const RUN_VALUE_NAME: &str = "Vox";
pub const RUN_VALUE_NAME_DEV: &str = "Vox (dev)";
pub const HIDDEN_ARG: &str = "--hidden";
/// Named-pipe / mutex key of the single-instance lock (suffixed with `-Demo` in demo mode).
pub const SINGLE_INSTANCE_KEY: &str = "com.vox.reader";
