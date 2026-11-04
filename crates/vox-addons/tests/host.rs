// `TtsError` is returned by value across the speech provider API (see vox-tts lib.rs).
#![allow(clippy::result_large_err)]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use vox_addons::{load_spec, AddonHost, AddonRuntime, BrokenReason, HelperTimeouts, RuntimeStatus};
use vox_core::read::ReadErrorCode;
use vox_core::tts::MIME_WAV;
use vox_tts::wav::read_wav_info;
use vox_tts::{CancelToken, SpeechProvider, SynthesisResult, TtsError};

const WAIT: Duration = Duration::from_secs(5);

fn fake(id: &str, name: &str) -> FakeAddon {
    FakeAddon::new(id, name)
}

/// [`FakeAddon::runtime`], but starting the fake Cargo built rather than the copy in the add-on folder: on Linux,
/// running a file just written can fail with ETXTBSY while another test thread is forking (the child holds the
/// still-open write descriptor until it execs), which would make these tests flaky under load.
fn live(addon: &FakeAddon, scratch: &Scratch) -> AddonRuntime {
    let dir = addon.write(scratch);
    let (manifest, mut spec) = load_spec(&dir, None).expect("the fake add-on loads");
    spec.timeouts = quick();
    spec.program = PathBuf::from(FAKE);
    AddonRuntime::new(manifest, spec)
}

fn host_of(runtimes: Vec<AddonRuntime>) -> AddonHost {
    let host = AddonHost::new(|_| {});
    for r in runtimes {
        host.replace(r);
    }
    host
}

/// A host whose health hook records the ids it is called with.
fn recording_host() -> (AddonHost, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    (AddonHost::new(move |id| sink.lock().unwrap().push(id.to_string())), seen)
}

fn synth(host: &AddonHost, rest: &str, text: &str) -> Result<SynthesisResult, TtsError> {
    host.synthesize(text, rest, true, &CancelToken::new())
}

fn synth_err(host: &AddonHost, rest: &str, text: &str) -> TtsError {
    match synth(host, rest, text) {
        Ok(_) => panic!("{rest} {text:?} should fail"),
        Err(e) => e,
    }
}

fn log_of(scratch: &Scratch, id: &str) -> Vec<String> {
    lines(&scratch.file(&format!("{id}-log.txt")))
}

fn count_of(id: &str) -> String {
    format!("{id}-count.txt")
}

fn has_line(scratch: &Scratch, id: &str, prefix: &str) -> bool {
    log_of(scratch, id).iter().any(|l| l.starts_with(prefix))
}

fn gone(pid: u32) -> bool {
    eventually(WAIT, || !is_alive(pid))
}

/// The pitch of the fake's mono 16-bit sine, from its zero crossings.
fn tone_hz(r: &SynthesisResult) -> f64 {
    let info = read_wav_info(&r.audio).expect("a WAV");
    assert_eq!((info.bits_per_sample, info.channels), (16, 1));
    let data = &r.audio[info.data_offset..info.data_offset + info.data_bytes];
    let samples: Vec<i16> = data.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
    let rises = samples.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
    rises as f64 / info.duration_sec
}

fn assert_hz(r: &SynthesisResult, hz: f64) {
    let got = tone_hz(r);
    assert!((got - hz).abs() < hz * 0.1, "expected about {hz} Hz, got {got}");
}

fn assert_plain_error(e: &TtsError, message: &str) {
    assert_eq!(e.message, message);
    assert_eq!(e.code, ReadErrorCode::Unknown);
    assert!(!e.retryable, "{message} should not be retryable");
    assert!(!e.aborted, "{message} is not a cancel");
}

#[test]
fn two_add_ons_with_the_same_voice_keys_give_four_distinct_voices() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s), live(&fake("fake-b", "Fake B"), &s)]);
    let voices = host.list_voices().unwrap();
    let ids: Vec<&str> = voices.iter().map(|v| v.voice_id.as_str()).collect();
    assert_eq!(ids, ["addon:fake-a:alpha", "addon:fake-a:beta", "addon:fake-b:alpha", "addon:fake-b:beta"]);
    let names: Vec<&str> = voices.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["Alpha (Fake A)", "Beta (Fake A)", "Alpha (Fake B)", "Beta (Fake B)"]);
    let categories: Vec<Option<&str>> = voices.iter().map(|v| v.category.as_deref()).collect();
    assert_eq!(categories, [Some("Fake A"), Some("Fake A"), Some("Fake B"), Some("Fake B")]);
    let labels = |g: &str, a: &str, l: &str| [("gender", g), ("accent", a), ("language", l)].map(|(k, v)| (k.to_string(), v.to_string())).to_vec();
    assert_eq!(voices[0].labels, labels("female", "American", "en-US"));
    assert_eq!(voices[3].labels, labels("male", "British", "en-GB"));
    assert_eq!(voices[0].description.as_deref(), Some("Fake A test voices."));
    assert_eq!(voices[2].description.as_deref(), Some("Fake B test voices."));
    assert!(voices.iter().all(|v| v.preview_url.is_none()));
    assert_eq!(spawns(&s, &count_of("fake-a")) + spawns(&s, &count_of("fake-b")), 0);
}

#[test]
fn a_voice_s_own_description_wins_over_the_add_on_description() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.voices = Some(json!([
        {"key": "alpha", "name": "Alpha", "description": "Bright and quick."},
        {"key": "beta", "name": "Beta"}
    ]));
    let host = host_of(vec![live(&a, &s)]);
    let voices = host.list_voices().unwrap();
    assert_eq!(voices[0].description.as_deref(), Some("Bright and quick."));
    assert_eq!(voices[1].description.as_deref(), Some("Fake A test voices."));
    assert!(voices[1].labels.is_empty());
}

#[test]
fn a_short_name_suffixes_the_voice_names_and_the_name_stays_the_category() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A Deluxe");
    a.extra = json!({"short_name": "FA"});
    let host = host_of(vec![live(&a, &s)]);
    let voices = host.list_voices().unwrap();
    assert_eq!(voices[0].name, "Alpha (FA)");
    assert_eq!(voices[0].category.as_deref(), Some("Fake A Deluxe"));
}

#[test]
fn each_voice_reaches_its_own_add_on_process() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s), live(&fake("fake-b", "Fake B"), &s)]);
    let a = synth(&host, "fake-a:alpha", "from a").unwrap();
    let b = synth(&host, "fake-b:beta", "from b").unwrap();
    assert_eq!(a.mime, MIME_WAV);
    assert_eq!(a.character_cost, Some(0.0));
    assert_eq!(b.character_cost, Some(0.0));
    assert_hz(&a, 440.0);
    assert_hz(&b, 220.0);
    assert!(has_line(&s, "fake-a", "text 2 1.0.0 from a"));
    assert!(has_line(&s, "fake-b", "text 2 1.0.0 from b"));
    assert!(!log_of(&s, "fake-a").iter().any(|l| l.contains("from b")));
    assert!(!log_of(&s, "fake-b").iter().any(|l| l.contains("from a")));
    let (pa, pb) = (spawned_pids(&s, &count_of("fake-a")), spawned_pids(&s, &count_of("fake-b")));
    assert_eq!((pa.len(), pb.len()), (1, 1));
    assert_ne!(pa[0], pb[0]);
    assert_eq!(host.runtime("fake-a").unwrap().helper.spawn_count(), 1);
    synth(&host, "fake-a:beta", "again").unwrap();
    assert_eq!(spawns(&s, &count_of("fake-a")), 1, "the second request reuses the process");
}

#[test]
fn list_voices_and_summary_never_spawn_while_the_voices_are_known() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s), live(&fake("fake-b", "Fake B"), &s)]);
    for _ in 0..3 {
        assert_eq!(host.list_voices().unwrap().len(), 4);
        assert_eq!(host.validate().unwrap(), 4);
        host.summary();
    }
    assert!(lines(&s.file("fake-a-count.txt")).is_empty());
    assert!(lines(&s.file("fake-b-count.txt")).is_empty());
    assert!(!host.runtime("fake-a").unwrap().helper.is_running());
}

#[test]
fn the_summary_counts_add_ons_and_voices() {
    let s = Scratch::new();
    let host = AddonHost::new(|_| {});
    host.replace(live(&fake("fake-a", "Fake A"), &s));
    assert_eq!(host.summary(), "1 add-on · 2 voices");
    host.replace(live(&fake("fake-b", "Fake B"), &s));
    assert_eq!(host.summary(), "2 add-ons · 4 voices");
    let mut c = fake("fake-c", "Fake C");
    c.voices = Some(json!([{"key": "alpha", "name": "Alpha"}]));
    host.replace(live(&c, &s));
    assert_eq!(host.summary(), "3 add-ons · 5 voices");
    host.remove("fake-a", Duration::from_millis(400));
    host.remove("fake-b", Duration::from_millis(400));
    assert_eq!(host.summary(), "1 add-on · 1 voice");
    assert_eq!(host.provider(), vox_core::providers::Provider::Addon);
}

#[test]
fn an_add_on_without_voices_in_its_manifest_uses_voices_json_without_spawning() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.voices = None;
    let dir = a.write(&s);
    let file = json!({"schema": 1, "voices": [{"key": "alpha", "name": "Ada", "gender": "female"}]});
    std::fs::write(dir.join("voices.json"), file.to_string()).unwrap();
    let (manifest, mut spec) = load_spec(&dir, None).unwrap();
    assert_eq!(manifest.voices, None);
    spec.timeouts = quick();
    let host = host_of(vec![AddonRuntime::new(manifest, spec)]);
    let voices = host.list_voices().unwrap();
    assert_eq!(voices.len(), 1);
    assert_eq!((voices[0].voice_id.as_str(), voices[0].name.as_str()), ("addon:fake-a:alpha", "Ada (Fake A)"));
    assert_eq!(host.summary(), "1 add-on · 1 voice");
    assert_eq!(spawns(&s, "fake-a-count.txt"), 0);
    let e = synth_err(&host, "fake-a:beta", "hi");
    assert_plain_error(&e, "This voice isn’t in Fake A 1.0.0.");
    assert_eq!(spawns(&s, "fake-a-count.txt"), 0);
}

#[test]
fn an_add_on_with_no_voice_list_anywhere_asks_its_helper_once() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.voices = None;
    let host = host_of(vec![live(&a, &s)]);
    assert_eq!(host.summary(), "1 add-on · 0 voices");
    assert_eq!(spawns(&s, "fake-a-count.txt"), 0, "the summary never spawns");
    for _ in 0..2 {
        let ids: Vec<String> = host.list_voices().unwrap().into_iter().map(|v| v.voice_id).collect();
        assert_eq!(ids, ["addon:fake-a:alpha", "addon:fake-a:beta"]);
    }
    assert_eq!(spawns(&s, "fake-a-count.txt"), 1);
    assert_eq!(log_of(&s, "fake-a").iter().filter(|l| l.starts_with("voices ")).count(), 1);
    assert_eq!(host.summary(), "1 add-on · 2 voices");
}

#[test]
fn a_voices_json_with_another_schema_is_ignored() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.voices = None;
    let dir = a.write(&s);
    let file = json!({"schema": 2, "voices": [{"key": "gamma", "name": "Gamma"}]});
    std::fs::write(dir.join("voices.json"), file.to_string()).unwrap();
    let (manifest, mut spec) = load_spec(&dir, None).unwrap();
    spec.timeouts = quick();
    let runtime = AddonRuntime::new(manifest, spec);
    assert!(runtime.known_voices().is_none());
}

#[test]
fn an_unknown_add_on_is_not_installed_with_code_unknown() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    let e = synth_err(&host, "fake-z:alpha", "hi");
    assert_plain_error(&e, "fake-z isn’t installed. Install it on the Voices page.");
    assert_eq!(e.status, None);
    assert_eq!(spawns(&s, "fake-a-count.txt"), 0);
}

#[test]
fn an_unknown_add_on_is_named_from_the_catalogue_names() {
    let host = AddonHost::new(|_| {});
    host.set_known_names(HashMap::from([("kittentts-2".to_string(), "KittenTTS 2".to_string())]));
    let e = synth_err(&host, "kittentts-2:bella", "hi");
    assert_plain_error(&e, "KittenTTS 2 isn’t installed. Install it on the Voices page.");
    let e = synth_err(&host, "other:bella", "hi");
    assert_plain_error(&e, "other isn’t installed. Install it on the Voices page.");
}

#[test]
fn a_malformed_voice_id_gives_the_offline_voice_not_installed_copy() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    for rest in ["", "fake-a", "fake-a:", ":alpha", "Fake-A:alpha", "fake--a:alpha", "fake-a:al pha", "fake-a:.alpha", "fake-a:alpha."] {
        let e = synth_err(&host, rest, "hi");
        assert_plain_error(&e, vox_tts::providers::ADDON_NOT_INSTALLED);
    }
    assert_eq!(spawns(&s, "fake-a-count.txt"), 0);
}

#[test]
fn a_voice_the_add_on_does_not_list_fails_without_spawning() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    let e = synth_err(&host, "fake-a:gamma", "hi");
    assert_plain_error(&e, "This voice isn’t in Fake A 1.0.0.");
    assert_eq!(spawns(&s, "fake-a-count.txt"), 0);
}

#[test]
fn a_renamed_voice_resolves_through_voice_aliases() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.extra = json!({"voice_aliases": {"old-alpha": "alpha", "old-beta": "beta"}});
    let host = host_of(vec![live(&a, &s)]);
    assert_hz(&synth(&host, "fake-a:old-alpha", "hello there").unwrap(), 440.0);
    assert_hz(&synth(&host, "fake-a:old-beta", "hello there").unwrap(), 220.0);
    let ids: Vec<String> = host.list_voices().unwrap().into_iter().map(|v| v.voice_id).collect();
    assert_eq!(ids, ["addon:fake-a:alpha", "addon:fake-a:beta"], "aliases are not listed as voices");
    let e = synth_err(&host, "fake-a:old-gamma", "hi");
    assert_plain_error(&e, "This voice isn’t in Fake A 1.0.0.");
}

#[test]
fn remove_makes_later_reads_say_it_was_removed_and_quits_the_helper() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s), live(&fake("fake-b", "Fake B"), &s)]);
    synth(&host, "fake-a:alpha", "hi").unwrap();
    let pid = spawned_pids(&s, "fake-a-count.txt")[0];
    assert!(is_alive(pid));
    let removed = host.remove("fake-a", Duration::from_secs(2)).expect("it was installed");
    assert_eq!(removed.id(), "fake-a");
    drop(removed);
    assert!(gone(pid), "the helper exits");
    assert!(!host.is_installed("fake-a"));
    assert_eq!(host.status("fake-a"), RuntimeStatus::NotInstalled);
    let e = synth_err(&host, "fake-a:alpha", "hi");
    assert_plain_error(&e, "Fake A was removed.");
    assert_eq!(spawns(&s, "fake-a-count.txt"), 1);
    let ids: Vec<String> = host.list_voices().unwrap().into_iter().map(|v| v.voice_id).collect();
    assert_eq!(ids, ["addon:fake-b:alpha", "addon:fake-b:beta"]);
    assert!(host.remove("fake-a", Duration::from_secs(2)).is_none());
    synth(&host, "fake-b:alpha", "still here").unwrap();
}

#[test]
fn installing_again_after_remove_clears_the_removed_copy() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    host.remove("fake-a", Duration::from_secs(2));
    assert_eq!(synth_err(&host, "fake-a:alpha", "hi").message, "Fake A was removed.");
    let mut again = fake("fake-a", "Fake A");
    again.version = "1.1.0".into();
    host.replace(live(&again, &s));
    assert_eq!(host.status("fake-a"), RuntimeStatus::Ready);
    synth(&host, "fake-a:alpha", "hi").unwrap();
}

#[test]
fn remove_during_a_slow_request_fails_it_quickly_with_was_removed() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    std::thread::scope(|sc| {
        let slow = sc.spawn(|| synth(&host, "fake-a:alpha", "@slow:5000 long"));
        assert!(eventually(WAIT, || has_line(&s, "fake-a", "text 2 1.0.0 @slow")));
        let pid = spawned_pids(&s, "fake-a-count.txt")[0];
        let started = Instant::now();
        host.remove("fake-a", Duration::from_secs(2));
        let e = slow.join().unwrap().expect_err("the request fails");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        assert_plain_error(&e, "Fake A was removed.");
        assert!(gone(pid));
    });
}

#[test]
fn replace_while_a_request_runs_finishes_it_on_the_old_helper_then_quits_it() {
    let s = Scratch::new();
    let mut v1 = fake("fake-a", "Fake A");
    v1.args = vec!["--heartbeat-ms".into(), "100".into()];
    let host = host_of(vec![live(&v1, &s)]);
    let mut v2 = fake("fake-a", "Fake A");
    v2.version = "2.0.0".into();
    v2.args = vec!["--tag".into(), "v2".into()];
    let v2 = live(&v2, &s);
    std::thread::scope(|sc| {
        let slow = sc.spawn(|| synth(&host, "fake-a:alpha", "@slow:1500 old"));
        assert!(eventually(WAIT, || has_line(&s, "fake-a", "text 2 1.0.0 @slow:1500 old")));
        let old = host.replace(v2).expect("v1 was installed");
        assert_eq!(old.manifest.version.as_str(), "1.0.0");
        drop(old);
        assert_eq!(host.runtime("fake-a").unwrap().manifest.version.as_str(), "2.0.0");

        synth(&host, "fake-a:alpha", "new").unwrap();
        assert!(has_line(&s, "fake-a", "text 2 v2 new"), "{:?}", log_of(&s, "fake-a"));
        let pids = spawned_pids(&s, "fake-a-count.txt");
        assert_eq!(pids.len(), 2);
        assert!(!slow.is_finished(), "the new request does not wait for the old one");
        assert!(is_alive(pids[0]), "the old helper still serves its request");

        let old_result = slow.join().unwrap().expect("the old request finishes");
        assert_hz(&old_result, 440.0);
        assert!(!log_of(&s, "fake-a").iter().any(|l| l.contains("v2 @slow") || l.contains("1.0.0 new")));
        assert!(gone(pids[0]), "the old helper exits after its request");
        assert!(is_alive(pids[1]));
    });
    assert_eq!(host.status("fake-a"), RuntimeStatus::Ready);
}

#[test]
fn status_tells_not_installed_from_ready() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    assert_eq!(host.status("fake-a"), RuntimeStatus::Ready);
    assert_eq!(host.status("fake-z"), RuntimeStatus::NotInstalled);
    assert!(host.is_installed("fake-a"));
    assert!(!host.is_installed("fake-z"));
}

#[test]
fn three_crashes_mark_the_add_on_broken_and_tell_the_health_hook() {
    let s = Scratch::new();
    let (host, seen) = recording_host();
    host.replace(live(&fake("fake-a", "Fake A"), &s));
    host.replace(live(&fake("fake-b", "Fake B"), &s));
    for _ in 0..2 {
        let e = synth_err(&host, "fake-a:alpha", "@crash");
        assert_eq!(e.message, "Fake A stopped unexpectedly.");
        assert_eq!(e.code, ReadErrorCode::Unknown);
        assert!(e.retryable);
        assert_eq!(host.status("fake-a"), RuntimeStatus::Ready);
    }
    assert!(seen.lock().unwrap().is_empty());
    let e = synth_err(&host, "fake-a:alpha", "@crash");
    assert_plain_error(&e, "Fake A keeps stopping. Repair it on the Voices page.");
    assert_eq!(host.status("fake-a"), RuntimeStatus::Broken(BrokenReason::KeepsStopping));
    assert_eq!(*seen.lock().unwrap(), ["fake-a"]);

    let e = synth_err(&host, "fake-a:alpha", "fine now");
    assert_plain_error(&e, "Fake A keeps stopping. Repair it on the Voices page.");
    assert_eq!(spawns(&s, "fake-a-count.txt"), 3, "a broken add-on is not started again");
    assert_eq!(host.status("fake-b"), RuntimeStatus::Ready);
    synth(&host, "fake-b:alpha", "unaffected").unwrap();
}

#[test]
fn a_helper_that_speaks_only_protocol_2_is_incompatible() {
    let s = Scratch::new();
    let (host, seen) = recording_host();
    let mut a = fake("fake-a", "Fake A");
    a.args = vec!["--mode".into(), "protocol:2".into()];
    host.replace(live(&a, &s));
    let e = synth_err(&host, "fake-a:alpha", "hi");
    assert_plain_error(&e, "Fake A needs an update.");
    assert_eq!(host.status("fake-a"), RuntimeStatus::Incompatible);
    assert_eq!(*seen.lock().unwrap(), ["fake-a"]);
    let e = synth_err(&host, "fake-a:alpha", "hi");
    assert_plain_error(&e, "Fake A needs an update.");
    assert_eq!(spawns(&s, "fake-a-count.txt"), 1);
}

#[test]
fn a_replaced_add_on_reports_health_under_its_id() {
    let s = Scratch::new();
    let (host, seen) = recording_host();
    host.replace(live(&fake("fake-a", "Fake A"), &s));
    let mut v2 = fake("fake-a", "Fake A");
    v2.version = "2.0.0".into();
    v2.args = vec!["--mode".into(), "protocol:2".into()];
    host.replace(live(&v2, &s));
    assert_eq!(synth_err(&host, "fake-a:alpha", "hi").message, "Fake A needs an update.");
    assert_eq!(*seen.lock().unwrap(), ["fake-a"]);
}

#[test]
fn quit_if_idle_quits_only_a_helper_idle_longer_than_the_limit() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    let helper = &host.runtime("fake-a").unwrap().helper;
    assert!(!helper.quit_if_idle(Duration::ZERO, Duration::from_secs(1)), "nothing runs yet");
    synth(&host, "fake-a:alpha", "hi").unwrap();
    let pid = spawned_pids(&s, "fake-a-count.txt")[0];
    assert!(!helper.quit_if_idle(Duration::from_secs(60), Duration::from_secs(1)));
    assert!(helper.is_running() && is_alive(pid));
    std::thread::sleep(Duration::from_millis(300));
    assert!(helper.idle_for() >= Duration::from_millis(300));
    assert!(helper.quit_if_idle(Duration::from_millis(200), Duration::from_secs(1)));
    assert!(!helper.is_running());
    assert!(gone(pid));
    assert_eq!(host.status("fake-a"), RuntimeStatus::Ready, "quitting an idle helper is not a crash");
    synth(&host, "fake-a:alpha", "again").unwrap();
    assert_eq!(spawns(&s, "fake-a-count.txt"), 2, "the next read starts it again");
}

#[test]
fn quit_if_idle_never_quits_a_helper_in_use() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    let runtime = host.runtime("fake-a").unwrap();
    std::thread::scope(|sc| {
        let slow = sc.spawn(|| synth(&host, "fake-a:alpha", "@slow:800 busy"));
        assert!(eventually(WAIT, || has_line(&s, "fake-a", "text 2 1.0.0 @slow")));
        assert!(!runtime.helper.quit_if_idle(Duration::ZERO, Duration::from_secs(1)));
        slow.join().unwrap().expect("the request is not disturbed");
    });
    assert!(runtime.helper.is_running());
}

#[test]
fn reap_idle_leaves_a_just_used_helper_running() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.extra = json!({"idle_exit_sec": 1});
    let mut b = fake("fake-b", "Fake B");
    b.extra = json!({"idle_exit_sec": 0});
    let host = host_of(vec![live(&a, &s), live(&b, &s), live(&fake("fake-c", "Fake C"), &s)]);
    assert_eq!(host.runtime("fake-a").unwrap().manifest.idle_exit_sec, 30, "clamped up to 30 s");
    assert_eq!(host.runtime("fake-b").unwrap().manifest.idle_exit_sec, 0);
    assert_eq!(host.runtime("fake-c").unwrap().manifest.idle_exit_sec, 300);
    for id in ["fake-a", "fake-b", "fake-c"] {
        synth(&host, &format!("{id}:alpha"), "hi").unwrap();
    }
    std::thread::sleep(Duration::from_millis(1200));
    host.reap_idle(Duration::from_secs(1));
    for id in ["fake-a", "fake-b", "fake-c"] {
        assert!(host.runtime(id).unwrap().helper.is_running(), "{id}");
        assert!(is_alive(spawned_pids(&s, &count_of(id))[0]), "{id}");
    }
}

#[test]
fn shutdown_stops_every_helper_within_its_grace() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s), live(&fake("fake-b", "Fake B"), &s)]);
    synth(&host, "fake-a:alpha", "hi").unwrap();
    synth(&host, "fake-b:alpha", "hi").unwrap();
    let pids = [spawned_pids(&s, "fake-a-count.txt")[0], spawned_pids(&s, "fake-b-count.txt")[0]];
    let started = Instant::now();
    host.shutdown(Duration::from_millis(400));
    assert!(started.elapsed() < Duration::from_millis(1500), "took {:?}", started.elapsed());
    for pid in pids {
        assert!(gone(pid), "{pid} still runs");
    }
}

#[test]
fn shutdown_during_a_request_ends_it_promptly() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s), live(&fake("fake-b", "Fake B"), &s)]);
    synth(&host, "fake-b:alpha", "hi").unwrap();
    std::thread::scope(|sc| {
        let slow = sc.spawn(|| synth(&host, "fake-a:alpha", "@slow:5000 long"));
        assert!(eventually(WAIT, || has_line(&s, "fake-a", "text 2 1.0.0 @slow")));
        let pids = [spawned_pids(&s, "fake-a-count.txt")[0], spawned_pids(&s, "fake-b-count.txt")[0]];
        let started = Instant::now();
        host.shutdown(Duration::from_millis(400));
        let e = slow.join().unwrap().expect_err("the request ends");
        assert!(started.elapsed() < Duration::from_millis(1500), "took {:?}", started.elapsed());
        assert_eq!(e.code, ReadErrorCode::Unknown);
        assert!(!e.retryable);
        for pid in pids {
            assert!(gone(pid), "{pid} still runs");
        }
    });
}

#[test]
fn load_spec_starts_the_exe_inside_the_folder_with_the_manifest_settings() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.args = vec!["--tag".into(), "x".into()];
    a.extra = json!({"timeouts": {"idle_ms": 7000}, "env": {"FAKE_FLAG": "1"}, "notices": ["NOTICE.txt"]});
    let dir = a.write(&s);
    std::fs::write(dir.join("NOTICE.txt"), "notice").unwrap();
    let data = s.file("fake-a-data");
    let (manifest, spec) = load_spec(&dir, Some(&data)).unwrap();
    let exe = if cfg!(windows) { "bin/vox-addon-fake.exe" } else { "bin/vox-addon-fake" };
    assert_eq!(manifest.exe, exe);
    assert_eq!(spec.program.canonicalize().unwrap(), dir.join(exe).canonicalize().unwrap());
    assert_eq!(spec.dir.canonicalize().unwrap(), dir.canonicalize().unwrap());
    assert!(spec.program.is_absolute());
    assert_eq!(spec.data_dir.as_deref(), Some(data.as_path()));
    assert_eq!(spec.args, manifest.args);
    assert_eq!(spec.args.last().map(String::as_str), Some("x"));
    assert_eq!(spec.env, [("FAKE_FLAG".to_string(), "1".to_string())]);
    assert_eq!(spec.timeouts, HelperTimeouts::from_manifest(&manifest.timeouts));
    assert_eq!(spec.timeouts.idle, Duration::from_secs(7));
    assert_eq!((spec.addon_id.as_str(), spec.name.as_str(), spec.version.as_str()), ("fake-a", "Fake A", "1.0.0"));
    let runtime = AddonRuntime::load(&dir, None).unwrap();
    assert_eq!(runtime.known_voices().unwrap().len(), 2);
}

#[cfg(not(windows))]
#[test]
fn load_rejects_an_exe_that_links_outside_the_folder() {
    let s = Scratch::new();
    let dir = fake("fake-a", "Fake A").write(&s);
    let exe = dir.join("bin/vox-addon-fake");
    std::fs::remove_file(&exe).unwrap();
    std::os::unix::fs::symlink(FAKE, &exe).unwrap();
    let e = load_spec(&dir, None).expect_err("a link out of the folder is refused");
    assert!(e.contains("bin/vox-addon-fake is not a file inside the add-on folder"), "{e}");
    assert!(AddonRuntime::load(&dir, None).is_err());

    let outside = s.file("outside-bin");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::copy(FAKE, outside.join("vox-addon-fake")).unwrap();
    std::fs::remove_dir_all(dir.join("bin")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("bin")).unwrap();
    let e = load_spec(&dir, None).expect_err("a linked folder out of the add-on is refused");
    assert!(e.contains("is not a file inside the add-on folder"), "{e}");
}

#[test]
fn load_rejects_an_exe_path_that_leaves_the_folder() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.extra = json!({"exe": "../vox-addon-fake"});
    let dir = a.write(&s);
    std::fs::copy(FAKE, dir.parent().unwrap().join("vox-addon-fake")).unwrap();
    assert!(load_spec(&dir, None).is_err());
}

#[test]
fn load_rejects_a_missing_exe() {
    let s = Scratch::new();
    let dir = fake("fake-a", "Fake A").write(&s);
    let exe = if cfg!(windows) { "bin/vox-addon-fake.exe" } else { "bin/vox-addon-fake" };
    std::fs::remove_file(dir.join(exe)).unwrap();
    let e = load_spec(&dir, None).expect_err("a missing exe is refused");
    assert!(e.starts_with(exe), "{e}");
    assert!(AddonRuntime::load(&dir, None).is_err());
}

#[test]
fn load_rejects_an_exe_that_is_a_folder() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.extra = json!({"exe": if cfg!(windows) { "bin.exe" } else { "bin" }});
    let dir = a.write(&s);
    if cfg!(windows) {
        std::fs::create_dir_all(dir.join("bin.exe")).unwrap();
    }
    assert!(load_spec(&dir, None).unwrap_err().contains("is not a file inside the add-on folder"));
}

#[test]
fn load_rejects_a_notice_that_does_not_exist() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.extra = json!({"notices": ["licenses/NOTICE.txt"]});
    let dir = a.write(&s);
    let e = load_spec(&dir, None).expect_err("a missing notice is refused");
    assert!(e.starts_with("licenses/NOTICE.txt"), "{e}");
    std::fs::create_dir_all(dir.join("licenses/NOTICE.txt")).unwrap();
    assert!(load_spec(&dir, None).unwrap_err().contains("is not a file inside the add-on folder"), "a folder is not a notice");
    std::fs::remove_dir(dir.join("licenses/NOTICE.txt")).unwrap();
    std::fs::write(dir.join("licenses/NOTICE.txt"), "notice").unwrap();
    assert!(load_spec(&dir, None).is_ok());
}

#[test]
fn load_rejects_a_bad_addon_json() {
    let s = Scratch::new();
    let dir = fake("fake-a", "Fake A").write(&s);
    let manifest = dir.join("addon.json");
    let good = std::fs::read(&manifest).unwrap();

    std::fs::write(&manifest, b"{ not json").unwrap();
    assert!(load_spec(&dir, None).unwrap_err().starts_with("not JSON"));

    std::fs::write(&manifest, b"[1, 2]").unwrap();
    assert!(load_spec(&dir, None).is_err());

    let mut v: serde_json::Value = serde_json::from_slice(&good).unwrap();
    v["schema"] = json!(2);
    std::fs::write(&manifest, v.to_string()).unwrap();
    assert!(load_spec(&dir, None).is_err());

    let mut v: serde_json::Value = serde_json::from_slice(&good).unwrap();
    v["protocol"] = json!({"min": 2, "max": 3});
    std::fs::write(&manifest, v.to_string()).unwrap();
    assert!(load_spec(&dir, None).unwrap_err().contains("not supported"));

    let mut big = good.clone();
    big.extend(std::iter::repeat_n(b' ', 1 << 20));
    std::fs::write(&manifest, big).unwrap();
    assert!(load_spec(&dir, None).unwrap_err().contains("too big"));

    std::fs::remove_file(&manifest).unwrap();
    assert!(load_spec(&dir, None).is_err());
    assert!(AddonRuntime::load(&dir, None).is_err());

    std::fs::write(&manifest, good).unwrap();
    assert!(load_spec(&dir, None).is_ok());
}

#[test]
fn remove_fails_the_running_and_the_waiting_request() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    std::thread::scope(|sc| {
        let running = sc.spawn(|| synth(&host, "fake-a:alpha", "@slow:5000 first"));
        assert!(eventually(WAIT, || has_line(&s, "fake-a", "text 2 1.0.0 @slow")));
        let waiting = sc.spawn(|| synth(&host, "fake-a:alpha", "second"));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!waiting.is_finished(), "it waits for its turn");
        let started = Instant::now();
        host.remove("fake-a", Duration::from_secs(2));
        for request in [running, waiting] {
            let e = request.join().unwrap().expect_err("removed");
            assert_plain_error(&e, "Fake A was removed.");
        }
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    });
    assert!(!log_of(&s, "fake-a").iter().any(|l| l.contains("second")), "the waiting request never reached the helper");
}

/// The waiting request may get its turn just after the running one gives up; it must still not start a helper from
/// the folder being removed. Repeated because which of the two notices the removal first depends on their 50 ms ticks.
#[test]
fn a_request_waiting_when_the_add_on_is_removed_never_starts_a_helper() {
    for round in 0..8u64 {
        let s = Scratch::new();
        let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
        let runtime = host.runtime("fake-a").unwrap();
        std::thread::scope(|sc| {
            let running = sc.spawn(|| synth(&host, "fake-a:alpha", "@slow:5000 first"));
            assert!(eventually(WAIT, || has_line(&s, "fake-a", "text 2 1.0.0 @slow")));
            // Sweep the waiter's tick against the running request's tick.
            std::thread::sleep(Duration::from_millis(round * 7));
            let waiting = sc.spawn(|| synth(&host, "fake-a:alpha", "second"));
            std::thread::sleep(Duration::from_millis(80));
            host.remove("fake-a", Duration::from_secs(2));
            for request in [running, waiting] {
                assert_eq!(request.join().unwrap().expect_err("removed").message, "Fake A was removed.");
            }
        });
        // The client's own count: a helper killed at once may not get as far as writing to the count file.
        assert_eq!(runtime.helper.spawn_count(), 1, "round {round}");
    }
}

#[test]
fn installing_a_broken_add_on_again_repairs_it() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    for _ in 0..3 {
        synth_err(&host, "fake-a:alpha", "@crash");
    }
    assert_eq!(host.status("fake-a"), RuntimeStatus::Broken(BrokenReason::KeepsStopping));
    host.replace(live(&fake("fake-a", "Fake A"), &s));
    assert_eq!(host.status("fake-a"), RuntimeStatus::Ready);
    synth(&host, "fake-a:alpha", "repaired").unwrap();
    assert_eq!(spawns(&s, "fake-a-count.txt"), 4);
}

#[test]
fn a_missing_helper_program_marks_the_add_on_incomplete() {
    let s = Scratch::new();
    let (host, seen) = recording_host();
    let runtime = fake("fake-a", "Fake A").runtime(&s);
    std::fs::remove_file(&runtime.helper.spec().program).unwrap();
    host.replace(runtime);
    let e = synth_err(&host, "fake-a:alpha", "hi");
    assert_plain_error(&e, "Fake A is incomplete. Repair it on the Voices page.");
    assert_eq!(host.status("fake-a"), RuntimeStatus::Broken(BrokenReason::Incomplete));
    assert_eq!(*seen.lock().unwrap(), ["fake-a"]);
}

#[test]
fn helper_error_codes_reach_the_reader_with_the_add_on_s_name() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.voices = Some(json!([{"key": "alpha", "name": "Alpha"}, {"key": "gamma", "name": "Gamma"}]));
    let host = host_of(vec![live(&a, &s)]);
    let cases = [
        ("gamma", "hi", "This voice isn’t in Fake A 1.0.0."),
        ("alpha", "@err:unknown_voice", "This voice isn’t in Fake A 1.0.0."),
        ("alpha", "@err:out_of_memory", "Not enough memory for Fake A."),
        ("alpha", "@err:model_load_failed", "Fake A couldn’t load its voice model. Try repairing it."),
        ("alpha", "@err:internal", "Fake A couldn’t read this text."),
        ("alpha", "@bad-wav", "Fake A sent audio Vox couldn’t use."),
    ];
    for (voice, text, message) in cases {
        assert_plain_error(&synth_err(&host, &format!("fake-a:{voice}"), text), message);
    }
    assert_eq!(spawns(&s, "fake-a-count.txt"), 1, "none of these restarts the helper");
    assert_eq!(host.status("fake-a"), RuntimeStatus::Ready);
}

#[test]
fn a_cancelled_read_returns_cancelled_and_the_next_one_works() {
    let s = Scratch::new();
    let host = host_of(vec![live(&fake("fake-a", "Fake A"), &s)]);
    let cancel = CancelToken::new();
    std::thread::scope(|sc| {
        let slow = sc.spawn(|| host.synthesize("@slow:5000 long", "fake-a:alpha", true, &cancel));
        assert!(eventually(WAIT, || has_line(&s, "fake-a", "text 2 1.0.0 @slow")));
        let started = Instant::now();
        cancel.cancel();
        let e = slow.join().unwrap().expect_err("cancelled");
        assert!(e.aborted, "{e:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    });
    synth(&host, "fake-a:alpha", "after").unwrap();
    assert_eq!(spawns(&s, "fake-a-count.txt"), 1, "the helper wound down in time and was kept");
}

#[test]
fn list_voices_skips_an_add_on_whose_helper_cannot_list_them() {
    let s = Scratch::new();
    let mut a = fake("fake-a", "Fake A");
    a.voices = None;
    a.args = vec!["--mode".into(), "exit-at-start".into()];
    let host = host_of(vec![live(&a, &s), live(&fake("fake-b", "Fake B"), &s)]);
    let ids: Vec<String> = host.list_voices().unwrap().into_iter().map(|v| v.voice_id).collect();
    assert_eq!(ids, ["addon:fake-b:alpha", "addon:fake-b:beta"]);
    assert_eq!(spawns(&s, "fake-b-count.txt"), 0);
}
