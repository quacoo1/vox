//! HelperClient against the fake helper (docs/addons-design.md "Helper protocol (v1)" and "Tests").

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use vox_addons::{BrokenReason, HelperClient, HelperError, HelperHealth};
use vox_core::read::ReadErrorCode;
use vox_core::text16::utf16_len;
use vox_core::tts::MIME_WAV;
use vox_tts::mock::even_alignment;
use vox_tts::wav::read_wav_info;
use vox_tts::{CancelToken, SynthesisResult};

const COUNT: &str = "count.txt";

fn client(scratch: &Scratch, args: &[&str]) -> HelperClient {
    HelperClient::new(fake_spec(scratch, args))
}

fn synth(c: &HelperClient, text: &str) -> Result<SynthesisResult, HelperError> {
    c.synthesize("alpha", text, true, &CancelToken::new())
}

fn duration(r: &SynthesisResult) -> f64 {
    read_wav_info(&r.audio).expect("a WAV").duration_sec
}

/// The fake's `text <id> <tag> <text>` log lines, as (id, text).
fn texts(scratch: &Scratch) -> Vec<(u64, String)> {
    lines(&scratch.file("log.txt"))
        .iter()
        .filter_map(|l| {
            let mut parts = l.splitn(4, ' ');
            (parts.next()? == "text").then_some(())?;
            let id = parts.next()?.parse().ok()?;
            let _tag = parts.next()?;
            Some((id, parts.next().unwrap_or("").to_string()))
        })
        .collect()
}

fn gone(pid: u32) -> bool {
    eventually(Duration::from_secs(3), || !is_alive(pid))
}

fn only_pid(scratch: &Scratch) -> u32 {
    let pids = spawned_pids(scratch, COUNT);
    assert_eq!(pids.len(), 1, "{pids:?}");
    pids[0]
}

/// Counts the client's health notifications.
fn health_counter(c: &HelperClient) -> Arc<AtomicUsize> {
    let n = Arc::new(AtomicUsize::new(0));
    let counter = n.clone();
    c.set_on_health(Box::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    n
}

/// Cancels `token` after `after`, from another thread.
fn cancel_later(token: &CancelToken, after: Duration) -> std::thread::JoinHandle<Instant> {
    let token = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        token.cancel();
        Instant::now()
    })
}

#[test]
fn the_handshake_starts_one_helper_and_it_is_reused() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    assert!(!c.is_running());
    assert_eq!(spawns(&s, COUNT), 0, "nothing is spawned before the first request");
    c.start().unwrap();
    assert!(c.is_running());
    assert_eq!(c.health(), HelperHealth::Ready);
    synth(&c, "one").unwrap();
    synth(&c, "two").unwrap();
    assert_eq!(c.spawn_count(), 1);
    assert_eq!(spawns(&s, COUNT), 1);
    let log = lines(&s.file("log.txt"));
    assert_eq!(log[0], "hello 1 1.0.0");
    assert_eq!(log.iter().filter(|l| l.starts_with("hello")).count(), 1);
}

#[test]
fn a_helper_that_wants_protocol_2_is_incompatible_and_is_not_respawned() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "protocol:2"]);
    let notified = health_counter(&c);
    assert_eq!(synth(&c, "hi").unwrap_err(), HelperError::Incompatible);
    assert_eq!(c.health(), HelperHealth::Incompatible);
    assert_eq!(notified.load(Ordering::SeqCst), 1);
    let t = Instant::now();
    assert_eq!(synth(&c, "hi").unwrap_err(), HelperError::Incompatible);
    assert_eq!(c.start().unwrap_err(), HelperError::Incompatible);
    assert!(t.elapsed() < Duration::from_millis(500));
    assert_eq!(spawns(&s, COUNT), 1);
    assert_eq!(c.spawn_count(), 1);
    let e = HelperError::Incompatible.into_tts("Fake A", "1.0.0");
    assert_eq!((e.code, e.retryable, e.message.as_str()), (ReadErrorCode::Unknown, false, "Fake A needs an update."));
}

#[test]
fn a_helper_that_never_says_hello_times_out_at_the_start_deadline_and_is_killed() {
    for mode in ["no-hello", "slow-hello:5000"] {
        let s = Scratch::new();
        let c = client(&s, &["--mode", mode]);
        let t = Instant::now();
        assert_eq!(synth(&c, "hi").unwrap_err(), HelperError::TimedOut, "{mode}");
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(1900) && took < Duration::from_secs(5), "{mode}: {took:?}");
        assert!(!c.is_running(), "{mode}");
        assert!(gone(only_pid(&s)), "{mode}");
        let e = HelperError::TimedOut.into_tts("Fake A", "1.0.0");
        assert_eq!((e.code, e.retryable, e.message.as_str()), (ReadErrorCode::Unknown, false, "Fake A took too long to respond."));
    }
}

#[test]
fn a_read_cancelled_during_a_slow_hello_returns_at_once_without_counting_a_crash() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "slow-hello:5000"]);
    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel.cancel();
        })
    };
    let t = Instant::now();
    assert_eq!(c.synthesize("alpha", "hi", false, &cancel).unwrap_err(), HelperError::Cancelled);
    assert!(t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
    canceller.join().unwrap();
    assert!(!c.is_running());
    assert!(eventually(Duration::from_secs(3), || gone(only_pid(&s))));
    assert_eq!(c.health(), HelperHealth::Ready);
}

#[test]
fn a_banner_before_hello_is_a_violation_that_kills_the_helper() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "banner"]);
    assert_eq!(synth(&c, "hi").unwrap_err(), HelperError::Crashed);
    assert!(!c.is_running());
    assert!(gone(only_pid(&s)));
    assert_eq!(c.health(), HelperHealth::Ready, "one violation does not use up the budget");
}

#[test]
fn a_helper_that_exits_at_start_is_a_retryable_crash() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "exit-at-start"]);
    let err = synth(&c, "hi").unwrap_err();
    assert_eq!(err, HelperError::Crashed);
    let e = err.into_tts("Fake A", "1.0.0");
    assert_eq!((e.code, e.retryable, e.message.as_str()), (ReadErrorCode::Unknown, true, "Fake A stopped unexpectedly."));
    assert!(!c.is_running());
}

#[test]
fn synthesize_gives_a_free_wav_with_word_timing_in_every_offset_unit() {
    let text = "héllo 👋 wörld";
    for unit in ["utf8", "codepoint", "utf16"] {
        let s = Scratch::new();
        let c = client(&s, &["--offsets", unit]);
        let r = synth(&c, text).unwrap();
        assert_eq!(r.mime, MIME_WAV, "{unit}");
        assert_eq!(r.character_cost, Some(0.0), "{unit}");
        assert_eq!(r.request_id, None);
        let info = read_wav_info(&r.audio).expect("a valid WAV");
        assert_eq!((info.format, info.channels, info.sample_rate, info.bits_per_sample), (1, 1, 24_000, 16), "{unit}");
        // 13 code points at 40 ms each.
        assert!((info.duration_sec - 0.52).abs() < 1e-3, "{unit}: {}", info.duration_sec);
        let a = r.alignment.expect("an alignment");
        assert_eq!(a.chars.len(), utf16_len(text), "{unit}");
        assert_eq!((a.starts.len(), a.ends.len()), (a.chars.len(), a.chars.len()), "{unit}");
        assert_eq!(a.chars[6], "👋");
        assert_eq!(a.chars[7], "");
        // The fake's cues: "héllo" at 0, "👋" at 0.24 s (code point 6), "wörld" at 0.32 s (code point 8).
        assert_eq!(a.starts[0], 0.0, "{unit}");
        assert_eq!(a.starts[6], 0.24, "{unit}");
        assert_eq!(a.starts[9], 0.32, "{unit}");
        assert_eq!(*a.ends.last().unwrap(), 0.52, "{unit}");
        assert_ne!(Some(a), even_alignment(text, info.duration_sec), "{unit}: the word timing is used");
    }
}

#[test]
fn bad_offsets_and_missing_words_fall_back_to_even_timing() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    for text in ["@bad-offsets héllo wörld", "@no-words héllo wörld"] {
        let r = synth(&c, text).unwrap();
        let a = r.alignment.clone().expect("an alignment");
        assert_eq!(a.chars.len(), utf16_len(text), "{text}");
        assert_eq!(Some(a), even_alignment(text, duration(&r)), "{text}");
    }
    // Word timing off: even too, whatever the helper sends.
    let r = c.synthesize("alpha", "héllo wörld", false, &CancelToken::new()).unwrap();
    assert_eq!(r.alignment, even_alignment("héllo wörld", duration(&r)));
}

#[test]
fn a_crash_is_retryable_and_the_next_call_respawns() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    synth(&c, "warm up").unwrap();
    let first = only_pid(&s);
    let err = synth(&c, "@crash").unwrap_err();
    assert_eq!(err, HelperError::Crashed);
    assert!(err.into_tts("Fake A", "1.0.0").retryable);
    assert!(!c.is_running());
    assert!(gone(first));
    synth(&c, "again").unwrap();
    let pids = spawned_pids(&s, COUNT);
    assert_eq!(pids.len(), 2);
    assert_ne!(pids[1], first);
    assert_eq!(c.health(), HelperHealth::Ready);
}

#[test]
fn crash_after_answers_then_crashes_the_next_request() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    synth(&c, "@crash-after:1 fine").unwrap();
    assert_eq!(synth(&c, "boom").unwrap_err(), HelperError::Crashed);
    synth(&c, "fine again").unwrap();
    assert_eq!(spawns(&s, COUNT), 2);
}

#[test]
fn three_crashes_break_the_add_on_and_it_fails_at_once_without_spawning() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    let notified = health_counter(&c);
    assert_eq!(synth(&c, "@crash").unwrap_err(), HelperError::Crashed);
    assert_eq!(synth(&c, "@crash").unwrap_err(), HelperError::Crashed);
    assert_eq!(notified.load(Ordering::SeqCst), 0);
    assert_eq!(synth(&c, "@crash").unwrap_err(), HelperError::Broken(BrokenReason::KeepsStopping));
    assert_eq!(c.health(), HelperHealth::Broken(BrokenReason::KeepsStopping));
    assert_eq!(notified.load(Ordering::SeqCst), 1);
    assert_eq!(spawns(&s, COUNT), 3);
    let t = Instant::now();
    let err = synth(&c, "hello").unwrap_err();
    assert!(t.elapsed() < Duration::from_millis(200), "{:?}", t.elapsed());
    assert_eq!(err, HelperError::Broken(BrokenReason::KeepsStopping));
    assert_eq!(c.start().unwrap_err(), HelperError::Broken(BrokenReason::KeepsStopping));
    assert_eq!(spawns(&s, COUNT), 3);
    assert_eq!(c.spawn_count(), 3);
    let e = err.into_tts("Fake A", "1.0.0");
    assert_eq!(
        (e.code, e.retryable, e.message.as_str()),
        (ReadErrorCode::Unknown, false, "Fake A keeps stopping. Repair it on the Voices page.")
    );
}

#[test]
fn violations_and_timeouts_count_toward_the_crash_budget() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    assert_eq!(synth(&c, "@garbage").unwrap_err(), HelperError::Crashed);
    assert_eq!(synth(&c, "@crash").unwrap_err(), HelperError::Crashed);
    assert!(synth(&c, "@hang").is_err());
    assert_eq!(c.health(), HelperHealth::Broken(BrokenReason::KeepsStopping));
    assert_eq!(synth(&c, "hi").unwrap_err(), HelperError::Broken(BrokenReason::KeepsStopping));
    assert_eq!(spawns(&s, COUNT), 3);
}

#[test]
fn a_hung_request_times_out_at_the_idle_deadline_and_the_helper_is_killed() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    c.start().unwrap();
    let pid = only_pid(&s);
    let t = Instant::now();
    assert_eq!(synth(&c, "@hang").unwrap_err(), HelperError::TimedOut);
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(950) && took < Duration::from_secs(4), "{took:?}");
    assert!(!c.is_running());
    assert!(gone(pid));
    synth(&c, "next").unwrap();
    assert_eq!(spawns(&s, COUNT), 2);
}

#[test]
fn heartbeats_keep_a_slow_request_alive_past_the_idle_deadline() {
    let s = Scratch::new();
    let c = client(&s, &["--heartbeat-ms", "200"]);
    let t = Instant::now();
    let r = synth(&c, "@slow:2500 done").unwrap();
    assert!(t.elapsed() >= Duration::from_millis(2400), "{:?}", t.elapsed());
    assert!(duration(&r) > 0.0);
    assert_eq!(spawns(&s, COUNT), 1);
}

#[test]
fn heartbeats_do_not_outlive_the_absolute_deadline() {
    let s = Scratch::new();
    let mut spec = fake_spec(&s, &["--heartbeat-ms", "200"]);
    spec.timeouts.synth_max = Duration::from_millis(1500);
    let c = HelperClient::new(spec);
    let t = Instant::now();
    assert_eq!(synth(&c, "@slow:5000").unwrap_err(), HelperError::TimedOut);
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(1450) && took < Duration::from_secs(4), "{took:?}");
    assert!(gone(only_pid(&s)));
}

#[test]
fn only_the_first_request_after_a_spawn_gets_the_load_allowance() {
    let s = Scratch::new();
    let mut spec = fake_spec(&s, &[]);
    spec.timeouts.load = Duration::from_millis(1500);
    let c = HelperClient::new(spec);
    // No heartbeats: 1.8 s of silence is past the 1 s idle deadline, but within it plus the load allowance.
    synth(&c, "@slow:1800 first").unwrap();
    let t = Instant::now();
    assert_eq!(synth(&c, "@slow:1800 second").unwrap_err(), HelperError::TimedOut);
    assert!(t.elapsed() < Duration::from_millis(1700), "{:?}", t.elapsed());
    // A respawned helper gets it again.
    synth(&c, "@slow:1800 third").unwrap();
    assert_eq!(spawns(&s, COUNT), 2);
}

#[test]
fn a_voice_list_request_does_not_use_up_the_load_allowance() {
    let s = Scratch::new();
    let mut spec = fake_spec(&s, &[]);
    spec.timeouts.load = Duration::from_millis(1500);
    let c = HelperClient::new(spec);
    // `voices` is answered without loading the model, so the first synthesize after it still may load.
    assert_eq!(c.voices().unwrap().len(), 2);
    synth(&c, "@slow:1800 first").unwrap();
    assert_eq!(synth(&c, "@slow:1800 second").unwrap_err(), HelperError::TimedOut);
    assert_eq!(spawns(&s, COUNT), 1);
}

#[test]
fn an_error_reply_does_not_use_up_the_load_allowance() {
    let s = Scratch::new();
    let mut spec = fake_spec(&s, &[]);
    spec.timeouts.load = Duration::from_millis(1500);
    let c = HelperClient::new(spec);
    assert_eq!(synth(&c, "@err:bad_request x").unwrap_err(), HelperError::Reply("bad_request".into()));
    synth(&c, "@slow:1800 first audio").unwrap();
}

#[test]
fn a_cancelled_request_returns_within_the_grace_and_the_helper_is_kept() {
    let s = Scratch::new();
    let c = client(&s, &["--heartbeat-ms", "200"]);
    c.start().unwrap();
    let pid = only_pid(&s);
    let token = CancelToken::new();
    let canceller = cancel_later(&token, Duration::from_millis(300));
    let t = Instant::now();
    let err = c.synthesize("alpha", "@slow:5000", true, &token).unwrap_err();
    let returned = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert_eq!(err, HelperError::Cancelled);
    assert!(err.into_tts("Fake A", "1.0.0").aborted);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let after_cancel = returned.duration_since(cancelled_at);
    assert!(after_cancel < quick().cancel_grace + Duration::from_millis(400), "{after_cancel:?}");
    assert!(lines(&s.file("log.txt")).iter().any(|l| l.starts_with("cancel 2 ")), "the helper was sent cancel");
    assert!(c.is_running());
    synth(&c, "next").unwrap();
    assert_eq!(spawned_pids(&s, COUNT), [pid], "the same process served the next call");
    assert!(is_alive(pid));
}

#[test]
fn a_helper_that_ignores_cancel_is_killed_after_the_grace_and_respawned() {
    let s = Scratch::new();
    let c = client(&s, &["--heartbeat-ms", "100"]);
    c.start().unwrap();
    let pid = only_pid(&s);
    let token = CancelToken::new();
    let canceller = cancel_later(&token, Duration::from_millis(200));
    let err = c.synthesize("alpha", "@ignore-cancel", true, &token).unwrap_err();
    let returned = Instant::now();
    let after_cancel = returned.duration_since(canceller.join().unwrap());
    assert_eq!(err, HelperError::Cancelled);
    assert!(after_cancel >= quick().cancel_grace && after_cancel < Duration::from_secs(2), "{after_cancel:?}");
    assert!(!c.is_running());
    assert!(gone(pid));
    synth(&c, "next").unwrap();
    assert_eq!(spawns(&s, COUNT), 2);
    assert_eq!(c.health(), HelperHealth::Ready);
}

#[test]
fn a_request_cancelled_while_it_waits_leaves_the_queue_at_once() {
    let s = Scratch::new();
    let c = Arc::new(client(&s, &["--heartbeat-ms", "200"]));
    c.start().unwrap();
    let slow = {
        let c = c.clone();
        std::thread::spawn(move || synth(&c, "@slow:2000"))
    };
    assert!(eventually(Duration::from_secs(3), || texts(&s).iter().any(|(_, t)| t.starts_with("@slow"))));
    let token = CancelToken::new();
    let canceller = cancel_later(&token, Duration::from_millis(200));
    let err = c.synthesize("alpha", "queued", true, &token).unwrap_err();
    let after_cancel = Instant::now().duration_since(canceller.join().unwrap());
    assert_eq!(err, HelperError::Cancelled);
    assert!(after_cancel < Duration::from_millis(500), "{after_cancel:?}");
    slow.join().unwrap().unwrap();
    assert!(texts(&s).iter().all(|(_, t)| t != "queued"), "the cancelled request was never sent");
    assert_eq!(spawns(&s, COUNT), 1);
}

#[test]
fn garbage_and_huge_lines_are_violations_that_kill_the_helper() {
    let s = Scratch::new();
    let mut spec = fake_spec(&s, &[]);
    spec.max_line = 1 << 20;
    let c = HelperClient::new(spec);
    for (i, text) in ["@garbage", "@huge:2000000"].into_iter().enumerate() {
        c.start().unwrap();
        let pid = spawned_pids(&s, COUNT)[i];
        assert_eq!(synth(&c, text).unwrap_err(), HelperError::Crashed, "{text}");
        assert!(!c.is_running(), "{text}");
        assert!(gone(pid), "{text}");
    }
    assert_eq!(spawns(&s, COUNT), 2);
}

#[test]
fn bad_wavs_are_refused_and_the_helper_is_kept() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    for text in ["@bad-wav", "@bad-wav:riff", "@bad-wav:size", "@bad-wav:8bit", "@long-wav"] {
        let err = synth(&c, text).unwrap_err();
        assert_eq!(err, HelperError::BadAudio, "{text}");
        assert!(c.is_running(), "{text}");
    }
    let e = HelperError::BadAudio.into_tts("Fake A", "1.0.0");
    assert_eq!((e.code, e.retryable, e.message.as_str()), (ReadErrorCode::Unknown, false, "Fake A sent audio Vox couldn’t use."));
    synth(&c, "fine").unwrap();
    assert_eq!(spawns(&s, COUNT), 1);
    assert_eq!(c.health(), HelperHealth::Ready);
}

#[test]
fn stale_replies_are_discarded_and_the_right_audio_is_returned() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    // The fake first sends 0.1 s of audio for the previous id, then the real answer: 12 characters, 0.48 s.
    let r = synth(&c, "@stale hello").unwrap();
    assert!((duration(&r) - 0.48).abs() < 1e-3, "{}", duration(&r));
    let r = synth(&c, "@stale again, longer").unwrap();
    assert!((duration(&r) - 0.8).abs() < 1e-3, "{}", duration(&r));
    assert_eq!(spawns(&s, COUNT), 1);
}

#[test]
fn error_replies_map_to_vox_copy_and_keep_the_helper() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    for (code, message) in [
        ("unknown_voice", "This voice isn’t in Fake A 1.0.0."),
        ("out_of_memory", "Not enough memory for Fake A."),
        ("model_load_failed", "Fake A couldn’t load its voice model. Try repairing it."),
        ("internal", "Fake A couldn’t read this text."),
    ] {
        let err = synth(&c, &format!("@err:{code}")).unwrap_err();
        assert_eq!(err, HelperError::Reply(code.into()));
        let e = err.into_tts("Fake A", "1.0.0");
        assert_eq!((e.code, e.retryable, e.message.as_str()), (ReadErrorCode::Unknown, false, message), "{code}");
        assert!(c.is_running(), "{code}");
    }
    assert_eq!(spawns(&s, COUNT), 1);
    assert_eq!(c.health(), HelperHealth::Ready);
}

#[test]
fn an_unknown_voice_key_is_an_unknown_voice_reply() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    assert_eq!(c.synthesize("gamma", "hi", true, &CancelToken::new()).unwrap_err(), HelperError::Reply("unknown_voice".into()));
    assert!(c.synthesize("beta", "hi", true, &CancelToken::new()).is_ok());
    assert_eq!(spawns(&s, COUNT), 1);
}

#[test]
fn text_over_the_request_cap_is_refused_without_spawning() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    let text = "a".repeat(70_000);
    assert_eq!(synth(&c, &text).unwrap_err(), HelperError::Reply("text_too_long".into()));
    assert_eq!(c.spawn_count(), 0);
    assert_eq!(spawns(&s, COUNT), 0);
}

#[test]
fn a_stderr_flood_does_not_deadlock() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "stderr-flood"]);
    let t = Instant::now();
    synth(&c, "still speaking").unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn concurrent_callers_are_served_one_at_a_time_in_arrival_order() {
    let s = Scratch::new();
    let c = Arc::new(client(&s, &["--heartbeat-ms", "200"]));
    c.start().unwrap();
    let started = Instant::now();
    let slow = {
        let c = c.clone();
        std::thread::spawn(move || synth(&c, "@slow:1500").map(|_| ()))
    };
    assert!(eventually(Duration::from_secs(3), || texts(&s).iter().any(|(_, t)| t.starts_with("@slow"))));
    let mut callers = Vec::new();
    for word in ["one", "two", "three"] {
        let c = c.clone();
        callers.push(std::thread::spawn(move || {
            synth(&c, word).unwrap();
            Instant::now()
        }));
        std::thread::sleep(Duration::from_millis(150));
    }
    slow.join().unwrap().unwrap();
    for h in callers {
        let done = h.join().unwrap();
        assert!(done.duration_since(started) >= Duration::from_millis(1450), "a caller was served while the slow request ran");
    }
    let got = texts(&s);
    assert_eq!(got, [(2, "@slow:1500".to_string()), (3, "one".into()), (4, "two".into()), (5, "three".into())]);
    assert_eq!(spawns(&s, COUNT), 1);
}

#[test]
fn dropping_the_client_ends_the_helper_within_two_seconds() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    synth(&c, "hi").unwrap();
    let pid = only_pid(&s);
    assert!(is_alive(pid));
    let t = Instant::now();
    drop(c);
    assert!(eventually(Duration::from_secs(2), || !is_alive(pid)));
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn stopping_the_client_ends_a_helper_that_ignores_stdin_eof() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "no-eof-exit"]);
    synth(&c, "hi").unwrap();
    let pid = only_pid(&s);
    let t = Instant::now();
    c.stop(Duration::from_millis(300));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(eventually(Duration::from_secs(2), || !is_alive(pid)));
    assert!(!c.is_running());
}

fn grandchild(scratch: &Scratch) -> u32 {
    let log = lines(&scratch.file("log.txt"));
    log.iter().find_map(|l| l.strip_prefix("grandchild ")?.parse().ok()).expect("the fake logged its grandchild")
}

#[test]
fn stop_leaves_no_grandchild() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "spawn-grandchild"]);
    synth(&c, "hi").unwrap();
    let (pid, gc) = (only_pid(&s), grandchild(&s));
    assert!(is_alive(gc));
    c.stop(Duration::from_millis(300));
    assert!(gone(pid));
    assert!(gone(gc), "grandchild {gc} outlived stop");
}

#[test]
fn dropping_the_client_leaves_no_grandchild() {
    let s = Scratch::new();
    let c = client(&s, &["--mode", "spawn-grandchild"]);
    synth(&c, "hi").unwrap();
    let (pid, gc) = (only_pid(&s), grandchild(&s));
    assert!(is_alive(gc));
    drop(c);
    assert!(gone(pid));
    assert!(gone(gc), "grandchild {gc} outlived the client");
}

#[test]
fn quit_if_idle_quits_only_an_idle_helper() {
    let s = Scratch::new();
    let c = Arc::new(client(&s, &["--heartbeat-ms", "200"]));
    assert!(!c.quit_if_idle(Duration::ZERO, Duration::from_millis(300)), "nothing runs");
    synth(&c, "hi").unwrap();
    assert!(!c.quit_if_idle(Duration::from_secs(60), Duration::from_millis(300)), "not idle long enough");
    let slow = {
        let c = c.clone();
        std::thread::spawn(move || synth(&c, "@slow:800"))
    };
    assert!(eventually(Duration::from_secs(3), || texts(&s).iter().any(|(_, t)| t.starts_with("@slow"))));
    assert!(!c.quit_if_idle(Duration::ZERO, Duration::from_millis(300)), "a request is running");
    slow.join().unwrap().unwrap();
    assert!(c.is_running());
    let pid = only_pid(&s);
    std::thread::sleep(Duration::from_millis(50));
    assert!(c.quit_if_idle(Duration::from_millis(20), Duration::from_millis(300)));
    assert!(!c.is_running());
    assert!(gone(pid));
    assert_eq!(c.health(), HelperHealth::Ready);
    synth(&c, "again").unwrap();
    assert_eq!(spawns(&s, COUNT), 2);
}

#[test]
fn stop_fails_running_and_waiting_requests_and_later_ones_with_removed() {
    let s = Scratch::new();
    let c = Arc::new(client(&s, &["--heartbeat-ms", "200"]));
    c.start().unwrap();
    let pid = only_pid(&s);
    let call = |text: &'static str| {
        let c = c.clone();
        std::thread::spawn(move || {
            let r = synth(&c, text);
            (r.map(|_| ()), Instant::now())
        })
    };
    let running = call("@slow:5000");
    assert!(eventually(Duration::from_secs(3), || texts(&s).iter().any(|(_, t)| t.starts_with("@slow"))));
    let waiting = call("queued");
    std::thread::sleep(Duration::from_millis(150));
    let stopped_at = Instant::now();
    c.stop(Duration::from_millis(300));
    for (what, h) in [("running", running), ("waiting", waiting)] {
        let (r, at) = h.join().unwrap();
        assert_eq!(r.unwrap_err(), HelperError::Removed, "{what}");
        let after = at.duration_since(stopped_at);
        assert!(after < Duration::from_millis(500), "{what}: {after:?}");
    }
    assert_eq!(c.health(), HelperHealth::Removed);
    assert!(!c.is_running());
    assert!(gone(pid));
    let spawned = c.spawn_count();
    let t = Instant::now();
    assert_eq!(synth(&c, "later").unwrap_err(), HelperError::Removed);
    assert_eq!(c.start().unwrap_err(), HelperError::Removed);
    assert_eq!(c.voices().unwrap_err(), HelperError::Removed);
    assert!(t.elapsed() < Duration::from_millis(200));
    assert_eq!(c.spawn_count(), spawned, "nothing is spawned once the add-on is removed");
    assert!(!c.is_running());
    assert!(texts(&s).iter().all(|(_, t)| t != "queued" && t != "later"));
    let e = HelperError::Removed.into_tts("Fake A", "1.0.0");
    assert_eq!((e.code, e.retryable, e.message.as_str()), (ReadErrorCode::Unknown, false, "Fake A was removed."));
}

/// The race is on the 50 ms ticks: when the running request gives its turn back before the waiting one notices the
/// removal, the waiting one takes the turn and spawns. A few attempts at different offsets make it near certain.
#[test]
fn a_request_waiting_when_stop_is_called_never_spawns_a_helper() {
    for k in 0..8u64 {
        let s = Scratch::new();
        let c = Arc::new(client(&s, &[]));
        c.start().unwrap();
        let call = |text: &'static str| {
            let c = c.clone();
            std::thread::spawn(move || synth(&c, text).map(|_| ()))
        };
        let running = call("@slow:900");
        assert!(eventually(Duration::from_secs(3), || texts(&s).iter().any(|(_, t)| t.starts_with("@slow"))));
        let waiting = call("queued");
        std::thread::sleep(Duration::from_millis(100 + k * 23));
        c.stop(Duration::from_millis(300));
        assert_eq!(running.join().unwrap().unwrap_err(), HelperError::Removed);
        assert_eq!(waiting.join().unwrap().unwrap_err(), HelperError::Removed);
        assert_eq!(c.spawn_count(), 1, "attempt {k}: a helper was spawned after stop()");
        assert!(!c.is_running());
    }
}

#[test]
fn voices_returns_the_helpers_list() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    let voices = c.voices().unwrap();
    let got: Vec<_> = voices.iter().map(|v| (v.key.as_str(), v.name.as_str(), v.gender.as_str(), v.language.as_str())).collect();
    assert_eq!(got, [("alpha", "Alpha", "female", "en-US"), ("beta", "Beta", "male", "en-GB")]);
    assert_eq!(spawns(&s, COUNT), 1);
    synth(&c, "after voices").unwrap();
    assert_eq!(spawns(&s, COUNT), 1);
}

#[test]
fn a_missing_program_is_incomplete_and_never_retried() {
    let s = Scratch::new();
    let mut spec = fake_spec(&s, &[]);
    spec.program = s.file("missing").join(if cfg!(windows) { "helper.exe" } else { "helper" });
    let c = HelperClient::new(spec);
    let notified = health_counter(&c);
    let err = synth(&c, "hi").unwrap_err();
    assert_eq!(err, HelperError::Broken(BrokenReason::Incomplete));
    assert_eq!(notified.load(Ordering::SeqCst), 1);
    assert_eq!(c.health(), HelperHealth::Broken(BrokenReason::Incomplete));
    let e = err.into_tts("Fake A", "1.0.0");
    assert_eq!(
        (e.code, e.retryable, e.message.as_str()),
        (ReadErrorCode::Unknown, false, "Fake A is incomplete. Repair it on the Voices page.")
    );
    assert_eq!(synth(&c, "hi").unwrap_err(), HelperError::Broken(BrokenReason::Incomplete));
    assert_eq!(c.spawn_count(), 0);
}

/// The helper is started through `sh`, which writes its environment to a file and then becomes the fake.
#[cfg(not(windows))]
#[test]
fn the_helper_gets_the_add_on_environment_and_no_vox_variables() {
    std::env::set_var("VOX_HELPER_RS_LEAK", "1");
    let s = Scratch::new();
    let env_file = s.file("env.txt");
    let fake = fake_spec(&s, &[]);
    let mut spec = vox_addons::HelperSpec::new("fake-a", "Fake A", "1.0.0", "/bin/sh".into(), s.path().to_path_buf());
    spec.args = vec!["-c".into(), r#"env > "$1"; shift; exec "$@""#.into(), "sh".into(), env_file.to_string_lossy().into_owned()];
    spec.args.push(FAKE.into());
    spec.args.extend(fake.args);
    spec.timeouts = quick();
    spec.data_dir = Some(s.file("data"));
    spec.env = vec![("FAKE_EXTRA".into(), "yes".into())];
    let c = HelperClient::new(spec);
    synth(&c, "hi").unwrap();
    let env = lines(&env_file);
    let has = |line: &str| env.iter().any(|l| l == line);
    assert!(!env.iter().any(|l| l.starts_with("VOX_HELPER_RS_LEAK=")), "a VOX_ variable leaked");
    assert!(has("VOX_ADDON_PROTOCOL=1"));
    assert!(has("VOX_ADDON_ID=fake-a"));
    assert!(has(&format!("VOX_ADDON_DIR={}", s.path().display())));
    assert!(has(&format!("VOX_ADDON_DATA={}", s.file("data").display())));
    assert!(has("FAKE_EXTRA=yes"));
    assert!(s.file("data").is_dir(), "the data folder is created before the spawn");
    let vox: Vec<_> = env.iter().filter(|l| l.starts_with("VOX_")).collect();
    assert_eq!(vox.len(), 4, "{vox:?}");
}

/// Linux: a helper killed from outside while idle is replaced on the next call.
#[cfg(not(windows))]
#[test]
fn a_helper_that_dies_while_idle_is_respawned_on_the_next_call() {
    let s = Scratch::new();
    let c = client(&s, &[]);
    synth(&c, "hi").unwrap();
    let pid = only_pid(&s);
    let status = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap();
    assert!(status.success());
    assert!(gone(pid));
    // The leader shows as a zombie while the fake's other threads are still exiting; only then is its exit readable
    // and its stdout closed.
    std::thread::sleep(Duration::from_millis(300));
    synth(&c, "again").unwrap();
    assert_eq!(spawns(&s, COUNT), 2);
    assert_eq!(c.health(), HelperHealth::Ready);
}

