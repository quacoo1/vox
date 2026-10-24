//! Threaded tests of the `Player` handle (null output, real player thread and decode worker).

use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use vox_core::player::{next_rate, PlayerStatus};
use vox_core::read::ReadErrorCode;
use vox_core::tts::MIME_WAV;

use super::*;
use crate::decode::tests::mock_wav;
use crate::output::OutputError;

type Log = Arc<StdMutex<Vec<PlayerEvent>>>;

fn spawn() -> (Player, Log) {
    let log: Log = Arc::default();
    let sink = log.clone();
    let config = PlayerConfig {
        report_interval: Duration::from_millis(500),
        warm_output: false,
        null_output: true,
        name: "test",
    };
    let player = Player::spawn(config, Arc::new(move |e| sink.lock().expect("log").push(e)));
    (player, log)
}

fn states(log: &Log) -> Vec<PlayerSnapshot> {
    log.lock()
        .expect("log")
        .iter()
        .filter_map(|e| match e {
            PlayerEvent::State(s) => Some(s.clone()),
            PlayerEvent::Failed { .. } => None,
        })
        .collect()
}

fn wait_for(what: &str, timeout_ms: u64, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn session(id: &str, rate: f64, volume: f64) -> TtsSessionStart {
    TtsSessionStart {
        session_id: id.into(),
        read_id: format!("read-{id}"),
        text: Arc::from("Hello there. General Kenobi."),
        title: "Hello there.".into(),
        voice_name: "Ava".into(),
        voice_id: "v".into(),
        source_app: None,
        estimated_duration_sec: 2.0,
        chunk_count: 1,
        rate,
        volume,
        highlight_words: true,
        from_cache: false,
        sentences: Arc::from(vec![0u32, 13]),
    }
}

fn chunk(id: &str, index: u32, secs: f64) -> TtsChunk {
    TtsChunk {
        session_id: id.into(),
        index,
        text_start: 0,
        text_end: 28,
        audio: Arc::from(mock_wav(22_050, (secs * 22_050.0) as usize)),
        mime: MIME_WAV.into(),
        alignment: None,
        duration_sec: secs,
    }
}

#[test]
fn plays_a_read_to_the_end_on_the_simulated_clock() {
    let (player, log) = spawn();
    player.start_session(session("s1", 1.0, 1.0));
    player.add_chunk(chunk("s1", 0, 0.4));
    player.finish_session(TtsSessionDone { session_id: "s1".into(), total_chunks: 1 });
    wait_for("ended", 5000, || player.snapshot().state.status == PlayerStatus::Ended);
    let s = player.snapshot();
    assert!((s.state.position_sec - 0.4).abs() < 1e-6);
    // Counting starts when the first block is reported played (one tick of the thread's coarse timer later).
    assert!(s.state.listened_sec > 0.25 && s.state.listened_sec <= 0.4, "{}", s.state.listened_sec);
    let statuses: Vec<PlayerStatus> = states(&log).iter().map(|s| s.state.status).collect();
    assert_eq!(statuses.first(), Some(&PlayerStatus::Loading));
    assert!(statuses.contains(&PlayerStatus::Playing));
    assert_eq!(statuses.last(), Some(&PlayerStatus::Ended));
    player.shutdown();
}

#[test]
fn stop_reports_the_session_then_no_session() {
    let (player, log) = spawn();
    player.start_session(session("s1", 1.0, 1.0));
    player.add_chunk(chunk("s1", 0, 2.0));
    wait_for("playing", 5000, || player.snapshot().state.status == PlayerStatus::Playing);
    std::thread::sleep(Duration::from_millis(100));
    let before = states(&log).len();
    player.command(PlayerCommand::Stop);
    wait_for("two idle reports", 5000, || states(&log).len() >= before + 2);
    std::thread::sleep(Duration::from_millis(50));
    let all = states(&log);
    let tail = &all[before..];
    assert_eq!(tail.len(), 2, "{tail:?}");
    assert_eq!(tail[0].state.status, PlayerStatus::Idle);
    assert_eq!(tail[0].state.session_id.as_deref(), Some("s1"));
    assert!(tail[1].session.is_none());
    assert_eq!(tail[1].state.status, PlayerStatus::Idle);
    assert!(player.snapshot().session.is_none());
    player.shutdown();
}

#[test]
fn supersede_reports_old_idle_then_new_loading() {
    let (player, log) = spawn();
    player.start_session(session("s1", 1.0, 1.0));
    player.add_chunk(chunk("s1", 0, 2.0));
    wait_for("playing", 5000, || player.snapshot().state.status == PlayerStatus::Playing);
    let before = states(&log).len();
    player.start_session(session("s2", 1.0, 1.0));
    wait_for("two reports", 5000, || states(&log).len() >= before + 2);
    std::thread::sleep(Duration::from_millis(50));
    let all = states(&log);
    let tail: Vec<_> = all[before..].iter().filter(|s| s.state.status != PlayerStatus::Playing).cloned().collect();
    assert_eq!(tail[0].state.status, PlayerStatus::Idle);
    assert_eq!(tail[0].state.session_id.as_deref(), Some("s1"));
    assert_eq!(tail[1].state.status, PlayerStatus::Loading);
    assert_eq!(tail[1].state.session_id.as_deref(), Some("s2"));
    // Nothing sessionless in between.
    assert!(all[before..].iter().all(|s| s.session.is_some()));
    player.shutdown();
}

#[test]
fn rate_and_volume_targets_are_visible_at_once() {
    let (player, _log) = spawn();
    // A quick double click on the speed pill advances twice (CycleRate reads the snapshot).
    player.command(PlayerCommand::Rate { rate: next_rate(player.snapshot().state.rate) });
    player.command(PlayerCommand::Rate { rate: next_rate(player.snapshot().state.rate) });
    assert_eq!(player.snapshot().state.rate, 1.5);
    player.command(PlayerCommand::Volume { volume: 0.25 });
    assert_eq!(player.snapshot().state.volume, 0.25);
    // Clamped like the thread does; non-finite values change nothing.
    player.command(PlayerCommand::Rate { rate: 7.0 });
    assert_eq!(player.snapshot().state.rate, 3.0);
    player.command(PlayerCommand::Rate { rate: f64::NAN });
    assert_eq!(player.snapshot().state.rate, 3.0);
    // start_session's payload too.
    player.start_session(session("s1", 0.75, 0.5));
    let s = player.snapshot();
    assert_eq!((s.state.rate, s.state.volume), (0.75, 0.5));
    // And the thread settles on the same values.
    wait_for("session", 5000, || player.snapshot().session.is_some());
    std::thread::sleep(Duration::from_millis(50));
    let s = player.snapshot();
    assert_eq!((s.state.rate, s.state.volume), (0.75, 0.5));
    player.shutdown();
}

#[test]
fn idle_player_thread_does_not_tick() {
    let (player, _log) = spawn();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(player.ticks(), 0, "an idle player blocks on its channel");
    // A read ticks while it plays …
    player.start_session(session("s1", 1.0, 1.0));
    player.add_chunk(chunk("s1", 0, 0.3));
    player.finish_session(TtsSessionDone { session_id: "s1".into(), total_chunks: 1 });
    wait_for("ended", 5000, || player.snapshot().state.status == PlayerStatus::Ended);
    // (Windows timers are coarse and the machine may be busy: just check that it ticked.)
    assert!(player.ticks() > 0);
    player.command(PlayerCommand::Stop);
    std::thread::sleep(Duration::from_millis(100));
    // … and stops ticking afterwards.
    let after = player.ticks();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(player.ticks(), after);
    player.shutdown();
}

#[test]
fn a_panicking_player_restarts_with_the_session_in_error() {
    let (player, log) = spawn();
    player.start_session(session("s1", 1.25, 0.5));
    player.add_chunk(chunk("s1", 0, 2.0));
    wait_for("playing", 5000, || player.snapshot().state.status == PlayerStatus::Playing);
    player.inject_panic();
    wait_for("failed", 5000, || log.lock().expect("log").iter().any(|e| matches!(e, PlayerEvent::Failed { .. })));
    let events = log.lock().expect("log").clone();
    let failed_at = events.iter().position(|e| matches!(e, PlayerEvent::Failed { .. })).expect("failed");
    // The error State comes right before Failed.
    match &events[failed_at - 1] {
        PlayerEvent::State(s) => {
            assert_eq!(s.state.status, PlayerStatus::Error);
            assert_eq!(s.state.session_id.as_deref(), Some("s1"));
            assert_eq!(s.state.error.as_deref(), Some(vox_core::copy::PLAYBACK_FAILED));
            assert_eq!(s.state.error_code, Some(ReadErrorCode::Unknown));
            assert_eq!((s.state.rate, s.state.volume), (1.25, 0.5), "rate and volume survive the restart");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        events[failed_at],
        PlayerEvent::Failed { session_id: Some("s1".into()), message: vox_core::copy::PLAYBACK_FAILED.into() }
    );
    // The restarted player works.
    player.start_session(session("s2", 1.0, 1.0));
    player.add_chunk(chunk("s2", 0, 0.2));
    player.finish_session(TtsSessionDone { session_id: "s2".into(), total_chunks: 1 });
    wait_for("second read ended", 5000, || {
        let s = player.snapshot();
        s.state.session_id.as_deref() == Some("s2") && s.state.status == PlayerStatus::Ended
    });
    player.shutdown();
}

#[test]
fn shutdown_is_idempotent_and_later_calls_are_harmless() {
    let (player, log) = spawn();
    player.start_session(session("s1", 1.0, 1.0));
    wait_for("session", 5000, || player.snapshot().session.is_some());
    let clone = player.clone();
    player.shutdown();
    clone.shutdown();
    let n = log.lock().expect("log").len();
    player.add_chunk(chunk("s1", 0, 0.2));
    player.command(PlayerCommand::Play);
    player.preload_output();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(log.lock().expect("log").len(), n, "no reports after shutdown");
    let _ = player.snapshot();
}

#[test]
fn dropping_every_handle_ends_the_thread() {
    let (player, _log) = spawn();
    let weak = Arc::downgrade(&player.inner.shared);
    drop(player);
    wait_for("thread exit", 5000, || weak.upgrade().is_none());
}

/// Plays a short tone through the real default device. Needs a Windows desktop with speakers:
/// `cargo test -p vox-audio -- --ignored real_device`.
#[test]
#[ignore]
fn real_device_plays_a_read() {
    let log: Log = Arc::default();
    let sink = log.clone();
    let player = Player::spawn(PlayerConfig::default(), Arc::new(move |e| sink.lock().expect("log").push(e)));
    player.preload_output();
    player.start_session(session("s1", 1.0, 1.0));
    player.add_chunk(chunk("s1", 0, 1.5));
    player.finish_session(TtsSessionDone { session_id: "s1".into(), total_chunks: 1 });
    wait_for("ended", 10_000, || player.snapshot().state.status == PlayerStatus::Ended);
    assert!(player.snapshot().state.listened_sec > 1.0);
    player.shutdown();
}

#[test]
fn handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync + Clone + 'static>() {}
    assert_send_sync::<Player>();
    assert_send_sync::<PlayerEvent>();
    assert_send_sync::<PlayerConfig>();
}

/// Pause, rate, seek and stop on the real default device (flush paths, fades, device-reported positions). Needs a
/// Windows desktop with speakers: `cargo test -p vox-audio -- --ignored real_device`.
#[test]
#[ignore]
fn real_device_transport() {
    let log: Log = Arc::default();
    let sink = log.clone();
    let player = Player::spawn(PlayerConfig::default(), Arc::new(move |e| sink.lock().expect("log").push(e)));
    player.start_session(session("s1", 1.0, 0.5));
    player.add_chunk(chunk("s1", 0, 4.0));
    player.finish_session(TtsSessionDone { session_id: "s1".into(), total_chunks: 1 });
    wait_for("playing", 5000, || player.snapshot().state.status == PlayerStatus::Playing);
    std::thread::sleep(Duration::from_millis(600));
    let p1 = player.snapshot().state.position_sec;
    assert!(p1 > 0.2, "the device reports progress: {p1}");
    player.command(PlayerCommand::Rate { rate: 1.5 });
    std::thread::sleep(Duration::from_millis(400));
    player.command(PlayerCommand::Pause);
    std::thread::sleep(Duration::from_millis(100));
    let paused_at = player.snapshot().state.position_sec;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(player.snapshot().state.position_sec, paused_at, "no progress while paused");
    player.command(PlayerCommand::Seek { position_sec: 3.0 });
    player.command(PlayerCommand::Play);
    wait_for("ended", 5000, || player.snapshot().state.status == PlayerStatus::Ended);
    player.command(PlayerCommand::Stop);
    wait_for("idle", 2000, || player.snapshot().session.is_none());
    assert!(!log.lock().expect("log").iter().any(|e| matches!(e, PlayerEvent::Failed { .. })));
    player.shutdown();
}

/// Opens that always fail with `OutputError::Stream`.
struct BrokenOutput;

impl crate::engine::OutputFactory for BrokenOutput {
    fn open(&mut self) -> Result<Box<dyn crate::output::Sink>, OutputError> {
        Err(OutputError::Stream("scripted".into()))
    }
    fn default_device_id(&mut self) -> Option<String> {
        Some("speakers".into())
    }
}

#[test]
fn three_stream_failures_restart_the_player_with_no_audio_output() {
    let log: Log = Arc::default();
    let sink = log.clone();
    let config = PlayerConfig {
        report_interval: Duration::from_millis(500),
        warm_output: true,
        null_output: false,
        name: "test",
    };
    let player = Player::spawn_with(
        config,
        Arc::new(move |e| sink.lock().expect("log").push(e)),
        Arc::new(|| Box::new(BrokenOutput) as Box<dyn crate::engine::OutputFactory>),
    );
    player.start_session(session("s1", 1.0, 1.0));
    player.add_chunk(chunk("s1", 0, 1.0));
    // Opens at 0 s, 1 s and 2 s fail → the player restarts with the session in error.
    wait_for("failed", 6000, || log.lock().expect("log").iter().any(|e| matches!(e, PlayerEvent::Failed { .. })));
    let events = log.lock().expect("log").clone();
    let at = events.iter().position(|e| matches!(e, PlayerEvent::Failed { .. })).expect("failed");
    match &events[at - 1] {
        PlayerEvent::State(s) => {
            assert_eq!(s.state.status, PlayerStatus::Error);
            assert_eq!(s.state.error.as_deref(), Some(vox_core::copy::NO_AUDIO_OUTPUT));
            assert_eq!(s.state.session_id.as_deref(), Some("s1"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        events[at],
        PlayerEvent::Failed { session_id: Some("s1".into()), message: vox_core::copy::NO_AUDIO_OUTPUT.into() }
    );
    // Statuses before the failure never left loading/playing (no spurious error).
    assert!(events[..at - 1]
        .iter()
        .all(|e| matches!(e, PlayerEvent::State(s) if s.state.status != PlayerStatus::Error)));
    player.shutdown();
}

/// Finding: if the decode worker cannot be started, the player thread decodes inline and reads still play.
#[test]
fn decodes_inline_when_the_worker_cannot_start() {
    let log: Log = Arc::default();
    let sink = log.clone();
    let config = PlayerConfig {
        report_interval: Duration::from_millis(500),
        warm_output: false,
        null_output: true,
        name: "test",
    };
    let player = Player::spawn_decoding_inline(config, Arc::new(move |e| sink.lock().expect("log").push(e)));
    player.start_session(session("s1", 1.0, 1.0));
    player.add_chunk(chunk("s1", 0, 0.3));
    player.finish_session(TtsSessionDone { session_id: "s1".into(), total_chunks: 1 });
    wait_for("ended", 5000, || player.snapshot().state.status == PlayerStatus::Ended);
    assert!((player.snapshot().state.position_sec - 0.3).abs() < 1e-6);
    assert!(!log.lock().expect("log").iter().any(|e| matches!(e, PlayerEvent::Failed { .. })));
    player.shutdown();
}

#[test]
fn inline_decoder_delivers_results_and_failures() {
    let (tx, rx) = unbounded::<DecodeResult>();
    let mut d = InlineDecoder { results: tx };
    d.request(DecodeJob { serial: 7, index: 2, audio: Arc::from(mock_wav(22_050, 100)), mime: MIME_WAV.into() });
    d.request(DecodeJob { serial: 7, index: 3, audio: Arc::from(&b"not audio"[..]), mime: MIME_WAV.into() });
    let a = rx.try_recv().expect("first result");
    assert_eq!((a.serial, a.index), (7, 2));
    assert_eq!(a.result.expect("decoded").frames(), 100);
    let b = rx.try_recv().expect("second result");
    assert_eq!(b.index, 3);
    assert!(b.result.is_err());
}

/// Finding: a decode channel with no sender left is always ready; the loop must stop selecting it, not spin.
#[test]
fn a_closed_decode_channel_does_not_spin_the_player_thread() {
    let shared = Arc::new(Shared::new());
    let (tx, rx) = unbounded::<Msg>();
    let thread_shared = shared.clone();
    let handle = std::thread::spawn(move || {
        let (result_tx, result_rx) = unbounded::<DecodeResult>();
        drop(result_tx);
        let config = PlayerConfig {
            report_interval: Duration::from_millis(500),
            warm_output: false,
            null_output: true,
            name: "test",
        };
        let (unused_tx, _) = unbounded::<DecodeResult>();
        let mut engine = Engine::new(
            config,
            Arc::new(|_| {}),
            thread_shared.clone(),
            Box::new(InlineDecoder { results: unused_tx }),
            Box::new(CpalFactory),
        );
        matches!(run(&mut engine, &thread_shared, &rx, &result_rx), RunEnd::Shutdown)
    });
    std::thread::sleep(Duration::from_millis(200));
    let passes = shared.wakeups.load(Ordering::Relaxed);
    tx.send(Msg::Shutdown).expect("send");
    assert!(handle.join().expect("player thread"), "ended by the shutdown");
    assert!(passes < 10, "{passes} loop passes while idle");
}
