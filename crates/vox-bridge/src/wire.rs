//! HTTP/1.x framing for the bridge's own accept loop (private; integrations.md §1.4 step 0, §1.7; ARCHITECTURE.md §8
//! D-INT-16). Pure byte handling, unit-tested without sockets:
//!
//! - [`scan_head`]: the request head out of the bytes received so far, with Node's rules where the extension or the
//!   §1.10 vectors can tell the difference: only Node's upper-case method list (an unknown or lower-case method is
//!   refused as soon as it is visible → raw 400), a request-target in origin form (`/…`, `*`) or absolute form
//!   (`scheme://…`), a 16 KiB head cap (→ raw 431), header values decoded as Latin-1 (Node's `binary` decoding) and
//!   trimmed of SP/HTAB, `Content-Length` digits only (duplicates must agree), `Transfer-Encoding` ending in `chunked`
//!   and never together with `Content-Length` (request-smuggling guard, as llhttp), keep-alive per HTTP version and
//!   `Connection` tokens, and `Expect` (HTTP/1.1 only: `100-continue` → interim 100, anything else → 417).
//! - [`ChunkedDecoder`]: `Transfer-Encoding: chunked` bodies (extensions and trailers skipped).
//! - [`serialize`]: a pipeline [`HttpResponse`] on the wire, with `Connection: keep-alive` + `Keep-Alive: timeout=N`
//!   or `Connection: close`.
//! - The raw answers Node's `clientError` handler wrote: [`RAW_400`], [`RAW_431`].

use crate::pipeline::HttpResponse;

/// Malformed request, unknown or lower-case method, header timeout (integrations.md §1.7).
pub(crate) const RAW_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
/// Header block over the cap.
pub(crate) const RAW_431: &[u8] =
    b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
/// An HTTP/1.1 `Expect` other than `100-continue` (Node's default `checkExpectation` answer).
pub(crate) const RAW_417: &[u8] = b"HTTP/1.1 417 Expectation Failed\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
/// The interim answer to `Expect: 100-continue`, sent as soon as the head is in (before any check, like Node).
pub(crate) const CONTINUE_100: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Node's `http.METHODS` (llhttp request methods). Anything else, including lower-case spellings, is a raw 400.
const METHODS: [&str; 35] = [
    "ACL",
    "BIND",
    "CHECKOUT",
    "CONNECT",
    "COPY",
    "DELETE",
    "GET",
    "HEAD",
    "LINK",
    "LOCK",
    "M-SEARCH",
    "MERGE",
    "MKACTIVITY",
    "MKCALENDAR",
    "MKCOL",
    "MOVE",
    "NOTIFY",
    "OPTIONS",
    "PATCH",
    "POST",
    "PROPFIND",
    "PROPPATCH",
    "PURGE",
    "PUT",
    "QUERY",
    "REBIND",
    "REPORT",
    "SEARCH",
    "SOURCE",
    "SUBSCRIBE",
    "TRACE",
    "UNBIND",
    "UNLINK",
    "UNLOCK",
    "UNSUBSCRIBE",
];

/// How the request body is delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// No `Content-Length` and no `Transfer-Encoding`: no body (Node semantics for requests).
    Empty,
    Length(u64),
    Chunked,
}

impl Framing {
    /// True when bytes of a body follow the head (they must be read or the connection closed).
    pub(crate) fn has_body(self) -> bool {
        !matches!(self, Framing::Empty | Framing::Length(0))
    }
}

/// What the client's `Expect` header asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    None,
    /// `100-continue` on HTTP/1.1: send [`CONTINUE_100`] right away.
    Continue,
    /// Any other expectation on HTTP/1.1: answer [`RAW_417`].
    Unsupported,
}

/// A parsed request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    pub method: String,
    pub target: String,
    /// Header names as received, values Latin-1 decoded and trimmed of SP/HTAB, in arrival order.
    pub headers: Vec<(String, String)>,
    pub framing: Framing,
    /// The connection may stay open after the response (HTTP/1.1 without `Connection: close`, or HTTP/1.0 with
    /// `Connection: keep-alive`).
    pub keep_alive: bool,
    pub expect: Expect,
}

/// The result of scanning the bytes received so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scan {
    /// The head is not complete yet.
    Partial,
    /// A complete head and its length in bytes (the body, if any, starts there).
    Complete(Head, usize),
    /// Malformed → [`RAW_400`].
    Bad,
    /// Head over the cap → [`RAW_431`].
    TooLarge,
}

/// Scans `buf` (everything received on the connection that is not consumed yet) for a request head.
pub(crate) fn scan_head(buf: &[u8], max_header_bytes: usize) -> Scan {
    if !method_plausible(buf) {
        return Scan::Bad;
    }
    // Every header line is at least 4 bytes (`a:\r\n`), so this many slots always fit a head within the cap.
    let max_headers = max_header_bytes / 4 + 2;
    let mut parsed = parse_raw(buf, 64);
    if matches!(parsed, Err(httparse::Error::TooManyHeaders)) {
        parsed = parse_raw(buf, max_headers);
    }
    match parsed {
        Ok(None) if buf.len() > max_header_bytes => Scan::TooLarge,
        Ok(None) => Scan::Partial,
        Ok(Some((_, len))) if len > max_header_bytes => Scan::TooLarge,
        Ok(Some((raw, len))) => match build_head(raw) {
            Some(head) => Scan::Complete(head, len),
            None => Scan::Bad,
        },
        Err(_) if buf.len() > max_header_bytes => Scan::TooLarge,
        Err(_) => Scan::Bad,
    }
}

/// True once `buf` holds a byte of a request (leading empty lines are skipped, like llhttp).
pub(crate) fn request_started(buf: &[u8]) -> bool {
    buf.iter().any(|b| *b != b'\r' && *b != b'\n')
}

/// False as soon as the method token cannot be one of [`METHODS`] (Node rejects `HEL…` at the third byte).
fn method_plausible(buf: &[u8]) -> bool {
    let start = buf.iter().position(|b| *b != b'\r' && *b != b'\n').unwrap_or(buf.len());
    let rest = &buf[start..];
    match rest.iter().position(|b| *b == b' ') {
        Some(end) => METHODS.iter().any(|m| m.as_bytes() == &rest[..end]),
        None => rest.is_empty() || METHODS.iter().any(|m| m.as_bytes().starts_with(rest)),
    }
}

struct RawHead {
    method: String,
    target: String,
    minor_version: u8,
    headers: Vec<(String, String)>,
}

fn parse_raw(buf: &[u8], slots: usize) -> Result<Option<(RawHead, usize)>, httparse::Error> {
    let mut headers = vec![httparse::EMPTY_HEADER; slots];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf)? {
        httparse::Status::Partial => Ok(None),
        httparse::Status::Complete(len) => {
            let raw = RawHead {
                method: req.method.unwrap_or_default().to_string(),
                target: req.path.unwrap_or_default().to_string(),
                minor_version: req.version.unwrap_or(1),
                headers: req.headers.iter().map(|h| (h.name.to_string(), header_value(h.value))).collect(),
            };
            Ok(Some((raw, len)))
        }
    }
}

/// Node decodes header values as Latin-1 (one char per byte) and strips the surrounding SP/HTAB.
fn header_value(bytes: &[u8]) -> String {
    let s: String = bytes.iter().map(|b| char::from(*b)).collect();
    s.trim_matches([' ', '\t']).to_string()
}

fn build_head(raw: RawHead) -> Option<Head> {
    if !METHODS.contains(&raw.method.as_str()) {
        return None;
    }
    if raw.method != "CONNECT" && !target_acceptable(&raw.target) {
        return None;
    }
    let framing = framing(&raw.headers)?;
    let keep_alive = keep_alive(raw.minor_version, &raw.headers);
    let expect = expect(raw.minor_version, &raw.headers);
    Some(Head { method: raw.method, target: raw.target, headers: raw.headers, framing, keep_alive, expect })
}

/// llhttp's request-target forms: origin form (`/…`), `*`, or absolute form `scheme://…`.
fn target_acceptable(target: &str) -> bool {
    let b = target.as_bytes();
    match b.first() {
        Some(b'/') | Some(b'*') => true,
        Some(c) if c.is_ascii_alphabetic() => {
            let end = b.iter().position(|c| !(c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')));
            matches!(end, Some(i) if b[i..].starts_with(b"://"))
        }
        _ => false,
    }
}

fn values<'a>(headers: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn tokens<'a>(headers: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    values(headers, name).flat_map(|v| v.split(',')).map(|t| t.trim_matches([' ', '\t'])).filter(|t| !t.is_empty())
}

/// `None` = invalid framing (raw 400).
fn framing(headers: &[(String, String)]) -> Option<Framing> {
    let has_te = values(headers, "transfer-encoding").next().is_some();
    let lengths: Vec<&str> = values(headers, "content-length").collect();
    if has_te {
        if !lengths.is_empty() {
            return None;
        }
        let last = tokens(headers, "transfer-encoding").last()?;
        return last.eq_ignore_ascii_case("chunked").then_some(Framing::Chunked);
    }
    let mut declared: Option<u64> = None;
    for v in lengths {
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: u64 = v.parse().ok()?;
        match declared {
            Some(d) if d != n => return None,
            _ => declared = Some(n),
        }
    }
    Some(declared.map_or(Framing::Empty, Framing::Length))
}

fn keep_alive(minor_version: u8, headers: &[(String, String)]) -> bool {
    let has = |token: &str| tokens(headers, "connection").any(|t| t.eq_ignore_ascii_case(token));
    if minor_version >= 1 {
        !has("close")
    } else {
        has("keep-alive")
    }
}

fn expect(minor_version: u8, headers: &[(String, String)]) -> Expect {
    if minor_version != 1 {
        return Expect::None;
    }
    let all: Vec<&str> = values(headers, "expect").collect();
    if all.is_empty() {
        return Expect::None;
    }
    if is_100_continue(&all.join(", ")) {
        Expect::Continue
    } else {
        Expect::Unsupported
    }
}

/// Node's `/(?:^|\W)100-continue(?:$|\W)/i`.
fn is_100_continue(value: &str) -> bool {
    const NEEDLE: &[u8] = b"100-continue";
    let b = value.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    (0..b.len()).any(|i| {
        b.len() - i >= NEEDLE.len()
            && b[i..i + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE)
            && (i == 0 || !word(b[i - 1]))
            && b.get(i + NEEDLE.len()).is_none_or(|c| !word(*c))
    })
}

/// Node's `http.STATUS_CODES` reason phrases for the statuses the bridge sends.
pub(crate) fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// The response bytes: status line, the pipeline's headers, then `Connection: keep-alive` + `Keep-Alive:
/// timeout=<secs>` when the connection stays open, else `Connection: close`; then the body.
pub(crate) fn serialize(resp: &HttpResponse, keep_alive: bool, keep_alive_secs: u64) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    for (name, value) in &resp.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if keep_alive {
        head.push_str(&format!("Connection: keep-alive\r\nKeep-Alive: timeout={keep_alive_secs}\r\n"));
    } else {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&resp.body);
    out
}

/// A malformed chunked body (raw 400).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    /// Hex digits of the chunk size.
    Size,
    /// A chunk extension (`;name=value`) or whitespace after the size, up to the line end.
    SizeExt,
    /// After the size line's CR.
    SizeLf,
    Data,
    DataCr,
    DataLf,
    /// Start of a trailer line (an empty one ends the body).
    TrailerStart,
    TrailerLine,
    TrailerLf,
    Done,
}

/// Incremental `Transfer-Encoding: chunked` decoder. Accepts CRLF or bare LF line ends.
#[derive(Debug, Clone)]
pub(crate) struct ChunkedDecoder {
    state: ChunkState,
    size: u64,
    digits: u32,
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        Self { state: ChunkState::Size, size: 0, digits: 0 }
    }
}

impl ChunkedDecoder {
    pub(crate) fn is_done(&self) -> bool {
        self.state == ChunkState::Done
    }

    /// Consumes bytes from `input`, appending chunk data to `out`. Returns how many bytes were consumed; stops at the
    /// end of the body (the rest of `input` belongs to the next request).
    pub(crate) fn decode(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, ChunkError> {
        let mut i = 0;
        while i < input.len() && self.state != ChunkState::Done {
            let c = input[i];
            match self.state {
                ChunkState::Size => match c {
                    b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                        self.digits += 1;
                        if self.digits > 16 {
                            return Err(ChunkError);
                        }
                        let d = (c as char).to_digit(16).ok_or(ChunkError)?;
                        self.size = self.size.checked_mul(16).and_then(|s| s.checked_add(u64::from(d))).ok_or(ChunkError)?;
                    }
                    b';' | b' ' | b'\t' if self.digits > 0 => self.state = ChunkState::SizeExt,
                    b'\r' if self.digits > 0 => self.state = ChunkState::SizeLf,
                    b'\n' if self.digits > 0 => self.end_size_line(),
                    _ => return Err(ChunkError),
                },
                ChunkState::SizeExt => match c {
                    b'\r' => self.state = ChunkState::SizeLf,
                    b'\n' => self.end_size_line(),
                    _ => {}
                },
                ChunkState::SizeLf => match c {
                    b'\n' => self.end_size_line(),
                    _ => return Err(ChunkError),
                },
                ChunkState::Data => {
                    let avail = (input.len() - i) as u64;
                    let take = avail.min(self.size) as usize;
                    out.extend_from_slice(&input[i..i + take]);
                    self.size -= take as u64;
                    i += take;
                    if self.size == 0 {
                        self.state = ChunkState::DataCr;
                    }
                    continue;
                }
                ChunkState::DataCr => match c {
                    b'\r' => self.state = ChunkState::DataLf,
                    b'\n' => self.next_chunk(),
                    _ => return Err(ChunkError),
                },
                ChunkState::DataLf => match c {
                    b'\n' => self.next_chunk(),
                    _ => return Err(ChunkError),
                },
                ChunkState::TrailerStart => match c {
                    b'\r' => self.state = ChunkState::TrailerLf,
                    b'\n' => self.state = ChunkState::Done,
                    _ => self.state = ChunkState::TrailerLine,
                },
                ChunkState::TrailerLine => {
                    if c == b'\n' {
                        self.state = ChunkState::TrailerStart;
                    }
                }
                ChunkState::TrailerLf => match c {
                    b'\n' => self.state = ChunkState::Done,
                    _ => return Err(ChunkError),
                },
                ChunkState::Done => {}
            }
            i += 1;
        }
        Ok(i)
    }

    fn end_size_line(&mut self) {
        self.state = if self.size == 0 { ChunkState::TrailerStart } else { ChunkState::Data };
    }

    fn next_chunk(&mut self) {
        self.state = ChunkState::Size;
        self.size = 0;
        self.digits = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 16 * 1024;

    fn complete(raw: &str) -> (Head, usize) {
        match scan_head(raw.as_bytes(), MAX) {
            Scan::Complete(h, n) => (h, n),
            other => panic!("expected a complete head for {raw:?}, got {other:?}"),
        }
    }

    #[test]
    fn complete_head_and_body_offset() {
        let raw = "POST /read HTTP/1.1\r\nHost: 127.0.0.1:1\r\nContent-Length: 2\r\n\r\n{}";
        let (h, n) = complete(raw);
        assert_eq!(n, raw.len() - 2);
        assert_eq!(h.method, "POST");
        assert_eq!(h.target, "/read");
        assert_eq!(h.framing, Framing::Length(2));
        assert!(h.keep_alive);
        assert_eq!(h.expect, Expect::None);
        assert_eq!(h.headers[0], ("Host".to_string(), "127.0.0.1:1".to_string()));
    }

    #[test]
    fn partial_heads_wait_for_more() {
        for raw in ["", "G", "GE", "GET", "GET ", "GET /health", "GET /health HTTP/1.1\r\n", "GET /health HTTP/1.1\r\nHost: x"] {
            assert_eq!(scan_head(raw.as_bytes(), MAX), Scan::Partial, "{raw:?}");
        }
        // Leading empty lines are skipped, like llhttp.
        assert_eq!(scan_head(b"\r\n\r\nGE", MAX), Scan::Partial);
        let (h, _) = complete("\r\nGET / HTTP/1.1\r\n\r\n");
        assert_eq!(h.method, "GET");
    }

    #[test]
    fn request_start_ignores_empty_lines() {
        assert!(!request_started(b""));
        assert!(!request_started(b"\r\n\n\r"));
        assert!(request_started(b"\r\nG"));
    }

    #[test]
    fn methods_follow_node() {
        // `HELLO` is refused as soon as it cannot be a method (Node: at `HEL`).
        assert_eq!(scan_head(b"HEL", MAX), Scan::Bad);
        assert_eq!(scan_head(b"HELLO\r\n\r\n", MAX), Scan::Bad);
        // Lower-case and unknown methods: raw 400.
        assert_eq!(scan_head(b"post /read HTTP/1.1\r\n\r\n", MAX), Scan::Bad);
        assert_eq!(scan_head(b"get", MAX), Scan::Bad);
        assert_eq!(scan_head(b"FOO / HTTP/1.1\r\n\r\n", MAX), Scan::Bad);
        assert_eq!(scan_head(b"GETX / HTTP/1.1\r\n\r\n", MAX), Scan::Bad);
        // Every Node method is accepted (routing decides later).
        for m in METHODS {
            let raw = format!("{m} /x HTTP/1.1\r\n\r\n");
            let (h, _) = complete(&raw);
            assert_eq!(h.method, m);
        }
    }

    #[test]
    fn malformed_heads_are_bad() {
        for raw in [
            "GET /health HTTP/2.0\r\n\r\n",
            "GET /health HTTP/1.1\r\nBad Header\r\n\r\n",
            "GET /health HTTP/1.1\r\nHost : x\r\n\r\n",
            "GET  HTTP/1.1\r\n\r\n",
            "GET health HTTP/1.1\r\n\r\n",
            "GET http:health HTTP/1.1\r\n\r\n",
            "GET 1://x/ HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(scan_head(raw.as_bytes(), MAX), Scan::Bad, "{raw:?}");
        }
    }

    #[test]
    fn request_target_forms() {
        for target in ["/health", "*", "//evil/health", "http://evil/health", "HTTPS://x/health", "/health\\"] {
            let raw = format!("GET {target} HTTP/1.1\r\n\r\n");
            let (h, _) = complete(&raw);
            assert_eq!(h.target, target);
        }
        // CONNECT carries an authority; it is accepted here and closed by the server.
        let (h, _) = complete("CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert_eq!(h.method, "CONNECT");
    }

    #[test]
    fn header_cap() {
        let big = format!("GET /health HTTP/1.1\r\nX-Big: {}\r\n\r\n", "a".repeat(20_000));
        assert_eq!(scan_head(big.as_bytes(), MAX), Scan::TooLarge);
        // Incomplete but already over the cap.
        assert_eq!(scan_head(&big.as_bytes()[..17_000], MAX), Scan::TooLarge);
        // A head just within the cap passes; a body after it does not count.
        let fill = MAX - "GET / HTTP/1.1\r\nX: \r\n\r\n".len();
        let ok = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(fill));
        assert_eq!(ok.len(), MAX);
        let mut with_body = ok.clone().into_bytes();
        with_body.extend_from_slice(&[b'x'; 5000]);
        assert!(matches!(scan_head(&with_body, MAX), Scan::Complete(_, n) if n == MAX));
        let over = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(fill + 1));
        assert_eq!(scan_head(over.as_bytes(), MAX), Scan::TooLarge);
        // Many small headers (more than the first parse attempt's slots) still parse.
        let mut many = String::from("GET / HTTP/1.1\r\n");
        for i in 0..500 {
            many.push_str(&format!("X-{i}: v\r\n"));
        }
        many.push_str("\r\n");
        let (h, _) = complete(&many);
        assert_eq!(h.headers.len(), 500);
    }

    #[test]
    fn header_values_are_latin1_and_trimmed() {
        let raw = b"GET / HTTP/1.1\r\nHost: \t127.0.0.1:5\xA0 \r\nX-Empty:\r\n\r\n";
        let Scan::Complete(h, _) = scan_head(raw, MAX) else { panic!("complete") };
        assert_eq!(h.headers[0].1, "127.0.0.1:5\u{A0}");
        assert_eq!(h.headers[1], ("X-Empty".to_string(), String::new()));
    }

    #[test]
    fn framing_rules() {
        let f = |extra: &str| match scan_head(format!("POST /read HTTP/1.1\r\n{extra}\r\n").as_bytes(), MAX) {
            Scan::Complete(h, _) => Some(h.framing),
            Scan::Bad => None,
            other => panic!("{other:?}"),
        };
        assert_eq!(f(""), Some(Framing::Empty));
        assert_eq!(f("Content-Length: 0\r\n"), Some(Framing::Length(0)));
        assert_eq!(f("Content-Length: 3000000\r\n"), Some(Framing::Length(3_000_000)));
        assert_eq!(f("Content-Length: 5\r\ncontent-length: 5\r\n"), Some(Framing::Length(5)));
        assert_eq!(f("Content-Length: 5\r\nContent-Length: 6\r\n"), None);
        assert_eq!(f("Content-Length: abc\r\n"), None);
        assert_eq!(f("Content-Length: -1\r\n"), None);
        assert_eq!(f("Content-Length: 1, 1\r\n"), None);
        assert_eq!(f("Content-Length: 99999999999999999999999\r\n"), None);
        assert_eq!(f("Transfer-Encoding: chunked\r\n"), Some(Framing::Chunked));
        assert_eq!(f("Transfer-Encoding: gzip, Chunked\r\n"), Some(Framing::Chunked));
        assert_eq!(f("Transfer-Encoding: chunked, gzip\r\n"), None);
        assert_eq!(f("Transfer-Encoding: chunked\r\nContent-Length: 5\r\n"), None);
        assert!(!Framing::Empty.has_body());
        assert!(!Framing::Length(0).has_body());
        assert!(Framing::Length(1).has_body());
        assert!(Framing::Chunked.has_body());
    }

    #[test]
    fn keep_alive_rules() {
        let k = |raw: &str| complete(raw).0.keep_alive;
        assert!(k("GET / HTTP/1.1\r\n\r\n"));
        assert!(!k("GET / HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(!k("GET / HTTP/1.1\r\nConnection: Upgrade, Close\r\n\r\n"));
        assert!(!k("GET / HTTP/1.0\r\n\r\n"));
        assert!(k("GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n"));
    }

    #[test]
    fn expect_rules() {
        let e = |raw: &str| complete(raw).0.expect;
        assert_eq!(e("POST / HTTP/1.1\r\nExpect: 100-continue\r\n\r\n"), Expect::Continue);
        assert_eq!(e("POST / HTTP/1.1\r\nExpect: 100-Continue\r\n\r\n"), Expect::Continue);
        assert_eq!(e("POST / HTTP/1.1\r\nExpect: x, 100-continue\r\n\r\n"), Expect::Continue);
        assert_eq!(e("POST / HTTP/1.1\r\nExpect: 100-continued\r\n\r\n"), Expect::Unsupported);
        assert_eq!(e("POST / HTTP/1.1\r\nExpect: something\r\n\r\n"), Expect::Unsupported);
        assert_eq!(e("POST / HTTP/1.0\r\nExpect: something\r\n\r\n"), Expect::None);
        assert_eq!(e("POST / HTTP/1.1\r\n\r\n"), Expect::None);
    }

    #[test]
    fn serialize_responses() {
        let resp = HttpResponse {
            status: 405,
            headers: vec![("Allow".into(), "GET".into())],
            body: b"{}".to_vec(),
            close: false,
        };
        assert_eq!(
            String::from_utf8(serialize(&resp, true, 5)).unwrap(),
            "HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\n\r\n{}"
        );
        assert_eq!(
            String::from_utf8(serialize(&resp, false, 5)).unwrap(),
            "HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nConnection: close\r\n\r\n{}"
        );
        assert_eq!(reason(413), "Payload Too Large");
        assert_eq!(reason(408), "Request Timeout");
        assert_eq!(reason(504), "Gateway Timeout");
        assert_eq!(reason(299), "Unknown");
    }

    fn decode_all(input: &[u8]) -> Result<(Vec<u8>, usize, bool), ChunkError> {
        let mut d = ChunkedDecoder::default();
        let mut out = Vec::new();
        let n = d.decode(input, &mut out)?;
        Ok((out, n, d.is_done()))
    }

    #[test]
    fn chunked_bodies() {
        let body = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\n\r\n";
        let (out, n, done) = decode_all(body).unwrap();
        assert_eq!(out, b"Wikipedia in\r\n\r\nchunks.");
        assert_eq!(n, body.len());
        assert!(done);
        // Trailers are skipped; bytes after the body are left for the next request.
        let body = b"3\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\nGET";
        let (out, n, done) = decode_all(body).unwrap();
        assert_eq!(out, b"abc");
        assert_eq!(n, body.len() - 3);
        assert!(done);
        // Bare LF line ends.
        let (out, _, done) = decode_all(b"2\nhi\n0\n\n").unwrap();
        assert_eq!(out, b"hi");
        assert!(done);
        // Malformed.
        assert_eq!(decode_all(b"x\r\n"), Err(ChunkError));
        assert_eq!(decode_all(b"\r\n"), Err(ChunkError));
        assert_eq!(decode_all(b"2\r\nhiX"), Err(ChunkError));
        assert_eq!(decode_all(b"11111111111111111\r\n"), Err(ChunkError));
    }

    #[test]
    fn chunked_bodies_byte_by_byte() {
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut d = ChunkedDecoder::default();
        let mut out = Vec::new();
        for b in body.iter() {
            assert_eq!(d.decode(std::slice::from_ref(b), &mut out), Ok(1));
        }
        assert!(d.is_done());
        assert_eq!(out, b"Wikipedia");
        // Partial input: nothing lost between calls.
        let mut d = ChunkedDecoder::default();
        let mut out = Vec::new();
        assert_eq!(d.decode(b"a\r\n01234", &mut out), Ok(8));
        assert!(!d.is_done());
        assert_eq!(d.decode(b"56789\r\n0\r\n\r\n", &mut out), Ok(12));
        assert!(d.is_done());
        assert_eq!(out, b"0123456789");
    }

    #[test]
    fn hundred_continue_matcher() {
        assert!(is_100_continue("100-continue"));
        assert!(is_100_continue("100-CONTINUE"));
        assert!(is_100_continue(" 100-continue "));
        assert!(is_100_continue("a,100-continue"));
        assert!(!is_100_continue("x100-continue"));
        assert!(!is_100_continue("100-continue_"));
        assert!(!is_100_continue("100-contin"));
        assert!(!is_100_continue(""));
    }
}
