//! Scenario tests for the player state machine on a simulated device and an injected clock
//! (reader-playback.md §12 #2–#5, #7–#14, #21–#28; ARCHITECTURE.md §9 vox-audio).

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use vox_core::player::{PlayerSnapshot, PlayerStatus, SkipDirection};
use vox_core::read::ReadErrorCode;
use vox_core::tts::MIME_WAV;

use super::*;
use crate::decode::decode;
use crate::decode::tests::mock_wav;
use crate::output::{BlockTag, RingSim, SimOutput};

// ───────────────────────────── harness ─────────────────────────────

#[derive(Clone, Default)]
struct Jobs(Arc<StdMutex<Vec<DecodeJob>>>);

impl DecodeQueue for Jobs {
    fn request(&mut self, job: DecodeJob) {
        self.0.lock().expect("jobs").push(job);
    }
}

/// What the scripted factory does on the next `open`.
#[derive(Clone, Copy, Debug)]
enum Open {
    Ok,
    NoDevice,
    Stream,
    /// A device that exists but never renders.
    Frozen,
    /// The real rings and callback on a simulated device clock (10 ms periods, 40 ms latency).
    Ring,
}

#[derive(Clone, Default)]
struct Factory {
    script: Arc<StdMutex<VecDeque<Open>>>,
    device: Arc<StdMutex<Option<String>>>,
    opens: Arc<StdMutex<usize>>,
}

impl OutputFactory for Factory {
    fn open(&mut self) -> Result<Box<dyn Sink>, OutputError> {
        *self.opens.lock().expect("opens") += 1;
        let next = self.script.lock().expect("script").pop_front().unwrap_or(Open::Ok);
        match next {
            Open::NoDevice => Err(OutputError::NoDevice),
            Open::Stream => Err(OutputError::Stream("scripted failure".into())),
            Open::Ring => {
                let mut r = RingSim::new(48_000, 2, 480, 40.0);
                r.device = self.device.lock().expect("device").clone();
                Ok(Box::new(r))
            }
            Open::Ok | Open::Frozen => {
                let mut s = SimOutput::new(48_000, 2);
                s.test_real = true;
                s.test_frozen = matches!(next, Open::Frozen);
                s.test_device = self.device.lock().expect("device").clone();
                s.capture = Some(Vec::new());
                Ok(Box::new(s))
            }
        }
    }

    fn default_device_id(&mut self) -> Option<String> {
        self.device.lock().expect("device").clone()
    }
}

struct H {
    e: Engine,
    shared: Arc<Shared>,
    now: f64,
    jobs: Jobs,
    events: Arc<StdMutex<Vec<PlayerEvent>>>,
    factory: Factory,
    auto_decode: bool,
    /// Every chunk index decoded, in order.
    decoded_indices: Vec<usize>,
}

fn config(warm: bool, null: bool) -> PlayerConfig {
    PlayerConfig { report_interval: Duration::from_millis(500), warm_output: warm, null_output: null, name: "test" }
}

impl H {
    fn with(cfg: PlayerConfig) -> H {
        let shared = Arc::new(Shared::new());
        let events: Arc<StdMutex<Vec<PlayerEvent>>> = Arc::default();
        let sink = events.clone();
        let jobs = Jobs::default();
        let factory = Factory::default();
        *factory.device.lock().expect("device") = Some("speakers".into());
        let e = Engine::new(
            cfg,
            Arc::new(move |ev| sink.lock().expect("events").push(ev)),
            shared.clone(),
            Box::new(jobs.clone()),
            Box::new(factory.clone()),
        );
        H { e, shared, now: 1000.0, jobs, events, factory, auto_decode: true, decoded_indices: Vec::new() }
    }

    /// Simulated clock, no warm-up (most scenarios).
    fn null() -> H {
        H::with(config(false, true))
    }

    /// A "real" (scripted) device with the warmer on.
    fn warm() -> H {
        H::with(config(true, false))
    }

    fn send(&mut self, msg: Msg) {
        self.e.handle(msg, self.now);
        if self.auto_decode {
            self.decode();
        }
    }

    fn cmd(&mut self, cmd: PlayerCommand) {
        self.send(Msg::Command { cmd, marked: false });
    }

    fn decode(&mut self) {
        for _ in 0..1000 {
            let jobs: Vec<DecodeJob> = std::mem::take(&mut *self.jobs.0.lock().expect("jobs"));
            if jobs.is_empty() {
                return;
            }
            for job in jobs {
                self.decoded_indices.push(job.index);
                let result = decode(&job.audio, &job.mime).map(Arc::new).map_err(|e| e.to_string());
                self.e.on_decoded(DecodeResult { serial: job.serial, index: job.index, result }, self.now);
            }
        }
        panic!("the engine keeps asking for the same decodes: {:?}", &self.decoded_indices[..50]);
    }

    /// Advances the clock in 10 ms ticks.
    fn step(&mut self, ms: f64) {
        let end = self.now + ms;
        while self.now < end - 1e-9 {
            self.now += TICK_MS;
            if self.auto_decode {
                self.decode();
            }
            self.e.tick(self.now);
        }
    }

    /// Steps until `pred` holds (at most `max_ms`); returns the elapsed ms.
    fn step_until(&mut self, max_ms: f64, pred: impl Fn(&PlayerSnapshot) -> bool) -> f64 {
        let start = self.now;
        while !pred(&self.snap()) {
            assert!(self.now - start < max_ms, "condition not reached within {max_ms} ms: {:?}", self.snap().state);
            self.step(TICK_MS);
        }
        self.now - start
    }

    fn snap(&self) -> PlayerSnapshot {
        snapshot_from(&self.shared.published.lock(), self.now)
    }

    fn status(&self) -> PlayerStatus {
        self.snap().state.status
    }

    fn states(&self) -> Vec<PlayerSnapshot> {
        self.events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|e| match e {
                PlayerEvent::State(s) => Some(s.clone()),
                PlayerEvent::Failed { .. } => None,
            })
            .collect()
    }

    fn all_events(&self) -> Vec<PlayerEvent> {
        self.events.lock().expect("events").clone()
    }

    fn clear(&mut self) {
        self.events.lock().expect("events").clear();
    }

    fn captured(&mut self) -> Vec<(BlockTag, Vec<f32>)> {
        self.e.output_mut().and_then(|o| o.as_sim()).and_then(|s| s.capture.clone()).unwrap_or_default()
    }

    fn ring(&mut self) -> &mut RingSim {
        self.e.output_mut().and_then(|o| o.as_ring_sim()).expect("a ring device")
    }
}

fn utf16(s: &str) -> u32 {
    vox_core::text16::utf16_len(s) as u32
}

fn start(id: &str, text: &str, sentences: &[u32]) -> Msg {
    Msg::Start {
        start: TtsSessionStart {
            session_id: id.into(),
            read_id: format!("read-{id}"),
            text: Arc::from(text),
            title: String::new(),
            voice_name: "Ava".into(),
            voice_id: "voice".into(),
            source_app: Some("Notepad".into()),
            estimated_duration_sec: utf16(text) as f64 / 15.0,
            chunk_count: 1,
            rate: 1.0,
            volume: 1.0,
            highlight_words: true,
            from_cache: false,
            sentences: Arc::from(sentences.to_vec()),
        },
        rate_marked: false,
        volume_marked: false,
    }
}

/// A mock WAV chunk of `secs` seconds covering `[a, b)`.
fn chunk(id: &str, index: u32, a: u32, b: u32, secs: f64) -> Msg {
    let frames = (secs * 22_050.0).round() as usize;
    Msg::Chunk(TtsChunk {
        session_id: id.into(),
        index,
        text_start: a,
        text_end: b,
        audio: Arc::from(mock_wav(22_050, frames)),
        mime: MIME_WAV.into(),
        alignment: None,
        duration_sec: secs,
    })
}

/// A loud continuous 220 Hz tone chunk of `secs` covering `[a, b)` (a hand-built WAV: no fades, no silence).
fn tone_chunk(id: &str, index: u32, a: u32, b: u32, secs: f64) -> Msg {
    let sr = 22_050usize;
    let frames = (secs * sr as f64).round() as usize;
    let pcm: Vec<u8> = (0..frames)
        .map(|i| ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / sr as f64).sin() * 16_000.0) as i16)
        .flat_map(|s| s.to_le_bytes())
        .collect();
    Msg::Chunk(TtsChunk {
        session_id: id.into(),
        index,
        text_start: a,
        text_end: b,
        audio: Arc::from(crate::decode::tests::wav_bytes(22_050, 1, &pcm)),
        mime: MIME_WAV.into(),
        alignment: None,
        duration_sec: secs,
    })
}

/// Longest run of near-silent samples and the largest sample-to-sample step.
fn silence_and_steps(x: &[f32]) -> (usize, f32) {
    let mut run = 0;
    let mut longest = 0;
    let mut step = 0f32;
    for (i, v) in x.iter().enumerate() {
        if v.abs() < 1e-3 {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
        if i > 0 {
            step = step.max((v - x[i - 1]).abs());
        }
    }
    (longest, step)
}

fn bad_chunk(id: &str, index: u32, a: u32, b: u32, secs: f64) -> Msg {
    Msg::Chunk(TtsChunk {
        session_id: id.into(),
        index,
        text_start: a,
        text_end: b,
        audio: Arc::from(&b"definitely not audio"[..]),
        mime: MIME_WAV.into(),
        alignment: None,
        duration_sec: secs,
    })
}

fn done(id: &str, n: u32) -> Msg {
    Msg::Done(TtsSessionDone { session_id: id.into(), total_chunks: n })
}

fn fail(id: &str, fatal: bool, code: ReadErrorCode, message: &str) -> Msg {
    Msg::Fail(TtsSessionError { session_id: id.into(), message: message.into(), code, fatal })
}

fn seek(sec: f64) -> PlayerCommand {
    PlayerCommand::Seek { position_sec: sec }
}

fn skip_sentence(dir: SkipDirection) -> PlayerCommand {
    PlayerCommand::Skip { direction: dir, unit: SkipUnit::Sentence, amount: None }
}

const TEXT: &str = "One sentence here. Another one there. And a third one.";

/// A read of three 0.5 s chunks over TEXT (sentence starts 0, 19, 38), all delivered and done.
fn three_chunk_read(h: &mut H) {
    h.send(start("s1", TEXT, &[0, 19, 38]));
    h.send(chunk("s1", 0, 0, 19, 0.5));
    h.send(chunk("s1", 1, 19, 38, 0.5));
    h.send(chunk("s1", 2, 38, utf16(TEXT), 0.5));
    h.send(done("s1", 3));
}

// ───────────────────────────── session events ─────────────────────────────

#[test]
fn start_reports_loading_with_payload_values() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0, 19, 38]));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Loading);
    assert!(s.buffering && s.wants_play && !s.starting);
    assert_eq!(s.state.session_id.as_deref(), Some("s1"));
    assert_eq!(s.state.read_id.as_deref(), Some("read-s1"));
    // Empty title → the text's first 120 units.
    assert_eq!(s.state.title, TEXT);
    assert_eq!(s.state.total_chars, utf16(TEXT));
    assert!((s.state.duration_sec - utf16(TEXT) as f64 / 15.0).abs() < 1e-9);
    assert!(s.state.duration_is_estimate);
    assert_eq!(s.state.source_app.as_deref(), Some("Notepad"));
    let states = h.states();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].state.status, PlayerStatus::Loading);
}

#[test]
fn edge_2_duplicates_other_sessions_and_out_of_order_chunks() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.clear();
    // Duplicate sessionStart: ignored entirely.
    h.send(start("s1", TEXT, &[0]));
    assert!(h.states().is_empty());
    // Events for another session: ignored.
    h.send(chunk("other", 0, 0, 19, 0.5));
    h.send(done("other", 1));
    h.send(fail("other", true, ReadErrorCode::Network, "x"));
    assert!(h.states().is_empty());
    assert_eq!(h.status(), PlayerStatus::Loading);
    // Out of order: chunk 1 is held until chunk 0 arrives.
    h.send(chunk("s1", 1, 19, 38, 0.5));
    assert_eq!(h.snap().state.buffered_fraction, 0.0);
    assert_eq!(h.status(), PlayerStatus::Loading);
    h.send(chunk("s1", 0, 0, 19, 0.5));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing);
    assert!((s.state.duration_sec - (1.0 + (utf16(TEXT) - 38) as f64 * (1.0 / 38.0))).abs() < 1e-6);
    // Duplicate index: ignored.
    let before = h.snap().state.buffered_fraction;
    h.send(chunk("s1", 0, 0, 19, 3.0));
    h.send(chunk("s1", 1, 19, 38, 3.0));
    assert_eq!(h.snap().state.buffered_fraction, before);
}

#[test]
fn plays_to_the_end_and_reports_completion() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    assert_eq!(h.status(), PlayerStatus::Playing);
    let ms = h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    // 1.5 s of audio.
    assert!((1450.0..1700.0).contains(&ms), "{ms}");
    let s = h.snap();
    assert_eq!(s.state.position_sec, s.state.duration_sec);
    assert_eq!(s.state.char_index, utf16(TEXT));
    assert!(!s.state.duration_is_estimate);
    assert_eq!(s.state.buffered_fraction, 1.0);
    assert!(!s.wants_play);
    // Listening ≈ the audio length (device-reported progress only).
    assert!((1.35..=1.51).contains(&s.state.listened_sec), "{}", s.state.listened_sec);
    // Reports: every status change and the 500 ms timer while playing.
    let states = h.states();
    let playing_reports = states.iter().filter(|s| s.state.status == PlayerStatus::Playing).count();
    assert!(playing_reports >= 3, "{playing_reports}");
}

#[test]
fn position_and_char_index_follow_the_device() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.step(260.0);
    let s = h.snap();
    // Played blocks lag the clock by at most one block + one tick.
    assert!((0.22..=0.27).contains(&s.state.position_sec), "{}", s.state.position_sec);
    // Linear fallback inside chunk 0 (19 chars over 0.5 s).
    let expected = (s.state.position_sec / 0.5 * 19.0).floor() as u32;
    assert!(s.state.char_index.abs_diff(expected) <= 1, "{} vs {expected}", s.state.char_index);
    // Into chunk 1 (gapless: no status change at the boundary).
    h.step(400.0);
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing);
    assert!(s.state.position_sec > 0.5 && s.state.char_index >= 19, "{:?}", s.state);
}

#[test]
fn edge_3_pause_holds_reports_and_position() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.step(300.0);
    h.cmd(PlayerCommand::Pause);
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Paused);
    assert!(!s.wants_play);
    let pos = s.state.position_sec;
    let listened = s.state.listened_sec;
    h.clear();
    h.step(5000.0);
    assert!(h.states().is_empty(), "no reports while paused");
    let s = h.snap();
    assert_eq!(s.state.position_sec, pos);
    assert_eq!(s.state.listened_sec, listened);
    // Resume continues from what was heard.
    h.cmd(PlayerCommand::Play);
    assert_eq!(h.status(), PlayerStatus::Playing);
    h.step(100.0);
    let s = h.snap();
    assert!(s.state.position_sec > pos && s.state.position_sec < pos + 0.12, "{} vs {pos}", s.state.position_sec);
}

#[test]
fn edge_4_pause_during_synthesizing() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.cmd(PlayerCommand::Pause);
    assert_eq!(h.status(), PlayerStatus::Paused);
    assert!(h.snap().buffering);
    h.send(chunk("s1", 0, 0, 19, 0.5));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Paused, "stays paused with audio ready");
    assert!(!s.buffering);
    h.step(300.0);
    assert_eq!(h.snap().state.position_sec, 0.0);
    h.cmd(PlayerCommand::Play);
    assert_eq!(h.status(), PlayerStatus::Playing);
    h.step(200.0);
    assert!(h.snap().state.position_sec > 0.1);
}

#[test]
fn edge_5_done_while_waiting_at_the_end_ends_at_once() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    h.step_until(1000.0, |s| s.buffering);
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Loading, "caught up with synthesis");
    assert!((s.state.position_sec - 0.3).abs() < 0.02);
    h.send(done("s1", 1));
    assert_eq!(h.status(), PlayerStatus::Ended);
}

#[test]
fn caught_up_then_next_chunk_resumes() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    h.step_until(1000.0, |s| s.buffering);
    h.send(chunk("s1", 1, 19, 38, 0.3));
    assert_eq!(h.status(), PlayerStatus::Playing);
    h.send(done("s1", 2));
    h.step_until(1000.0, |s| s.state.status == PlayerStatus::Ended);
}

#[test]
fn non_fatal_warning_then_ended_stopped_early() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    h.send(fail("s1", false, ReadErrorCode::Network, ""));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing, "playback continues");
    let w = s.warning.expect("warning");
    assert_eq!(w.message, WARNING_DEFAULT);
    assert_eq!(w.code, ReadErrorCode::Network);
    h.send(done("s1", 1));
    h.step_until(1000.0, |s| s.state.status == PlayerStatus::Ended);
    assert!(h.snap().warning.is_some(), "the warning is never cleared within the session");
}

#[test]
fn edge_7_fatal_error_card() {
    let mut h = H::warm();
    h.send(start("s1", TEXT, &[0]));
    h.step(1100.0);
    h.send(chunk("s1", 0, 0, 19, 0.5));
    h.step(200.0);
    h.send(fail("s1", true, ReadErrorCode::Quota, "Out of credits"));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Error);
    assert_eq!(s.state.error.as_deref(), Some("Out of credits"));
    assert_eq!(s.state.error_code, Some(ReadErrorCode::Quota));
    assert!(!s.wants_play);
    let pos = s.state.position_sec;
    // play / seek / skip / toggle: no-ops.
    for c in [PlayerCommand::Play, seek(0.0), skip_sentence(SkipDirection::Forward), PlayerCommand::Toggle] {
        h.cmd(c);
        let s = h.snap();
        assert_eq!(s.state.status, PlayerStatus::Error);
        assert_eq!(s.state.position_sec, pos);
    }
    // Rate and volume still apply.
    h.cmd(PlayerCommand::Rate { rate: 1.5 });
    assert_eq!(h.snap().state.rate, 1.5);
    // The output is released 30 s later.
    assert!(h.e.output_mut().map(|o| o.is_running()).unwrap_or(false));
    h.step(29_000.0);
    assert!(h.e.output_mut().map(|o| o.is_running()).unwrap_or(false));
    h.step(1_100.0);
    assert!(!h.e.output_mut().map(|o| o.is_running()).unwrap_or(true), "released after 30 s");
    // Only stop (or a new session) leaves the error card.
    h.cmd(PlayerCommand::Stop);
    assert_eq!(h.status(), PlayerStatus::Idle);
}

#[test]
fn fatal_message_defaults() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(fail("s1", true, ReadErrorCode::Unknown, ""));
    assert_eq!(h.snap().state.error.as_deref(), Some(ERROR_DEFAULT));
}

#[test]
fn edge_8_play_after_ended_replays_and_seek_back_resumes() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    h.cmd(PlayerCommand::Play);
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing);
    assert!(s.wants_play);
    assert!(s.state.position_sec < 0.02);
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    // Seeking after the end resumes playback.
    h.cmd(seek(0.6));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing);
    assert!((s.state.position_sec - 0.6).abs() < 0.01);
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    // Skip back after the end resumes too; skip forward on ended does nothing.
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert_eq!(h.status(), PlayerStatus::Ended);
    h.cmd(skip_sentence(SkipDirection::Back));
    assert_eq!(h.status(), PlayerStatus::Playing);
}

#[test]
fn edge_9_next_sentence_past_the_end_and_before_audio_arrived() {
    let mut h = H::null();
    // Sentence 2 (char 38) is in chunk 1, which has not arrived.
    h.send(start("s1", TEXT, &[0, 19, 38]));
    h.send(chunk("s1", 0, 0, 38, 1.0));
    h.step(100.0);
    h.cmd(skip_sentence(SkipDirection::Forward));
    let s = h.snap();
    assert!((s.state.position_sec - 0.5).abs() < 0.01, "sentence 1 at 19/38 of 1 s: {}", s.state.position_sec);
    // Next: its audio has not arrived → end of the available audio, then buffering.
    h.cmd(skip_sentence(SkipDirection::Forward));
    let s = h.snap();
    assert!((s.state.position_sec - 0.95).abs() < 0.02, "{}", s.state.position_sec);
    h.step_until(1000.0, |s| s.buffering && s.state.status == PlayerStatus::Loading);
    // Chunk 1 arrives with done: next on the last sentence ends the read.
    h.send(chunk("s1", 1, 38, utf16(TEXT), 0.5));
    h.send(done("s1", 2));
    h.step(50.0);
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert_eq!(h.status(), PlayerStatus::Ended);
    assert_eq!(h.snap().state.position_sec, h.snap().state.duration_sec);
}

#[test]
fn previous_sentence_grace() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    // 0.3 s into sentence 1 (starts at 0.5 s): less than 1.2 wall seconds → previous sentence (0).
    h.cmd(seek(0.8));
    h.step(20.0);
    h.cmd(skip_sentence(SkipDirection::Back));
    assert!(h.snap().state.position_sec < 0.03);
    // At 2× the grace is 2.4 audio seconds; at 0.5× it is 0.6: 0.7 s into chunk 2 → restart chunk 2 sentence.
    h.cmd(PlayerCommand::Rate { rate: 0.5 });
    h.cmd(seek(1.4));
    h.step(10.0);
    // Sentence starts are 0, 0.5, 1.0: pos ≈ 1.4, grace 0.6 → targets ≤ 0.8 → 0.5.
    h.cmd(skip_sentence(SkipDirection::Back));
    assert!((h.snap().state.position_sec - 0.5).abs() < 0.02, "{}", h.snap().state.position_sec);
}

#[test]
fn partial_text_skips_ten_seconds() {
    let mut h = H::null();
    h.send(start("s1", "short", &[0]));
    // A chunk whose range lies beyond the text.
    h.send(chunk("s1", 0, 0, 50, 12.0));
    assert!(h.snap().partial_text);
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert!((h.snap().state.position_sec - 10.0).abs() < 0.05, "{}", h.snap().state.position_sec);
}

#[test]
fn edge_10_seek_clamps() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 1.0));
    h.cmd(PlayerCommand::Pause);
    h.cmd(seek(99.0));
    assert!((h.snap().state.position_sec - 0.95).abs() < 1e-9);
    h.cmd(seek(-5.0));
    assert_eq!(h.snap().state.position_sec, 0.0);
    // seekFraction is a fraction of the estimated total: dragging past the synthesized part lands at the end.
    h.cmd(PlayerCommand::SeekFraction { fraction: 1.0 });
    assert!((h.snap().state.position_sec - 0.95).abs() < 1e-9);
    h.cmd(PlayerCommand::Skip { direction: SkipDirection::Back, unit: SkipUnit::Seconds, amount: None });
    assert_eq!(h.snap().state.position_sec, 0.0);
    h.cmd(PlayerCommand::Skip { direction: SkipDirection::Forward, unit: SkipUnit::Seconds, amount: Some(0.25) });
    assert!((h.snap().state.position_sec - 0.25).abs() < 1e-9);
}

#[test]
fn edge_11_listening_banks_rate_seeks_and_ignores_sleep() {
    let mut h = H::null();
    h.send(start("s1", "x".repeat(150).as_str(), &[0]));
    h.send(chunk("s1", 0, 0, 150, 10.0));
    h.send(done("s1", 1));
    h.step(1000.0);
    let l1 = h.snap().state.listened_sec;
    assert!((0.95..=1.0).contains(&l1), "{l1}");
    // 2×: 1 wall second = 2 audio seconds; listening counts wall time.
    h.cmd(PlayerCommand::Rate { rate: 2.0 });
    let p1 = h.snap().state.position_sec;
    h.step(1000.0);
    let s = h.snap();
    assert!((s.state.listened_sec - l1 - 1.0).abs() < 0.06, "{} after {l1}", s.state.listened_sec);
    assert!((s.state.position_sec - p1 - 2.0).abs() < 0.1, "position {} from {p1}", s.state.position_sec);
    // A seek is not listening.
    let l2 = s.state.listened_sec;
    h.cmd(seek(8.0));
    h.step(10.0);
    assert!(h.snap().state.listened_sec - l2 < 0.02);
    // System sleep: the clock jumps 60 s without ticks; only the queued audio (≤ 150 ms) can count.
    let l3 = h.snap().state.listened_sec;
    h.now += 60_000.0;
    h.step(10.0);
    let gained = h.snap().state.listened_sec - l3;
    assert!(gained < 0.2, "sleep must not count: {gained}");
}

#[test]
fn edge_12_and_25_undecodable_chunk_is_skipped_but_keeps_its_slot() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0, 19, 38]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    h.send(bad_chunk("s1", 1, 19, 38, 0.4));
    h.send(chunk("s1", 2, 38, utf16(TEXT), 0.3));
    h.send(done("s1", 3));
    let s = h.snap();
    assert!((s.state.duration_sec - 1.0).abs() < 1e-6, "the failed chunk still occupies 0.4 s");
    // Play through: chunk 1 is skipped (never heard), the read still ends normally.
    let mut saw_chunk2 = false;
    while h.status() != PlayerStatus::Ended {
        h.step(10.0);
        let s = h.snap();
        if s.state.char_index >= 38 && s.state.status == PlayerStatus::Playing {
            saw_chunk2 = true;
            assert!(s.state.position_sec >= 0.7 - 1e-9, "{}", s.state.position_sec);
        }
        assert!(h.now < 10_000.0);
    }
    assert!(saw_chunk2);
    // About 0.6 s of audio was actually listened to.
    let l = h.snap().state.listened_sec;
    assert!((0.5..0.65).contains(&l), "{l}");
    // No chunk-1 audio was ever fed.
    assert!(h.captured().is_empty() || !h.captured().iter().any(|(t, _)| t.chunk == 1));
}

#[test]
fn edge_14_warm_up_gates_speech_for_one_rendered_second() {
    let mut h = H::warm();
    h.send(start("s1", TEXT, &[0]));
    // The chunk arrives quickly (the device is still waking): STARTING.
    h.step(100.0);
    h.send(chunk("s1", 0, 0, 19, 0.5));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Loading);
    assert!(s.starting && !s.buffering, "audio ready, waiting for the output");
    let waited = h.step_until(2000.0, |s| s.state.status == PlayerStatus::Playing);
    // 1 s of keep-alive rendered since the stream started at sessionStart.
    assert!((850.0..=950.0).contains(&waited), "{waited}");
    assert!(!h.snap().starting);
}

#[test]
fn edge_14_and_28_no_device_never_waits_and_retries_after_a_minute() {
    let mut h = H::warm();
    *h.factory.device.lock().expect("device") = None;
    h.factory.script.lock().expect("script").push_back(Open::NoDevice);
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    assert_eq!(h.status(), PlayerStatus::Playing, "no wait without a device");
    assert!(h.e.output_kind_is_sim_no_device());
    // The simulated clock drives position and listening.
    h.step(200.0);
    assert!(h.snap().state.position_sec > 0.15);
    assert!(h.snap().state.listened_sec > 0.15);
    h.send(done("s1", 1));
    h.step_until(1000.0, |s| s.state.status == PlayerStatus::Ended);
    // A new read within 60 s does not try to open the device again …
    h.send(start("s2", TEXT, &[0]));
    assert_eq!(*h.factory.opens.lock().expect("opens"), 1);
    h.cmd(PlayerCommand::Stop);
    // … after 60 s it does.
    h.step(60_000.0);
    *h.factory.device.lock().expect("device") = Some("speakers".into());
    h.send(start("s3", TEXT, &[0]));
    assert_eq!(*h.factory.opens.lock().expect("opens"), 2);
    assert!(!h.e.output_kind_is_sim_no_device());
}

#[test]
fn edge_28_broken_device_costs_one_three_second_wait() {
    let mut h = H::warm();
    h.factory.script.lock().expect("script").push_back(Open::Frozen);
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.5));
    assert!(h.snap().starting);
    let waited = h.step_until(4000.0, |s| s.state.status == PlayerStatus::Playing);
    assert!((2900.0..=3100.0).contains(&waited), "{waited}");
    // Given up: pause/play starts at once until the output is released.
    h.cmd(PlayerCommand::Pause);
    h.cmd(PlayerCommand::Play);
    assert_eq!(h.status(), PlayerStatus::Playing);
}

#[test]
fn edge_21_pause_during_starting_abandons_the_start() {
    let mut h = H::warm();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.5));
    assert!(h.snap().starting);
    h.cmd(PlayerCommand::Pause);
    assert_eq!(h.status(), PlayerStatus::Paused);
    assert!(!h.snap().starting);
    // The warm-up resolves: still paused, nothing plays.
    h.step(1500.0);
    assert_eq!(h.status(), PlayerStatus::Paused);
    assert_eq!(h.snap().state.position_sec, 0.0);
    // Play later: the output is warm by now (paused holds it 60 s) → starts at once.
    h.cmd(PlayerCommand::Play);
    assert_eq!(h.status(), PlayerStatus::Playing);
}

#[test]
fn edge_22_seek_while_paused_stays_paused_and_seek_while_buffering_resumes() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0, 19]));
    h.send(chunk("s1", 0, 0, 38, 1.0));
    h.step(100.0);
    h.cmd(PlayerCommand::Pause);
    h.cmd(seek(0.7));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Paused);
    assert!((s.state.position_sec - 0.7).abs() < 1e-9);
    // 0.2 s into sentence 1 (starts at 0.5 s) is within the 1.2 s grace → the previous sentence (0).
    h.cmd(skip_sentence(SkipDirection::Back));
    assert_eq!(h.status(), PlayerStatus::Paused);
    assert_eq!(h.snap().state.position_sec, 0.0);
    // Next sentence while paused stays paused too.
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert_eq!(h.status(), PlayerStatus::Paused);
    assert!((h.snap().state.position_sec - 0.5).abs() < 1e-9);
    // Buffering at the end of the available audio, then seek back into it: playback resumes.
    h.cmd(PlayerCommand::Play);
    h.step_until(2000.0, |s| s.buffering);
    assert_eq!(h.status(), PlayerStatus::Loading);
    h.cmd(seek(0.2));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing);
    assert!(!s.buffering);
}

#[test]
fn edge_23_next_on_last_sentence_while_paused_and_done_ends() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.cmd(seek(1.2));
    h.cmd(PlayerCommand::Pause);
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert_eq!(h.status(), PlayerStatus::Ended);
}

#[test]
fn edge_24_paused_while_buffering_then_done_ends_without_playing() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    h.step_until(1000.0, |s| s.buffering);
    h.cmd(PlayerCommand::Pause);
    assert_eq!(h.status(), PlayerStatus::Paused);
    assert!(h.snap().buffering);
    h.send(done("s1", 1));
    assert_eq!(h.status(), PlayerStatus::Ended);
}

#[test]
fn edge_26_toggle_on_the_error_card_does_nothing() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(fail("s1", true, ReadErrorCode::NoApiKey, "Add your key"));
    h.clear();
    h.cmd(PlayerCommand::Toggle);
    assert_eq!(h.status(), PlayerStatus::Error);
    assert!(h.states().is_empty() || h.states().iter().all(|s| s.state.status == PlayerStatus::Error));
}

#[test]
fn edge_27_toggle_on_an_ended_read_replays_it() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    // The ended session stays (the media overlay keeps listing it).
    assert!(h.snap().session.is_some());
    h.cmd(PlayerCommand::Toggle);
    assert_eq!(h.status(), PlayerStatus::Playing);
    assert!(h.snap().state.position_sec < 0.02);
}

#[test]
fn ended_chunk_arrival_restarts_playback_defensively() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.2));
    h.send(done("s1", 1));
    h.step_until(1000.0, |s| s.state.status == PlayerStatus::Ended);
    h.send(chunk("s1", 1, 19, 38, 0.2));
    assert_eq!(h.status(), PlayerStatus::Playing);
}

// ───────────────────────────── stop / supersede (D-RP-11) ─────────────────────────────

#[test]
fn explicit_stop_reports_idle_with_session_then_without() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.step(400.0);
    h.clear();
    h.cmd(PlayerCommand::Stop);
    let states = h.states();
    assert_eq!(states.len(), 2, "{states:?}");
    assert_eq!(states[0].state.status, PlayerStatus::Idle);
    assert_eq!(states[0].state.session_id.as_deref(), Some("s1"));
    assert!(states[0].session.is_some());
    // The final listening for the session: what was heard (≈ 0.4 s).
    assert!((0.33..=0.41).contains(&states[0].state.listened_sec), "{}", states[0].state.listened_sec);
    assert!(states[0].state.position_sec > 0.3);
    assert_eq!(states[1].state.status, PlayerStatus::Idle);
    assert!(states[1].session.is_none() && states[1].state.session_id.is_none());
    assert_eq!(states[1].state.rate, 1.0);
    // Stop while idle: nothing.
    h.clear();
    h.cmd(PlayerCommand::Stop);
    assert!(h.states().is_empty());
    // Nothing plays any more.
    h.step(500.0);
    assert!(h.states().is_empty());
}

#[test]
fn supersede_reports_idle_old_then_loading_new() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.step(300.0);
    h.clear();
    h.send(start("s2", "Second read.", &[0]));
    let states = h.states();
    assert_eq!(states.len(), 2, "{states:?}");
    assert_eq!(states[0].state.status, PlayerStatus::Idle);
    assert_eq!(states[0].state.session_id.as_deref(), Some("s1"));
    assert!(states[0].state.listened_sec > 0.2);
    assert_eq!(states[1].state.status, PlayerStatus::Loading);
    assert_eq!(states[1].state.session_id.as_deref(), Some("s2"));
    assert_eq!(states[1].state.listened_sec, 0.0);
    // Late chunks of the old session are ignored.
    h.send(chunk("s1", 3, 0, 5, 0.2));
    assert_eq!(h.snap().state.session_id.as_deref(), Some("s2"));
    assert_eq!(h.snap().state.buffered_fraction, 0.0);
}

#[test]
fn start_payload_overrides_rate_and_volume() {
    let mut h = H::null();
    h.cmd(PlayerCommand::Rate { rate: 2.0 });
    h.cmd(PlayerCommand::Volume { volume: 0.2 });
    // No session: the stored values change and an idle state is reported.
    let s = h.snap();
    assert_eq!((s.state.rate, s.state.volume), (2.0, 0.2));
    assert!(s.session.is_none());
    let Msg::Start { mut start, .. } = start("s1", TEXT, &[0]) else { unreachable!() };
    start.rate = 9.0;
    start.volume = f64::NAN;
    h.send(Msg::Start { start, rate_marked: false, volume_marked: false });
    let s = h.snap();
    assert_eq!(s.state.rate, 3.0, "clamped");
    assert_eq!(s.state.volume, 0.2, "non-finite keeps the previous value");
}

#[test]
fn decoded_duration_refinement_is_reported_even_while_paused() {
    let mut h = H::null();
    h.auto_decode = false;
    h.send(start("s1", TEXT, &[0]));
    h.cmd(PlayerCommand::Pause);
    // The engine estimated 2 s, the audio is 1 s.
    let Msg::Chunk(mut c) = chunk("s1", 0, 0, 19, 1.0) else { unreachable!() };
    c.duration_sec = 2.0;
    h.send(Msg::Chunk(c));
    h.send(done("s1", 1));
    assert!((h.snap().state.duration_sec - 2.0).abs() < 1e-9);
    h.clear();
    h.decode();
    let states = h.states();
    assert_eq!(states.len(), 1, "one report for the refinement");
    assert_eq!(states[0].state.status, PlayerStatus::Paused);
    assert!((states[0].state.duration_sec - 1.0).abs() < 1e-6);
}

#[test]
fn listener_panics_are_contained() {
    let shared = Arc::new(Shared::new());
    let mut e = Engine::new(
        config(false, true),
        Arc::new(|_| panic!("listener bug")),
        shared.clone(),
        Box::new(Jobs::default()),
        Box::new(Factory::default()),
    );
    e.handle(start("s1", TEXT, &[0]), 0.0);
    assert_eq!(e.status(), PlayerStatus::Loading);
}

// ───────────────────────────── devices and failures (§5.8, §5.9) ─────────────────────────────

#[test]
fn mid_read_no_device_switches_to_the_simulated_clock() {
    let mut h = H::with(config(false, false));
    three_chunk_read(&mut h);
    h.step(300.0);
    let pos = h.snap().state.position_sec;
    // The only output is unplugged: the stream reports the loss, reopening finds no device.
    *h.factory.device.lock().expect("device") = None;
    h.factory.script.lock().expect("script").push_back(Open::NoDevice);
    if let Some(sim) = h.e.output_mut().and_then(|o| o.as_sim()) {
        sim.test_lost = Some(true);
    }
    h.step(10.0);
    assert!(h.e.output_kind_is_sim_no_device());
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing, "no status change");
    assert!((s.state.position_sec - pos).abs() < 0.03, "position kept: {} vs {pos}", s.state.position_sec);
    // Keeps playing on the simulated clock to the end; no failure, no exit.
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    assert!(h.e.take_exit().is_none());
    assert!(!h.all_events().iter().any(|e| matches!(e, PlayerEvent::Failed { .. })));
}

#[test]
fn default_device_change_reopens_and_keeps_position() {
    let mut h = H::with(config(false, false));
    three_chunk_read(&mut h);
    h.step(400.0);
    let pos = h.snap().state.position_sec;
    let opens = *h.factory.opens.lock().expect("opens");
    *h.factory.device.lock().expect("device") = Some("headphones".into());
    // The 2 s poll notices the new default.
    h.step(2100.0 - 400.0);
    assert_eq!(*h.factory.opens.lock().expect("opens"), opens + 1);
    let s = h.snap();
    assert!(s.state.status == PlayerStatus::Playing || s.state.status == PlayerStatus::Ended);
    assert!(s.state.position_sec >= pos);
}

#[test]
fn three_stream_failures_in_ten_seconds_end_the_player() {
    let mut h = H::with(config(false, false));
    {
        let mut script = h.factory.script.lock().expect("script");
        script.extend([Open::Stream, Open::Stream, Open::Stream]);
    }
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.5));
    assert!(h.e.take_exit().is_none());
    // Retried after 1 s each.
    h.step(1000.0);
    assert!(h.e.take_exit().is_none());
    h.step(1000.0);
    assert_eq!(h.e.take_exit(), Some(Exit::OutputFailed));
}

#[test]
fn stream_failures_spread_out_do_not_fail_the_player() {
    let mut h = H::with(config(false, false));
    h.factory.script.lock().expect("script").push_back(Open::Stream);
    h.send(Msg::Preload);
    h.step(6000.0);
    h.factory.script.lock().expect("script").push_back(Open::Stream);
    h.send(start("s1", TEXT, &[0]));
    h.step(6000.0);
    h.factory.script.lock().expect("script").push_back(Open::Stream);
    h.cmd(PlayerCommand::Stop);
    h.send(start("s2", TEXT, &[0]));
    assert!(h.e.take_exit().is_none());
}

#[test]
fn recovery_reports_error_state_then_failed() {
    let mut h = H::null();
    let info = Arc::new(SessionInfo {
        id: "s9".into(),
        read_id: "r9".into(),
        text: Arc::from("Hello"),
        title: "Hello".into(),
        voice_name: "Ava".into(),
        voice_id: "v".into(),
        source_app: None,
        from_cache: false,
        highlight_words: true,
        sentences: Arc::from(vec![0u32]),
    });
    h.e.recover(Some(info), vox_core::copy::PLAYBACK_FAILED, h.now);
    let events = h.all_events();
    assert_eq!(events.len(), 2);
    match &events[0] {
        PlayerEvent::State(s) => {
            assert_eq!(s.state.status, PlayerStatus::Error);
            assert_eq!(s.state.session_id.as_deref(), Some("s9"));
            assert_eq!(s.state.error.as_deref(), Some(vox_core::copy::PLAYBACK_FAILED));
            assert_eq!(s.state.error_code, Some(ReadErrorCode::Unknown));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        events[1],
        PlayerEvent::Failed { session_id: Some("s9".into()), message: vox_core::copy::PLAYBACK_FAILED.into() }
    );
    // Stop leaves the error card with the usual two reports.
    h.clear();
    h.cmd(PlayerCommand::Stop);
    assert_eq!(h.states().len(), 2);
    // Without a session the restart is silent.
    let mut quiet = H::null();
    quiet.e.recover(None, vox_core::copy::PLAYBACK_FAILED, 0.0);
    assert!(quiet.all_events().is_empty());
}

#[test]
fn preload_opens_the_stream_suspended() {
    let mut h = H::warm();
    h.send(Msg::Preload);
    assert_eq!(*h.factory.opens.lock().expect("opens"), 1);
    assert!(!h.e.output_mut().map(|o| o.is_running()).unwrap_or(true));
    // Idle: no wake-ups needed.
    assert_eq!(h.e.next_wakeup(h.now), None);
    // A second preload does nothing.
    h.send(Msg::Preload);
    assert_eq!(*h.factory.opens.lock().expect("opens"), 1);
}

#[test]
fn idle_engine_needs_no_ticks() {
    let mut h = H::null();
    assert_eq!(h.e.next_wakeup(h.now), None);
    three_chunk_read(&mut h);
    assert!(h.e.next_wakeup(h.now).is_some());
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    assert_eq!(h.e.next_wakeup(h.now), None, "ended: nothing to tick");
    h.cmd(PlayerCommand::Stop);
    assert_eq!(h.e.next_wakeup(h.now), None);
    // A held real stream wakes only for its release.
    let mut w = H::warm();
    w.send(start("s1", TEXT, &[0]));
    w.step(1100.0);
    w.cmd(PlayerCommand::Stop);
    let next = w.e.next_wakeup(w.now).expect("release / device poll");
    assert!(next - w.now > 10.0 * TICK_MS, "no 10 ms tick while only holding the stream");
    w.step(31_000.0);
    assert_eq!(w.e.next_wakeup(w.now), None, "released: block");
}

#[test]
fn rate_change_while_playing_is_seamless() {
    let mut h = H::null();
    h.send(start("s1", "x".repeat(150).as_str(), &[0]));
    h.send(chunk("s1", 0, 0, 150, 4.0));
    h.send(done("s1", 1));
    h.step(500.0);
    let before = h.snap().state.position_sec;
    h.cmd(PlayerCommand::Rate { rate: 1.5 });
    assert_eq!(h.snap().state.rate, 1.5);
    h.step(400.0);
    let after = h.snap().state.position_sec;
    // 0.4 wall s at 1.5× ≈ 0.6 audio s (± a block).
    assert!((after - before - 0.6).abs() < 0.06, "{before} → {after}");
}

#[test]
fn gapless_join_in_the_rendered_stream() {
    // Two chunks back to back on a "real" (capturing) device: no silent gap > 1 ms between them.
    let mut h = H::with(config(false, false));
    let text = "x".repeat(60);
    h.send(start("s1", &text, &[0]));
    // A loud continuous tone split across two chunks (no fades): build the WAVs by hand.
    let sr = 22_050usize;
    let tone: Vec<i16> = (0..sr)
        .map(|i| ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / sr as f64).sin() * 16_000.0) as i16)
        .collect();
    let wav = |samples: &[i16]| {
        let pcm: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        Arc::from(crate::decode::tests::wav_bytes(22_050, 1, &pcm))
    };
    let split = 11_000;
    for (index, (a, b, part)) in [(0u32, 30u32, &tone[..split]), (1, 60, &tone[split..])].into_iter().enumerate() {
        h.send(Msg::Chunk(TtsChunk {
            session_id: "s1".into(),
            index: index as u32,
            text_start: a,
            text_end: b,
            audio: wav(part),
            mime: MIME_WAV.into(),
            alignment: None,
            duration_sec: part.len() as f64 / sr as f64,
        }));
    }
    h.send(done("s1", 2));
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    let blocks = h.captured();
    assert!(blocks.iter().any(|(t, _)| t.chunk == 1));
    let left: Vec<f32> = blocks.iter().flat_map(|(_, s)| s.iter().step_by(2).copied()).collect();
    let inner = &left[480..left.len() - 480];
    let mut run = 0;
    let mut longest = 0;
    for x in inner {
        if x.abs() < 1e-3 {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    assert!(longest < 48, "silent run of {longest} samples at 48 kHz");
    assert!((left.len() as f64 - 48_000.0).abs() < 200.0, "{}", left.len());
}

// ───────────────────────────── more transport paths ─────────────────────────────

#[test]
fn cross_chunk_seek_while_playing_feeds_the_target_chunk() {
    let mut h = H::with(config(false, false));
    three_chunk_read(&mut h);
    h.step(100.0);
    h.cmd(seek(1.2));
    assert_eq!(h.status(), PlayerStatus::Playing);
    h.step(100.0);
    let s = h.snap();
    assert!((1.2..1.35).contains(&s.state.position_sec), "{}", s.state.position_sec);
    assert!(s.state.char_index >= 38);
    // The audio fed after the seek comes from chunk 2, starting at 0.2 s into it.
    let blocks = h.captured();
    let after: Vec<_> = blocks.iter().rev().take_while(|(t, _)| t.chunk == 2).collect();
    let first = after.last().expect("chunk 2 blocks");
    assert!((first.0.source_frame_end - 0.2 * 22_050.0).abs() < 400.0, "{}", first.0.source_frame_end);
}

#[test]
fn seek_into_a_failed_chunk_skips_to_the_next_one() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0, 19, 38]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    h.send(bad_chunk("s1", 1, 19, 38, 0.4));
    h.send(chunk("s1", 2, 38, utf16(TEXT), 0.3));
    h.send(done("s1", 3));
    h.cmd(seek(0.5));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing);
    assert!((s.state.position_sec - 0.7).abs() < 0.02, "the start of chunk 2: {}", s.state.position_sec);
    // While paused the playhead may sit inside the failed chunk until play.
    h.cmd(PlayerCommand::Pause);
    h.cmd(seek(0.5));
    assert!((h.snap().state.position_sec - 0.5).abs() < 1e-9);
    h.cmd(PlayerCommand::Play);
    assert!((h.snap().state.position_sec - 0.7).abs() < 0.02);
}

#[test]
fn out_of_order_chunk_while_playing_waits_for_the_gap() {
    let mut h = H::null();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.3));
    h.send(chunk("s1", 2, 38, utf16(TEXT), 0.3));
    h.step_until(1000.0, |s| s.buffering);
    h.send(chunk("s1", 1, 19, 38, 0.3));
    h.send(done("s1", 3));
    assert_eq!(h.status(), PlayerStatus::Playing);
    let ms = h.step_until(2000.0, |s| s.state.status == PlayerStatus::Ended);
    assert!((550.0..800.0).contains(&ms), "chunks 1 and 2 both play: {ms}");
}

#[test]
fn volume_command_applies_and_reports() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.clear();
    h.cmd(PlayerCommand::Volume { volume: 0.3 });
    assert_eq!(h.snap().state.volume, 0.3);
    assert_eq!(h.states().len(), 1);
    // Same value: no report.
    h.cmd(PlayerCommand::Volume { volume: 0.3 });
    assert_eq!(h.states().len(), 1);
    h.cmd(PlayerCommand::Volume { volume: f64::NAN });
    assert_eq!(h.states().len(), 1);
}

#[test]
fn pause_release_then_play_warms_again() {
    let mut h = H::warm();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 38, 3.0));
    h.step_until(2000.0, |s| s.state.status == PlayerStatus::Playing);
    h.step(300.0);
    h.cmd(PlayerCommand::Pause);
    // Resumed within 60 s: instant.
    h.step(30_000.0);
    h.cmd(PlayerCommand::Play);
    assert_eq!(h.status(), PlayerStatus::Playing);
    h.cmd(PlayerCommand::Pause);
    // After the 60 s hold the stream is suspended: the next play warms up again (STARTING).
    h.step(60_100.0);
    assert!(!h.e.output_mut().map(|o| o.is_running()).unwrap_or(true));
    h.cmd(PlayerCommand::Play);
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Loading);
    assert!(s.starting);
    h.step_until(1500.0, |s| s.state.status == PlayerStatus::Playing);
}

#[test]
fn stop_during_starting_never_starts() {
    let mut h = H::warm();
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.5));
    assert!(h.snap().starting);
    h.cmd(PlayerCommand::Stop);
    h.clear();
    h.step(2000.0);
    assert!(h.states().is_empty(), "the abandoned start never reports");
    assert_eq!(h.status(), PlayerStatus::Idle);
}

#[test]
fn one_stream_failure_on_reopen_is_retried() {
    let mut h = H::with(config(false, false));
    three_chunk_read(&mut h);
    h.step(200.0);
    h.factory.script.lock().expect("script").push_back(Open::Stream);
    if let Some(sim) = h.e.output_mut().and_then(|o| o.as_sim()) {
        sim.test_lost = Some(false);
    }
    h.step(10.0);
    assert_eq!(h.status(), PlayerStatus::Playing, "status unchanged while the output is retried");
    let pos = h.snap().state.position_sec;
    h.step(1100.0);
    // Reopened after 1 s and continued from the heard position.
    assert!(h.snap().state.position_sec > pos);
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    assert!(h.e.take_exit().is_none());
}

#[test]
fn next_sentence_while_playing_moves_across_chunks() {
    let mut h = H::null();
    three_chunk_read(&mut h);
    h.step(100.0);
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert!((h.snap().state.position_sec - 0.5).abs() < 0.01);
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert!((h.snap().state.position_sec - 1.0).abs() < 0.01);
    assert_eq!(h.status(), PlayerStatus::Playing);
    // Past the last sentence with synthesis done: ended.
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert_eq!(h.status(), PlayerStatus::Ended);
}

#[test]
fn alignment_drives_the_char_index() {
    let mut h = H::null();
    h.send(start("s1", "abcd", &[0]));
    let Msg::Chunk(mut c) = chunk("s1", 0, 0, 4, 1.0) else { unreachable!() };
    c.alignment = Some(Arc::new(Alignment {
        chars: vec!["a".into(), "b".into(), "c".into(), "d".into()],
        starts: vec![0.0, 0.1, 0.2, 0.9],
        ends: vec![0.1, 0.2, 0.9, 1.0],
    }));
    h.send(Msg::Chunk(c));
    h.cmd(PlayerCommand::Pause);
    h.cmd(seek(0.5));
    assert_eq!(h.snap().state.char_index, 2, "aligned: c spans 0.2–0.9");
    h.cmd(seek(0.92));
    assert_eq!(h.snap().state.char_index, 3);
}

// ───────────────────────────── review findings (regressions) ─────────────────────────────

/// Finding: after an output reopen mid-read, a seek or skip while playing must not wait for a warm-up (STARTING),
/// and the warmer measures the reopened stream from its start.
#[test]
fn seek_after_a_device_change_is_not_gated() {
    let mut h = H::warm();
    h.send(start("s1", TEXT, &[0, 19, 38]));
    h.send(chunk("s1", 0, 0, utf16(TEXT), 12.0));
    h.send(done("s1", 1));
    h.step_until(2000.0, |s| s.state.status == PlayerStatus::Playing);
    h.step(300.0);
    let opens = *h.factory.opens.lock().expect("opens");
    // Headphones plugged in: Windows switches the default device; the 2 s poll reopens on it.
    *h.factory.device.lock().expect("device") = Some("headphones".into());
    let mut waited = 0.0;
    while *h.factory.opens.lock().expect("opens") == opens {
        assert!(waited < 2100.0, "the poll reopens within 2 s");
        h.step(TICK_MS);
        waited += TICK_MS;
    }
    h.step(100.0);
    assert_eq!(h.status(), PlayerStatus::Playing);
    // A seek right after the reopen (the new stream has rendered less than 1 s): no STARTING, no status change.
    h.clear();
    h.cmd(seek(1.0));
    let s = h.snap();
    assert_eq!(s.state.status, PlayerStatus::Playing);
    assert!(!s.starting);
    assert!(h.states().iter().all(|s| s.state.status == PlayerStatus::Playing));
    h.step(200.0);
    assert!(h.snap().state.position_sec > 1.1, "{}", h.snap().state.position_sec);
    h.cmd(skip_sentence(SkipDirection::Forward));
    assert_eq!(h.status(), PlayerStatus::Playing);
    assert!(!h.snap().starting);
    // Unplugged again: reopened on the speakers. More than 1 s later the new stream is warm although nothing asked
    // the warmer in between, so even a start that may be gated (play after pause) starts at once.
    *h.factory.device.lock().expect("device") = Some("speakers".into());
    h.step(3200.0);
    assert_eq!(*h.factory.opens.lock().expect("opens"), opens + 2);
    h.cmd(PlayerCommand::Pause);
    h.cmd(PlayerCommand::Play);
    assert_eq!(h.status(), PlayerStatus::Playing);
    assert!(!h.snap().starting);
}

/// Finding (reader-playback.md §12 #28): an output that opens but never renders must not hang the read.
#[test]
fn edge_28_frozen_output_is_reopened_and_the_read_continues() {
    let mut h = H::warm();
    h.factory.script.lock().expect("script").push_back(Open::Frozen);
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.5));
    h.send(done("s1", 1));
    h.step_until(4000.0, |s| s.state.status == PlayerStatus::Playing);
    assert_eq!(h.snap().state.position_sec, 0.0, "nothing renders");
    // Within ~2 s the stall is noticed and the output reopened; the read then plays to its end.
    let ms = h.step_until(4000.0, |s| s.state.status == PlayerStatus::Ended);
    assert!(ms < 3000.0, "{ms}");
    assert_eq!(*h.factory.opens.lock().expect("opens"), 2);
    assert!(h.snap().state.listened_sec > 0.4, "{}", h.snap().state.listened_sec);
    assert!(!h.all_events().iter().any(|e| matches!(e, PlayerEvent::Failed { .. })));
    assert!(h.e.take_exit().is_none());
}

#[test]
fn output_frozen_twice_moves_the_read_to_the_simulated_clock() {
    let mut h = H::warm();
    h.factory.script.lock().expect("script").extend([Open::Frozen, Open::Frozen]);
    h.send(start("s1", TEXT, &[0]));
    h.send(chunk("s1", 0, 0, 19, 0.5));
    h.send(done("s1", 1));
    h.step_until(4000.0, |s| s.state.status == PlayerStatus::Playing);
    let ms = h.step_until(6000.0, |s| s.state.status == PlayerStatus::Ended);
    assert!(ms < 5000.0, "{ms}");
    assert!(h.e.output_kind_is_sim_no_device());
    // The stalled device is left alone (the 2 s poll does not reopen it).
    h.send(start("s2", TEXT, &[0]));
    h.send(chunk("s2", 0, 0, 19, 3.0));
    h.step(2500.0);
    assert_eq!(*h.factory.opens.lock().expect("opens"), 2);
    assert_eq!(h.status(), PlayerStatus::Playing);
    assert!(!h.all_events().iter().any(|e| matches!(e, PlayerEvent::Failed { .. })));
}

/// Findings: after a flush the real ring still holds stale samples (only the callback discards them), so the fill
/// level must count the current generation only; a rate change continues exactly where the device was handed
/// audio, and neither it nor a seek leaves a hole or a click.
#[test]
fn rate_change_and_seek_on_the_real_ring_leave_no_gap_and_no_click() {
    let mut h = H::with(config(false, false));
    h.factory.script.lock().expect("script").push_back(Open::Ring);
    let text = "x".repeat(150);
    h.send(start("s1", &text, &[0]));
    h.send(tone_chunk("s1", 0, 0, 150, 4.0));
    h.send(done("s1", 1));
    h.step(600.0);
    let handed = h.ring().consumed().expect("handed position");
    let tags_before = h.ring().pushed_tags.len();
    h.cmd(PlayerCommand::Rate { rate: 1.5 });
    // The new feed starts where the device was handed audio: its first block ends one block (480 device frames at
    // 1.5x = 331 source frames) later, neither repeating nor skipping what was handed.
    let first_new = h.ring().pushed_tags[tags_before];
    assert!(first_new.generation > handed.generation);
    let advance = first_new.source_frame_end - handed.source_frame_end;
    assert!((320.0..=345.0).contains(&advance), "{advance}");
    h.step(400.0);
    h.cmd(seek(2.5));
    h.step(300.0);
    let left: Vec<f32> = h.ring().rendered_out.iter().step_by(2).copied().collect();
    // Skip the fade-in of the start.
    let (silence, step) = silence_and_steps(&left[480..]);
    assert!(silence < 48, "a silent run of {silence} samples");
    // A 220 Hz tone at 0.49 moves at most 0.0141 per sample; a 2 ms fade adds 0.005.
    assert!(step < 0.03, "a click: step {step}");
    assert!(h.snap().state.position_sec > 2.6);
    assert_eq!(h.e.underrun_frames(), 0);
}

/// Findings: under-runs are counted (and logged) only when the ring runs dry while speech is being fed.
#[test]
fn under_runs_count_only_when_the_ring_runs_dry_mid_read() {
    let mut h = H::with(config(false, false));
    h.factory.script.lock().expect("script").push_back(Open::Ring);
    three_chunk_read(&mut h);
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    h.step(300.0);
    assert_eq!(h.e.underrun_frames(), 0, "a normal read and its drained end never under-run");
    let text = "x".repeat(150);
    h.send(start("s2", &text, &[0]));
    h.send(tone_chunk("s2", 0, 0, 150, 8.0));
    h.step(300.0);
    // The player thread is starved for 300 ms while the device keeps playing: the ring (150 ms) runs dry.
    h.now += 300.0;
    h.step(10.0);
    let frames = h.e.underrun_frames();
    assert!((4_800..=9_600).contains(&frames), "{frames}");
    // Listening only counts what the device played.
    assert_eq!(h.status(), PlayerStatus::Playing);
    let s = h.snap();
    assert!(s.state.listened_sec <= s.state.position_sec + 0.05, "{:?}", s.state);
    // A new device counts from zero: its under-runs are seen too.
    h.factory.script.lock().expect("script").push_back(Open::Ring);
    let opens = *h.factory.opens.lock().expect("opens");
    *h.factory.device.lock().expect("device") = Some("headphones".into());
    for _ in 0..250 {
        if *h.factory.opens.lock().expect("opens") > opens {
            break;
        }
        h.step(TICK_MS);
    }
    assert_eq!(*h.factory.opens.lock().expect("opens"), opens + 1, "reopened on the new default device");
    h.step(300.0);
    assert_eq!(h.e.underrun_frames(), frames, "a reopen is not an under-run");
    h.now += 300.0;
    h.step(10.0);
    let more = h.e.underrun_frames() - frames;
    assert!((4_800..=9_600).contains(&more), "{more}");
}

/// Finding: leaving the no-device clock for a real device while the next chunk is still decoding must not let
/// the old feed (in an old generation) play on the new device during STARTING, or replay that chunk afterwards.
#[test]
fn leaving_the_no_device_clock_restarts_the_feed_in_a_new_generation() {
    let mut h = H::warm();
    // No device at first. Later opens succeed, but no default device is reported, so only the 60 s creation retry
    // (not the 2 s poll) picks the device up.
    *h.factory.device.lock().expect("device") = None;
    h.factory.script.lock().expect("script").push_back(Open::NoDevice);
    h.send(start("s1", TEXT, &[0, 19]));
    h.send(chunk("s1", 0, 0, 19, 1.0));
    assert!(h.e.output_kind_is_sim_no_device());
    assert_eq!(h.status(), PlayerStatus::Playing);
    // Chunk 1 arrives but its decode is held.
    h.auto_decode = false;
    h.send(chunk("s1", 1, 19, utf16(TEXT), 1.0));
    h.step(200.0);
    // A minute passes (the thread slept): the creation retry is due when chunk 0 ends.
    h.now += 60_000.0;
    h.step_until(2000.0, |s| s.state.status == PlayerStatus::Loading);
    assert!(!h.e.output_kind_is_sim_no_device(), "the device was opened");
    assert!(h.snap().starting, "warming the new device up");
    // Chunk 1 decodes during the warm-up: nothing is fed to the device before speech may start …
    h.decode();
    h.step(300.0);
    assert!(h.captured().is_empty(), "fed during STARTING: {:?}", h.captured().first().map(|b| b.0));
    h.step_until(2000.0, |s| s.state.status == PlayerStatus::Playing);
    h.step(300.0);
    // … then chunk 1 plays from its start, in one generation, never going back.
    let blocks = h.captured();
    let first = blocks.first().expect("blocks").0;
    assert_eq!(first.chunk, 1);
    assert!(first.source_frame_end < 1_000.0, "{first:?}");
    for w in blocks.windows(2) {
        assert_eq!(w[1].0.generation, w[0].0.generation);
        assert!(w[1].0.source_frame_end > w[0].0.source_frame_end, "{:?} then {:?}", w[0].0, w[1].0);
    }
}

/// Finding: with chunks cur+1 and cur+2 undecodable, chunk cur+3 is decoded once and joined gaplessly.
#[test]
fn failed_chunks_ahead_do_not_make_the_next_one_decode_again() {
    let mut h = H::null();
    let text = "x".repeat(80);
    h.send(start("s1", &text, &[0]));
    h.send(chunk("s1", 0, 0, 20, 0.5));
    h.send(bad_chunk("s1", 1, 20, 40, 0.3));
    h.send(bad_chunk("s1", 2, 40, 60, 0.3));
    h.send(chunk("s1", 3, 60, 80, 0.5));
    h.send(done("s1", 4));
    h.step_until(3000.0, |s| s.state.status == PlayerStatus::Ended);
    assert_eq!(h.decoded_indices.iter().filter(|i| **i == 3).count(), 1, "{:?}", h.decoded_indices);
    // Gapless over the failed chunks: never buffering once playing.
    let states = h.states();
    let playing_from = states.iter().position(|s| s.state.status == PlayerStatus::Playing).expect("played");
    let after = &states[playing_from..];
    assert!(after.iter().all(|s| matches!(s.state.status, PlayerStatus::Playing | PlayerStatus::Ended)));
    assert!(h.snap().state.listened_sec > 0.9, "{}", h.snap().state.listened_sec);
}
