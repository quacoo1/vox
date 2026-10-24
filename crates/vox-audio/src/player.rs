//! The session player (reader-playback.md §4; BRIEF.md Product rulings "Audio"; ARCHITECTURE.md §5). Cross-crate
//! contract.
//!
//! A [`Player`] owns a player thread (state machine, decoding, stretching, resampling, feeding the output) and,
//! lazily, the WASAPI stream (whose callback only copies queued blocks, applies volume and counts rendered frames —
//! no locks, no allocation). Handles are cheap clones; every method only enqueues a message, except
//! [`Player::snapshot`], which reads a shared, lock-light state and computes position/duration/char index for "now".
//!
//! Semantics reproduced exactly (reader-playback.md): `startSession` (duplicate id ignored; the previous session is
//! stopped and reported `idle` with its final `listenedSec`; rate/volume from the payload; `prepareOutput`;
//! `loading`), `addChunk` (contiguous order, duplicates ignored, decode ahead), `finishSession`, `failSession`
//! (fatal → `error`, non-fatal → `warning`), the status machine (§4.5), transport commands (§4.6: `play` on `ended`
//! = seek 0; `skip` sentence uses `TtsSessionStart::sentences` and the 1.2 wall-second grace; seconds default 10),
//! timeline maths (§4.8), the listening clock (§4.9: counts only while `playing` and audible, capped by media
//! progress / rate; banked before rate changes and seeks), output gating on the warmer (§4.7 `playCurrent`; ≤ 3 s),
//! failed (undecodable) chunks keep their timeline slot and are skipped.
//!
//! Deliberate differences (ARCHITECTURE.md §5):
//! - **Gapless** (Product ruling): chunk *n+1* is decoded, time-stretched and resampled before chunk *n* ends and its
//!   blocks are queued right behind it in the same output stream, so there is no gap and no click between chunks
//!   (the `<audio>`-per-chunk gap is gone). "Audible" starts when the first block of the session reaches the device
//!   and stays true across chunk boundaries; the listening clock still banks per chunk at the boundary.
//! - **Pitch-preserving speed** with WSOLA (`stretch`), 0.5×–3×; rate 1 is a bit-exact pass-through.
//! - **Device changes**: the output follows the Windows default device; on loss/change the player reopens the new
//!   default within ~1 s and resumes from the same position (no status change; logged).
//! - **No output device at all** (reader-playback.md §6 "Creation failure never waits", §12 #28): the session plays
//!   on a simulated clock (timing, reports, listening and look-ahead behave as if audible; nothing is heard), exactly
//!   like Chromium did; the default device is polled every 2 s and used as soon as one appears. This is not a failure.
//!   The same applies **mid-read**: a (re)open that fails with `OutputError::NoDevice` (the only output was unplugged)
//!   switches the session to the simulated clock, keeping position and status, and does **not** count toward the
//!   failure budget below.
//! - **A stalled output** (reader-playback.md §12 #28 "a context that exists but never runs"; in the original only
//!   the warmer's context was affected, here speech shares the stream): once the warm-up has given up, a read whose
//!   real output renders nothing for 2 s reopens it (logged); a second stall within a minute moves the read to the
//!   simulated clock like a missing device (that device is not retried for a minute unless the default changes), so
//!   the read still progresses and ends. Ring under-runs while speech is fed (the original's `waiting`) are counted
//!   by the callback and logged (D-RP-10); they ramp down and fade back in rather than cut.
//! - **Failure recovery** (Product ruling "A player failure restarts the player and stops the engine with an error
//!   card"): if the player panics, or stream (re)opens fail with `OutputError::Stream` 3 times within 10 s
//!   (`NoDevice` never counts), the supervisor at the top of the player thread (which runs the player state machine
//!   under `catch_unwind`) drops it together with its output stream and starts a fresh one, re-creates the failed
//!   session's `SessionInfo` (kept in the shared snapshot state) in status `error` (`error =
//!   vox_core::copy::PLAYBACK_FAILED` after a panic, `NO_AUDIO_OUTPUT` after stream failures; `errorCode = unknown`;
//!   no chunks; rate and volume kept), sends a [`PlayerEvent::State`] for it and then [`PlayerEvent::Failed`]. The core
//!   then stops the engine and shows the widget (error card with "Try again"). With no session the restart is silent
//!   apart from the log. Messages queued meanwhile are kept (the channel outlives the restart). Decoding runs on a
//!   separate worker thread where a panicking decoder only fails its chunk (if that thread cannot be started, the
//!   player thread decodes inline, logged, with the same panic guard).
//!
//! Reports: a [`PlayerEvent::State`] on every status change, on every discrete change (chunk arrival, done, warning,
//! seek, rate, volume, pause/play), on every spec `notify()` / `changed(report = false)` point (reader-playback.md
//! §4.6, §4.10 — e.g. the `onChunkMeta` decoded-duration refinement, also while paused) and every 500 ms while
//! `playing`. The core uses them for history, the engine's look-ahead, the tray, SMTC and as the UI's discrete
//! updates; the UI additionally polls [`Player::snapshot`] at ~30 Hz while playing (ARCHITECTURE.md §3.3). Offsets are
//! UTF-16 units of the session text throughout.
//!
//! **Explicit stop** (reader-playback.md §4.6 `stop()`, §9 Lifetime): `command(Stop)` emits the final
//! `State(idle, session Some, final listenedSec)` (the core's last listening fold needs the session) and **then a
//! second `State(idle, session None)`** keeping `rate` and `volume`, so the core's carried snapshot, SMTC and the
//! widget's view selection see "no session". The `stop()` inside [`Player::start_session`] (a supersede) does **not**
//! emit the sessionless state: it yields `[State(idle, old session), State(loading, new session)]` with nothing in
//! between, so nothing flickers. Scenario tests: Stop → `[idle+Some, idle+None]`; supersede → `[idle+old, loading+new]`.
//!
//! Synchronous targets (reader-playback.md §8.5 `setRate` is synchronous in the original): [`Player::command`] with
//! `Rate`/`Volume` writes the clamped target into the shared snapshot state (`state.rate` / `state.volume`) **before**
//! enqueueing, and [`Player::start_session`] does the same from the payload, so `snapshot()` reflects them at once
//! (the core's `CycleRate` computes `next_rate(player.snapshot().state.rate)`; a quick double click advances twice).
//!
//! Idle cost (BRIEF.md Goal 3): the player thread's 10 ms tick runs only while audio is playing or about to (status
//! `playing`, a gated `loading` start, a feed in progress, a pending warm-up or an output being re-opened). While the
//! stream is merely held after a stop/pause it wakes only for the 2 s device poll and the scheduled release; otherwise
//! it blocks on its channels (commands and decode results) with no timeout.
//!
//! Listening clock vs display position (reader-playback.md §4.9 "system sleep must add nothing", §8.7/§8.9 highlight
//! follows what is heard): each block popped by the cpal callback is tagged with its device playback instant
//! (`OutputCallbackInfo::timestamp().playback`); the "played" tag is the last block whose playback instant ≤ now and
//! drives position, `charIndex` and chunk `ended`. The listening clock's media time comes **only** from that
//! device-reported tag on the player thread, never from [`Player::snapshot`]'s extrapolation, which is a display-only
//! interpolation (`(now − tag_seen_at) × rate`, capped at the chunk).

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use parking_lot::Mutex;
use vox_core::constants::{RATE_MAX, RATE_MIN};
use vox_core::player::{PlayerCommand, PlayerSnapshot};
use vox_core::tts::{TtsChunk, TtsSessionDone, TtsSessionError, TtsSessionStart};

use crate::decode::decode;
use crate::engine::{DecodeJob, DecodeQueue, DecodeResult, Engine, Exit, Msg, OutputFactory};
use crate::output::{AudioOutput, OutputError, Sink};
use crate::shared::Shared;

/// Player settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerConfig {
    /// State report cadence while playing (500 ms).
    pub report_interval: Duration,
    /// Use the output warmer (keep-alive signal + gated starts). Off in tests.
    pub warm_output: bool,
    /// Open no audio device at all (tests / screenshot mode on machines without audio): timing runs on a simulated
    /// clock.
    pub null_output: bool,
    /// Name for the thread and the log scope (`player` / `preview`).
    pub name: &'static str,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            report_interval: Duration::from_millis(vox_core::constants::PLAYER_REPORT_MS),
            warm_output: true,
            null_output: false,
            name: "player",
        }
    }
}

/// What the player tells the core.
// The variants are the cross-crate contract (ARCHITECTURE.md §2.5); a report is a full snapshot by design.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// A state report (see module docs for when).
    State(PlayerSnapshot),
    /// The player failed and restarted itself (module docs "Failure recovery"); the preceding `State` already shows
    /// the `error` card state. `session_id` = the session that failed (`None` = idle restart). The core stops the
    /// engine (if that session is the engine's) and shows the widget when enabled.
    Failed { session_id: Option<String>, message: String },
}

/// Listener for [`PlayerEvent`]s (vox-app enqueues `Command::PlayerReport`). Must not block.
pub type PlayerEvents = Arc<dyn Fn(PlayerEvent) + Send + Sync>;

struct Inner {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// Handle to the player thread.
#[derive(Clone)]
pub struct Player {
    inner: Arc<Inner>,
}

impl Player {
    /// Spawns the player thread. The output stream is opened lazily (first `preload_output` or session).
    pub fn spawn(config: PlayerConfig, events: PlayerEvents) -> Player {
        Self::spawn_with(config, events, Arc::new(|| Box::new(CpalFactory) as Box<dyn OutputFactory>))
    }

    /// [`Player::spawn`] with an injected output factory (tests script device failures through it).
    pub(crate) fn spawn_with(config: PlayerConfig, events: PlayerEvents, factory: FactoryFn) -> Player {
        Self::spawn_impl(config, events, factory, false)
    }

    /// Tests: a player whose decode worker "could not be started" (inline decoding on the player thread).
    #[cfg(test)]
    pub(crate) fn spawn_decoding_inline(config: PlayerConfig, events: PlayerEvents) -> Player {
        Self::spawn_impl(config, events, Arc::new(|| Box::new(CpalFactory) as Box<dyn OutputFactory>), true)
    }

    fn spawn_impl(config: PlayerConfig, events: PlayerEvents, factory: FactoryFn, no_worker: bool) -> Player {
        let (tx, rx) = unbounded::<Msg>();
        let shared = Arc::new(Shared::new());
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("vox-{}", config.name))
            .spawn(move || player_thread(config, events, thread_shared, rx, factory, no_worker));
        let thread = match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                tracing::error!(target: "audio", player = config.name, "could not start the player thread: {e}");
                None
            }
        };
        Player { inner: Arc::new(Inner { tx, shared, thread: Mutex::new(thread) }) }
    }

    fn send(&self, msg: Msg) {
        // After shutdown the thread is gone: messages are dropped.
        let _ = self.inner.tx.send(msg);
    }

    /// `startSession` (§4.4).
    pub fn start_session(&self, start: TtsSessionStart) {
        let (rate_marked, volume_marked) = {
            let mut p = self.inner.shared.published.lock();
            let r = start.rate.is_finite();
            let v = start.volume.is_finite();
            if r {
                p.rate = start.rate.clamp(RATE_MIN, RATE_MAX);
                p.rate_pending += 1;
            }
            if v {
                p.volume = start.volume.clamp(0.0, 1.0);
                p.volume_pending += 1;
            }
            (r, v)
        };
        self.send(Msg::Start { start, rate_marked, volume_marked });
    }

    /// `addChunk` (§4.4). Audio is decoded on the player's decode worker.
    pub fn add_chunk(&self, chunk: TtsChunk) {
        self.send(Msg::Chunk(chunk));
    }

    /// `finishSession` (§4.4).
    pub fn finish_session(&self, done: TtsSessionDone) {
        self.send(Msg::Done(done));
    }

    /// `failSession` (§4.4).
    pub fn fail_session(&self, error: TtsSessionError) {
        self.send(Msg::Fail(error));
    }

    /// Transport command (§4.6). `Stop` ends the session (final `idle` report with the session, then a sessionless
    /// `idle` report — module docs). `Rate`/`Volume` update the shared snapshot synchronously before enqueueing.
    /// Callers pass commands through `vox_core::player::sanitize_player_command` first (the core does).
    pub fn command(&self, cmd: PlayerCommand) {
        let marked = {
            let mut p = self.inner.shared.published.lock();
            match &cmd {
                PlayerCommand::Rate { rate } if rate.is_finite() => {
                    p.rate = rate.clamp(RATE_MIN, RATE_MAX);
                    p.rate_pending += 1;
                    true
                }
                PlayerCommand::Volume { volume } if volume.is_finite() => {
                    p.volume = volume.clamp(0.0, 1.0);
                    p.volume_pending += 1;
                    true
                }
                _ => false,
            }
        };
        self.send(Msg::Command { cmd, marked });
    }

    /// The current snapshot computed for "now" (position includes the time played since the last report).
    pub fn snapshot(&self) -> PlayerSnapshot {
        self.inner.shared.snapshot()
    }

    /// Builds the output stream while idle and leaves it suspended (warmer `preload`, §6). vox-app calls it once,
    /// `PRELOAD_OUTPUT_DELAY_MS` (1 s) after core startup whatever the window visibility (`TimerId::PreloadOutput`);
    /// a no-op with `null_output`.
    pub fn preload_output(&self) {
        self.send(Msg::Preload);
    }

    /// Stops the session (without reporting) and closes the output; the thread exits. Idempotent. Blocks until the
    /// thread has exited (the core bounds the wait), except when called from the player thread itself.
    pub fn shutdown(&self) {
        self.send(Msg::Shutdown);
        let handle = self.inner.thread.lock().take();
        if let Some(handle) = handle {
            if handle.thread().id() != std::thread::current().id() && handle.join().is_err() {
                tracing::error!(target: "audio", "the player thread ended with a panic");
            }
        }
    }

    /// Timer-driven wake-ups of the player thread so far (idle-cost checks).
    #[cfg(test)]
    pub(crate) fn ticks(&self) -> u64 {
        self.inner.shared.ticks.load(Ordering::Relaxed)
    }

    /// Makes the player thread panic (supervisor recovery tests).
    #[cfg(test)]
    pub(crate) fn inject_panic(&self) {
        self.send(Msg::Panic);
    }
}

// ───────────────────────────── the thread ─────────────────────────────

struct ChannelDecoder {
    tx: Sender<DecodeJob>,
}

impl DecodeQueue for ChannelDecoder {
    fn request(&mut self, job: DecodeJob) {
        // The worker only goes away with the player thread.
        let _ = self.tx.send(job);
    }
}

/// Makes the output factory for each (re)started player.
pub(crate) type FactoryFn = Arc<dyn Fn() -> Box<dyn OutputFactory> + Send + Sync>;

struct CpalFactory;

impl OutputFactory for CpalFactory {
    fn open(&mut self) -> Result<Box<dyn Sink>, OutputError> {
        // cpal (and the drivers below it) must not take the player down with a panic.
        match catch_unwind(AssertUnwindSafe(AudioOutput::open)) {
            Ok(result) => result.map(|o| Box::new(o) as Box<dyn Sink>),
            Err(_) => Err(OutputError::Stream("the audio backend panicked while opening the device".into())),
        }
    }

    fn default_device_id(&mut self) -> Option<String> {
        catch_unwind(crate::output::default_device_id).ok().flatten()
    }
}

/// Decodes one chunk file; a panicking decoder is a decode failure.
fn decode_job(job: DecodeJob) -> DecodeResult {
    let result = match catch_unwind(AssertUnwindSafe(|| decode(&job.audio, &job.mime))) {
        Ok(Ok(audio)) => Ok(Arc::new(audio)),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the decoder panicked".to_string()),
    };
    DecodeResult { serial: job.serial, index: job.index, result }
}

/// Decodes chunk files off the player thread.
fn decode_worker(jobs: Receiver<DecodeJob>, results: Sender<DecodeResult>) {
    for job in jobs.iter() {
        if results.send(decode_job(job)).is_err() {
            break;
        }
    }
}

/// The fallback when the decode worker could not be started: decodes on the player thread, delivering through the
/// same result channel (the engine never sees a result re-entrantly).
struct InlineDecoder {
    results: Sender<DecodeResult>,
}

impl DecodeQueue for InlineDecoder {
    fn request(&mut self, job: DecodeJob) {
        // The player thread holds the receiver for as long as it can call this.
        let _ = self.results.send(decode_job(job));
    }
}

enum Wake {
    Msg(Result<Msg, crossbeam_channel::RecvError>),
    Decoded(Result<DecodeResult, crossbeam_channel::RecvError>),
    Timeout,
}

enum RunEnd {
    Shutdown,
    Disconnected,
    Failed(Exit),
}

fn run(engine: &mut Engine, shared: &Shared, rx: &Receiver<Msg>, decoded: &Receiver<DecodeResult>) -> RunEnd {
    let mut decoded = decoded.clone();
    loop {
        if let Some(exit) = engine.take_exit() {
            return RunEnd::Failed(exit);
        }
        #[cfg(test)]
        shared.wakeups.fetch_add(1, Ordering::Relaxed);
        let now = shared.now_ms();
        let wake = match engine.next_wakeup(now) {
            Some(at) => {
                let timeout = Duration::from_secs_f64(((at - now).max(0.0)) / 1000.0);
                select! {
                    recv(rx) -> m => Wake::Msg(m),
                    recv(decoded) -> r => Wake::Decoded(r),
                    default(timeout) => Wake::Timeout,
                }
            }
            None => select! {
                recv(rx) -> m => Wake::Msg(m),
                recv(decoded) -> r => Wake::Decoded(r),
            },
        };
        let now = shared.now_ms();
        match wake {
            Wake::Msg(Ok(Msg::Shutdown)) => {
                engine.shutdown_quietly();
                return RunEnd::Shutdown;
            }
            Wake::Msg(Ok(msg)) => engine.handle(msg, now),
            Wake::Msg(Err(_)) => return RunEnd::Disconnected,
            Wake::Decoded(Ok(result)) => engine.on_decoded(result, now),
            Wake::Decoded(Err(_)) => {
                // No decoder is left (cannot happen while this thread holds a sender): a disconnected channel is
                // always ready, so stop selecting it instead of spinning.
                tracing::error!(target: "audio", "the decode channel closed");
                decoded = crossbeam_channel::never();
            }
            Wake::Timeout => {
                shared.ticks.fetch_add(1, Ordering::Relaxed);
            }
        }
        engine.tick(now);
    }
}

/// The supervisor (R-Audio): runs the engine under `catch_unwind`; a panic or an output failure restarts it with
/// the failed session in `error` (`State` then `Failed`).
fn player_thread(
    config: PlayerConfig,
    events: PlayerEvents,
    shared: Arc<Shared>,
    rx: Receiver<Msg>,
    factory: FactoryFn,
    no_worker: bool,
) {
    let (job_tx, job_rx) = unbounded::<DecodeJob>();
    // The thread keeps a result sender itself: the channel never disconnects under `run`, and inline decoding (below)
    // delivers through it.
    let (result_tx, result_rx) = unbounded::<DecodeResult>();
    let worker = if no_worker {
        Err(std::io::Error::other("the decode worker is disabled"))
    } else {
        let worker_results = result_tx.clone();
        std::thread::Builder::new()
            .name(format!("vox-{}-decode", config.name))
            .spawn(move || decode_worker(job_rx, worker_results))
    };
    let inline = match &worker {
        Ok(_) => false,
        Err(e) => {
            tracing::error!(
                target: "audio",
                player = config.name,
                "could not start the decode thread ({e}); decoding on the player thread"
            );
            true
        }
    };
    let mut recovery: Option<&'static str> = None;
    let mut restarts: VecDeque<Instant> = VecDeque::new();
    loop {
        let decoder: Box<dyn DecodeQueue> = if inline {
            Box::new(InlineDecoder { results: result_tx.clone() })
        } else {
            Box::new(ChannelDecoder { tx: job_tx.clone() })
        };
        let mut engine = Engine::new(config, events.clone(), shared.clone(), decoder, factory());
        if let Some(message) = recovery.take() {
            let session = shared.published.lock().session.clone();
            let now = shared.now_ms();
            if catch_unwind(AssertUnwindSafe(|| engine.recover(session, message, now))).is_err() {
                tracing::error!(target: "audio", player = config.name, "player recovery panicked");
            }
        }
        let outcome = catch_unwind(AssertUnwindSafe(|| run(&mut engine, &shared, &rx, &result_rx)));
        // Dropping the engine closes the stream; that must not take the thread down either.
        let _ = catch_unwind(AssertUnwindSafe(move || drop(engine)));
        match outcome {
            Ok(RunEnd::Shutdown) | Ok(RunEnd::Disconnected) => break,
            Ok(RunEnd::Failed(Exit::OutputFailed)) => {
                tracing::error!(target: "audio", player = config.name, "audio output failed; restarting the player");
                recovery = Some(vox_core::copy::NO_AUDIO_OUTPUT);
            }
            Err(_) => {
                tracing::error!(target: "audio", player = config.name, "the player panicked; restarting it");
                recovery = Some(vox_core::copy::PLAYBACK_FAILED);
            }
        }
        // A player that keeps failing must not spin.
        let now = Instant::now();
        restarts.push_back(now);
        while restarts.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(10)) {
            restarts.pop_front();
        }
        if restarts.len() > 5 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    // The decode worker ends on its own: its job channel closes here.
    drop(job_tx);
    drop(result_tx);
    drop(worker);
}

#[cfg(test)]
#[path = "player_tests.rs"]
mod tests;
