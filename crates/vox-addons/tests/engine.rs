//! A real `TtsEngine` (mock = false) reading with add-on voices through `AddonHost` (docs/addons-design.md "Tests",
//! the `tests/engine.rs` row). The ElevenLabs client points at a local stand-in that counts its requests: an add-on
//! read must never reach it.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::*;
use parking_lot::Mutex;
use vox_addons::{AddonHost, AddonRuntime, BrokenReason, RuntimeStatus};
use vox_core::providers::Provider;
use vox_core::read::ReadErrorCode;
use vox_core::settings::Settings;
use vox_core::tts::{TtsChunk, TtsSessionDone, TtsSessionError, TtsSessionStart, MIME_WAV};
use vox_tts::wav::read_wav_info;
use vox_tts::engine::EngineEvents;
use vox_tts::{ClientConfig, ElevenLabsClient, EngineEvent, KeySource, SpeechProvider, StartSessionOptions, TtsEngine, TtsError, TtsSink};

const WAIT: Duration = Duration::from_secs(10);
const VOICE: &str = "addon:fake-a:alpha";
const LOG: &str = "fake-a-log.txt";
const COUNT: &str = "fake-a-count.txt";

/// A local ElevenLabs API that answers every request with a non-retryable 400 and counts them.
struct ElevenLabsStandIn {
    base_url: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ElevenLabsStandIn {
    fn start() -> ElevenLabsStandIn {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind the ElevenLabs stand-in");
        let port = server.server_addr().to_ip().expect("ip listener").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hits.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let Ok(Some(request)) = server.recv_timeout(Duration::from_millis(20)) else { continue };
                h.fetch_add(1, Ordering::SeqCst);
                let _ = request.respond(tiny_http::Response::from_string("{\"detail\":\"no\"}").with_status_code(400));
            }
        });
        ElevenLabsStandIn { base_url: format!("http://127.0.0.1:{port}"), hits, stop, thread: Some(thread) }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for ElevenLabsStandIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Debug, Clone)]
enum Entry {
    Start,
    Chunk(TtsChunk),
    Done(TtsSessionDone),
    Error(TtsSessionError),
    Event(EngineEvent),
}

type ChunkHook = Box<dyn FnMut(&TtsChunk) + Send>;

/// The sink and event listener: records everything, and runs `on_chunk` (on the session worker, before it asks for
/// the next chunk) for each chunk delivered.
#[derive(Default)]
struct Recorder {
    log: Mutex<Vec<Entry>>,
    on_chunk: Mutex<Option<ChunkHook>>,
}

impl Recorder {
    fn push(&self, e: Entry) {
        self.log.lock().push(e);
    }

    fn entries(&self) -> Vec<Entry> {
        self.log.lock().clone()
    }

    fn chunks(&self) -> Vec<TtsChunk> {
        self.entries().into_iter().filter_map(|e| if let Entry::Chunk(c) = e { Some(c) } else { None }).collect()
    }

    fn errors(&self) -> Vec<TtsSessionError> {
        self.entries().into_iter().filter_map(|e| if let Entry::Error(x) = e { Some(x) } else { None }).collect()
    }

    fn events(&self) -> Vec<EngineEvent> {
        self.entries().into_iter().filter_map(|e| if let Entry::Event(x) = e { Some(x) } else { None }).collect()
    }

    fn position(&self, f: impl Fn(&Entry) -> bool) -> Option<usize> {
        self.log.lock().iter().position(f)
    }

    fn wait_for(&self, f: impl Fn(&[Entry]) -> bool) -> bool {
        eventually(WAIT, || f(&self.entries()))
    }

    fn on_chunk(&self, hook: impl FnMut(&TtsChunk) + Send + 'static) {
        *self.on_chunk.lock() = Some(Box::new(hook));
    }
}

impl TtsSink for Recorder {
    fn session_start(&self, _: TtsSessionStart) {
        self.push(Entry::Start);
    }
    fn chunk(&self, chunk: TtsChunk) {
        self.push(Entry::Chunk(chunk.clone()));
        if let Some(hook) = self.on_chunk.lock().as_mut() {
            hook(&chunk);
        }
    }
    fn done(&self, done: TtsSessionDone) {
        self.push(Entry::Done(done));
    }
    fn error(&self, error: TtsSessionError) {
        self.push(Entry::Error(error));
    }
}

/// An engine wired like vox-app's: the add-on host registered for `Provider::Addon`, ElevenLabs at the stand-in with
/// a key set (so a misrouted read would really reach it).
struct Rig {
    engine: TtsEngine,
    rec: Arc<Recorder>,
    host: Arc<AddonHost>,
    health: Arc<Mutex<Vec<String>>>,
    eleven: ElevenLabsStandIn,
    scratch: Scratch,
}

impl Rig {
    fn new() -> Rig {
        let eleven = ElevenLabsStandIn::start();
        let key: KeySource = Arc::new(|| Some("sk-test-key".to_string()));
        let config = ClientConfig { base_url: eleven.base_url.clone(), user_agent: vox_tts::user_agent(), proxy: None, platform_roots: false };
        let client = ElevenLabsClient::new(key, config);
        let rec = Arc::new(Recorder::default());
        let r = rec.clone();
        let events: EngineEvents = Arc::new(move |e: EngineEvent| r.push(Entry::Event(e)));
        let engine = TtsEngine::new(client, rec.clone(), events, false);
        let health = Arc::new(Mutex::new(Vec::new()));
        let h = health.clone();
        let host = Arc::new(AddonHost::new(move |id: &str| h.lock().push(id.to_string())));
        engine.set_provider(Provider::Addon, Some(host.clone() as Arc<dyn SpeechProvider>));
        Rig { engine, rec, host, health, eleven, scratch: Scratch::new() }
    }

    /// A rig with "Fake A" (`fake-a`, voices alpha and beta) installed, with these extra fake flags.
    fn with_fake_a(args: &[&str]) -> Rig {
        let rig = Rig::new();
        rig.host.replace(fake_a("1.0.0", args).runtime(&rig.scratch));
        rig
    }

    fn start(&self, voice_id: &str, text: &str) -> String {
        self.engine.start(StartSessionOptions {
            read_id: "read-1".into(),
            text: text.into(),
            title: "Title".into(),
            voice_id: voice_id.into(),
            voice_name: "Alpha (Fake A)".into(),
            source_app: None,
            settings: Settings::default(),
        })
    }

    /// The fake's `text <id> <tag> <first 40 chars>` log lines, as (tag, text).
    fn texts(&self) -> Vec<(String, String)> {
        lines(&self.scratch.file(LOG))
            .iter()
            .filter_map(|l| {
                let mut parts = l.strip_prefix("text ")?.splitn(3, ' ');
                let _id = parts.next()?;
                Some((parts.next()?.to_string(), parts.next().unwrap_or("").to_string()))
            })
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        *self.rec.on_chunk.lock() = None;
        self.engine.stop();
        self.host.shutdown(Duration::from_millis(400));
    }
}

fn fake_a(version: &str, args: &[&str]) -> FakeAddon {
    let mut a = FakeAddon::new("fake-a", "Fake A");
    a.version = version.into();
    a.args = args.iter().map(|s| s.to_string()).collect();
    a
}

/// `n` sentences of 40-41 UTF-16 units; 40 make three chunks, the first being sentences 0-4 (204 units).
fn long_text(n: usize) -> String {
    (0..n).map(|i| format!("Sentence number {i} has a few words in it.")).collect::<Vec<_>>().join(" ")
}

/// [`long_text`]`(40)` with sentence 5, the second chunk's first, opening with a fake trigger. A line break goes
/// before it: after ". " a sentence that starts with "@" and a lowercase word is no new sentence (UAX #29 SB8), so
/// it would join sentence 4 and move the chunk boundary.
fn second_chunk_starting_with(trigger: &str) -> String {
    let text = long_text(40);
    assert!(text.contains(" Sentence number 5 "));
    text.replacen(" Sentence number 5 ", &format!("\n{trigger} number 5 "), 1)
}

fn is_complete(e: &Entry) -> bool {
    matches!(e, Entry::Event(EngineEvent::Complete { .. }))
}

fn is_failed(e: &Entry) -> bool {
    matches!(e, Entry::Event(EngineEvent::Failed { .. }))
}

fn is_done(e: &Entry) -> bool {
    matches!(e, Entry::Done(_))
}

fn is_stopped_with_error(e: &Entry) -> bool {
    matches!(e, Entry::Event(EngineEvent::Stopped { error: Some(_), .. }))
}

fn failed_error(rec: &Recorder) -> TtsError {
    rec.events()
        .into_iter()
        .find_map(|e| if let EngineEvent::Failed { error, .. } = e { Some(error) } else { None })
        .expect("a Failed event")
}

/// The chunk's text in the session text (ASCII here, so UTF-16 offsets are byte offsets).
fn chunk_text<'a>(text: &'a str, c: &TtsChunk) -> &'a str {
    &text[c.text_start as usize..c.text_end as usize]
}

/// A WAV the player can use, with an alignment that covers the chunk's text and its audio.
fn assert_usable_wav_chunk(text: &str, c: &TtsChunk) {
    assert_eq!(c.mime, MIME_WAV, "chunk {}", c.index);
    let wav = read_wav_info(&c.audio).unwrap_or_else(|| panic!("chunk {} is not a usable WAV", c.index));
    assert!(wav.duration_sec > 0.0, "chunk {}: empty audio", c.index);
    assert!((c.duration_sec - wav.duration_sec).abs() < 0.05, "chunk {}: duration {} vs WAV {}", c.index, c.duration_sec, wav.duration_sec);
    let a = c.alignment.as_ref().unwrap_or_else(|| panic!("chunk {} has no alignment", c.index));
    assert!(a.is_consistent(), "chunk {}", c.index);
    assert_eq!(a.chars.concat(), chunk_text(text, c), "chunk {}: alignment chars", c.index);
    assert!(a.starts.iter().zip(&a.ends).all(|(s, e)| s.is_finite() && e.is_finite() && s <= e), "chunk {}: start after end", c.index);
    assert!(a.starts.windows(2).all(|w| w[0] <= w[1]), "chunk {}: starts go backwards", c.index);
    assert!((a.last_end() - c.duration_sec).abs() < 0.05, "chunk {}: alignment ends at {} of {}", c.index, a.last_end(), c.duration_sec);
}

#[test]
fn a_three_chunk_read_on_an_addon_voice_delivers_wav_chunks_with_alignment_and_completes_billed_zero() {
    let rig = Rig::with_fake_a(&[]);
    let text = long_text(40);
    let session = rig.start(VOICE, &text);
    assert!(rig.rec.wait_for(|e| e.iter().any(is_complete)), "no Complete: {:?}", rig.rec.events());

    let chunks = rig.rec.chunks();
    assert_eq!(chunks.iter().map(|c| c.index).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!((chunks[0].text_start, chunks[0].text_end), (0, 204));
    assert_eq!(chunks[2].text_end as usize, text.len());
    for c in &chunks {
        assert_eq!(c.session_id, session);
        assert_usable_wav_chunk(&text, c);
    }
    let done = rig.rec.position(is_done).expect("done");
    assert!(done < rig.rec.position(is_complete).unwrap(), "done comes before Complete");
    assert!(matches!(&rig.rec.entries()[done], Entry::Done(d) if d.total_chunks == 3));
    let Some(EngineEvent::Complete { read_id, chunks: cached, characters_billed }) = rig.rec.events().pop() else { panic!("Complete last") };
    assert_eq!(read_id, "read-1");
    assert_eq!(characters_billed, 0.0);
    assert_eq!(cached.len(), 3);
    assert!(cached.iter().all(|c| c.mime == MIME_WAV));
    assert!(rig.rec.errors().is_empty(), "{:?}", rig.rec.errors());

    assert_eq!(spawns(&rig.scratch, COUNT), 1);
    assert_eq!(rig.texts().len(), 3);
    assert_eq!(rig.eleven.hits(), 0);
}

#[test]
fn a_crash_on_the_second_chunk_is_retried_on_a_fresh_helper_and_the_read_completes() {
    let rig = Rig::with_fake_a(&[]);
    let text = format!("@crash-after:1 {}", long_text(40));
    let started = Instant::now();
    rig.start(VOICE, &text);
    assert!(rig.rec.wait_for(|e| e.iter().any(is_complete)), "no Complete: {:?} {:?}", rig.rec.events(), rig.rec.errors());
    assert!(started.elapsed() >= Duration::from_millis(900), "the engine waits 1 s before the retry");

    let chunks = rig.rec.chunks();
    assert_eq!(chunks.iter().map(|c| c.index).collect::<Vec<_>>(), [0, 1, 2]);
    for c in &chunks {
        assert_usable_wav_chunk(&text, c);
    }
    assert!(rig.rec.errors().is_empty(), "a retried crash is not shown: {:?}", rig.rec.errors());
    let Some(EngineEvent::Complete { characters_billed, .. }) = rig.rec.events().pop() else { panic!("Complete last") };
    assert_eq!(characters_billed, 0.0);

    assert_eq!(spawns(&rig.scratch, COUNT), 2);
    let texts = rig.texts();
    assert_eq!(texts.len(), 4, "chunk 0, chunk 1 (crashed), chunk 1 again, chunk 2: {texts:?}");
    assert_eq!(texts[1].1, texts[2].1, "the retry sends the same chunk");
    assert_eq!(rig.host.status("fake-a"), RuntimeStatus::Ready);
    assert_eq!(rig.eleven.hits(), 0);
}

/// Each try crashes: tries 1 and 2 are retryable crashes (after which the engine waits 1 s, then 2 s), and the third
/// crash spends the budget of 3, so it comes back as Broken, not retryable, and the read fails with that copy.
#[test]
fn a_helper_that_crashes_on_every_try_of_the_first_chunk_fails_the_read_once_the_budget_is_spent() {
    let rig = Rig::with_fake_a(&[]);
    let started = Instant::now();
    let session = rig.start(VOICE, "@crash Hello there, this is the first chunk.");
    assert!(rig.rec.wait_for(|e| e.iter().any(is_failed)), "no Failed: {:?}", rig.rec.entries());
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(2900), "two retry waits (1 s + 2 s) come first: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");

    let keeps_stopping = "Fake A keeps stopping. Repair it on the Voices page.";
    let e = failed_error(&rig.rec);
    assert_eq!(e.code, ReadErrorCode::Unknown);
    assert_eq!(e.message, keeps_stopping);
    assert!(!e.retryable);
    assert_eq!(
        rig.rec.errors(),
        [TtsSessionError { session_id: session, message: keeps_stopping.into(), code: ReadErrorCode::Unknown, fatal: true }]
    );
    assert!(rig.rec.chunks().is_empty());
    assert_eq!(spawns(&rig.scratch, COUNT), 3);
    assert_eq!(rig.host.status("fake-a"), RuntimeStatus::Broken(BrokenReason::KeepsStopping));
    assert!(rig.health.lock().iter().any(|id| id == "fake-a"), "the core is told: {:?}", rig.health.lock());

    // Broken: the next read fails at once, without starting the helper again.
    let started = Instant::now();
    rig.start(VOICE, "Hello again.");
    assert!(rig.rec.wait_for(|e| e.iter().filter(|x| is_failed(x)).count() == 2));
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    assert_eq!(failed_error(&rig.rec).message, keeps_stopping);
    assert_eq!(spawns(&rig.scratch, COUNT), 3);
    assert_eq!(rig.eleven.hits(), 0);
}

#[test]
fn a_later_chunk_the_helper_refuses_cuts_the_read_short_without_a_retry() {
    let rig = Rig::with_fake_a(&[]);
    let text = second_chunk_starting_with("@err:internal");
    let session = rig.start(VOICE, &text);
    assert!(rig.rec.wait_for(|e| e.iter().any(is_done)), "no done: {:?}", rig.rec.entries());

    let chunks = rig.rec.chunks();
    assert_eq!(chunks.len(), 1, "only chunk 0 plays");
    assert_usable_wav_chunk(&text, &chunks[0]);
    assert!(text[chunks[0].text_end as usize..].trim_start().starts_with("@err:internal"), "the trigger opens chunk 1");

    let copy = "Fake A couldn’t read this text.";
    assert_eq!(
        rig.rec.errors(),
        [TtsSessionError { session_id: session, message: copy.into(), code: ReadErrorCode::Unknown, fatal: false }]
    );
    let stopped = rig.rec.events().into_iter().find_map(|e| match e {
        EngineEvent::Stopped { characters_billed, error: Some(e), .. } => Some((characters_billed, e)),
        _ => None,
    });
    let (billed, e) = stopped.expect("Stopped with the error");
    assert_eq!(billed, 0.0);
    assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unknown, copy, false));
    let (err, stop, done) = (
        rig.rec.position(|e| matches!(e, Entry::Error(_))).unwrap(),
        rig.rec.position(is_stopped_with_error).unwrap(),
        rig.rec.position(is_done).unwrap(),
    );
    assert!(err < stop && stop < done, "error, then Stopped, then done");
    assert!(matches!(&rig.rec.entries()[done], Entry::Done(d) if d.total_chunks == 1));
    assert!(!rig.rec.entries().iter().any(|e| is_complete(e) || is_failed(e)));

    let texts = rig.texts();
    assert_eq!(texts.len(), 2, "the refused chunk is asked for once: {texts:?}");
    assert!(texts[1].1.starts_with("@err:internal"));
    assert_eq!(spawns(&rig.scratch, COUNT), 1);
    assert_eq!(rig.eleven.hits(), 0);
}

#[test]
fn an_update_mid_read_sends_later_chunks_to_the_new_version_and_the_old_helper_exits() {
    let rig = Rig::with_fake_a(&["--tag", "v1"]);
    let v1_dir = rig.host.runtime("fake-a").unwrap().dir.clone();
    let v2 = Mutex::new(Some(fake_a("2.0.0", &["--tag", "v2"]).runtime(&rig.scratch)));
    let old: Arc<Mutex<Option<Arc<AddonRuntime>>>> = Arc::default();
    let (host, o) = (rig.host.clone(), old.clone());
    rig.rec.on_chunk(move |c| {
        if c.index == 1 {
            if let Some(next) = v2.lock().take() {
                *o.lock() = host.replace(next);
            }
        }
    });
    let text = long_text(40);
    rig.start(VOICE, &text);
    assert!(rig.rec.wait_for(|e| e.iter().any(is_complete)), "no Complete: {:?} {:?}", rig.rec.events(), rig.rec.errors());

    let chunks = rig.rec.chunks();
    assert_eq!(chunks.len(), 3);
    for c in &chunks {
        assert_usable_wav_chunk(&text, c);
    }
    assert!(rig.rec.errors().is_empty(), "{:?}", rig.rec.errors());
    let tags: Vec<String> = rig.texts().into_iter().map(|(tag, _)| tag).collect();
    assert_eq!(tags, ["v1", "v1", "v2"]);
    assert_eq!(rig.host.runtime("fake-a").unwrap().manifest.version.as_str(), "2.0.0");

    let pids = spawned_pids(&rig.scratch, COUNT);
    assert_eq!(pids.len(), 2, "{pids:?}");
    let old = old.lock().take().expect("replace returned the old version");
    assert_eq!(old.dir, v1_dir);
    assert!(is_alive(pids[0]), "the v1 helper lives as long as its runtime");
    drop(old);
    assert!(eventually(Duration::from_secs(3), || !is_alive(pids[0])), "the v1 helper still runs");
    assert!(is_alive(pids[1]), "the v2 helper keeps running");
    assert!(eventually(Duration::from_secs(3), || std::fs::remove_dir_all(&v1_dir).is_ok() || !v1_dir.exists()), "the v1 folder is deletable");
    assert_eq!(rig.eleven.hits(), 0);
}

#[test]
fn an_update_while_a_chunk_is_in_flight_lets_the_old_version_finish_it_and_the_next_chunk_comes_from_the_new_one() {
    let rig = Rig::with_fake_a(&["--tag", "v1", "--heartbeat-ms", "100"]);
    let v1_dir = rig.host.runtime("fake-a").unwrap().dir.clone();
    let text = second_chunk_starting_with("@slow:600");
    rig.start(VOICE, &text);
    assert!(
        eventually(WAIT, || rig.texts().iter().any(|(_, t)| t.starts_with("@slow"))),
        "chunk 1 never reached the helper: {:?}",
        rig.texts()
    );
    let old = rig.host.replace(fake_a("2.0.0", &["--tag", "v2"]).runtime(&rig.scratch)).expect("v1 was installed");
    drop(old);
    assert!(rig.rec.wait_for(|e| e.iter().any(is_complete)), "no Complete: {:?} {:?}", rig.rec.events(), rig.rec.errors());

    let chunks = rig.rec.chunks();
    assert_eq!(chunks.len(), 3);
    for c in &chunks {
        assert_usable_wav_chunk(&text, c);
    }
    assert!(rig.rec.errors().is_empty(), "{:?}", rig.rec.errors());
    let tags: Vec<String> = rig.texts().into_iter().map(|(tag, _)| tag).collect();
    assert_eq!(tags, ["v1", "v1", "v2"], "v1 finishes the chunk it was serving");
    let pids = spawned_pids(&rig.scratch, COUNT);
    assert_eq!(pids.len(), 2, "{pids:?}");
    assert!(eventually(Duration::from_secs(3), || !is_alive(pids[0])), "the v1 helper still runs");
    assert!(eventually(Duration::from_secs(3), || std::fs::remove_dir_all(&v1_dir).is_ok() || !v1_dir.exists()), "the v1 folder is deletable");
    assert_eq!(rig.eleven.hits(), 0);
}

#[test]
fn removing_the_addon_while_a_later_chunk_is_in_flight_cuts_the_read_short_with_was_removed() {
    let rig = Rig::with_fake_a(&["--heartbeat-ms", "100"]);
    let text = second_chunk_starting_with("@slow:5000");
    let session = rig.start(VOICE, &text);
    assert!(
        eventually(WAIT, || rig.texts().iter().any(|(_, t)| t.starts_with("@slow"))),
        "chunk 1 never reached the helper: {:?}",
        rig.texts()
    );
    let pid = spawned_pids(&rig.scratch, COUNT)[0];
    let removed_at = Instant::now();
    assert!(rig.host.remove("fake-a", Duration::from_millis(300)).is_some());
    assert!(rig.rec.wait_for(|e| e.iter().any(is_done)), "no done: {:?}", rig.rec.entries());
    assert!(removed_at.elapsed() < Duration::from_secs(2), "the in-flight chunk is not waited for: {:?}", removed_at.elapsed());

    assert_eq!(rig.rec.chunks().len(), 1);
    let copy = "Fake A was removed.";
    assert_eq!(
        rig.rec.errors(),
        [TtsSessionError { session_id: session, message: copy.into(), code: ReadErrorCode::Unknown, fatal: false }]
    );
    assert!(rig.rec.position(is_stopped_with_error).unwrap() < rig.rec.position(is_done).unwrap());
    assert!(!rig.rec.entries().iter().any(|e| is_complete(e) || is_failed(e)));
    assert!(eventually(Duration::from_secs(3), || !is_alive(pid)), "the helper still runs");
    assert_eq!(rig.host.status("fake-a"), RuntimeStatus::NotInstalled);

    // A new read on that voice fails before any audio, with the same copy.
    rig.start(VOICE, "Hello again.");
    assert!(rig.rec.wait_for(|e| e.iter().any(is_failed)));
    let e = failed_error(&rig.rec);
    assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unknown, copy, false));
    assert_eq!(spawns(&rig.scratch, COUNT), 1);
    assert_eq!(rig.eleven.hits(), 0);
}

#[test]
fn removing_the_addon_between_chunks_cuts_the_read_short_with_was_removed() {
    let rig = Rig::with_fake_a(&[]);
    let host = rig.host.clone();
    rig.rec.on_chunk(move |c| {
        if c.index == 0 {
            host.remove("fake-a", Duration::from_millis(300));
        }
    });
    let session = rig.start(VOICE, &long_text(40));
    assert!(rig.rec.wait_for(|e| e.iter().any(is_done)), "no done: {:?}", rig.rec.entries());

    assert_eq!(rig.rec.chunks().len(), 1);
    assert_eq!(
        rig.rec.errors(),
        [TtsSessionError { session_id: session, message: "Fake A was removed.".into(), code: ReadErrorCode::Unknown, fatal: false }]
    );
    assert!(rig.rec.position(is_stopped_with_error).is_some());
    let pid = spawned_pids(&rig.scratch, COUNT)[0];
    assert!(eventually(Duration::from_secs(3), || !is_alive(pid)), "the helper still runs");
    assert_eq!(rig.texts().len(), 1, "chunk 1 never reached a helper");
    assert_eq!(rig.eleven.hits(), 0);
}

#[test]
fn a_voice_of_an_addon_that_is_not_installed_fails_with_unknown_and_never_reaches_elevenlabs() {
    let rig = Rig::with_fake_a(&[]);
    let session = rig.start("addon:missing:alpha", "Hello there.");
    assert!(rig.rec.wait_for(|e| e.iter().any(is_failed)), "no Failed: {:?}", rig.rec.entries());

    let copy = "missing isn’t installed. Install it on the Voices page.";
    let e = failed_error(&rig.rec);
    assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unknown, copy, false));
    assert_eq!(rig.rec.errors(), [TtsSessionError { session_id: session, message: copy.into(), code: ReadErrorCode::Unknown, fatal: true }]);
    assert!(rig.rec.chunks().is_empty());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(rig.eleven.hits(), 0);
    assert_eq!(spawns(&rig.scratch, COUNT), 0, "no add-on helper starts either");
}

/// The control for the zero-hit checks: an ElevenLabs voice in the same rig does reach the stand-in.
#[test]
fn an_elevenlabs_voice_in_the_same_engine_reaches_the_stand_in() {
    let rig = Rig::with_fake_a(&[]);
    rig.start("voice-1", "Hello there.");
    assert!(rig.rec.wait_for(|e| e.iter().any(is_failed)), "no Failed: {:?}", rig.rec.entries());
    assert!(rig.eleven.hits() >= 1);
    assert_eq!(spawns(&rig.scratch, COUNT), 0);
}
