//! The client of one add-on's helper process (docs/addons-design.md "Helper protocol", "Timeouts", "Crash and restart
//! policy").
//!
//! - **One process, started lazily.** The first request spawns the helper through [`vox_win::process::Spawner`]
//!   (contained: it cannot outlive Vox) and runs the `hello` handshake; later requests reuse it until it crashes, is
//!   quit as idle, or the add-on is removed or replaced.
//! - **One request at a time, first come first served.** Callers queue for their turn ([`Turnstile`]); a caller whose
//!   read is cancelled while it waits leaves the queue at once.
//! - **Threads per process:** `vox-addon-<id>-out` reads stdout lines into a channel, `-in` writes queued requests (so
//!   a helper that stops reading never blocks a Vox thread), `-err` drains stderr into the log.
//! - **Deadlines.** A pending request fails when no frame for it arrives for `idle` (any frame, `progress` included,
//!   resets it) or after `synth_max`. Until the helper has returned audio once (it loads its model on the first
//!   `synthesize`), `load` is added to the first idle window and to the absolute limit. The wait loop ticks every
//!   50 ms to check the read's cancel token.
//! - **Only the pending request's frames are kept.** The stdout thread drops every frame that is not for the request
//!   being waited for (late answers, unsolicited output), so an idle helper cannot make Vox hold its output.
//! - **Cancel** sends `cancel{target}` and waits up to the grace for the target's last frame; a helper that does not
//!   wind down in time is killed (the next call respawns it).
//! - **Crashes** (stdout EOF, exit, a protocol violation) fail the request as retryable, so the engine's retry respawns
//!   the helper. Crashes, violations and timeouts count toward the budget: 3 within 10 minutes mark the add-on Broken,
//!   and it fails at once, without spawning, until it is repaired or Vox restarts.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use parking_lot::{Condvar, Mutex};
use vox_core::addons::{parse_voices, ManifestVoice, Timeouts};
use vox_core::constants::{
    ADDON_CANCEL_GRACE_MS, ADDON_CRASH_LIMIT, ADDON_CRASH_WINDOW_SEC, ADDON_MAX_LINE, ADDON_MAX_REQUEST_LINE, ADDON_PROTOCOL_MAX,
    ADDON_PROTOCOL_MIN, ADDON_VOICES_TIMEOUT_MS, APP_VERSION,
};
use vox_core::read::ReadErrorCode;
use vox_tts::{CancelToken, SynthesisResult, TtsError};
use vox_win::process::{classify_spawn_error, ContainedChild, SpawnBlock, Spawner};

use crate::audio::build_result;
use crate::protocol::{encode, parse_helper_line, HelperBody, HelperMsg, HostMsg, LineError, LineReader, OffsetUnit, ProtocolRange, SynthOptions};

/// How often a waiting request checks its cancel token and deadlines.
const TICK: Duration = Duration::from_millis(50);
/// Stderr: lines are cut to this many bytes; the first [`STDERR_BURST`] lines of a process are logged, then at most
/// one a second; the last [`STDERR_TAIL`] are kept for the crash log.
const STDERR_LINE: usize = 1024;
const STDERR_BURST: u32 = 200;
const STDERR_TAIL: usize = 64;

/// How to start one add-on's helper and how long to wait for it.
#[derive(Debug, Clone)]
pub struct HelperSpec {
    pub addon_id: String,
    /// The add-on's name, for the user-facing messages.
    pub name: String,
    pub version: String,
    /// Absolute path of the helper program (never searched on PATH).
    pub program: PathBuf,
    /// The version folder: the helper's working directory and `VOX_ADDON_DIR`.
    pub dir: PathBuf,
    /// `VOX_ADDON_DATA`, created before the spawn; `None` leaves it unset.
    pub data_dir: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub timeouts: HelperTimeouts,
    /// Cap on one helper → Vox line.
    pub max_line: usize,
}

/// The deadlines of [`HelperClient`] (docs/addons-design.md "Timeouts").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperTimeouts {
    pub start: Duration,
    pub load: Duration,
    pub idle: Duration,
    pub synth_max: Duration,
    pub voices: Duration,
    pub cancel_grace: Duration,
}

impl HelperTimeouts {
    /// A manifest's (already clamped) timeouts plus Vox's own.
    pub fn from_manifest(t: &Timeouts) -> Self {
        HelperTimeouts {
            start: Duration::from_millis(t.start_ms),
            load: Duration::from_millis(t.load_ms),
            idle: Duration::from_millis(t.idle_ms),
            synth_max: Duration::from_millis(t.synth_max_ms),
            voices: Duration::from_millis(ADDON_VOICES_TIMEOUT_MS),
            cancel_grace: Duration::from_millis(ADDON_CANCEL_GRACE_MS),
        }
    }
}

impl Default for HelperTimeouts {
    fn default() -> Self {
        HelperTimeouts::from_manifest(&Timeouts::default())
    }
}

impl HelperSpec {
    /// A spec with the default timeouts and line cap.
    pub fn new(addon_id: &str, name: &str, version: &str, program: PathBuf, dir: PathBuf) -> Self {
        HelperSpec {
            addon_id: addon_id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            program,
            dir,
            data_dir: None,
            args: Vec::new(),
            env: Vec::new(),
            timeouts: HelperTimeouts::default(),
            max_line: ADDON_MAX_LINE,
        }
    }
}

/// Why an add-on stopped being usable until it is repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokenReason {
    /// The crash budget ran out.
    KeepsStopping,
    /// The helper program is missing.
    Incomplete,
}

/// A request that did not produce what was asked for. [`HelperError::into_tts`] turns it into the engine's error with
/// Vox-written copy; nothing the helper wrote is ever shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperError {
    Cancelled,
    /// Exited, closed stdout or broke the protocol (retryable: the next call respawns it).
    Crashed,
    TimedOut,
    Broken(BrokenReason),
    /// The helper speaks no protocol version this Vox does.
    Incompatible,
    /// The program could not be started.
    Spawn(SpawnBlock),
    /// The add-on was removed while this request waited or ran.
    Removed,
    /// The helper answered with an error code (`unknown_voice`, `out_of_memory`, …).
    Reply(String),
    /// The audio was unusable (the helper is kept: the protocol was fine).
    BadAudio,
}

impl HelperError {
    /// The engine's error for this failure of add-on `name` (`version` is named when a voice is missing). Every one
    /// is `Unknown` (never `NoApiKey`, whose card talks about ElevenLabs) and only a crash is retryable.
    pub fn into_tts(&self, name: &str, version: &str) -> TtsError {
        let message = match self {
            HelperError::Cancelled => return TtsError::cancelled(),
            HelperError::Crashed => {
                let mut e = TtsError::new(ReadErrorCode::Unknown, format!("{name} stopped unexpectedly."));
                e.retryable = true;
                return e;
            }
            HelperError::TimedOut => format!("{name} took too long to respond."),
            HelperError::Broken(BrokenReason::KeepsStopping) => format!("{name} keeps stopping. Repair it on the Voices page."),
            HelperError::Broken(BrokenReason::Incomplete) => format!("{name} is incomplete. Repair it on the Voices page."),
            HelperError::Incompatible => format!("{name} needs an update."),
            HelperError::Spawn(SpawnBlock::Blocked) if cfg!(windows) => format!("Windows blocked {name} from starting."),
            HelperError::Spawn(SpawnBlock::Blocked) => format!("Your system blocked {name} from starting."),
            HelperError::Spawn(SpawnBlock::NoExec) if cfg!(windows) => format!("{name} isn’t built for this PC."),
            HelperError::Spawn(SpawnBlock::NoExec) => {
                format!("Your system blocked {name} from starting. Is the add-ons folder on a noexec drive?")
            }
            HelperError::Spawn(_) => format!("{name} couldn’t start."),
            HelperError::Removed => format!("{name} was removed."),
            HelperError::Reply(code) => match code.as_str() {
                "unknown_voice" => format!("This voice isn’t in {name} {version}."),
                "out_of_memory" => format!("Not enough memory for {name}."),
                "model_load_failed" => format!("{name} couldn’t load its voice model. Try repairing it."),
                _ => format!("{name} couldn’t read this text."),
            },
            HelperError::BadAudio => format!("{name} sent audio Vox couldn’t use."),
        };
        TtsError::new(ReadErrorCode::Unknown, message)
    }
}

/// Whether the add-on can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperHealth {
    Ready,
    Broken(BrokenReason),
    Incompatible,
    Removed,
}

/// What the stdout thread hands over.
enum Frame {
    Msg(HelperMsg),
    /// A protocol violation (the stream is not read any further).
    Violation(String),
    /// Stdout closed.
    End,
}

/// One running helper.
struct Proc {
    child: ContainedChild,
    /// Requests for the `-in` thread; dropping it closes the helper's stdin.
    tx: Option<Sender<Vec<u8>>>,
    rx: Receiver<Frame>,
    /// The id whose frames the stdout thread passes on (0: none).
    pending: Arc<AtomicU64>,
    /// The `hello` offset unit; `None` = one this Vox does not know (word cues are then not used).
    unit: Option<OffsetUnit>,
    /// Whether a `synthesize` has returned audio (until then the model may still be loading).
    loaded: bool,
    stderr: Arc<StderrTail>,
}

impl Proc {
    fn send(&self, line: Vec<u8>) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.send(line).is_ok())
    }

    /// Sends `line` after telling the stdout thread to pass on the frames of request `id`.
    fn ask(&self, id: u64, line: Vec<u8>) -> bool {
        self.pending.store(id, Ordering::SeqCst);
        self.send(line)
    }

    /// Kills the helper and logs the end of its stderr, for diagnosis.
    fn kill(mut self, addon: &str, why: &str) {
        self.tx = None;
        let _ = self.child.kill_tree();
        let tail = self.stderr.lines.lock().iter().cloned().collect::<Vec<_>>().join("\n");
        if tail.is_empty() {
            tracing::warn!(target: "addons", "{addon}: stopped the helper ({why})");
        } else {
            tracing::warn!(target: "addons", "{addon}: stopped the helper ({why}); its last stderr lines:\n{tail}");
        }
    }

    /// `quit`, stdin closed, up to `grace` to exit, then the tree is killed.
    fn quit(mut self, id: u64, grace: Duration) {
        self.send(encode(&HostMsg::Quit { id }));
        self.tx = None;
        let _ = self.child.wait_timeout(grace);
        let _ = self.child.kill_tree();
    }

    /// Drops frames left over from earlier requests; `false` when the helper has died meanwhile.
    fn drain(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(Frame::Msg(_)) => continue,
                Ok(Frame::Violation(_) | Frame::End) => return false,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return false,
            }
        }
        !matches!(self.child.try_exit_status(), Ok(Some(_)) | Err(_))
    }
}

#[derive(Default)]
struct Slot {
    proc: Option<Proc>,
    next_id: u64,
}

impl Slot {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// First come, first served turns at the helper, which a waiting caller can leave (cancelled read, removed add-on).
#[derive(Default)]
struct Turnstile {
    queue: Mutex<Queue>,
    changed: Condvar,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<u64>,
    busy: bool,
    next: u64,
}

struct Turn<'a>(&'a Turnstile);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.queue.lock().busy = false;
        self.0.changed.notify_all();
    }
}

impl Turnstile {
    /// Waits for this caller's turn; `Err` as soon as `give_up` says so.
    fn enter<E>(&self, mut give_up: impl FnMut() -> Option<E>) -> Result<Turn<'_>, E> {
        let mut q = self.queue.lock();
        let me = q.next;
        q.next += 1;
        q.waiting.push_back(me);
        loop {
            // Giving up wins over a turn that just came free: a removed add-on must not start a helper again.
            if let Some(e) = give_up() {
                q.waiting.retain(|&t| t != me);
                drop(q);
                self.changed.notify_all();
                return Err(e);
            }
            if !q.busy && q.waiting.front() == Some(&me) {
                q.waiting.pop_front();
                q.busy = true;
                return Ok(Turn(self));
            }
            self.changed.wait_for(&mut q, TICK);
        }
    }

    /// A turn only when nobody holds or waits for one.
    fn try_enter(&self) -> Option<Turn<'_>> {
        let mut q = self.queue.lock();
        if q.busy || !q.waiting.is_empty() {
            return None;
        }
        q.busy = true;
        Some(Turn(self))
    }
}

/// How a wait for a reply ended.
enum Outcome {
    /// The request's terminal frame (anything but `progress`).
    Reply(HelperBody),
    Violation(String),
    Ended,
    TimedOut,
    Cancelled,
    Removed,
}

/// The client of one add-on's helper. Thread-safe; requests are served one at a time in arrival order.
pub struct HelperClient {
    spec: HelperSpec,
    turns: Turnstile,
    slot: Mutex<Slot>,
    crashes: Mutex<VecDeque<Instant>>,
    health: Mutex<HelperHealth>,
    removed: AtomicBool,
    /// When the helper last finished a request (or was spawned), for the idle reaper.
    last_used: Mutex<Instant>,
    spawns: AtomicU64,
    on_health: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl HelperClient {
    pub fn new(spec: HelperSpec) -> Self {
        HelperClient {
            spec,
            turns: Turnstile::default(),
            slot: Mutex::new(Slot::default()),
            crashes: Mutex::new(VecDeque::new()),
            health: Mutex::new(HelperHealth::Ready),
            removed: AtomicBool::new(false),
            last_used: Mutex::new(Instant::now()),
            spawns: AtomicU64::new(0),
            on_health: OnceLock::new(),
        }
    }

    pub fn spec(&self) -> &HelperSpec {
        &self.spec
    }

    /// Called (on the failing request's thread) when the add-on turns Broken or Incompatible. Set once.
    pub fn set_on_health(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.on_health.set(f);
    }

    pub fn health(&self) -> HelperHealth {
        if self.removed.load(Ordering::SeqCst) {
            return HelperHealth::Removed;
        }
        *self.health.lock()
    }

    /// How many helper processes this client has started.
    pub fn spawn_count(&self) -> u64 {
        self.spawns.load(Ordering::SeqCst)
    }

    /// Whether a helper process is running now.
    pub fn is_running(&self) -> bool {
        self.slot.lock().proc.is_some()
    }

    /// Time since the helper last finished a request.
    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().elapsed()
    }

    /// Synthesizes `text` with the helper's voice `voice`.
    pub fn synthesize(&self, voice: &str, text: &str, word_timing: bool, cancel: &CancelToken) -> Result<SynthesisResult, HelperError> {
        self.usable()?;
        let _turn = self.turns.enter(|| self.give_up(cancel))?;
        // The request ahead may have used up the crash budget or found the helper incompatible.
        self.usable()?;
        let request = |id| encode(&HostMsg::Synthesize { id, voice, text, word_timing, options: SynthOptions {} });
        if request(u64::MAX).len() > ADDON_MAX_REQUEST_LINE {
            return Err(HelperError::Reply("text_too_long".into()));
        }
        let mut slot = self.slot.lock();
        self.ensure_running(&mut slot, cancel)?;
        let id = slot.next_id();
        let line = request(id);
        let proc = slot.proc.as_mut().expect("running");
        let extra = if proc.loaded { Duration::ZERO } else { self.spec.timeouts.load };
        let t = self.spec.timeouts;
        let outcome = if proc.ask(id, line) { self.await_reply(proc, id, t.idle, extra, t.synth_max + extra, cancel) } else { Outcome::Ended };
        let result = match outcome {
            Outcome::Reply(HelperBody::Audio(audio)) => {
                proc.loaded = true;
                let unit = proc.unit;
                build_result(text, audio, unit, word_timing).map_err(|e| {
                    tracing::warn!(target: "addons", "{}: unusable audio: {}", self.spec.addon_id, e.0);
                    HelperError::BadAudio
                })
            }
            Outcome::Reply(HelperBody::Error(e)) => {
                tracing::warn!(target: "addons", "{}: synthesize failed: {} ({})", self.spec.addon_id, e.code, cut(&e.message, 300));
                Err(HelperError::Reply(e.code))
            }
            Outcome::Reply(_) => Err(self.crashed(&mut slot, "it answered synthesize with another message")),
            other => Err(self.failed(&mut slot, id, other)),
        };
        *self.last_used.lock() = Instant::now();
        result
    }

    /// The helper's own voice list (for an add-on whose manifest lists none).
    pub fn voices(&self) -> Result<Vec<ManifestVoice>, HelperError> {
        self.usable()?;
        let cancel = CancelToken::new();
        let deadline = Instant::now() + self.spec.timeouts.voices;
        let _turn = self.turns.enter(|| if Instant::now() >= deadline { Some(HelperError::TimedOut) } else { self.give_up(&cancel) })?;
        self.usable()?;
        let mut slot = self.slot.lock();
        self.ensure_running(&mut slot, &cancel)?;
        let id = slot.next_id();
        let proc = slot.proc.as_mut().expect("running");
        let wait = self.spec.timeouts.voices;
        let outcome = if proc.ask(id, encode(&HostMsg::Voices { id })) { self.await_reply(proc, id, wait, Duration::ZERO, wait, &cancel) } else { Outcome::Ended };
        let result = match outcome {
            Outcome::Reply(HelperBody::Voices(list)) => parse_voices(&list).map_err(|e| {
                tracing::warn!(target: "addons", "{}: bad voice list: {e}", self.spec.addon_id);
                HelperError::Reply("bad_voices".into())
            }),
            Outcome::Reply(HelperBody::Error(e)) => Err(HelperError::Reply(e.code)),
            Outcome::Reply(_) => Err(HelperError::Reply("bad_voices".into())),
            other => Err(self.failed(&mut slot, id, other)),
        };
        *self.last_used.lock() = Instant::now();
        result
    }

    /// Starts the helper (and runs the handshake) if it is not running.
    pub fn start(&self) -> Result<(), HelperError> {
        self.usable()?;
        let cancel = CancelToken::new();
        let _turn = self.turns.enter(|| self.give_up(&cancel))?;
        self.usable()?;
        self.ensure_running(&mut self.slot.lock(), &cancel)
    }

    /// Quits the helper if it has been idle for `after` and nobody is using or waiting for it. `true` if it quit.
    pub fn quit_if_idle(&self, after: Duration, grace: Duration) -> bool {
        let Some(_turn) = self.turns.try_enter() else { return false };
        let mut slot = self.slot.lock();
        if slot.proc.is_none() || self.idle_for() < after {
            return false;
        }
        let id = slot.next_id();
        if let Some(p) = slot.proc.take() {
            tracing::info!(target: "addons", "{}: quitting the idle helper", self.spec.addon_id);
            p.quit(id, grace);
        }
        true
    }

    /// For remove and shutdown: every waiting and running request fails with [`HelperError::Removed`] (within a
    /// tick), later ones too, and the helper is quit (`grace` to exit, then killed).
    pub fn stop(&self, grace: Duration) {
        self.removed.store(true, Ordering::SeqCst);
        // Running and waiting requests notice `removed` within a tick and give the turn back.
        let _turn = self.turns.enter::<()>(|| None).ok();
        let mut slot = self.slot.lock();
        let id = slot.next_id();
        if let Some(p) = slot.proc.take() {
            p.quit(id, grace);
        }
    }

    // ───────────────────────────── internals ─────────────────────────────

    fn usable(&self) -> Result<(), HelperError> {
        match self.health() {
            HelperHealth::Ready => Ok(()),
            HelperHealth::Broken(r) => Err(HelperError::Broken(r)),
            HelperHealth::Incompatible => Err(HelperError::Incompatible),
            HelperHealth::Removed => Err(HelperError::Removed),
        }
    }

    fn give_up(&self, cancel: &CancelToken) -> Option<HelperError> {
        if self.removed.load(Ordering::SeqCst) {
            Some(HelperError::Removed)
        } else if cancel.is_cancelled() {
            Some(HelperError::Cancelled)
        } else {
            None
        }
    }

    fn set_health(&self, h: HelperHealth) {
        *self.health.lock() = h;
        if let Some(f) = self.on_health.get() {
            f();
        }
    }

    /// Records a crash, violation or timeout; `true` when that used up the budget (the add-on is now Broken).
    fn count_crash(&self) -> bool {
        let mut crashes = self.crashes.lock();
        let now = Instant::now();
        let window = Duration::from_secs(ADDON_CRASH_WINDOW_SEC);
        while crashes.front().is_some_and(|t| now.duration_since(*t) > window) {
            crashes.pop_front();
        }
        crashes.push_back(now);
        let broken = crashes.len() >= ADDON_CRASH_LIMIT;
        drop(crashes);
        if broken {
            tracing::warn!(target: "addons", "{}: {ADDON_CRASH_LIMIT} failures in {ADDON_CRASH_WINDOW_SEC} s; marking it broken", self.spec.addon_id);
            self.set_health(HelperHealth::Broken(BrokenReason::KeepsStopping));
        }
        broken
    }

    /// The error for a wait that ended without a terminal frame, after cleaning up the helper.
    fn failed(&self, slot: &mut Slot, id: u64, outcome: Outcome) -> HelperError {
        let addon = &self.spec.addon_id;
        match outcome {
            Outcome::Cancelled => {
                // Ask the helper to stop, and keep it if it winds down in time.
                let cancel_id = slot.next_id();
                let grace = self.spec.timeouts.cancel_grace;
                let proc = slot.proc.as_mut().expect("running");
                let kept = proc.ask(id, encode(&HostMsg::Cancel { id: cancel_id, target: id }))
                    && matches!(self.await_reply(proc, id, grace, Duration::ZERO, grace, &CancelToken::new()), Outcome::Reply(_));
                if !kept {
                    if let Some(p) = slot.proc.take() {
                        p.kill(addon, "it did not stop after cancel");
                    }
                }
                HelperError::Cancelled
            }
            Outcome::Removed => {
                if let Some(p) = slot.proc.take() {
                    p.kill(addon, "the add-on was removed");
                }
                HelperError::Removed
            }
            Outcome::TimedOut => {
                if let Some(p) = slot.proc.take() {
                    p.kill(addon, "no reply in time");
                }
                self.count_crash();
                HelperError::TimedOut
            }
            Outcome::Violation(why) => self.crashed(slot, &format!("protocol violation: {why}")),
            Outcome::Ended => self.crashed(slot, "it exited"),
            Outcome::Reply(_) => self.crashed(slot, "an unexpected reply"),
        }
    }

    fn crashed(&self, slot: &mut Slot, why: &str) -> HelperError {
        if let Some(p) = slot.proc.take() {
            p.kill(&self.spec.addon_id, why);
        }
        if self.count_crash() {
            HelperError::Broken(BrokenReason::KeepsStopping)
        } else {
            HelperError::Crashed
        }
    }

    /// Waits for request `id`'s terminal frame, at most `max`. The first idle window is `idle + first_extra`; any frame
    /// for the request starts a new one of `idle`. Frames for other ids are stale (an answer after a cancel or a
    /// timeout) and dropped. Afterwards the stdout thread passes nothing on until the next request.
    fn await_reply(&self, proc: &mut Proc, id: u64, idle: Duration, first_extra: Duration, max: Duration, cancel: &CancelToken) -> Outcome {
        proc.pending.store(id, Ordering::SeqCst);
        let outcome = self.wait_frames(proc, id, idle, first_extra, max, cancel);
        proc.pending.store(0, Ordering::SeqCst);
        outcome
    }

    fn wait_frames(&self, proc: &mut Proc, id: u64, idle: Duration, first_extra: Duration, max: Duration, cancel: &CancelToken) -> Outcome {
        let start = Instant::now();
        let hard = start + max;
        let mut soft = start + idle + first_extra;
        loop {
            if self.removed.load(Ordering::SeqCst) {
                return Outcome::Removed;
            }
            if cancel.is_cancelled() {
                return Outcome::Cancelled;
            }
            match proc.rx.recv_timeout(TICK) {
                Ok(Frame::Msg(m)) if m.id == Some(id) => match m.body {
                    HelperBody::Progress => soft = Instant::now() + idle,
                    HelperBody::Other(kind) => {
                        tracing::debug!(target: "addons", "{}: ignoring a {kind:?} frame", self.spec.addon_id);
                        soft = Instant::now() + idle;
                    }
                    body => return Outcome::Reply(body),
                },
                Ok(Frame::Msg(m)) => {
                    tracing::debug!(target: "addons", "{}: dropping a stale frame for {:?}", self.spec.addon_id, m.id);
                }
                Ok(Frame::Violation(why)) => return Outcome::Violation(why),
                Ok(Frame::End) | Err(RecvTimeoutError::Disconnected) => return Outcome::Ended,
                Err(RecvTimeoutError::Timeout) => {
                    // Exited without closing stdout (a grandchild may hold it).
                    if matches!(proc.child.try_exit_status(), Ok(Some(_)) | Err(_)) {
                        return Outcome::Ended;
                    }
                }
            }
            let now = Instant::now();
            if now >= soft.min(hard) {
                return Outcome::TimedOut;
            }
        }
    }

    /// A running, handshaken helper in the slot. A read cancelled during the handshake stops waiting for it at once
    /// (the half-started helper is killed, which does not count as a crash).
    fn ensure_running(&self, slot: &mut Slot, cancel: &CancelToken) -> Result<(), HelperError> {
        if let Some(p) = slot.proc.as_mut() {
            if p.drain() {
                return Ok(());
            }
            // Died while idle (killed, out of memory…): not a request failure, but it still counts.
            if let Some(p) = slot.proc.take() {
                p.kill(&self.spec.addon_id, "it exited while idle");
            }
            if self.count_crash() {
                return Err(HelperError::Broken(BrokenReason::KeepsStopping));
            }
        }
        let proc = self.spawn()?;
        slot.proc = Some(proc);
        let id = slot.next_id();
        let range = ProtocolRange { min: ADDON_PROTOCOL_MIN, max: ADDON_PROTOCOL_MAX };
        let hello = encode(&HostMsg::Hello { id, protocol: range, vox: APP_VERSION, os: std::env::consts::OS, arch: std::env::consts::ARCH });
        let proc = slot.proc.as_mut().expect("just spawned");
        let wait = self.spec.timeouts.start;
        let outcome = if proc.ask(id, hello) { self.await_reply(proc, id, wait, Duration::ZERO, wait, cancel) } else { Outcome::Ended };
        match outcome {
            Outcome::Reply(HelperBody::Hello(h)) => {
                let protocol = h.protocol.unwrap_or(0);
                if !(u64::from(ADDON_PROTOCOL_MIN)..=u64::from(ADDON_PROTOCOL_MAX)).contains(&protocol) {
                    return Err(self.incompatible(slot, &format!("it chose protocol {protocol}")));
                }
                proc.unit = match h.offsets.as_deref() {
                    None => Some(OffsetUnit::Utf8),
                    Some(s) => OffsetUnit::parse(s),
                };
                tracing::info!(
                    target: "addons",
                    "{}: helper {} {} started (pid {})",
                    self.spec.addon_id,
                    cut(&h.name, 60),
                    cut(&h.version, 30),
                    proc.child.id()
                );
                *self.last_used.lock() = Instant::now();
                Ok(())
            }
            Outcome::Reply(HelperBody::Error(e)) if e.code == "unsupported_protocol" => Err(self.incompatible(slot, "it supports no protocol this Vox speaks")),
            Outcome::Reply(_) => Err(self.crashed(slot, "it did not answer hello")),
            Outcome::TimedOut => {
                if let Some(p) = slot.proc.take() {
                    p.kill(&self.spec.addon_id, "no hello in time");
                }
                self.count_crash();
                Err(HelperError::TimedOut)
            }
            Outcome::Violation(why) => Err(self.crashed(slot, &format!("protocol violation before hello: {why}"))),
            Outcome::Ended => Err(self.crashed(slot, "it exited before hello")),
            Outcome::Cancelled => {
                if let Some(p) = slot.proc.take() {
                    p.kill(&self.spec.addon_id, "the read was cancelled during its start");
                }
                Err(HelperError::Cancelled)
            }
            Outcome::Removed => {
                if let Some(p) = slot.proc.take() {
                    p.kill(&self.spec.addon_id, "the add-on was removed");
                }
                Err(HelperError::Removed)
            }
        }
    }

    fn incompatible(&self, slot: &mut Slot, why: &str) -> HelperError {
        if let Some(p) = slot.proc.take() {
            p.kill(&self.spec.addon_id, why);
        }
        self.set_health(HelperHealth::Incompatible);
        HelperError::Incompatible
    }

    fn spawn(&self) -> Result<Proc, HelperError> {
        let spec = &self.spec;
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).current_dir(&spec.dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().to_ascii_uppercase().starts_with("VOX_") {
                cmd.env_remove(&key);
            }
        }
        cmd.env("VOX_ADDON_PROTOCOL", ADDON_PROTOCOL_MAX.to_string()).env("VOX_ADDON_ID", &spec.addon_id).env("VOX_ADDON_DIR", &spec.dir);
        if let Some(data) = &spec.data_dir {
            if let Err(e) = std::fs::create_dir_all(data) {
                tracing::warn!(target: "addons", "{}: cannot create {}: {e}", spec.addon_id, data.display());
            }
            cmd.env("VOX_ADDON_DATA", data);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = Spawner::global().spawn(cmd).map_err(|e| {
            let block = classify_spawn_error(&e);
            tracing::warn!(target: "addons", "{}: cannot start {}: {e} ({block:?})", spec.addon_id, spec.program.display());
            if block == SpawnBlock::NotFound {
                self.set_health(HelperHealth::Broken(BrokenReason::Incomplete));
                HelperError::Broken(BrokenReason::Incomplete)
            } else {
                HelperError::Spawn(block)
            }
        })?;
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let (stdin, stdout, stderr) = match (child.take_stdin(), child.take_stdout(), child.take_stderr()) {
            (Some(i), Some(o), Some(e)) => (i, o, e),
            _ => return Err(HelperError::Crashed),
        };
        let name = |what: &str| format!("vox-addon-{}-{what}", spec.addon_id);
        let (frame_tx, frame_rx) = bounded::<Frame>(64);
        let max_line = spec.max_line;
        let pending = Arc::new(AtomicU64::new(0));
        let wanted = pending.clone();
        let started = std::thread::Builder::new().name(name("out")).spawn(move || read_stdout(stdout, max_line, &wanted, frame_tx)).is_ok();
        let (line_tx, line_rx) = unbounded::<Vec<u8>>();
        let started = started && std::thread::Builder::new().name(name("in")).spawn(move || write_stdin(stdin, line_rx)).is_ok();
        let tail = Arc::new(StderrTail::default());
        let (addon, t) = (spec.addon_id.clone(), tail.clone());
        let started = started && std::thread::Builder::new().name(name("err")).spawn(move || drain_stderr(stderr, &addon, &t)).is_ok();
        let proc = Proc { child, tx: Some(line_tx), rx: frame_rx, pending, unit: Some(OffsetUnit::Utf8), loaded: false, stderr: tail };
        if !started {
            proc.kill(&spec.addon_id, "its I/O threads could not start");
            return Err(HelperError::Crashed);
        }
        Ok(proc)
    }
}

impl Drop for HelperClient {
    fn drop(&mut self) {
        let slot = self.slot.get_mut();
        let id = slot.next_id();
        if let Some(p) = slot.proc.take() {
            p.quit(id, Duration::from_millis(vox_core::constants::ADDON_QUIT_GRACE_MS));
        }
    }
}

/// Reads stdout lines into frames. Only frames for the `pending` request are passed on; the rest are dropped here,
/// so nothing a helper writes between requests is held. Violations and the end of stdout always are.
fn read_stdout(stdout: impl Read, max_line: usize, pending: &AtomicU64, tx: Sender<Frame>) {
    let mut reader = LineReader::new(BufReader::with_capacity(64 << 10, stdout), max_line);
    loop {
        let frame = match reader.next_line() {
            Ok(Some(line)) => match parse_helper_line(&line) {
                Ok(m) => {
                    let wanted = pending.load(Ordering::SeqCst);
                    if wanted == 0 || m.id != Some(wanted) {
                        tracing::debug!(target: "addons", "dropping a frame for {:?} (waiting for {wanted})", m.id);
                        continue;
                    }
                    Frame::Msg(m)
                }
                Err(why) => Frame::Violation(why),
            },
            Ok(None) | Err(LineError::Io(_)) => Frame::End,
            Err(LineError::TooLong) => Frame::Violation(format!("a line over {max_line} bytes")),
            Err(LineError::Truncated) => Frame::Violation("stdout ended in the middle of a line".into()),
        };
        let last = !matches!(frame, Frame::Msg(_));
        if tx.send(frame).is_err() || last {
            return;
        }
    }
}

fn write_stdin(mut stdin: impl Write, rx: Receiver<Vec<u8>>) {
    for line in rx {
        if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
            return;
        }
    }
}

/// The last stderr lines of one process.
#[derive(Default)]
struct StderrTail {
    lines: Mutex<VecDeque<String>>,
}

fn drain_stderr(stderr: impl Read, addon: &str, tail: &StderrTail) {
    let mut reader = BufReader::with_capacity(16 << 10, stderr);
    let mut line = Vec::with_capacity(STDERR_LINE + 1);
    let mut logged = 0u32;
    let mut dropped = 0u64;
    let mut last_log = Instant::now();
    loop {
        line.clear();
        match (&mut reader).take(STDERR_LINE as u64).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A longer line: skip the rest of it.
        if line.last() != Some(&b'\n') && line.len() == STDERR_LINE && !skip_line(&mut reader) {
            break;
        }
        let text = String::from_utf8_lossy(&line).trim_end().to_string();
        {
            let mut t = tail.lines.lock();
            if t.len() == STDERR_TAIL {
                t.pop_front();
            }
            t.push_back(text.clone());
        }
        if logged < STDERR_BURST || last_log.elapsed() >= Duration::from_secs(1) {
            if dropped > 0 {
                tracing::info!(target: "addons", "{addon}: ({dropped} stderr lines not logged)");
                dropped = 0;
            }
            tracing::info!(target: "addons", "{addon}: {text}");
            logged = logged.saturating_add(1);
            last_log = Instant::now();
        } else {
            dropped += 1;
        }
    }
}

/// Reads up to the end of the current line; `false` at the end of the stream.
fn skip_line(reader: &mut impl BufRead) -> bool {
    loop {
        let buf = match reader.fill_buf() {
            Ok(b) if !b.is_empty() => b,
            _ => return false,
        };
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            reader.consume(i + 1);
            return true;
        }
        let n = buf.len();
        reader.consume(n);
    }
}

/// At most `max` characters of untrusted text, for the log.
fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_have_vox_copy_and_only_crashes_retry() {
        let all = [
            HelperError::Crashed,
            HelperError::TimedOut,
            HelperError::Broken(BrokenReason::KeepsStopping),
            HelperError::Broken(BrokenReason::Incomplete),
            HelperError::Incompatible,
            HelperError::Spawn(SpawnBlock::Blocked),
            HelperError::Spawn(SpawnBlock::NoExec),
            HelperError::Spawn(SpawnBlock::Other),
            HelperError::Removed,
            HelperError::Reply("unknown_voice".into()),
            HelperError::Reply("out_of_memory".into()),
            HelperError::Reply("model_load_failed".into()),
            HelperError::Reply("internal".into()),
            HelperError::BadAudio,
        ];
        for e in &all {
            let t = e.into_tts("KittenTTS 2", "2.0.0");
            assert_eq!(t.code, ReadErrorCode::Unknown, "{e:?}");
            assert_eq!(t.retryable, *e == HelperError::Crashed, "{e:?}");
            assert!(t.message.contains("KittenTTS 2"), "{e:?}: {}", t.message);
            assert!(!t.message.contains("ElevenLabs"));
        }
        assert_eq!(HelperError::Reply("unknown_voice".into()).into_tts("KittenTTS 2", "2.0.0").message, "This voice isn’t in KittenTTS 2 2.0.0.");
        assert!(HelperError::Cancelled.into_tts("K", "1").aborted);
    }

    #[test]
    fn the_stdout_thread_passes_on_only_the_pending_requests_frames() {
        let input = concat!(
            "{\"id\":1,\"type\":\"audio\",\"wav\":\"old\"}\n",
            "{\"type\":\"progress\"}\n",
            "{\"id\":2,\"type\":\"progress\"}\n",
            "{\"id\":3,\"type\":\"audio\",\"wav\":\"future\"}\n",
            "{\"id\":2,\"type\":\"error\",\"code\":\"internal\"}\n",
        );
        let (tx, rx) = bounded(64);
        read_stdout(input.as_bytes(), 1 << 20, &AtomicU64::new(2), tx);
        let got: Vec<String> = rx
            .try_iter()
            .map(|f| match f {
                Frame::Msg(m) => format!("{:?} {:?}", m.id, std::mem::discriminant(&m.body)),
                Frame::Violation(v) => format!("violation {v}"),
                Frame::End => "end".into(),
            })
            .collect();
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got[0].starts_with("Some(2)") && got[1].starts_with("Some(2)") && got[2] == "end", "{got:?}");
        // With nothing pending, nothing is kept; a violation still is.
        let (tx, rx) = bounded(64);
        read_stdout(&b"{\"id\":1,\"type\":\"progress\"}\nnot json\n"[..], 1 << 20, &AtomicU64::new(0), tx);
        let got: Vec<bool> = rx.try_iter().map(|f| matches!(f, Frame::Violation(_))).collect();
        assert_eq!(got, [true]);
    }

    #[test]
    fn the_turnstile_is_first_come_first_served_and_can_be_left() {
        let t = Arc::new(Turnstile::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = t.enter::<()>(|| None).unwrap();
        let mut handles = Vec::new();
        for i in 0..4 {
            let (turns, order) = (t.clone(), order.clone());
            handles.push(std::thread::spawn(move || {
                let _turn = turns.enter::<()>(|| None).unwrap();
                order.lock().push(i);
            }));
            // Queue them in a known order.
            while t.queue.lock().waiting.len() < i + 1 {
                std::thread::yield_now();
            }
        }
        // A caller that gives up leaves the queue without blocking the others.
        let gave_up = t.enter(|| Some("cancelled"));
        assert_eq!(gave_up.err(), Some("cancelled"));
        assert!(t.try_enter().is_none());
        drop(first);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*order.lock(), [0, 1, 2, 3]);
        assert!(t.try_enter().is_some());
    }
}
