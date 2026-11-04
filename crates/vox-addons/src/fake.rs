//! The fake helper (`vox-addon-fake`, built for the integration tests and never packaged; docs/addons-design.md
//! "Fake helper"). It speaks protocol 1 like a real helper and misbehaves on request.
//!
//! - Voices `alpha` ("Alpha", female, en-US) and `beta` ("Beta", male, en-GB).
//! - `synthesize`: a 24 kHz mono 16-bit sine, 40 ms per character (at most 20 s), 440 Hz for alpha and 220 Hz for
//!   beta, with a word cue per whitespace-separated word in the declared offset unit.
//! - It honours `cancel`, exits on `quit` and on stdin EOF.
//!
//! Flags: `--offsets utf8|codepoint|utf16`, `--heartbeat-ms N` (progress frames while slow), `--tag T` (the hello
//! `version`, and in the log), `--count-file PATH` (a line per spawn), `--log-file PATH` (a line per request), and
//! `--mode normal|exit-at-start|no-hello|slow-hello:MS|protocol:2|banner|no-eof-exit|spawn-grandchild|stderr-flood`.
//!
//! Triggers at the start of a `synthesize` text: `@crash`, `@crash-after:N` (this and the next N-1 requests answer,
//! the one after crashes), `@hang`, `@slow:MS`, `@ignore-cancel`, `@garbage`, `@huge[:BYTES]`,
//! `@bad-wav[:riff|size|8bit]`, `@long-wav`, `@bad-offsets`, `@no-words`, `@err:CODE`, `@stale`.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError};
use serde_json::{json, Value};
use vox_core::text16::utf16_len;
use vox_tts::wav::encode_wav;

use crate::protocol::{LineReader, OffsetUnit};

const RATE: u32 = 24_000;

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Normal,
    ExitAtStart,
    NoHello,
    SlowHello(u64),
    Protocol2,
    Banner,
    NoEofExit,
    SpawnGrandchild,
    StderrFlood,
    /// The grandchild of `spawn-grandchild`: sleeps.
    Sleep,
}

struct Opts {
    offsets: OffsetUnit,
    offsets_name: &'static str,
    heartbeat: Option<Duration>,
    tag: String,
    count_file: Option<PathBuf>,
    log_file: Option<PathBuf>,
    mode: Mode,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { offsets: OffsetUnit::Utf8, offsets_name: "utf8", heartbeat: None, tag: "1.0.0".into(), count_file: None, log_file: None, mode: Mode::Normal };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--offsets" => {
                let v = value()?;
                (o.offsets, o.offsets_name) = match v.as_str() {
                    "utf8" => (OffsetUnit::Utf8, "utf8"),
                    "codepoint" => (OffsetUnit::Codepoint, "codepoint"),
                    "utf16" => (OffsetUnit::Utf16, "utf16"),
                    _ => return Err(format!("bad --offsets {v}")),
                };
            }
            "--heartbeat-ms" => o.heartbeat = Some(Duration::from_millis(value()?.parse().map_err(|_| "bad --heartbeat-ms")?)),
            "--tag" => o.tag = value()?,
            "--count-file" => o.count_file = Some(value()?.into()),
            "--log-file" => o.log_file = Some(value()?.into()),
            "--mode" => {
                let v = value()?;
                o.mode = match v.as_str() {
                    "normal" => Mode::Normal,
                    "exit-at-start" => Mode::ExitAtStart,
                    "no-hello" => Mode::NoHello,
                    "protocol:2" => Mode::Protocol2,
                    "banner" => Mode::Banner,
                    "no-eof-exit" => Mode::NoEofExit,
                    "spawn-grandchild" => Mode::SpawnGrandchild,
                    "stderr-flood" => Mode::StderrFlood,
                    "sleep" => Mode::Sleep,
                    s => match s.strip_prefix("slow-hello:").and_then(|ms| ms.parse().ok()) {
                        Some(ms) => Mode::SlowHello(ms),
                        None => return Err(format!("bad --mode {v}")),
                    },
                };
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(o)
}

fn append(path: &Option<PathBuf>, line: &str) {
    if let Some(p) = path {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// What stdin delivered: a message, or the end of the stream.
enum Input {
    Msg(Value),
    Eof,
}

/// How a wait inside a request ended.
enum Waited {
    Done,
    Cancelled,
    Exit,
}

struct Fake<W: Write> {
    o: Opts,
    out: W,
    rx: Receiver<Input>,
    /// Requests that arrived while one was being served.
    queued: VecDeque<Value>,
    /// `@crash-after`: requests left before the crash.
    crash_in: Option<u32>,
    _grandchild: Option<std::process::Child>,
}

/// Runs the fake helper; returns its exit code.
#[doc(hidden)]
pub fn run(args: Vec<String>, stdin: impl Read + Send + 'static, stdout: impl Write, mut stderr: impl Write) -> i32 {
    let o = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(stderr, "vox-addon-fake: {e}");
            return 64;
        }
    };
    append(&o.count_file, &format!("spawn {}", std::process::id()));
    match o.mode {
        Mode::ExitAtStart => return 2,
        Mode::Sleep => {
            std::thread::sleep(Duration::from_secs(60));
            return 0;
        }
        _ => {}
    }
    let _ = writeln!(stderr, "vox-addon-fake {} starting", o.tag);
    let (tx, rx) = unbounded();
    std::thread::spawn(move || {
        let mut reader = LineReader::new(BufReader::new(stdin), 64 << 10);
        while let Ok(Some(line)) = reader.next_line() {
            let msg = serde_json::from_slice(&line).unwrap_or(Value::Null);
            if tx.send(Input::Msg(msg)).is_err() {
                return;
            }
        }
        let _ = tx.send(Input::Eof);
    });
    let mut fake = Fake { o, out: stdout, rx, queued: VecDeque::new(), crash_in: None, _grandchild: None };
    if fake.o.mode == Mode::Banner && !fake.write_raw(b"vox-addon-fake: hello, world\n") {
        return 1;
    }
    loop {
        let msg = match fake.queued.pop_front() {
            Some(m) => m,
            None => match fake.rx.recv() {
                Ok(Input::Msg(m)) => m,
                Ok(Input::Eof) | Err(_) => {
                    if fake.o.mode == Mode::NoEofExit {
                        std::thread::sleep(Duration::from_secs(60));
                    }
                    return 0;
                }
            },
        };
        if let Some(code) = fake.handle(msg, &mut stderr) {
            return code;
        }
    }
}

impl<W: Write> Fake<W> {
    fn write_raw(&mut self, bytes: &[u8]) -> bool {
        self.out.write_all(bytes).and_then(|()| self.out.flush()).is_ok()
    }

    fn send(&mut self, v: Value) -> bool {
        let mut line = v.to_string().into_bytes();
        line.push(b'\n');
        self.write_raw(&line)
    }

    /// Handles one message; `Some(code)` to exit.
    fn handle(&mut self, msg: Value, stderr: &mut impl Write) -> Option<i32> {
        let id = msg.get("id").and_then(Value::as_u64).unwrap_or(0);
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        append(&self.o.log_file, &format!("{kind} {id} {}", self.o.tag));
        let ok = match kind.as_str() {
            "hello" => return self.hello(id, stderr),
            "voices" => self.send(json!({"id": id, "type": "voices", "voices": [
                {"key": "alpha", "name": "Alpha", "gender": "female", "language": "en-US", "accent": "American"},
                {"key": "beta", "name": "Beta", "gender": "male", "language": "en-GB", "accent": "British"}
            ]})),
            "synthesize" => return self.synthesize(id, &msg),
            "cancel" => true,
            "quit" => return Some(0),
            _ => self.send(json!({"id": id, "type": "error", "code": "unsupported", "message": format!("no {kind:?}")})),
        };
        (!ok).then_some(1)
    }

    fn hello(&mut self, id: u64, stderr: &mut impl Write) -> Option<i32> {
        match self.o.mode {
            Mode::NoHello => return None,
            Mode::SlowHello(ms) => std::thread::sleep(Duration::from_millis(ms)),
            Mode::Protocol2 => {
                self.send(json!({"id": id, "type": "error", "code": "unsupported_protocol", "supported": {"min": 2, "max": 2}}));
                return Some(0);
            }
            Mode::SpawnGrandchild => {
                let me = std::env::current_exe().ok()?;
                let child = Command::new(me).args(["--mode", "sleep"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()?;
                append(&self.o.log_file, &format!("grandchild {}", child.id()));
                self._grandchild = Some(child);
            }
            Mode::StderrFlood => {
                let line = format!("{}\n", "x".repeat(1023));
                for _ in 0..10_240 {
                    if stderr.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                }
            }
            _ => {}
        }
        let ok = self.send(json!({
            "id": id, "type": "hello", "protocol": 1, "name": "vox-addon-fake", "version": self.o.tag,
            "offsets": self.o.offsets_name, "word_timing": "exact"
        }));
        (!ok).then_some(1)
    }

    fn synthesize(&mut self, id: u64, msg: &Value) -> Option<i32> {
        let voice = msg.get("voice").and_then(Value::as_str).unwrap_or("").to_string();
        let text = msg.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let word_timing = msg.get("word_timing").and_then(Value::as_bool).unwrap_or(false);
        append(&self.o.log_file, &format!("text {id} {} {}", self.o.tag, text.chars().take(40).collect::<String>()));
        let trigger = text.trim_start().strip_prefix('@').map(|t| t.split_whitespace().next().unwrap_or("").to_string()).unwrap_or_default();
        let (name, arg) = trigger.split_once(':').map_or((trigger.as_str(), ""), |(n, a)| (n, a));

        if let Some(left) = self.crash_in {
            if left == 0 {
                return Some(self.crash(id));
            }
        }
        let freq = match voice.as_str() {
            "alpha" => 440.0,
            "beta" => 220.0,
            _ => {
                let ok = self.send(json!({"id": id, "type": "error", "code": "unknown_voice", "message": format!("no voice {voice:?}")}));
                return (!ok).then_some(1);
            }
        };
        let seconds = (text.chars().count() as f64 * 0.04).min(20.0);
        let mut words = Some(self.word_cues(&text));
        let mut wav = tone(seconds, freq);
        match name {
            "crash" => return Some(self.crash(id)),
            "crash-after" => self.crash_in = Some(arg.parse().unwrap_or(1)),
            "hang" => return None,
            "slow" => match self.wait(id, arg.parse().unwrap_or(1000), true) {
                Waited::Done => {}
                Waited::Cancelled => {
                    let ok = self.send(json!({"id": id, "type": "error", "code": "cancelled", "message": "cancelled"}));
                    return (!ok).then_some(1);
                }
                Waited::Exit => return Some(0),
            },
            "ignore-cancel" => {
                if let Waited::Exit = self.wait(id, 30_000, false) {
                    return Some(0);
                }
            }
            "garbage" => return (!self.write_raw(b"this is not json\n")).then_some(1),
            "huge" => {
                let n: usize = arg.parse().unwrap_or(vox_core::constants::ADDON_MAX_LINE + 1024);
                let head = format!("{{\"id\":{id},\"type\":\"audio\",\"format\":\"wav\",\"wav\":\"");
                let ok = self.write_raw(head.as_bytes()) && self.write_raw(&vec![b'A'; n]) && self.write_raw(b"\"}\n");
                return (!ok).then_some(1);
            }
            "bad-wav" => match arg {
                "size" => wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes()),
                "8bit" => wav[34] = 8,
                _ => wav[0..4].copy_from_slice(b"RIFX"),
            },
            "long-wav" => wav = tone((0.5 * utf16_len(&text) as f64 + 15.0).min(700.0), freq),
            "bad-offsets" => {
                let mid = text.char_indices().find(|(_, c)| c.len_utf8() > 1).map_or(1, |(i, _)| i + 1);
                words = Some(json!([
                    {"from": 1_000_000, "to": 1_000_005, "start": 0.0},
                    {"from": 5, "to": 2, "start": 0.1},
                    {"from": mid, "to": mid + 1, "start": 0.2},
                    {"from": 0.5, "to": 2, "start": 0.3}
                ]));
            }
            "no-words" => words = None,
            "err" => {
                let ok = self.send(json!({"id": id, "type": "error", "code": arg, "message": "as asked"}));
                return (!ok).then_some(1);
            }
            "stale" => {
                let old = tone(0.1, freq);
                if !self.send(json!({"id": id.saturating_sub(1), "type": "audio", "format": "wav", "wav": b64(&old)})) {
                    return Some(1);
                }
            }
            _ => {}
        }
        if let Some(left) = self.crash_in.as_mut() {
            *left = left.saturating_sub(1);
        }
        let mut reply = json!({"id": id, "type": "audio", "format": "wav", "wav": b64(&wav), "offsets": self.o.offsets_name});
        if let (true, Some(w)) = (word_timing, words) {
            reply["words"] = w;
        }
        (!self.send(reply)).then_some(1)
    }

    /// Half a line, then exit 3.
    fn crash(&mut self, id: u64) -> i32 {
        self.write_raw(format!("{{\"id\":{id},\"type\":\"au").as_bytes());
        3
    }

    /// Spends `ms` on request `id`, with progress heartbeats when asked; a `cancel` for it ends the wait when
    /// `honour_cancel`. Other requests are kept for later.
    fn wait(&mut self, id: u64, ms: u64, honour_cancel: bool) -> Waited {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut beat = Instant::now();
        loop {
            let now = Instant::now();
            if now >= end {
                return Waited::Done;
            }
            if let Some(every) = self.o.heartbeat {
                if now.duration_since(beat) >= every {
                    if !self.send(json!({"id": id, "type": "progress", "stage": "synthesizing", "fraction": 0.5})) {
                        return Waited::Exit;
                    }
                    beat = now;
                }
            }
            let step = (end - now).min(Duration::from_millis(20));
            match self.rx.recv_timeout(step) {
                Ok(Input::Msg(m)) => {
                    let kind = m.get("type").and_then(Value::as_str).unwrap_or("");
                    match kind {
                        "cancel" if m.get("target").and_then(Value::as_u64) == Some(id) => {
                            append(&self.o.log_file, &format!("cancel {id} {}", self.o.tag));
                            if honour_cancel {
                                return Waited::Cancelled;
                            }
                        }
                        "quit" => return Waited::Exit,
                        _ => self.queued.push_back(m),
                    }
                }
                Ok(Input::Eof) | Err(RecvTimeoutError::Disconnected) => return Waited::Exit,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// A cue per whitespace-separated word, 40 ms per character from the start.
    fn word_cues(&self, text: &str) -> Value {
        let mut cues = Vec::new();
        let mut start: Option<(usize, usize, usize)> = None; // (byte, codepoint, utf16) of the word start
        let (mut cp, mut u16) = (0usize, 0usize);
        let unit = self.o.offsets;
        let pick = |b: usize, c: usize, u: usize| match unit {
            OffsetUnit::Utf8 => b,
            OffsetUnit::Codepoint => c,
            OffsetUnit::Utf16 => u,
        };
        for (b, ch) in text.char_indices() {
            if ch.is_whitespace() {
                if let Some((sb, sc, su)) = start.take() {
                    cues.push(json!({"from": pick(sb, sc, su), "to": pick(b, cp, u16), "start": sc as f64 * 0.04}));
                }
            } else if start.is_none() {
                start = Some((b, cp, u16));
            }
            cp += 1;
            u16 += ch.len_utf16();
        }
        if let Some((sb, sc, su)) = start {
            cues.push(json!({"from": pick(sb, sc, su), "to": pick(text.len(), cp, u16), "start": sc as f64 * 0.04}));
        }
        Value::Array(cues)
    }
}

fn tone(seconds: f64, freq: f64) -> Vec<u8> {
    let n = ((seconds * f64::from(RATE)) as usize).max(1);
    let samples: Vec<f32> = (0..n).map(|i| (0.2 * (std::f64::consts::TAU * freq * i as f64 / f64::from(RATE)).sin()) as f32).collect();
    encode_wav(&samples, RATE)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
