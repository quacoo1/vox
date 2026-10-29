//! Azure AI Speech (REST): `POST https://<region>.tts.speech.microsoft.com/cognitiveservices/v1` with SSML, the
//! resource key in `Ocp-Apim-Subscription-Key`, MP3 at 24 kHz / 48 kbps. Voices: `GET …/cognitiveservices/voices/list`
//! (neural voices only). The REST API has no word timing, so highlighting is spread evenly over the audio.

use serde_json::Value;
use vox_core::providers::{Provider, ProviderCredentials};
use vox_core::voices::Voice;

use crate::cancel::CancelToken;
use crate::client::{ProxyConfig, SynthesisResult};
use crate::error::TtsError;
use crate::providers::{build_agent, mp3_result, send, status_error, voice, xml_escape, SpeechProvider, LIST_TIMEOUT, SYNTH_TIMEOUT};

const NAME: &str = "Azure AI Speech";
const OUTPUT_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";

pub struct AzureClient {
    creds: ProviderCredentials,
    agent: ureq::Agent,
    base: String,
}

impl AzureClient {
    pub fn new(creds: ProviderCredentials, proxy: Option<&ProxyConfig>, user_agent: &str) -> Self {
        let base = format!("https://{}.tts.speech.microsoft.com/cognitiveservices", creds.region);
        Self { agent: build_agent(user_agent, proxy), creds, base }
    }

    fn key_header(&self) -> (&'static str, String) {
        ("ocp-apim-subscription-key", self.creds.key.clone())
    }
}

/// `en-US-AvaNeural` → `en-US`.
fn locale_of(short_name: &str) -> String {
    short_name.splitn(3, '-').take(2).collect::<Vec<_>>().join("-")
}

/// The SSML body for one chunk.
pub(crate) fn ssml(text: &str, short_name: &str) -> String {
    format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='{}'><voice name='{}'>{}</voice></speak>",
        xml_escape(&locale_of(short_name)),
        xml_escape(short_name),
        xml_escape(text)
    )
}

fn map_voice(v: &Value) -> Option<Voice> {
    let short = v.get("ShortName")?.as_str()?;
    if v.get("VoiceType").and_then(Value::as_str).is_some_and(|t| !t.eq_ignore_ascii_case("neural")) {
        return None;
    }
    let display = v.get("DisplayName").and_then(Value::as_str).unwrap_or(short);
    let name = if short.contains("Multilingual") && !display.contains("Multilingual") { format!("{display} (Multilingual)") } else { display.to_string() };
    let gender = v.get("Gender").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let locale_name = v.get("LocaleName").and_then(Value::as_str).unwrap_or("").to_string();
    let locale = v.get("Locale").and_then(Value::as_str).unwrap_or("").to_string();
    let tier = if short.contains(":DragonHD") || short.contains("HD") { "HD" } else { "Neural" };
    Some(voice(Provider::Azure, short, name, vec![("gender", gender), ("accent", locale_name), ("language", locale), ("use_case", tier.to_string())]))
}

impl SpeechProvider for AzureClient {
    fn provider(&self) -> Provider {
        Provider::Azure
    }

    fn synthesize(&self, text: &str, rest: &str, _word_timing: bool, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        let body = ssml(text, rest);
        let headers = [self.key_header(), ("content-type", "application/ssml+xml".into()), ("x-microsoft-outputformat", OUTPUT_FORMAT.into())];
        let reply = send(&self.agent, NAME, "POST", &format!("{}/v1", self.base), &headers, Some(body.as_bytes()), SYNTH_TIMEOUT)?;
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        if !(200..300).contains(&reply.status) {
            let detail = String::from_utf8_lossy(&reply.body).to_string();
            return Err(status_error(NAME, reply.status, Some(&detail), reply.header("retry-after")));
        }
        mp3_result(text, reply.body)
    }

    fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        let reply = send(&self.agent, NAME, "GET", &format!("{}/voices/list", self.base), &[self.key_header()], None, LIST_TIMEOUT)?;
        if !(200..300).contains(&reply.status) {
            return Err(status_error(NAME, reply.status, None, reply.header("retry-after")));
        }
        let data: Value = serde_json::from_slice(&reply.body).map_err(|_| TtsError::bad_response())?;
        let mut voices: Vec<Voice> = data.as_array().map(|a| a.iter().filter_map(map_voice).collect()).unwrap_or_default();
        voices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.voice_id.cmp(&b.voice_id)));
        Ok(voices)
    }

    fn summary(&self) -> String {
        self.creds.summary(Provider::Azure)
    }

    fn region(&self) -> String {
        self.creds.region.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ssml_names_the_voice_and_escapes_text() {
        let s = ssml("Tom & Jerry <3", "en-GB-SoniaNeural");
        assert!(s.contains("xml:lang='en-GB'") && s.contains("name='en-GB-SoniaNeural'") && s.contains("Tom &amp; Jerry &lt;3"));
    }

    #[test]
    fn voices_map_neural_only() {
        let v = json!({"ShortName":"en-US-AvaMultilingualNeural","DisplayName":"Ava","Gender":"Female","Locale":"en-US","LocaleName":"English (United States)","VoiceType":"Neural"});
        let m = map_voice(&v).unwrap();
        assert_eq!(m.voice_id, "azure:en-US-AvaMultilingualNeural");
        assert_eq!(m.name, "Ava (Multilingual)");
        assert_eq!(m.category.as_deref(), Some("Azure AI Speech"));
        assert!(map_voice(&json!({"ShortName":"x","VoiceType":"Standard"})).is_none());
    }

    /// `cargo test -p vox-tts azure_live -- --ignored`
    #[test]
    #[ignore]
    fn azure_live_rejects_a_fake_key() {
        let c = AzureClient::new(ProviderCredentials { key: "0".repeat(32), secret: String::new(), region: "eastus".into() }, None, "Vox-test");
        let e = c.list_voices().unwrap_err();
        assert_eq!(e.code, vox_core::read::ReadErrorCode::Unauthorized, "{e:?}");
    }
}
