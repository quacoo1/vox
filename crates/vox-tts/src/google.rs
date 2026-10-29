//! Google Cloud Text-to-Speech with an API key (sent in the `x-goog-api-key` header, never in the URL):
//! `POST https://texttospeech.googleapis.com/v1/text:synthesize` → base64 MP3; voices from `GET /v1/voices`. Only the
//! higher-quality families are listed (Chirp 3 HD, Chirp HD, Studio, Neural2, Journey, Polyglot) to keep the list
//! manageable. No word timing, so highlighting is spread evenly over the audio.

use base64::Engine as _;
use serde_json::{json, Value};
use vox_core::providers::{Provider, ProviderCredentials};
use vox_core::voices::Voice;

use crate::cancel::CancelToken;
use crate::client::{ProxyConfig, SynthesisResult};
use crate::error::TtsError;
use crate::providers::{build_agent, mp3_result, send, status_error, voice, SpeechProvider, LIST_TIMEOUT, SYNTH_TIMEOUT};

const NAME: &str = "Google Cloud";
const BASE: &str = "https://texttospeech.googleapis.com/v1";
const FAMILIES: &[(&str, &str)] =
    &[("Chirp3-HD", "Chirp 3 HD"), ("Chirp-HD", "Chirp HD"), ("Studio", "Studio"), ("Neural2", "Neural2"), ("Journey", "Journey"), ("Polyglot", "Polyglot")];

pub struct GoogleClient {
    creds: ProviderCredentials,
    agent: ureq::Agent,
}

impl GoogleClient {
    pub fn new(creds: ProviderCredentials, proxy: Option<&ProxyConfig>, user_agent: &str) -> Self {
        Self { agent: build_agent(user_agent, proxy), creds }
    }

    fn key(&self) -> (&'static str, String) {
        ("x-goog-api-key", self.creds.key.clone())
    }
}

/// `en-US-Chirp3-HD-Achernar` → `en-US`.
fn language_of(name: &str) -> String {
    name.splitn(3, '-').take(2).collect::<Vec<_>>().join("-")
}

fn family_of(name: &str) -> Option<&'static str> {
    FAMILIES.iter().find(|(tag, _)| name.contains(&format!("-{tag}-"))).map(|(_, label)| *label)
}

/// `en-US-Chirp3-HD-Achernar` → `Achernar`, `en-GB-Neural2-A` → `Neural2 A`.
fn display_name(name: &str, family: &str) -> String {
    let tail = name.rsplit('-').next().unwrap_or(name);
    if tail.len() <= 2 {
        format!("{family} {tail}")
    } else {
        tail.to_string()
    }
}

fn map_voice(v: &Value) -> Option<Voice> {
    let name = v.get("name")?.as_str()?;
    let family = family_of(name)?;
    let gender = match v.get("ssmlGender").and_then(Value::as_str) {
        Some("MALE") => "male",
        Some("FEMALE") => "female",
        _ => "",
    };
    let language = language_of(name);
    Some(voice(
        Provider::Google,
        name,
        format!("{} ({language})", display_name(name, family)),
        vec![("gender", gender.to_string()), ("accent", language.clone()), ("language", language), ("use_case", family.to_string())],
    ))
}

fn error(status: u16, body: &[u8]) -> TtsError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = v.pointer("/error/message").and_then(Value::as_str);
    // Google answers a bad or restricted key with 400 INVALID_ARGUMENT / 403 PERMISSION_DENIED.
    let reason = v.pointer("/error/details/0/reason").and_then(Value::as_str).unwrap_or("");
    if status == 400 && (reason == "API_KEY_INVALID" || message.is_some_and(|m| m.contains("API key not valid"))) {
        return status_error(NAME, 401, message, None);
    }
    status_error(NAME, status, message, None)
}

impl SpeechProvider for GoogleClient {
    fn provider(&self) -> Provider {
        Provider::Google
    }

    fn synthesize(&self, text: &str, rest: &str, _word_timing: bool, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        let body = serde_json::to_vec(&json!({
            "input": { "text": text },
            "voice": { "languageCode": language_of(rest), "name": rest },
            "audioConfig": { "audioEncoding": "MP3" },
        }))
        .map_err(|_| TtsError::bad_response())?;
        let headers = [self.key(), ("content-type", "application/json".into())];
        let reply = send(&self.agent, NAME, "POST", &format!("{BASE}/text:synthesize"), &headers, Some(&body), SYNTH_TIMEOUT)?;
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        if !(200..300).contains(&reply.status) {
            return Err(error(reply.status, &reply.body));
        }
        let data: Value = serde_json::from_slice(&reply.body).map_err(|_| TtsError::bad_response())?;
        let audio = data
            .get("audioContent")
            .and_then(Value::as_str)
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .ok_or_else(TtsError::no_audio)?;
        mp3_result(text, audio)
    }

    fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        let reply = send(&self.agent, NAME, "GET", &format!("{BASE}/voices"), &[self.key()], None, LIST_TIMEOUT)?;
        if !(200..300).contains(&reply.status) {
            return Err(error(reply.status, &reply.body));
        }
        let data: Value = serde_json::from_slice(&reply.body).map_err(|_| TtsError::bad_response())?;
        let mut voices: Vec<Voice> = data.get("voices").and_then(Value::as_array).map(|a| a.iter().filter_map(map_voice).collect()).unwrap_or_default();
        voices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.voice_id.cmp(&b.voice_id)));
        Ok(voices)
    }

    fn summary(&self) -> String {
        self.creds.summary(Provider::Google)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voices_keep_quality_families_only() {
        let chirp = map_voice(&json!({"name":"en-US-Chirp3-HD-Achernar","ssmlGender":"FEMALE","languageCodes":["en-US"]})).unwrap();
        assert_eq!(chirp.voice_id, "google:en-US-Chirp3-HD-Achernar");
        assert_eq!(chirp.name, "Achernar (en-US)");
        let neural = map_voice(&json!({"name":"en-GB-Neural2-A","ssmlGender":"FEMALE"})).unwrap();
        assert_eq!(neural.name, "Neural2 A (en-GB)");
        assert!(map_voice(&json!({"name":"en-US-Standard-B","ssmlGender":"MALE"})).is_none());
    }

    #[test]
    fn a_bad_key_is_unauthorized() {
        let e = error(400, br#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}"#);
        assert_eq!(e.code, vox_core::read::ReadErrorCode::Unauthorized);
    }

    /// `cargo test -p vox-tts google_live -- --ignored`
    #[test]
    #[ignore]
    fn google_live_rejects_a_fake_key() {
        let c = GoogleClient::new(ProviderCredentials { key: "AIzaInvalidKeyForTests000000000000000".into(), ..Default::default() }, None, "Vox-test");
        assert_eq!(c.list_voices().unwrap_err().code, vox_core::read::ReadErrorCode::Unauthorized);
    }
}
