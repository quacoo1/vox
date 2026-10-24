//! Core tests behind real services in a temporary demo profile (ARCHITECTURE.md §9, vox-app row): a mock-mode
//! `TtsEngine` (no network), `null_output` players (simulated clock), temp stores, no hook, no hotkeys, no bridge
//! listener. The core is driven on the test thread (`Core::new` + direct calls / `dispatch`), and the `UiSink` stays
//! detached so every published update can be inspected in order.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vox_audio::{Player, PlayerConfig};
use vox_bridge::BridgeServer;
use vox_core::app::{AppRoute, RuntimeFlags, Toast, ToastKind};
use vox_core::constants::PARTIAL_READ_ERROR;
use vox_core::history::{ReadRecord, ReadSource, RecordPatch};
use vox_core::ids::new_uuid;
use vox_core::player::{PlayerCommand, PlayerSnapshot, PlayerStatus};
use vox_core::read::{ReadErrorCode, ReadRequest};
use vox_core::tts::CachedChunk;
use vox_store::{AudioCache, HistoryStore, Profile, SettingsStore};
use vox_tts::{ClientConfig, ElevenLabsClient, EngineEvent, TtsEngine, TtsError};
use vox_win::selection::SelectionService;

use super::engine_sink::{engine_events, player_events, preview_events, PlayerSink};
use super::reader::{new_record, Reader};
use super::*;
use crate::command::{CatalogResult, ReplayOrigin};
use crate::update::{CreditsUpdate, CreditsView, PreviewUpdate};

thread_local! {
    /// The next `Core::handle` on this thread panics (D-ARCH-3 test).
    static PANIC_REQUEST: Cell<bool> = const { Cell::new(false) };
    /// The next `Core::handle` on this thread panics **after** its handler ran (and maybe answered).
    static PANIC_AFTER_REQUEST: Cell<bool> = const { Cell::new(false) };
    /// The next `Core::guarded` step with this name panics.
    static STEP_PANIC: Cell<Option<&'static str>> = const { Cell::new(None) };
    /// Extra steps for the next quit's flush phase (a hung service).
    static EXTRA_SHUTDOWN_TASKS: RefCell<Vec<lifecycle::ShutdownTask>> = const { RefCell::new(Vec::new()) };
}

/// Consumed by `Core::handle` in test builds.
pub(super) fn take_panic_request() -> bool {
    PANIC_REQUEST.with(|c| c.replace(false))
}

/// Consumed by `Core::handle` in test builds.
pub(super) fn take_panic_after_request() -> bool {
    PANIC_AFTER_REQUEST.with(|c| c.replace(false))
}

/// Consumed by `Core::guarded` in test builds: `true` when `step` is the step that must panic.
pub(super) fn take_step_panic(step: &str) -> bool {
    STEP_PANIC.with(|c| match c.get() {
        Some(s) if s == step => {
            c.set(None);
            true
        }
        _ => false,
    })
}

/// Consumed by `Core::shutdown` in test builds.
pub(super) fn take_extra_shutdown_tasks() -> Vec<lifecycle::ShutdownTask> {
    EXTRA_SHUTDOWN_TASKS.with(|t| std::mem::take(&mut *t.borrow_mut()))
}

/// A core over real services in a temporary demo profile.
struct Kit {
    core: Core,
    ui: UiSink,
    dir: PathBuf,
}

impl Kit {
    /// `key`: the API key (environment fallback, never persisted); `mock`: the engine's mock mode (the key gate is
    /// skipped in mock mode).
    fn new(key: Option<&str>, mock: bool) -> Kit {
        // An unroutable base URL: nothing in these tests may reach the network.
        Kit::with_base_url(key, mock, "http://127.0.0.1:9")
    }

    /// [`Kit::new`] against an ElevenLabs stand-in at `base_url`.
    fn with_base_url(key: Option<&str>, mock: bool, base_url: &str) -> Kit {
        let dir = std::env::temp_dir().join(format!("vox-core-test-{}", new_uuid()));
        std::fs::create_dir_all(&dir).expect("temp profile");
        let profile = Profile::at(&dir, true);
        let flags = RuntimeFlags { demo: true, mock_tts: mock, shots_dir: Some(dir.join("shots")), debug: false };
        let runtime = Runtime {
            flags,
            profile: profile.clone(),
            exe_path: dir.join("vox.exe"),
            extension_dir: dir.join("browser-extension"),
            is_packaged: false,
            runtime_label: crate::runtime::runtime_label(),
            build_hash: "test".into(),
            argv: Vec::new(),
            cwd: dir.clone(),
        };
        let settings = SettingsStore::open(&profile, key.map(str::to_string));
        let history = HistoryStore::open(&profile);
        let cache = AudioCache::open(&profile);
        {
            let cache = cache.clone();
            history.on_removed(Box::new(move |ids: &[String]| {
                for id in ids {
                    cache.remove(id);
                }
            }));
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = CoreHandle::new(tx);
        let key_source = {
            let settings = settings.clone();
            Arc::new(move || settings.api_key())
        };
        let client = ElevenLabsClient::new(key_source, ClientConfig { base_url: base_url.into(), ..ClientConfig::default() });
        let config = |name: &'static str| PlayerConfig { null_output: true, warm_output: false, name, ..PlayerConfig::default() };
        let player = Player::spawn(config("player"), player_events(handle.clone()));
        let preview_player = Player::spawn(config("preview"), preview_events(handle.clone()));
        let engine = TtsEngine::new(client.clone(), Arc::new(PlayerSink { player: player.clone() }), engine_events(handle.clone()), mock);
        let selection = SelectionService::new(Arc::new(|_| {}));
        let bridge_playing = Arc::new(AtomicBool::new(false));
        let bridge = Arc::new(BridgeServer::new(Arc::new(bridge::BridgeGlue { core: handle.clone(), playing: bridge_playing.clone() })));
        let services = Services {
            runtime,
            settings,
            history,
            cache,
            client,
            engine,
            player,
            preview_player,
            selection,
            hotkeys: None,
            media: None,
            bridge,
            bridge_playing,
        };
        let ui = UiSink::new(|_| {});
        let core = Core::new(services, rx, handle, ui.clone());
        Kit { core, ui, dir }
    }

    /// Everything published since the last call, in order.
    fn updates(&self) -> Vec<UiUpdate> {
        self.ui.take_queued()
    }

    fn record(&self, id: &str) -> Option<ReadRecord> {
        self.core.services.history.get(id)
    }

    fn add_record(&self, id: &str, text: &str) -> ReadRecord {
        let mut r = ReadRecord::new(id.to_string(), now_unix_ms(), ReadSource::Manual);
        r.text = text.to_string();
        r.char_count = text.encode_utf16().count() as u64;
        r.voice_id = "voice-1".into();
        r.voice_name = "George".into();
        self.core.services.history.add(r.clone());
        r
    }

    /// Dispatches incoming commands (and due timers) until `done` holds or `timeout` passes.
    fn pump_until(&mut self, timeout: Duration, mut done: impl FnMut(&mut Core) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if done(&mut self.core) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            if let Ok(cmd) = self.core.rx.recv_deadline(deadline.min(now + Duration::from_millis(50))) {
                self.core.dispatch(cmd);
            }
            self.core.fire_due_timers();
        }
    }

    /// Waits until the reading player's snapshot satisfies `f` (the player thread applies commands asynchronously).
    fn wait_player(&self, timeout: Duration, f: impl Fn(&PlayerSnapshot) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if f(&self.core.services.player.snapshot()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for Kit {
    fn drop(&mut self) {
        let s = &self.core.services;
        s.engine.stop();
        s.player.shutdown();
        s.preview_player.shutdown();
        s.cache.flush(Duration::from_secs(2));
        s.history.flush();
        s.settings.flush();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn toasts(updates: &[UiUpdate]) -> Vec<Toast> {
    updates.iter().filter_map(|u| if let UiUpdate::Toast(t) = u { Some(t.clone()) } else { None }).collect()
}

fn widget_intents(updates: &[UiUpdate]) -> Vec<WidgetIntent> {
    updates.iter().filter_map(|u| if let UiUpdate::Widget(w) = u { Some(w.clone()) } else { None }).collect()
}

fn read_answers(updates: &[UiUpdate]) -> Vec<(RequestId, vox_core::read::ReadResult)> {
    updates
        .iter()
        .filter_map(|u| if let UiUpdate::ReadStarted { req, result } = u { Some((*req, result.clone())) } else { None })
        .collect()
}

/// A cached chunk of mock audio for `text`.
fn mock_chunk(text: &str) -> CachedChunk {
    let r = vox_tts::mock::mock_synthesize(text, 1.0);
    let len = text.encode_utf16().count() as u32;
    CachedChunk { index: 0, text_start: 0, text_end: len, mime: r.mime, alignment: r.alignment, duration_sec: 0.6, audio: r.audio }
}

// ───────────────────────────── new_record (reader-playback.md §2.3) ─────────────────────────────

#[test]
fn new_record_follows_the_record_table() {
    let mut req = ReadRequest::new("unused", ReadSource::ContextChip);
    req.source_app = Some(String::new());
    req.source_process = Some("chrome.exe".into());
    req.source_title = Some(format!("  {}  ", "t".repeat(250)));
    req.source_url = Some(String::new());
    let full = format!("{} end", "word ".repeat(40)); // 204 units, 41 words
    let r = new_record("id-1", 5.0, &full, false, &req, "v1", "George", "eleven_flash_v2_5");
    assert_eq!(r.text, full[..160]);
    assert!(r.text_truncated);
    assert_eq!(r.char_count, 204);
    assert_eq!(r.word_count, 41, "counted over the full text");
    assert_eq!((r.voice_id.as_str(), r.voice_name.as_str(), r.model_id.as_str()), ("v1", "George", "eleven_flash_v2_5"));
    assert_eq!(r.source, ReadSource::ContextChip);
    assert_eq!(r.source_app, None, "empty strings are absent");
    assert_eq!(r.source_url, None);
    assert_eq!(r.source_process.as_deref(), Some("chrome.exe"));
    assert_eq!(r.source_title.as_deref(), Some("t".repeat(200).as_str()), "trimmed, then cut to 200 units");
    assert_eq!(r.created_at, 5.0);

    // Exactly 160 units: kept whole, not truncated.
    let exact = "a".repeat(160);
    let r = new_record("id-2", 1.0, &exact, false, &req, "v", "n", "m");
    assert_eq!((r.text.as_str(), r.text_truncated), (exact.as_str(), false));
    // The 160-unit preview never splits a surrogate pair.
    let text = format!("{}😀b", "a".repeat(159));
    let r = new_record("id-3", 1.0, &text, false, &req, "v", "n", "m");
    assert_eq!(r.text, "a".repeat(159));
    assert!(r.text_truncated);
    assert_eq!(r.char_count, 162);
    // storeText on: the full text.
    let r = new_record("id-4", 1.0, &text, true, &req, "v", "n", "m");
    assert_eq!((r.text.as_str(), r.text_truncated), (text.as_str(), false));
}

// ───────────────────────────── reads ─────────────────────────────

#[test]
fn no_key_reads_toast_and_create_no_record() {
    let mut kit = Kit::new(None, false);
    let result = kit.core.read(ReadRequest::new("Hello there.", ReadSource::Manual));
    assert_eq!(result.code, Some(ReadErrorCode::NoApiKey));
    assert!(kit.core.services.history.is_empty());
    let toasts = toasts(&kit.updates());
    assert_eq!(toasts.len(), 1);
    assert_eq!(toasts[0].message, vox_core::copy::ADD_KEY_TO_READ);
    let action = toasts[0].action.clone().expect("Add key");
    assert_eq!(action.label, vox_core::copy::ADD_KEY_ACTION);
    assert_eq!(action.route, Some(AppRoute::settings("account")));
}

#[test]
fn empty_text_is_refused_with_a_toast() {
    let mut kit = Kit::new(None, true);
    let result = kit.core.read(ReadRequest::new(" \n\t ", ReadSource::Manual));
    assert_eq!(result.code, Some(ReadErrorCode::EmptyText));
    assert_eq!(toasts(&kit.updates())[0].message, vox_core::copy::NOTHING_TO_READ);
    assert!(kit.core.services.history.is_empty());
}

#[test]
fn a_read_creates_its_record_tracks_the_session_and_shows_the_widget() {
    let mut kit = Kit::new(None, true);
    let result = kit.core.read(ReadRequest::new("Hello there. This is Vox.", ReadSource::Manual));
    assert!(result.ok);
    let id = result.read_id.clone().expect("read id");
    let rec = kit.record(&id).expect("record saved before synthesis");
    assert_eq!(rec.text, "Hello there. This is Vox.");
    assert_eq!(rec.source, ReadSource::Manual);
    let sess = kit.core.reader.sessions.latest_for(&id).expect("tracked at once").clone();
    assert!(sess.record_saved && !sess.from_cache);
    // The show waits for the player's first report of the new session (immediate if the player already has it).
    assert!(kit.pump_until(Duration::from_secs(3), |c| c.reader.show_pending.is_none()));
    assert_eq!(widget_intents(&kit.updates()), [WidgetIntent::Show { reposition: false }]);

    // Widget off: no show.
    kit.core.update_settings(None, &serde_json::json!({ "widget": { "enabled": false } }));
    kit.updates();
    assert!(kit.core.read(ReadRequest::new("Second read.", ReadSource::Manual)).ok);
    assert!(kit.core.reader.show_pending.is_none());
    kit.pump_until(Duration::from_millis(300), |_| false);
    assert!(widget_intents(&kit.updates()).is_empty());
}

/// The card is never built from the previous session's snapshot: a read's `Widget(Show)` follows the player's report
/// of the new session, and a Stop before that report drops the show.
#[test]
fn a_read_shows_the_widget_after_the_player_reports_its_session() {
    let mut kit = Kit::new(None, true);
    assert!(kit.core.read(ReadRequest::new("First read.", ReadSource::Manual)).ok);
    assert!(kit.pump_until(Duration::from_secs(3), |c| c.reader.show_pending.is_none()));
    kit.updates();
    // A second read while the first plays. Usually the player thread has not processed the new `Start` when the read
    // returns: the show waits for that session's report (published right after its `Player` update). If the player
    // was quicker, the show goes out at once (the UI then reads the player's snapshot, which has the new session).
    let second = kit.core.read(ReadRequest::new("Second read.", ReadSource::Manual)).read_id.unwrap();
    let immediate = widget_intents(&kit.updates());
    if immediate.is_empty() {
        assert!(kit.core.reader.show_pending.is_some());
        assert!(kit.pump_until(Duration::from_secs(3), |c| c.reader.show_pending.is_none()));
        let u = kit.updates();
        let show_at = u.iter().position(|x| matches!(x, UiUpdate::Widget(WidgetIntent::Show { .. }))).expect("shown");
        assert!(
            u[..show_at].iter().any(|x| matches!(x, UiUpdate::Player(s) if s.state.read_id.as_deref() == Some(second.as_str()))),
            "a Player update of the new session precedes the show"
        );
    } else {
        assert_eq!(immediate, [WidgetIntent::Show { reposition: false }]);
    }

    // A Stop before the new session's report drops a pending show: no show after the hide.
    assert!(kit.core.read(ReadRequest::new("Third read.", ReadSource::Manual)).ok);
    kit.core.command(PlayerCommand::Stop);
    assert!(kit.core.reader.show_pending.is_none());
    kit.pump_until(Duration::from_millis(300), |_| false);
    let intents = widget_intents(&kit.updates());
    assert_eq!(intents.last(), Some(&WidgetIntent::Hide), "{intents:?}");
}

#[test]
fn history_off_still_tracks_but_writes_nothing() {
    let mut kit = Kit::new(None, true);
    kit.core.update_settings(None, &serde_json::json!({ "library": { "saveHistory": false } }));
    let result = kit.core.read(ReadRequest::new("Not saved.", ReadSource::Manual));
    assert!(result.ok);
    let id = result.read_id.unwrap();
    assert!(kit.record(&id).is_none());
    assert!(!kit.core.reader.sessions.latest_for(&id).unwrap().record_saved);
}

#[test]
fn a_long_text_is_cut_with_a_toast() {
    let mut kit = Kit::new(None, true);
    kit.core.update_settings(None, &serde_json::json!({ "text": { "maxChars": 1000 } }));
    let max = kit.core.settings().text.max_chars;
    kit.updates();
    let text = "Lorem ipsum dolor sit amet. ".repeat(100);
    let result = kit.core.read(ReadRequest::new(text, ReadSource::Manual));
    let rec = kit.record(&result.read_id.unwrap()).unwrap();
    assert!(rec.char_count <= u64::from(max));
    let toasts = toasts(&kit.updates());
    assert!(toasts.iter().any(|t| t.message == vox_core::copy::reading_first(max)), "{toasts:?}");
}

// ───────────────────────────── stop / toggle ─────────────────────────────

#[test]
fn stop_hides_the_widget_only_when_a_session_existed() {
    let mut kit = Kit::new(None, true);
    kit.core.command(PlayerCommand::Stop);
    assert!(widget_intents(&kit.updates()).is_empty(), "stop while idle publishes nothing");

    assert!(kit.core.read(ReadRequest::new("Hello there.", ReadSource::Manual)).ok);
    assert!(kit.wait_player(Duration::from_secs(3), |s| s.session.is_some()));
    kit.updates();
    kit.core.command(PlayerCommand::Stop);
    assert_eq!(widget_intents(&kit.updates()), [WidgetIntent::Hide]);
    assert!(kit.core.services.engine.current_session_id().is_none(), "synthesis stopped too");
}

/// D-ARCH-8: `Player::start_session` only queues the player's `Start`, so a Stop handled right after the read (bridge
/// `/read` then `/command stop`, a Stop hotkey racing a read) may see no player session yet; the engine's counts.
#[test]
fn a_stop_right_after_a_read_started_still_hides_the_widget() {
    let mut kit = Kit::new(None, true);
    assert!(kit.core.read(ReadRequest::new("Hello there.", ReadSource::Manual)).ok);
    kit.core.command(PlayerCommand::Stop); // no wait for the player thread
    // The show is usually still waiting for the player's report (then dropped by the Stop); either way the widget
    // ends hidden and no show follows the hide.
    let intents = widget_intents(&kit.updates());
    assert!(
        intents == [WidgetIntent::Hide] || intents == [WidgetIntent::Show { reposition: false }, WidgetIntent::Hide],
        "{intents:?}"
    );
    kit.pump_until(Duration::from_millis(300), |_| false);
    assert!(widget_intents(&kit.updates()).is_empty(), "the stopped session's late report shows nothing");
    // A cached replay too (engine.play_cached).
    let r = kit.add_record("rec-stop", "Hi.");
    kit.core.services.cache.save(&r.id, &[mock_chunk("Hi.")]).unwrap();
    kit.core.replay(None, &r.id, ReplayOrigin::Request);
    assert!(kit.pump_until(Duration::from_secs(5), |c| c.reader.sessions.latest_for("rec-stop").is_some()));
    kit.updates();
    kit.core.command(PlayerCommand::Stop);
    assert_eq!(widget_intents(&kit.updates()), [WidgetIntent::Hide]);
}

#[test]
fn invalid_player_commands_are_dropped() {
    let mut kit = Kit::new(None, true);
    kit.core.command(PlayerCommand::Seek { position_sec: f64::NAN });
    kit.core.command(PlayerCommand::Rate { rate: 1.5 });
    assert_eq!(kit.core.services.player.snapshot().state.rate, 1.5, "rate applied synchronously to the snapshot");
}

#[test]
fn toggle_playback_toasts_when_nothing_can_play() {
    let mut kit = Kit::new(None, true);
    kit.core.toggle_playback();
    let t = toasts(&kit.updates());
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].message, vox_core::copy::nothing_is_playing(Some("Ctrl+Alt+R")));
    // A newest record that claims to be cached but has no cache entry: still the toast (no replay).
    let r = kit.add_record("rec-a", "Hi.");
    kit.core.services.history.update(&r.id, &RecordPatch { cached: Some(true), ..RecordPatch::default() });
    kit.core.toggle_playback();
    assert_eq!(toasts(&kit.updates()).len(), 1);
    assert!(kit.core.reader.sessions.is_empty());
}

#[test]
fn toggle_playback_replays_the_newest_cached_read_for_free() {
    let mut kit = Kit::new(None, false); // no key, not mock: a cached replay needs neither
    let r = kit.add_record("rec-b", "Hi.");
    kit.core.services.cache.save(&r.id, &[mock_chunk("Hi.")]).expect("cache save");
    kit.core.services.history.update(&r.id, &RecordPatch { cached: Some(true), ..RecordPatch::default() });
    kit.core.toggle_playback();
    let replayed = kit.pump_until(Duration::from_secs(5), |c| c.reader.sessions.latest_for("rec-b").is_some());
    assert!(replayed, "the cached newest read was replayed");
    assert!(kit.core.reader.sessions.latest_for("rec-b").unwrap().from_cache);
    assert!(toasts(&kit.updates()).is_empty());
    // While a session exists the hotkey toggles.
    assert!(kit.wait_player(Duration::from_secs(3), |s| s.session.is_some()));
    kit.core.toggle_playback();
    assert!(toasts(&kit.updates()).is_empty());
}

/// The play/pause hotkey's replay branch: the newest record is cached and indexed, but its audio cannot be loaded
/// (manifest gone) → the full text is re-synthesized as a new `replay` record, no toast.
#[test]
fn toggle_playback_re_synthesizes_when_the_cached_audio_cannot_be_loaded() {
    let mut kit = Kit::new(None, true);
    let r = kit.add_record("rec-load", "Read me once more.");
    kit.core.services.cache.save(&r.id, &[mock_chunk("Read me once more.")]).unwrap();
    kit.core.services.history.update(&r.id, &RecordPatch { cached: Some(true), ..RecordPatch::default() });
    let manifest = kit.core.services.runtime.profile.audio_cache_dir().join(&r.id).join("manifest.json");
    std::fs::remove_file(&manifest).expect("manifest written by the save");
    assert!(kit.core.services.cache.has(&r.id), "still indexed: the toggle takes the replay branch");
    kit.core.toggle_playback();
    let resynthesized = kit.pump_until(Duration::from_secs(5), |c| c.services.history.newest().is_some_and(|n| n.id != "rec-load"));
    assert!(resynthesized, "a new read was started");
    let new = kit.core.services.history.newest().unwrap();
    assert_eq!(new.source, ReadSource::Replay);
    assert_eq!(new.text, "Read me once more.");
    assert_eq!((new.voice_id.as_str(), new.voice_name.as_str()), ("voice-1", "George"), "the record's voice");
    assert!(kit.core.reader.sessions.latest_for(&new.id).is_some_and(|s| !s.from_cache));
    assert!(!kit.record("rec-load").unwrap().cached, "the stale flag is cleared");
    assert!(toasts(&kit.updates()).is_empty(), "no `Nothing is playing` toast");
}

// ───────────────────────────── replay (reader-playback.md §2.6) ─────────────────────────────

#[test]
fn replay_answers_invalid_and_unknown_ids_at_once() {
    let mut kit = Kit::new(None, true);
    kit.core.replay(Some(RequestId(1)), "", ReplayOrigin::Request);
    kit.core.replay(Some(RequestId(2)), &"x".repeat(129), ReplayOrigin::Request);
    kit.core.replay(Some(RequestId(3)), "gone", ReplayOrigin::Request);
    let answers = read_answers(&kit.updates());
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0].1.error.as_deref(), Some("Invalid id"));
    assert_eq!(answers[1].1.error.as_deref(), Some("Invalid id"));
    assert_eq!(answers[2].1, vox_core::read::ReadResult::not_in_history());
    assert_eq!(answers[2].1.read_id, None);
}

#[test]
fn a_cache_hit_replays_without_a_key_and_keeps_the_base_values() {
    let mut kit = Kit::new(None, false);
    let r = kit.add_record("rec-c", "Hi.");
    kit.core.services.history.update(
        &r.id,
        &RecordPatch { listened_sec: Some(12.0), completed: Some(true), cached: Some(true), ..RecordPatch::default() },
    );
    kit.core.services.cache.save(&r.id, &[mock_chunk("Hi.")]).unwrap();
    kit.core.replay(Some(RequestId(7)), "rec-c", ReplayOrigin::Request);
    assert!(kit.pump_until(Duration::from_secs(5), |c| c.reader.sessions.latest_for("rec-c").is_some()));
    let sess = kit.core.reader.sessions.latest_for("rec-c").unwrap().clone();
    assert!(sess.from_cache && sess.record_saved);
    assert_eq!((sess.base_listened_sec, sess.base_completed), (12.0, true));
    let answers = read_answers(&kit.updates());
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].0, RequestId(7));
    assert!(answers[0].1.ok);
    assert_eq!(answers[0].1.read_id.as_deref(), Some("rec-c"), "the same id");
}

#[test]
fn a_preview_only_replay_rebuilds_the_session_text() {
    let mut kit = Kit::new(None, true);
    let r = kit.add_record("rec-d", "Hi.");
    kit.core.services.history.update(&r.id, &RecordPatch { text_truncated: Some(true), ..RecordPatch::default() });
    let mut late = mock_chunk("There.");
    late.index = 1;
    late.text_start = 200;
    late.text_end = 206;
    kit.core.services.cache.save(&r.id, &[mock_chunk("Hi."), late]).unwrap();
    kit.core.replay(None, "rec-d", ReplayOrigin::Request);
    assert!(kit.pump_until(Duration::from_secs(5), |c| c.reader.sessions.latest_for("rec-d").is_some()));
    assert!(kit.wait_player(Duration::from_secs(3), |s| s.session.as_ref().is_some_and(|x| x.read_id == "rec-d")));
    let text = kit.core.services.player.snapshot().session.unwrap().text.to_string();
    assert!(text.starts_with("Hi."), "{text:?}");
    assert!(text.ends_with('…'), "a placeholder for the chunk past the preview: {text:?}");
}

#[test]
fn a_miss_clears_the_stale_flag_and_answers_not_cached_for_preview_only_records() {
    let mut kit = Kit::new(None, true);
    let r = kit.add_record("rec-e", "Hi.");
    kit.core.services.history.update(
        &r.id,
        &RecordPatch { cached: Some(true), text_truncated: Some(true), ..RecordPatch::default() },
    );
    kit.core.replay(Some(RequestId(9)), "rec-e", ReplayOrigin::Request);
    let answers = read_answers(&kit.updates());
    assert_eq!(answers[0].1, vox_core::read::ReadResult::not_cached("rec-e"));
    assert!(!kit.record("rec-e").unwrap().cached, "stale cached flag cleared");
}

#[test]
fn a_miss_with_the_full_text_re_synthesizes_as_a_new_replay_record() {
    let mut kit = Kit::new(None, true);
    let mut r = kit.add_record("rec-f", "Read me again.");
    r.source_app = Some("Notepad".into());
    kit.core.services.history.add(r);
    kit.core.replay(Some(RequestId(4)), "rec-f", ReplayOrigin::Request);
    let answers = read_answers(&kit.updates());
    let new_id = answers[0].1.read_id.clone().expect("a new read");
    assert_ne!(new_id, "rec-f");
    let rec = kit.record(&new_id).unwrap();
    assert_eq!(rec.source, ReadSource::Replay);
    assert_eq!(rec.source_app.as_deref(), Some("Notepad"));
    assert_eq!((rec.voice_id.as_str(), rec.voice_name.as_str()), ("voice-1", "George"), "the record's voice");
}

/// reader-playback.md §12 #1: a new read supersedes the old one, whose billing is still written.
#[test]
fn a_superseded_read_keeps_its_billing() {
    let mut kit = Kit::new(None, true);
    let first = kit.core.read(ReadRequest::new("The first read is long enough to bill a chunk. ".repeat(20), ReadSource::Manual));
    let first_id = first.read_id.unwrap();
    std::thread::sleep(Duration::from_millis(800)); // the mock delivers chunk 0 after 350 ms
    let second = kit.core.read(ReadRequest::new("The second read.", ReadSource::Manual));
    assert!(second.ok);
    let billed = kit.pump_until(Duration::from_secs(5), |c| c.services.history.get(&first_id).is_some_and(|r| r.characters_billed > 0));
    assert!(billed, "{:?}", kit.record(&first_id));
    assert!(!kit.record(&first_id).unwrap().completed);
}

#[test]
fn startup_reconciliation_fixes_stale_flags_and_redacts_preview_text() {
    let kit = Kit::new(None, true);
    let (history, cache) = (&kit.core.services.history, &kit.core.services.cache);
    let stale = kit.add_record("rec-stale", "Hi.");
    history.update(&stale.id, &RecordPatch { cached: Some(true), ..RecordPatch::default() });
    let preview = kit.add_record("rec-preview", "Hi.");
    history.update(&preview.id, &RecordPatch { cached: Some(true), text_truncated: Some(true), ..RecordPatch::default() });
    cache.save(&preview.id, &[mock_chunk("Hi.")]).unwrap();
    assert!(cache.has_text(&preview.id));
    retention::reconcile_cache(history, cache);
    assert!(!history.get("rec-stale").unwrap().cached);
    assert!(history.get("rec-preview").unwrap().cached);
    assert!(!cache.has_text("rec-preview"), "the index forgets the text at once");
    // Retention: 0 keeps everything; an old non-favourite read goes.
    let mut old = ReadRecord::new("rec-old".into(), now_unix_ms() - 40.0 * 86_400_000.0, ReadSource::Manual);
    old.text = "old".into();
    history.add(old);
    assert_eq!(retention::apply_retention(history, 0, now_unix_ms()), 0);
    assert_eq!(retention::apply_retention(history, 30, now_unix_ms()), 1);
    assert!(history.get("rec-old").is_none());
}

// ───────────────────────────── engine events ─────────────────────────────

#[test]
fn complete_writes_duration_and_billing_then_caches_after_the_save() {
    let mut kit = Kit::new(None, true);
    let r = kit.add_record("rec-g", "Hi.");
    kit.core.reader.sessions.track("s-g".into(), r.id.clone(), true, false, 0.0, false);
    kit.core.on_engine_event(EngineEvent::Complete { read_id: r.id.clone(), chunks: vec![mock_chunk("Hi.")], characters_billed: 3.4 });
    let rec = kit.record(&r.id).unwrap();
    assert_eq!((rec.audio_duration_sec, rec.characters_billed), (0.6, 3));
    assert!(!rec.cached, "cached only after the save");
    assert!(kit.pump_until(Duration::from_secs(5), |c| c.services.history.get("rec-g").is_some_and(|r| r.cached)));
    assert!(kit.core.services.cache.has("rec-g"));
}

#[test]
fn nothing_is_cached_for_a_record_deleted_during_synthesis_or_the_save() {
    let mut kit = Kit::new(None, true);
    // Gone at Complete: nothing saved.
    kit.core.reader.sessions.track("s-h".into(), "rec-h".into(), true, false, 0.0, false);
    kit.core.on_engine_event(EngineEvent::Complete { read_id: "rec-h".into(), chunks: vec![mock_chunk("Hi.")], characters_billed: 3.0 });
    assert!(!kit.pump_until(Duration::from_millis(400), |c| c.services.cache.has("rec-h")));
    // Deleted while the save ran: the entry is removed again.
    kit.core.services.cache.save("rec-i", &[mock_chunk("Hi.")]).unwrap();
    kit.core.on_cache_saved("rec-i", Ok(()));
    assert!(!kit.core.services.cache.has("rec-i"));
    // A failed save leaves `cached` false.
    let r = kit.add_record("rec-j", "Hi.");
    kit.core.on_cache_saved(&r.id, Err("disk full".into()));
    assert!(!kit.record(&r.id).unwrap().cached);
}

/// `pruneCache(keep)` protects `keep`, the engine's read and the player's last read even when they are the least
/// recently used; the evicted read's `cached` flag is cleared.
#[test]
fn prune_protects_keep_the_engine_read_and_the_last_read() {
    let mut kit = Kit::new(None, true);
    kit.core.update_settings(None, &serde_json::json!({ "library": { "cacheLimitMB": 50 } }));
    let engine_read = kit.core.read(ReadRequest::new("The engine's read.", ReadSource::Manual)).read_id.unwrap();
    assert_eq!(kit.core.services.engine.current_read_id().as_deref(), Some(engine_read.as_str()));
    kit.add_record("rec-keep", "Keep.");
    kit.add_record("rec-last", "Last.");
    kit.add_record("rec-free", "Free.");
    kit.core.reader.last_snapshot.state.read_id = Some("rec-last".into());
    // 4 × 13 MB > the 50 MB limit; the three protected entries are the oldest.
    let big = |text: &str| {
        let mut c = mock_chunk(text);
        c.audio = Arc::from(vec![1u8; 13_000_000]);
        c
    };
    for id in ["rec-keep", engine_read.as_str(), "rec-last", "rec-free"] {
        kit.core.services.cache.save(id, &[big("x")]).unwrap();
        kit.core.services.history.update(id, &RecordPatch { cached: Some(true), ..RecordPatch::default() });
    }
    let evicted = kit.core.prune_cache(&["rec-keep".to_string()]);
    assert_eq!(evicted, ["rec-free"]);
    assert!(!kit.record("rec-free").unwrap().cached);
    for id in ["rec-keep", engine_read.as_str(), "rec-last"] {
        assert!(kit.core.services.cache.has(id), "{id} protected");
        assert!(kit.record(id).unwrap().cached);
    }
    assert!(kit.core.prune_cache(&[]).is_empty(), "39 MB left: under the limit");
}

#[test]
fn a_later_chunk_failure_makes_the_read_partial_and_never_completed() {
    let mut kit = Kit::new(None, true);
    let r = kit.add_record("rec-k", "Hi.");
    kit.core.reader.sessions.track("s-k".into(), r.id.clone(), true, false, 0.0, false);
    kit.core.on_engine_event(EngineEvent::Stopped {
        read_id: r.id.clone(),
        characters_billed: 220.4,
        error: Some(TtsError::new(ReadErrorCode::Quota, "out of credits")),
    });
    let rec = kit.record(&r.id).unwrap();
    assert_eq!(rec.error.as_deref(), Some(PARTIAL_READ_ERROR));
    assert_eq!(rec.characters_billed, 220);
    // The player then reaches the end of the delivered audio.
    let mut ended = PlayerSnapshot::default();
    ended.state.status = PlayerStatus::Ended;
    ended.state.session_id = Some("s-k".into());
    ended.state.read_id = Some(r.id.clone());
    ended.state.duration_sec = 14.0;
    ended.state.listened_sec = 14.0;
    kit.core.on_player_report(ended);
    let rec = kit.record(&r.id).unwrap();
    assert!(!rec.completed, "a partial read never completes");
    assert_eq!(rec.listened_sec, 14.0);
    // A later cumulative Stopped only raises the billing.
    kit.core.on_engine_event(EngineEvent::Stopped { read_id: r.id.clone(), characters_billed: 900.0, error: None });
    assert_eq!(kit.record(&r.id).unwrap().characters_billed, 900);
}

#[test]
fn failed_reads_toast_with_open_settings_only_for_settings_errors() {
    let mut kit = Kit::new(None, true);
    let r = kit.add_record("rec-l", "Hi.");
    kit.core.reader.sessions.track("s-l".into(), r.id.clone(), true, false, 0.0, false);
    kit.core.on_engine_event(EngineEvent::Failed { read_id: r.id.clone(), error: TtsError::new(ReadErrorCode::Quota, "quota") });
    let t = toasts(&kit.updates());
    assert_eq!(t[0].kind, ToastKind::Error);
    assert_eq!(t[0].action.as_ref().and_then(|a| a.route.clone()), Some(AppRoute::settings("account")));
    assert!(kit.record(&r.id).unwrap().error.is_some());
    kit.core.on_engine_event(EngineEvent::Failed { read_id: r.id.clone(), error: TtsError::network() });
    let t = toasts(&kit.updates());
    assert!(t[0].action.is_none());
}

// ───────────────────────────── settings, keys, credits ─────────────────────────────

#[test]
fn update_settings_publishes_settings_then_saved() {
    let mut kit = Kit::new(None, true);
    kit.core.update_settings(Some(RequestId(5)), &serde_json::json!([1, 2]));
    let u = kit.updates();
    assert_eq!(u.len(), 1);
    assert!(matches!(&u[0], UiUpdate::SettingsSaved { req: RequestId(5), result: Err(e) } if e == "Invalid settings patch"));
    // Unchanged values still publish both, in order.
    kit.core.update_settings(Some(RequestId(6)), &serde_json::json!({}));
    let u = kit.updates();
    assert!(matches!(u[0], UiUpdate::Settings(_)));
    assert!(matches!(u[1], UiUpdate::SettingsSaved { req: RequestId(6), result: Ok(()) }));
    // A rate change goes to the player at once.
    kit.core.update_settings(None, &serde_json::json!({ "playbackRate": 1.25 }));
    assert_eq!(kit.core.services.player.snapshot().state.rate, 1.25);
}

#[test]
fn clearing_the_key_publishes_settings_and_app_info_before_done() {
    let mut kit = Kit::new(Some("sk_test_key_123"), true);
    assert!(kit.core.services.settings.has_api_key());
    let generation = kit.core.key_generation;
    kit.core.clear_api_key(RequestId(11));
    let u = kit.updates();
    let settings_at = u.iter().position(|x| matches!(x, UiUpdate::Settings(s) if !s.has_api_key)).expect("settings");
    let info_at = u.iter().position(|x| matches!(x, UiUpdate::AppInfo(_))).expect("app info");
    let done_at = u.iter().position(|x| matches!(x, UiUpdate::Done { req: RequestId(11), result: Ok(()) })).expect("done");
    assert!(settings_at < info_at && info_at < done_at, "{u:?}");
    assert_eq!(kit.core.key_generation, generation + 1, "older catalog answers are dropped from now on");
}

#[test]
fn a_too_long_key_is_refused_at_once() {
    let mut kit = Kit::new(None, true);
    kit.core.set_api_key(RequestId(3), "k".repeat(257));
    let u = kit.updates();
    assert!(matches!(&u[..], [UiUpdate::ApiKey { req: RequestId(3), result }] if !result.ok
        && result.error.as_deref() == Some(vox_core::read::eleven_messages::KEY_TOO_LONG)));
}

#[test]
fn demo_credits_without_a_key_settle_on_the_demo_account() {
    let mut kit = Kit::new(None, true);
    kit.core.refresh_credits(true);
    let credits: Vec<CreditsUpdate> = kit.updates().into_iter().filter_map(|u| if let UiUpdate::Credits(c) = u { Some(c) } else { None }).collect();
    assert_eq!(credits.len(), 2);
    assert!(credits[0].refreshing);
    assert!(!credits[1].refreshing);
    assert!(matches!(&credits[1].view, CreditsView::Ready(a) if a.tier == "creator"));
}

/// `nextResetUnix` comes from the API: a finite but huge value must neither panic the reset timer nor skip the rest of
/// the resume handling (`Refresh(Day)`).
#[test]
fn an_implausible_reset_time_neither_panics_nor_skips_the_day_refresh() {
    let mut kit = Kit::new(None, true);
    let account = |reset: f64| vox_core::voices::AccountInfo {
        tier: "creator".into(),
        character_count: 1.0,
        character_limit: 10.0,
        next_reset_unix: Some(reset),
        status: None,
    };
    for reset in [1e300, 1.8e306] {
        kit.core.credits.seed(account(reset));
        assert!(kit.core.credits.rearm());
        kit.core.timers.cancel(TimerId::CreditsReset);
        kit.core.dispatch(Command::System(SystemEvent::Resumed));
        let u = kit.updates();
        assert!(u.iter().any(|x| matches!(x, UiUpdate::Refresh(RefreshReason::Day))), "{u:?}");
        assert!(kit.core.timers.is_set(TimerId::CreditsReset), "re-checked within the hour");
    }
}

#[test]
fn catalog_results_of_an_older_key_are_dropped() {
    let mut kit = Kit::new(None, true);
    kit.core.key_generation = 4;
    kit.core.on_catalog(CatalogResult::Voices { generation: 3, refresh: false, result: Ok(Arc::new(Vec::new())) });
    assert!(kit.updates().is_empty());
    kit.core.on_catalog(CatalogResult::Voices { generation: 4, refresh: true, result: Ok(Arc::new(Vec::new())) });
    assert!(matches!(&kit.updates()[..], [UiUpdate::Voices { refresh: true, result: Ok(_) }]));
    // Demo without a key: the demo lists, answered at once.
    kit.core.fetch_models(false);
    assert!(matches!(&kit.updates()[..], [UiUpdate::Models { result: Ok(m) }] if m.len() == 4));
}

#[test]
fn a_preview_download_that_failed_reports_the_connection_message() {
    let mut kit = Kit::new(None, true);
    kit.core.preview = Some(catalog::PreviewState { voice_id: "v1".into(), session_id: None, playing: false, cancel: vox_tts::CancelToken::new() });
    kit.core.on_catalog(CatalogResult::Preview { voice_id: "v1".into(), result: Err("offline".into()) });
    let u = kit.updates();
    assert!(matches!(&u[..], [UiUpdate::Preview(PreviewUpdate::Failed { voice_id, message })]
        if voice_id == "v1" && message == crate::update::PREVIEW_FAILED_CONNECTION));
    assert!(kit.core.preview.is_none());
    // A stop with nothing playing publishes nothing.
    kit.core.stop_preview();
    assert!(kit.updates().is_empty());
}

// ───────────────────────────── shell sources ─────────────────────────────

#[test]
fn a_widget_drag_remembers_the_position() {
    let mut kit = Kit::new(None, true);
    kit.core.on_window_event(WindowEvent::WidgetDragged { x: 10, y: -20 });
    let s = kit.core.settings();
    assert_eq!(s.widget.position, vox_core::settings::WidgetPosition::Remember);
    assert_eq!((s.widget.custom_x, s.widget.custom_y), (Some(10), Some(-20)));
}

#[test]
fn tray_checkboxes_write_settings() {
    let mut kit = Kit::new(None, true);
    kit.core.on_tray(TrayAction::SetWidgetEnabled(false));
    assert!(!kit.core.settings().widget.enabled);
    assert!(widget_intents(&kit.updates()).contains(&WidgetIntent::Hide), "turning the widget off hides it");
    kit.core.on_tray(TrayAction::SetHotkeysEnabled(false));
    assert!(!kit.core.settings().triggers.hotkeys_enabled);
}

#[test]
fn hook_status_reaches_app_info() {
    let mut kit = Kit::new(None, true);
    kit.core.on_selection(SelectionEvent::HookStatus(false));
    let u = kit.updates();
    assert!(matches!(&u[..], [UiUpdate::AppInfo(info)] if !info.native_hook_available));
    kit.core.on_selection(SelectionEvent::NoSelection(ReadSource::Hotkey));
    assert_eq!(toasts(&kit.updates())[0].message, vox_core::copy::SELECT_SOME_TEXT);
}

#[test]
fn toasts_before_the_ui_is_ready_are_published_and_routed_later() {
    let mut kit = Kit::new(None, true);
    kit.core.toast(Toast::error("early"));
    let u = kit.updates();
    assert_eq!(toasts(&u).len(), 1, "published at once");
    assert!(widget_intents(&u).is_empty(), "the extra route waits for the first MainState");
    // The main window opens hidden: the widget shows the deferred toast.
    kit.core.on_window_event(WindowEvent::MainState { visible: false, focused: false });
    assert_eq!(widget_intents(&kit.updates()), [WidgetIntent::Show { reposition: false }]);
}

#[test]
fn app_info_reflects_the_runtime() {
    let kit = Kit::new(None, true);
    let info = queries::build_app_info(&kit.core.services.runtime, &kit.core.services.settings, &kit.core.services.bridge, None, Vec::new());
    assert_eq!(info.name, "Vox");
    assert_eq!(info.build, "test");
    assert!(info.demo_mode && !info.is_packaged);
    assert!(info.native_hook_available, "no attempt yet → available");
    assert!(!info.server_running);
    assert_eq!(info.server_port, kit.core.settings().server.port);
    assert_eq!(info.api_key_notice, None);
}

// ───────────────────────────── D-ARCH-3 ─────────────────────────────

#[test]
fn a_panicking_request_is_still_answered() {
    let mut kit = Kit::new(None, true);
    PANIC_REQUEST.with(|c| c.set(true));
    kit.core.dispatch(Command::QueryHome { req: RequestId(21) });
    let u = kit.updates();
    assert!(matches!(&u[..], [UiUpdate::Home { req: RequestId(21), result: Err(e) }] if e == vox_core::copy::SOMETHING_WENT_WRONG));
    // The next request works normally.
    kit.core.dispatch(Command::QueryHome { req: RequestId(22) });
    assert!(matches!(&kit.updates()[..], [UiUpdate::Home { req: RequestId(22), result: Ok(_) }]));
}

/// The owed answer is one-shot: a handler that already answered and then panicked gets no failure answer on top.
#[test]
fn a_request_answered_before_its_handler_panicked_gets_no_second_answer() {
    let mut kit = Kit::new(None, true);
    PANIC_AFTER_REQUEST.with(|c| c.set(true));
    kit.core.dispatch(Command::QueryCacheStats { req: RequestId(31) });
    let u = kit.updates();
    assert!(matches!(&u[..], [UiUpdate::CacheStats { req: RequestId(31), result: Ok(_) }]), "{u:?}");
    PANIC_AFTER_REQUEST.with(|c| c.set(true));
    kit.core.dispatch(Command::UpdateSettings { req: Some(RequestId(32)), patch: serde_json::json!({ "volume": 0.4 }) });
    let saved: Vec<UiUpdate> = kit.updates().into_iter().filter(|u| matches!(u, UiUpdate::SettingsSaved { .. })).collect();
    assert!(matches!(&saved[..], [UiUpdate::SettingsSaved { req: RequestId(32), result: Ok(()) }]), "{saved:?}");
    // The tracking ends with the command: a later panicking request is answered with its failure again.
    PANIC_REQUEST.with(|c| c.set(true));
    kit.core.dispatch(Command::QueryCacheStats { req: RequestId(33) });
    assert!(matches!(&kit.updates()[..], [UiUpdate::CacheStats { req: RequestId(33), result: Err(_) }]));
}

/// The second hop of a key validation (`Catalog(ApiKey)`, where the key is saved) owes `ApiKey{req}` too.
#[test]
fn a_panicking_key_validation_hop_is_still_answered() {
    let mut kit = Kit::new(None, true);
    PANIC_REQUEST.with(|c| c.set(true));
    let result = vox_core::voices::ApiKeyResult { ok: true, error: None, warning: None, account: None };
    kit.core.dispatch(Command::Catalog(CatalogResult::ApiKey { req: RequestId(41), key: "sk_test_key_123".into(), result }));
    let u = kit.updates();
    assert!(
        matches!(&u[..], [UiUpdate::ApiKey { req: RequestId(41), result }]
            if !result.ok && result.error.as_deref() == Some(vox_core::copy::SOMETHING_WENT_WRONG)),
        "{u:?}"
    );
}

/// Each reaction is guarded: a panicking one (here the widget hide) is logged, the later ones (the volume) still run,
/// and the save is answered once, `Ok`.
#[test]
fn a_panicking_reaction_skips_no_other_reaction() {
    let mut kit = Kit::new(None, true);
    STEP_PANIC.with(|c| c.set(Some("widget reaction")));
    let patch = serde_json::json!({ "widget": { "enabled": false }, "volume": 0.3 });
    kit.core.dispatch(Command::UpdateSettings { req: Some(RequestId(51)), patch });
    assert_eq!(STEP_PANIC.with(Cell::get), None, "the widget reaction ran (and panicked)");
    assert_eq!(kit.core.services.player.snapshot().state.volume, 0.3, "the volume reaction after it still ran");
    let u = kit.updates();
    assert!(widget_intents(&u).is_empty());
    let saved: Vec<&UiUpdate> = u.iter().filter(|x| matches!(x, UiUpdate::SettingsSaved { .. })).collect();
    assert!(matches!(&saved[..], [UiUpdate::SettingsSaved { req: RequestId(51), result: Ok(()) }]), "{saved:?}");
}

/// A removed key is answered `Done{Ok}` even when a key-change reaction panics; the refetches after it still run.
#[test]
fn a_cleared_key_is_answered_ok_even_when_a_reaction_panics() {
    let mut kit = Kit::new(Some("sk_test_key_123"), true);
    let generation = kit.core.key_generation;
    STEP_PANIC.with(|c| c.set(Some("credits reaction")));
    kit.core.dispatch(Command::ClearApiKey { req: RequestId(61) });
    assert_eq!(STEP_PANIC.with(Cell::get), None, "the credits reaction ran (and panicked)");
    let u = kit.updates();
    let done: Vec<&UiUpdate> = u.iter().filter(|x| matches!(x, UiUpdate::Done { .. })).collect();
    assert!(matches!(&done[..], [UiUpdate::Done { req: RequestId(61), result: Ok(()) }]), "{done:?}");
    assert!(u.iter().any(|x| matches!(x, UiUpdate::Voices { .. })), "the voices refetch after it still ran");
    assert!(u.iter().any(|x| matches!(x, UiUpdate::AppInfo(_))));
    assert_eq!(kit.core.key_generation, generation + 1);
}

/// Each startup step is guarded: a panic in an early one (orphan removal) skips none of the later ones.
#[test]
fn a_panicking_startup_step_skips_no_later_step() {
    let mut kit = Kit::new(None, true);
    STEP_PANIC.with(|c| c.set(Some("cache orphans")));
    kit.core.startup();
    assert_eq!(STEP_PANIC.with(Cell::get), None, "the orphan step ran (and panicked)");
    let u = kit.updates();
    assert!(u.iter().any(|x| matches!(x, UiUpdate::AppInfo(_))), "the bridge sync's AppInfo");
    assert!(u.iter().any(|x| matches!(x, UiUpdate::Credits(c) if !c.refreshing)), "the first credits refresh settled");
    assert!(kit.core.timers.is_set(TimerId::Midnight));
    assert!(kit.core.timers.is_set(TimerId::RetentionInterval));
}

// ───────────────────────────── quit / session end ─────────────────────────────

#[test]
fn quit_mid_synthesis_persists_the_billing_and_publishes_quit() {
    let mut kit = Kit::new(None, true);
    // Long enough that the look-ahead holds synthesis after two or three chunks.
    let text = "This sentence is here to be read aloud by the mock voice. ".repeat(90);
    let result = kit.core.read(ReadRequest::new(text, ReadSource::Manual));
    let id = result.read_id.unwrap();
    // Let the mock engine deliver its first chunks without dispatching anything.
    std::thread::sleep(Duration::from_millis(1_200));
    kit.updates();
    kit.core.shutdown();
    let rec = kit.record(&id).unwrap();
    assert!(rec.characters_billed > 0, "billing known at stop time persisted");
    assert!(!rec.completed);
    assert!(kit.updates().iter().any(|u| matches!(u, UiUpdate::Quit)));
    assert!(kit.core.exit && kit.core.quitting);
    // Later commands are ignored.
    kit.core.dispatch(Command::QueryHome { req: RequestId(1) });
    assert!(kit.updates().is_empty());
}

#[test]
fn session_end_is_idempotent() {
    let mut kit = Kit::new(None, true);
    kit.core.update_settings(None, &serde_json::json!({ "volume": 0.5 }));
    kit.core.session_end();
    kit.core.session_end();
    assert!(kit.core.services.runtime.profile.settings_file().is_file(), "written synchronously");
}

/// reader-playback.md §12 #19: the listening values of the player's exact snapshot reach the record when the app
/// quits or the session ends, although the 2 s write throttle held them back.
fn listening_is_persisted_by(end: impl FnOnce(&mut Core)) {
    let mut kit = Kit::new(None, true);
    let text = "This sentence is here to be read aloud by the mock voice. ".repeat(4);
    let id = kit.core.read(ReadRequest::new(text, ReadSource::Manual)).read_id.expect("started");
    // The session sees its reports (first report and `playing` write; later ones are throttled).
    let played = kit.pump_until(Duration::from_secs(10), |c| {
        let s = &c.reader.last_snapshot.state;
        s.status == PlayerStatus::Playing && s.position_sec >= 1.0
    });
    assert!(played, "the read did not play: {:?}", kit.core.reader.last_snapshot.state);
    // Freeze the clock (the `paused` report stays queued, so only the persist path can write it).
    kit.core.services.player.command(PlayerCommand::Pause);
    assert!(kit.wait_player(Duration::from_secs(3), |s| s.state.status == PlayerStatus::Paused));
    let exact = kit.core.services.player.snapshot().state;
    let before = kit.record(&id).unwrap();
    assert!(before.listened_sec < vox_core::js::round1(exact.listened_sec), "{before:?} vs {exact:?}");
    end(&mut kit.core);
    let rec = kit.record(&id).unwrap();
    assert_eq!(rec.listened_sec, vox_core::js::round1(exact.listened_sec));
    assert_eq!(rec.max_position_sec, Some(vox_core::js::round1(exact.position_sec)));
    assert!(!rec.completed);
    let logged: f64 = rec.listen_log.as_ref().expect("the ledger credits the current hour").0.values().sum();
    assert!((logged - rec.listened_sec).abs() < 0.15, "ledger {logged} vs listened {}", rec.listened_sec);
}

#[test]
fn listening_is_persisted_on_session_end() {
    listening_is_persisted_by(|core| core.session_end());
}

#[test]
fn listening_is_persisted_on_quit() {
    listening_is_persisted_by(|core| core.shutdown());
}

/// ARCHITECTURE.md §9 integration: a service whose flush never returns → `Core::shutdown` still publishes
/// `UiUpdate::Quit` once the 3 s flush budget is spent.
#[test]
fn a_hung_service_cannot_hold_up_the_quit() {
    let mut kit = Kit::new(None, true);
    let (release, hold) = crossbeam_channel::bounded::<()>(0);
    EXTRA_SHUTDOWN_TASKS.with(|t| {
        t.borrow_mut().push((
            "hung",
            Box::new(move || {
                let _ = hold.recv();
            }),
        ))
    });
    let started = Instant::now();
    kit.core.shutdown();
    let elapsed = started.elapsed();
    assert!(kit.updates().iter().any(|u| matches!(u, UiUpdate::Quit)));
    assert!(kit.core.exit);
    assert!(elapsed >= lifecycle::SHUTDOWN_BUDGET, "the flush budget was waited for: {elapsed:?}");
    assert!(elapsed < lifecycle::SHUTDOWN_BUDGET + lifecycle::SERVICE_JOIN + Duration::from_millis(1_500), "{elapsed:?}");
    drop(release); // the hung step's thread ends
}

// ───────────────────────────── demo-mode smoke ─────────────────────────────

/// `VOX_DEMO=1 VOX_MOCK_TTS=1`: a read plays to the end through the null-output player; the record is completed,
/// listened, billed and cached; reports reached the UI.
#[test]
fn demo_smoke_a_read_plays_to_the_end() {
    let mut kit = Kit::new(None, true);
    kit.core.startup();
    kit.updates();
    let result = kit.core.read(ReadRequest::new("Hello there. This is Vox.", ReadSource::Manual));
    let id = result.read_id.expect("started");
    let finished = kit.pump_until(Duration::from_secs(20), |c| {
        c.services.history.get(&id).is_some_and(|r| r.completed && r.cached) && c.reader.last_snapshot.state.status == PlayerStatus::Ended
    });
    let rec = kit.record(&id).unwrap();
    assert!(finished, "the read did not finish: {rec:?}");
    assert!(rec.listened_sec > 0.0);
    assert!(rec.audio_duration_sec > 0.0);
    assert_eq!(rec.characters_billed, 25);
    assert!(rec.listen_log.is_some(), "the ledger credits the current hour");
    let u = kit.updates();
    assert!(u.iter().any(|x| matches!(x, UiUpdate::Player(s) if s.state.status == PlayerStatus::Playing)));
    assert!(u.iter().any(|x| matches!(x, UiUpdate::Player(s) if s.state.status == PlayerStatus::Ended)));
}

// ───────────────────────────── integration: a partial read (ARCHITECTURE.md §4.14) ─────────────────────────────

/// Standard base64 (the test's stand-in server encodes the audio itself).
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b = [group[0], *group.get(1).unwrap_or(&0), *group.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= group.len() {
                out.push(ABC[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A minimal ElevenLabs stand-in on `127.0.0.1`: the first `with-timestamps` request returns `audio`, every later
/// one answers 503; anything else 404. Returns the base URL and the synthesis request counter.
fn eleven_stand_in(audio: Vec<u8>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://127.0.0.1:{}", listener.local_addr().expect("addr").port());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let counter = counter.clone();
            let audio = audio.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head_end = loop {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let body_len = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                    .unwrap_or(0);
                while buf.len() < head_end + body_len {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let request_line = head.lines().next().unwrap_or_default().to_string();
                let (status, body) = if request_line.contains("/with-timestamps") {
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        (200, serde_json::json!({ "audio_base64": base64(&audio), "alignment": null }).to_string())
                    } else {
                        (503, serde_json::json!({ "detail": { "status": "service_unavailable", "message": "busy" } }).to_string())
                    }
                } else {
                    (404, "{}".to_string())
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (base, calls)
}

#[test]
fn the_base64_helper_matches_the_standard() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
}

/// Chunk 1 returns 503 four times while the null-output player has already drained chunk 0: the engine's
/// `Stopped{error}` reaches the core before the player's `ended`, so the read is stored partial, never completed.
#[test]
fn a_read_whose_later_chunk_fails_is_partial_never_completed() {
    let audio = vox_tts::mock::mock_synthesize("Hi.", 1.0).audio.to_vec(); // 0.6 s of WAV
    let (base, calls) = eleven_stand_in(audio);
    let mut kit = Kit::with_base_url(Some("sk_test_partial_read_key"), false, &base);
    let text = "This first sentence is long enough to need a second chunk of text. ".repeat(5);
    let result = kit.core.read(ReadRequest::new(text, ReadSource::Manual));
    let id = result.read_id.expect("started");
    let settled = kit.pump_until(Duration::from_secs(30), |c| {
        c.reader.last_snapshot.state.status == PlayerStatus::Ended && c.services.history.get(&id).is_some_and(|r| r.error.is_some())
    });
    let rec = kit.record(&id).unwrap();
    assert!(settled, "the read did not settle: {rec:?}");
    assert!(!rec.completed, "a partial read is never completed");
    assert_eq!(rec.error.as_deref(), Some(PARTIAL_READ_ERROR));
    assert!(!rec.cached, "nothing cached");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5, "chunk 0 once, chunk 1 four times (no timeout retries)");
    assert!(kit.core.reader.last_snapshot.warning.is_some(), "the player shows the stops-early warning");
}
