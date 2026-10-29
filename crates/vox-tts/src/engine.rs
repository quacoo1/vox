//! `TtsEngine` (tts.md §4): turns cleaned text into an ordered stream of synthesized chunks for the player, at most
//! one live session.
//!
//! Contract (cross-crate; the core relies on every point; tts.md §4 with the Product-ruling changes D-NET-2/D-NET-4
//! and the threading rules of ARCHITECTURE.md §2.4 / §4.14):
//! - **One lock.** One `Mutex<EngineState>` guards the current session and, per session, `finished`, `billed`
//!   (an `f64` sum), `delivered_sec` and `position_sec`. The worker holds it for every "re-check live → update
//!   `finished`/`billed` → sink call(s) → event emission" sequence; [`TtsEngine::stop`], [`TtsEngine::start`] and
//!   [`TtsEngine::play_cached`] take the same lock to swap the session and cancel its token. This is safe because the
//!   sink and the event listener only enqueue (vox-app). Consequence: once `stop()`/`start()` returned, no sink call
//!   and no `Complete`/`Failed`/`Stopped{error}` for the old session can follow; the **only** event that may still be
//!   emitted for a session that is no longer live is the D-NET-4 trailing `Stopped` (below). A stale `Failed` (an
//!   error toast while the user's next read starts) is therefore impossible by construction, not by convention.
//! - [`TtsEngine::start`] stops the previous session (see `stop`), chunks the text (`vox_text::chunk_text_default`),
//!   computes `sentences` (`vox_text::sentence_starts`), calls `sink.session_start` **synchronously**, then spawns the
//!   session worker and returns the session id. Nothing else is delivered for the new session before `start` returns.
//! - Synthesis is sequential (one request in flight per session, one worker thread per session); chunks 0 and 1 are
//!   requested at once; chunk `i ≥ 2` waits until `delivered_sec − position_sec < 120` ([`TtsEngine::progress`]
//!   wakes it). Retries: only `TtsError::retryable` errors (see `crate::error`: 429/5xx/unsent — never timeouts),
//!   delays 1/2/4 s, lengthened to `retry-after` when the server sends one (capped at 10 s: `retry-after: 3` waits
//!   3 s, `30` waits 10 s), max 4 attempts per chunk, liveness re-checked before every sleep and every request.
//! - Chunk 0 fails → `sink.error(fatal)` + `EngineEvent::Failed` (under the lock, in that order).
//! - **Chunk i > 0 fails (cut short)** → under the session lock, in this order: `finished = true` →
//!   `sink.error(non-fatal)` → `events(EngineEvent::Stopped { read_id, characters_billed, error: Some(e) })` →
//!   `sink.done(delivered)`. Why this order (ARCHITECTURE.md §4.14, D-NET-5): the core channel is FIFO and the player
//!   can report `ended` only after it has processed `done`, so `Stopped` (which makes the core mark the session
//!   `partial`) is always queued on the core channel before any report the player produces after `done`; the player
//!   still sees the warning before `done`. (Electron's order done → stopped was safe only because `stopped` ran
//!   synchronously in main before any widget IPC.) The same sequence ends a session whose key was cleared mid-read
//!   (non-fatal `no-api-key`).
//! - All delivered → `finished = true` → `sink.done` → `EngineEvent::Complete` (never for partial audio; a panicking
//!   sink does not prevent `Complete`).
//! - **Billing** (BRIEF.md "Count billed characters for chunks that were requested and returned audio, including ones
//!   that arrive after a stop"): a chunk is billed when its request returned audio — `characterCost` header (may be
//!   fractional), else the chunk's UTF-16 length — whether or not the session is still live. Billing is accumulated
//!   as an `f64` per session and emitted unrounded; the reader writes `max(0, js_round(billed))` (two chunks billed
//!   1.5 and 1.5 → 3).
//! - **Stop / supersede** ([`TtsEngine::stop`], and the implicit stop inside `start`/`play_cached`): under the lock,
//!   cancel the token and forget the session (`current_*` become `None`); then, still on the **caller's thread**,
//!   if the session had not finished and `billed > 0`, emit `EngineEvent::Stopped { read_id, characters_billed:
//!   billed_so_far, error: None }` synchronously (only an enqueue; tts.md §4.7 "emit stopped(readId, billed)"). So
//!   the supersede ordering of tts.md §4.4 holds (the old session's `Stopped` precedes every chunk/event of the new
//!   one), and a quit (`engine.stop()`, then the core's shutdown drain of queued `Engine` commands) persists the
//!   billing known at stop time. Cancelling ends retry sleeps and look-ahead waits at once and prevents new requests,
//!   but an **in-flight request is allowed to finish** (blocking ureq; bounded by its timeout). If it returns audio
//!   after the stop, the worker bills it (not delivered) and emits **one more** `Stopped { error: None }` carrying the
//!   **cumulative** total for that read (D-NET-4); the reader writes `charactersBilled` as an absolute value, so the
//!   second event only raises it. That trailing event is emitted from the worker thread, possibly after events of a
//!   newer session; consumers key it by `read_id` (`Sessions::latest_for`), never by "current session". A stop
//!   before anything returned audio emits nothing.
//! - Mock mode (`VOX_MOCK_TTS`/demo): 350 ms / 120 ms latency (cancellable), `mock::mock_synthesize`, no network, no
//!   retries; billing = UTF-16 length.
//! - [`TtsEngine::play_cached`]: `session_start(from_cache)` synchronously, with `sentences =
//!   vox_text::sentence_starts(text)` over the (possibly rebuilt, preview + `…`) session text and
//!   `estimated_duration_sec = Σ chunk durations`; then — on the worker, after re-checking liveness once — every
//!   chunk (renumbered 0..n−1, sorted by stored index, stable) and `done`, without yielding between them; zero chunks
//!   → fatal `not-cached` + `Failed`. No `Complete`, no billing, no look-ahead.
//! - Sink and event callbacks must not block or call back into the engine (vox-app's implementations just enqueue).
//!   Panics inside them are caught and logged (`[tts] sink failed` / `[tts] event listener failed`); synthesis and
//!   billing continue. An unexpected panic in the worker → if live and unfinished: before any chunk was delivered,
//!   fatal `ENGINE_CRASHED` + `Failed` (tts.md §4); after one or more chunks were delivered, the cut-short sequence
//!   above with a non-fatal `ENGINE_CRASHED` (so `Stopped` carries the billed characters, which `Failed` cannot, and
//!   the delivered audio keeps playing); always logged `session crashed`.
//! - Logging: scope `tts`; never the text or the key; failures as `read failed: <code> (HTTP <status>)`.
//!
//! Implementation notes: the engine lock is a `parking_lot::ReentrantMutex<RefCell<EngineState>>` — the same single
//! lock, made re-entrant so that a callback that does call back into the engine (against the rule above) re-enters
//! instead of deadlocking; no `RefCell` borrow is ever held across a sink or listener call. The look-ahead wait uses a
//! per-session wake counter (bumped by `progress()` and by the stop) read under the engine lock, so a wake-up between
//! the check and the wait is never lost. The `Stopped` emitted by a stop is emitted while the lock is still held, so a
//! worker's trailing cumulative `Stopped` can never overtake it.
//!
//! Required tests (ARCHITECTURE.md §9, against the `tiny_http` mock server): `Stopped{error}` is observed before the
//! sink's `done`; start/stop in a tight loop never yields `Failed`/`Complete` for a `read_id` after its `stop()`
//! returned; stop with billed > 0 emits `Stopped` before `stop()`/`start()` returns; audio arriving after the stop
//! yields a second `Stopped` with the cumulative total; key cleared mid-read → non-fatal `no-api-key`, then
//! `Stopped{error}`, then `done`; worker panic → fatal `ENGINE_CRASHED` + `Failed`; sink panic → `Complete` still
//! emitted; `play_cached` with zero chunks → fatal `not-cached` + `Failed`; billing 1.5 + 1.5 → 3.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex, ReentrantMutex, ReentrantMutexGuard, RwLock};
use vox_core::constants::{
    ESTIMATED_CHARS_PER_SECOND, LOOKAHEAD_FREE_CHUNKS, LOOKAHEAD_SEC, MAX_RETRY_AFTER_MS, MOCK_LATENCY_FIRST_MS,
    MOCK_LATENCY_NEXT_MS, RETRY_DELAYS_MS,
};
use vox_core::settings::Settings;
use vox_core::text16::utf16_len;
use vox_core::tts::{Alignment, CachedChunk, TextChunk, TtsChunk, TtsSessionDone, TtsSessionError, TtsSessionStart};

use crate::client::{ElevenLabsClient, SynthesisRequest, SynthesisResult};
use crate::duration::chunk_duration;
use crate::error::TtsError;
use crate::mock::{mock_synthesize, positive_speed};
use crate::providers::SpeechProvider;
use std::collections::HashMap;
use vox_core::providers::{provider_of, split_voice_id, Provider};

/// Delivery to the player (vox-app forwards to `vox_audio::Player`).
pub trait TtsSink: Send + Sync {
    fn session_start(&self, start: TtsSessionStart);
    fn chunk(&self, chunk: TtsChunk);
    fn done(&self, done: TtsSessionDone);
    fn error(&self, error: TtsSessionError);
}

/// Events for the read controller.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// Every chunk of a fresh read was synthesized (safe to cache). `characters_billed` is the unrounded `f64` sum
    /// (the reader writes `max(0, js_round(billed))`).
    Complete { read_id: String, chunks: Vec<CachedChunk>, characters_billed: f64 },
    /// The session failed before any audio.
    Failed { read_id: String, error: TtsError },
    /// Ended before the whole text after some chunks were billed. `error: Some` = a later chunk failed for good
    /// (emitted before `sink.done`, module docs); `error: None` = stopped/superseded (emitted synchronously by
    /// `stop()`/`start()`/`play_cached()`, plus at most one trailing cumulative event from the worker, D-NET-4).
    /// `characters_billed` is the cumulative unrounded total for the read (absolute, not a delta).
    Stopped { read_id: String, characters_billed: f64, error: Option<TtsError> },
}

/// Event listener (vox-app enqueues `Command::Engine`).
pub type EngineEvents = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// `StartSessionOptions`.
#[derive(Debug, Clone)]
pub struct StartSessionOptions {
    pub read_id: String,
    /// Already cleaned and truncated.
    pub text: String,
    pub title: String,
    pub voice_id: String,
    pub voice_name: String,
    pub source_app: Option<String>,
    /// Current settings: model, format, voice settings, language, rate, volume, highlightWords.
    pub settings: Settings,
}

/// `PlayCachedOptions`.
#[derive(Debug, Clone)]
pub struct PlayCachedOptions {
    pub read_id: String,
    /// Session text (preview + `…` placeholders for preview-only records).
    pub text: String,
    pub title: String,
    pub voice_id: String,
    pub voice_name: String,
    pub source_app: Option<String>,
    pub settings: Settings,
    pub chunks: Vec<CachedChunk>,
}

// ───────────────────────────── internals ─────────────────────────────

/// A counter the look-ahead wait sleeps on; bumped by `progress()` and by the stop.
#[derive(Default)]
struct Waker {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl Waker {
    fn generation(&self) -> u64 {
        *self.generation.lock()
    }
    fn bump(&self) {
        let mut g = self.generation.lock();
        *g = g.wrapping_add(1);
        self.changed.notify_all();
    }
    /// Blocks until the counter moves past `seen`.
    fn wait_past(&self, seen: u64) {
        let mut g = self.generation.lock();
        while *g == seen {
            self.changed.wait(&mut g);
        }
    }
}

/// The immutable identity of one session.
struct Session {
    id: String,
    read_id: String,
    cancel: crate::CancelToken,
    waker: Waker,
}

/// The current session's mutable state (guarded by the engine lock).
struct Current {
    session: Arc<Session>,
    finished: bool,
    billed: f64,
    delivered_sec: f64,
    /// Fresh-synthesis chunks handed to the sink (a crash after the first one ends the session as cut short).
    delivered_chunks: u32,
    position_sec: f64,
}

#[derive(Default)]
struct EngineState {
    current: Option<Current>,
}

type Guard<'a> = ReentrantMutexGuard<'a, RefCell<EngineState>>;

/// What a fresh-synthesis worker needs besides the chunks.
struct FreshJob {
    chunks: Vec<TextChunk>,
    voice_id: String,
    settings: Settings,
}

struct EngineInner {
    client: ElevenLabsClient,
    sink: Arc<dyn TtsSink>,
    events: EngineEvents,
    mock: bool,
    /// Multiplies retry and mock-latency delays (1 in production; tests shorten them).
    delay_scale: f64,
    state: ReentrantMutex<RefCell<EngineState>>,
    /// Connected providers besides ElevenLabs (voices `<prefix>:<rest>` route here).
    providers: RwLock<HashMap<Provider, Arc<dyn SpeechProvider>>>,
}

/// `positive()`-style speed, then the estimate `len16 / 15 / speed`.
fn estimate_sec(text: &str, speed: f64) -> f64 {
    utf16_len(text) as f64 / ESTIMATED_CHARS_PER_SECOND / positive_speed(speed)
}

/// The wait before retry `attempt + 1` (0-based `attempt` of the failed try), in ms: the backoff step, lengthened to
/// the server's `retry-after` hint capped at 10 s.
fn retry_delay_ms(attempt: usize, retry_after_sec: Option<f64>) -> f64 {
    let step = RETRY_DELAYS_MS.get(attempt).copied().unwrap_or(0) as f64;
    let hinted = (retry_after_sec.unwrap_or(0.0) * 1000.0).min(MAX_RETRY_AFTER_MS as f64);
    step.max(hinted)
}

fn failure_line(e: &TtsError) -> String {
    match e.status {
        Some(status) => format!("read failed: {} (HTTP {status})", e.code.as_str()),
        None => format!("read failed: {}", e.code.as_str()),
    }
}

impl EngineInner {
    fn lock(&self) -> Guard<'_> {
        self.state.lock()
    }

    fn is_live(&self, g: &Guard<'_>, s: &Arc<Session>) -> bool {
        !s.cancel.is_cancelled() && g.borrow().current.as_ref().is_some_and(|c| Arc::ptr_eq(&c.session, s))
    }

    fn is_live_now(&self, s: &Arc<Session>) -> bool {
        let g = self.lock();
        self.is_live(&g, s)
    }

    /// Runs `f` on the current state if `s` is still the current session.
    fn with_current<R>(&self, g: &Guard<'_>, s: &Arc<Session>, f: impl FnOnce(&mut Current) -> R) -> Option<R> {
        let mut st = g.borrow_mut();
        st.current.as_mut().filter(|c| Arc::ptr_eq(&c.session, s)).map(f)
    }

    fn to_sink(&self, what: &str, call: impl FnOnce(&dyn TtsSink)) {
        if catch_unwind(AssertUnwindSafe(|| call(&*self.sink))).is_err() {
            tracing::error!(target: "tts", "sink failed ({what})");
        }
    }

    fn emit(&self, event: EngineEvent) {
        if catch_unwind(AssertUnwindSafe(|| (self.events)(event))).is_err() {
            tracing::error!(target: "tts", "event listener failed");
        }
    }

    fn scaled(&self, ms: f64) -> Duration {
        Duration::from_secs_f64((ms * self.delay_scale).max(0.0) / 1000.0)
    }

    /// `stop()` under the lock: forget + cancel the current session, then report its billing if it had not finished.
    fn stop_locked(&self, g: &Guard<'_>) {
        let previous = g.borrow_mut().current.take();
        let Some(cur) = previous else { return };
        cur.session.cancel.cancel();
        cur.session.waker.bump();
        if !cur.finished && cur.billed > 0.0 {
            self.emit(EngineEvent::Stopped { read_id: cur.session.read_id.clone(), characters_billed: cur.billed, error: None });
        }
    }

    /// `begin(readId)`: stop the current session and install a new one.
    fn begin(&self, g: &Guard<'_>, read_id: &str) -> Arc<Session> {
        self.stop_locked(g);
        let session = Arc::new(Session {
            id: vox_core::ids::new_uuid(),
            read_id: read_id.to_string(),
            cancel: crate::CancelToken::new(),
            waker: Waker::default(),
        });
        g.borrow_mut().current = Some(Current {
            session: session.clone(),
            finished: false,
            billed: 0.0,
            delivered_sec: 0.0,
            delivered_chunks: 0,
            position_sec: 0.0,
        });
        session
    }

    /// `fail(s, e)`: finished → `sink.error(fatal)` → `Failed`.
    fn fail_locked(&self, g: &Guard<'_>, s: &Arc<Session>, e: TtsError) {
        self.with_current(g, s, |c| c.finished = true);
        tracing::warn!(target: "tts", "{}", failure_line(&e));
        let error = TtsSessionError { session_id: s.id.clone(), message: e.message.clone(), code: e.code, fatal: true };
        self.to_sink("error", |k| k.error(error));
        self.emit(EngineEvent::Failed { read_id: s.read_id.clone(), error: e });
    }

    /// Cut short (D-NET-5): finished → `sink.error(non-fatal)` → `Stopped{error}` → `sink.done(delivered)`.
    fn cut_short_locked(&self, g: &Guard<'_>, s: &Arc<Session>, e: TtsError, billed: f64, delivered: u32) {
        self.with_current(g, s, |c| c.finished = true);
        let error = TtsSessionError { session_id: s.id.clone(), message: e.message.clone(), code: e.code, fatal: false };
        self.to_sink("error", |k| k.error(error));
        self.emit(EngineEvent::Stopped { read_id: s.read_id.clone(), characters_billed: billed, error: Some(e) });
        let done = TtsSessionDone { session_id: s.id.clone(), total_chunks: delivered };
        self.to_sink("done", |k| k.done(done));
    }

    /// A panic escaped a worker: always logged. If the session is still live and unfinished it ends visibly: with
    /// nothing delivered yet → fatal `ENGINE_CRASHED` + `Failed` (tts.md §4); after some chunks were delivered → cut
    /// short like a later-chunk failure (non-fatal `ENGINE_CRASHED`, `Stopped{billed, error}`, `done(delivered)`), so
    /// the billed characters reach the reader (BRIEF billing ruling) and the delivered audio keeps playing.
    fn crashed(&self, s: &Arc<Session>) {
        tracing::error!(target: "tts", "session crashed");
        let g = self.lock();
        if !self.is_live(&g, s) {
            return;
        }
        let progress = self.with_current(&g, s, |c| (!c.finished).then_some((c.billed, c.delivered_chunks))).flatten();
        match progress {
            Some((_, 0)) => self.fail_locked(&g, s, TtsError::crashed()),
            Some((billed, delivered)) => {
                let e = TtsError::crashed();
                tracing::warn!(target: "tts", "{} (after {delivered} chunks)", failure_line(&e));
                self.cut_short_locked(&g, s, e, billed, delivered);
            }
            None => {}
        }
    }

    /// Spawns a session worker; a failed spawn fails the session at once (still under the caller's lock).
    fn spawn_worker(self: &Arc<Self>, g: &Guard<'_>, s: &Arc<Session>, work: impl FnOnce(&Arc<Self>) + Send + 'static) {
        let inner = self.clone();
        let session = s.clone();
        let spawned = std::thread::Builder::new().name("vox-tts-session".into()).spawn(move || {
            if catch_unwind(AssertUnwindSafe(|| work(&inner))).is_err() {
                inner.crashed(&session);
            }
        });
        if let Err(e) = spawned {
            tracing::error!(target: "tts", "couldn't start the session worker: {e}");
            self.fail_locked(g, s, TtsError::crashed());
        }
    }

    /// `waitForListener`: true once less than 120 s of delivered audio is unplayed; false when the session ended.
    fn wait_for_listener(&self, s: &Arc<Session>) -> bool {
        loop {
            let seen = {
                let g = self.lock();
                if !self.is_live(&g, s) {
                    return false;
                }
                let st = g.borrow();
                let Some(cur) = st.current.as_ref() else { return false };
                if cur.delivered_sec - cur.position_sec < LOOKAHEAD_SEC {
                    return true;
                }
                s.waker.generation()
            };
            s.waker.wait_past(seen);
        }
    }

    /// Per-chunk synthesis with the engine's retry policy (tts.md §4.5, D-NET-2), or the mock.
    fn synthesize_chunk(&self, s: &Arc<Session>, job: &FreshJob, i: usize) -> Result<SynthesisResult, TtsError> {
        let chunk = &job.chunks[i];
        if self.mock {
            let ms = if i == 0 { MOCK_LATENCY_FIRST_MS } else { MOCK_LATENCY_NEXT_MS };
            if !s.cancel.sleep(self.scaled(ms as f64)) {
                return Err(TtsError::cancelled());
            }
            return Ok(mock_synthesize(&chunk.text, job.settings.voice.speed));
        }
        let provider = provider_of(&job.voice_id);
        if provider != Provider::ElevenLabs {
            let client = self.providers.read().get(&provider).cloned();
            let (Some(client), Some((_, rest))) = (client, split_voice_id(&job.voice_id)) else {
                return Err(crate::providers::not_connected(provider));
            };
            return self.with_retries(s, i, || client.synthesize(&chunk.text, rest, job.settings.widget.highlight_words, &s.cancel));
        }
        let req = SynthesisRequest {
            text: chunk.text.clone(),
            voice_id: job.voice_id.clone(),
            model_id: job.settings.model_id.clone(),
            output_format: job.settings.output_format.as_str().to_string(),
            voice_settings: job.settings.voice.clone(),
            previous_text: vox_text::context_before(i.checked_sub(1).map(|p| job.chunks[p].text.as_str())),
            next_text: vox_text::context_after(job.chunks.get(i + 1).map(|c| c.text.as_str())),
            language_code: Some(job.settings.language_code.clone()).filter(|c| !c.is_empty()),
        };
        self.with_retries(s, i, || self.client.synthesize(&req, &s.cancel))
    }

    /// Runs `call` with the engine's retry policy (retryable errors only, backoff, liveness re-checks).
    fn with_retries(&self, s: &Arc<Session>, i: usize, call: impl Fn() -> Result<SynthesisResult, TtsError>) -> Result<SynthesisResult, TtsError> {
        let mut attempt = 0usize;
        loop {
            match call() {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if e.aborted || !e.retryable || attempt >= RETRY_DELAYS_MS.len() || !self.is_live_now(s) {
                        return Err(e);
                    }
                    let wait_ms = retry_delay_ms(attempt, e.retry_after_sec);
                    tracing::info!(
                        target: "tts",
                        "chunk {i} failed ({}{}); retry {} in {wait_ms} ms",
                        e.code.as_str(),
                        e.status.map(|st| format!(", HTTP {st}")).unwrap_or_default(),
                        attempt + 1
                    );
                    if !s.cancel.sleep(self.scaled(wait_ms)) {
                        return Err(TtsError::cancelled());
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// `run(s, chunks, opts)`: the fresh-synthesis worker.
    fn run_fresh(&self, s: &Arc<Session>, job: FreshJob) {
        // The first lock waits until `start` released it: nothing reaches the sink before `start` returned.
        {
            let g = self.lock();
            if !self.is_live(&g, s) {
                return;
            }
            if job.chunks.is_empty() {
                self.fail_locked(&g, s, TtsError::empty_text());
                return;
            }
        }
        let total = job.chunks.len();
        let mut delivered: Vec<CachedChunk> = Vec::with_capacity(total);
        let mut billed = 0.0f64;
        for i in 0..total {
            if i >= LOOKAHEAD_FREE_CHUNKS && !self.wait_for_listener(s) {
                return;
            }
            let outcome = self.synthesize_chunk(s, &job, i);
            let g = self.lock();
            let result = match outcome {
                Ok(result) => result,
                Err(e) => {
                    if !self.is_live(&g, s) {
                        return; // stopped / superseded: silent
                    }
                    if i == 0 {
                        self.fail_locked(&g, s, e);
                        return;
                    }
                    tracing::warn!(target: "tts", "{} (chunk {i} of {total})", failure_line(&e));
                    self.cut_short_locked(&g, s, e, billed, delivered.len() as u32);
                    return;
                }
            };
            let chunk = &job.chunks[i];
            let len16 = utf16_len(&chunk.text);
            // Billed whenever the request returned audio, live or not (D-NET-4).
            billed += result.character_cost.unwrap_or(len16 as f64);
            if !self.is_live(&g, s) {
                // Audio that arrived after a stop: billed, never delivered; one more cumulative `Stopped`.
                self.emit(EngineEvent::Stopped { read_id: s.read_id.clone(), characters_billed: billed, error: None });
                return;
            }
            // Recorded at once, so a stop or a crash from here on reports it.
            self.with_current(&g, s, |c| c.billed = billed);
            let duration_sec = chunk_duration(&result, len16, &job.settings);
            self.with_current(&g, s, |c| c.delivered_sec += duration_sec);
            let SynthesisResult { audio, mime, alignment, .. } = result;
            let shared_alignment = alignment.map(Arc::new);
            let tts_chunk = TtsChunk {
                session_id: s.id.clone(),
                index: i as u32,
                text_start: chunk.start,
                text_end: chunk.end,
                audio: audio.clone(),
                mime: mime.clone(),
                alignment: shared_alignment.clone(),
                duration_sec,
            };
            self.to_sink("chunk", |k| k.chunk(tts_chunk));
            self.with_current(&g, s, |c| c.delivered_chunks += 1);
            delivered.push(CachedChunk {
                index: i as u32,
                text_start: chunk.start,
                text_end: chunk.end,
                mime,
                alignment: shared_alignment.map(|a| Alignment::clone(&a)),
                duration_sec,
                audio,
            });
            if i + 1 == total {
                // A callback that stopped the engine re-entrantly already reported this session's billing.
                if !self.is_live(&g, s) {
                    return;
                }
                self.with_current(&g, s, |c| c.finished = true);
                let done = TtsSessionDone { session_id: s.id.clone(), total_chunks: total as u32 };
                self.to_sink("done", |k| k.done(done));
                let chunks = std::mem::take(&mut delivered);
                self.emit(EngineEvent::Complete { read_id: s.read_id.clone(), chunks, characters_billed: billed });
                return;
            }
        }
    }

    /// `replay(s, chunks)`: the cached-playback worker.
    fn run_cached(&self, s: &Arc<Session>, chunks: Vec<CachedChunk>) {
        let g = self.lock();
        if !self.is_live(&g, s) {
            return;
        }
        if chunks.is_empty() {
            self.fail_locked(&g, s, TtsError::cached_audio_missing());
            return;
        }
        let total = chunks.len();
        for (index, c) in chunks.into_iter().enumerate() {
            let tts_chunk = TtsChunk {
                session_id: s.id.clone(),
                index: index as u32, // renumbered so the player always sees 0..n−1
                text_start: c.text_start,
                text_end: c.text_end,
                audio: c.audio,
                mime: c.mime,
                alignment: c.alignment.map(Arc::new),
                duration_sec: c.duration_sec,
            };
            self.to_sink("chunk", |k| k.chunk(tts_chunk));
        }
        self.with_current(&g, s, |c| c.finished = true);
        let done = TtsSessionDone { session_id: s.id.clone(), total_chunks: total as u32 };
        self.to_sink("done", |k| k.done(done));
    }
}

/// The engine handle (one per app).
pub struct TtsEngine {
    inner: Arc<EngineInner>,
}

impl TtsEngine {
    pub fn new(client: ElevenLabsClient, sink: Arc<dyn TtsSink>, events: EngineEvents, mock: bool) -> Self {
        Self::with_delay_scale(client, sink, events, mock, 1.0)
    }

    /// [`TtsEngine::new`] with retry and mock-latency delays multiplied by `scale` (tests).
    fn with_delay_scale(client: ElevenLabsClient, sink: Arc<dyn TtsSink>, events: EngineEvents, mock: bool, scale: f64) -> Self {
        Self {
            inner: Arc::new(EngineInner {
                client,
                sink,
                events,
                mock,
                delay_scale: scale,
                state: ReentrantMutex::new(RefCell::new(EngineState::default())),
                providers: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Starts a fresh synthesis session; returns its id.
    pub fn start(&self, opts: StartSessionOptions) -> String {
        let StartSessionOptions { read_id, text, title, voice_id, voice_name, source_app, settings } = opts;
        // Pure work first (outside the lock).
        let chunks = vox_text::chunk_text_default(&text);
        let sentences: Arc<[u32]> = Arc::from(vox_text::sentence_starts(&text));
        let estimated_duration_sec = estimate_sec(&text, settings.voice.speed);
        let inner = &self.inner;
        let g = inner.lock();
        let session = inner.begin(&g, &read_id);
        let start = TtsSessionStart {
            session_id: session.id.clone(),
            read_id,
            text: Arc::from(text),
            title,
            voice_name,
            voice_id: voice_id.clone(),
            source_app: source_app.filter(|a| !a.is_empty()),
            estimated_duration_sec,
            chunk_count: chunks.len() as u32,
            rate: settings.playback_rate,
            volume: settings.volume,
            highlight_words: settings.widget.highlight_words,
            from_cache: false,
            sentences,
        };
        inner.to_sink("session_start", |k| k.session_start(start));
        let job = FreshJob { chunks, voice_id, settings };
        let s = session.clone();
        inner.spawn_worker(&g, &session, move |inner| inner.run_fresh(&s, job));
        drop(g);
        session.id.clone()
    }

    /// Plays cached chunks as a session; returns its id.
    pub fn play_cached(&self, opts: PlayCachedOptions) -> String {
        let PlayCachedOptions { read_id, text, title, voice_id, voice_name, source_app, settings, mut chunks } = opts;
        chunks.sort_by_key(|c| c.index); // stable: equal indices keep their order
        let sentences: Arc<[u32]> = Arc::from(vox_text::sentence_starts(&text));
        let estimated_duration_sec = chunks.iter().map(|c| c.duration_sec).sum();
        let inner = &self.inner;
        let g = inner.lock();
        let session = inner.begin(&g, &read_id);
        let start = TtsSessionStart {
            session_id: session.id.clone(),
            read_id,
            text: Arc::from(text),
            title,
            voice_name,
            voice_id,
            source_app: source_app.filter(|a| !a.is_empty()),
            estimated_duration_sec,
            chunk_count: chunks.len() as u32,
            rate: settings.playback_rate,
            volume: settings.volume,
            highlight_words: settings.widget.highlight_words,
            from_cache: true,
            sentences,
        };
        inner.to_sink("session_start", |k| k.session_start(start));
        let s = session.clone();
        inner.spawn_worker(&g, &session, move |inner| inner.run_cached(&s, chunks));
        drop(g);
        session.id.clone()
    }

    /// The player's playhead (audio seconds) for look-ahead; ignored for other sessions or non-finite values.
    pub fn progress(&self, session_id: &str, position_sec: f64) {
        if !position_sec.is_finite() {
            return;
        }
        let g = self.inner.lock();
        let mut st = g.borrow_mut();
        if let Some(cur) = st.current.as_mut().filter(|c| c.session.id == session_id) {
            cur.position_sec = position_sec.max(0.0); // may move backwards on a seek back
            cur.session.waker.bump();
        }
    }

    /// Stops the current session (idempotent): under the engine lock cancels its token and forgets it (`current_*`
    /// become `None`); then emits `Stopped { billed_so_far, error: None }` synchronously on the caller's thread when
    /// the session had not finished and billed > 0. Returns at once (never waits for the worker); the worker may emit
    /// one trailing cumulative `Stopped` if an in-flight request still returns audio (module docs, D-NET-4).
    pub fn stop(&self) {
        let g = self.inner.lock();
        self.inner.stop_locked(&g);
    }

    /// The latest session id, kept after synthesis finishes (audio may still play) until `stop()`.
    pub fn current_session_id(&self) -> Option<String> {
        let g = self.inner.lock();
        let st = g.borrow();
        st.current.as_ref().map(|c| c.session.id.clone())
    }

    pub fn current_read_id(&self) -> Option<String> {
        let g = self.inner.lock();
        let st = g.borrow();
        st.current.as_ref().map(|c| c.session.read_id.clone())
    }

    /// Connects (or with `None` disconnects) a provider for voices with its prefix.
    pub fn set_provider(&self, provider: Provider, client: Option<Arc<dyn SpeechProvider>>) {
        let mut map = self.inner.providers.write();
        match client {
            Some(c) => {
                map.insert(provider, c);
            }
            None => {
                map.remove(&provider);
            }
        }
    }

    /// The connected client for `provider`, if any.
    pub fn provider(&self, provider: Provider) -> Option<Arc<dyn SpeechProvider>> {
        self.inner.providers.read().get(&provider).cloned()
    }

    /// Every connected provider, in `Provider::EXTRA` order.
    pub fn providers(&self) -> Vec<Arc<dyn SpeechProvider>> {
        let map = self.inner.providers.read();
        Provider::EXTRA.iter().filter_map(|p| map.get(p).cloned()).collect()
    }

    pub fn is_mock(&self) -> bool {
        self.inner.mock
    }
}

impl Drop for TtsEngine {
    /// Dropping the engine ends the current session (its worker winds down on its own).
    fn drop(&mut self) {
        let g = self.inner.lock();
        let st = g.borrow();
        if let Some(cur) = st.current.as_ref() {
            cur.session.cancel.cancel();
            cur.session.waker.bump();
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
