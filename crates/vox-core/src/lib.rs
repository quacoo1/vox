//! # vox-core
//!
//! The shared contracts of the Vox rewrite: every type that crosses a crate boundary, the settings schema with its
//! defaults and sanitize rules, history records, the listen-log ledger, player / read / TTS stream types, the pure
//! formatting helpers the UI needs, hotkey parsing and validation, window placement geometry and constants.
//!
//! Rules for this crate (docs/ARCHITECTURE.md §2.1):
//! - **Pure.** No file, network or registry access, no Windows APIs, no threads. Everything here is unit-tested.
//! - **Text offsets are UTF-16 code units** (`u32`) wherever they are persisted or cross a crate boundary
//!   (see [`text16`] and ARCHITECTURE.md §5.6). Rust strings stay UTF-8; conversions happen only through
//!   [`text16`].
//! - **JSON compatibility.** Serialized forms match what the Electron app wrote (field names, field order, optional
//!   fields omitted, integral numbers without a fraction). See [`json`].
//! - Local-time computations are generic over [`chrono::TimeZone`] so they can be tested with fixed offsets;
//!   production code passes [`chrono::Local`].

pub mod analytics;
pub mod app;
pub mod cache;
pub mod charts;
pub mod cli;
pub mod constants;
pub mod copy;
pub mod format;
pub mod geometry;
pub mod history;
pub mod hotkey;
pub mod ids;
pub mod js;
pub mod json;
pub mod listen_log;
pub mod locale;
pub mod player;
pub mod providers;
pub mod read;
pub mod settings;
pub mod text16;
pub mod time;
pub mod timing;
pub mod tts;
pub mod voices;

pub use analytics::{AnalyticsRange, AnalyticsSummary};
pub use app::{AppInfo, AppPage, AppRoute, ChipMode, RuntimeFlags, Toast, ToastAction, ToastKind};
pub use cache::CacheStats;
pub use history::{HistoryPage, HistoryQuery, ReadRecord, ReadSource, RecordPatch};
pub use listen_log::ListenLog;
pub use player::{PlayerCommand, PlayerSnapshot, PlayerState, PlayerStatus, SessionInfo, SessionWarning};
pub use read::{ForegroundInfo, ReadErrorCode, ReadRequest, ReadResult};
pub use settings::{Settings, ThemeMode, WidgetPosition};
pub use tts::{Alignment, AudioBytes, CachedChunk, TextChunk, TtsChunk, TtsSessionDone, TtsSessionError, TtsSessionStart};
pub use voices::{AccountInfo, TtsModel, Voice};
