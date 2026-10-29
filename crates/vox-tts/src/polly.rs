//! Amazon Polly (`polly.<region>.amazonaws.com`), the second speech provider.
//!
//! - Synthesis: `POST /v1/speech` twice per chunk, run in parallel: once for MP3 audio (24 kHz) and once for word
//!   speech marks (`OutputFormat: json`, `SpeechMarkTypes: ["word"]`). The marks become the per-UTF-16-unit
//!   [`Alignment`] the player highlights from; when the marks request fails the chunk still plays, with evenly spread
//!   timing. AWS bills both requests, so the marks are skipped (evenly spread timing, half the cost) when nothing
//!   highlights: word highlighting off, or a voice sample. The last aligned end is the MP3's real duration (frame walk), so the engine's duration rule
//!   (`chunk_duration`: alignment end first) stays exact.
//! - Voices: `GET /v1/voices` (paginated). Each voice is offered once per useful engine: `neural` (or `standard`
//!   when there is no neural version) plus `generative` when available; ids are `polly:<engine>:<VoiceId>`.
//! - Auth: AWS Signature Version 4 with the user's access key ([`sign`]); credentials never reach logs.
//! - Polly characters are billed by AWS, not ElevenLabs: results carry `character_cost = Some(0)` so they never count
//!   against the ElevenLabs credits shown in the app.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use vox_core::providers::{parse_polly_rest, polly_voice_id, PollyVoiceRef, Provider, ProviderCredentials};
use vox_core::read::ReadErrorCode;
use vox_core::text16::utf16_len;
use vox_core::tts::{Alignment, MIME_MPEG};
use vox_core::voices::Voice;

use crate::cancel::CancelToken;
use crate::client::{ProxyConfig, SynthesisResult};
use crate::providers::{build_agent, mp3_duration_sec, send, SpeechProvider, LIST_TIMEOUT, SYNTH_TIMEOUT};
use crate::error::TtsError;
use crate::mock::even_alignment;

type HmacSha256 = Hmac<Sha256>;

const SERVICE: &str = "polly";
const SAMPLE_RATE: &str = "24000";

/// Credentials as entered are wrong or not allowed to use Polly.
const MSG_UNAUTHORIZED: &str = "Amazon Polly refused the request. Check the access key, secret key and region in Settings → Voice providers.";
const MSG_CLOCK: &str = "Your PC’s clock is wrong, so AWS rejects the request. Turn on “Set time automatically” in Windows Settings → Time & language → Date & time.";
const MSG_SECRET: &str = "AWS says the secret access key doesn’t match this access key ID. Paste the secret key again (AWS shows it only once, when the key is created).";
const MSG_DENIED: &str = "This AWS key is not allowed to use Amazon Polly. Give it the AmazonPollyReadOnlyAccess permission (or polly:SynthesizeSpeech and polly:DescribeVoices).";
const MSG_THROTTLED: &str = "Amazon Polly is busy (too many requests). Try again in a moment.";
const MSG_TOO_LONG: &str = "This text is too long for one Amazon Polly request.";
const MSG_GENERIC: &str = "Amazon Polly couldn’t read this text.";

/// A Polly client for one set of credentials (cheap to clone).
#[derive(Clone)]
pub struct PollyClient {
    inner: Arc<Inner>,
}

struct Inner {
    creds: ProviderCredentials,
    agent: ureq::Agent,
    /// Overridable for tests (`http://127.0.0.1:<port>`); production is `https://polly.<region>.amazonaws.com`.
    endpoint: String,
    /// AWS's clock minus ours, in ms (learned from a "signature expired" answer, like the AWS SDKs' clock-skew
    /// correction), so a PC whose clock is off still signs requests AWS accepts.
    skew_ms: AtomicI64,
}

impl std::fmt::Debug for PollyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollyClient").field("creds", &self.inner.creds).finish_non_exhaustive()
    }
}

impl PollyClient {
    pub fn new(creds: ProviderCredentials, proxy: Option<&ProxyConfig>, user_agent: &str) -> Self {
        let endpoint = format!("https://polly.{}.amazonaws.com", creds.region);
        Self::with_endpoint(creds, proxy, user_agent, endpoint)
    }

    /// [`PollyClient::new`] against another endpoint (tests).
    pub fn with_endpoint(creds: ProviderCredentials, proxy: Option<&ProxyConfig>, user_agent: &str, endpoint: String) -> Self {
        Self { inner: Arc::new(Inner { creds, agent: build_agent(user_agent, proxy), endpoint, skew_ms: AtomicI64::new(0) }) }
    }

    pub fn credentials(&self) -> &ProviderCredentials {
        &self.inner.creds
    }

    /// Synthesizes `text` with `voice`: MP3 audio, plus word timing from speech marks when `word_timing`.
    pub fn synthesize_ref(&self, text: &str, voice: &PollyVoiceRef, word_timing: bool, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
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
        let (audio, marks) = if word_timing {
            std::thread::scope(|scope| {
                let marks = scope.spawn(|| self.post_speech(&marks_body));
                let audio = self.post_speech(&audio_body);
                (audio, Some(marks.join().unwrap_or_else(|_| Err(TtsError::new(ReadErrorCode::Unknown, MSG_GENERIC)))))
            })
        } else {
            (self.post_speech(&audio_body), None)
        };
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        let audio = audio?;
        if audio.is_empty() {
            return Err(TtsError::no_audio());
        }
        let duration = mp3_duration_sec(&audio).unwrap_or_else(|| utf16_len(text) as f64 / 15.0);
        let alignment = match marks {
            None => None,
            Some(Ok(raw)) => alignment_from_marks(text, &parse_word_marks(&raw), duration),
            Some(Err(e)) => {
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

    fn post_speech(&self, body: &Value) -> Result<Vec<u8>, TtsError> {
        let bytes = serde_json::to_vec(body).map_err(|_| TtsError::bad_response())?;
        self.request("POST", "/v1/speech", &[], &bytes, SYNTH_TIMEOUT)
    }

    /// One signed request; the body on 2xx, a mapped error otherwise. A clock-skew rejection is retried once with
    /// the time AWS reported.
    fn request(&self, method: &str, path: &str, query: &[(String, String)], body: &[u8], timeout: Duration) -> Result<Vec<u8>, TtsError> {
        let host = self.inner.endpoint.split_once("://").map_or(self.inner.endpoint.as_str(), |(_, h)| h).to_string();
        let content_type = if method == "POST" { Some("application/json") } else { None };
        let qs = canonical_query(query);
        let url = if qs.is_empty() { format!("{}{path}", self.inner.endpoint) } else { format!("{}{path}?{qs}", self.inner.endpoint) };
        for attempt in 0..2 {
            let skew = self.inner.skew_ms.load(Ordering::Relaxed);
            let signed = sign(&SignInput {
                method,
                host: &host,
                path,
                query,
                content_type,
                payload: body,
                creds: &self.inner.creds,
                service: SERVICE,
                now: Utc::now() + chrono::Duration::milliseconds(skew),
            });
            let mut headers = vec![("x-amz-date", signed.amz_date.clone()), ("authorization", signed.authorization.clone())];
            if let Some(ct) = content_type {
                headers.push(("content-type", ct.to_string()));
            }
            let reply = send(&self.inner.agent, "Amazon Polly", method, &url, &headers, (method == "POST").then_some(body), timeout)?;
            if (200..300).contains(&reply.status) {
                return Ok(reply.body);
            }
            let error = map_error(reply.status, reply.header("x-amzn-errortype"), &reply.body);
            if attempt == 0 && is_clock_error(&error) {
                if let Some(server) = reply.header("date").and_then(|d| DateTime::parse_from_rfc2822(d).ok()) {
                    let offset = (server.with_timezone(&Utc) - Utc::now()).num_milliseconds();
                    if (offset - skew).abs() > 60_000 {
                        tracing::warn!(target: "tts", "the PC clock is {}s off AWS; correcting and retrying", offset / 1000);
                        self.inner.skew_ms.store(offset, Ordering::Relaxed);
                        continue;
                    }
                }
            }
            return Err(error);
        }
        Err(TtsError::new(ReadErrorCode::Unauthorized, MSG_CLOCK))
    }
}

/// AWS rejected the request because our timestamp is too far from its clock.
fn is_clock_error(e: &TtsError) -> bool {
    let msg = e.api_message.as_deref().unwrap_or("").to_ascii_lowercase();
    e.api_code.as_deref() == Some("RequestTimeTooSkewed")
        || msg.contains("signature expired")
        || msg.contains("signature not yet current")
        || msg.contains("request time")
}

/// Maps a Polly error response to a user-facing [`TtsError`].
fn map_error(status: u16, error_type: Option<&str>, raw: &[u8]) -> TtsError {
    let body: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
    let api_message = body.get("message").or_else(|| body.get("Message")).and_then(Value::as_str).map(str::to_string);
    let kind = error_type
        .or_else(|| body.get("__type").and_then(Value::as_str))
        .map(|t| t.split(':').next().unwrap_or(t).rsplit('#').next().unwrap_or(t).to_string())
        .unwrap_or_default();
    let lower = api_message.as_deref().unwrap_or("").to_ascii_lowercase();
    let clock = kind == "RequestTimeTooSkewed" || lower.contains("signature expired") || lower.contains("signature not yet current");
    let (code, message, retryable) = match (kind.as_str(), status) {
        _ if clock => (ReadErrorCode::Unauthorized, MSG_CLOCK, false),
        ("InvalidSignatureException", _) => (ReadErrorCode::Unauthorized, MSG_SECRET, false),
        ("UnrecognizedClientException" | "IncompleteSignatureException" | "MissingAuthenticationTokenException" | "ExpiredTokenException", _) => {
            (ReadErrorCode::Unauthorized, MSG_UNAUTHORIZED, false)
        }
        ("AccessDeniedException", _) => (ReadErrorCode::Unauthorized, MSG_DENIED, false),
        ("ThrottlingException", _) | (_, 429) => (ReadErrorCode::RateLimit, MSG_THROTTLED, true),
        ("TextLengthExceededException", _) => (ReadErrorCode::TooLong, MSG_TOO_LONG, false),
        (_, 401 | 403) => (ReadErrorCode::Unauthorized, MSG_UNAUTHORIZED, false),
        (_, s) if s >= 500 => (ReadErrorCode::Unknown, MSG_GENERIC, true),
        _ => (ReadErrorCode::Unknown, MSG_GENERIC, false),
    };
    // AWS's messages never contain the secret (at most the canonical request and the time), so they are safe to log.
    tracing::warn!(target: "tts", "polly HTTP {status} {kind}: {}", api_message.as_deref().unwrap_or(""));
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

impl SpeechProvider for PollyClient {
    fn provider(&self) -> Provider {
        Provider::Polly
    }
    fn synthesize(&self, text: &str, rest: &str, word_timing: bool, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        let voice = parse_polly_rest(rest).ok_or_else(|| TtsError::new(ReadErrorCode::Unknown, MSG_GENERIC))?;
        self.synthesize_ref(text, &voice, word_timing, cancel)
    }
    fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        PollyClient::list_voices(self)
    }
    fn summary(&self) -> String {
        self.inner.creds.summary(Provider::Polly)
    }
    fn region(&self) -> String {
        self.inner.creds.region.clone()
    }
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

// ───────────────────────────── SigV4 ─────────────────────────────

/// What [`sign`] needs.
pub struct SignInput<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub content_type: Option<&'a str>,
    pub payload: &'a [u8],
    pub creds: &'a ProviderCredentials,
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
    let key = signing_key(&input.creds.secret, date, &input.creds.region, input.service);
    let signature = hex::encode(hmac(&key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        input.creds.key
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

    fn creds(secret: &str, region: &str) -> ProviderCredentials {
        ProviderCredentials { key: "AKIDEXAMPLE".into(), secret: secret.into(), region: region.into() }
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
        let c = ProviderCredentials { key: "AKIAIOSFODNN7EXAMPLE".into(), secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), region: "us-east-1".into() };
        let client = PollyClient::new(c, None, "Vox-test");
        let e = PollyClient::list_voices(&client).unwrap_err();
        assert_eq!(e.code, ReadErrorCode::Unauthorized, "{e:?}");
        let voice = PollyVoiceRef { engine: "neural".into(), voice: "Joanna".into() };
        let e = client.synthesize_ref("Hello", &voice, true, &CancelToken::new()).unwrap_err();
        assert_eq!(e.code, ReadErrorCode::Unauthorized, "{e:?}");
    }

    /// Speech marks cost a second billed request, so they are only fetched when something highlights from them.
    #[test]
    fn speech_marks_only_with_word_timing() {
        use crate::client::test_support::{MockServer, Reply};
        let server = MockServer::start(|req, _| {
            if req.json()["OutputFormat"] == "json" {
                Reply::raw(200, r#"{"time":0,"type":"word","start":0,"end":5,"value":"Hello"}"#)
            } else {
                Reply::raw(200, "not really mp3")
            }
        });
        let client = PollyClient::with_endpoint(creds("secret", "us-east-1"), None, "Vox-test", server.base_url.clone());
        let voice = PollyVoiceRef { engine: "neural".into(), voice: "Joanna".into() };

        let r = client.synthesize_ref("Hello there", &voice, false, &CancelToken::new()).unwrap();
        let formats: Vec<String> = server.seen().iter().map(|s| s.json()["OutputFormat"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(formats, ["mp3"]);
        assert_eq!(r.alignment, even_alignment("Hello there", utf16_len("Hello there") as f64 / 15.0));

        let r = client.synthesize_ref("Hello there", &voice, true, &CancelToken::new()).unwrap();
        let mut formats: Vec<String> = server.seen().iter().map(|s| s.json()["OutputFormat"].as_str().unwrap_or("").to_string()).collect();
        formats.sort();
        assert_eq!(formats, ["json", "mp3", "mp3"]);
        assert_ne!(r.alignment, even_alignment("Hello there", utf16_len("Hello there") as f64 / 15.0));
    }

    #[test]
    fn clock_and_secret_problems_get_their_own_messages() {
        let expired = map_error(403, Some("InvalidSignatureException"), br#"{"message":"Signature expired: 20261025T001700Z is now earlier than 20261025T090212Z (20261025T090712Z - 5 min.)"}"#);
        assert!(is_clock_error(&expired));
        assert_eq!(expired.message, MSG_CLOCK);
        let mismatch = map_error(403, Some("InvalidSignatureException"), br#"{"message":"The request signature we calculated does not match the signature you provided."}"#);
        assert!(!is_clock_error(&mismatch));
        assert_eq!(mismatch.message, MSG_SECRET);
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
