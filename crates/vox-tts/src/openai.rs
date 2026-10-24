//! OpenAI text to speech: `POST https://api.openai.com/v1/audio/speech` (`gpt-4o-mini-tts`, MP3), Bearer auth. The
//! voice list is fixed (OpenAI has no voices endpoint); the key is checked with `GET /v1/models`. No word timing, so
//! highlighting is spread evenly over the audio.

use serde_json::{json, Value};
use vox_core::providers::{Provider, ProviderCredentials};
use vox_core::read::ReadErrorCode;
use vox_core::voices::Voice;

use crate::cancel::CancelToken;
use crate::client::{ProxyConfig, SynthesisResult};
use crate::error::TtsError;
use crate::providers::{build_agent, mp3_result, send, status_error, voice, SpeechProvider, LIST_TIMEOUT, SYNTH_TIMEOUT};

const NAME: &str = "OpenAI";
const BASE: &str = "https://api.openai.com/v1";
const MODEL: &str = "gpt-4o-mini-tts";

/// `(voice, gender label, description)`.
const VOICES: &[(&str, &str, &str)] = &[
    ("alloy", "", "Neutral and balanced"),
    ("ash", "male", "Clear and confident"),
    ("ballad", "male", "Warm and melodic"),
    ("coral", "female", "Bright and friendly"),
    ("echo", "male", "Calm and even"),
    ("fable", "male", "Expressive storyteller"),
    ("nova", "female", "Energetic and upbeat"),
    ("onyx", "male", "Deep and steady"),
    ("sage", "female", "Thoughtful and soft"),
    ("shimmer", "female", "Light and gentle"),
    ("verse", "male", "Versatile and lively"),
];

pub struct OpenAiClient {
    creds: ProviderCredentials,
    agent: ureq::Agent,
}

impl OpenAiClient {
    pub fn new(creds: ProviderCredentials, proxy: Option<&ProxyConfig>, user_agent: &str) -> Self {
        Self { agent: build_agent(user_agent, proxy), creds }
    }

    fn auth(&self) -> (&'static str, String) {
        ("authorization", format!("Bearer {}", self.creds.key))
    }
}

/// OpenAI's `{"error": {"message", "code"}}` → a user-facing error (quota problems come back as 429).
fn error(status: u16, body: &[u8], retry_after: Option<&str>) -> TtsError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = v.pointer("/error/message").and_then(Value::as_str);
    let code = v.pointer("/error/code").and_then(Value::as_str).unwrap_or("");
    if code == "insufficient_quota" {
        return TtsError { status: Some(status), ..TtsError::new(ReadErrorCode::Quota, "Your OpenAI account has no credit left (insufficient quota).") };
    }
    status_error(NAME, status, message, retry_after)
}

impl SpeechProvider for OpenAiClient {
    fn provider(&self) -> Provider {
        Provider::OpenAi
    }

    fn synthesize(&self, text: &str, rest: &str, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        let body = serde_json::to_vec(&json!({ "model": MODEL, "input": text, "voice": rest, "response_format": "mp3" })).map_err(|_| TtsError::bad_response())?;
        let headers = [self.auth(), ("content-type", "application/json".into())];
        let reply = send(&self.agent, NAME, "POST", &format!("{BASE}/audio/speech"), &headers, Some(&body), SYNTH_TIMEOUT)?;
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        if !(200..300).contains(&reply.status) {
            return Err(error(reply.status, &reply.body, reply.header("retry-after")));
        }
        mp3_result(text, reply.body)
    }

    fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        Ok(VOICES
            .iter()
            .map(|(id, gender, description)| {
                let mut name = id.to_string();
                name[..1].make_ascii_uppercase();
                voice(Provider::OpenAi, id, name, vec![("gender", gender.to_string()), ("description", description.to_string()), ("use_case", "Expressive".into())])
            })
            .collect())
    }

    fn validate(&self) -> Result<usize, TtsError> {
        let reply = send(&self.agent, NAME, "GET", &format!("{BASE}/models/{MODEL}"), &[self.auth()], None, LIST_TIMEOUT)?;
        if !(200..300).contains(&reply.status) {
            return Err(error(reply.status, &reply.body, reply.header("retry-after")));
        }
        Ok(VOICES.len())
    }

    fn summary(&self) -> String {
        self.creds.summary(Provider::OpenAi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_voice_list() {
        let c = OpenAiClient::new(ProviderCredentials { key: "sk-test".into(), ..Default::default() }, None, "t");
        let v = c.list_voices().unwrap();
        assert_eq!(v.len(), VOICES.len());
        assert_eq!(v[3].voice_id, "openai:coral");
        assert_eq!(v[3].name, "Coral");
    }

    #[test]
    fn quota_errors_are_quota() {
        let e = error(429, br#"{"error":{"message":"You exceeded your current quota","code":"insufficient_quota"}}"#, None);
        assert_eq!(e.code, ReadErrorCode::Quota);
        assert_eq!(error(401, br#"{"error":{"message":"bad key"}}"#, None).code, ReadErrorCode::Unauthorized);
    }

    /// `cargo test -p vox-tts openai_live -- --ignored`
    #[test]
    #[ignore]
    fn openai_live_rejects_a_fake_key() {
        let c = OpenAiClient::new(ProviderCredentials { key: "sk-invalid-key-for-tests".into(), ..Default::default() }, None, "Vox-test");
        assert_eq!(c.validate().unwrap_err().code, ReadErrorCode::Unauthorized);
    }
}
