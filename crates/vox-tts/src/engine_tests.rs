//! Engine tests (ARCHITECTURE.md §9 vox-tts): mock-mode sessions and sessions against the `tiny_http` mock server.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::ThreadId;
use std::time::Instant;

use serde_json::json;
use vox_core::read::{eleven_messages as msg, ReadErrorCode};

use super::*;
use crate::client::test_support::{client, client_with, config, key_source, synthesis_reply, MockServer, Reply, Seen};

#[derive(Debug, Clone)]
enum Entry {
    Start(TtsSessionStart),
    Chunk(TtsChunk),
    Done(TtsSessionDone),
    Error(TtsSessionError),
    Event(EngineEvent),
    Mark(String),
}

impl Entry {
    fn clone_event(&self) -> Option<EngineEvent> {
        match self {
            Entry::Event(e) => Some(e.clone()),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Recorder {
    log: Mutex<Vec<Entry>>,
    panic_on_chunk: AtomicBool,
    event_threads: Mutex<Vec<ThreadId>>,
}

impl Recorder {
    fn push(&self, e: Entry) {
        self.log.lock().push(e);
    }
    fn entries(&self) -> Vec<Entry> {
        self.log.lock().clone()
    }
    fn mark(&self, m: impl Into<String>) {
        self.push(Entry::Mark(m.into()));
    }
    fn chunks(&self) -> Vec<TtsChunk> {
        self.entries().into_iter().filter_map(|e| if let Entry::Chunk(c) = e { Some(c) } else { None }).collect()
    }
    fn chunk_count(&self) -> usize {
        self.log.lock().iter().filter(|e| matches!(e, Entry::Chunk(_))).count()
    }
    fn events(&self) -> Vec<EngineEvent> {
        self.entries().into_iter().filter_map(|e| if let Entry::Event(ev) = e { Some(ev) } else { None }).collect()
    }
    fn wait_for(&self, pred: impl Fn(&[Entry]) -> bool) -> bool {
        let deadline = Instant::now() + WAIT;
        loop {
            let snapshot = self.entries(); // the predicate may lock the log itself
            if pred(&snapshot) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn position(&self, pred: impl Fn(&Entry) -> bool) -> Option<usize> {
        self.log.lock().iter().position(pred)
    }
}

impl TtsSink for Recorder {
    fn session_start(&self, start: TtsSessionStart) {
        self.push(Entry::Start(start));
    }
    fn chunk(&self, chunk: TtsChunk) {
        self.push(Entry::Chunk(chunk));
        if self.panic_on_chunk.load(Ordering::SeqCst) {
            panic!("sink failure (test)");
        }
    }
    fn done(&self, done: TtsSessionDone) {
        self.push(Entry::Done(done));
    }
    fn error(&self, error: TtsSessionError) {
        self.push(Entry::Error(error));
    }
}

const WAIT: Duration = Duration::from_secs(15);

fn engine_with(client: ElevenLabsClient, mock: bool, scale: f64) -> (TtsEngine, Arc<Recorder>) {
    let rec = Arc::new(Recorder::default());
    let r = rec.clone();
    let events: EngineEvents = Arc::new(move |e: EngineEvent| {
        r.event_threads.lock().push(std::thread::current().id());
        r.push(Entry::Event(e));
    });
    (TtsEngine::with_delay_scale(client, rec.clone(), events, mock, scale), rec)
}

fn offline_client() -> ElevenLabsClient {
    client("http://127.0.0.1:9", "unused")
}

fn mock_engine() -> (TtsEngine, Arc<Recorder>) {
    engine_with(offline_client(), true, 0.01)
}

fn opts(read_id: &str, text: &str) -> StartSessionOptions {
    StartSessionOptions {
        read_id: read_id.into(),
        text: text.into(),
        title: "Title".into(),
        voice_id: "voice-1".into(),
        voice_name: "George".into(),
        source_app: Some("Chrome".into()),
        settings: Settings::default(),
    }
}

/// `n` sentences of ~42 units: 40 → 3 chunks (204/876/587, tts.md §3.8), 80 → 5 chunks.
fn long_text(n: usize) -> String {
    (0..n).map(|i| format!("Sentence number {i} has a few words in it.")).collect::<Vec<_>>().join(" ")
}

fn len16s(text: &str) -> Vec<f64> {
    vox_text::chunk_text_default(text).iter().map(|c| utf16_len(&c.text) as f64).collect()
}

fn is_event(e: &Entry, f: impl Fn(&EngineEvent) -> bool) -> bool {
    matches!(e, Entry::Event(ev) if f(ev))
}

fn has_complete(e: &[Entry]) -> bool {
    e.iter().any(|x| is_event(x, |ev| matches!(ev, EngineEvent::Complete { .. })))
}

fn has_event(e: &[Entry]) -> bool {
    e.iter().any(|x| matches!(x, Entry::Event(_)))
}

fn has_done(e: &[Entry]) -> bool {
    e.iter().any(|x| matches!(x, Entry::Done(_)))
}

fn wait_until(f: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn reply_for(s: &Seen) -> Reply {
    let text = s.json()["text"].as_str().unwrap_or("").to_string();
    synthesis_reply(b"MP3", &text)
}

// ───────────── pure helpers ─────────────

#[test]
fn retry_delays_and_log_lines() {
    assert_eq!(retry_delay_ms(0, None), 1000.0);
    assert_eq!(retry_delay_ms(1, None), 2000.0);
    assert_eq!(retry_delay_ms(2, None), 4000.0);
    assert_eq!(retry_delay_ms(0, Some(3.0)), 3000.0); // retry-after: 3 → waits 3 s
    assert_eq!(retry_delay_ms(0, Some(30.0)), 10_000.0); // retry-after: 30 → capped at 10 s
    assert_eq!(retry_delay_ms(2, Some(1.5)), 4000.0); // a hint never shortens the backoff
    assert_eq!(retry_delay_ms(1, Some(2.5)), 2500.0);
    assert_eq!(failure_line(&TtsError::network()), "read failed: network");
    let e = TtsError { status: Some(401), ..TtsError::new(ReadErrorCode::Quota, "x") };
    assert_eq!(failure_line(&e), "read failed: quota (HTTP 401)");
    assert_eq!(estimate_sec(&"x".repeat(30), 2.0), 1.0);
    assert_eq!(estimate_sec(&"x".repeat(30), 0.0), 2.0);
    assert_eq!(estimate_sec("😀", 1.0), 2.0 / 15.0);
}

// ───────────── mock mode ─────────────

#[test]
fn session_start_is_delivered_before_start_returns() {
    let (engine, rec) = mock_engine();
    assert!(engine.is_mock());
    let text = "Hello there. General Kenobi.";
    let mut o = opts("read-1", text);
    o.settings.playback_rate = 1.5;
    o.settings.volume = 0.25;
    o.settings.widget.highlight_words = false;
    o.settings.voice.speed = 2.0;
    let sid = engine.start(o);
    let entries = rec.entries();
    let Some(Entry::Start(start)) = entries.first() else { panic!("no session_start: {entries:?}") };
    assert_eq!(entries.len(), 1, "nothing else before start returned");
    assert_eq!(start.session_id, sid);
    assert_eq!(sid.len(), 36);
    assert_eq!(start.read_id, "read-1");
    assert_eq!(&*start.text, text);
    assert_eq!((start.title.as_str(), start.voice_name.as_str(), start.voice_id.as_str()), ("Title", "George", "voice-1"));
    assert_eq!(start.source_app.as_deref(), Some("Chrome"));
    assert_eq!(start.estimated_duration_sec, 28.0 / 15.0 / 2.0);
    assert_eq!(start.chunk_count, 1);
    assert_eq!((start.rate, start.volume, start.highlight_words, start.from_cache), (1.5, 0.25, false, false));
    assert_eq!(&*start.sentences, &vox_text::sentence_starts(text)[..]);
    assert_eq!(engine.current_session_id().as_deref(), Some(sid.as_str()));
    assert_eq!(engine.current_read_id().as_deref(), Some("read-1"));
    // An empty source app is not forwarded.
    let mut o = opts("read-2", "Hi.");
    o.source_app = Some(String::new());
    let sid2 = engine.start(o);
    assert_ne!(sid, sid2);
    let starts: Vec<_> = rec.entries().into_iter().filter_map(|e| if let Entry::Start(s) = e { Some(s) } else { None }).collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].source_app, None);
}

#[test]
fn mock_read_completes_with_billing_and_order() {
    let (engine, rec) = mock_engine();
    let text = long_text(40);
    let sid = engine.start(opts("r1", &text));
    assert!(rec.wait_for(has_complete));
    let entries = rec.entries();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| match e {
            Entry::Start(_) => "start",
            Entry::Chunk(_) => "chunk",
            Entry::Done(_) => "done",
            Entry::Error(_) => "error",
            Entry::Event(_) => "event",
            Entry::Mark(_) => "mark",
        })
        .collect();
    assert_eq!(kinds, ["start", "chunk", "chunk", "chunk", "done", "event"]);
    let chunks = rec.chunks();
    let expected = vox_text::chunk_text_default(&text);
    assert_eq!(expected.len(), 3);
    for (i, (c, e)) in chunks.iter().zip(&expected).enumerate() {
        assert_eq!((c.index as usize, c.text_start, c.text_end), (i, e.start, e.end));
        assert_eq!(c.session_id, sid);
        assert_eq!(c.mime, "audio/wav");
        let len = utf16_len(&e.text) as f64;
        assert!((c.duration_sec - (len / 15.0).max(0.6)).abs() < 1e-3, "{} vs {}", c.duration_sec, len / 15.0);
        assert_eq!(c.alignment.as_ref().unwrap().chars.len(), utf16_len(&e.text));
    }
    let Some(Entry::Done(done)) = entries.get(4) else { panic!() };
    assert_eq!(done.total_chunks, 3);
    let Some(Entry::Event(EngineEvent::Complete { read_id, chunks: cached, characters_billed })) = entries.get(5) else {
        panic!()
    };
    assert_eq!(read_id, "r1");
    assert_eq!(*characters_billed, len16s(&text).iter().sum::<f64>());
    assert_eq!(cached.len(), 3);
    assert_eq!((cached[1].index, cached[1].text_start), (1, expected[1].start));
    assert!(Arc::ptr_eq(&cached[2].audio, &chunks[2].audio)); // one shared buffer
    assert_eq!(cached[0].alignment.as_ref(), chunks[0].alignment.as_deref());
    // The session is kept after synthesis finished, until stop(); a stop then reports nothing.
    assert_eq!(engine.current_session_id().as_deref(), Some(sid.as_str()));
    engine.stop();
    assert_eq!(engine.current_session_id(), None);
    assert_eq!(engine.current_read_id(), None);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rec.events().len(), 1);
    engine.stop(); // idempotent
}

#[test]
fn empty_text_fails_with_empty_text() {
    let (engine, rec) = mock_engine();
    engine.start(opts("r-empty", "   "));
    assert!(rec.wait_for(has_event));
    let entries = rec.entries();
    let Some(Entry::Start(start)) = entries.first() else { panic!() };
    assert_eq!(start.chunk_count, 0);
    let Some(Entry::Error(err)) = entries.get(1) else { panic!("{entries:?}") };
    assert!(err.fatal);
    assert_eq!((err.code, err.message.as_str()), (ReadErrorCode::EmptyText, msg::NO_TEXT));
    let Some(Entry::Event(EngineEvent::Failed { read_id, error })) = entries.get(2) else { panic!() };
    assert_eq!((read_id.as_str(), error.code), ("r-empty", ReadErrorCode::EmptyText));
}

/// Chunk 2 is requested at once (75 s < 120 s at 15 chars/s); chunk 3 waits for `progress`.
#[test]
fn lookahead_waits_for_the_listener() {
    let (engine, rec) = mock_engine();
    let text = long_text(80);
    let lens = len16s(&text);
    assert!(lens.len() >= 5, "{lens:?}");
    let durations: Vec<f64> = lens.iter().map(|l| l / 15.0).collect();
    assert!(durations[0] + durations[1] < 120.0, "{durations:?}");
    assert!(durations[0] + durations[1] + durations[2] >= 120.0, "{durations:?}");
    let sid = engine.start(opts("r-la", &text));
    assert!(rec.wait_for(|_| rec.chunk_count() == 3));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(rec.chunk_count(), 3, "chunk 3 must wait for the listener");
    // Reports for another session, or non-finite ones, are ignored.
    engine.progress("another-session", 1_000.0);
    engine.progress(&sid, f64::NAN);
    engine.progress(&sid, f64::INFINITY);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(rec.chunk_count(), 3);
    // Just enough playback: delivered − position < 120 → one more chunk, then it waits again.
    let delivered: f64 = rec.chunks().iter().map(|c| c.duration_sec).sum();
    engine.progress(&sid, delivered - 119.0);
    assert!(rec.wait_for(|_| rec.chunk_count() == 4));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(rec.chunk_count(), 4);
    // A seek back moves the playhead backwards (negative counts as 0); then the listener catches up.
    engine.progress(&sid, -5.0);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rec.chunk_count(), 4);
    engine.progress(&sid, 10_000.0);
    assert!(rec.wait_for(has_complete));
    assert_eq!(rec.chunk_count(), lens.len());
}

#[test]
fn stop_reports_billing_synchronously_and_silences_the_session() {
    let (engine, rec) = mock_engine();
    let text = long_text(80);
    let lens = len16s(&text);
    engine.start(opts("r-stop", &text));
    assert!(rec.wait_for(|_| rec.chunk_count() == 3));
    engine.stop();
    // Emitted before stop() returned, on this thread.
    assert_eq!(
        rec.events(),
        [EngineEvent::Stopped { read_id: "r-stop".into(), characters_billed: lens[..3].iter().sum(), error: None }]
    );
    assert_eq!(rec.event_threads.lock().last().copied(), Some(std::thread::current().id()));
    assert_eq!(engine.current_session_id(), None);
    // Nothing follows for the stopped session.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(rec.chunk_count(), 3);
    assert_eq!(rec.events().len(), 1);
    assert!(!has_done(&rec.entries()));
}

#[test]
fn stop_before_anything_was_billed_reports_nothing() {
    let (engine, rec) = engine_with(offline_client(), true, 1.0); // 350 ms first latency
    engine.start(opts("r-early", "Hello."));
    engine.stop();
    std::thread::sleep(Duration::from_millis(500));
    assert!(rec.events().is_empty());
    assert_eq!(rec.entries().len(), 1); // only session_start
}

#[test]
fn supersede_orders_the_old_stopped_before_the_new_session() {
    let (engine, rec) = mock_engine();
    let text = long_text(80);
    let lens = len16s(&text);
    let old = engine.start(opts("old", &text));
    assert!(rec.wait_for(|_| rec.chunk_count() == 3));
    let new = engine.start(opts("new", "A short new read."));
    // Before start() returned: the old session's Stopped, then the new session_start, nothing else.
    let stopped_at = rec
        .position(|e| is_event(e, |ev| matches!(ev, EngineEvent::Stopped { read_id, .. } if read_id == "old")))
        .expect("old stopped");
    let start_at = rec.position(|e| matches!(e, Entry::Start(s) if s.session_id == new)).expect("new start");
    assert!(stopped_at < start_at);
    assert_eq!(rec.entries().len(), start_at + 1, "nothing of the new session before start() returned");
    assert!(rec.wait_for(|e| e.iter().any(|x| is_event(x, |ev| matches!(ev, EngineEvent::Complete { read_id, .. } if read_id == "new")))));
    let entries = rec.entries();
    assert!(entries[start_at..].iter().all(|e| !matches!(e, Entry::Chunk(c) if c.session_id == old)));
    assert!(!entries.iter().any(|e| is_event(e, |ev| matches!(ev, EngineEvent::Complete { read_id, .. } if read_id == "old"))));
    let Some(Entry::Event(EngineEvent::Stopped { characters_billed, .. })) = entries.get(stopped_at) else { panic!() };
    assert_eq!(*characters_billed, lens[..3].iter().sum::<f64>());
}

#[test]
fn sink_panics_do_not_stop_synthesis_or_complete() {
    let (engine, rec) = mock_engine();
    rec.panic_on_chunk.store(true, Ordering::SeqCst);
    engine.start(opts("r-panic", &long_text(40)));
    assert!(rec.wait_for(has_complete));
    let Some(EngineEvent::Complete { chunks, .. }) = rec.events().pop() else { panic!() };
    assert_eq!(chunks.len(), 3);
}

#[test]
fn listener_panics_are_contained() {
    let rec = Arc::new(Recorder::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let events: EngineEvents = Arc::new(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        panic!("listener failure (test)");
    });
    let engine = TtsEngine::with_delay_scale(offline_client(), rec.clone(), events, true, 0.01);
    engine.start(opts("a", "One."));
    assert!(rec.wait_for(has_done));
    assert!(wait_until(|| calls.load(Ordering::SeqCst) == 1));
    engine.start(opts("b", "Two."));
    assert!(rec.wait_for(|e| e.iter().filter(|x| matches!(x, Entry::Done(_))).count() == 2));
    assert!(wait_until(|| calls.load(Ordering::SeqCst) == 2));
}

/// Start/stop in a tight loop: no `Failed`/`Complete`/`Stopped{error}` for a read after its `stop()` returned.
fn tight_loop(engine: &TtsEngine, rec: &Recorder, rounds: usize) {
    for i in 0..rounds {
        let read = format!("loop-{i}");
        engine.start(opts(&read, &long_text(1 + i % 7)));
        if i % 3 == 1 {
            std::thread::sleep(Duration::from_micros(200 * (i % 5) as u64));
        }
        engine.stop();
        rec.mark(read);
    }
    std::thread::sleep(Duration::from_millis(400));
    let entries = rec.entries();
    for (at, e) in entries.iter().enumerate() {
        let Entry::Mark(read) = e else { continue };
        for later in &entries[at + 1..] {
            match later {
                Entry::Event(EngineEvent::Complete { read_id, .. } | EngineEvent::Failed { read_id, .. }) => {
                    assert_ne!(read_id, read, "{later:?} after its stop()")
                }
                Entry::Event(EngineEvent::Stopped { read_id, error, .. }) => {
                    assert!(!(read_id == read && error.is_some()), "{later:?} after its stop()")
                }
                _ => {}
            }
        }
    }
}

#[test]
fn tight_start_stop_loop_mock() {
    let (engine, rec) = engine_with(offline_client(), true, 0.0);
    tight_loop(&engine, &rec, 300);
}

#[test]
fn tight_start_stop_loop_network() {
    let server = MockServer::start(|s, _| reply_for(s).header("x-character-count", "10"));
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.0);
    tight_loop(&engine, &rec, 100);
}

#[test]
fn play_cached_renumbers_and_never_completes() {
    let (engine, rec) = mock_engine();
    let cached = |index: u32, start: u32, end: u32, d: f64| CachedChunk {
        index,
        text_start: start,
        text_end: end,
        mime: "audio/mpeg".into(),
        alignment: None,
        duration_sec: d,
        audio: Arc::from(vec![index as u8]),
    };
    let text = "First part. Second part. Third part.";
    let sid = engine.play_cached(PlayCachedOptions {
        read_id: "cached-1".into(),
        text: text.into(),
        title: "T".into(),
        voice_id: "v".into(),
        voice_name: "George".into(),
        source_app: None,
        settings: Settings { playback_rate: 2.0, ..Settings::default() },
        chunks: vec![cached(7, 25, 36, 1.5), cached(2, 0, 11, 1.0), cached(2, 12, 24, 2.0)],
    });
    let entries = rec.entries();
    let Some(Entry::Start(start)) = entries.first() else { panic!() };
    assert_eq!(entries.len(), 1);
    assert!(start.from_cache);
    assert_eq!((start.chunk_count, start.estimated_duration_sec, start.rate), (3, 4.5, 2.0));
    assert_eq!((start.voice_id.as_str(), start.read_id.as_str()), ("v", "cached-1"));
    assert_eq!(&*start.sentences, &vox_text::sentence_starts(text)[..]);
    assert!(rec.wait_for(has_done));
    let chunks = rec.chunks();
    assert_eq!(chunks.iter().map(|c| c.index).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(chunks.iter().map(|c| c.text_start).collect::<Vec<_>>(), [0, 12, 25]); // stable sort
    assert_eq!(chunks.iter().map(|c| c.audio[0]).collect::<Vec<_>>(), [2, 2, 7]);
    assert_eq!(chunks.iter().map(|c| c.duration_sec).collect::<Vec<_>>(), [1.0, 2.0, 1.5]);
    assert!(chunks.iter().all(|c| c.session_id == sid));
    let Some(Entry::Done(done)) = rec.entries().last().cloned() else { panic!() };
    assert_eq!(done.total_chunks, 3);
    std::thread::sleep(Duration::from_millis(50));
    assert!(rec.events().is_empty(), "no Complete, no billing for cached playback");
    engine.stop();
    assert!(rec.events().is_empty());
}

#[test]
fn play_cached_without_chunks_fails_not_cached() {
    let (engine, rec) = mock_engine();
    engine.play_cached(PlayCachedOptions {
        read_id: "gone".into(),
        text: "x".into(),
        title: "T".into(),
        voice_id: String::new(),
        voice_name: String::new(),
        source_app: None,
        settings: Settings::default(),
        chunks: vec![],
    });
    assert!(rec.wait_for(has_event));
    let entries = rec.entries();
    let Some(Entry::Error(err)) = entries.get(1) else { panic!("{entries:?}") };
    assert!(err.fatal);
    assert_eq!((err.code, err.message.as_str()), (ReadErrorCode::NotCached, msg::CACHED_AUDIO_MISSING));
    assert!(matches!(&entries[2], Entry::Event(EngineEvent::Failed { read_id, error })
        if read_id == "gone" && error.code == ReadErrorCode::NotCached));
}

#[test]
fn play_cached_supersedes_a_billed_session() {
    let (engine, rec) = mock_engine();
    engine.start(opts("fresh", &long_text(80)));
    assert!(rec.wait_for(|_| rec.chunk_count() == 3));
    engine.play_cached(PlayCachedOptions {
        read_id: "old-read".into(),
        text: "Cached.".into(),
        title: "T".into(),
        voice_id: "v".into(),
        voice_name: "V".into(),
        source_app: None,
        settings: Settings::default(),
        chunks: vec![CachedChunk {
            index: 0,
            text_start: 0,
            text_end: 7,
            mime: "audio/wav".into(),
            alignment: None,
            duration_sec: 0.6,
            audio: crate::mock::mock_synthesize("Cached.", 1.0).audio,
        }],
    });
    assert!(matches!(rec.events().as_slice(), [EngineEvent::Stopped { read_id, error: None, .. }] if read_id == "fresh"));
    assert!(rec.wait_for(has_done));
}

// ───────────── against the mock server ─────────────

#[test]
fn requests_carry_the_settings_and_context() {
    let server = MockServer::start(|s, _| reply_for(s));
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.01);
    let text = long_text(40);
    let mut o = opts("r-net", &text);
    o.voice_id = "voice x".into();
    o.settings.model_id = "eleven_multilingual_v2".into();
    o.settings.language_code = "de".into();
    o.settings.output_format = vox_core::settings::OutputFormat::Mp3_22050_32;
    engine.start(o);
    assert!(rec.wait_for(has_complete));
    let seen = server.seen();
    assert_eq!(seen.len(), 3);
    let chunks = vox_text::chunk_text_default(&text);
    for (i, s) in seen.iter().enumerate() {
        assert_eq!(s.url, "/v1/text-to-speech/voice%20x/with-timestamps?output_format=mp3_22050_32");
        let body = s.json();
        assert_eq!(body["text"], json!(chunks[i].text));
        assert_eq!(body["model_id"], json!("eleven_multilingual_v2"));
        assert_eq!(body["language_code"], json!("de"));
        let prev = vox_text::context_before(i.checked_sub(1).map(|p| chunks[p].text.as_str()));
        let next = vox_text::context_after(chunks.get(i + 1).map(|c| c.text.as_str()));
        assert_eq!(body.get("previous_text").and_then(|v| v.as_str()), prev.as_deref());
        assert_eq!(body.get("next_text").and_then(|v| v.as_str()), next.as_deref());
    }
    assert!(seen[0].json().get("previous_text").is_none());
    assert!(seen[2].json().get("next_text").is_none());
    // Billing falls back to UTF-16 lengths without the header; durations come from the alignment.
    let Some(EngineEvent::Complete { characters_billed, .. }) = rec.events().pop() else { panic!() };
    assert_eq!(characters_billed, len16s(&text).iter().sum::<f64>());
    let c0 = &rec.chunks()[0];
    assert_eq!(c0.mime, "audio/mpeg");
    assert!((c0.duration_sec - chunks[0].text.chars().count() as f64 * 0.05).abs() < 1e-9);
}

/// D-NET-5: `error(non-fatal)` → `Stopped{error}` → `done`, with fractional billing summed unrounded.
#[test]
fn later_chunk_failure_reports_stopped_before_done() {
    let server = MockServer::start(|s, n| {
        if n < 2 {
            reply_for(s).header("x-character-count", "1.5")
        } else {
            Reply::json(401, json!({"detail":{"status":"quota_exceeded","message":"quota"}}))
        }
    });
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.01);
    let sid = engine.start(opts("r-cut", &long_text(40)));
    assert!(rec.wait_for(has_done));
    std::thread::sleep(Duration::from_millis(50));
    let entries = rec.entries();
    let error_at = rec.position(|e| matches!(e, Entry::Error(_))).unwrap();
    let stopped_at = rec.position(|e| is_event(e, |ev| matches!(ev, EngineEvent::Stopped { .. }))).unwrap();
    let done_at = rec.position(|e| matches!(e, Entry::Done(_))).unwrap();
    assert!(error_at < stopped_at && stopped_at < done_at, "{entries:?}");
    let Entry::Error(err) = &entries[error_at] else { panic!() };
    assert!(!err.fatal);
    assert_eq!((err.code, err.message.as_str(), err.session_id.as_str()), (ReadErrorCode::Quota, msg::QUOTA, sid.as_str()));
    let Entry::Event(EngineEvent::Stopped { read_id, characters_billed, error: Some(e) }) = &entries[stopped_at] else {
        panic!()
    };
    // Billing 1.5 + 1.5 → 3 (unrounded sum; the reader rounds once).
    assert_eq!((read_id.as_str(), *characters_billed, e.code), ("r-cut", 3.0, ReadErrorCode::Quota));
    let Entry::Done(done) = &entries[done_at] else { panic!() };
    assert_eq!(done.total_chunks, 2);
    assert_eq!(rec.chunk_count(), 2);
    assert!(!has_complete(&entries));
    assert_eq!(server.count(), 3, "quota is not retried");
    // The session finished (cut short): a later stop reports nothing more.
    engine.stop();
    assert_eq!(rec.events().len(), 1);
}

#[test]
fn first_chunk_failure_is_fatal() {
    let server = MockServer::start(|_, _| Reply::json(401, json!({"detail":{"status":"invalid_api_key"}})));
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.01);
    engine.start(opts("r-bad", "Hello there."));
    assert!(rec.wait_for(has_event));
    let entries = rec.entries();
    assert!(matches!(&entries[1], Entry::Error(e) if e.fatal && e.code == ReadErrorCode::Unauthorized && e.message == msg::BAD_KEY));
    assert!(matches!(&entries[2], Entry::Event(EngineEvent::Failed { read_id, error }) if read_id == "r-bad" && error.status == Some(401)));
    assert_eq!(entries.len(), 3);
}

#[test]
fn retries_only_retryable_errors() {
    // 503, then 429 with a 30 s hint, then 200: three attempts for one chunk.
    let server = MockServer::start(|s, n| match n {
        0 => Reply::json(503, json!({})),
        1 => Reply::json(429, json!({})).header("retry-after", "30"),
        _ => reply_for(s),
    });
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.01);
    let started = Instant::now();
    engine.start(opts("r-retry", "Hello."));
    assert!(rec.wait_for(has_complete));
    assert_eq!(server.count(), 3);
    // Waits: the 1 s step, then the 30 s hint capped at 10 s (× 0.01 → ≥ 110 ms in all).
    assert!(started.elapsed() >= Duration::from_millis(105), "{:?}", started.elapsed());

    // Four 5xx in a row: the error stands after 4 attempts.
    let server = MockServer::start(|_, _| Reply::json(500, json!({})));
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.01);
    engine.start(opts("r-5xx", "Hello."));
    assert!(rec.wait_for(has_event));
    assert_eq!(server.count(), 4);
    assert!(matches!(&rec.events()[0], EngineEvent::Failed { error, .. } if error.message == msg::SERVER));

    // Non-retryable 4xx: one attempt.
    let server = MockServer::start(|_, _| Reply::json(400, json!({"detail":{"message":"nope"}})));
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.01);
    engine.start(opts("r-400", "Hello."));
    assert!(rec.wait_for(has_event));
    assert_eq!(server.count(), 1);
}

#[test]
fn timeouts_are_never_retried() {
    // A generous 600 ms budget so a loaded machine still delivers the request before the timeout fires.
    let server = MockServer::start(|s, _| reply_for(s).delay(Duration::from_millis(2_000)));
    let c = client_with(&server.base_url, key_source("k"), Duration::from_millis(600), Duration::from_millis(600));
    let (engine, rec) = engine_with(c, false, 0.01);
    engine.start(opts("r-timeout", "Hello."));
    assert!(rec.wait_for(has_event));
    std::thread::sleep(Duration::from_millis(100));
    let events = rec.events();
    assert_eq!(server.count(), 1, "{events:?}");
    assert!(matches!(&events[0], EngineEvent::Failed { error, .. } if *error == TtsError::timeout()), "{events:?}");
}

#[test]
fn retry_sleeps_end_at_stop() {
    let server = MockServer::start(|_, _| Reply::json(503, json!({})));
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 1.0); // the real 1 s backoff
    engine.start(opts("r-sleep", "Hello."));
    assert!(wait_until(|| server.count() == 1));
    std::thread::sleep(Duration::from_millis(50));
    let stopped = Instant::now();
    engine.stop();
    assert!(stopped.elapsed() < Duration::from_millis(500), "stop never waits for the worker");
    std::thread::sleep(Duration::from_millis(1_300));
    assert_eq!(server.count(), 1, "no retry after the stop");
    assert!(rec.events().is_empty());
}

/// D-NET-4: audio that arrives after a stop is billed (cumulative trailing `Stopped`), never delivered.
#[test]
fn audio_after_stop_is_billed_once_more() {
    let server = MockServer::start(|s, n| {
        let reply = reply_for(s).header("x-character-count", "100");
        if n == 1 {
            reply.delay(Duration::from_millis(400))
        } else {
            reply
        }
    });
    let (engine, rec) = engine_with(client(&server.base_url, "k"), false, 0.01);
    engine.start(opts("r-late", &long_text(40)));
    assert!(rec.wait_for(|_| rec.chunk_count() == 1));
    assert!(wait_until(|| server.count() == 2)); // chunk 1 in flight
    engine.stop();
    assert_eq!(rec.events(), [EngineEvent::Stopped { read_id: "r-late".into(), characters_billed: 100.0, error: None }]);
    // A new read meanwhile: the trailing event may follow its events; consumers key it by read id.
    engine.start(opts("r-next", "Next."));
    let late_count = |e: &[Entry]| {
        e.iter().filter(|x| is_event(x, |ev| matches!(ev, EngineEvent::Stopped { read_id, .. } if read_id == "r-late"))).count()
    };
    assert!(rec.wait_for(|e| late_count(e) == 2));
    let late: Vec<_> =
        rec.events().into_iter().filter(|e| matches!(e, EngineEvent::Stopped { read_id, .. } if read_id == "r-late")).collect();
    assert_eq!(late[1], EngineEvent::Stopped { read_id: "r-late".into(), characters_billed: 200.0, error: None });
    // The late audio was never delivered, and nothing more was requested for the stopped read.
    assert!(rec.wait_for(has_complete));
    assert_eq!(rec.chunks().iter().filter(|c| c.index == 1).count(), 0);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(server.count(), 3); // r-late ×2 + r-next ×1
    assert_eq!(late_count(&rec.entries()), 2);
}

#[test]
fn key_cleared_mid_read_cuts_the_session_short() {
    let key = Arc::new(Mutex::new(Some("k".to_string())));
    let k = key.clone();
    let source: crate::KeySource = Arc::new(move || k.lock().clone());
    let clear = key.clone();
    let server = MockServer::start(move |s, _| {
        *clear.lock() = None; // the user removes the key while chunk 0 is being synthesized
        reply_for(s)
    });
    let c = ElevenLabsClient::new(source, config(&server.base_url));
    let (engine, rec) = engine_with(c, false, 0.01);
    engine.start(opts("r-key", &long_text(40)));
    assert!(rec.wait_for(has_done));
    let entries = rec.entries();
    let error_at = rec.position(|e| matches!(e, Entry::Error(_))).unwrap();
    let stopped_at = rec.position(|e| is_event(e, |ev| matches!(ev, EngineEvent::Stopped { .. }))).unwrap();
    let done_at = rec.position(|e| matches!(e, Entry::Done(_))).unwrap();
    assert!(error_at < stopped_at && stopped_at < done_at);
    assert!(matches!(&entries[error_at], Entry::Error(e) if !e.fatal && e.code == ReadErrorCode::NoApiKey && e.message == msg::NO_KEY));
    assert!(matches!(&entries[stopped_at], Entry::Event(EngineEvent::Stopped { error: Some(e), .. }) if e.code == ReadErrorCode::NoApiKey));
    assert!(matches!(&entries[done_at], Entry::Done(d) if d.total_chunks == 1));
    assert_eq!(server.count(), 1);
}

#[test]
fn worker_panic_fails_the_session() {
    let source: crate::KeySource = Arc::new(|| panic!("key store bug (test)"));
    let c = ElevenLabsClient::new(source, config("http://127.0.0.1:9"));
    let (engine, rec) = engine_with(c, false, 0.01);
    engine.start(opts("r-crash", "Hello."));
    assert!(rec.wait_for(has_event));
    let entries = rec.entries();
    assert!(matches!(&entries[1], Entry::Error(e)
        if e.fatal && e.message == msg::ENGINE_CRASHED && e.code == ReadErrorCode::Unknown));
    assert!(matches!(&entries[2], Entry::Event(EngineEvent::Failed { read_id, error })
        if read_id == "r-crash" && *error == TtsError::crashed()));
    // The engine keeps working.
    engine.stop();
    assert_eq!(engine.current_session_id(), None);
}

/// A panic after chunk 0 was delivered (here: the key source, on its second call = chunk 1's request) ends the
/// session as cut short, so the billed characters reach the reader and the delivered audio keeps playing.
#[test]
fn worker_panic_after_delivery_cuts_the_session_short() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let source: crate::KeySource = Arc::new(move || {
        if c.fetch_add(1, Ordering::SeqCst) >= 1 {
            panic!("key store bug (test)");
        }
        Some("k".to_string())
    });
    let server = MockServer::start(|s, _| reply_for(s).header("x-character-count", "100"));
    let (engine, rec) = engine_with(ElevenLabsClient::new(source, config(&server.base_url)), false, 0.01);
    let sid = engine.start(opts("r-crash-late", &long_text(40)));
    assert!(rec.wait_for(has_done));
    std::thread::sleep(Duration::from_millis(50));
    let entries = rec.entries();
    assert_eq!(entries.len(), 5, "{entries:?}");
    assert!(matches!(&entries[0], Entry::Start(_)));
    assert!(matches!(&entries[1], Entry::Chunk(c) if c.index == 0));
    assert!(matches!(&entries[2], Entry::Error(e)
        if !e.fatal && e.message == msg::ENGINE_CRASHED && e.code == ReadErrorCode::Unknown && e.session_id == sid));
    assert_eq!(
        entries[3].clone_event(),
        Some(EngineEvent::Stopped { read_id: "r-crash-late".into(), characters_billed: 100.0, error: Some(TtsError::crashed()) })
    );
    assert!(matches!(&entries[4], Entry::Done(d) if d.total_chunks == 1));
    assert_eq!(server.count(), 1);
    // Finished: a later stop reports nothing more, and the engine keeps working.
    engine.stop();
    assert_eq!(rec.events().len(), 1);
    assert_eq!(engine.current_session_id(), None);
}

/// D-NET-2 end to end: a synthesis request that was written and then lost its connection is never re-sent.
#[test]
fn failures_after_sending_are_not_retried() {
    let (url, accepted) = crate::client::test_support::hangup_server();
    let (engine, rec) = engine_with(client(&url, "k"), false, 0.01);
    engine.start(opts("r-lost", "Hello."));
    assert!(rec.wait_for(has_event));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert!(matches!(&rec.events()[0], EngineEvent::Failed { error, .. } if *error == TtsError::network() && !error.retryable));
}

#[test]
fn dropping_the_engine_ends_the_worker() {
    let (engine, rec) = mock_engine();
    engine.start(opts("r-drop", &long_text(80)));
    assert!(rec.wait_for(|_| rec.chunk_count() == 3));
    drop(engine);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(rec.chunk_count(), 3);
    assert!(rec.events().is_empty());
}
