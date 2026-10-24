//! The ElevenLabs error model (tts.md §2.4). Messages are already user-facing (`vox_core::read::eleven_messages`) and
//! never contain the API key.
//!
//! Retry policy (BRIEF.md Product rulings "Do not retry timeouts (they may already be billed); retry only what is
//! safe"; ARCHITECTURE.md §8 D-NET-2). `retryable` is true only when the request certainly was not processed:
//!
//! | Failure | `retryable` | Why |
//! |---|---|---|
//! | HTTP 429 / `rate_limit_error` / concurrency codes / `system_busy` | yes | rejected before synthesis |
//! | HTTP ≥ 500 (not quota/permission/…) | yes | ElevenLabs does not bill failed requests |
//! | transport failure **before the request was sent** (DNS, TCP connect refused/reset/unreachable, the proxy `CONNECT` exchange, TLS handshake reset/EOF, resolve/connect timeout) — [`TtsError::network_unsent`] | yes | nothing reached the server |
//! | TLS handshake **rejected** before sending (certificate refused, protocol alert) — [`TtsError::network`] | no | nothing was sent, but retrying cannot help (a proxied request still gets its one direct retry, D-NET-1) |
//! | timeout — [`TtsError::timeout`] | **no** (was yes) | the server may have synthesized and billed it |
//! | transport failure on an established connection, whatever the OS error (reset or network down while waiting, body read error) — [`TtsError::network`] | **no** (was yes) | may be billed |
//!
//! The client knows the phase from the connector chain itself (`crate::client` module docs), never from the I/O error
//! kind alone.
//! | 2xx with an unreadable body / no audio — [`TtsError::bad_response`], [`TtsError::no_audio`] | **no** (was yes) | a 2xx was billed |
//! | everything else | no | |

use serde_json::Value;
use vox_core::js::is_js_whitespace;
use vox_core::read::{eleven_messages as msg, ReadErrorCode};
use vox_core::text16::prefix16;

use crate::jsval::{js_number, js_string, str_of};

/// Cap on raw (non-JSON) error messages and on the "other HTTP" message (tts.md §2.4.3, §2.4.4 row 10).
const MESSAGE_CAP: usize = vox_core::constants::ERROR_MESSAGE_CAP;

/// `ElevenLabsError`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct TtsError {
    pub code: ReadErrorCode,
    /// User-facing; shown verbatim.
    pub message: String,
    pub status: Option<u16>,
    /// `detail.status ?? detail.code` in original case.
    pub api_code: Option<String>,
    pub api_message: Option<String>,
    pub param: Option<String>,
    pub request_id: Option<String>,
    /// The engine retries only these.
    pub retryable: bool,
    /// The key is valid but lacks the scope for this endpoint.
    pub missing_permission: bool,
    /// Cancelled by the caller: never shown.
    pub aborted: bool,
    pub retry_after_sec: Option<f64>,
}

impl TtsError {
    /// A plain error with a code and message (all flags false).
    pub fn new(code: ReadErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            status: None,
            api_code: None,
            api_message: None,
            param: None,
            request_id: None,
            retryable: false,
            missing_permission: false,
            aborted: false,
            retry_after_sec: None,
        }
    }
    pub fn no_key() -> Self {
        Self::new(ReadErrorCode::NoApiKey, msg::NO_KEY)
    }
    /// 2xx with an unparsable body (not retryable: a 2xx synthesis was billed).
    pub fn bad_response() -> Self {
        Self::new(ReadErrorCode::Unknown, msg::BAD_RESPONSE)
    }
    /// 2xx synthesis without audio (not retryable: billed).
    pub fn no_audio() -> Self {
        Self::new(ReadErrorCode::Unknown, msg::NO_AUDIO)
    }
    pub fn cancelled() -> Self {
        Self { aborted: true, ..Self::new(ReadErrorCode::Unknown, msg::CANCELLED) }
    }
    /// The request timeout fired (not retryable: possibly billed).
    pub fn timeout() -> Self {
        Self::new(ReadErrorCode::Network, msg::TIMEOUT)
    }
    /// Transport failure after the request was (possibly) sent, or a TLS handshake that was rejected before sending
    /// (not retryable).
    pub fn network() -> Self {
        Self::new(ReadErrorCode::Network, msg::NETWORK)
    }
    /// Transport failure before any request byte was sent: DNS, TCP connect refused/reset/unreachable, any failure of
    /// the proxy `CONNECT` exchange, a TLS handshake reset or cut off, a resolve/connect timeout (retryable). A
    /// rejected handshake (certificate, alert) is [`TtsError::network`] instead.
    pub fn network_unsent() -> Self {
        Self { retryable: true, ..Self::new(ReadErrorCode::Network, msg::NETWORK) }
    }
    /// Engine: the text yields no chunks.
    pub fn empty_text() -> Self {
        Self::new(ReadErrorCode::EmptyText, msg::NO_TEXT)
    }
    /// Engine: a cached session with zero chunks.
    pub fn cached_audio_missing() -> Self {
        Self::new(ReadErrorCode::NotCached, msg::CACHED_AUDIO_MISSING)
    }
    /// Engine: unexpected internal failure.
    pub fn crashed() -> Self {
        Self::new(ReadErrorCode::Unknown, msg::ENGINE_CRASHED)
    }
}

/// `parseErrorBody(raw)` result (tts.md §2.4.3). `kind` is the body's `type`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorBody {
    pub kind: Option<String>,
    pub code: Option<String>,
    pub status: Option<String>,
    pub message: Option<String>,
    pub request_id: Option<String>,
    pub param: Option<String>,
}

/// `parseErrorBody` (tts.md §2.4.3): JSON `detail` object / string / FastAPI array, or the first 300 chars of a
/// non-HTML text body.
pub fn parse_error_body(raw: &str) -> ErrorBody {
    let json = match vox_core::json::parse_lenient(raw) {
        Ok(v) => v,
        Err(_) => {
            let text = raw.trim_matches(is_js_whitespace);
            if !text.is_empty() && !text.starts_with('<') {
                return ErrorBody { message: Some(prefix16(text, MESSAGE_CAP).to_string()), ..ErrorBody::default() };
            }
            return ErrorBody::default();
        }
    };
    // `detail` when the body is an object that has the key (even with a null value), else the body itself.
    let detail = match &json {
        Value::Object(m) if m.contains_key("detail") => m.get("detail").unwrap_or(&Value::Null),
        other => other,
    };
    match detail {
        Value::String(s) => ErrorBody { message: Some(s.clone()), ..ErrorBody::default() },
        Value::Array(items) => {
            // FastAPI validation errors: [{ loc: [...], msg, type }]
            let objects: Vec<&serde_json::Map<String, Value>> = items.iter().filter_map(Value::as_object).collect();
            let messages: Vec<&str> = objects.iter().filter_map(|o| str_of(o.get("msg"))).collect();
            let message = if messages.is_empty() { None } else { Some(messages.join("; ")) };
            let param = objects.first().and_then(|o| match o.get("loc") {
                Some(Value::Array(loc)) => Some(loc.iter().map(js_string).collect::<Vec<_>>().join(".")),
                _ => None,
            });
            ErrorBody { kind: Some("validation_error".into()), message, param, ..ErrorBody::default() }
        }
        Value::Object(d) => ErrorBody {
            kind: str_of(d.get("type")).map(str::to_string),
            code: str_of(d.get("code")).map(str::to_string),
            status: str_of(d.get("status")).map(str::to_string),
            message: str_of(d.get("message")).map(str::to_string),
            request_id: str_of(d.get("request_id")).map(str::to_string),
            param: str_of(d.get("param")).map(str::to_string),
        },
        _ => ErrorBody::default(),
    }
}

/// `errorFromResponse(status, raw, headers)` (tts.md §2.4.4): the 10-row first-match classification. `header(name)`
/// returns a header value (case-insensitive; multiple values joined with `", "`).
pub fn error_from_response(status: u16, raw: &str, header: &dyn Fn(&str) -> Option<String>) -> TtsError {
    let body = parse_error_body(raw);
    let ids: Vec<String> = [body.status.as_deref(), body.code.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    let has = |names: &[&str]| names.iter().any(|n| ids.iter().any(|id| id == n));
    let message = body.message.clone();
    let has_message = message.as_deref().is_some_and(|m| !m.is_empty());
    let kind = body.kind.as_deref();

    let base = TtsError {
        status: Some(status),
        api_code: body.status.clone().or_else(|| body.code.clone()),
        api_message: message.clone(),
        param: body.param.clone(),
        request_id: body.request_id.clone().or_else(|| header("request-id")).or_else(|| header("x-trace-id")),
        retry_after_sec: parse_retry_after(header("retry-after").as_deref()),
        ..TtsError::new(ReadErrorCode::Unknown, String::new())
    };
    let with = |code: ReadErrorCode, text: String| TtsError { code, message: text, ..base.clone() };

    // 1. Quota first: ElevenLabs reports `quota_exceeded` with HTTP 401.
    if has(&["quota_exceeded", "insufficient_credits", "payment_required"]) || status == 402 || kind == Some("payment_required") {
        return with(ReadErrorCode::Quota, msg::QUOTA.into());
    }
    // 2. A valid key without the scope for this endpoint.
    if has(&["missing_permissions", "insufficient_permissions"]) || mentions_missing_permission(message.as_deref().unwrap_or("")) {
        return TtsError { missing_permission: true, ..with(ReadErrorCode::Unauthorized, permission_message(message.as_deref())) };
    }
    // 3. Rejected key (or ElevenLabs' own explanation, e.g. "Unusual activity detected…").
    if status == 401 || kind == Some("authentication_error") {
        let generic = has(&INVALID_KEY_CODES) || !has_message;
        let text = if generic { msg::BAD_KEY.to_string() } else { format!("ElevenLabs: {}", message.as_deref().unwrap_or("")) };
        return with(ReadErrorCode::Unauthorized, text);
    }
    // 4. Rate limits (retryable).
    if status == 429
        || kind == Some("rate_limit_error")
        || has(&["too_many_concurrent_requests", "concurrent_limit_exceeded", "rate_limit_exceeded", "system_busy"])
    {
        let text = if has(&["too_many_concurrent_requests", "concurrent_limit_exceeded"]) { msg::CONCURRENCY } else { msg::RATE_LIMIT };
        return TtsError { retryable: true, ..with(ReadErrorCode::RateLimit, text.into()) };
    }
    // 5–7. Voice / model problems.
    if has(&["voice_not_found"]) {
        return with(ReadErrorCode::Unknown, msg::VOICE_MISSING.into());
    }
    if has(&["voice_access_denied"]) {
        return with(ReadErrorCode::Unknown, msg::VOICE_DENIED.into());
    }
    if has(&["model_not_found", "model_access_denied", "unsupported_model"]) {
        return with(ReadErrorCode::Unknown, msg::MODEL.into());
    }
    // 8. One request's text is over the per-request limit (not quota).
    if has(&["text_too_long", "max_character_limit_exceeded"]) {
        return with(ReadErrorCode::TooLong, msg::TOO_LONG.into());
    }
    // 9. Server trouble (retryable: failed requests are not billed).
    if status >= 500 {
        return TtsError { retryable: true, ..with(ReadErrorCode::Unknown, msg::SERVER.into()) };
    }
    // 10. Everything else, HTTP 403 without a recognised code included.
    let fallback = match message.as_deref() {
        Some(m) if !m.is_empty() => format!("ElevenLabs: {m}"),
        _ => format!("ElevenLabs returned an error (HTTP {status})"),
    };
    with(ReadErrorCode::Unknown, prefix16(&fallback, MESSAGE_CAP).to_string())
}

/// `/missing (?:the )?permission/i` (ASCII case folding, like a JS regex without the `u` flag).
fn mentions_missing_permission(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("missing permission") || lower.contains("missing the permission")
}

/// The first capture of `/permission\s+["'`]?([a-z_]+)/i` (JS `\s`, ASCII case folding).
fn permission_scope(message: &str) -> Option<&str> {
    let lower = message.to_ascii_lowercase(); // same byte offsets as `message`
    let mut from = 0;
    while let Some(found) = lower[from..].find("permission") {
        let after = from + found + "permission".len();
        let rest = &message[after..];
        let trimmed = rest.trim_start_matches(is_js_whitespace);
        if trimmed.len() < rest.len() {
            let body = trimmed.strip_prefix(['"', '\'', '`']).unwrap_or(trimmed);
            let end = body.find(|c: char| !(c.is_ascii_alphabetic() || c == '_')).unwrap_or(body.len());
            if end > 0 {
                return Some(&body[..end]);
            }
            // An optional quote that is not followed by a scope: retry without consuming the quote.
            let end = trimmed.find(|c: char| !(c.is_ascii_alphabetic() || c == '_')).unwrap_or(trimmed.len());
            if end > 0 {
                return Some(&trimmed[..end]);
            }
        }
        from = from + found + 1;
    }
    None
}

/// `permissionMessage(apiMessage)` (tts.md §2.4.4).
pub fn permission_message(api_message: Option<&str>) -> String {
    let what = match permission_scope(api_message.unwrap_or("")) {
        Some(scope) => match scope {
            "text_to_speech" => "Text to Speech".to_string(),
            "voices_read" => "Voices (read)".to_string(),
            "user_read" => "User (read)".to_string(),
            "models_read" => "Models (read)".to_string(),
            other => other.replace('_', " "),
        },
        None => "this feature".to_string(),
    };
    msg::permission(&what)
}

/// `retry-after` → seconds when finite and > 0 (JS `Number()` rules: trims, accepts `0x` hex, rejects dates).
pub fn parse_retry_after(v: Option<&str>) -> Option<f64> {
    let n = js_number(v.unwrap_or(""));
    (n.is_finite() && n > 0.0).then_some(n)
}

/// `intHeader` (tts.md §2.5): absent/blank → none; else finite and ≥ 0 (fractions allowed).
pub fn int_header(v: Option<&str>) -> Option<f64> {
    let v = v?;
    if v.trim_matches(is_js_whitespace).is_empty() {
        return None;
    }
    let n = js_number(v);
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// Key-validation `INVALID_KEY_CODES`.
pub const INVALID_KEY_CODES: [&str; 5] =
    ["invalid_api_key", "missing_api_key", "needs_authorization", "invalid_authorization_header", "unauthorized"];

#[cfg(test)]
mod tests {
    use super::*;

    fn no_headers(_: &str) -> Option<String> {
        None
    }

    fn classify(status: u16, raw: &str) -> TtsError {
        error_from_response(status, raw, &no_headers)
    }

    fn classify_with(status: u16, raw: &str, headers: &[(&str, &str)]) -> TtsError {
        let headers: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        error_from_response(status, raw, &move |name: &str| {
            headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
        })
    }

    /// tts.md §2.4.4 "Verified outputs from the original code", every row (messages with D-COPY-1 apostrophes).
    #[test]
    fn verified_classification_table() {
        let e = classify(401, r#"{"detail":{"status":"quota_exceeded","message":"This request exceeds your quota"}}"#);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Quota, msg::QUOTA, false));

        let e = classify(401, r#"{"detail":{"status":"invalid_api_key","message":"Invalid API key"}}"#);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unauthorized, msg::BAD_KEY, false));
        assert_eq!(e.api_code.as_deref(), Some("invalid_api_key"));

        let e = classify(
            401,
            r#"{"detail":{"status":"detected_unusual_activity","message":"Unusual activity detected. Free Tier usage disabled."}}"#,
        );
        assert_eq!(e.code, ReadErrorCode::Unauthorized);
        assert_eq!(e.message, "ElevenLabs: Unusual activity detected. Free Tier usage disabled.");

        let e = classify_with(429, r#"{"detail":{"status":"too_many_concurrent_requests","message":"x"}}"#, &[("retry-after", "3")]);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::RateLimit, msg::CONCURRENCY, true));
        assert_eq!(e.retry_after_sec, Some(3.0));

        let e = classify(422, r#"{"detail":[{"loc":["body","language_code"],"msg":"bad lang","type":"value_error"}]}"#);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unknown, "ElevenLabs: bad lang", false));
        assert_eq!(e.param.as_deref(), Some("body.language_code"));
        assert_eq!(e.status, Some(422));

        let e = classify(503, "<html>bad gateway</html>");
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unknown, msg::SERVER, true));

        let e = classify(418, "plain teapot text");
        assert_eq!((e.code, e.message.as_str()), (ReadErrorCode::Unknown, "ElevenLabs: plain teapot text"));

        let e = classify(403, r#"{"detail":{"message":"Forbidden"}}"#);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unknown, "ElevenLabs: Forbidden", false));

        let e = classify(403, "");
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::Unknown, "ElevenLabs returned an error (HTTP 403)", false));

        let e = classify(400, r#""just a string""#);
        assert_eq!((e.code, e.message.as_str()), (ReadErrorCode::Unknown, "ElevenLabs: just a string"));

        let e = classify(400, r#"{"detail":""}"#);
        assert_eq!((e.code, e.message.as_str()), (ReadErrorCode::Unknown, "ElevenLabs returned an error (HTTP 400)"));
        assert_eq!(e.api_message.as_deref(), Some(""));

        let e = classify(400, r#"{"detail":{"status":"QUOTA_EXCEEDED"}}"#);
        assert_eq!((e.code, e.message.as_str()), (ReadErrorCode::Quota, msg::QUOTA));
        assert_eq!(e.api_code.as_deref(), Some("QUOTA_EXCEEDED"));

        let e = classify(400, r#"{"status":"voice_not_found","message":"no voice"}"#);
        assert_eq!((e.code, e.message.as_str()), (ReadErrorCode::Unknown, msg::VOICE_MISSING));

        let e = classify(400, r#"{"detail":{"status":"max_character_limit_exceeded"}}"#);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::TooLong, msg::TOO_LONG, false));

        let e = classify(401, r#"{"detail":{"status":"needs_authorization"}}"#);
        assert_eq!((e.code, e.message.as_str()), (ReadErrorCode::Unauthorized, msg::BAD_KEY));

        let e = classify(
            401,
            r#"{"detail":{"code":"unauthorized","status":"missing_permissions","message":"The API key you used is missing the permission user_read to execute this operation."}}"#,
        );
        assert_eq!(e.code, ReadErrorCode::Unauthorized);
        assert_eq!(e.message, msg::permission("User (read)"));
        assert!(e.missing_permission);
        assert!(!e.retryable);

        let e = classify_with(500, r#"{"detail":{"status":"system_busy"}}"#, &[("retry-after", "1.5")]);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::RateLimit, msg::RATE_LIMIT, true));
        assert_eq!(e.retry_after_sec, Some(1.5));

        let e = classify_with(429, "{}", &[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]);
        assert_eq!((e.code, e.message.as_str(), e.retryable), (ReadErrorCode::RateLimit, msg::RATE_LIMIT, true));
        assert_eq!(e.retry_after_sec, None);

        let e = classify(400, r#"{"detail":[{"loc":[],"msg":""}]}"#);
        assert_eq!((e.code, e.message.as_str()), (ReadErrorCode::Unknown, "ElevenLabs returned an error (HTTP 400)"));
        assert_eq!(e.param.as_deref(), Some(""));

        let long = "x".repeat(400);
        let e = classify(400, &long);
        assert_eq!(e.code, ReadErrorCode::Unknown);
        assert_eq!(e.message.chars().count(), 300);
        assert!(e.message.starts_with("ElevenLabs: xxx"));

        let e = classify_with(401, r#"{"detail":{"request_id":"body-id"}}"#, &[("request-id", "hdr")]);
        assert_eq!(e.request_id.as_deref(), Some("body-id"));

        let e = classify_with(429, "{}", &[("retry-after", " 0x10 ")]);
        assert_eq!(e.retry_after_sec, Some(16.0));
    }

    #[test]
    fn classification_details() {
        // 401 messages are not truncated (row 3); row 10 is.
        let long = "m".repeat(400);
        let e = classify(401, &format!(r#"{{"detail":{{"message":"{long}"}}}}"#));
        assert_eq!(e.message.len(), "ElevenLabs: ".len() + 400);
        let e = classify(400, &format!(r#"{{"detail":{{"message":"{long}"}}}}"#));
        assert_eq!(e.message.len(), 300);
        // Row 10 never splits a surrogate pair (D-C-17): 299 units + an emoji → the emoji is dropped.
        let raw = format!(r#"{{"detail":{{"message":"{}😀"}}}}"#, "a".repeat(299 - "ElevenLabs: ".len()));
        let e = classify(400, &raw);
        assert!(e.message.ends_with('a'));
        // Request id fallbacks: header request-id, then x-trace-id.
        assert_eq!(classify_with(400, "{}", &[("x-trace-id", "t")]).request_id.as_deref(), Some("t"));
        assert_eq!(classify_with(400, "{}", &[("request-id", "r"), ("x-trace-id", "t")]).request_id.as_deref(), Some("r"));
        // 402 and payment_required type → quota; body.type is compared exactly.
        assert_eq!(classify(402, "{}").code, ReadErrorCode::Quota);
        assert_eq!(classify(400, r#"{"detail":{"type":"payment_required"}}"#).code, ReadErrorCode::Quota);
        assert_eq!(classify(400, r#"{"detail":{"type":"PAYMENT_REQUIRED"}}"#).code, ReadErrorCode::Unknown);
        assert_eq!(classify(400, r#"{"detail":{"type":"authentication_error"}}"#).message, msg::BAD_KEY);
        assert_eq!(classify(400, r#"{"detail":{"type":"rate_limit_error"}}"#).code, ReadErrorCode::RateLimit);
        // The missing-permission regex alone is enough.
        let e = classify(403, r#"{"detail":{"message":"Key is MISSING THE PERMISSION text_to_speech"}}"#);
        assert!(e.missing_permission);
        assert_eq!(e.message, msg::permission("Text to Speech"));
        // A 401 whose code is an invalid-key code keeps the generic text even with a message.
        let e = classify(401, r#"{"detail":{"code":"unauthorized","message":"nope"}}"#);
        assert_eq!(e.message, msg::BAD_KEY);
        // Voice/model rows.
        assert_eq!(classify(404, r#"{"detail":{"code":"voice_access_denied"}}"#).message, msg::VOICE_DENIED);
        assert_eq!(classify(400, r#"{"detail":{"status":"unsupported_model"}}"#).message, msg::MODEL);
        assert_eq!(classify(400, r#"{"detail":{"status":"text_too_long"}}"#).code, ReadErrorCode::TooLong);
        // 5xx with a body message is still the server message.
        let e = classify(502, r#"{"detail":{"message":"upstream"}}"#);
        assert_eq!((e.message.as_str(), e.retryable), (msg::SERVER, true));
    }

    #[test]
    fn parse_error_body_cases() {
        assert_eq!(parse_error_body(""), ErrorBody::default());
        assert_eq!(parse_error_body("   "), ErrorBody::default());
        assert_eq!(parse_error_body("  <html>x</html>"), ErrorBody::default());
        assert_eq!(parse_error_body("  oops  ").message.as_deref(), Some("oops"));
        assert_eq!(parse_error_body(&"y".repeat(500)).message.map(|m| m.len()), Some(300));
        // A top-level object without `detail` is read as the detail.
        let b = parse_error_body(r#"{"status":"voice_not_found","message":"no voice"}"#);
        assert_eq!(b.status.as_deref(), Some("voice_not_found"));
        assert_eq!(b.message.as_deref(), Some("no voice"));
        // `detail: null` → nothing (the key exists, so the body itself is not used).
        assert_eq!(parse_error_body(r#"{"detail":null,"message":"m"}"#), ErrorBody::default());
        assert_eq!(parse_error_body(r#"{"detail":5}"#), ErrorBody::default());
        assert_eq!(parse_error_body("42"), ErrorBody::default());
        assert_eq!(parse_error_body("null"), ErrorBody::default());
        // Full detail object; empty strings are absent.
        let b = parse_error_body(
            r#"{"detail":{"type":"t","code":"c","status":"s","message":"m","request_id":"r","param":"p","extra":1}}"#,
        );
        assert_eq!(
            b,
            ErrorBody {
                kind: Some("t".into()),
                code: Some("c".into()),
                status: Some("s".into()),
                message: Some("m".into()),
                request_id: Some("r".into()),
                param: Some("p".into()),
            }
        );
        let b = parse_error_body(r#"{"detail":{"type":"","code":3,"message":""}}"#);
        assert_eq!(b, ErrorBody::default());
        // FastAPI arrays: non-object items skipped, messages joined, loc stringified JS-style.
        let b = parse_error_body(r#"{"detail":["x",{"loc":["body",0,null,true],"msg":"a"},{"msg":"b"},{"msg":""}]}"#);
        assert_eq!(b.kind.as_deref(), Some("validation_error"));
        assert_eq!(b.message.as_deref(), Some("a; b"));
        assert_eq!(b.param.as_deref(), Some("body.0.null.true"));
        let b = parse_error_body(r#"{"detail":[{"msg":"only"}]}"#);
        assert_eq!(b.param, None);
        assert_eq!(parse_error_body("[]").kind.as_deref(), Some("validation_error"));
        // Bodies with a BOM or lone surrogate escapes still parse (lenient, D-NET-7).
        assert_eq!(parse_error_body("\u{FEFF}{\"detail\":\"x\\ud800\"}").message.as_deref(), Some("x\u{FFFD}"));
    }

    #[test]
    fn permission_message_examples_and_quirks() {
        let what = |m: &str| permission_message(Some(m));
        assert_eq!(
            what("The API key you used is missing the permission text_to_speech to execute this operation."),
            "Your ElevenLabs key isn\u{2019}t allowed to use Text to Speech \u{2014} enable it for the key on elevenlabs.io, or use another key"
        );
        assert_eq!(what("missing permission voices_read"), msg::permission("Voices (read)"));
        assert_eq!(what("missing permission models_read"), msg::permission("Models (read)"));
        assert_eq!(what("missing permission \"user_read\""), msg::permission("User (read)"));
        assert_eq!(what("Permission `speech_to_text` needed"), msg::permission("speech to text"));
        // Case-insensitive regex, case-sensitive lookup.
        assert_eq!(what("missing permission TEXT_TO_SPEECH"), msg::permission("TEXT TO SPEECH"));
        // The first word after `permission` + whitespace.
        assert_eq!(what("missing permission to do x"), msg::permission("to"));
        // `permission` must be followed directly by whitespace.
        assert_eq!(what("permissions x"), msg::permission("this feature"));
        assert_eq!(what("permissions x, permission\u{00A0}voices_read"), msg::permission("Voices (read)"));
        assert_eq!(permission_message(None), msg::permission("this feature"));
        assert_eq!(what("permission 123"), msg::permission("this feature"));
    }

    #[test]
    fn retry_after_and_int_header() {
        assert_eq!(parse_retry_after(None), None);
        assert_eq!(parse_retry_after(Some("")), None);
        assert_eq!(parse_retry_after(Some("0")), None);
        assert_eq!(parse_retry_after(Some("-1")), None);
        assert_eq!(parse_retry_after(Some("3")), Some(3.0));
        assert_eq!(parse_retry_after(Some("1.5")), Some(1.5));
        assert_eq!(parse_retry_after(Some("0x10")), Some(16.0));
        assert_eq!(parse_retry_after(Some(" 1e3 ")), Some(1000.0));
        assert_eq!(parse_retry_after(Some("Infinity")), None);
        assert_eq!(parse_retry_after(Some("3, 3")), None);
        assert_eq!(parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")), None);

        assert_eq!(int_header(None), None);
        assert_eq!(int_header(Some("")), None);
        assert_eq!(int_header(Some("  ")), None);
        assert_eq!(int_header(Some("0")), Some(0.0));
        assert_eq!(int_header(Some("42")), Some(42.0));
        assert_eq!(int_header(Some("1.5")), Some(1.5));
        assert_eq!(int_header(Some("0x10")), Some(16.0));
        assert_eq!(int_header(Some(" 1e3 ")), Some(1000.0));
        assert_eq!(int_header(Some("-1")), None);
        assert_eq!(int_header(Some("Infinity")), None);
        assert_eq!(int_header(Some("3, 3")), None);
        assert_eq!(int_header(Some("abc")), None);
    }

    /// D-NET-2: retryable only when certainly unprocessed.
    #[test]
    fn retry_policy_per_constructor() {
        assert!(!TtsError::timeout().retryable);
        assert_eq!(TtsError::timeout().code, ReadErrorCode::Network);
        assert_eq!(TtsError::timeout().message, msg::TIMEOUT);
        assert!(!TtsError::network().retryable);
        assert!(TtsError::network_unsent().retryable);
        assert_eq!(TtsError::network_unsent().message, msg::NETWORK);
        assert!(!TtsError::bad_response().retryable);
        assert!(!TtsError::no_audio().retryable);
        assert!(!TtsError::no_key().retryable);
        assert_eq!(TtsError::no_key().code, ReadErrorCode::NoApiKey);
        let c = TtsError::cancelled();
        assert!(c.aborted && !c.retryable);
        assert_eq!(c.message, msg::CANCELLED);
        assert_eq!(TtsError::empty_text().code, ReadErrorCode::EmptyText);
        assert_eq!(TtsError::cached_audio_missing().code, ReadErrorCode::NotCached);
        assert_eq!(TtsError::crashed().message, msg::ENGINE_CRASHED);
        assert!(classify(429, "{}").retryable);
        assert!(classify(500, "{}").retryable);
        assert!(!classify(400, "{}").retryable);
        assert!(!classify(401, "{}").retryable);
        assert!(!classify(402, "{}").retryable);
        assert_eq!(TtsError::network().to_string(), msg::NETWORK);
    }
}
