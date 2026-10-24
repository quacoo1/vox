//! Amazon Polly (`polly.<region>.amazonaws.com`), the second speech provider.
//!
//! - Synthesis: `POST /v1/speech` twice per chunk, run in parallel: once for MP3 audio (24 kHz) and once for word
//!   speech marks (`OutputFormat: json`, `SpeechMarkTypes: ["word"]`). The marks become the per-UTF-16-unit
//!   [`Alignment`] the player highlights from; when the marks request fails the chunk still plays, with evenly spread
//!   timing. The last aligned end is the MP3's real duration (frame walk), so the engine's duration rule
//!   (`chunk_duration`: alignment end first) stays exact.
//! - Voices: `GET /v1/voices` (paginated). Each voice is offered once per useful engine: `neural` (or `standard`
//!   when there is no neural version) plus `generative` when available; ids are `polly:<engine>:<VoiceId>`.
//! - Auth: AWS Signature Version 4 with the user's access key ([`sign`]); credentials never reach logs.
//! - Polly characters are billed by AWS, not ElevenLabs: results carry `character_cost = Some(0)` so they never count
//!   against the ElevenLabs credits shown in the app.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use vox_core::providers::{polly_voice_id, PollyCredentials, PollyVoiceRef, Provider};
use vox_core::read::ReadErrorCode;
use vox_core::text16::utf16_len;
use vox_core::tts::{Alignment, MIME_MPEG};
use vox_core::voices::Voice;

use crate::cancel::CancelToken;
use crate::client::{ProxyConfig, SynthesisResult};
use crate::error::TtsError;
use crate::mock::even_alignment;

type HmacSha256 = Hmac<Sha256>;

const SERVICE: &str = "polly";
const SAMPLE_RATE: &str = "24000";
const SYNTH_TIMEOUT: Duration = Duration::from_secs(45);
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// Credentials as entered are wrong or not allowed to use Polly.
const MSG_UNAUTHORIZED: &str = "Amazon Polly refused the request. Check the access key, secret key and region in Settings → Voice providers.";
const MSG_DENIED: &str = "This AWS key is not allowed to use Amazon Polly. Give it the AmazonPollyReadOnlyAccess permission (or polly:SynthesizeSpeech and polly:DescribeVoices).";
const MSG_THROTTLED: &str = "Amazon Polly is busy (too many requests). Try again in a moment.";
const MSG_TOO_LONG: &str = "This text is too long for one Amazon Polly request.";
const MSG_NETWORK: &str = "Can’t reach Amazon Polly. Check your internet connection.";
const MSG_GENERIC: &str = "Amazon Polly couldn’t read this text.";
const MSG_NOT_CONNECTED: &str = "Connect Amazon Polly in Settings → Voice providers to use this voice.";

/// A Polly client for one set of credentials (cheap to clone).
#[derive(Clone)]
pub struct PollyClient {
    inner: Arc<Inner>,
}

struct Inner {
    creds: PollyCredentials,
    agent: ureq::Agent,
    /// Overridable for tests (`http://127.0.0.1:<port>`); production is `https://polly.<region>.amazonaws.com`.
    endpoint: String,
}

impl std::fmt::Debug for PollyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollyClient").field("creds", &self.inner.creds).finish_non_exhaustive()
    }
}

/// The error for a Polly voice when Polly is not connected.
pub fn not_connected() -> TtsError {
    TtsError::new(ReadErrorCode::NoApiKey, MSG_NOT_CONNECTED)
}

impl PollyClient {
    pub fn new(creds: PollyCredentials, proxy: Option<&ProxyConfig>, user_agent: &str) -> Self {
        let endpoint = format!("https://polly.{}.amazonaws.com", creds.region);
        Self::with_endpoint(creds, proxy, user_agent, endpoint)
    }

    /// [`PollyClient::new`] against another endpoint (tests).
    pub fn with_endpoint(creds: PollyCredentials, proxy: Option<&ProxyConfig>, user_agent: &str, endpoint: String) -> Self {
        Self { inner: Arc::new(Inner { creds, agent: build_agent(user_agent, proxy), endpoint }) }
    }

    pub fn credentials(&self) -> &PollyCredentials {
        &self.inner.creds
    }

    /// Synthesizes `text` with `voice`: MP3 audio plus word timing.
    pub fn synthesize(&self, text: &str, voice: &PollyVoiceRef, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        let audio_body = json!({
            "Engine": voice.engine, "OutputFormat": "mp3", "SampleRate": SAMPLE_RATE,
            "Text": text, "TextType": "text", "VoiceId": voice.voice,
        });
        let marks_body = json!({
            "Engine": voice.engine, "OutputFormat": "json", "SpeechMarkTypes": ["word"],
            "Text": text, "TextType": "text", "VoiceId": voice.voice,
        });
        let (audio, marks) = std::thread::scope(|scope| {
            let marks = scope.spawn(|| self.post_speech(&marks_body));
            let audio = self.post_speech(&audio_body);
            (audio, marks.join().unwrap_or_else(|_| Err(TtsError::new(ReadErrorCode::Unknown, MSG_GENERIC))))
        });
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        let audio = audio?;
        if audio.is_empty() {
            return Err(TtsError::no_audio());
        }
        let duration = mp3_duration_sec(&audio).unwrap_or_else(|| utf16_len(text) as f64 / 15.0);
        let alignment = match marks {
            Ok(raw) => alignment_from_marks(text, &parse_word_marks(&raw), duration),
            Err(e) => {
                tracing::warn!(target: "tts", "polly speech marks failed ({}); spreading the timing evenly", e.code.as_str());
                None
            }
        }
        .or_else(|| even_alignment(text, duration));
        Ok(SynthesisResult { audio: Arc::from(audio), mime: MIME_MPEG.to_string(), alignment, request_id: None, character_cost: Some(0.0) })
    }

    /// Every Polly voice, as Vox voices (one per offered engine), sorted by name.
    pub fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _page in 0..20 {
            let mut query = vec![("IncludeAdditionalLanguageCodes".to_string(), "false".to_string())];
            if let Some(t) = &token {
                query.push(("NextToken".to_string(), t.clone()));
            }
            let raw = self.request("GET", "/v1/voices", &query, &[], LIST_TIMEOUT)?;
            let data: Value = serde_json::from_slice(&raw).map_err(|_| TtsError::bad_response())?;
            if let Some(list) = data.get("Voices").and_then(Value::as_array) {
                for v in list {
                    out.extend(map_voice(v));
                }
            }
            token = data.get("NextToken").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
            if token.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.voice_id.cmp(&b.voice_id)));
        Ok(out)
    }

    /// Checks the credentials (lists voices); `Ok(voice count)`.
    pub fn validate(&self) -> Result<usize, TtsError> {
        self.list_voices().map(|v| v.len())
    }

    fn post_speech(&self, body: &Value) -> Result<Vec<u8>, TtsError> {
        let bytes = serde_json::to_vec(body).map_err(|_| TtsError::bad_response())?;
        self.request("POST", "/v1/speech", &[], &bytes, SYNTH_TIMEOUT)
    }

    /// One signed request; the body on 2xx, a mapped error otherwise.
    fn request(&self, method: &str, path: &str, query: &[(String, String)], body: &[u8], timeout: Duration) -> Result<Vec<u8>, TtsError> {
        let host = self.inner.endpoint.split_once("://").map_or(self.inner.endpoint.as_str(), |(_, h)| h).to_string();
        let content_type = if method == "POST" { Some("application/json") } else { None };
        let signed = sign(&SignInput {
            method,
            host: &host,
            path,
            query,
            content_type,
            payload: body,
            creds: &self.inner.creds,
            service: SERVICE,
            now: Utc::now(),
        });
        let qs = canonical_query(query);
        let url = if qs.is_empty() { format!("{}{path}", self.inner.endpoint) } else { format!("{}{path}?{qs}", self.inner.endpoint) };
        let result = if method == "POST" {
            let mut rb = self.inner.agent.post(&url).header("x-amz-date", &signed.amz_date).header("authorization", &signed.authorization);
            if let Some(ct) = content_type {
                rb = rb.header("content-type", ct);
            }
            rb.config().timeout_global(Some(timeout)).max_redirects(0).build().send(body)
        } else {
            self.inner
                .agent
                .get(&url)
                .header("x-amz-date", &signed.amz_date)
                .header("authorization", &signed.authorization)
                .config()
                .timeout_global(Some(timeout))
                .build()
                .call()
        };
        let mut res = result.map_err(|e| {
            tracing::warn!(target: "tts", "polly {method} {path}: transport error: {e}");
            match e {
                ureq::Error::Timeout(_) => TtsError::timeout(),
                _ => TtsError::new(ReadErrorCode::Network, MSG_NETWORK),
            }
        })?;
        let status = res.status().as_u16();
        let error_type = res.headers().get("x-amzn-errortype").and_then(|v| v.to_str().ok()).map(str::to_string);
        let mut raw = Vec::new();
        res.body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .reader()
            .read_to_end(&mut raw)
            .map_err(|_| TtsError::new(ReadErrorCode::Network, MSG_NETWORK))?;
        if (200..300).contains(&status) {
            return Ok(raw);
        }
        Err(map_error(status, error_type.as_deref(), &raw))
    }
}

fn build_agent(user_agent: &str, proxy: Option<&ProxyConfig>) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    let proxy = proxy.and_then(|p| ureq::Proxy::new(&p.url).ok()).filter(|p| matches!(p.protocol(), ureq::ProxyProtocol::Http | ureq::ProxyProtocol::Https));
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(user_agent)
        .tls_config(tls)
        .proxy(proxy)
        .timeout_connect(Some(Duration::from_secs(10)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// Maps a Polly error response to a user-facing [`TtsError`].
fn map_error(status: u16, error_type: Option<&str>, raw: &[u8]) -> TtsError {
    let body: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
    let api_message = body.get("message").or_else(|| body.get("Message")).and_then(Value::as_str).map(str::to_string);
    let kind = error_type
        .or_else(|| body.get("__type").and_then(Value::as_str))
        .map(|t| t.split(':').next().unwrap_or(t).rsplit('#').next().unwrap_or(t).to_string())
        .unwrap_or_default();
    let (code, message, retryable) = match (kind.as_str(), status) {
        ("UnrecognizedClientException" | "InvalidSignatureException" | "IncompleteSignatureException" | "MissingAuthenticationTokenException" | "ExpiredTokenException", _) => {
            (ReadErrorCode::Unauthorized, MSG_UNAUTHORIZED, false)
        }
        ("AccessDeniedException", _) => (ReadErrorCode::Unauthorized, MSG_DENIED, false),
        ("ThrottlingException", _) | (_, 429) => (ReadErrorCode::RateLimit, MSG_THROTTLED, true),
        ("TextLengthExceededException", _) => (ReadErrorCode::TooLong, MSG_TOO_LONG, false),
        (_, 401 | 403) => (ReadErrorCode::Unauthorized, MSG_UNAUTHORIZED, false),
        (_, s) if s >= 500 => (ReadErrorCode::Unknown, MSG_GENERIC, true),
        _ => (ReadErrorCode::Unknown, MSG_GENERIC, false),
    };
    tracing::warn!(target: "tts", "polly HTTP {status} {kind}");
    let message = match (code, &api_message) {
        // Polly's own wording is the most useful detail for voice/engine/language problems.
        (ReadErrorCode::Unknown, Some(m)) if status == 400 => format!("{MSG_GENERIC} ({m})"),
        _ => message.to_string(),
    };
    TtsError { status: Some(status), api_code: Some(kind).filter(|k| !k.is_empty()), api_message, retryable, ..TtsError::new(code, message) }
}

/// One Polly `DescribeVoices` entry → Vox voices (one per offered engine).
fn map_voice(v: &Value) -> Vec<Voice> {
    let Some(id) = v.get("Id").and_then(Value::as_str) else { return vec![] };
    let name = v.get("Name").and_then(Value::as_str).unwrap_or(id);
    let engines: Vec<&str> = v.get("SupportedEngines").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut offered: Vec<&str> = Vec::new();
    if engines.contains(&"neural") {
        offered.push("neural");
    } else if engines.contains(&"standard") || engines.is_empty() {
        offered.push("standard");
    }
    if engines.contains(&"generative") {
        offered.push("generative");
    }
    let gender = v.get("Gender").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let language = v.get("LanguageName").and_then(Value::as_str).unwrap_or("");
    let language_code = v.get("LanguageCode").and_then(Value::as_str).unwrap_or("");
    offered
        .into_iter()
        .map(|engine| {
            let engine_label = match engine {
                "neural" => "Neural",
                "generative" => "Generative",
                "long-form" => "Long-form",
                _ => "Standard",
            };
            let display = if engine == "generative" { format!("{name} (Generative)") } else { name.to_string() };
            let mut labels = Vec::new();
            if !gender.is_empty() {
                labels.push(("gender".to_string(), gender.clone()));
            }
            if !language.is_empty() {
                labels.push(("accent".to_string(), language.to_string()));
            }
            if !language_code.is_empty() {
                labels.push(("language".to_string(), language_code.to_string()));
            }
            labels.push(("use_case".to_string(), engine_label.to_string()));
            Voice {
                voice_id: polly_voice_id(engine, id),
                name: display,
                labels,
                category: Some(Provider::Polly.display_name().to_string()),
                description: None,
                preview_url: None,
            }
        })
        .collect()
}

// ───────────────────────────── speech marks → alignment ─────────────────────────────

/// One Polly word mark: start time (ms) and the word's UTF-8 byte range in the request text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WordMark {
    pub time_ms: f64,
    pub start: usize,
    pub end: usize,
}

/// Parses Polly's newline-delimited JSON speech marks (word marks only).
pub fn parse_word_marks(raw: &[u8]) -> Vec<WordMark> {
    String::from_utf8_lossy(raw)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|m| m.get("type").and_then(Value::as_str) == Some("word"))
        .filter_map(|m| {
            Some(WordMark {
                time_ms: m.get("time")?.as_f64()?,
                start: m.get("start")?.as_u64()? as usize,
                end: m.get("end")?.as_u64()? as usize,
            })
        })
        .collect()
}

/// Builds the per-UTF-16-unit alignment from word marks: each word's units share the word's span from its start
/// time to the next word's start (the last word ends at `duration`); units between words take the previous word's
/// end. `None` without usable marks.
pub fn alignment_from_marks(text: &str, marks: &[WordMark], duration: f64) -> Option<Alignment> {
    if text.is_empty() || marks.is_empty() {
        return None;
    }
    // UTF-8 byte offset → UTF-16 unit index, for every char boundary.
    let mut byte_to_u16 = vec![0usize; text.len() + 1];
    let mut chars: Vec<String> = Vec::new();
    let mut u16_index = 0usize;
    for (b, ch) in text.char_indices() {
        byte_to_u16[b] = u16_index;
        let units = ch.len_utf16();
        chars.push(ch.to_string());
        if units == 2 {
            chars.push(String::new());
        }
        u16_index += units;
        for k in 1..ch.len_utf8() {
            byte_to_u16[b + k] = u16_index;
        }
    }
    byte_to_u16[text.len()] = u16_index;
    let total = u16_index;
    let duration = duration.max(0.0);
    let mut starts = vec![f64::NAN; total];
    let mut ends = vec![f64::NAN; total];
    let mut sorted: Vec<WordMark> = marks.iter().copied().filter(|m| m.start < m.end && m.end <= text.len()).collect();
    sorted.sort_by(|a, b| a.time_ms.partial_cmp(&b.time_ms).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.is_empty() {
        return None;
    }
    for (i, m) in sorted.iter().enumerate() {
        let t0 = (m.time_ms / 1000.0).min(duration);
        let t1 = sorted.get(i + 1).map_or(duration, |n| (n.time_ms / 1000.0).min(duration)).max(t0);
        let (a, b) = (byte_to_u16[m.start], byte_to_u16[m.end]);
        let n = (b - a).max(1) as f64;
        for (k, u) in (a..b).enumerate() {
            starts[u] = round3(t0 + (t1 - t0) * k as f64 / n);
            ends[u] = round3(t0 + (t1 - t0) * (k + 1) as f64 / n);
        }
    }
    // Fill units outside words with the previous end (0 before the first word).
    let mut last = 0.0;
    for u in 0..total {
        if starts[u].is_nan() {
            starts[u] = last;
            ends[u] = last;
        } else {
            last = ends[u];
        }
    }
    if let Some(e) = ends.last_mut() {
        *e = round3(duration).max(*e);
    }
    Some(Alignment { chars, starts, ends })
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
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

/// `(frame length in bytes, samples per frame, sample rate)` of an MPEG audio frame header.
fn frame_info(h: &[u8]) -> Option<(usize, u32, u32)> {
    if h[0] != 0xff || h[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x03; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5
    let layer = (h[1] >> 1) & 0x03; // 1 = Layer III, 2 = II, 3 = I
    if version == 1 || layer == 0 {
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
    // Kbps for Layer III (the only layer Polly emits); Layers I/II are not expected.
    const V1_L3: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const V2_L3: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    if layer != 1 {
        return None;
    }
    let kbps = if mpeg1 { V1_L3[bitrate_index] } else { V2_L3[bitrate_index] };
    let samples: u32 = if mpeg1 { 1152 } else { 576 };
    let len = (samples as usize / 8) * kbps as usize * 1000 / rate as usize + padding;
    Some((len, samples, rate))
}

// ───────────────────────────── SigV4 ─────────────────────────────

/// What [`sign`] needs.
pub struct SignInput<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub content_type: Option<&'a str>,
    pub payload: &'a [u8],
    pub creds: &'a PollyCredentials,
    pub service: &'a str,
    pub now: DateTime<Utc>,
}

/// The headers a signed request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub amz_date: String,
    pub authorization: String,
    pub signature: String,
}

/// AWS Signature Version 4 (headers signed: `content-type` when present, `host`, `x-amz-date`).
pub fn sign(input: &SignInput<'_>) -> Signed {
    let amz_date = input.now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let mut headers: Vec<(&str, String)> = Vec::new();
    if let Some(ct) = input.content_type {
        headers.push(("content-type", ct.to_string()));
    }
    headers.push(("host", input.host.to_string()));
    headers.push(("x-amz-date", amz_date.clone()));
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed_headers = headers.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        uri_encode_path(input.path),
        canonical_query(input.query),
        canonical_headers,
        signed_headers,
        hex::encode(Sha256::digest(input.payload))
    );
    let scope = format!("{date}/{}/{}/aws4_request", input.creds.region, input.service);
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical_request.as_bytes())));
    let key = signing_key(&input.creds.secret_access_key, date, &input.creds.region, input.service);
    let signature = hex::encode(hmac(&key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        input.creds.access_key_id
    );
    Signed { amz_date, authorization, signature }
}

/// The SigV4 signing key.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Sorted, RFC 3986-encoded `k=v&…`.
fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k), uri_encode(v))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

fn uri_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn uri_encode_path(path: &str) -> String {
    path.split('/').map(uri_encode).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(secret: &str, region: &str) -> PollyCredentials {
        PollyCredentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: secret.into(), region: region.into() }
    }

    /// The worked example from the AWS SigV4 documentation (IAM ListUsers, 2015-08-30 12:36:00 UTC).
    #[test]
    fn sigv4_matches_the_aws_documentation_example() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        assert_eq!(
            hex::encode(signing_key(secret, "20150830", "us-east-1", "iam")),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
        let c = creds(secret, "us-east-1");
        let query = vec![("Action".to_string(), "ListUsers".to_string()), ("Version".to_string(), "2010-05-08".to_string())];
        let now = DateTime::parse_from_rfc3339("2015-08-30T12:36:00Z").unwrap().with_timezone(&Utc);
        let signed = sign(&SignInput {
            method: "GET",
            host: "iam.amazonaws.com",
            path: "/",
            query: &query,
            content_type: Some("application/x-www-form-urlencoded; charset=utf-8"),
            payload: b"",
            creds: &c,
            service: "iam",
            now,
        });
        assert_eq!(signed.amz_date, "20150830T123600Z");
        assert_eq!(signed.signature, "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7");
        assert!(signed.authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request"));
    }

    #[test]
    fn query_encoding_is_sorted_and_strict() {
        let q = vec![("b".to_string(), "x y".to_string()), ("a".to_string(), "1/2+3".to_string())];
        assert_eq!(canonical_query(&q), "a=1%2F2%2B3&b=x%20y");
    }

    #[test]
    fn word_marks_become_utf16_alignment() {
        let text = "Hi there, café 😀 ok";
        let raw = r#"{"time":0,"type":"word","start":0,"end":2,"value":"Hi"}
{"time":300,"type":"word","start":3,"end":8,"value":"there"}
{"time":800,"type":"word","start":10,"end":15,"value":"café"}
{"time":1200,"type":"word","start":21,"end":23,"value":"ok"}
{"time":0,"type":"sentence","start":0,"end":23,"value":"x"}"#;
        let marks = parse_word_marks(raw.as_bytes());
        assert_eq!(marks.len(), 4);
        let a = alignment_from_marks(text, &marks, 1.5).unwrap();
        assert!(a.is_consistent());
        assert_eq!(a.chars.len(), utf16_len(text));
        assert_eq!(a.starts[0], 0.0);
        assert_eq!(a.starts[3], 0.3); // "t" of "there"
        assert_eq!(a.starts[10], 0.8); // "c" of "café"
        assert_eq!(*a.ends.last().unwrap(), 1.5);
        // Monotonic, and the emoji's two units are both covered.
        assert!(a.starts.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn no_marks_means_no_alignment() {
        assert!(alignment_from_marks("hello", &[], 1.0).is_none());
        assert!(alignment_from_marks("", &[WordMark { time_ms: 0.0, start: 0, end: 1 }], 1.0).is_none());
    }

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
    fn voices_map_per_engine() {
        let v = json!({"Gender":"Female","Id":"Ruth","LanguageCode":"en-US","LanguageName":"US English","Name":"Ruth","SupportedEngines":["generative","long-form","neural"]});
        let out = map_voice(&v);
        let ids: Vec<&str> = out.iter().map(|v| v.voice_id.as_str()).collect();
        assert_eq!(ids, ["polly:neural:Ruth", "polly:generative:Ruth"]);
        assert_eq!(out[1].name, "Ruth (Generative)");
        assert_eq!(out[0].category.as_deref(), Some("Amazon Polly"));
        assert_eq!(out[0].label("accent"), Some("US English"));
        let std_only = json!({"Gender":"Male","Id":"Brian","LanguageName":"British English","Name":"Brian","SupportedEngines":["standard"]});
        assert_eq!(map_voice(&std_only)[0].voice_id, "polly:standard:Brian");
    }

    /// Live: fake credentials reach AWS and come back as "unauthorized" (endpoint, TLS and request shape are right).
    /// `cargo test -p vox-tts polly_live -- --ignored`
    #[test]
    #[ignore]
    fn polly_live_rejects_fake_credentials() {
        let c = PollyCredentials { access_key_id: "AKIAIOSFODNN7EXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), region: "us-east-1".into() };
        let client = PollyClient::new(c, None, "Vox-test");
        let e = client.list_voices().unwrap_err();
        assert_eq!(e.code, ReadErrorCode::Unauthorized, "{e:?}");
        let voice = PollyVoiceRef { engine: "neural".into(), voice: "Joanna".into() };
        let e = client.synthesize("Hello", &voice, &CancelToken::new()).unwrap_err();
        assert_eq!(e.code, ReadErrorCode::Unauthorized, "{e:?}");
    }

    #[test]
    fn errors_map_to_codes() {
        let e = map_error(403, Some("UnrecognizedClientException:http://internal.amazon.com/coral/"), br#"{"message":"bad"}"#);
        assert_eq!(e.code, ReadErrorCode::Unauthorized);
        let e = map_error(400, Some("ThrottlingException"), b"{}");
        assert!(e.retryable && e.code == ReadErrorCode::RateLimit);
        let e = map_error(400, None, br#"{"__type":"com.amazonaws#InvalidSampleRateException","message":"Invalid sample rate"}"#);
        assert_eq!(e.code, ReadErrorCode::Unknown);
        assert!(e.message.contains("Invalid sample rate"));
        let e = map_error(503, None, b"");
        assert!(e.retryable);
    }
}
