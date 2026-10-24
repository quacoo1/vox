//! Tolerant response mapping (tts.md §2.5). Helpers accept any JSON and silently drop wrong-typed fields.

use std::cmp::Ordering;
use std::sync::OnceLock;

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use icu_collator::options::{CollatorOptions, Strength};
use icu_collator::preferences::CollationNumericOrdering;
use icu_collator::{CollatorBorrowed, CollatorPreferences};
use serde_json::Value;
use vox_core::js::js_trim;
use vox_core::tts::Alignment;
use vox_core::voices::{AccountInfo, ModelLanguage, TtsModel, Voice};

use crate::error::TtsError;
use crate::jsval::{js_string, num_of, str_of};

/// `mapVoice(raw)`: `None` without a non-empty `voice_id`; premade names `"George - Warm…"` split into name +
/// `labels.tagline`.
pub fn map_voice(raw: &Value) -> Option<Voice> {
    let obj = raw.as_object()?;
    let voice_id = str_of(obj.get("voice_id"))?.to_string();
    let category = str_of(obj.get("category")).map(str::to_string);
    let mut name = str_of(obj.get("name")).map(js_trim).filter(|n| !n.is_empty()).unwrap_or(&voice_id).to_string();

    let mut labels: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(raw_labels)) = obj.get("labels") {
        for (k, v) in raw_labels {
            match v {
                Value::String(s) if !js_trim(s).is_empty() => labels.push((k.clone(), js_trim(s).to_string())),
                Value::Number(_) | Value::Bool(_) => labels.push((k.clone(), js_string(v))),
                _ => {}
            }
        }
    }

    // Premade voices are named "George - Warm, Captivating Storyteller": keep the short name, move the rest.
    if category.as_deref() == Some("premade") {
        if let Some(dash) = name.find(" - ").filter(|&at| at > 0) {
            let tagline = js_trim(&name[dash + 3..]).to_string();
            if !tagline.is_empty() && !labels.iter().any(|(k, _)| k == "tagline") {
                labels.push(("tagline".into(), tagline));
            }
            name = js_trim(&name[..dash]).to_string();
        }
    }

    Some(Voice {
        voice_id,
        name,
        labels,
        category,
        description: str_of(obj.get("description")).map(js_trim).filter(|d| !d.is_empty()).map(str::to_string),
        preview_url: str_of(obj.get("preview_url")).map(str::to_string),
    })
}

/// The voice-name collator: root locale, primary strength (case- and accent-insensitive), numeric ordering
/// (`localeCompare(…, {sensitivity: 'base', numeric: true})`). `None` only if the compiled data were unusable.
fn name_collator() -> Option<&'static CollatorBorrowed<'static>> {
    static COLLATOR: OnceLock<Option<CollatorBorrowed<'static>>> = OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            let mut prefs = CollatorPreferences::default();
            prefs.numeric_ordering = Some(CollationNumericOrdering::True);
            let mut options = CollatorOptions::default();
            options.strength = Some(Strength::Primary);
            match CollatorBorrowed::try_new(prefs, options) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(target: "tts", "voice collator unavailable ({e}); sorting names by lower case");
                    None
                }
            }
        })
        .as_ref()
}

fn compare_names(a: &str, b: &str) -> Ordering {
    match name_collator() {
        Some(c) => c.compare(a, b),
        None => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

fn voice_rank(v: &Voice) -> u8 {
    match v.category.as_deref() {
        Some("cloned") | Some("generated") | Some("professional") => 0,
        Some("premade") => 2,
        _ => 1,
    }
}

/// `sortVoices`: de-duplicate by id (first wins), then own voices (cloned/generated/professional) rank 0, other
/// non-premade 1, premade 2; within a rank by name with `icu_collator` (root locale, `Strength::Primary`, numeric
/// ordering on — `localeCompare(…, {sensitivity:'base', numeric:true})`), stable (D-VO-1). Must pass the tts.md §2.5
/// examples (`Émile`/`emile` equal → input order kept).
pub fn sort_voices(voices: Vec<Voice>) -> Vec<Voice> {
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<Voice> = voices.into_iter().filter(|v| seen.insert(v.voice_id.clone())).collect();
    unique.sort_by(|a, b| voice_rank(a).cmp(&voice_rank(b)).then_with(|| compare_names(&a.name, &b.name)));
    unique
}

/// `mapModel(raw)`: only `can_do_text_to_speech === true` with a non-empty `model_id`.
pub fn map_model(raw: &Value) -> Option<TtsModel> {
    let obj = raw.as_object()?;
    if obj.get("can_do_text_to_speech") != Some(&Value::Bool(true)) {
        return None;
    }
    let model_id = str_of(obj.get("model_id"))?.to_string();
    let languages = match obj.get("languages") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_object)
            .map(|l| {
                let id = str_of(l.get("language_id")).unwrap_or("").to_string();
                let name = str_of(l.get("name")).or_else(|| str_of(l.get("language_id"))).unwrap_or("").to_string();
                ModelLanguage { id, name }
            })
            .filter(|l| !l.id.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let cost_factor = match obj.get("model_rates") {
        Some(Value::Object(rates)) => num_of(rates.get("character_cost_multiplier")),
        _ => None,
    };
    Some(TtsModel {
        name: str_of(obj.get("name")).unwrap_or(&model_id).to_string(),
        description: str_of(obj.get("description")).map(js_trim).filter(|d| !d.is_empty()).map(str::to_string),
        languages,
        max_chars: num_of(obj.get("maximum_text_length_per_request"))
            .or_else(|| num_of(obj.get("max_characters_request_subscribed_user"))),
        cost_factor,
        model_id,
    })
}

/// `mapAccount(raw)`: a non-object → `bad_response`.
pub fn map_account(raw: &Value) -> Result<AccountInfo, TtsError> {
    let obj = raw.as_object().ok_or_else(TtsError::bad_response)?;
    Ok(AccountInfo {
        tier: str_of(obj.get("tier")).unwrap_or("unknown").to_string(),
        character_count: num_of(obj.get("character_count")).unwrap_or(0.0),
        character_limit: num_of(obj.get("character_limit")).unwrap_or(0.0),
        next_reset_unix: num_of(obj.get("next_character_count_reset_unix")),
        status: str_of(obj.get("status")).map(str::to_string),
    })
}

/// `mapAlignment(raw)`: requires the three arrays, equal non-zero length, finite times; chars stringified JS-style.
pub fn map_alignment(raw: &Value) -> Option<Alignment> {
    let obj = raw.as_object()?;
    let chars = obj.get("characters")?.as_array()?;
    let starts = obj.get("character_start_times_seconds")?.as_array()?;
    let ends = obj.get("character_end_times_seconds")?.as_array()?;
    let n = chars.len();
    if n == 0 || starts.len() != n || ends.len() != n {
        return None;
    }
    let times = |values: &[Value]| -> Option<Vec<f64>> { values.iter().map(|v| num_of(Some(v))).collect() };
    Some(Alignment { chars: chars.iter().map(js_string).collect(), starts: times(starts)?, ends: times(ends)? })
}

/// `mimeForFormat`: mp3 → audio/mpeg, wav → audio/wav, opus → audio/ogg, pcm → audio/pcm, ulaw/alaw → audio/basic,
/// else application/octet-stream.
pub fn mime_for_format(format: &str) -> &'static str {
    if format.starts_with("mp3") {
        "audio/mpeg"
    } else if format.starts_with("wav") {
        "audio/wav"
    } else if format.starts_with("opus") {
        "audio/ogg"
    } else if format.starts_with("pcm") {
        "audio/pcm"
    } else if format.starts_with("ulaw") || format.starts_with("alaw") {
        "audio/basic"
    } else {
        "application/octet-stream"
    }
}

/// `mp3Kbps`: `^mp3_\d+_(\d+)$` → the bitrate (0 → `None`).
pub fn mp3_kbps(format: &str) -> Option<u32> {
    let rest = format.strip_prefix("mp3_")?;
    let (rate, kbps) = rest.split_once('_')?;
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(rate) || !all_digits(kbps) {
        return None;
    }
    kbps.parse::<u32>().ok().filter(|&k| k > 0)
}

/// Node-compatible lenient base64 (tts.md §2.6 step 2): both alphabets, skip invalid chars, stop at the first `=`,
/// drop a dangling sextet.
pub fn decode_base64_lenient(s: &str) -> Vec<u8> {
    let mut clean = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '=' => break,
            '-' => clean.push('+'),
            '_' => clean.push('/'),
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => clean.push(c),
            _ => {}
        }
    }
    if clean.len() % 4 == 1 {
        clean.pop();
    }
    let engine = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    engine.decode(clean.as_bytes()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn voice(id: &str, name: &str, category: Option<&str>) -> Voice {
        Voice {
            voice_id: id.into(),
            name: name.into(),
            labels: vec![],
            category: category.map(str::to_string),
            description: None,
            preview_url: None,
        }
    }

    #[test]
    fn map_voice_example_and_premade_split() {
        let v = map_voice(&json!({
            "voice_id": "v1",
            "name": "George - Warm, Captivating Storyteller",
            "category": "premade",
            "labels": {"accent": " british ", "age": "", "n": 3}
        }))
        .unwrap();
        assert_eq!(
            vox_core::json::to_js_compact(&v),
            r#"{"voiceId":"v1","name":"George","labels":{"accent":"british","n":"3","tagline":"Warm, Captivating Storyteller"},"category":"premade"}"#
        );
        // `" - "` at the end after trimming, or at index 0: no split.
        assert_eq!(map_voice(&json!({"voice_id":"a","name":"A - ","category":"premade"})).unwrap().name, "A -");
        assert_eq!(map_voice(&json!({"voice_id":"a","name":"  - x","category":"premade"})).unwrap().name, "- x");
        // Non-premade names are never split; category is case-sensitive.
        assert_eq!(map_voice(&json!({"voice_id":"a","name":"A - B","category":"cloned"})).unwrap().name, "A - B");
        assert_eq!(map_voice(&json!({"voice_id":"a","name":"A - B","category":"Premade"})).unwrap().name, "A - B");
        // An existing tagline label wins; the name is still split.
        let v = map_voice(&json!({"voice_id":"a","name":"A - B","category":"premade","labels":{"tagline":"T"}})).unwrap();
        assert_eq!(v.name, "A");
        assert_eq!(v.labels, vec![("tagline".to_string(), "T".to_string())]);
    }

    #[test]
    fn map_voice_tolerance() {
        assert!(map_voice(&json!(null)).is_none());
        assert!(map_voice(&json!([])).is_none());
        assert!(map_voice(&json!({"name":"x"})).is_none());
        assert!(map_voice(&json!({"voice_id":""})).is_none());
        assert!(map_voice(&json!({"voice_id":5})).is_none());
        let v = map_voice(&json!({
            "voice_id": "id1", "name": "   ", "description": "  d  ", "preview_url": " u ",
            "labels": {"a": true, "b": 1.5, "c": null, "d": [1], "e": {}, "f": "  ", "g": " x "}
        }))
        .unwrap();
        assert_eq!(v.name, "id1");
        assert_eq!(v.description.as_deref(), Some("d"));
        assert_eq!(v.preview_url.as_deref(), Some(" u "));
        assert_eq!(v.category, None);
        assert_eq!(
            v.labels,
            vec![("a".into(), "true".into()), ("b".into(), "1.5".into()), ("g".into(), "x".into())]
        );
        let v = map_voice(&json!({"voice_id":"id","description":"   ","preview_url":"","labels":"nope"})).unwrap();
        assert_eq!((v.description, v.preview_url, v.labels.len()), (None, None, 0));
    }

    #[test]
    fn sort_voices_examples() {
        let sorted = sort_voices(vec![
            voice("z", "zed", Some("premade")),
            voice("v10", "Voice 10", None),
            voice("v2", "voice 2", None),
            voice("m", "Mine", Some("cloned")),
            voice("z", "zed duplicate", Some("cloned")),
        ]);
        let names: Vec<&str> = sorted.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Mine", "voice 2", "Voice 10", "zed"]);

        let sorted = sort_voices(vec![
            voice("z", "zed", Some("premade")),
            voice("v10", "Voice 10", None),
            voice("v2", "voice 2", None),
            voice("m", "Mine", Some("cloned")),
            voice("e1", "Émile", Some("generated")),
            voice("e2", "emile", Some("professional")),
        ]);
        let names: Vec<&str> = sorted.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Émile", "emile", "Mine", "voice 2", "Voice 10", "zed"]);
        // Equal at primary strength in the other input order: input order kept (stable).
        let sorted = sort_voices(vec![voice("e2", "emile", Some("cloned")), voice("e1", "Émile", Some("cloned"))]);
        assert_eq!(sorted[0].voice_id, "e2");
        // Unknown categories rank with "no category".
        let sorted = sort_voices(vec![voice("p", "A", Some("premade")), voice("x", "B", Some("famous")), voice("o", "C", Some("cloned"))]);
        let ids: Vec<&str> = sorted.iter().map(|v| v.voice_id.as_str()).collect();
        assert_eq!(ids, ["o", "x", "p"]);
        assert!(sort_voices(vec![]).is_empty());
    }

    #[test]
    fn map_model_cases() {
        let m = map_model(&json!({
            "model_id": "eleven_flash_v2_5",
            "can_do_text_to_speech": true,
            "name": "Eleven Flash v2.5",
            "description": "  fast  ",
            "languages": [{"language_id":"en","name":"English"},{"language_id":""},{"name":"x"},"str",{"language_id":"fr"}],
            "maximum_text_length_per_request": 0,
            "model_rates": {"character_cost_multiplier": 0.5}
        }))
        .unwrap();
        assert_eq!(m.model_id, "eleven_flash_v2_5");
        assert_eq!(m.name, "Eleven Flash v2.5");
        assert_eq!(m.description.as_deref(), Some("fast"));
        assert_eq!(
            m.languages,
            vec![ModelLanguage { id: "en".into(), name: "English".into() }, ModelLanguage { id: "fr".into(), name: "fr".into() }]
        );
        assert_eq!(m.max_chars, Some(0.0));
        assert_eq!(m.cost_factor, Some(0.5));

        assert!(map_model(&json!({"model_id":"a","can_do_text_to_speech":"true"})).is_none());
        assert!(map_model(&json!({"model_id":"a"})).is_none());
        assert!(map_model(&json!({"model_id":"","can_do_text_to_speech":true})).is_none());
        assert!(map_model(&json!("x")).is_none());
        let m = map_model(&json!({"model_id":"m","can_do_text_to_speech":true,"name":" n ","languages":"x",
            "max_characters_request_subscribed_user": 5000, "model_rates": 3})).unwrap();
        assert_eq!(m.name, " n ");
        assert!(m.languages.is_empty());
        assert_eq!(m.max_chars, Some(5000.0));
        assert_eq!(m.cost_factor, None);
        assert_eq!(map_model(&json!({"model_id":"m","can_do_text_to_speech":true})).unwrap().name, "m");
    }

    #[test]
    fn map_account_does_not_coerce() {
        let a = map_account(&json!({"tier":"", "character_count":"5", "character_limit":100, "next_character_count_reset_unix": 1700000000, "status":"active"})).unwrap();
        assert_eq!(a.tier, "unknown");
        assert_eq!(a.character_count, 0.0);
        assert_eq!(a.character_limit, 100.0);
        assert_eq!(a.next_reset_unix, Some(1_700_000_000.0));
        assert_eq!(a.status.as_deref(), Some("active"));
        let a = map_account(&json!({"tier":"creator","character_count":12.5})).unwrap();
        assert_eq!((a.tier.as_str(), a.character_count, a.character_limit, a.next_reset_unix, a.status), ("creator", 12.5, 0.0, None, None));
        assert_eq!(map_account(&json!([1])).unwrap_err(), TtsError::bad_response());
        assert_eq!(map_account(&json!(null)).unwrap_err().message, vox_core::read::eleven_messages::BAD_RESPONSE);
    }

    #[test]
    fn map_alignment_cases() {
        let a = map_alignment(&json!({
            "characters": ["H", 1, null, true],
            "character_start_times_seconds": [0, 0.1, 0.2, 0.3],
            "character_end_times_seconds": [0.1, 0.2, 0.3, 0.4]
        }))
        .unwrap();
        assert_eq!(a.chars, ["H", "1", "null", "true"]);
        assert_eq!(a.starts, [0.0, 0.1, 0.2, 0.3]);
        // Times are not checked for order or sign.
        assert!(map_alignment(&json!({"characters":["a","b"],"character_start_times_seconds":[1,-1],"character_end_times_seconds":[0,0]})).is_some());
        // Unequal or zero lengths, missing arrays, non-numbers.
        assert!(map_alignment(&json!({"characters":["a"],"character_start_times_seconds":[0,1],"character_end_times_seconds":[1]})).is_none());
        assert!(map_alignment(&json!({"characters":[],"character_start_times_seconds":[],"character_end_times_seconds":[]})).is_none());
        assert!(map_alignment(&json!({"characters":["a"],"character_start_times_seconds":[0]})).is_none());
        assert!(map_alignment(&json!({"characters":["a"],"character_start_times_seconds":["0"],"character_end_times_seconds":[1]})).is_none());
        assert!(map_alignment(&json!({"characters":"a","character_start_times_seconds":[0],"character_end_times_seconds":[1]})).is_none());
        assert!(map_alignment(&json!(null)).is_none());
        // A non-finite time (`1e400` parsed leniently becomes the string "Infinity") → None.
        let v = vox_core::json::parse_lenient(
            r#"{"characters":["a"],"character_start_times_seconds":[0],"character_end_times_seconds":[1e400]}"#,
        )
        .unwrap();
        assert!(map_alignment(&v).is_none());
    }

    #[test]
    fn mime_and_bitrate() {
        assert_eq!(mime_for_format("mp3_44100_128"), "audio/mpeg");
        assert_eq!(mime_for_format("wav_44100"), "audio/wav");
        assert_eq!(mime_for_format("opus_48000_64"), "audio/ogg");
        assert_eq!(mime_for_format("pcm_16000"), "audio/pcm");
        assert_eq!(mime_for_format("ulaw_8000"), "audio/basic");
        assert_eq!(mime_for_format("alaw_8000"), "audio/basic");
        assert_eq!(mime_for_format("flac"), "application/octet-stream");
        assert_eq!(mime_for_format("MP3_44100_128"), "application/octet-stream");

        assert_eq!(mp3_kbps("mp3_44100_128"), Some(128));
        assert_eq!(mp3_kbps("mp3_44100_64"), Some(64));
        assert_eq!(mp3_kbps("mp3_22050_32"), Some(32));
        assert_eq!(mp3_kbps("mp3_x_0"), None);
        assert_eq!(mp3_kbps("mp3_44100_0"), None);
        assert_eq!(mp3_kbps("mp3_44100"), None);
        assert_eq!(mp3_kbps("mp3_44100_128_x"), None);
        assert_eq!(mp3_kbps("mp3__128"), None);
        assert_eq!(mp3_kbps("mp3_44100_1٢8"), None);
        assert_eq!(mp3_kbps("pcm_44100_128"), None);
    }

    #[test]
    fn lenient_base64_examples() {
        assert_eq!(decode_base64_lenient("QUJD"), b"ABC");
        assert_eq!(decode_base64_lenient("QU JD\n"), b"ABC");
        assert_eq!(decode_base64_lenient("Q!U#JD"), b"ABC");
        assert_eq!(decode_base64_lenient("QUJD=QUJD"), b"ABC");
        assert_eq!(decode_base64_lenient("QUJ"), b"AB");
        assert_eq!(decode_base64_lenient("QUJDQ"), b"ABC");
        assert_eq!(decode_base64_lenient("!!!!"), b"");
        assert_eq!(decode_base64_lenient(""), b"");
        // Both alphabets, even mixed: 0xFB 0xEF 0xBE is "++++" / "----", 0xFF 0xFF 0xFF is "////" / "____".
        assert_eq!(decode_base64_lenient("-_+/"), decode_base64_lenient("+/+/"));
        assert_eq!(decode_base64_lenient("____"), vec![0xFF, 0xFF, 0xFF]);
        // Non-zero trailing bits are accepted.
        assert_eq!(decode_base64_lenient("QUJ="), b"AB");
        assert_eq!(decode_base64_lenient("QUK"), b"AB");
        assert_eq!(decode_base64_lenient("QR"), b"A");
    }
}
