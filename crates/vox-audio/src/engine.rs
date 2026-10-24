//! The player state machine (reader-playback.md §4.3–§4.10), single-threaded and driven by the player thread with
//! "now" (monotonic ms): session events, transport commands, decode results and a 10 ms tick while active. The
//! browser's per-chunk `<audio>` element maps to "the feed of the current chunk" (ARCHITECTURE.md §5.2):
//!
//! | Original | Here |
//! |---|---|
//! | `ensureAudio(i)` / `releaseAudio(c)` | request the chunk's decode / drop its PCM (encoded bytes stay) |
//! | element `paused` | `!element_playing` (the feed is not streaming the current chunk) |
//! | `playing` event (audible) | the first block of the current output generation reported played |
//! | `waiting` event (stall) | a ring under-run while speech is fed: counted by the callback and logged (listening needs no correction: it is capped by the device-reported progress) |
//! | `ended` event | the played tag passes the chunk's last frame (or moves to a later chunk: gapless) |
//! | `currentTime` | the played tag's source frame ÷ the chunk's decoded rate (else the chunk `offset`) |
//! | `loadedmetadata` duration | the decoded duration, adopted when it differs by > 0.01 s (a `State` report) |
//! | element `error` | decode failure → chunk `failed` (keeps its slot, skipped) |
//!
//! Every flush of the output (pause, seek, stop, rate change, device change) first records the heard position as
//! the chunk offset and banks the listening clock, so positions never jump back and nothing is double counted.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use vox_core::constants::{
    BACK_GRACE_SEC, DEFAULT_SKIP_SEC, ESTIMATED_CHARS_PER_SECOND, IDLE_OUTPUT_HOLD_MS, PARTIAL_SKIP_SEC,
    PAUSED_OUTPUT_HOLD_MS, RATE_MAX, RATE_MIN,
};
use vox_core::player::{PlayerCommand, PlayerStatus, SessionInfo, SessionWarning, SkipUnit};
use vox_core::read::ReadErrorCode;
use vox_core::text16::{prefix16, utf16_len};
use vox_core::tts::{Alignment, AudioBytes, TtsChunk, TtsSessionDone, TtsSessionError, TtsSessionStart, MIME_MPEG};

use crate::clock::ListeningClock;
use crate::decode::DecodedAudio;
use crate::feeder::{ChunkAudio, ChunkSource, Feeder};
use crate::output::{OutputError, SimOutput, Sink, Underruns};
use crate::player::{PlayerConfig, PlayerEvent, PlayerEvents};
use crate::shared::{snapshot_from, Shared};
use crate::timeline::{Timeline, TimelineChunk};
use crate::warmer::{OutputWarmer, WarmState};

/// Tick period while active.
pub(crate) const TICK_MS: f64 = 10.0;
/// Device poll period while a stream is held (or a no-device session waits for one).
const DEVICE_POLL_MS: f64 = 2_000.0;
/// Retry delay after a stream (re)open failed with `OutputError::Stream`.
const STREAM_RETRY_MS: f64 = 1_000.0;
/// `Stream` failures within this window …
const FAILURE_WINDOW_MS: f64 = 10_000.0;
/// … that end the player (R-Audio recovery).
const FAILURE_BUDGET: usize = 3;
/// The simulated device (no output / `null_output`).
const SIM_RATE: u32 = 48_000;
const SIM_CHANNELS: u16 = 2;
/// A running real output that renders nothing for this long while a read plays has stalled: it is reopened
/// (reader-playback.md §12 #28 "a context that exists but never runs").
const STALL_MS: f64 = 2_000.0;
/// A gap this long between two ticks means the player thread did not run (system sleep, a starved process): the
/// stall window restarts rather than counting it.
const STALL_GAP_MS: f64 = 250.0;
/// A second stall within this window gives up on the device: the read continues on the simulated clock and the
/// device is not tried again for this long (unless the default device changes).
const STALL_REPEAT_MS: f64 = 60_000.0;
/// Under-runs are logged at most this often (aggregated).
const UNDERRUN_LOG_MS: f64 = 5_000.0;

const WARNING_DEFAULT: &str = "Part of the text could not be synthesized";
const ERROR_DEFAULT: &str = "Something went wrong";

/// Messages from the handle.
pub(crate) enum Msg {
    Start {
        start: TtsSessionStart,
        rate_marked: bool,
        volume_marked: bool,
    },
    Chunk(TtsChunk),
    Done(TtsSessionDone),
    Fail(TtsSessionError),
    Command {
        cmd: PlayerCommand,
        marked: bool,
    },
    Preload,
    Shutdown,
    /// Tests: makes the engine panic (supervisor recovery).
    #[cfg(test)]
    Panic,
}

pub(crate) struct DecodeJob {
    pub serial: u64,
    pub index: usize,
    pub audio: AudioBytes,
    pub mime: String,
}

pub(crate) struct DecodeResult {
    pub serial: u64,
    pub index: usize,
    pub result: Result<Arc<DecodedAudio>, String>,
}

/// Where decode jobs go (a worker thread in production).
pub(crate) trait DecodeQueue {
    fn request(&mut self, job: DecodeJob);
}

/// Opens outputs (cpal in production).
pub(crate) trait OutputFactory {
    fn open(&mut self) -> Result<Box<dyn Sink>, OutputError>;
    /// The current default device (`None` = no device).
    fn default_device_id(&mut self) -> Option<String>;
}

/// Why the engine asks its thread to restart it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    /// Stream (re)opens failed `FAILURE_BUDGET` times within `FAILURE_WINDOW_MS`.
    OutputFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutKind {
    None,
    Real,
    /// `PlayerConfig::null_output`.
    SimNull,
    /// No output device: simulated clock until one appears.
    SimNoDevice,
}

#[derive(Debug, Clone, Copy)]
struct Gate {
    token: u64,
    serial: u64,
    chunk: usize,
}

/// Progress watchdog of a real output while a read plays.
#[derive(Debug, Clone, Copy)]
struct Watch {
    /// Start of the current no-progress window.
    since: f64,
    last_tick: f64,
    rendered: u64,
}

/// Under-run bookkeeping for the log.
#[derive(Debug, Clone, Copy, Default)]
struct UnderrunLog {
    /// Counters as last seen.
    seen: Underruns,
    /// Not logged yet.
    pending_frames: u64,
    pending_xruns: u64,
    next_log_ms: f64,
    /// Everything counted while playing (tests).
    total_frames: u64,
}

struct ChunkEntry {
    audio: AudioBytes,
    mime: String,
    alignment: Option<Arc<Alignment>>,
    decoded: Option<Arc<DecodedAudio>>,
    decoding: bool,
    decoded_rate: Option<u32>,
    decoded_frames: Option<usize>,
    /// Seconds to start at when the chunk's feed (re)starts.
    offset: f64,
}

struct Session {
    serial: u64,
    info: Arc<SessionInfo>,
    timeline: Timeline,
    entries: Vec<ChunkEntry>,
    pending: BTreeMap<u32, TtsChunk>,
    warning: Option<SessionWarning>,
    partial_text: bool,
    /// The chunk whose decode the feeder asked for last (its result is kept even beyond the look-ahead window).
    feeder_wants: Option<usize>,
}

impl Session {
    fn new(serial: u64, info: Arc<SessionInfo>, estimated_duration_sec: f64) -> Self {
        let text_len = utf16_len(&info.text) as u32;
        Self {
            serial,
            info,
            timeline: Timeline::new(text_len, estimated_duration_sec),
            entries: Vec::new(),
            pending: BTreeMap::new(),
            warning: None,
            partial_text: false,
            feeder_wants: None,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Decoded audio of chunk `i` is worth keeping: it is the current chunk or one of the next two playable chunks
    /// (failed chunks are skipped, as the feeder skips them), or the feeder is waiting for it.
    fn keeps_decoded(&self, i: usize, cur: usize) -> bool {
        if self.feeder_wants == Some(i) {
            return true;
        }
        if i < cur || i >= self.len() {
            return false;
        }
        let failed = |k: &usize| self.timeline.chunks.get(*k).map(|c| c.failed).unwrap_or(true);
        let playable_before = (cur..i).filter(|k| !failed(k)).count();
        playable_before <= 2
    }

    /// `pushChunk`.
    fn push_chunk(&mut self, c: TtsChunk) {
        let len = self.timeline.text_len;
        if c.text_start > len || c.text_end > len {
            self.partial_text = true;
        }
        let text_start = c.text_start.min(len);
        let text_end = c.text_end.clamp(text_start, len);
        let alignment = c.alignment.filter(|a| a.usable_for_timing());
        let duration_sec = if c.duration_sec.is_finite() && c.duration_sec > 0.0 { c.duration_sec } else { 0.0 };
        self.timeline.chunks.push(TimelineChunk {
            text_start,
            text_end,
            duration_sec,
            alignment_starts: alignment.as_ref().map(|a| a.starts.clone()),
            failed: false,
        });
        self.entries.push(ChunkEntry {
            audio: c.audio,
            mime: if c.mime.is_empty() { MIME_MPEG.to_string() } else { c.mime },
            alignment,
            decoded: None,
            decoding: false,
            decoded_rate: None,
            decoded_frames: None,
            offset: 0.0,
        });
    }

    fn request_decode(&mut self, i: usize, decoder: &mut dyn DecodeQueue) {
        let failed = self.timeline.chunks.get(i).map(|c| c.failed).unwrap_or(true);
        let Some(e) = self.entries.get_mut(i) else { return };
        if failed || e.decoded.is_some() || e.decoding {
            return;
        }
        e.decoding = true;
        decoder.request(DecodeJob { serial: self.serial, index: i, audio: e.audio.clone(), mime: e.mime.clone() });
    }
}

struct SessionSource<'a> {
    session: &'a mut Session,
    decoder: &'a mut dyn DecodeQueue,
}

impl ChunkSource for SessionSource<'_> {
    fn audio(&mut self, i: usize) -> ChunkAudio {
        let s = &mut *self.session;
        if i >= s.len() {
            return ChunkAudio::Missing { more: !s.timeline.done };
        }
        if s.timeline.chunks[i].failed {
            return ChunkAudio::Failed;
        }
        if let Some(d) = &s.entries[i].decoded {
            return ChunkAudio::Ready(d.clone());
        }
        s.request_decode(i, self.decoder);
        s.feeder_wants = Some(i);
        ChunkAudio::Pending
    }
}

pub(crate) struct Engine {
    cfg: PlayerConfig,
    events: PlayerEvents,
    shared: Arc<Shared>,
    decoder: Box<dyn DecodeQueue>,
    factory: Box<dyn OutputFactory>,
    status: PlayerStatus,
    session: Option<Session>,
    cur: usize,
    want_play: bool,
    waiting: bool,
    rate: f64,
    volume: f64,
    clock: ListeningClock,
    audible: bool,
    error: Option<(String, ReadErrorCode)>,
    start_token: u64,
    gated_token: Option<u64>,
    gate: Option<Gate>,
    element_playing: bool,
    warmer: OutputWarmer,
    out: Option<Box<dyn Sink>>,
    kind: OutKind,
    feeder: Feeder,
    next_report_ms: Option<f64>,
    next_poll_ms: Option<f64>,
    retry_open_ms: Option<f64>,
    stream_failures: Vec<f64>,
    serial: u64,
    no_device_logged: bool,
    exit: Option<Exit>,
    /// Output generations, unique across every output this engine opens (a block of an older output's generation
    /// can never pass for a current one).
    generation: u64,
    watch: Option<Watch>,
    last_stall_ms: Option<f64>,
    /// The device that stalled twice (its id, when) — not reopened for `STALL_REPEAT_MS` unless the default changes.
    stalled_device: Option<(Option<String>, f64)>,
    underruns: UnderrunLog,
}

impl Engine {
    pub(crate) fn new(
        cfg: PlayerConfig,
        events: PlayerEvents,
        shared: Arc<Shared>,
        decoder: Box<dyn DecodeQueue>,
        factory: Box<dyn OutputFactory>,
    ) -> Self {
        let (rate, volume) = {
            let p = shared.published.lock();
            (p.rate, p.volume)
        };
        Self {
            cfg,
            events,
            shared,
            decoder,
            factory,
            status: PlayerStatus::Idle,
            session: None,
            cur: 0,
            want_play: true,
            waiting: false,
            rate: if rate.is_finite() { rate.clamp(RATE_MIN, RATE_MAX) } else { 1.0 },
            volume: if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 },
            clock: ListeningClock::default(),
            audible: false,
            error: None,
            start_token: 0,
            gated_token: None,
            gate: None,
            element_playing: false,
            warmer: OutputWarmer::default(),
            out: None,
            kind: OutKind::None,
            feeder: Feeder::new(),
            next_report_ms: None,
            next_poll_ms: None,
            retry_open_ms: None,
            stream_failures: Vec::new(),
            serial: 0,
            no_device_logged: false,
            exit: None,
            generation: 0,
            watch: None,
            last_stall_ms: None,
            stalled_device: None,
            underruns: UnderrunLog::default(),
        }
    }

    pub(crate) fn take_exit(&mut self) -> Option<Exit> {
        self.exit.take()
    }

    // ───────────────────────────── inputs ─────────────────────────────

    pub(crate) fn handle(&mut self, msg: Msg, now: f64) {
        self.update_playhead(now);
        match msg {
            Msg::Start { start, rate_marked, volume_marked } => {
                self.start_session(start, now);
                self.unmark(rate_marked, volume_marked);
            }
            Msg::Chunk(c) => self.add_chunk(c, now),
            Msg::Done(d) => self.finish_session(d, now),
            Msg::Fail(e) => self.fail_session(e, now),
            Msg::Command { cmd, marked } => {
                let (r, v) = match &cmd {
                    PlayerCommand::Rate { .. } => (marked, false),
                    PlayerCommand::Volume { .. } => (false, marked),
                    _ => (false, false),
                };
                self.command(cmd, now);
                self.unmark(r, v);
            }
            Msg::Preload => self.preload(now),
            Msg::Shutdown => self.shutdown_quietly(),
            #[cfg(test)]
            Msg::Panic => panic!("injected player panic"),
        }
        self.pump();
        self.publish(now);
    }

    fn unmark(&mut self, rate: bool, volume: bool) {
        if !rate && !volume {
            return;
        }
        let mut p = self.shared.published.lock();
        if rate {
            p.rate_pending = p.rate_pending.saturating_sub(1);
            if p.rate_pending == 0 {
                p.rate = self.rate;
            }
        }
        if volume {
            p.volume_pending = p.volume_pending.saturating_sub(1);
            if p.volume_pending == 0 {
                p.volume = self.volume;
            }
        }
    }

    pub(crate) fn on_decoded(&mut self, r: DecodeResult, now: f64) {
        self.update_playhead(now);
        let cur = self.cur;
        let waiting_for = self.feeder.waiting_for();
        let Some(s) = self.session.as_mut() else { return };
        if s.serial != r.serial {
            return;
        }
        let i = r.index;
        let keep = s.keeps_decoded(i, cur) || waiting_for == Some(i);
        let Some(e) = s.entries.get_mut(i) else { return };
        e.decoding = false;
        match r.result {
            Ok(audio) => {
                e.decoded_rate = Some(audio.sample_rate);
                e.decoded_frames = Some(audio.frames());
                let d = audio.duration_sec();
                if keep {
                    e.decoded = Some(audio);
                }
                let tc = &mut s.timeline.chunks[i];
                let refined = d.is_finite() && d > 0.0 && (d - tc.duration_sec).abs() > 0.01;
                if refined {
                    // `onChunkMeta`: the decoded duration is more exact than the engine's.
                    tc.duration_sec = d;
                }
                if refined {
                    self.changed(now);
                }
            }
            Err(message) => {
                tracing::warn!(target: "audio", player = self.cfg.name, chunk = i, "chunk could not be decoded: {message}");
                self.on_chunk_error(i, now);
            }
        }
        self.pump();
        self.publish(now);
    }

    /// The 10 ms tick (and every other wake-up).
    pub(crate) fn tick(&mut self, now: f64) {
        self.update_playhead(now);
        self.check_devices(now);
        self.watch_progress(now);
        if self.out.is_none()
            && self.session.is_some()
            && matches!(self.status, PlayerStatus::Playing | PlayerStatus::Loading)
        {
            self.ensure_output(now);
            if self.out.is_some() && self.status == PlayerStatus::Playing {
                self.restart_heard_feed(now);
            }
        }
        if let Some(g) = self.gate {
            if self.poll_warmer(now) == WarmState::Ready {
                self.gate = None;
                let same = self.session.as_ref().map(|s| s.serial) == Some(g.serial) && self.cur == g.chunk;
                // A newer start, a pause, a stop or a seek supersedes the gated start.
                if g.token == self.start_token
                    && same
                    && self.want_play
                    && !self.waiting
                    && self.status == PlayerStatus::Loading
                {
                    self.start_element(now);
                }
            }
        } else if self.warmer.is_waiting() {
            let _ = self.poll_warmer(now);
        }
        if self.warmer.should_suspend(now) {
            if let Some(out) = self.out.as_mut() {
                out.suspend();
                tracing::debug!(target: "audio", player = self.cfg.name, "output released");
            }
        }
        self.pump();
        self.check_underruns(now);
        if self.status == PlayerStatus::Playing {
            if let Some(t) = self.next_report_ms {
                if now >= t {
                    self.next_report_ms = Some(now + self.report_interval_ms());
                    self.report(now);
                }
            }
        }
        self.publish(now);
    }

    /// When the thread should wake next without a message (`None` = block).
    pub(crate) fn next_wakeup(&self, now: f64) -> Option<f64> {
        let active_status = matches!(self.status, PlayerStatus::Playing | PlayerStatus::Loading);
        let fast = self.gate.is_some()
            || self.warmer.is_waiting()
            || self.status == PlayerStatus::Playing
            || (self.status == PlayerStatus::Loading && self.feeder.is_active())
            || (self.out.is_none() && self.session.is_some() && active_status);
        if fast {
            return Some(now + TICK_MS);
        }
        let mut next: Option<f64> = None;
        let mut consider = |t: Option<f64>| {
            if let Some(t) = t {
                next = Some(next.map_or(t, |n: f64| n.min(t)));
            }
        };
        let held = self.kind == OutKind::Real && self.out.as_ref().map(|o| o.is_running()).unwrap_or(false);
        if held {
            consider(self.warmer.release_at());
            consider(self.next_poll_ms);
        }
        if self.kind == OutKind::SimNoDevice && self.session.is_some() && self.status != PlayerStatus::Ended {
            consider(self.next_poll_ms);
        }
        next
    }

    /// Restart after a failure (R-Audio): the failed session in `error`, `State` then `Failed`.
    pub(crate) fn recover(&mut self, session: Option<Arc<SessionInfo>>, message: &str, now: f64) {
        let Some(info) = session else {
            self.publish(now);
            return;
        };
        self.serial += 1;
        let est = utf16_len(&info.text) as f64 / ESTIMATED_CHARS_PER_SECOND;
        let id = info.id.clone();
        self.session = Some(Session::new(self.serial, info, est));
        self.error = Some((message.to_string(), ReadErrorCode::Unknown));
        self.want_play = false;
        self.waiting = false;
        self.set_status(PlayerStatus::Error, true, now);
        self.emit(PlayerEvent::Failed { session_id: Some(id), message: message.to_string() });
    }

    /// Stops everything without reporting and closes the output.
    pub(crate) fn shutdown_quietly(&mut self) {
        self.feeder.stop();
        self.gate = None;
        self.session = None;
        self.out = None;
        self.kind = OutKind::None;
    }

    // ───────────────────────────── session events (§4.4) ─────────────────────────────

    fn start_session(&mut self, s: TtsSessionStart, now: f64) {
        if self.session.as_ref().map(|x| x.info.id == s.session_id).unwrap_or(false) {
            return; // duplicate delivery
        }
        // Supersede: the old session's final `idle` report, no sessionless report in between.
        self.stop(now, false);
        let text = s.text;
        let title = if s.title.is_empty() { prefix16(&text, 120).to_string() } else { s.title };
        let sentences: Arc<[u32]> = if s.sentences.is_empty() { Arc::from(vec![0u32]) } else { s.sentences };
        let len16 = utf16_len(&text) as f64;
        let info = Arc::new(SessionInfo {
            id: s.session_id,
            read_id: s.read_id,
            text,
            title,
            voice_name: s.voice_name,
            voice_id: s.voice_id,
            source_app: s.source_app.filter(|a| !a.is_empty()),
            from_cache: s.from_cache,
            highlight_words: s.highlight_words,
            sentences,
        });
        let est = if s.estimated_duration_sec.is_finite() && s.estimated_duration_sec > 0.0 {
            s.estimated_duration_sec
        } else {
            len16 / ESTIMATED_CHARS_PER_SECOND
        };
        self.serial += 1;
        tracing::info!(target: "audio", player = self.cfg.name, session = %info.id, "session started");
        self.session = Some(Session::new(self.serial, info, est));
        self.cur = 0;
        self.want_play = true;
        self.waiting = true;
        self.clock.reset();
        self.audible = false;
        self.error = None;
        self.element_playing = false;
        self.gate = None;
        if s.rate.is_finite() {
            self.rate = s.rate.clamp(RATE_MIN, RATE_MAX);
        }
        if s.volume.is_finite() {
            self.volume = s.volume.clamp(0.0, 1.0);
            if let Some(out) = self.out.as_ref() {
                out.set_volume(self.volume as f32);
            }
        }
        // Start waking the device now, so it is up by the time the first chunk arrives.
        let _ = self.prepare_output(now);
        self.set_status(PlayerStatus::Loading, true, now);
    }

    fn add_chunk(&mut self, c: TtsChunk, now: f64) {
        let Some(s) = self.session.as_mut() else { return };
        if s.info.id != c.session_id {
            return;
        }
        let idx = c.index;
        if (idx as usize) < s.len() || s.pending.contains_key(&idx) {
            return; // duplicate
        }
        s.pending.insert(idx, c);
        while let Some(next) = s.pending.remove(&(s.len() as u32)) {
            s.push_chunk(next);
        }
        let len = s.len();
        if self.status == PlayerStatus::Error {
            self.changed(now);
            return;
        }
        let caught_up = self.waiting || self.status == PlayerStatus::Ended;
        if caught_up && self.cur < len {
            if self.status == PlayerStatus::Ended {
                self.want_play = true;
            }
            self.waiting = false;
            if self.want_play {
                self.play_current(now);
            } else {
                self.ensure_audio(self.cur);
                self.set_status(PlayerStatus::Paused, false, now);
            }
        } else if self.cur + 1 < len {
            self.ensure_audio(self.cur + 1);
        }
        self.changed(now);
    }

    fn finish_session(&mut self, d: TtsSessionDone, now: f64) {
        let Some(s) = self.session.as_mut() else { return };
        if s.info.id != d.session_id {
            return;
        }
        s.timeline.done = true;
        let len = s.len();
        if self.status != PlayerStatus::Error && self.waiting && self.cur >= len {
            self.finish(now, false);
        }
        self.changed(now);
    }

    fn fail_session(&mut self, e: TtsSessionError, now: f64) {
        let Some(s) = self.session.as_mut() else { return };
        if s.info.id != e.session_id {
            return;
        }
        if !e.fatal {
            let message = if e.message.is_empty() { WARNING_DEFAULT.to_string() } else { e.message };
            s.warning = Some(SessionWarning { message, code: e.code });
            self.changed(now);
            return;
        }
        self.halt_audio(now);
        self.error = Some((if e.message.is_empty() { ERROR_DEFAULT.to_string() } else { e.message }, e.code));
        self.waiting = false;
        self.want_play = false;
        self.gate = None;
        self.release_output(now, IDLE_OUTPUT_HOLD_MS);
        self.set_status(PlayerStatus::Error, true, now);
    }

    // ───────────────────────────── transport (§4.6) ─────────────────────────────

    fn command(&mut self, cmd: PlayerCommand, now: f64) {
        match cmd {
            PlayerCommand::Toggle => {
                if matches!(self.status, PlayerStatus::Playing | PlayerStatus::Loading) {
                    self.pause(now);
                } else {
                    self.play(now);
                }
            }
            PlayerCommand::Play => self.play(now),
            PlayerCommand::Pause => self.pause(now),
            PlayerCommand::Stop => self.stop(now, true),
            PlayerCommand::Seek { position_sec } => {
                if position_sec.is_finite() {
                    self.seek(position_sec, now);
                }
            }
            PlayerCommand::SeekFraction { fraction } => {
                if fraction.is_finite() {
                    let duration = self.session.as_ref().map(|s| s.timeline.duration_sec()).unwrap_or(0.0);
                    self.seek(fraction.clamp(0.0, 1.0) * duration, now);
                }
            }
            PlayerCommand::Skip { direction, unit, amount } => {
                let dir = direction.sign();
                match unit {
                    SkipUnit::Sentence => self.skip_sentence(dir, now),
                    SkipUnit::Seconds => {
                        let amount = amount.filter(|a| a.is_finite() && *a > 0.0).unwrap_or(DEFAULT_SKIP_SEC);
                        self.skip_seconds(dir * amount, now);
                    }
                }
            }
            PlayerCommand::Rate { rate } => self.set_rate(rate, now),
            PlayerCommand::Volume { volume } => self.set_volume(volume, now),
        }
    }

    fn play(&mut self, now: f64) {
        if self.session.is_none() || self.status == PlayerStatus::Error {
            return;
        }
        self.want_play = true;
        if self.status == PlayerStatus::Ended {
            self.seek(0.0, now); // "Play again"
            return;
        }
        let len = self.session.as_ref().map(|s| s.len()).unwrap_or(0);
        if self.waiting || self.cur >= len {
            self.waiting = true;
            let _ = self.prepare_output(now);
            self.set_status(PlayerStatus::Loading, false, now);
            self.changed(now);
            return;
        }
        self.play_current(now);
        self.changed(now);
    }

    fn pause(&mut self, now: f64) {
        if self.session.is_none() {
            return;
        }
        self.want_play = false;
        if matches!(self.status, PlayerStatus::Playing | PlayerStatus::Loading) {
            // Resume restarts from what was actually heard.
            self.halt_audio(now);
            self.set_status(PlayerStatus::Paused, false, now);
            self.release_output(now, PAUSED_OUTPUT_HOLD_MS);
        }
        self.changed(now);
    }

    /// `stop()`; `explicit` = a Stop command (adds the sessionless report), not a supersede.
    fn stop(&mut self, now: f64, explicit: bool) {
        if self.session.is_none() {
            if self.status != PlayerStatus::Idle {
                self.set_status(PlayerStatus::Idle, true, now);
            }
            return;
        }
        self.halt_audio(now);
        self.waiting = false;
        self.gate = None;
        self.release_output(now, IDLE_OUTPUT_HOLD_MS);
        // Reported with the session still attached: the final listenedSec for it.
        self.set_status(PlayerStatus::Idle, true, now);
        if let Some(s) = self.session.as_ref() {
            tracing::info!(target: "audio", player = self.cfg.name, session = %s.info.id, "session stopped");
        }
        self.session = None;
        self.error = None;
        self.cur = 0;
        self.clock.reset();
        self.audible = false;
        self.next_report_ms = None;
        if explicit {
            self.report(now);
        }
    }

    fn seek(&mut self, sec: f64, now: f64) {
        let Some(s) = self.session.as_ref() else { return };
        if self.status == PlayerStatus::Error || s.len() == 0 {
            return;
        }
        let (i, offset) = s.timeline.locate(sec);
        if self.status == PlayerStatus::Ended {
            self.want_play = true; // seeking after the end resumes playback
        }
        // Speech was streaming: the output is awake, so the restart is never gated on the warmer (§4.6 "seeking
        // while it plays … no warm-up gate, no status change"; the original's element was simply not paused).
        let streaming = self.status == PlayerStatus::Playing && self.element_playing;
        // A jump never counts as listening: bank, stop the current audio.
        self.halt_audio(now);
        if i != self.cur {
            let old = self.cur;
            self.release_audio(old, now);
            self.cur = i;
        }
        if let Some(s) = self.session.as_mut() {
            for (k, e) in s.entries.iter_mut().enumerate() {
                if k != i && k != i + 1 {
                    e.decoded = None;
                }
            }
            if let Some(e) = s.entries.get_mut(i) {
                e.offset = offset;
            }
        }
        self.waiting = false;
        self.ensure_audio(i);
        if self.want_play {
            self.play_current_gated(now, !streaming);
        } else {
            self.set_status(PlayerStatus::Paused, false, now);
        }
        self.changed(now);
    }

    fn position_now(&self) -> f64 {
        let Some(s) = self.session.as_ref() else { return 0.0 };
        s.timeline.position_sec(self.cur, self.media_now(), self.status == PlayerStatus::Ended)
    }

    fn skip_seconds(&mut self, delta: f64, now: f64) {
        if self.session.is_some() {
            let target = self.position_now() + delta;
            self.seek(target, now);
        }
    }

    fn skip_sentence(&mut self, dir: f64, now: f64) {
        let Some(s) = self.session.as_ref() else { return };
        if self.status == PlayerStatus::Error || s.len() == 0 {
            return;
        }
        if dir > 0.0 && self.status == PlayerStatus::Ended {
            return;
        }
        if s.partial_text {
            self.skip_seconds(dir * PARTIAL_SKIP_SEC, now);
            return;
        }
        let pos = self.position_now();
        let times: Vec<f64> = s.info.sentences.iter().map(|ch| s.timeline.time_at_char(*ch)).collect();
        if dir > 0.0 {
            match times.iter().copied().find(|t| *t > pos + 0.05) {
                Some(t) if t.is_finite() => self.seek(t, now),
                _ => {
                    if s.timeline.done {
                        // "Next" past the last sentence ends the read.
                        self.finish(now, true);
                        self.changed(now);
                    } else {
                        let available = s.timeline.available_sec();
                        self.seek(available, now);
                    }
                }
            }
            return;
        }
        // 1.2 wall seconds expressed in audio seconds.
        let grace = BACK_GRACE_SEC * self.rate;
        let mut target = 0.0;
        for t in times {
            if !t.is_finite() || t > pos - grace {
                break;
            }
            target = t;
        }
        self.seek(target, now);
    }

    fn set_rate(&mut self, rate: f64, now: f64) {
        if !rate.is_finite() {
            return;
        }
        let r = rate.clamp(RATE_MIN, RATE_MAX);
        if r == self.rate {
            return;
        }
        let streaming = self.status == PlayerStatus::Playing && self.element_playing && self.feeder.is_active();
        if streaming {
            // Continue right after the audio already handed to the device, so the switch is seamless; the
            // listening so far is banked at the old rate.
            let resume = self.out.as_ref().and_then(|o| o.consumed());
            self.halt_audio(now);
            self.rate = r;
            let (chunk, sec) = self.resume_point(resume);
            self.start_feed(chunk, sec, now);
            self.element_playing = true;
        } else {
            self.clock.credit(now, self.media_now(), self.rate);
            self.rate = r;
        }
        self.changed(now);
    }

    fn set_volume(&mut self, volume: f64, now: f64) {
        if !volume.is_finite() {
            return;
        }
        let v = volume.clamp(0.0, 1.0);
        if v == self.volume {
            return;
        }
        self.volume = v;
        if let Some(out) = self.out.as_ref() {
            out.set_volume(v as f32);
        }
        self.changed(now);
    }

    // ───────────────────────────── chunk lifecycle (§4.7) ─────────────────────────────

    fn ensure_audio(&mut self, i: usize) {
        if let Some(s) = self.session.as_mut() {
            s.request_decode(i, self.decoder.as_mut());
        }
    }

    /// `releaseAudio`: banks listening first when it is the current chunk, then drops the PCM.
    fn release_audio(&mut self, i: usize, now: f64) {
        if i == self.cur {
            self.set_audible(false, now);
        }
        if let Some(e) = self.session.as_mut().and_then(|s| s.entries.get_mut(i)) {
            e.decoded = None;
        }
    }

    fn play_current(&mut self, now: f64) {
        self.play_current_gated(now, true);
    }

    /// `playCurrent()`; `gate = false` when the start replaces speech that was streaming (the output is awake).
    fn play_current_gated(&mut self, now: f64, gate: bool) {
        let Some(s) = self.session.as_ref() else { return };
        let Some(c) = s.timeline.chunks.get(self.cur) else { return };
        if c.failed {
            self.advance(self.cur + 1, now);
            return;
        }
        let serial = s.serial;
        self.ensure_audio(self.cur);
        self.start_token += 1;
        let token = self.start_token;
        let ready = self.prepare_output(now);
        if gate && ready == WarmState::Waiting && !self.element_playing {
            // "STARTING": the audio is ready, the output is still waking up.
            self.gated_token = Some(token);
            self.gate = Some(Gate { token, serial, chunk: self.cur });
            self.set_status(PlayerStatus::Loading, false, now);
            return;
        }
        self.start_element(now);
    }

    fn start_element(&mut self, now: f64) {
        if self.session.is_none() {
            return;
        }
        self.gate = None;
        if !(self.element_playing && self.feeder.covers(self.cur)) {
            let offset = self.session.as_ref().and_then(|s| s.entries.get(self.cur)).map(|e| e.offset).unwrap_or(0.0);
            self.start_feed(self.cur, offset, now);
        }
        self.element_playing = true;
        self.set_status(PlayerStatus::Playing, false, now);
        self.ensure_audio(self.cur + 1);
        self.pump();
    }

    /// Chunk `i` failed to decode: it keeps its timeline slot and is skipped.
    fn on_chunk_error(&mut self, i: usize, now: f64) {
        let Some(s) = self.session.as_mut() else { return };
        let Some(c) = s.timeline.chunks.get_mut(i) else { return };
        c.failed = true;
        self.release_audio(i, now);
        if i == self.cur && (self.status == PlayerStatus::Playing || self.want_play) {
            self.advance(i + 1, now);
        } else {
            self.changed(now);
        }
    }

    fn on_chunk_ended(&mut self, i: usize, now: f64) {
        if i != self.cur {
            return;
        }
        if let Some(e) = self.session.as_mut().and_then(|s| s.entries.get_mut(i)) {
            e.offset = 0.0;
        }
        self.release_audio(i, now);
        self.advance(i + 1, now);
    }

    fn advance(&mut self, next: usize, now: f64) {
        self.set_audible(false, now);
        let Some(s) = self.session.as_mut() else { return };
        let len = s.len();
        let mut cur = next;
        while cur < len && s.timeline.chunks[cur].failed {
            cur += 1;
        }
        self.cur = cur;
        for e in s.entries.iter_mut().take(cur) {
            e.decoded = None;
        }
        if cur < len {
            self.waiting = false;
            s.entries[cur].offset = 0.0;
            if !self.feeder.covers(cur) {
                // The previous chunk's audio is finished; this one starts afresh.
                self.element_playing = false;
            }
            self.ensure_audio(cur);
            if self.want_play {
                self.play_current(now);
            } else {
                self.set_status(PlayerStatus::Paused, false, now);
            }
            self.changed(now);
            return;
        }
        if s.timeline.done {
            self.finish(now, false);
            self.changed(now);
            return;
        }
        // Caught up with synthesis.
        self.waiting = true;
        self.element_playing = false;
        self.feeder.stop();
        if self.want_play {
            self.set_status(PlayerStatus::Loading, false, now);
        }
        self.changed(now);
    }

    /// `finish()`: everything played (`explicit` = ended by a skip while audio may still be queued).
    fn finish(&mut self, now: f64, explicit: bool) {
        if explicit {
            self.halt_audio(now);
        } else {
            // The drained tail plays out; nothing to flush.
            self.set_audible(false, now);
            self.feeder.stop();
            self.element_playing = false;
        }
        if let Some(s) = self.session.as_mut() {
            for e in s.entries.iter_mut() {
                e.decoded = None;
            }
            self.cur = s.len();
        }
        self.waiting = false;
        self.want_play = false;
        self.gate = None;
        self.release_output(now, IDLE_OUTPUT_HOLD_MS);
        self.set_status(PlayerStatus::Ended, false, now);
    }

    // ───────────────────────────── playhead and listening (§4.9) ─────────────────────────────

    fn played_tag(&self) -> Option<crate::output::BlockTag> {
        self.out.as_ref().and_then(|o| o.played())
    }

    /// Media time (source seconds) of the current chunk, from the device-reported played block only.
    fn media_now(&self) -> f64 {
        let Some(s) = self.session.as_ref() else { return 0.0 };
        let Some(e) = s.entries.get(self.cur) else { return 0.0 };
        if let Some(tag) = self.played_tag() {
            let chunk = tag.chunk as usize;
            if chunk == self.cur {
                if let Some(sr) = e.decoded_rate.filter(|r| *r > 0) {
                    return (tag.source_frame_end / sr as f64).max(0.0);
                }
            } else if chunk > self.cur {
                return s.timeline.dur(self.cur);
            }
        }
        e.offset
    }

    fn set_audible(&mut self, audible: bool, now: f64) {
        if self.audible == audible {
            return;
        }
        self.audible = audible;
        self.sync_clock(now);
    }

    fn sync_clock(&mut self, now: f64) {
        let len = self.session.as_ref().map(|s| s.len()).unwrap_or(0);
        let running = self.status == PlayerStatus::Playing && self.audible && self.cur < len;
        let media = self.media_now();
        self.clock.sync(running, now, media, self.rate);
    }

    fn update_playhead(&mut self, now: f64) {
        if let Some(out) = self.out.as_mut() {
            out.update(now);
        }
        if self.session.is_none() {
            return;
        }
        // Chunk ends: the device has played past the current chunk (gapless: into the next one).
        for _ in 0..64 {
            if self.status != PlayerStatus::Playing || !self.element_playing {
                break;
            }
            let Some(tag) = self.played_tag() else { break };
            let cur = self.cur;
            let chunk = tag.chunk as usize;
            let frames = self.session.as_ref().and_then(|s| s.entries.get(cur)).and_then(|e| e.decoded_frames);
            let ended =
                chunk > cur || (chunk == cur && frames.map(|f| tag.source_frame_end >= f as f64).unwrap_or(false));
            if !ended {
                break;
            }
            self.on_chunk_ended(cur, now);
            if self.cur == cur {
                break;
            }
        }
        // "playing": the device has played blocks of the current generation (D-RP-10).
        if self.status == PlayerStatus::Playing && self.element_playing && !self.audible {
            if let Some(tag) = self.played_tag() {
                if tag.chunk as usize >= self.cur {
                    self.set_audible(true, now);
                }
            }
        }
    }

    /// Records the heard position as the current chunk's offset, banks the listening clock, stops the feed and
    /// flushes the output.
    fn halt_audio(&mut self, now: f64) {
        if self.feeder.is_active() || self.element_playing {
            let t = self.media_now();
            let cur = self.cur;
            if let Some(s) = self.session.as_mut() {
                let dur = s.timeline.dur(cur);
                if let Some(e) = s.entries.get_mut(cur) {
                    e.offset = t.clamp(0.0, dur.max(0.0));
                }
            }
        }
        self.set_audible(false, now);
        self.feeder.stop();
        if let Some(s) = self.session.as_mut() {
            s.feeder_wants = None;
        }
        let generation = self.next_generation();
        if let Some(out) = self.out.as_mut() {
            out.set_feeding(false);
            out.start_generation(generation);
        }
        self.element_playing = false;
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn start_feed(&mut self, chunk: usize, start_sec: f64, now: f64) {
        self.set_audible(false, now);
        let generation = self.next_generation();
        if let Some(out) = self.out.as_mut() {
            out.start_generation(generation);
        }
        self.feeder.start(generation, chunk, start_sec, self.rate);
    }

    /// Where to continue after a rate change: exactly where the device was handed audio (interpolated inside the block
    /// it is in, so nothing is replayed or skipped; the 2 ms fade-out overlaps the fade-in), else the heard position.
    fn resume_point(&self, consumed: Option<crate::output::BlockTag>) -> (usize, f64) {
        let offset = self.session.as_ref().and_then(|s| s.entries.get(self.cur)).map(|e| e.offset).unwrap_or(0.0);
        let Some(tag) = consumed else { return (self.cur, offset) };
        let chunk = tag.chunk as usize;
        if chunk < self.cur {
            return (self.cur, offset);
        }
        let rate = self.session.as_ref().and_then(|s| s.entries.get(chunk)).and_then(|e| e.decoded_rate);
        match rate {
            Some(sr) if sr > 0 => (chunk, tag.source_frame_end / sr as f64),
            _ => (self.cur, offset),
        }
    }

    /// After an output change: restart the current chunk's feed from the heard position.
    fn restart_heard_feed(&mut self, now: f64) {
        if self.status != PlayerStatus::Playing || self.session.is_none() {
            return;
        }
        let offset = self.session.as_ref().and_then(|s| s.entries.get(self.cur)).map(|e| e.offset).unwrap_or(0.0);
        if let Some(out) = self.out.as_mut() {
            if !out.is_running() {
                out.resume();
            }
        }
        self.start_feed(self.cur, offset, now);
        self.element_playing = true;
        self.pump();
    }

    fn pump(&mut self) {
        if let (Some(out), Some(session)) = (self.out.as_mut(), self.session.as_mut()) {
            if self.feeder.is_active() {
                let mut src = SessionSource { session, decoder: self.decoder.as_mut() };
                self.feeder.pump(out.as_mut(), &mut src);
            }
        }
        // Set after the blocks are queued: an empty ring from here on is an under-run (ARCHITECTURE.md §5.2
        // `waiting`), not the start or the drained end of a feed.
        let feeding = self.status == PlayerStatus::Playing && self.element_playing && self.feeder.is_feeding();
        if let Some(out) = self.out.as_ref() {
            out.set_feeding(feeding);
        }
    }

    /// Logs ring under-runs while a read plays (D-RP-10: "stalls cannot occur except on under-run (logged)"),
    /// aggregated and at most every `UNDERRUN_LOG_MS`. Listening needs no correction: the clock is capped by the
    /// device-reported media progress, which an under-run does not advance.
    fn check_underruns(&mut self, now: f64) {
        let Some(out) = self.out.as_ref() else { return };
        let u = out.underruns();
        let log = &mut self.underruns;
        let frames = u.frames.saturating_sub(log.seen.frames);
        let xruns = u.xruns.saturating_sub(log.seen.xruns);
        log.seen = u;
        if self.status == PlayerStatus::Playing && (frames > 0 || xruns > 0) {
            log.pending_frames += frames;
            log.pending_xruns += xruns;
            log.total_frames += frames;
        }
        if (log.pending_frames > 0 || log.pending_xruns > 0) && now >= log.next_log_ms {
            let ms = log.pending_frames as f64 * 1000.0 / out.sample_rate().max(1) as f64;
            tracing::warn!(
                target: "audio",
                player = self.cfg.name,
                silent_ms = ms.round() as u64,
                xruns = log.pending_xruns,
                "audio under-run while playing"
            );
            log.pending_frames = 0;
            log.pending_xruns = 0;
            log.next_log_ms = now + UNDERRUN_LOG_MS;
        }
    }

    /// The stall watchdog (reader-playback.md §12 #28): a running real output that renders nothing for `STALL_MS`
    /// while a read plays is reopened; a second stall within `STALL_REPEAT_MS` moves the read to the simulated clock
    /// so it still progresses and ends. (Only the device's own consumption counts: late playback reports — a device
    /// with a huge reported latency — delay the position but are not a stall.)
    fn watch_progress(&mut self, now: f64) {
        let Some(out) = self.out.as_ref() else {
            self.watch = None;
            return;
        };
        let watching = self.kind == OutKind::Real
            && self.status == PlayerStatus::Playing
            && self.element_playing
            && out.is_running();
        if !watching {
            self.watch = None;
            return;
        }
        let rendered = out.rendered();
        let w = self.watch.get_or_insert(Watch { since: now, last_tick: now, rendered });
        let gap = now - w.last_tick > STALL_GAP_MS;
        w.last_tick = now;
        if rendered != w.rendered || gap {
            *w = Watch { since: now, last_tick: now, rendered };
            return;
        }
        if now - w.since < STALL_MS {
            return;
        }
        self.watch = None;
        self.on_stall(now);
    }

    fn on_stall(&mut self, now: f64) {
        let repeated = self.last_stall_ms.is_some_and(|t| now - t < STALL_REPEAT_MS);
        self.last_stall_ms = Some(now);
        if !repeated {
            tracing::warn!(target: "audio", player = self.cfg.name, "the audio output stopped making progress");
            self.reopen(now, "stalled");
            return;
        }
        tracing::warn!(
            target: "audio",
            player = self.cfg.name,
            "the audio output stalled again; playing on a simulated clock"
        );
        let device = self.out.as_ref().and_then(|o| o.device_id().map(str::to_string));
        let resume_feed = self.status == PlayerStatus::Playing && self.element_playing;
        self.drop_output(now);
        // No new stream for a minute (each one could stall again), unless the default device changes.
        self.warmer.open_failed(now);
        self.stalled_device = Some((device, now));
        self.no_device_logged = true;
        self.use_sim_no_device(now);
        if resume_feed {
            self.restart_heard_feed(now);
        }
    }

    // ───────────────────────────── output and warmer (§6, §5.7–§5.9) ─────────────────────────────

    fn sim() -> Box<dyn Sink> {
        Box::new(SimOutput::new(SIM_RATE, SIM_CHANNELS))
    }

    /// Makes `out` the output. It starts in a fresh generation, so nothing fed for an earlier output can play on it;
    /// a feed still running (one started while there was no output) restarts on it from the heard position through
    /// the usual paths (`restart_heard_feed`, `play_current` → `start_element`).
    fn install_output(&mut self, mut out: Box<dyn Sink>, kind: OutKind, now: f64) {
        if self.feeder.is_active() {
            self.feeder.stop();
            self.element_playing = false;
        }
        let generation = self.next_generation();
        out.start_generation(generation);
        out.set_volume(self.volume as f32);
        out.update(now);
        self.out = Some(out);
        self.kind = kind;
        self.watch = None;
        // A new output counts its under-runs from zero.
        self.underruns.seen = Underruns::default();
    }

    /// Drops the output after recording the heard position and stopping the feed (`halt_audio`).
    fn drop_output(&mut self, now: f64) {
        if self.out.is_some() {
            self.halt_audio(now);
        }
        self.out = None;
        self.kind = OutKind::None;
        self.watch = None;
    }

    fn use_sim_no_device(&mut self, now: f64) {
        if !self.no_device_logged {
            tracing::warn!(target: "audio", player = self.cfg.name, "no audio output device; playing on a simulated clock");
            self.no_device_logged = true;
        }
        let mut sim = Self::sim();
        sim.resume();
        self.install_output(sim, OutKind::SimNoDevice, now);
        self.next_poll_ms = Some(now + DEVICE_POLL_MS);
    }

    fn record_stream_failure(&mut self, now: f64) {
        self.stream_failures.retain(|t| now - *t < FAILURE_WINDOW_MS);
        self.stream_failures.push(now);
        if self.stream_failures.len() >= FAILURE_BUDGET {
            tracing::error!(target: "audio", player = self.cfg.name, "the audio output failed {FAILURE_BUDGET} times in 10 s");
            self.exit = Some(Exit::OutputFailed);
        }
    }

    /// Opens the real output (none may be installed); `NoDevice` → simulated clock; `Stream` → counted, retried
    /// after 1 s.
    fn open_real(&mut self, now: f64) {
        match self.factory.open() {
            Ok(out) => {
                tracing::info!(target: "audio", player = self.cfg.name, rate = out.sample_rate(), channels = out.channels(), "audio output opened");
                out.set_keep_alive(self.cfg.warm_output);
                self.install_output(out, OutKind::Real, now);
                self.warmer.opened();
                self.retry_open_ms = None;
                self.no_device_logged = false;
                self.next_poll_ms = Some(now + DEVICE_POLL_MS);
            }
            Err(OutputError::NoDevice) => {
                self.warmer.open_failed(now);
                self.use_sim_no_device(now);
            }
            Err(OutputError::Stream(message)) => {
                tracing::warn!(target: "audio", player = self.cfg.name, "opening the audio output failed: {message}");
                self.out = None;
                self.kind = OutKind::None;
                self.retry_open_ms = Some(now + STREAM_RETRY_MS);
                self.record_stream_failure(now);
            }
        }
    }

    fn ensure_output(&mut self, now: f64) {
        if self.kind == OutKind::SimNoDevice
            && self.warmer.may_open(now)
            && !(self.status == PlayerStatus::Playing && self.element_playing)
        {
            // Creation is retried at most every 60 s (each try can block); the 2 s poll covers playing sessions. A
            // feed may still be running on the simulated clock (the next chunk being decoded): it stops here and
            // restarts on the new output, in a generation of its own.
            self.drop_output(now);
        }
        if let Some(out) = self.out.as_ref() {
            if out.lost().is_some() {
                self.reopen(now, "lost");
            }
            return;
        }
        if self.cfg.null_output {
            self.install_output(Self::sim(), OutKind::SimNull, now);
            return;
        }
        if self.retry_open_ms.map(|t| now < t).unwrap_or(false) {
            return;
        }
        if !self.warmer.may_open(now) {
            // Creation failed less than 60 s ago (no device): no wait, simulated clock.
            self.use_sim_no_device(now);
            return;
        }
        self.open_real(now);
    }

    /// The device went away, the default changed or the stream stalled: reopen and continue from the heard position.
    fn reopen(&mut self, now: f64, why: &str) {
        tracing::info!(target: "audio", player = self.cfg.name, "audio device {why}; reopening the output");
        let held = self.out.as_ref().map(|o| o.is_running()).unwrap_or(false);
        let resume_feed = self.status == PlayerStatus::Playing && self.element_playing;
        self.drop_output(now);
        self.open_real(now);
        if let Some(out) = self.out.as_mut() {
            if held || resume_feed {
                out.resume();
            }
        }
        if resume_feed && self.out.is_some() {
            self.restart_heard_feed(now);
        }
    }

    fn check_devices(&mut self, now: f64) {
        match self.kind {
            OutKind::Real => {
                let Some(out) = self.out.as_ref() else { return };
                if let Some(no_device) = out.lost() {
                    self.reopen(now, if no_device { "removed" } else { "lost" });
                    return;
                }
                if out.is_running() && self.next_poll_ms.map(|t| now >= t).unwrap_or(true) {
                    self.next_poll_ms = Some(now + DEVICE_POLL_MS);
                    let current = out.device_id().map(str::to_string);
                    if let Some(id) = self.factory.default_device_id().filter(|id| !id.is_empty()) {
                        if current.as_deref().is_some_and(|c| !c.is_empty() && c != id) {
                            self.reopen(now, "changed");
                        }
                    }
                }
            }
            OutKind::SimNoDevice => {
                if self.session.is_some() && self.next_poll_ms.map(|t| now >= t).unwrap_or(true) {
                    self.next_poll_ms = Some(now + DEVICE_POLL_MS);
                    if let Some(id) = self.factory.default_device_id() {
                        // A device that stalled twice is left alone for a minute, unless the default changed.
                        let held_off = self.stalled_device.as_ref().is_some_and(|(device, at)| {
                            now - at < STALL_REPEAT_MS && device.as_deref().is_none_or(|d| d == id)
                        });
                        if !held_off {
                            self.reopen(now, "appeared");
                        }
                    }
                }
            }
            OutKind::None | OutKind::SimNull => {}
        }
    }

    /// `prepareOutput()`: make sure the stream exists and runs, cancel a pending release, maybe wait for warm-up.
    fn prepare_output(&mut self, now: f64) -> WarmState {
        self.ensure_output(now);
        let warm = self.cfg.warm_output;
        let Some(out) = self.out.as_mut() else {
            // No output at all (a stream failure, retried): nothing to wait for.
            self.warmer.cancel_release();
            return WarmState::Ready;
        };
        if !out.is_running() {
            out.resume();
        }
        if !warm || out.is_simulated() {
            self.warmer.cancel_release();
            return WarmState::Ready;
        }
        self.warmer.prepare(now, out.rendered(), out.sample_rate(), out.is_running())
    }

    fn poll_warmer(&mut self, now: f64) -> WarmState {
        match self.out.as_ref() {
            Some(out) if self.cfg.warm_output && !out.is_simulated() => {
                self.warmer.poll(now, out.rendered(), out.sample_rate(), out.is_running())
            }
            _ => WarmState::Ready,
        }
    }

    fn release_output(&mut self, now: f64, after_ms: u64) {
        if self.out.is_some() {
            self.warmer.release(now, after_ms);
        }
    }

    fn preload(&mut self, now: f64) {
        if self.cfg.null_output || self.out.is_some() {
            return;
        }
        // Builds the stream while idle and leaves it suspended (the first read does not pay for it).
        self.ensure_output(now);
    }

    // ───────────────────────────── reporting (§4.10) ─────────────────────────────

    fn report_interval_ms(&self) -> f64 {
        (self.cfg.report_interval.as_secs_f64() * 1000.0).max(1.0)
    }

    fn set_status(&mut self, next: PlayerStatus, force: bool, now: f64) {
        if next == self.status && !force {
            return;
        }
        self.status = next;
        self.sync_clock(now);
        if next == PlayerStatus::Playing {
            if self.next_report_ms.is_none() {
                self.next_report_ms = Some(now + self.report_interval_ms());
            }
        } else {
            self.next_report_ms = None;
        }
        self.report(now);
    }

    fn changed(&mut self, now: f64) {
        self.report(now);
    }

    fn report(&mut self, now: f64) {
        self.publish(now);
        let snapshot = {
            let p = self.shared.published.lock();
            snapshot_from(&p, now)
        };
        self.emit(PlayerEvent::State(snapshot));
    }

    fn emit(&self, event: PlayerEvent) {
        // Reporting must never break playback.
        if catch_unwind(AssertUnwindSafe(|| (self.events)(event))).is_err() {
            tracing::error!(target: "audio", player = self.cfg.name, "player event listener panicked");
        }
    }

    pub(crate) fn publish(&self, now: f64) {
        let t = self.media_now();
        let listened = self.clock.listened_sec(now, t, self.rate);
        let mut p = self.shared.published.lock();
        p.status = self.status;
        if p.rate_pending == 0 {
            p.rate = self.rate;
        }
        if p.volume_pending == 0 {
            p.volume = self.volume;
        }
        p.buffering = self.waiting;
        p.starting =
            self.status == PlayerStatus::Loading && !self.waiting && self.gated_token == Some(self.start_token);
        p.error = self.error.clone();
        let Some(s) = self.session.as_ref() else {
            p.session = None;
            p.wants_play = false;
            p.warning = None;
            p.partial_text = false;
            p.listened_sec = 0.0;
            p.extrapolate = false;
            return;
        };
        if p.session.as_ref().map(|x| !Arc::ptr_eq(x, &s.info)).unwrap_or(true) {
            p.session = Some(s.info.clone());
        }
        p.wants_play = self.want_play && !matches!(self.status, PlayerStatus::Ended | PlayerStatus::Error);
        if p.warning != s.warning {
            p.warning = s.warning.clone();
        }
        p.partial_text = s.partial_text;
        p.listened_sec = listened;
        let tl = &s.timeline;
        let n = tl.chunks.len();
        p.t = t;
        p.at_ms = now;
        p.extrapolate = self.status == PlayerStatus::Playing && self.audible;
        p.ended = self.status == PlayerStatus::Ended;
        p.position_base = tl.chunk_start(self.cur.min(n));
        p.cur_exists = self.cur < n;
        p.cur_dur = tl.dur(self.cur);
        p.cur_text = tl.chunks.get(self.cur).map(|c| (c.text_start, c.text_end)).unwrap_or((0, 0));
        p.cur_alignment = s.entries.get(self.cur).and_then(|e| e.alignment.clone());
        p.prev_text_end = if self.cur > 0 {
            tl.chunks.get(self.cur - 1).or(tl.chunks.last()).map(|c| c.text_end).unwrap_or(0)
        } else {
            0
        };
        p.available = tl.available_sec();
        p.duration = tl.duration_sec();
        p.done = tl.done;
        p.buffered = tl.buffered_fraction();
        p.text_len = tl.text_len;
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> PlayerStatus {
        self.status
    }

    #[cfg(test)]
    pub(crate) fn output_kind_is_sim_no_device(&self) -> bool {
        self.kind == OutKind::SimNoDevice
    }

    #[cfg(test)]
    pub(crate) fn output_mut(&mut self) -> Option<&mut Box<dyn Sink>> {
        self.out.as_mut()
    }

    /// Under-run frames counted while playing so far.
    #[cfg(test)]
    pub(crate) fn underrun_frames(&self) -> u64 {
        self.underruns.total_frames
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
