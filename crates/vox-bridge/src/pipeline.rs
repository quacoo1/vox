//! The request pipeline (integrations.md §1.4–§1.6), in this exact order for every request: `OPTIONS` → 403;
//! `Host` must be `127.0.0.1:<port>`/`localhost:<port>` → else 403 `Forbidden host`; `Origin` absent/empty OK, `null`
//! / `http:` / `https:` / `file:` → 403 `Forbidden origin`; path (trailing slashes removed); `/health` GET/HEAD
//! (else 405 `Allow: GET`); `/read`, `/command` POST only (405 `Allow: POST`), others 404; `x-vox-client:
//! browser-extension` → else 403; `Content-Type` `^application/json\s*(;|$)` → else 415; body ≤ 2 MiB (413
//! `too-long`, `Connection: close`) within 15 s (408); UTF-8 (BOM stripped), JSON object.
//!
//! Every JSON response carries `Content-Type: application/json; charset=utf-8`, `Content-Length`,
//! `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`; HEAD omits the body. No CORS headers anywhere.
//!
//! Path parsing (step 4) follows the WHATWG URL rules the old server got from `new URL(target, 'http://127.0.0.1')`
//! for every request-target the HTTP layer lets through: query and fragment dropped, `\` = `/`, `//host/…` and
//! `scheme://host/…` route by their path (an empty or invalid host → 400 `Bad URL`), dot segments (also `%2e`)
//! resolved, percent-escapes otherwise kept (`/%68ealth` → 404), trailing slashes stripped from paths longer than
//! one character, compared case-sensitively.

use serde_json::{json, Value};
use vox_core::constants::{
    BRIDGE_MAX_BODY_BYTES, BRIDGE_TITLE_MAX, BRIDGE_URL_MAX, LOCAL_SERVER_CLIENT_HEADER, LOCAL_SERVER_CLIENT_VALUE,
};
use vox_core::js::{is_js_whitespace, js_trim};
use vox_core::json::{parse_lenient_with, OutOfRange};
use vox_core::player::{parse_bridge_command, PlayerCommand};
use vox_core::read::{BridgeReadRequest, ReadErrorCode};
use vox_core::text16::{prefix16, utf16_len};

/// A response to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Send `Connection: close` and close after writing.
    pub close: bool,
}

/// A routed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Health { head: bool },
    Read,
    Command,
}

/// The request headers in arrival order (names as received). Lookups are case-insensitive; duplicates follow
/// integrations.md §1.4 "Duplicate headers": `Host`, `Content-Type` and `Content-Length` use the **first** value;
/// `Origin` and `x-vox-client` join all values with `", "` in arrival order (private helpers `first_header(name)` and
/// `joined_header(name)`). Verified: `Host: 127.0.0.1:$P` then `Host: evil:$P` → 200; `application/json` then
/// `text/plain` → accepted; duplicate `x-vox-client` → 403; `Browser-Extension` → 403; both Origin orders.
pub type Headers<'a> = &'a [(String, String)];

/// Steps 1–7 (method, Host, Origin, path, method per route, client header). `Err` is the response to send.
pub fn check_request(method: &str, url: &str, headers: Headers<'_>, bound_port: u16) -> Result<Route, HttpResponse> {
    let method = method.to_ascii_uppercase();
    let head = method == "HEAD";
    let fail = |status: u16, message: &str| error_response_for(status, message, None, head);
    // 1. Preflights are how browsers ask permission for cross-origin calls; the answer is always no (no CORS headers).
    if method == "OPTIONS" {
        return Err(fail(403, "Forbidden"));
    }
    // 2. DNS-rebinding guard: the Host must name the loopback listener literally.
    let host = js_trim(first_header(headers, "host").unwrap_or("")).to_lowercase();
    if host != format!("127.0.0.1:{bound_port}") && host != format!("localhost:{bound_port}") {
        return Err(fail(403, "Forbidden host"));
    }
    // 3. Web origins (and `null` / `file:`) are refused; extension origins and no Origin pass.
    let origin = joined_header(headers, "origin").map(|o| js_trim(&o).to_lowercase()).unwrap_or_default();
    if origin == "null" || ["http:", "https:", "file:"].iter().any(|p| origin.starts_with(p)) {
        return Err(fail(403, "Forbidden origin"));
    }
    // 4. Path.
    let Some(path) = parse_path(url) else {
        return Err(fail(400, "Bad URL"));
    };
    // 5. Health: no client header, no content type, no body.
    if path == "/health" {
        if method != "GET" && method != "HEAD" {
            return Err(with_header(fail(405, "Method not allowed"), "Allow", "GET"));
        }
        return Ok(Route::Health { head });
    }
    // 6–7. Routes.
    let route = match path.as_str() {
        "/read" => Route::Read,
        "/command" => Route::Command,
        _ => return Err(fail(404, "Not found")),
    };
    if method != "POST" {
        return Err(with_header(fail(405, "Method not allowed"), "Allow", "POST"));
    }
    // 8 (integrations.md numbering). The custom header is what forces a CORS preflight in browsers.
    if joined_header(headers, LOCAL_SERVER_CLIENT_HEADER).as_deref() != Some(LOCAL_SERVER_CLIENT_VALUE) {
        return Err(fail(403, "Forbidden"));
    }
    Ok(route)
}

/// Step 8 pre-checks before reading the body: content type, declared length.
pub fn check_body_headers(headers: Headers<'_>) -> Result<(), HttpResponse> {
    let content_type = first_header(headers, "content-type").unwrap_or("").to_lowercase();
    if !is_json_content_type(&content_type) {
        return Err(error_response(415, "Content-Type must be application/json", None));
    }
    if let Some(declared) = first_header(headers, "content-length").and_then(js_number) {
        if declared > BRIDGE_MAX_BODY_BYTES as f64 {
            return Err(error_response(413, "Request body too large", Some(ReadErrorCode::TooLong)));
        }
    }
    Ok(())
}

/// Body → JSON object: lossy UTF-8, then `vox_core::json::parse_lenient_with(text, OutOfRange::Null)` (BOM stripped,
/// lone surrogate escapes → U+FFFD, an out-of-range literal like `1e400` → `null`, which reproduces every old outcome:
/// `"text":1e400` → 400 `"text" must be a string` (code `empty-text`), `title`/`url` dropped, `rate`/`positionSec`/
/// `amount` → `must be a finite number`, `direction` → `must be 1 or -1`, a top-level `1e400` → `Expected a JSON
/// object`; ARCHITECTURE.md §8 D-INT-15); empty or invalid → 400 `Invalid JSON`; not an object → 400 `Expected a JSON
/// object`.
pub fn parse_json_body(body: &[u8]) -> Result<serde_json::Map<String, Value>, HttpResponse> {
    let text = String::from_utf8_lossy(body);
    match parse_lenient_with(&text, OutOfRange::Null) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(error_response(400, "Expected a JSON object", None)),
        Err(_) => Err(error_response(400, "Invalid JSON", None)),
    }
}

/// `POST /read` body (integrations.md §1.6.2): `text` must be a non-blank string (400 `empty-text`); `url` kept if a
/// string ≤ 2048 matching `^(https?|file)://`; `title` trimmed, ≤ 300, dropped if blank.
pub fn parse_read_body(body: &serde_json::Map<String, Value>) -> Result<BridgeReadRequest, HttpResponse> {
    let Some(Value::String(text)) = body.get("text") else {
        return Err(error_response(400, "\"text\" must be a string", Some(ReadErrorCode::EmptyText)));
    };
    if js_trim(text).is_empty() {
        return Err(error_response(400, "Nothing to read \u{2014} the text is empty", Some(ReadErrorCode::EmptyText)));
    }
    let url = match body.get("url") {
        Some(Value::String(u)) if utf16_len(u) <= BRIDGE_URL_MAX && has_page_scheme(u) => Some(u.clone()),
        _ => None,
    };
    let title = match body.get("title") {
        Some(Value::String(t)) if !js_trim(t).is_empty() => Some(prefix16(js_trim(t), BRIDGE_TITLE_MAX).to_string()),
        _ => None,
    };
    Ok(BridgeReadRequest { text: text.clone(), url, title })
}

/// `POST /command` body via `vox_core::player::parse_bridge_command` (400 with its message).
pub fn parse_command_body(body: &serde_json::Map<String, Value>) -> Result<PlayerCommand, HttpResponse> {
    parse_bridge_command(&Value::Object(body.clone())).map_err(|message| error_response(400, &message, None))
}

/// A JSON response with the four standard headers.
pub fn json_response(status: u16, body: &Value, head: bool) -> HttpResponse {
    // Serializing a `Value` cannot fail (its map keys are strings); the fallback only keeps this total.
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    HttpResponse {
        status,
        headers: vec![
            ("Content-Type".to_string(), "application/json; charset=utf-8".to_string()),
            ("Content-Length".to_string(), payload.len().to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
            ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
        ],
        body: if head { Vec::new() } else { payload },
        close: false,
    }
}

/// `{ "ok": false, "error": msg, "code"?: code }`. A 408 or 413 (raised around the body) also closes the connection
/// (`Connection: close`, integrations.md §1.4 steps 10–11).
pub fn error_response(status: u16, message: &str, code: Option<ReadErrorCode>) -> HttpResponse {
    error_response_for(status, message, code, false)
}

/// `{ "ok": true, "app": "Vox", "version": "<x.y.z>", "playing": bool }`.
pub fn health_body(status: &crate::server::BridgeStatus) -> Value {
    json!({ "ok": true, "app": status.app_name, "version": status.version, "playing": status.playing })
}

fn error_response_for(status: u16, message: &str, code: Option<ReadErrorCode>, head: bool) -> HttpResponse {
    let mut body = serde_json::Map::new();
    body.insert("ok".into(), Value::Bool(false));
    body.insert("error".into(), Value::String(message.to_string()));
    if let Some(code) = code {
        body.insert("code".into(), Value::String(code.as_str().to_string()));
    }
    let mut resp = json_response(status, &Value::Object(body), head);
    resp.close = matches!(status, 408 | 413);
    resp
}

fn with_header(mut resp: HttpResponse, name: &str, value: &str) -> HttpResponse {
    resp.headers.push((name.to_string(), value.to_string()));
    resp
}

/// The first value of a header (Node keeps the first `Host` / `Content-Type` / `Content-Length`).
fn first_header<'a>(headers: Headers<'a>, name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// All values of a header joined with `", "` in arrival order (Node's handling of `Origin` / `x-vox-client`).
fn joined_header(headers: Headers<'_>, name: &str) -> Option<String> {
    let values: Vec<&str> = headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// `/^application\/json\s*(;|$)/` on the lower-cased value (JS `\s`).
fn is_json_content_type(lower: &str) -> bool {
    lower.strip_prefix("application/json").is_some_and(|rest| {
        let rest = rest.trim_start_matches(is_js_whitespace);
        rest.is_empty() || rest.starts_with(';')
    })
}

/// `Number(value)` for the decimal forms a `Content-Length` can take; anything else is NaN (`None`), which the old
/// check skipped (the HTTP layer already refuses a non-numeric `Content-Length`).
fn js_number(value: &str) -> Option<f64> {
    let t = js_trim(value);
    if t.is_empty() {
        return Some(0.0);
    }
    if !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    t.parse::<f64>().ok()
}

/// `/^(https?|file):\/\//i`.
fn has_page_scheme(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    ["http://", "https://", "file://"].iter().any(|p| lower.starts_with(p))
}

/// WHATWG special schemes (their `\` is a path separator and their URLs need a host, except `file`).
const SPECIAL_SCHEMES: [&str; 6] = ["ftp", "file", "http", "https", "ws", "wss"];

/// The path the old server routed on (`new URL(target, 'http://127.0.0.1').pathname`, trailing slashes stripped),
/// or `None` where that constructor threw (→ 400 `Bad URL`).
pub(crate) fn parse_path(target: &str) -> Option<String> {
    // The URL parser drops tabs and newlines and trims C0 controls and spaces.
    let cleaned: String = target.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let cleaned = cleaned.trim_matches(|c: char| c <= ' ');
    // Query and fragment never take part in routing (and end any authority).
    let end = cleaned.find(['?', '#']).unwrap_or(cleaned.len());
    let input = &cleaned[..end];

    let path = match scheme_of(input) {
        Some((scheme, rest)) if SPECIAL_SCHEMES.contains(&scheme.as_str()) => {
            let rest = rest.replace('\\', "/");
            if scheme == "http" && !rest.starts_with("//") {
                // Same scheme as the base: a relative reference (`http:/health`, `http:health`).
                relative_path(&rest)
            } else if scheme == "file" {
                let after = rest.strip_prefix("//").map(|r| r.find('/').map_or("", |i| &r[i..])).unwrap_or(&rest);
                absolute_path(after)
            } else {
                authority_path(rest.trim_start_matches('/'), true)?
            }
        }
        Some((_, rest)) => match rest.strip_prefix("//") {
            Some(after) => authority_path(after, false)?,
            // An opaque path (`mailto:x`) is not normalized; it never matches a route.
            None => rest.to_string(),
        },
        None => {
            let input = input.replace('\\', "/");
            if input.starts_with("//") {
                authority_path(input.trim_start_matches('/'), true)?
            } else {
                relative_path(&input)
            }
        }
    };
    Some(if path.len() > 1 { path.trim_end_matches('/').to_string() } else { path })
}

/// `scheme:` at the start (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`), lower-cased, and the rest.
fn scheme_of(input: &str) -> Option<(String, &str)> {
    let colon = input.find(':')?;
    let scheme = &input[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some((scheme.to_ascii_lowercase(), &input[colon + 1..]))
}

/// A reference resolved against the base path `/`.
fn relative_path(rest: &str) -> String {
    if rest.starts_with('/') {
        absolute_path(rest)
    } else {
        absolute_path(&format!("/{rest}"))
    }
}

/// `authority [path]`: validates the host (an empty host fails for special schemes) and returns the path.
fn authority_path(after_slashes: &str, special: bool) -> Option<String> {
    let split = after_slashes.find('/').unwrap_or(after_slashes.len());
    let (authority, path) = after_slashes.split_at(split);
    if !valid_authority(authority, special) {
        return None;
    }
    Some(absolute_path(path))
}

fn valid_authority(authority: &str, special: bool) -> bool {
    // Credentials end at the last `@`.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if host_port.starts_with('[') {
        match host_port.find(']') {
            Some(i) => (&host_port[..=i], &host_port[i + 1..]),
            None => return false,
        }
    } else {
        match host_port.rfind(':') {
            Some(i) => (&host_port[..i], &host_port[i..]),
            None => (host_port, ""),
        }
    };
    if !port.is_empty() {
        let Some(digits) = port.strip_prefix(':') else { return false };
        if !digits.bytes().all(|b| b.is_ascii_digit()) || digits.parse::<u32>().map_or(!digits.is_empty(), |p| p > 65_535) {
            return false;
        }
    }
    if host.is_empty() {
        return !special;
    }
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.trim_end_matches(']');
        return !inner.is_empty() && inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    let decoded = percent_decode(host);
    !decoded.chars().any(|c| {
        c <= ' '
            || matches!(c, '#' | '%' | '/' | ':' | '<' | '>' | '?' | '@' | '[' | '\\' | ']' | '^' | '|' | '\u{7f}')
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(b[i + 1]) * 16 + hex(b[i + 2]));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolves `.` / `..` segments (including their `%2e` spellings) in a path that starts with `/` (or is empty).
fn absolute_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
    let last = segments.len() - 1;
    for (i, seg) in segments.iter().enumerate() {
        let lower = seg.to_ascii_lowercase();
        let single = lower == "." || lower == "%2e";
        let double = matches!(lower.as_str(), ".." | ".%2e" | "%2e." | "%2e%2e");
        if double {
            out.pop();
            if i == last {
                out.push("");
            }
        } else if single {
            if i == last {
                out.push("");
            }
        } else {
            out.push(seg);
        }
    }
    format!("/{}", out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::BridgeStatus;

    const P: u16 = 47_823;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    fn host() -> (&'static str, String) {
        ("Host", format!("127.0.0.1:{P}"))
    }

    fn base_with(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let (n, v) = host();
        let mut h = vec![(n.to_string(), v)];
        h.extend(hdrs(extra));
        h
    }

    fn client() -> Vec<(String, String)> {
        base_with(&[("x-vox-client", "browser-extension"), ("Content-Type", "application/json")])
    }

    fn body_json(resp: &HttpResponse) -> Value {
        serde_json::from_slice(&resp.body).expect("JSON body")
    }

    fn body_text(resp: &HttpResponse) -> String {
        String::from_utf8(resp.body.clone()).expect("UTF-8 body")
    }

    fn header<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
        resp.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn err(method: &str, url: &str, headers: &[(String, String)]) -> HttpResponse {
        check_request(method, url, headers, P).expect_err("an error response")
    }

    fn assert_error(resp: &HttpResponse, status: u16, body: &str) {
        assert_eq!(resp.status, status);
        assert_eq!(body_text(resp), body);
    }

    #[test]
    fn health_routes() {
        let h = base_with(&[]);
        assert_eq!(check_request("GET", "/health", &h, P), Ok(Route::Health { head: false }));
        assert_eq!(check_request("HEAD", "/health", &h, P), Ok(Route::Health { head: true }));
        for url in ["/health/?x=1", "/health//", "/health#x", "/health\\", "//evil/health", "http://evil/health", "/x/../health", "/./health"] {
            assert_eq!(check_request("GET", url, &h, P), Ok(Route::Health { head: false }), "{url}");
        }
        for url in ["/%68ealth", "/health%2F", "/HEALTH", "/nope", "/", "/health.", "/healthz"] {
            assert_error(&err("GET", url, &h), 404, r#"{"ok":false,"error":"Not found"}"#);
        }
        for url in ["//", "///", "//:80/health", "//evil:x/health", "//evil:70000/health", "//ev%il/health"] {
            assert_error(&err("GET", url, &h), 400, r#"{"ok":false,"error":"Bad URL"}"#);
        }
    }

    #[test]
    fn method_rules() {
        let h = base_with(&[]);
        for m in ["POST", "PATCH", "PUT", "DELETE"] {
            let r = err(m, "/health", &h);
            assert_error(&r, 405, r#"{"ok":false,"error":"Method not allowed"}"#);
            assert_eq!(header(&r, "Allow"), Some("GET"));
        }
        for m in ["GET", "HEAD", "PUT", "PATCH", "DELETE"] {
            let r = err(m, "/read", &client());
            assert_eq!(r.status, 405);
            assert_eq!(header(&r, "Allow"), Some("POST"));
        }
        // HEAD on an error: same status and Content-Length, no body.
        let r = err("HEAD", "/nope", &h);
        assert_eq!(r.status, 404);
        assert!(r.body.is_empty());
        assert_eq!(header(&r, "Content-Length"), Some(r#"{"ok":false,"error":"Not found"}"#.len().to_string().as_str()));
        let r = err("HEAD", "/read", &h);
        assert_eq!((r.status, r.body.is_empty()), (405, true));
    }

    #[test]
    fn options_always_forbidden() {
        let with_origin = base_with(&[("Origin", "https://evil.com")]);
        assert_error(&err("OPTIONS", "/read", &with_origin), 403, r#"{"ok":false,"error":"Forbidden"}"#);
        // Before Host and Origin, on any path.
        assert_error(&err("OPTIONS", "/nope", &hdrs(&[("Host", "evil")])), 403, r#"{"ok":false,"error":"Forbidden"}"#);
        let r = err("OPTIONS", "/health", &client());
        assert!(r.headers.iter().all(|(n, _)| !n.to_ascii_lowercase().starts_with("access-control")));
    }

    #[test]
    fn host_rules() {
        let forbidden = r#"{"ok":false,"error":"Forbidden host"}"#;
        for value in [
            "evil.com:47823".to_string(),
            "127.0.0.1".to_string(),
            "localhost.:47823".to_string(),
            "[::1]:47823".to_string(),
            "127.0.0.1:047823".to_string(),
            "127.0.0.1:1".to_string(),
            String::new(),
        ] {
            assert_error(&err("GET", "/health", &hdrs(&[("Host", value.as_str())])), 403, forbidden);
        }
        // HTTP/1.0 without Host.
        assert_error(&err("GET", "/health", &[]), 403, forbidden);
        for value in ["LOCALHOST:47823", "localhost:47823", " 127.0.0.1:47823 ", "127.0.0.1:47823\u{A0}"] {
            assert!(check_request("GET", "/health", &hdrs(&[("Host", value)]), P).is_ok(), "{value:?}");
        }
        // Duplicate Host: the first one decides.
        let dup = hdrs(&[("Host", "127.0.0.1:47823"), ("Host", "evil:47823")]);
        assert!(check_request("GET", "/health", &dup, P).is_ok());
        let dup = hdrs(&[("Host", "evil:47823"), ("host", "127.0.0.1:47823")]);
        assert_eq!(err("GET", "/health", &dup).status, 403);
        // The bound port matters (port 0 listeners report the real port).
        assert!(check_request("GET", "/health", &hdrs(&[("host", "127.0.0.1:5000")]), 5000).is_ok());
    }

    #[test]
    fn origin_rules() {
        let forbidden = r#"{"ok":false,"error":"Forbidden origin"}"#;
        for origin in ["http://x", "null", "NULL", "https://evil.com", " HTTPS://X ", "file://", "file:", "http:"] {
            assert_error(&err("GET", "/health", &base_with(&[("Origin", origin)])), 403, forbidden);
        }
        for origin in ["chrome-extension://abc", "moz-extension://x", "", "   ", "httpx://x"] {
            assert!(check_request("GET", "/health", &base_with(&[("Origin", origin)]), P).is_ok(), "{origin:?}");
        }
        // Duplicates are joined, so only the first one decides.
        let ok = base_with(&[("Origin", "chrome-extension://a"), ("Origin", "http://x")]);
        assert!(check_request("GET", "/health", &ok, P).is_ok());
        let bad = base_with(&[("Origin", "http://x"), ("origin", "chrome-extension://a")]);
        assert_error(&err("GET", "/health", &bad), 403, forbidden);
        // Origin is checked before the path and on every route.
        assert_eq!(err("GET", "/nope", &base_with(&[("Origin", "null")])).status, 403);
    }

    #[test]
    fn client_header_rules() {
        let forbidden = r#"{"ok":false,"error":"Forbidden"}"#;
        assert_error(&err("POST", "/read", &base_with(&[("Content-Type", "application/json")])), 403, forbidden);
        // The client header is checked before the content type.
        assert_error(&err("POST", "/read", &base_with(&[("Content-Type", "text/plain")])), 403, forbidden);
        assert_error(&err("POST", "/command", &base_with(&[("x-vox-client", "Browser-Extension")])), 403, forbidden);
        let dup = base_with(&[("x-vox-client", "browser-extension"), ("X-Vox-Client", "browser-extension")]);
        assert_error(&err("POST", "/read", &dup), 403, forbidden);
        assert_eq!(check_request("POST", "/read", &client(), P), Ok(Route::Read));
        assert_eq!(check_request("POST", "/command/", &client(), P), Ok(Route::Command));
        // `/health` needs no client header.
        assert!(check_request("GET", "/health", &base_with(&[]), P).is_ok());
    }

    #[test]
    fn content_type_rules() {
        for ct in ["application/json", "Application/JSON; charset=utf-8", "application/json ;x", "application/json\t;x", "application/json\u{A0};x"] {
            assert_eq!(check_body_headers(&hdrs(&[("Content-Type", ct)])), Ok(()), "{ct:?}");
        }
        for ct in [None, Some("text/plain"), Some("application/jsonx"), Some("application/vnd.api+json"), Some("")] {
            let h = ct.map(|c| hdrs(&[("Content-Type", c)])).unwrap_or_default();
            let r = check_body_headers(&h).expect_err("415");
            assert_error(&r, 415, r#"{"ok":false,"error":"Content-Type must be application/json"}"#);
            assert!(!r.close);
        }
        // Duplicate Content-Type: the first one decides.
        assert_eq!(check_body_headers(&hdrs(&[("Content-Type", "application/json"), ("Content-Type", "text/plain")])), Ok(()));
        assert!(check_body_headers(&hdrs(&[("Content-Type", "text/plain"), ("Content-Type", "application/json")])).is_err());
    }

    #[test]
    fn declared_length_rules() {
        let with_len = |len: &str| hdrs(&[("Content-Type", "application/json"), ("Content-Length", len)]);
        let r = check_body_headers(&with_len("3000000")).expect_err("413");
        assert_error(&r, 413, r#"{"ok":false,"error":"Request body too large","code":"too-long"}"#);
        assert!(r.close);
        assert_eq!(check_body_headers(&with_len("2097152")), Ok(()));
        assert!(check_body_headers(&with_len("2097153")).is_err());
        assert_eq!(check_body_headers(&with_len("0")), Ok(()));
        // The content type is checked before the length.
        let r = check_body_headers(&hdrs(&[("Content-Type", "text/plain"), ("Content-Length", "3000000")])).unwrap_err();
        assert_eq!(r.status, 415);
    }

    #[test]
    fn json_body_rules() {
        let invalid = r#"{"ok":false,"error":"Invalid JSON"}"#;
        let not_object = r#"{"ok":false,"error":"Expected a JSON object"}"#;
        for body in [&b"{x"[..], b"", b"   ", b"\xEF\xBB\xBF", b"{\"a\":1}x", b"\xEF\xBB\xBF\xEF\xBB\xBF{}"] {
            assert_error(&parse_json_body(body).unwrap_err(), 400, invalid);
        }
        for body in [&b"[]"[..], b"null", b"\"x\"", b"1", b"true", b"1e400", b"-1e400"] {
            assert_error(&parse_json_body(body).unwrap_err(), 400, not_object);
        }
        assert!(parse_json_body(b" \n {\"text\":\"hi\"} \n ").is_ok());
        let map = parse_json_body(b"\xEF\xBB\xBF{\"text\":\"hi\"}").unwrap();
        assert_eq!(map.get("text"), Some(&Value::String("hi".into())));
        // Lossy UTF-8.
        let map = parse_json_body(b"{\"text\":\"a\xFF\"}").unwrap();
        assert_eq!(map.get("text"), Some(&Value::String("a\u{FFFD}".into())));
        // Lone surrogate escapes → U+FFFD; a real pair survives; an escaped backslash is not an escape.
        let map = parse_json_body(br#"{"text":"a\ud83d","title":"\udc00x","pair":"\ud83d\ude00","lit":"\\ud83d"}"#).unwrap();
        assert_eq!(map["text"], "a\u{FFFD}");
        assert_eq!(map["title"], "\u{FFFD}x");
        assert_eq!(map["pair"], "\u{1F600}");
        assert_eq!(map["lit"], "\\ud83d");
        // Last duplicate key wins.
        let map = parse_json_body(br#"{"type":"jump","type":"stop"}"#).unwrap();
        assert_eq!(map["type"], "stop");
        // 1e400 becomes null (D-INT-15).
        let map = parse_json_body(br#"{"rate":1e400,"x":-1E+999}"#).unwrap();
        assert_eq!(map["rate"], Value::Null);
        assert_eq!(map["x"], Value::Null);
    }

    fn read_req(json: &str) -> Result<BridgeReadRequest, HttpResponse> {
        parse_read_body(&parse_json_body(json.as_bytes()).expect("object"))
    }

    #[test]
    fn read_body_rules() {
        let not_string = r#"{"ok":false,"error":"\"text\" must be a string","code":"empty-text"}"#;
        let empty = "{\"ok\":false,\"error\":\"Nothing to read \u{2014} the text is empty\",\"code\":\"empty-text\"}";
        for json in [r#"{"text":1}"#, r#"{}"#, r#"{"text":null}"#, r#"{"text":["a"]}"#, r#"{"text":1e400}"#] {
            assert_error(&read_req(json).unwrap_err(), 400, not_string);
        }
        for json in [r#"{"text":" \n\t "}"#, r#"{"text":""}"#, r#"{"text":"\uFEFF\u00A0"}"#, r#"{"text":"\u3000\u2028"}"#] {
            assert_error(&read_req(json).unwrap_err(), 400, empty);
        }
        assert_eq!(read_req(r#"{"text":"hi"}"#).unwrap(), BridgeReadRequest { text: "hi".into(), url: None, title: None });
        // Text is passed through untouched.
        assert_eq!(read_req(r#"{"text":"  hi  "}"#).unwrap().text, "  hi  ");
        assert_eq!(
            read_req(r#"{"text":"hi","url":"javascript:alert(1)","title":"  T  ","extra":1}"#).unwrap(),
            BridgeReadRequest { text: "hi".into(), url: None, title: Some("T".into()) }
        );
        assert_eq!(
            read_req(r#"{"text":"a","title":"   ","url":"HTTPS://X"}"#).unwrap(),
            BridgeReadRequest { text: "a".into(), url: Some("HTTPS://X".into()), title: None }
        );
        assert_eq!(read_req(r#"{"text":"a","url":"FILE:///c:/a.txt"}"#).unwrap().url.as_deref(), Some("FILE:///c:/a.txt"));
        for url in ["https:example.com", "ftp://x", "http:/x", " http://x", "javascript:x"] {
            let json = format!(r#"{{"text":"a","url":{}}}"#, Value::String(url.into()));
            assert_eq!(read_req(&json).unwrap().url, None, "{url}");
        }
        // Non-string url/title are dropped, never an error; 1e400 becomes null and is dropped too.
        let r = read_req(r#"{"text":"a","url":5,"title":5}"#).unwrap();
        assert_eq!((r.url, r.title), (None, None));
        let r = read_req(r#"{"text":"a","url":1e400,"title":1e400}"#).unwrap();
        assert_eq!((r.url, r.title), (None, None));
        // URL length: ≤ 2048 UTF-16 units.
        let long = format!("https://x/{}", "a".repeat(2048 - 10));
        assert_eq!(read_req(&format!(r#"{{"text":"a","url":"{long}"}}"#)).unwrap().url.as_deref(), Some(long.as_str()));
        let too_long = format!("{long}a");
        assert_eq!(read_req(&format!(r#"{{"text":"a","url":"{too_long}"}}"#)).unwrap().url, None);
        // Title: trimmed, cut to 300 units, never splitting a pair.
        let title = format!("  {}  ", "t".repeat(400));
        assert_eq!(read_req(&format!(r#"{{"text":"a","title":"{title}"}}"#)).unwrap().title.unwrap().len(), 300);
        let emoji_title = format!("{}\u{1F600}x", "t".repeat(299));
        let json = format!(r#"{{"text":"a","title":{}}}"#, Value::String(emoji_title));
        assert_eq!(read_req(&json).unwrap().title.unwrap(), "t".repeat(299));
        // A lone surrogate from the extension's 300-unit cut arrives as U+FFFD.
        let r = read_req(r#"{"text":"a\ud83d","title":"T\ud83d"}"#).unwrap();
        assert_eq!((r.text.as_str(), r.title.as_deref()), ("a\u{FFFD}", Some("T\u{FFFD}")));
    }

    fn command(json: &str) -> Result<PlayerCommand, HttpResponse> {
        parse_command_body(&parse_json_body(json.as_bytes()).expect("object"))
    }

    fn command_error(json: &str) -> String {
        let r = command(json).unwrap_err();
        assert_eq!(r.status, 400);
        assert!(body_json(&r).get("code").is_none());
        body_json(&r)["error"].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn command_body_rules() {
        use vox_core::player::{SkipDirection, SkipUnit};
        assert_eq!(command(r#"{"type":"toggle","x":true}"#), Ok(PlayerCommand::Toggle));
        assert_eq!(command(r#"{"type":"stop"}"#), Ok(PlayerCommand::Stop));
        assert_eq!(command(r#"{"type":"rate","rate":10}"#), Ok(PlayerCommand::Rate { rate: 3.0 }));
        assert_eq!(command(r#"{"type":"seek","positionSec":-5}"#), Ok(PlayerCommand::Seek { position_sec: 0.0 }));
        assert_eq!(command(r#"{"type":"seek","positionSec":1.2e29}"#), Ok(PlayerCommand::Seek { position_sec: 1.2e29 }));
        assert_eq!(
            command(r#"{"type":"skip","direction":-1,"unit":"seconds","amount":99999}"#),
            Ok(PlayerCommand::Skip { direction: SkipDirection::Back, unit: SkipUnit::Seconds, amount: Some(3600.0) })
        );
        assert_eq!(
            command(r#"{"type":"skip","direction":1.0,"unit":"sentence"}"#),
            Ok(PlayerCommand::Skip { direction: SkipDirection::Forward, unit: SkipUnit::Sentence, amount: None })
        );
        assert_eq!(command(r#"{"type":"jump","type":"stop"}"#), Ok(PlayerCommand::Stop));
        assert_eq!(command_error(r#"{"type":"skip","direction":2,"unit":"words"}"#), "\"direction\" must be 1 or -1");
        assert_eq!(command_error(r#"{"type":"skip","direction":-1,"unit":"x","amount":"y"}"#), "\"unit\" must be sentence|seconds");
        assert_eq!(command_error(r#"{"type":"skip","direction":1,"unit":"seconds","amount":null}"#), "\"amount\" must be a finite number");
        assert_eq!(command_error(r#"{"type":"jump"}"#), "Unknown command type");
        assert_eq!(command_error(r#"{"type":1}"#), "Unknown command type");
        assert_eq!(command_error(r#"{"type":"Stop"}"#), "Unknown command type");
        assert_eq!(command_error(r#"{}"#), "Unknown command type");
        assert_eq!(command_error(r#"{"type":"rate","rate":"2"}"#), "\"rate\" must be a finite number");
        // 1e400 → null → the same messages as JS Infinity (D-INT-15).
        assert_eq!(command_error(r#"{"type":"rate","rate":1e400}"#), "\"rate\" must be a finite number");
        assert_eq!(command_error(r#"{"type":"seek","positionSec":1e400}"#), "\"positionSec\" must be a finite number");
        assert_eq!(command_error(r#"{"type":"skip","direction":1,"unit":"seconds","amount":1e400}"#), "\"amount\" must be a finite number");
        assert_eq!(command_error(r#"{"type":"skip","direction":1e400,"unit":"seconds"}"#), "\"direction\" must be 1 or -1");
        assert_eq!(command_error(r#"{"type":"seekFraction"}"#), "\"fraction\" must be a finite number");
        assert_eq!(command_error(r#"{"type":"volume","volume":true}"#), "\"volume\" must be a finite number");
        assert_eq!(body_text(&command(r#"{"type":"jump"}"#).unwrap_err()), r#"{"ok":false,"error":"Unknown command type"}"#);
    }

    #[test]
    fn response_format() {
        let status = BridgeStatus { app_name: "Vox".into(), version: "0.1.0".into(), playing: false };
        let r = json_response(200, &health_body(&status), false);
        assert_eq!(body_text(&r), r#"{"ok":true,"app":"Vox","version":"0.1.0","playing":false}"#);
        assert_eq!(
            r.headers,
            hdrs(&[
                ("Content-Type", "application/json; charset=utf-8"),
                ("Content-Length", "57"),
                ("Cache-Control", "no-store"),
                ("X-Content-Type-Options", "nosniff"),
            ])
        );
        assert!(!r.close);
        let head = json_response(200, &health_body(&status), true);
        assert!(head.body.is_empty());
        assert_eq!(header(&head, "Content-Length"), Some("57"));
        // Non-ASCII is not escaped; Content-Length counts UTF-8 bytes.
        let r = error_response(400, "Nothing to read \u{2014} the text is empty", Some(ReadErrorCode::EmptyText));
        assert_eq!(header(&r, "Content-Length"), Some(r.body.len().to_string().as_str()));
        assert!(body_text(&r).contains('\u{2014}'));
        // Key order ok, error, code; 408/413 close the connection, others do not.
        let r = error_response(504, "Timed out waiting for the app", Some(ReadErrorCode::Unknown));
        assert_eq!(body_text(&r), r#"{"ok":false,"error":"Timed out waiting for the app","code":"unknown"}"#);
        assert!(!r.close);
        assert!(error_response(408, "Request timed out", None).close);
        assert!(error_response(413, "Request body too large", Some(ReadErrorCode::TooLong)).close);
        assert_eq!(body_text(&error_response(408, "Request timed out", None)), r#"{"ok":false,"error":"Request timed out"}"#);
    }

    #[test]
    fn path_parser() {
        let p = |s: &str| parse_path(s);
        assert_eq!(p("/"), Some("/".into()));
        assert_eq!(p("/health"), Some("/health".into()));
        assert_eq!(p("/health/"), Some("/health".into()));
        assert_eq!(p("/health?x=/y"), Some("/health".into()));
        assert_eq!(p("/health#frag?x"), Some("/health".into()));
        assert_eq!(p("/a/b/../../health"), Some("/health".into()));
        assert_eq!(p("/a/%2e%2E/health"), Some("/health".into()));
        assert_eq!(p("/../health"), Some("/health".into()));
        assert_eq!(p("/health/.."), Some("/".into()));
        assert_eq!(p("*"), Some("/*".into()));
        assert_eq!(p("health"), Some("/health".into()));
        assert_eq!(p("//evil/health"), Some("/health".into()));
        assert_eq!(p("\\\\evil\\health"), Some("/health".into()));
        assert_eq!(p("//evil"), Some("/".into()));
        assert_eq!(p("//user:pw@evil:8080/health"), Some("/health".into()));
        assert_eq!(p("//[::1]:80/health"), Some("/health".into()));
        assert_eq!(p("https://x/health/"), Some("/health".into()));
        assert_eq!(p("https:///x/health"), Some("/health".into()));
        assert_eq!(p("http:/health"), Some("/health".into()));
        assert_eq!(p("file:///health"), Some("/health".into()));
        assert_eq!(p("foo://bar/health"), Some("/health".into()));
        assert_eq!(p("mailto:x"), Some("x".into()));
        assert_eq!(p("////"), None);
        assert_eq!(p("//"), None);
        assert_eq!(p("///"), None);
        assert_eq!(p("//@/health"), None);
        assert_eq!(p("https://"), None);
        assert_eq!(p("//[::1/health"), None);
        assert_eq!(p("//a<b/health"), None);
        assert_eq!(p("//evil:/health"), Some("/health".into()));
    }
}
