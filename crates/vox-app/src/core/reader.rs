//! The read controller (reader-playback.md §2). Runs on the core thread.
//!
//! - `read(req)`: key gate (unless mock) → `pre_cut` → `clean_text` (fallback: JS-trimmed raw) → empty check →
//!   `truncate_text` (+ toast `Reading the first N characters`) → record (if `saveHistory`; 160-unit char-safe preview
//!   when `storeText` is off) → `engine.start` (+ `sessions.track` immediately after) → show the widget if enabled,
//!   once the player reports the new session (`show_widget_for`; no widget-readiness wait: the player exists before any
//!   read). Before a read the core re-reads the Windows proxy when the last check is ≥ 30 s old (`client.set_proxy` on
//!   change, D-NET-1).
//! - `read_clipboard(req)`: spawns a core worker that runs `vox_win::clipboard::read_clipboard_text()` (it may block
//!   on a delayed-rendering owner such as Excel) and sends `Command::ClipboardText{req, text}`;
//!   `on_clipboard_text`: blank → info toast `Copy some text first` + `ReadStarted{req, clipboard_empty()}`; else
//!   `read(ReadRequest::new(text, Clipboard))` → `ReadStarted{req}`.
//! - `replay(req, id, origin)`: invalid id / not in history → answered at once; `cache.has(id)` → `cache.load_async`
//!   (the I/O worker reads every chunk and touches `lastAccess`; up to ~100 MB, never on the core thread) →
//!   `Command::ReplayLoaded` → `on_replay_loaded`: the record is re-read, then reader-playback.md §2.6 step 2 (≥ 1 chunk:
//!   `engine.play_cached` with `vox_tts::cached::preview_replay` for preview-only records, accumulating into the
//!   original record; + track + widget Show) or step 3 (miss: stale `cached` cleared; re-synthesize as a `replay`
//!   record with the record's voice; `not-cached` for preview-only without audio). `origin = TogglePlayback` turns a
//!   final failure into the `Nothing is playing…` toast.
//! - CLI actions (`--read-text` > `--read-clipboard` > `--read-file`), `read_file` (a worker runs
//!   `vox_text::read_text_file(path, maxChars + 1000)`; success → `Command::ReadPrepared`, source `explorer`; failure →
//!   warn `read-file failed: <e>` (never the text) and `Command::Toast(Toast::error(copy::couldnt_open(name)))` with
//!   `name` = the resolved path's file name (lossy), routed by `core::toasts`).
//! - `toggle_playback()` (toggle when status ≠ idle or an engine session exists; else replay the newest record if it is
//!   cached and `cache.has` it — origin `TogglePlayback`; else the "Nothing is playing" toast).
//! - `command(cmd)`: `vox_core::player::sanitize_player_command` first (invalid → dropped, logged at debug). For
//!   `Stop`: `had = services.player.snapshot().session.is_some() || engine.current_session_id().is_some()` **before**
//!   forwarding (the engine's session covers a Stop handled before the player thread processed the new session's
//!   `Start`), then `player.command(Stop)`, `engine.stop()`, and if `had` → `UiUpdate::Widget(WidgetIntent::Hide)` —
//!   for every source (UI, tray, hotkey, SMTC, bridge); the echo of the widget's own Close is a harmless second hide.
//!   Stop while idle publishes nothing.
//! - Engine events (matched by `read_id` through `Sessions::latest_for`, never by "current session": a trailing
//!   `Stopped` for a superseded read may arrive after events of the newer read, D-NET-4):
//!   - `Complete{chunks, billed}`: the session's record still exists and is saved, not from cache → patch
//!     `{audioDurationSec: round1(Σ finite durations), charactersBilled: max(0, js_round(billed))}` at once; then if
//!     `library.cacheAudio` and there are chunks → `cache.save_async(id, if rec.text_truncated { without_text(chunks)
//!     } else { chunks })`, whose callback sends `Command::CacheSaved`. `on_cache_saved`: `Err` → log `audio cache save
//!     failed` (`cached` stays false); `Ok` and `history.get(id)` is `None` (deleted meanwhile) → `cache.remove(id)`
//!     (no orphan); `Ok` otherwise → `prune_cache(&[id])` → `history.set_cached(evicted, false)` →
//!     `history.update(id, {cached: true})`. Record gone at `Complete` → nothing cached (§12 #17).
//!   - `Failed`: record error + toast with "Open settings" when `error_needs_settings`.
//!   - `Stopped{billed, error}`: write `charactersBilled = max(0, js_round(billed))` (absolute; a second, larger
//!     cumulative event only raises it); `error: Some` → session `partial = true`, record error `PARTIAL_READ_ERROR`,
//!     never completed. Because the engine emits `Stopped{error}` before `sink.done`, it is always processed before
//!     the player's `ended` (§4.14); `Sessions::on_state` additionally treats a snapshot warning as partial.
//! - Player reports: store; `engine.progress`; fold into sessions → history patches; credits settle detection; tray /
//!   SMTC refresh; bridge `playing` atomic; publish `UiUpdate::Player`. `PlayerFailed` → `engine.stop()` when it is
//!   the engine's session, widget shown for the error card.
//!
//! Tests (ARCHITECTURE.md §9, vox-app row): behind a fake engine + `null_output` players + temp stores — `new_record`
//! against §2.3's table; the three `toggle_playback` branches; replay (no key needed on a hit, base values, preview-only
//! text rebuilt, stale `cached` cleared, `not-cached`); `Complete` (record gone → nothing cached; save error → cached
//! false; prune protects keep + engine read + last readId); `Stopped{error}` then an `ended` report → not completed;
//! `Failed` toast actions; history off; Stop with a session → `Widget(Hide)`, Stop while idle → nothing.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vox_core::app::{AppRoute, Toast};
use vox_core::constants::{
    CREDITS_AFTER_READ_MS, PARTIAL_READ_ERROR, PREVIEW_CHARS, RAW_CUT_FACTOR, RAW_CUT_MIN, READ_FILE_EXTRA_CHARS, SOURCE_TITLE_MAX,
    TITLE_CHARS,
};
use vox_core::copy;
use vox_core::history::{count_words, ReadRecord, ReadSource, RecordPatch};
use vox_core::ids::new_uuid;
use vox_core::js::{js_round, js_trim, round1};
use vox_core::player::{next_rate, sanitize_player_command, PlayerCommand, PlayerSnapshot, PlayerStatus};
use vox_core::read::{error_message, error_needs_settings, ReadErrorCode, ReadRequest, ReadResult, VoiceOverride};
use vox_core::text16::{prefix16, utf16_len};
use vox_core::tts::CachedChunk;
use vox_text::CleanOptions;
use vox_tts::{EngineEvent, PlayCachedOptions, StartSessionOptions};

use super::sessions::{PendingWrite, Sessions};
use super::timers::TimerId;
use super::Core;
use crate::command::{Command, ReplayOrigin, RequestId};
use crate::update::{UiUpdate, WidgetIntent};

/// Reader state kept by the core.
#[derive(Default)]
pub struct ReaderState {
    pub sessions: Sessions,
    /// The last player report (after an explicit stop: the sessionless `idle` report).
    pub last_snapshot: PlayerSnapshot,
    /// The session whose first player report shows the widget (`show_widget_for`).
    pub show_pending: Option<String>,
}

/// The reader's entry points. Implemented on `Core` (they need every service); listed here as the module's
/// contract.
pub trait Reader {
    fn read(&mut self, req: ReadRequest) -> ReadResult;
    /// Spawns the clipboard worker (module docs); the answer comes back as `Command::ClipboardText`.
    fn read_clipboard(&mut self, req: Option<RequestId>);
    fn on_clipboard_text(&mut self, req: Option<RequestId>, text: Option<String>);
    /// Validates the id and starts the replay; answers `ReadStarted{req}` at once (invalid id, not in history, cache
    /// miss) or after `ReplayLoaded`.
    fn replay(&mut self, req: Option<RequestId>, read_id: &str, origin: ReplayOrigin);
    fn on_replay_loaded(&mut self, req: Option<RequestId>, read_id: &str, origin: ReplayOrigin, chunks: Option<Vec<CachedChunk>>);
    /// `--read-file`: resolves `path` against `cwd` (`vox_core::cli::resolve_read_file`), then a worker extracts the
    /// text and sends `Command::ReadPrepared` or `Command::Toast` (module docs; ARCHITECTURE.md §4.7).
    fn read_file(&mut self, path: &str, cwd: &std::path::Path);
    fn toggle_playback(&mut self);
    fn command(&mut self, cmd: PlayerCommand);
    fn on_engine_event(&mut self, ev: EngineEvent);
    fn on_cache_saved(&mut self, read_id: &str, result: Result<(), String>);
    fn on_player_report(&mut self, snapshot: PlayerSnapshot);
    /// `pruneCache(keep)`: protect `keep` + the engine's read + the player's read; evicted → `cached = false`.
    fn prune_cache(&mut self, keep: &[String]) -> Vec<String>;
}

/// The record a fresh read creates (reader-playback.md §2.3 step 8). Pure.
#[allow(clippy::too_many_arguments)] // one argument per record source field, as documented in ARCHITECTURE.md §9
pub fn new_record(
    read_id: &str,
    now_ms: f64,
    text: &str,
    store_text: bool,
    req: &ReadRequest,
    voice_id: &str,
    voice_name: &str,
    model_id: &str,
) -> ReadRecord {
    let non_empty = |s: &Option<String>| s.clone().filter(|v| !v.is_empty());
    let mut r = ReadRecord::new(read_id.to_string(), now_ms, req.source);
    r.text = if store_text { text.to_string() } else { prefix16(text, PREVIEW_CHARS).to_string() };
    r.text_truncated = !store_text && utf16_len(text) > PREVIEW_CHARS;
    r.char_count = utf16_len(text) as u64;
    r.word_count = count_words(text);
    r.voice_id = voice_id.to_string();
    r.voice_name = voice_name.to_string();
    r.model_id = model_id.to_string();
    r.source_app = non_empty(&req.source_app);
    r.source_process = non_empty(&req.source_process);
    r.source_url = non_empty(&req.source_url);
    r.source_title =
        req.source_title.as_deref().map(|t| prefix16(js_trim(t), SOURCE_TITLE_MAX).to_string()).filter(|t| !t.is_empty());
    r
}

/// `replay`'s id rule (`requireId`): a non-empty string of at most 128 UTF-16 units.
fn valid_request_id(id: &str) -> bool {
    !id.is_empty() && utf16_len(id) <= 128
}

impl Reader for Core {
    fn read(&mut self, req: ReadRequest) -> ReadResult {
        self.maybe_recheck_proxy(false);
        let settings = self.settings();
        // 2. API key gate (skipped in mock-TTS mode).
        if !self.services.engine.is_mock() {
            match vox_core::providers::provider_of(&settings.voice_id) {
                vox_core::providers::Provider::ElevenLabs => {
                    if !self.services.settings.has_api_key() {
                        self.toast(Toast::error(copy::ADD_KEY_TO_READ).with_action(copy::ADD_KEY_ACTION, Some(AppRoute::settings("account"))));
                        return ReadResult::no_api_key();
                    }
                }
                provider => {
                    if self.services.engine.provider(provider).is_none() {
                        let message = vox_tts::not_connected(provider).message;
                        self.toast(Toast::error(message.clone()).with_action("Open settings", Some(AppRoute::settings("providers"))));
                        return ReadResult::failed(vox_core::read::ReadErrorCode::NoApiKey, &message);
                    }
                }
            }
        }
        // 3–6. Pre-cut, clean, empty check, truncate.
        let max_chars = settings.text.max_chars as usize;
        let (raw, raw_cut) = vox_text::pre_cut(&req.text, (max_chars * RAW_CUT_FACTOR).max(RAW_CUT_MIN));
        let options = CleanOptions::from(&settings.text);
        let cleaned = match catch_unwind(|| vox_text::clean_text(&raw, options)) {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!(target: "reader", "text clean-up failed; reading the trimmed text");
                js_trim(&raw).to_string()
            }
        };
        if cleaned.is_empty() {
            self.toast(Toast::info(copy::NOTHING_TO_READ));
            return ReadResult::nothing_to_read();
        }
        let (text, truncated) = vox_text::truncate_text(&cleaned, max_chars);
        if truncated || raw_cut {
            self.toast(Toast::info(copy::reading_first(settings.text.max_chars)));
        }
        // 7–9. The record exists before the engine starts.
        let read_id = new_uuid();
        let voice_id = req.voice.as_ref().map(|v| v.id.clone()).filter(|v| !v.is_empty()).unwrap_or_else(|| settings.voice_id.clone());
        let voice_name =
            req.voice.as_ref().map(|v| v.name.clone()).filter(|v| !v.is_empty()).unwrap_or_else(|| settings.voice_name.clone());
        let record_saved = settings.library.save_history;
        if record_saved {
            let rec = new_record(&read_id, super::now_unix_ms(), &text, settings.library.store_text, &req, &voice_id, &voice_name, &settings.model_id);
            self.services.history.add(rec);
        }
        // 11. Start synthesis (a superseded session's `Stopped{billed}` is enqueued synchronously first).
        let opts = StartSessionOptions {
            read_id: read_id.clone(),
            title: vox_text::preview_title(&text, TITLE_CHARS),
            text,
            voice_id,
            voice_name,
            source_app: req.source_app.clone(),
            settings: settings.clone(),
        };
        let engine = &self.services.engine;
        let session_id = match catch_unwind(AssertUnwindSafe(|| engine.start(opts))) {
            Ok(id) => id,
            Err(_) => {
                tracing::error!(target: "reader", "the TTS engine failed to start a session");
                let message = error_message(ReadErrorCode::Unknown, None);
                if record_saved {
                    self.services.history.update(&read_id, &RecordPatch { error: Some(Some(message.clone())), ..RecordPatch::default() });
                }
                self.toast(Toast::error(message.clone()));
                return ReadResult::failed_with_id(read_id, ReadErrorCode::Unknown, message);
            }
        };
        // 12. Tracked before the next command (a player report) can be processed (ARCHITECTURE.md §3.6 #2).
        self.show_widget_for(&session_id);
        self.reader.sessions.track(session_id, read_id.clone(), record_saved, false, 0.0, false);
        ReadResult::started(read_id)
    }

    fn read_clipboard(&mut self, req: Option<RequestId>) {
        let core = self.handle.clone();
        let spawned = std::thread::Builder::new().name("vox-clipboard-read".into()).spawn(move || {
            // Delayed rendering (Excel, Word) can block here for seconds: never on the core thread.
            let text = catch_unwind(vox_win::clipboard::read_clipboard_text).ok().flatten();
            core.send(Command::ClipboardText { req, text });
        });
        if let Err(e) = spawned {
            tracing::warn!(target: "reader", "cannot start the clipboard reader: {e}");
            self.on_clipboard_text(req, None);
        }
    }

    fn on_clipboard_text(&mut self, req: Option<RequestId>, text: Option<String>) {
        match text.filter(|t| !js_trim(t).is_empty()) {
            None => {
                self.toast(Toast::info(copy::COPY_SOME_TEXT));
                self.answer_read(req, ReadResult::clipboard_empty());
            }
            Some(text) => {
                let result = self.read(ReadRequest::new(text, ReadSource::Clipboard));
                self.answer_read(req, result);
            }
        }
    }

    fn replay(&mut self, req: Option<RequestId>, read_id: &str, origin: ReplayOrigin) {
        if !valid_request_id(read_id) {
            self.finish_replay(req, origin, ReadResult::failed(ReadErrorCode::Unknown, "Invalid id"));
            return;
        }
        let Some(rec) = self.services.history.get(read_id) else {
            self.finish_replay(req, origin, ReadResult::not_in_history());
            return;
        };
        if self.services.cache.has(read_id) {
            // Up to ~100 MB: loaded (and touched) on the cache's I/O worker, never here.
            let core = self.handle.clone();
            let id = read_id.to_string();
            self.services.cache.load_async(
                read_id.to_string(),
                Box::new(move |chunks| core.send(Command::ReplayLoaded { req, read_id: id, origin, chunks })),
            );
            return;
        }
        self.replay_miss(req, rec, origin);
    }

    fn on_replay_loaded(&mut self, req: Option<RequestId>, read_id: &str, origin: ReplayOrigin, chunks: Option<Vec<CachedChunk>>) {
        let Some(rec) = self.services.history.get(read_id) else {
            self.finish_replay(req, origin, ReadResult::not_in_history());
            return;
        };
        let chunks = match chunks {
            Some(c) if !c.is_empty() => c,
            _ => {
                tracing::debug!(target: "reader", "cached audio unusable; replay falls back");
                self.replay_miss(req, rec, origin);
                return;
            }
        };
        // reader-playback.md §2.6 step 2: a free cached replay that accumulates into the original record.
        let (text, chunks) = if rec.text_truncated { vox_tts::cached::preview_replay(&rec.text, chunks) } else { (rec.text.clone(), chunks) };
        let settings = self.settings();
        let opts = PlayCachedOptions {
            read_id: rec.id.clone(),
            text,
            title: vox_text::preview_title(&rec.text, TITLE_CHARS),
            voice_id: rec.voice_id.clone(),
            voice_name: rec.voice_name.clone(),
            source_app: rec.source_app.clone(),
            settings: settings.clone(),
            chunks,
        };
        let engine = &self.services.engine;
        let session_id = match catch_unwind(AssertUnwindSafe(|| engine.play_cached(opts))) {
            Ok(id) => id,
            Err(_) => {
                tracing::error!(target: "reader", "the TTS engine failed to start a cached replay");
                self.finish_replay(req, origin, ReadResult::failed_with_id(rec.id.clone(), ReadErrorCode::Unknown, error_message(ReadErrorCode::Unknown, None)));
                return;
            }
        };
        self.show_widget_for(&session_id);
        self.reader.sessions.track(session_id, rec.id.clone(), true, true, rec.listened_sec, rec.completed);
        self.finish_replay(req, origin, ReadResult::started(rec.id));
    }

    fn read_file(&mut self, path: &str, cwd: &std::path::Path) {
        let file = vox_core::cli::resolve_read_file(cwd, path);
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| file.to_string_lossy().into_owned());
        let max_chars = self.settings().text.max_chars as usize + READ_FILE_EXTRA_CHARS;
        let core = self.handle.clone();
        let spawned = std::thread::Builder::new().name("vox-read-file".into()).spawn(move || {
            let extracted = catch_unwind(AssertUnwindSafe(|| vox_text::read_text_file(&file, max_chars)));
            match extracted {
                Ok(Ok(text)) => {
                    let mut req = ReadRequest::new(text, ReadSource::Explorer);
                    req.source_app = Some("Explorer".into());
                    req.source_process = Some("explorer.exe".into());
                    req.source_title = Some(name);
                    core.send(Command::ReadPrepared(req));
                }
                Ok(Err(e)) => {
                    tracing::warn!(target: "reader", "read-file failed: {e}");
                    core.send(Command::Toast(Toast::error(copy::couldnt_open(&name))));
                }
                Err(_) => {
                    tracing::warn!(target: "reader", "read-file failed: the text extraction panicked");
                    core.send(Command::Toast(Toast::error(copy::couldnt_open(&name))));
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(target: "reader", "read-file failed: cannot start the file reader: {e}");
        }
    }

    fn toggle_playback(&mut self) {
        let status = self.services.player.snapshot().state.status;
        if status != PlayerStatus::Idle || self.services.engine.current_session_id().is_some() {
            self.command(PlayerCommand::Toggle);
            return;
        }
        if let Some(latest) = self.services.history.newest() {
            if latest.cached && self.services.cache.has(&latest.id) {
                self.replay(None, &latest.id, ReplayOrigin::TogglePlayback);
                return;
            }
        }
        self.toast_nothing_playing();
    }

    fn command(&mut self, cmd: PlayerCommand) {
        let Some(cmd) = sanitize_player_command(cmd) else {
            tracing::debug!(target: "reader", "dropped an invalid player command");
            return;
        };
        if cmd == PlayerCommand::Stop {
            // The engine's session counts too: `start_session` only queues the player's `Start`, so a Stop right
            // after a read or replay started can see no player session yet although `Widget(Show)` was published.
            let had_session =
                self.services.player.snapshot().session.is_some() || self.services.engine.current_session_id().is_some();
            // A read whose widget show still waits for the player's first report is not shown any more.
            self.reader.show_pending = None;
            self.services.player.command(PlayerCommand::Stop);
            self.services.engine.stop();
            if had_session {
                // D-ARCH-8: every stop of a live session hides the widget, whatever its source.
                self.publish(UiUpdate::Widget(WidgetIntent::Hide));
            }
            return;
        }
        self.services.player.command(cmd);
    }

    fn on_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Complete { read_id, chunks, characters_billed } => self.on_complete(&read_id, chunks, characters_billed),
            EngineEvent::Failed { read_id, error } => {
                let message = error_message(error.code, Some(&error.message));
                if self.reader.sessions.latest_for(&read_id).is_some_and(|s| s.record_saved) {
                    self.services.history.update(&read_id, &RecordPatch { error: Some(Some(message.clone())), ..RecordPatch::default() });
                }
                if error.aborted {
                    return;
                }
                let mut toast = Toast::error(message);
                if error_needs_settings(error.code, &error.message, error.missing_permission) {
                    toast = toast.with_settings_action("account");
                }
                self.toast(toast);
            }
            EngineEvent::Stopped { read_id, characters_billed, error } => {
                let Some(sess) = self.reader.sessions.latest_for(&read_id) else { return };
                if sess.from_cache {
                    return;
                }
                if error.is_some() {
                    sess.partial = true;
                }
                if !sess.record_saved {
                    return;
                }
                let mut patch = RecordPatch { characters_billed: Some(billed(characters_billed)), ..RecordPatch::default() };
                if error.is_some() {
                    patch.error = Some(Some(PARTIAL_READ_ERROR.to_string()));
                }
                self.services.history.update(&read_id, &patch);
            }
        }
    }

    fn on_cache_saved(&mut self, read_id: &str, result: Result<(), String>) {
        match result {
            Err(e) => tracing::warn!(target: "reader", "audio cache save failed: {e}"),
            Ok(()) if self.services.history.get(read_id).is_none() => {
                // Deleted while the save ran: no orphan (reader-playback.md §12 #17).
                self.services.cache.remove(read_id);
            }
            Ok(()) => {
                self.prune_cache(&[read_id.to_string()]);
                self.services.history.update(read_id, &RecordPatch { cached: Some(true), ..RecordPatch::default() });
            }
        }
    }

    fn on_player_report(&mut self, snapshot: PlayerSnapshot) {
        let prev_status = self.reader.last_snapshot.state.status;
        let status = snapshot.state.status;
        if let Some(sid) = &snapshot.state.session_id {
            self.services.engine.progress(sid, snapshot.state.position_sec);
        }
        let now = self.mono_ms();
        if let Some(write) = self.reader.sessions.on_state(&snapshot, now) {
            self.write_session(&write, now);
        }
        if super::credits::read_settled(prev_status, status) {
            self.timers.set(TimerId::CreditsAfterRead, Instant::now() + Duration::from_millis(CREDITS_AFTER_READ_MS));
        }
        if status == PlayerStatus::Playing && prev_status != PlayerStatus::Playing {
            // Samples and readings never overlap (ui-settings-widget.md §4.6).
            self.stop_preview();
        }
        if let Some(media) = &self.services.media {
            media.update(super::media::media_state(&snapshot, self.settings().triggers.media_keys));
        }
        if status == PlayerStatus::Playing {
            if !self.timers.is_set(TimerId::MediaTick) {
                self.timers.set(TimerId::MediaTick, Instant::now() + Duration::from_secs(1));
            }
        } else {
            self.timers.cancel(TimerId::MediaTick);
        }
        self.services.bridge_playing.store(status == PlayerStatus::Playing, std::sync::atomic::Ordering::Relaxed);
        self.reader.last_snapshot = snapshot.clone();
        let first_report = self.reader.show_pending.as_ref().is_some_and(|id| snapshot.session.as_ref().is_some_and(|s| s.id == *id));
        self.publish(UiUpdate::Player(snapshot));
        if first_report {
            // The read's deferred widget show (`show_widget_for`): the UI now has the new session's snapshot.
            self.reader.show_pending = None;
            if self.settings().widget.enabled {
                self.publish(UiUpdate::Widget(WidgetIntent::Show { reposition: false }));
            }
        }
    }

    fn prune_cache(&mut self, keep: &[String]) -> Vec<String> {
        let limit = self.settings().library.cache_limit_bytes();
        let mut protect: Vec<String> = keep.to_vec();
        protect.extend(self.services.engine.current_read_id());
        protect.extend(self.reader.last_snapshot.state.read_id.clone());
        let evicted = self.services.cache.prune(limit, &protect);
        if !evicted.is_empty() {
            self.services.history.set_cached(&evicted, false);
        }
        evicted
    }
}

/// `max(0, js_round(billed))` (D-NET-6: billing arrives unrounded).
fn billed(characters_billed: f64) -> f64 {
    if characters_billed.is_finite() {
        js_round(characters_billed).max(0.0)
    } else {
        0.0
    }
}

impl Core {
    /// The miss path of a replay (reader-playback.md §2.6 step 3).
    fn replay_miss(&mut self, req: Option<RequestId>, rec: ReadRecord, origin: ReplayOrigin) {
        if rec.cached {
            self.services.history.set_cached(std::slice::from_ref(&rec.id), false);
        }
        let result = if rec.text_truncated {
            ReadResult::not_cached(rec.id.clone())
        } else {
            self.read(ReadRequest {
                text: rec.text.clone(),
                source: ReadSource::Replay,
                source_app: rec.source_app.clone(),
                source_process: rec.source_process.clone(),
                source_title: rec.source_title.clone(),
                source_url: rec.source_url.clone(),
                voice: Some(VoiceOverride { id: rec.voice_id.clone(), name: rec.voice_name.clone() }),
            })
        };
        self.finish_replay(req, origin, result);
    }

    /// Answers a replay: `ReadStarted{req}` for a UI request; the play/pause hotkey's replay turns a failure into the
    /// `Nothing is playing…` toast.
    fn finish_replay(&mut self, req: Option<RequestId>, origin: ReplayOrigin, result: ReadResult) {
        match origin {
            ReplayOrigin::Request => self.answer_read(req, result),
            ReplayOrigin::TogglePlayback => {
                if !result.ok {
                    self.toast_nothing_playing();
                }
            }
        }
    }

    fn toast_nothing_playing(&mut self) {
        let settings = self.settings();
        let hotkey = settings.triggers.hotkeys_enabled.then_some(settings.triggers.hotkeys.read_selection.as_str());
        self.toast(Toast::info(copy::nothing_is_playing(hotkey)));
    }

    /// `complete(readId, chunks, billed)` (reader-playback.md §2.13; D-RP-14: duration + billing at once, `cached`
    /// after the save).
    fn on_complete(&mut self, read_id: &str, chunks: Vec<CachedChunk>, characters_billed: f64) {
        let Some(sess) = self.reader.sessions.latest_for(read_id) else { return };
        if sess.from_cache || !sess.record_saved {
            return;
        }
        let Some(rec) = self.services.history.get(read_id) else {
            return; // deleted / cleared during synthesis: nothing to cache
        };
        let duration: f64 = chunks.iter().map(|c| c.duration_sec).filter(|d| d.is_finite()).sum();
        self.services.history.update(
            read_id,
            &RecordPatch {
                audio_duration_sec: Some(round1(duration)),
                characters_billed: Some(billed(characters_billed)),
                ..RecordPatch::default()
            },
        );
        if self.settings().library.cache_audio && !chunks.is_empty() {
            let to_save = if rec.text_truncated { vox_tts::cached::without_text(&chunks) } else { chunks };
            let core = self.handle.clone();
            let id = read_id.to_string();
            self.services.cache.save_async(
                read_id.to_string(),
                to_save,
                Box::new(move |result| core.send(Command::CacheSaved { read_id: id, result: result.map_err(|e| e.to_string()) })),
            );
        }
    }

    /// `writeSession` against the current record (a vanished record writes nothing and remembers nothing).
    pub(super) fn write_session(&mut self, write: &PendingWrite, now_ms: f64) {
        let Some(rec) = self.services.history.get(&write.read_id) else { return };
        let hour = vox_core::listen_log::hour_key(&chrono::Local::now());
        self.services.history.update(&write.read_id, &write.patch(&rec, &hour));
        self.reader.sessions.mark_written(&write.session_id, write.next, now_ms);
    }

    /// A read or cached replay started `session_id`: `Widget(Show)` once the UI can build the card for it. The
    /// engine only queues the player's `Start`, so the show waits for the player's first report of that session
    /// ([`Reader::on_player_report`] publishes `Player` then `Show`); when the player already has it, the show is
    /// published at once. A newer read replaces the wait, a Stop drops it. Widget off → nothing.
    fn show_widget_for(&mut self, session_id: &str) {
        self.reader.show_pending = None;
        if !self.settings().widget.enabled {
            return;
        }
        if self.services.player.snapshot().session.as_ref().is_some_and(|s| s.id == session_id) {
            self.publish(UiUpdate::Widget(WidgetIntent::Show { reposition: false }));
        } else {
            self.reader.show_pending = Some(session_id.to_string());
        }
    }

    /// Widget speed pill: next preset now (the player's snapshot holds the target synchronously) + persist it.
    pub(super) fn cycle_rate(&mut self) {
        let next = next_rate(self.services.player.snapshot().state.rate);
        self.services.player.command(PlayerCommand::Rate { rate: next });
        self.update_settings(None, &serde_json::json!({ "playbackRate": next }));
    }

    /// The reading player failed and restarted (R-Audio): stop synthesis for that session, show the error card.
    pub(super) fn on_player_failed(&mut self, session_id: Option<String>, message: &str) {
        tracing::error!(target: "reader", "audio player failed: {message}");
        if session_id.is_some() && session_id == self.services.engine.current_session_id() {
            self.services.engine.stop();
        }
        self.reader.show_pending = None;
        if self.settings().widget.enabled {
            self.publish(UiUpdate::Widget(WidgetIntent::Show { reposition: false }));
        }
    }

    /// `persistPlayback()` with the player's exact snapshot (D-RP-6).
    pub(super) fn persist_playback(&mut self) {
        let current = self.services.player.snapshot();
        let now = self.mono_ms();
        let writes = self.reader.sessions.persist_all(now, Some(&current));
        for write in writes {
            self.write_session(&write, now);
        }
    }
}

// Keep `Arc` in scope for doc links of shared snapshots.
#[allow(unused)]
type SharedSnapshot = Arc<PlayerSnapshot>;
