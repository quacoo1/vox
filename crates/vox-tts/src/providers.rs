//! The interface every non-ElevenLabs speech provider implements, plus what they share: a plain HTTPS agent (Windows
//! certificate store, the system proxy), body reading, status-code error mapping and the MP3 duration walk.
//!
//! A provider turns one chunk of text into audio for a voice (`rest` = the part of the voice id after the provider
//! prefix, see `vox_core::providers`) and lists its voices. Providers without word timing return an alignment spread
//! evenly over the real audio duration, so highlighting still moves through the sentence.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use vox_core::providers::{Provider, ProviderCredentials};
use vox_core::read::ReadErrorCode;
use vox_core::text16::utf16_len;
use vox_core::tts::MIME_MPEG;
use vox_core::voices::Voice;

use crate::cancel::CancelToken;
use crate::client::{ProxyConfig, SynthesisResult};
use crate::error::TtsError;
use crate::mock::even_alignment;

/// One connected speech provider.
pub trait SpeechProvider: Send + Sync {
    fn provider(&self) -> Provider;
    /// Synthesizes `text` with the voice `rest` (the voice id without its provider prefix).
    fn synthesize(&self, text: &str, rest: &str, cancel: &CancelToken) -> Result<SynthesisResult, TtsError>;
    /// Every voice, as Vox voices (ids with the provider prefix, category = the provider's display name).
    fn list_voices(&self) -> Result<Vec<Voice>, TtsError>;
    /// Checks the credentials; `Ok(voice count)`.
    fn validate(&self) -> Result<usize, TtsError> {
        self.list_voices().map(|v| v.len())
    }
    /// The one-line status shown when connected.
    fn summary(&self) -> String;
    /// The region id, when the provider has one.
    fn region(&self) -> String {
        String::new()
    }
}

/// Builds the client for `provider` from validated credentials (Windows needs none).
pub fn connect(provider: Provider, creds: ProviderCredentials, proxy: Option<&ProxyConfig>, user_agent: &str) -> Option<Arc<dyn SpeechProvider>> {
    let p: Arc<dyn SpeechProvider> = match provider {
        Provider::Polly => Arc::new(crate::polly::PollyClient::new(creds, proxy, user_agent)),
        Provider::Azure => Arc::new(crate::azure::AzureClient::new(creds, proxy, user_agent)),
        Provider::OpenAi => Arc::new(crate::openai::OpenAiClient::new(creds, proxy, user_agent)),
        Provider::Google => Arc::new(crate::google::GoogleClient::new(creds, proxy, user_agent)),
        Provider::Windows => Arc::new(crate::windows_voices::WindowsVoices::new()),
        Provider::ElevenLabs => return None,
    };
    Some(p)
}

/// The error for a voice whose provider is not connected.
pub fn not_connected(provider: Provider) -> TtsError {
    TtsError::new(ReadErrorCode::NoApiKey, format!("Connect {} in Settings → Voice providers to use this voice.", provider.display_name()))
}

pub(crate) const SYNTH_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const LIST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// A plain HTTPS agent: Windows certificate store, optional CONNECT proxy, no status-as-error.
pub(crate) fn build_agent(user_agent: &str, proxy: Option<&ProxyConfig>) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    let proxy = proxy
        .and_then(|p| ureq::Proxy::new(&p.url).ok())
        .filter(|p| matches!(p.protocol(), ureq::ProxyProtocol::Http | ureq::ProxyProtocol::Https));
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(user_agent)
        .tls_config(tls)
        .proxy(proxy)
        .timeout_connect(Some(Duration::from_secs(10)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// A finished HTTP exchange.
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    /// Selected response headers (lower-case name, value).
    pub headers: Vec<(String, String)>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Sends a request and reads the whole body. `name` is the provider's display name (for messages and logs).
pub(crate) fn send(
    agent: &ureq::Agent,
    name: &str,
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Reply, TtsError> {
    let result = if method == "POST" {
        let mut rb = agent.post(url);
        for (k, v) in headers {
            rb = rb.header(*k, v.as_str());
        }
        rb.config().timeout_global(Some(timeout)).max_redirects(0).build().send(body.unwrap_or(&[]))
    } else {
        let mut rb = agent.get(url);
        for (k, v) in headers {
            rb = rb.header(*k, v.as_str());
        }
        rb.config().timeout_global(Some(timeout)).build().call()
    };
    let mut res = result.map_err(|e| {
        // Never log the URL: some providers carry the key in a query string.
        tracing::warn!(target: "tts", "{name} {method}: transport error: {}", transport_kind(&e));
        match e {
            ureq::Error::Timeout(_) => TtsError::timeout(),
            _ => network(name),
        }
    })?;
    let status = res.status().as_u16();
    let headers = res
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str().starts_with("x-") || matches!(k.as_str(), "retry-after" | "content-type" | "date"))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    let mut body = Vec::new();
    res.body_mut().with_config().limit(MAX_BODY_BYTES).reader().read_to_end(&mut body).map_err(|_| network(name))?;
    Ok(Reply { status, body, headers })
}

fn transport_kind(e: &ureq::Error) -> &'static str {
    match e {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::Io(_) => "io",
        ureq::Error::Tls(_) => "tls",
        ureq::Error::HostNotFound => "host not found",
        _ => "other",
    }
}

pub(crate) fn network(name: &str) -> TtsError {
    TtsError::new(ReadErrorCode::Network, format!("Can’t reach {name}. Check your internet connection."))
}

/// Maps a non-2xx status to a user-facing error. `detail` is the provider's own message, appended for 400s.
pub(crate) fn status_error(name: &str, status: u16, detail: Option<&str>, retry_after: Option<&str>) -> TtsError {
    let (code, message, retryable) = match status {
        401 | 403 => (ReadErrorCode::Unauthorized, format!("{name} refused the request. Check the key in Settings → Voice providers."), false),
        429 => (ReadErrorCode::RateLimit, format!("{name} is busy (too many requests). Try again in a moment."), true),
        402 => (ReadErrorCode::Quota, format!("Your {name} account has no credit left."), false),
        s if s >= 500 => (ReadErrorCode::Unknown, format!("{name} had a problem. Try again in a moment."), true),
        _ => (
            ReadErrorCode::Unknown,
            match detail.map(str::trim).filter(|d| !d.is_empty()) {
                Some(d) => format!("{name} couldn’t read this text ({}).", truncate(d, 160)),
                None => format!("{name} couldn’t read this text."),
            },
            false,
        ),
    };
    tracing::warn!(target: "tts", "{name} HTTP {status}");
    TtsError {
        status: Some(status),
        retryable,
        api_message: detail.map(str::to_string),
        retry_after_sec: retry_after.and_then(|v| v.trim().parse::<f64>().ok()),
        ..TtsError::new(code, message)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// An MP3 synthesis result with evenly spread timing over its real duration.
pub(crate) fn mp3_result(text: &str, audio: Vec<u8>) -> Result<SynthesisResult, TtsError> {
    if audio.is_empty() {
        return Err(TtsError::no_audio());
    }
    let duration = mp3_duration_sec(&audio).unwrap_or_else(|| utf16_len(text) as f64 / 15.0);
    Ok(SynthesisResult { audio: Arc::from(audio), mime: MIME_MPEG.to_string(), alignment: even_alignment(text, duration), request_id: None, character_cost: Some(0.0) })
}

/// A voice for the Voices page.
pub(crate) fn voice(provider: Provider, rest: &str, name: String, labels: Vec<(&str, String)>) -> Voice {
    Voice {
        voice_id: vox_core::providers::voice_id(provider, rest),
        name,
        labels: labels.into_iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.to_string(), v)).collect(),
        category: Some(provider.display_name().to_string()),
        description: None,
        preview_url: None,
    }
}

// ───────────────────────────── MP3 duration ─────────────────────────────

/// Duration of an MPEG audio stream by walking its frame headers (skips an ID3v2 tag). `None` without frames.
pub fn mp3_duration_sec(data: &[u8]) -> Option<f64> {
    let mut i = 0usize;
    if data.len() >= 10 && &data[..3] == b"ID3" {
        let size = (usize::from(data[6] & 0x7f) << 21) | (usize::from(data[7] & 0x7f) << 14) | (usize::from(data[8] & 0x7f) << 7) | usize::from(data[9] & 0x7f);
        i = 10 + size;
    }
    let mut seconds = 0.0;
    let mut frames = 0usize;
    while i + 4 <= data.len() {
        match frame_info(&data[i..i + 4]) {
            Some((len, samples, rate)) if len > 4 => {
                seconds += f64::from(samples) / f64::from(rate);
                frames += 1;
                i += len;
            }
            _ => i += 1,
        }
    }
    (frames > 0).then_some(seconds)
}

/// `(frame length in bytes, samples per frame, sample rate)` of an MPEG Layer III frame header.
pub(crate) fn frame_info(h: &[u8]) -> Option<(usize, u32, u32)> {
    if h[0] != 0xff || h[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x03; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5
    let layer = (h[1] >> 1) & 0x03; // 1 = Layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let bitrate_index = usize::from(h[2] >> 4);
    let rate_index = usize::from((h[2] >> 2) & 0x03);
    let padding = usize::from((h[2] >> 1) & 0x01);
    if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    const RATES: [[u32; 3]; 3] = [[44100, 48000, 32000], [22050, 24000, 16000], [11025, 12000, 8000]];
    let rate = RATES[match version {
        3 => 0,
        2 => 1,
        _ => 2,
    }][rate_index];
    const V1_L3: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2_L3: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let kbps = if mpeg1 { V1_L3[bitrate_index] } else { V2_L3[bitrate_index] };
    let samples: u32 = if mpeg1 { 1152 } else { 576 };
    let len = (samples as usize / 8) * kbps as usize * 1000 / rate as usize + padding;
    Some((len, samples, rate))
}

/// Escapes text for an SSML / XML body.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control characters other than tab/newline are invalid in XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_frames_are_timed() {
        // MPEG-2 Layer III, 24 kHz, 48 kbps: 576 samples, 144 bytes per frame (no padding).
        let header = [0xff, 0xf3, 0x64, 0x00];
        assert_eq!(frame_info(&header), Some((144, 576, 24000)));
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(&header);
            data.extend(std::iter::repeat_n(0u8, 140));
        }
        let d = mp3_duration_sec(&data).unwrap();
        assert!((d - 100.0 * 576.0 / 24000.0).abs() < 1e-9, "{d}");
        assert_eq!(mp3_duration_sec(b"not audio"), None);
    }

    #[test]
    fn status_errors_map_to_codes() {
        assert_eq!(status_error("X", 401, None, None).code, ReadErrorCode::Unauthorized);
        let busy = status_error("X", 429, None, Some("3"));
        assert!(busy.retryable && busy.retry_after_sec == Some(3.0));
        assert!(status_error("X", 503, None, None).retryable);
        assert!(status_error("X", 400, Some("bad voice"), None).message.contains("bad voice"));
    }

    #[test]
    fn xml_is_escaped() {
        assert_eq!(xml_escape("a<b & \"c\"\u{1}"), "a&lt;b &amp; &quot;c&quot; ");
    }
}
