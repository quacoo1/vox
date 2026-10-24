//! Socket tests (ARCHITECTURE.md §9): the integrations.md §1.10 wire vectors and the §1.4/§1.6 inline examples
//! against a real listener on `127.0.0.1` (ephemeral ports, scaled-down limits, a fake [`BridgeHandler`]), plus the
//! connection-level rows (raw 400/431, 408 stall, keep-alive close, 64-connection cap, CONNECT, `Expect`, half-closed
//! clients, EOF right after a closing answer, no body on any HEAD answer) and the
//! lifecycle (busy port, restart, stop with open connections, immediate rebind after stop and after drop, stop while
//! `read` is pending). Loopback only: they run in the normal `cargo test -p vox-bridge`.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde_json::{json, Value};
use vox_core::player::{PlayerCommand, SkipDirection, SkipUnit};
use vox_core::read::{BridgeReadRequest, ReadErrorCode, ReadResult};

use crate::server::{BridgeHandler, BridgeLimits, BridgeServer, BridgeStatus};
use crate::wire::{RAW_400, RAW_431};

const READ_ID: &str = "0b5f8a52-3c1e-4d6a-9f3b-2a7c1e9d4b10";
const HEALTH: &str = r#"{"ok":true,"app":"Vox","version":"0.1.0","playing":false}"#;

#[derive(Clone, Default)]
enum Mode {
    /// `{ok:true, readId}` at once.
    #[default]
    Started,
    Answer(ReadResult),
    /// `None` at once (→ 504).
    NoAnswer,
    /// Waits out the deadline, then `None`.
    UntilDeadline,
    /// Waits until [`Fake::release`] (ignores the deadline).
    Gate,
    Panic,
}

#[derive(Default)]
struct Fake {
    reads: Mutex<Vec<BridgeReadRequest>>,
    commands: Mutex<Vec<PlayerCommand>>,
    playing: AtomicBool,
    /// `status()` panics while set.
    status_panics: AtomicBool,
    mode: Mutex<Mode>,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    released: Mutex<bool>,
    gate: Condvar,
}

impl Fake {
    fn with_mode(mode: Mode) -> Arc<Self> {
        let fake = Self::default();
        *fake.mode.lock() = mode;
        Arc::new(fake)
    }

    fn release(&self) {
        *self.released.lock() = true;
        self.gate.notify_all();
    }

    fn last_read(&self) -> BridgeReadRequest {
        self.reads.lock().last().cloned().expect("the handler got a read")
    }
}

impl BridgeHandler for Fake {
    fn read(&self, req: BridgeReadRequest, deadline: Instant) -> Option<ReadResult> {
        self.reads.lock().push(req);
        if let Some(tx) = self.entered.lock().as_ref() {
            let _ = tx.send(());
        }
        let mode = self.mode.lock().clone();
        match mode {
            Mode::Started => Some(ReadResult::started(READ_ID)),
            Mode::Answer(result) => Some(result),
            Mode::NoAnswer => None,
            Mode::UntilDeadline => {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                None
            }
            Mode::Gate => {
                let mut released = self.released.lock();
                while !*released {
                    self.gate.wait(&mut released);
                }
                Some(ReadResult::started(READ_ID))
            }
            Mode::Panic => panic!("fake handler failure (expected in this test)"),
        }
    }

    fn command(&self, cmd: PlayerCommand) {
        self.commands.lock().push(cmd);
    }

    fn status(&self) -> BridgeStatus {
        if self.status_panics.load(Ordering::SeqCst) {
            panic!("fake status failure (expected in this test)");
        }
        BridgeStatus { app_name: "Vox".into(), version: "0.1.0".into(), playing: self.playing.load(Ordering::SeqCst) }
    }
}

fn limits() -> BridgeLimits {
    BridgeLimits {
        headers: Duration::from_secs(3),
        keep_alive: Duration::from_secs(3),
        body: Duration::from_secs(3),
        handler: Duration::from_secs(3),
        idle: Duration::from_secs(5),
        max_connections: 64,
        max_header_bytes: 16 * 1024,
    }
}

fn serve(fake: &Arc<Fake>, limits: BridgeLimits) -> (BridgeServer, u16) {
    let server = BridgeServer::with_limits(fake.clone(), limits);
    let started = server.start(0);
    assert!(started.ok, "{started:?}");
    assert_ne!(started.port, 0);
    assert!(server.running());
    assert_eq!(server.port(), started.port);
    (server, started.port)
}

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("UTF-8 body")
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }
}

struct Client {
    s: TcpStream,
    buf: Vec<u8>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Client {
    fn new(port: u16) -> Self {
        let s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
        let _ = s.set_nodelay(true);
        Self { s, buf: Vec::new() }
    }

    fn send(&mut self, data: &[u8]) {
        self.s.write_all(data).expect("send");
    }

    fn fill(&mut self) -> usize {
        let mut chunk = [0u8; 16 * 1024];
        let n = self.s.read(&mut chunk).expect("read");
        self.buf.extend_from_slice(&chunk[..n]);
        n
    }

    /// One response (`head_request`: no body follows whatever Content-Length says).
    fn response(&mut self, head_request: bool) -> Resp {
        let end = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i + 4;
            }
            assert!(self.fill() > 0, "connection closed before a response; got {:?}", String::from_utf8_lossy(&self.buf));
        };
        let head = String::from_utf8(self.buf.drain(..end).collect()).expect("ASCII head");
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        assert!(status_line.starts_with("HTTP/1.1 "), "{status_line}");
        let status: u16 = status_line[9..12].parse().expect("status");
        let headers: Vec<(String, String)> = lines
            .filter(|l| !l.is_empty())
            .map(|l| {
                let (n, v) = l.split_once(": ").expect("header line");
                (n.to_string(), v.to_string())
            })
            .collect();
        let resp = Resp { status, headers, body: Vec::new() };
        let len = if head_request || status == 100 {
            0
        } else {
            resp.header("Content-Length").map_or(0, |v| v.parse().expect("length"))
        };
        while self.buf.len() < len {
            assert!(self.fill() > 0, "connection closed inside a body");
        }
        Resp { body: self.buf.drain(..len).collect(), ..resp }
    }

    /// Everything the server still sends until its FIN. `Err` (with what arrived and the error) on a reset, or when
    /// nothing more arrived for `within`: unlike [`Client::until_closed`], only a clean EOF passes.
    fn read_to_eof(&mut self, within: Duration) -> Result<Vec<u8>, (Vec<u8>, ErrorKind)> {
        self.s.set_read_timeout(Some(within)).expect("timeout");
        loop {
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => return Ok(std::mem::take(&mut self.buf)),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) => return Err((std::mem::take(&mut self.buf), e.kind())),
            }
        }
    }

    /// Exactly one response followed by a clean EOF (no reset).
    fn answer_then_eof(&mut self, head_request: bool, within: Duration) -> Resp {
        let all = match self.read_to_eof(within) {
            Ok(all) => all,
            Err((got, kind)) => panic!("{kind:?} instead of EOF after {:?}", String::from_utf8_lossy(&got)),
        };
        self.buf = all;
        let r = self.response(head_request);
        assert!(self.buf.is_empty(), "bytes after the answer: {:?}", String::from_utf8_lossy(&self.buf));
        r
    }

    fn half_close(&self) {
        self.s.shutdown(Shutdown::Write).expect("half-close");
    }

    /// Everything the server still sends until it closes the connection, or `None` if it is still open after
    /// `within`.
    fn until_closed(&mut self, within: Duration) -> Option<Vec<u8>> {
        self.s.set_read_timeout(Some(within)).expect("timeout");
        let started = Instant::now();
        loop {
            let mut chunk = [0u8; 4096];
            match self.s.read(&mut chunk) {
                Ok(0) => return Some(std::mem::take(&mut self.buf)),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if started.elapsed() >= within {
                        return None;
                    }
                }
                Err(_) => return Some(std::mem::take(&mut self.buf)),
            }
        }
    }
}

fn get(path: &str, port: u16, extra: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}\r\n").into_bytes()
}

const CLIENT: &str = "x-vox-client: browser-extension\r\nContent-Type: application/json\r\n";

fn post(path: &str, port: u16, extra: &str, body: &[u8]) -> Vec<u8> {
    let mut out =
        format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Content-Length: {}\r\n\r\n", body.len())
            .into_bytes();
    out.extend_from_slice(body);
    out
}

/// One request on a fresh connection.
fn roundtrip(port: u16, raw: &[u8]) -> Resp {
    let head_request = raw.starts_with(b"HEAD ");
    let mut c = Client::new(port);
    c.send(raw);
    c.response(head_request)
}

fn assert_json_headers(r: &Resp) {
    assert_eq!(r.header("Content-Type"), Some("application/json; charset=utf-8"));
    assert_eq!(r.header("Cache-Control"), Some("no-store"));
    assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"));
    assert!(r.headers.iter().all(|(n, _)| !n.to_ascii_lowercase().starts_with("access-control")), "{:?}", r.headers);
}

fn expect(r: &Resp, status: u16, body: &str) {
    assert_eq!((r.status, r.text().as_str()), (status, body), "{:?}", r.headers);
    assert_json_headers(r);
    assert_eq!(r.header("Content-Length"), Some(body.len().to_string().as_str()));
}

#[test]
fn wire_vectors_health_and_guards() {
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, limits());

    let r = roundtrip(p, &get("/health", p, ""));
    expect(&r, 200, HEALTH);
    assert_eq!(r.header("Connection"), Some("keep-alive"));
    assert_eq!(r.header("Keep-Alive"), Some("timeout=3"));

    let r = roundtrip(p, format!("HEAD /health HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n").as_bytes());
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    assert_eq!(r.header("Content-Length"), Some("57"));
    assert_json_headers(&r);

    for path in ["/health/?x=1", "/health//", "/health#x", "/health\\", "//evil/health", "http://evil/health"] {
        expect(&roundtrip(p, &get(path, p, "")), 200, HEALTH);
    }
    for path in ["/nope", "/%68ealth", "/health%2F", "/HEALTH"] {
        expect(&roundtrip(p, &get(path, p, "")), 404, r#"{"ok":false,"error":"Not found"}"#);
    }
    for path in ["//", "///"] {
        expect(&roundtrip(p, &get(path, p, "")), 400, r#"{"ok":false,"error":"Bad URL"}"#);
    }
    let r = roundtrip(p, format!("HEAD /nope HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n").as_bytes());
    assert_eq!((r.status, r.body.len()), (404, 0));
    assert_eq!(r.header("Content-Length"), Some("32"));

    for method in ["POST", "PATCH"] {
        let r = roundtrip(p, format!("{method} /health HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n").as_bytes());
        expect(&r, 405, r#"{"ok":false,"error":"Method not allowed"}"#);
        assert_eq!(r.header("Allow"), Some("GET"));
    }
    let r = roundtrip(p, &get("/read", p, ""));
    expect(&r, 405, r#"{"ok":false,"error":"Method not allowed"}"#);
    assert_eq!(r.header("Allow"), Some("POST"));

    let r = roundtrip(
        p,
        format!("OPTIONS /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: https://evil.com\r\n\r\n").as_bytes(),
    );
    expect(&r, 403, r#"{"ok":false,"error":"Forbidden"}"#);

    let forbidden_host = r#"{"ok":false,"error":"Forbidden host"}"#;
    for host in [format!("evil.com:{p}"), "127.0.0.1".to_string(), format!("localhost.:{p}"), format!("[::1]:{p}")] {
        let r = roundtrip(p, format!("GET /health HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes());
        expect(&r, 403, forbidden_host);
    }
    let r = roundtrip(p, format!("GET /health HTTP/1.1\r\nHost: LOCALHOST:{p}\r\n\r\n").as_bytes());
    expect(&r, 200, HEALTH);
    // HTTP/1.0 without Host.
    let r = roundtrip(p, b"GET /health HTTP/1.0\r\n\r\n");
    expect(&r, 403, forbidden_host);
    assert_eq!(r.header("Connection"), Some("close"));
    // Two Hosts: the first one decides.
    let r = roundtrip(p, format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nHost: evil:{p}\r\n\r\n").as_bytes());
    expect(&r, 200, HEALTH);

    let forbidden_origin = r#"{"ok":false,"error":"Forbidden origin"}"#;
    for origin in ["http://x", "null"] {
        expect(&roundtrip(p, &get("/health", p, &format!("Origin: {origin}\r\n"))), 403, forbidden_origin);
    }
    expect(&roundtrip(p, &get("/health", p, "Origin: chrome-extension://abc\r\n")), 200, HEALTH);
    let both = "Origin: chrome-extension://a\r\nOrigin: http://x\r\n";
    expect(&roundtrip(p, &get("/health", p, both)), 200, HEALTH);
    let both = "Origin: http://x\r\nOrigin: chrome-extension://a\r\n";
    expect(&roundtrip(p, &get("/health", p, both)), 403, forbidden_origin);

    // `playing` comes from the handler.
    fake.playing.store(true, Ordering::SeqCst);
    expect(
        &roundtrip(p, &get("/health", p, "")),
        200,
        r#"{"ok":true,"app":"Vox","version":"0.1.0","playing":true}"#,
    );
}

#[test]
fn wire_vectors_post_headers_and_bodies() {
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, limits());
    let forbidden = r#"{"ok":false,"error":"Forbidden"}"#;

    let r = roundtrip(p, &post("/read", p, "Content-Type: application/json\r\n", br#"{"text":"hi"}"#));
    expect(&r, 403, forbidden);
    // An answer sent before the body was read closes the connection.
    assert_eq!(r.header("Connection"), Some("close"));
    expect(&roundtrip(p, &post("/read", p, "Content-Type: text/plain\r\n", b"x")), 403, forbidden);
    let dup = "x-vox-client: browser-extension\r\nx-vox-client: browser-extension\r\nContent-Type: application/json\r\n";
    expect(&roundtrip(p, &post("/read", p, dup, br#"{"text":"hi"}"#)), 403, forbidden);
    let wrong_case = "x-vox-client: Browser-Extension\r\nContent-Type: application/json\r\n";
    expect(&roundtrip(p, &post("/read", p, wrong_case, br#"{"text":"hi"}"#)), 403, forbidden);

    let unsupported = r#"{"ok":false,"error":"Content-Type must be application/json"}"#;
    for ct in ["text/plain", "application/vnd.api+json", "application/jsonx"] {
        let extra = format!("x-vox-client: browser-extension\r\nContent-Type: {ct}\r\n");
        expect(&roundtrip(p, &post("/command", p, &extra, br#"{"type":"toggle"}"#)), 415, unsupported);
    }
    let missing = roundtrip(p, &post("/read", p, "x-vox-client: browser-extension\r\n", br#"{"text":"hi"}"#));
    expect(&missing, 415, unsupported);
    for ct in ["Application/JSON; charset=utf-8", "application/json ;x", "application/json\t;x"] {
        let extra = format!("x-vox-client: browser-extension\r\nContent-Type: {ct}\r\n");
        assert_eq!(roundtrip(p, &post("/read", p, &extra, br#"{"text":"hi"}"#)).status, 200, "{ct}");
    }
    let dup_ct = "x-vox-client: browser-extension\r\nContent-Type: application/json\r\nContent-Type: text/plain\r\n";
    assert_eq!(roundtrip(p, &post("/read", p, dup_ct, br#"{"text":"hi"}"#)).status, 200);

    // Declared length over 2 MiB: 413 + Connection: close, without reading the body.
    let raw = format!(
        "POST /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Content-Length: 3000000\r\n\r\n"
    );
    let r = roundtrip(p, raw.as_bytes());
    expect(&r, 413, r#"{"ok":false,"error":"Request body too large","code":"too-long"}"#);
    assert_eq!(r.header("Connection"), Some("close"));

    let invalid = r#"{"ok":false,"error":"Invalid JSON"}"#;
    for body in [&b"{x"[..], b""] {
        expect(&roundtrip(p, &post("/read", p, CLIENT, body)), 400, invalid);
    }
    for body in [&b"[]"[..], b"null", b"\"x\"", b"1e400"] {
        expect(&roundtrip(p, &post("/read", p, CLIENT, body)), 400, r#"{"ok":false,"error":"Expected a JSON object"}"#);
    }
    let not_string = r#"{"ok":false,"error":"\"text\" must be a string","code":"empty-text"}"#;
    expect(&roundtrip(p, &post("/read", p, CLIENT, br#"{"text":1}"#)), 400, not_string);
    expect(&roundtrip(p, &post("/read", p, CLIENT, br#"{"text":1e400}"#)), 400, not_string);
    let empty = "{\"ok\":false,\"error\":\"Nothing to read \u{2014} the text is empty\",\"code\":\"empty-text\"}";
    expect(&roundtrip(p, &post("/read", p, CLIENT, b"{\"text\":\" \\n\\t \"}")), 400, empty);
    expect(&roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"\uFEFF\u00A0"}"#)), 400, empty);
    // Only the four accepted Content-Type variants reached the handler; no refused request did.
    assert_eq!(fake.reads.lock().len(), 4);
}

#[test]
fn wire_vectors_read_route() {
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, limits());
    let started = json!({ "ok": true, "readId": READ_ID });

    let mut body = b"\xEF\xBB\xBF".to_vec();
    body.extend_from_slice(br#"{"text":"hi"}"#);
    let r = roundtrip(p, &post("/read", p, CLIENT, &body));
    assert_eq!(r.status, 200);
    assert_json_headers(&r);
    assert_eq!(r.json(), started);
    assert_eq!(fake.last_read(), BridgeReadRequest { text: "hi".into(), url: None, title: None });

    let body = br#"{"text":"hi","url":"javascript:alert(1)","title":"  T  ","extra":1}"#;
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, body)).json(), started);
    assert_eq!(fake.last_read(), BridgeReadRequest { text: "hi".into(), url: None, title: Some("T".into()) });

    let body = br#"{"text":"a","title":"   ","url":"HTTPS://X"}"#;
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, body)).status, 200);
    assert_eq!(fake.last_read(), BridgeReadRequest { text: "a".into(), url: Some("HTTPS://X".into()), title: None });

    let body = br#"{"text":"a","url":"FILE:///c:/a.txt","title":5}"#;
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, body)).status, 200);
    assert_eq!(fake.last_read().url.as_deref(), Some("FILE:///c:/a.txt"));
    assert_eq!(fake.last_read().title, None);

    let body = br#"{"text":"a","url":5,"title":1e400}"#;
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, body)).status, 200);
    assert_eq!((fake.last_read().url, fake.last_read().title), (None, None));

    // Lone surrogate escapes (the extension's UTF-16 cuts) → U+FFFD; lossy UTF-8.
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"a\ud83d","title":"\udc00x"}"#)).status, 200);
    assert_eq!(fake.last_read().text, "a\u{FFFD}");
    assert_eq!(fake.last_read().title.as_deref(), Some("\u{FFFD}x"));
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, b"{\"text\":\"a\xFF\"}")).status, 200);
    assert_eq!(fake.last_read().text, "a\u{FFFD}");
    // JSON whitespace around the value; last duplicate key wins.
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, b" \n {\"text\":\"x\",\"text\":\"y\"} \n ")).status, 200);
    assert_eq!(fake.last_read().text, "y");

    // Application refusals are 200 + ok:false (compared as parsed JSON).
    *fake.mode.lock() = Mode::Answer(ReadResult::no_api_key());
    let r = roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({ "ok": false, "code": "no-api-key", "error": "No ElevenLabs API key" }));
    *fake.mode.lock() = Mode::Answer(ReadResult::failed_with_id(READ_ID, ReadErrorCode::Unknown, "engine down"));
    let r = roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    assert_eq!(r.json(), json!({ "ok": false, "readId": READ_ID, "code": "unknown", "error": "engine down" }));
    *fake.mode.lock() = Mode::Answer(ReadResult::still_starting());
    let r = roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    assert_eq!(r.json(), json!({ "ok": false, "code": "unknown", "error": "Vox is still starting" }));
}

#[test]
fn wire_vectors_command_route() {
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, limits());
    let ok = r#"{"ok":true}"#;

    expect(&roundtrip(p, &post("/command", p, CLIENT, br#"{"type":"toggle","x":true}"#)), 200, ok);
    expect(&roundtrip(p, &post("/command", p, CLIENT, br#"{"type":"rate","rate":10}"#)), 200, ok);
    expect(&roundtrip(p, &post("/command", p, CLIENT, br#"{"type":"seek","positionSec":-5}"#)), 200, ok);
    expect(&roundtrip(p, &post("/command", p, CLIENT, br#"{"type":"seek","positionSec":1.2e29}"#)), 200, ok);
    let skip = br#"{"type":"skip","direction":-1,"unit":"seconds","amount":99999}"#;
    expect(&roundtrip(p, &post("/command", p, CLIENT, skip)), 200, ok);
    let skip = br#"{"type":"skip","direction":1.0,"unit":"sentence"}"#;
    expect(&roundtrip(p, &post("/command", p, CLIENT, skip)), 200, ok);
    expect(&roundtrip(p, &post("/command", p, CLIENT, br#"{"type":"jump","type":"stop"}"#)), 200, ok);
    assert_eq!(
        *fake.commands.lock(),
        vec![
            PlayerCommand::Toggle,
            PlayerCommand::Rate { rate: 3.0 },
            PlayerCommand::Seek { position_sec: 0.0 },
            PlayerCommand::Seek { position_sec: 1.2e29 },
            PlayerCommand::Skip { direction: SkipDirection::Back, unit: SkipUnit::Seconds, amount: Some(3600.0) },
            PlayerCommand::Skip { direction: SkipDirection::Forward, unit: SkipUnit::Sentence, amount: None },
            PlayerCommand::Stop,
        ]
    );

    let cases: [(&[u8], &str); 7] = [
        (br#"{"type":"skip","direction":2,"unit":"words"}"#, r#"{"ok":false,"error":"\"direction\" must be 1 or -1"}"#),
        (br#"{"type":"jump"}"#, r#"{"ok":false,"error":"Unknown command type"}"#),
        (
            br#"{"type":"skip","direction":1,"unit":"seconds","amount":null}"#,
            r#"{"ok":false,"error":"\"amount\" must be a finite number"}"#,
        ),
        (br#"{"type":"rate","rate":"2"}"#, r#"{"ok":false,"error":"\"rate\" must be a finite number"}"#),
        (br#"{"type":"rate","rate":1e400}"#, r#"{"ok":false,"error":"\"rate\" must be a finite number"}"#),
        (
            br#"{"type":"skip","direction":-1,"unit":"x","amount":"y"}"#,
            r#"{"ok":false,"error":"\"unit\" must be sentence|seconds"}"#,
        ),
        (br#"{"type":"skip","direction":1e400,"unit":"seconds"}"#, r#"{"ok":false,"error":"\"direction\" must be 1 or -1"}"#),
    ];
    for (body, expected) in cases {
        expect(&roundtrip(p, &post("/command", p, CLIENT, body)), 400, expected);
    }
    assert_eq!(fake.commands.lock().len(), 7, "invalid commands never reach the handler");
}

#[test]
fn keep_alive_pipelining_and_idle_close() {
    let fake = Fake::with_mode(Mode::Started);
    let lim = BridgeLimits { keep_alive: Duration::from_millis(400), ..limits() };
    let (_server, p) = serve(&fake, lim);
    let mut c = Client::new(p);
    // Several requests on one connection, two of them pipelined in one write.
    c.send(&get("/health", p, ""));
    let r = c.response(false);
    expect(&r, 200, HEALTH);
    assert_eq!(r.header("Connection"), Some("keep-alive"));
    assert_eq!(r.header("Keep-Alive"), Some("timeout=0"));
    let mut two = post("/command", p, CLIENT, br#"{"type":"stop"}"#);
    two.extend_from_slice(&get("/health", p, ""));
    c.send(&two);
    expect(&c.response(false), 200, r#"{"ok":true}"#);
    expect(&c.response(false), 200, HEALTH);
    // Then the server closes the idle connection after the keep-alive timeout.
    let idle = Instant::now();
    let rest = c.until_closed(Duration::from_secs(5)).expect("closed after the keep-alive timeout");
    assert!(rest.is_empty());
    let waited = idle.elapsed();
    assert!(waited >= Duration::from_millis(300), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");

    // Stray CRLFs after a body do not start a request: the idle close stays silent (no raw 400).
    let mut c = Client::new(p);
    let mut with_crlf = post("/command", p, CLIENT, br#"{"type":"play"}"#);
    with_crlf.extend_from_slice(b"\r\n");
    c.send(&with_crlf);
    expect(&c.response(false), 200, r#"{"ok":true}"#);
    let idle = Instant::now();
    assert_eq!(c.until_closed(Duration::from_secs(5)), Some(Vec::new()));
    assert!(idle.elapsed() < Duration::from_secs(2), "{:?}", idle.elapsed());

    // A client asking for close gets Connection: close.
    let mut c = Client::new(p);
    c.send(&get("/health", p, "Connection: close\r\n"));
    let r = c.response(false);
    assert_eq!(r.header("Connection"), Some("close"));
    assert!(c.until_closed(Duration::from_secs(5)).is_some());
}

#[test]
fn raw_answers_for_malformed_requests() {
    let fake = Fake::with_mode(Mode::Started);
    // A short keep-alive also shortens the wait for the client to close after a raw answer.
    let lim = BridgeLimits { keep_alive: Duration::from_millis(300), ..limits() };
    let (_server, p) = serve(&fake, lim);

    for garbage in [&b"HELLO\r\n\r\n"[..], b"post /read HTTP/1.1\r\n\r\n", b"GET /health HTTP/9.9\r\n\r\n", b"get /health HTTP/1.1\r\n"] {
        let mut c = Client::new(p);
        c.send(garbage);
        let got = c.until_closed(Duration::from_secs(5)).expect("closed");
        assert_eq!(got, RAW_400, "{:?}", String::from_utf8_lossy(garbage));
    }

    let mut c = Client::new(p);
    let big = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nX-Big: {}\r\n\r\n", "a".repeat(20_000));
    c.send(big.as_bytes());
    assert_eq!(c.until_closed(Duration::from_secs(5)).expect("closed"), RAW_431);

    // CONNECT: closed without an answer.
    let mut c = Client::new(p);
    c.send(format!("CONNECT example.com:443 HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n").as_bytes());
    assert_eq!(c.until_closed(Duration::from_secs(5)).expect("closed"), b"");

    // Conflicting framing (request smuggling): raw 400.
    let mut c = Client::new(p);
    c.send(
        format!("POST /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Content-Length: 5\r\nContent-Length: 6\r\n\r\n")
            .as_bytes(),
    );
    assert_eq!(c.until_closed(Duration::from_secs(5)).expect("closed"), RAW_400);

    // The server keeps serving.
    expect(&roundtrip(p, &get("/health", p, "")), 200, HEALTH);
}

#[test]
fn header_timeout_answers_raw_400() {
    let fake = Fake::with_mode(Mode::Started);
    let lim = BridgeLimits { headers: Duration::from_millis(300), ..limits() };
    let (_server, p) = serve(&fake, lim);
    for sent in [&b""[..], b"GET /health HTTP/1.1\r\nHost: 127"] {
        let started = Instant::now();
        let mut c = Client::new(p);
        c.send(sent);
        assert_eq!(c.until_closed(Duration::from_secs(5)).expect("closed"), RAW_400);
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(250), "{waited:?}");
        assert!(waited < Duration::from_secs(3), "{waited:?}");
    }
}

#[test]
fn body_stall_answers_408_and_closes() {
    let fake = Fake::with_mode(Mode::Started);
    let lim = BridgeLimits { body: Duration::from_millis(300), ..limits() };
    let (_server, p) = serve(&fake, lim);
    let mut c = Client::new(p);
    let started = Instant::now();
    c.send(format!("POST /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Content-Length: 20\r\n\r\n{{\"tex").as_bytes());
    let r = c.response(false);
    let waited = started.elapsed();
    expect(&r, 408, r#"{"ok":false,"error":"Request timed out"}"#);
    assert_eq!(r.header("Connection"), Some("close"));
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(c.until_closed(Duration::from_secs(5)).is_some());
    assert!(fake.reads.lock().is_empty());
}

#[test]
fn read_answers_504_without_a_reply() {
    let fake = Fake::with_mode(Mode::NoAnswer);
    let lim = BridgeLimits { handler: Duration::from_millis(300), ..limits() };
    let (_server, p) = serve(&fake, lim);
    let timed_out = r#"{"ok":false,"error":"Timed out waiting for the app","code":"unknown"}"#;
    let mut c = Client::new(p);
    c.send(&post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    let r = c.response(false);
    expect(&r, 504, timed_out);
    // The connection stays usable after a 504.
    assert_eq!(r.header("Connection"), Some("keep-alive"));
    c.send(&get("/health", p, ""));
    expect(&c.response(false), 200, HEALTH);

    // A handler that waits until its deadline (now + handler limit).
    *fake.mode.lock() = Mode::UntilDeadline;
    let started = Instant::now();
    let r = roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    expect(&r, 504, timed_out);
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(fake.reads.lock().len(), 2);
}

#[test]
fn handler_panic_answers_500_and_server_survives() {
    let fake = Fake::with_mode(Mode::Panic);
    let (_server, p) = serve(&fake, limits());
    let r = roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    expect(&r, 500, r#"{"ok":false,"error":"Internal error","code":"unknown"}"#);
    *fake.mode.lock() = Mode::Started;
    assert_eq!(roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"hi"}"#)).status, 200);
    expect(&roundtrip(p, &get("/health", p, "")), 200, HEALTH);
}

#[test]
fn expect_continue_and_chunked_bodies() {
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, limits());

    let mut c = Client::new(p);
    let body = br#"{"type":"toggle"}"#;
    c.send(
        format!(
            "POST /command HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Expect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    let interim = c.response(false);
    assert_eq!(interim.status, 100);
    c.send(body);
    expect(&c.response(false), 200, r#"{"ok":true}"#);
    assert_eq!(*fake.commands.lock(), vec![PlayerCommand::Toggle]);

    // Any other expectation: 417.
    let mut c = Client::new(p);
    c.send(format!("POST /command HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nExpect: magic\r\n\r\n").as_bytes());
    assert_eq!(c.response(false).status, 417);

    // Chunked body, split across writes.
    let mut c = Client::new(p);
    c.send(format!("POST /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Transfer-Encoding: chunked\r\n\r\n").as_bytes());
    c.send(b"5\r\n{\"tex\r\n");
    std::thread::sleep(Duration::from_millis(30));
    c.send(b"9;ext=1\r\nt\":\"hi\"}\r\n0\r\n\r\n");
    let r = c.response(false);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(fake.last_read().text, "hi");
    // Keep-alive still works after a chunked body.
    c.send(&get("/health", p, ""));
    expect(&c.response(false), 200, HEALTH);

    // A chunked body over 2 MiB: 413 + close.
    let mut c = Client::new(p);
    c.send(format!("POST /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Transfer-Encoding: chunked\r\n\r\n").as_bytes());
    let chunk = vec![b'a'; 64 * 1024];
    let mut sent = 0;
    while sent <= vox_core::constants::BRIDGE_MAX_BODY_BYTES {
        let mut piece = format!("{:x}\r\n", chunk.len()).into_bytes();
        piece.extend_from_slice(&chunk);
        piece.extend_from_slice(b"\r\n");
        if c.s.write_all(&piece).is_err() {
            break;
        }
        sent += chunk.len();
    }
    let r = c.response(false);
    expect(&r, 413, r#"{"ok":false,"error":"Request body too large","code":"too-long"}"#);
    assert_eq!(r.header("Connection"), Some("close"));
}

#[test]
fn connection_cap_closes_extra_connections() {
    let fake = Fake::with_mode(Mode::Started);
    let lim = BridgeLimits {
        headers: Duration::from_secs(20),
        keep_alive: Duration::from_secs(20),
        idle: Duration::from_secs(20),
        max_connections: 64,
        ..limits()
    };
    let (_server, p) = serve(&fake, lim);
    // 64 live connections (each served once, so each is accepted and registered), idle in keep-alive.
    let mut live: Vec<Client> = (0..64)
        .map(|_| {
            let mut c = Client::new(p);
            c.send(&get("/health", p, ""));
            assert_eq!(c.response(false).status, 200);
            c
        })
        .collect();
    // The 65th is closed right after accept, without an answer.
    let mut extra = Client::new(p);
    let _ = extra.s.write_all(&get("/health", p, ""));
    assert_eq!(extra.until_closed(Duration::from_secs(5)), Some(Vec::new()));
    // Existing connections keep working.
    live[10].send(&get("/health", p, ""));
    expect(&live[10].response(false), 200, HEALTH);
    // Once one closes, a new connection is served again.
    drop(live.remove(0));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut c = Client::new(p);
        let _ = c.s.write_all(&get("/health", p, ""));
        let mut first = [0u8; 12];
        if c.s.read_exact(&mut first).is_ok() {
            assert_eq!(&first, b"HTTP/1.1 200");
            break;
        }
        assert!(Instant::now() < deadline, "no slot freed after a client closed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn start_on_a_busy_port_reports_the_exact_error() {
    let fake = Fake::with_mode(Mode::Started);
    let other = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = other.local_addr().expect("addr").port();
    let server = BridgeServer::with_limits(fake.clone(), limits());
    let r = server.start(port);
    assert!(!r.ok);
    assert_eq!(r.port, port);
    assert_eq!(r.error.as_deref(), Some(format!("Port {port} is already in use by another program").as_str()));
    assert!(!server.running());
    assert_eq!(server.port(), port, "the last requested port while stopped");
    // A failed restart leaves the bridge stopped.
    let (server2, p2) = serve(&fake, limits());
    let r = server2.start(port);
    assert!(!r.ok);
    assert!(!server2.running());
    assert!(refused(p2), "the old listener is gone");
    drop(other);
    let r = server2.start(port);
    assert!(r.ok, "{r:?}");
    assert_eq!(server2.port(), port);
}

/// Nothing listens on `port` (a refused loopback connect can take a while on Windows; a timeout counts too).
fn refused(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err()
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

#[test]
fn restart_on_another_port_and_same_port_noop() {
    let fake = Fake::with_mode(Mode::Started);
    let server = BridgeServer::with_limits(fake.clone(), limits());
    assert_eq!(server.port(), 0);
    assert!(!server.running());
    let p1 = free_port();
    assert!(server.start(p1).ok);
    assert_eq!(server.port(), p1);
    // The listener is exclusive.
    assert!(std::net::TcpListener::bind(("127.0.0.1", p1)).is_err());

    // Same port while running: nothing restarts (an open keep-alive connection survives).
    let mut c = Client::new(p1);
    c.send(&get("/health", p1, ""));
    expect(&c.response(false), 200, HEALTH);
    let again = server.start(p1);
    assert_eq!(again, crate::StartResult { ok: true, port: p1, error: None });
    c.send(&get("/health", p1, ""));
    expect(&c.response(false), 200, HEALTH);

    // Another port: the old listener and its connections go, the new one serves.
    let p2 = free_port();
    let moved = server.start(p2);
    assert_eq!(moved, crate::StartResult { ok: true, port: p2, error: None });
    assert!(c.until_closed(Duration::from_secs(5)).is_some(), "old connections are closed");
    assert!(refused(p1));
    expect(&roundtrip(p2, &get("/health", p2, "")), 200, HEALTH);
    // The Host check follows the new port.
    let stale_host = roundtrip(p2, format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{p1}\r\n\r\n").as_bytes());
    assert_eq!(stale_host.status, 403);

    // And back to the first port right away.
    assert!(server.start(p1).ok);
    expect(&roundtrip(p1, &get("/health", p1, "")), 200, HEALTH);

    // Port 0 always restarts on a fresh ephemeral port.
    let e1 = server.start(0);
    let e2 = server.start(0);
    assert!(e1.ok && e2.ok);
    assert_ne!(e1.port, e2.port);
    assert_eq!(server.port(), e2.port);

    server.stop();
    assert!(!server.running());
    assert_eq!(server.port(), 0, "the last requested port");
    server.stop();
}

#[test]
fn stop_with_open_connections_then_rebind_same_port() {
    let fake = Fake::with_mode(Mode::Started);
    let (server, p) = serve(&fake, limits());
    // An idle keep-alive connection, one waiting for its head, and one in the middle of a body.
    let mut idle = Client::new(p);
    idle.send(&get("/health", p, ""));
    expect(&idle.response(false), 200, HEALTH);
    let mut half_head = Client::new(p);
    half_head.send(b"GET /hea");
    let mut half_body = Client::new(p);
    half_body.send(format!("POST /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Content-Length: 50\r\n\r\n{{").as_bytes());
    std::thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    server.stop();
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    assert!(!server.running());
    assert_eq!(server.port(), 0);
    for c in [&mut idle, &mut half_head, &mut half_body] {
        assert_eq!(c.until_closed(Duration::from_secs(2)), Some(Vec::new()), "hard-closed without an answer");
    }
    let r = server.start(p);
    assert!(r.ok, "{r:?}");
    expect(&roundtrip(p, &get("/health", p, "")), 200, HEALTH);
}

#[test]
fn drop_then_bind_again_immediately() {
    let fake = Fake::with_mode(Mode::Started);
    let lim = BridgeLimits { keep_alive: Duration::from_millis(200), ..limits() };
    let (server, p) = serve(&fake, lim);
    // Connections closed every way: by the client, by the server after `Connection: close`, by the keep-alive
    // timeout, and one still open.
    let mut by_client = Client::new(p);
    by_client.send(&get("/health", p, ""));
    by_client.response(false);
    drop(by_client);
    let mut by_server = Client::new(p);
    by_server.send(&get("/health", p, "Connection: close\r\n"));
    by_server.response(false);
    assert!(by_server.until_closed(Duration::from_secs(3)).is_some());
    let mut timed_out = Client::new(p);
    timed_out.send(&get("/health", p, ""));
    timed_out.response(false);
    assert!(timed_out.until_closed(Duration::from_secs(3)).is_some());
    // The graceful closes: a client that closes after the server's FIN (the server then holds a TIME_WAIT), and a
    // client that half-closes right after its request (the server is the passive closer).
    let mut fin_first = Client::new(p);
    fin_first.send(&get("/health", p, "Connection: close\r\n"));
    expect(&fin_first.answer_then_eof(false, Duration::from_secs(3)), 200, HEALTH);
    drop(fin_first);
    let mut half = Client::new(p);
    half.send(&get("/health", p, ""));
    half.half_close();
    expect(&half.answer_then_eof(false, Duration::from_secs(3)), 200, HEALTH);
    // Let the server threads see those clients' FINs and close.
    std::thread::sleep(Duration::from_millis(50));
    let mut open = Client::new(p);
    open.send(&get("/health", p, ""));
    open.response(false);

    // ARCHITECTURE.md §9: drop the server, bind again within 100 ms.
    let again = BridgeServer::with_limits(fake.clone(), lim);
    let dropped = Instant::now();
    drop(server);
    let drop_took = dropped.elapsed();
    let r = again.start(p);
    let took = dropped.elapsed();
    assert!(r.ok, "{r:?}");
    assert!(took < Duration::from_millis(100), "drop {drop_took:?}, drop + bind {took:?}");
    expect(&roundtrip(p, &get("/health", p, "")), 200, HEALTH);
    drop(open);
    drop(half);
}

#[test]
fn stop_never_waits_for_a_pending_read() {
    let fake = Fake::with_mode(Mode::Gate);
    let (tx, rx) = mpsc::channel();
    *fake.entered.lock() = Some(tx);
    let (server, p) = serve(&fake, limits());
    let mut c = Client::new(p);
    c.send(&post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    rx.recv_timeout(Duration::from_secs(5)).expect("the handler was called");

    let started = Instant::now();
    server.stop();
    assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    // The waiting connection was reset at once; the same port binds again while the handler still waits.
    assert_eq!(c.until_closed(Duration::from_secs(2)), Some(Vec::new()));
    let r = server.start(p);
    assert!(r.ok, "{r:?}");
    // When the core finally answers, the old connection is gone; the new listener is unaffected.
    fake.release();
    std::thread::sleep(Duration::from_millis(50));
    *fake.mode.lock() = Mode::Started;
    let r = roundtrip(p, &post("/read", p, CLIENT, br#"{"text":"again"}"#));
    assert_eq!(r.json(), json!({ "ok": true, "readId": READ_ID }));
    assert_eq!(fake.reads.lock().len(), 2);
}

#[test]
fn concurrent_reads_do_not_block_each_other() {
    let fake = Fake::with_mode(Mode::Gate);
    let (tx, rx) = mpsc::channel();
    *fake.entered.lock() = Some(tx);
    let (_server, p) = serve(&fake, limits());
    let mut waiting = Client::new(p);
    waiting.send(&post("/read", p, CLIENT, br#"{"text":"one"}"#));
    rx.recv_timeout(Duration::from_secs(5)).expect("the handler was called");
    // Health and commands are answered while a read waits for the core.
    expect(&roundtrip(p, &get("/health", p, "")), 200, HEALTH);
    expect(&roundtrip(p, &post("/command", p, CLIENT, br#"{"type":"pause"}"#)), 200, r#"{"ok":true}"#);
    fake.release();
    assert_eq!(waiting.response(false).json(), json!({ "ok": true, "readId": READ_ID }));
    // The connection is usable again after the parked read.
    waiting.send(&get("/health", p, ""));
    expect(&waiting.response(false), 200, HEALTH);
}

fn head_health(port: u16) -> Vec<u8> {
    format!("HEAD /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").into_bytes()
}

/// A HEAD answer never carries a body, not even the 500 of a failing `status()` (integrations.md §1.4 step 5), so the
/// keep-alive connection stays in sync.
#[test]
fn head_answers_never_carry_a_body() {
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, limits());
    let internal = r#"{"ok":false,"error":"Internal error","code":"unknown"}"#;
    fake.status_panics.store(true, Ordering::SeqCst);
    let mut c = Client::new(p);
    c.send(&head_health(p));
    let r = c.response(true);
    assert_eq!((r.status, r.body.len()), (500, 0));
    assert_eq!(r.header("Content-Length"), Some(internal.len().to_string().as_str()));
    assert_eq!(r.header("Connection"), Some("keep-alive"));
    assert_json_headers(&r);
    // The next answer on the same connection parses from its first byte.
    c.send(&get("/health", p, ""));
    expect(&c.response(false), 500, internal);
    fake.status_panics.store(false, Ordering::SeqCst);
    c.send(&head_health(p));
    let r = c.response(true);
    assert_eq!((r.status, r.body.len()), (200, 0));
    c.send(&get("/health", p, ""));
    expect(&c.response(false), 200, HEALTH);
}

/// A client that half-closes right after its request (`shutdown(SD_SEND)`, as `nc -N` or Node's `socket.end(req)`)
/// still gets its whole answer, then a clean EOF (no reset).
#[test]
fn half_closed_clients_get_their_answer() {
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, limits());
    let within = Duration::from_secs(5);
    for _ in 0..20 {
        let mut c = Client::new(p);
        c.send(&post("/command", p, CLIENT, br#"{"type":"pause"}"#));
        c.half_close();
        expect(&c.answer_then_eof(false, within), 200, r#"{"ok":true}"#);
    }
    assert_eq!(*fake.commands.lock(), vec![PlayerCommand::Pause; 20]);

    // Keep-alive or close, read at once or later.
    for extra in ["", "Connection: close\r\n"] {
        let mut c = Client::new(p);
        c.send(&get("/health", p, extra));
        c.half_close();
        std::thread::sleep(Duration::from_millis(200));
        expect(&c.answer_then_eof(false, within), 200, HEALTH);
    }
    let mut c = Client::new(p);
    c.send(&head_health(p));
    c.half_close();
    assert_eq!(c.answer_then_eof(true, within).status, 200);

    // `/read` (the socket waits in the registry while the handler runs).
    let mut c = Client::new(p);
    c.send(&post("/read", p, CLIENT, br#"{"text":"hi"}"#));
    c.half_close();
    assert_eq!(c.answer_then_eof(false, within).json(), json!({ "ok": true, "readId": READ_ID }));

    // Two pipelined requests, then the half-close: both answers, then EOF.
    let mut c = Client::new(p);
    let mut two = get("/health", p, "");
    two.extend_from_slice(&post("/command", p, CLIENT, br#"{"type":"play"}"#));
    c.send(&two);
    c.half_close();
    let all = c.read_to_eof(within).expect("clean EOF");
    c.buf = all;
    expect(&c.response(false), 200, HEALTH);
    expect(&c.response(false), 200, r#"{"ok":true}"#);
    assert!(c.buf.is_empty());

    // A head cut short by the half-close is a parse error at EOF for Node: the raw 400.
    let mut c = Client::new(p);
    c.send(b"GET /hea");
    c.half_close();
    assert_eq!(c.read_to_eof(within).expect("clean EOF"), RAW_400);
    // Connect and half-close without a request: closed without an answer.
    let mut c = Client::new(p);
    c.half_close();
    assert_eq!(c.read_to_eof(within).expect("clean EOF"), b"");
}

/// A closing answer (`Connection: close`, HTTP/1.0, an early 413, a 417, the raw 400/431) is followed by EOF at once,
/// like Node's `socket.end()`: a client reading to EOF neither waits out the close grace nor gets a reset.
#[test]
fn closing_answers_are_followed_by_eof_at_once() {
    let fake = Fake::with_mode(Mode::Started);
    // keep_alive 3 s: after a closing answer the server waits up to 1 s for the client to close.
    let (_server, p) = serve(&fake, limits());
    let fast = Duration::from_millis(500);
    let to_eof = |c: &mut Client| {
        let started = Instant::now();
        let all = c.read_to_eof(Duration::from_secs(5));
        let took = started.elapsed();
        assert!(took < fast, "EOF after {took:?}");
        match all {
            Ok(all) => all,
            Err((got, kind)) => panic!("{kind:?} instead of EOF after {:?}", String::from_utf8_lossy(&got)),
        }
    };
    let answer = |c: &mut Client| {
        c.buf = to_eof(c);
        let r = c.response(false);
        assert!(c.buf.is_empty());
        assert_eq!(r.header("Connection"), Some("close"));
        r
    };

    let mut c = Client::new(p);
    c.send(format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{p}\r\n\r\n").as_bytes());
    expect(&answer(&mut c), 200, HEALTH);

    let mut c = Client::new(p);
    c.send(&get("/health", p, "Connection: close\r\n"));
    expect(&answer(&mut c), 200, HEALTH);

    let mut c = Client::new(p);
    c.send(format!("POST /read HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{CLIENT}Content-Length: 3000000\r\n\r\n").as_bytes());
    expect(&answer(&mut c), 413, r#"{"ok":false,"error":"Request body too large","code":"too-long"}"#);

    let mut c = Client::new(p);
    c.send(format!("POST /command HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nExpect: magic\r\n\r\n").as_bytes());
    assert_eq!(answer(&mut c).status, 417);

    let big = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nX-Big: {}\r\n\r\n", "a".repeat(20_000));
    for (raw, expected) in [(&b"HELLO\r\n\r\n"[..], RAW_400), (big.as_bytes(), RAW_431)] {
        let mut c = Client::new(p);
        c.send(raw);
        assert_eq!(to_eof(&mut c), expected);
    }
}

/// The spec's own `curl` examples (integrations.md §1.10) plus keep-alive reuse, HEAD and a > 1 MiB body (curl then
/// sends `Expect: 100-continue`), against a real HTTP client.
///
/// Ignored because it needs `curl.exe` (bundled with Windows 10 1803 and later in `C:\Windows\System32`). Run it with
/// `cargo test -p vox-bridge -- --ignored curl`.
#[test]
#[ignore = "needs curl.exe; run with `cargo test -p vox-bridge -- --ignored curl`"]
fn curl_end_to_end() {
    use std::process::Command;
    let fake = Fake::with_mode(Mode::Started);
    let (_server, p) = serve(&fake, BridgeLimits::default());
    let curl = |args: &[&str]| {
        let out = Command::new("curl.exe").args(["-s", "--max-time", "20"]).args(args).output().expect("run curl.exe");
        assert!(out.status.success(), "curl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let base = format!("http://127.0.0.1:{p}");

    let (out, _) = curl(&[&format!("{base}/health")]);
    assert_eq!(out, HEALTH);
    let (out, _) = curl(&["-I", &format!("{base}/health")]);
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
    assert!(out.contains("Content-Length: 57\r\n"), "{out}");

    let (out, _) = curl(&[
        "-X",
        "POST",
        &format!("{base}/read"),
        "-H",
        "x-vox-client: browser-extension",
        "-H",
        "content-type: application/json",
        "-d",
        r#"{"text":"Hello from the bridge","url":"https://example.com","title":"Example"}"#,
    ]);
    assert_eq!(serde_json::from_str::<Value>(&out).expect("JSON"), json!({ "ok": true, "readId": READ_ID }));
    assert_eq!(
        fake.last_read(),
        BridgeReadRequest {
            text: "Hello from the bridge".into(),
            url: Some("https://example.com".into()),
            title: Some("Example".into())
        }
    );

    let (out, _) = curl(&[
        "-X",
        "POST",
        &format!("{base}/command"),
        "-H",
        "x-vox-client: browser-extension",
        "-H",
        "content-type: application/json",
        "-d",
        r#"{"type":"toggle"}"#,
    ]);
    assert_eq!(out, r#"{"ok":true}"#);
    assert_eq!(fake.commands.lock().last(), Some(&PlayerCommand::Toggle));

    // Two requests on one keep-alive connection.
    let (out, err) = curl(&["-v", &format!("{base}/health"), &format!("{base}/health")]);
    assert_eq!(out, format!("{HEALTH}{HEALTH}"));
    assert!(err.contains("Re-using existing connection") || err.contains("Reusing existing connection"), "{err}");

    // A 1.5 MiB body: curl asks `Expect: 100-continue` and waits for the interim answer.
    let text = "word ".repeat(300_000);
    let path = std::env::temp_dir().join(format!("vox-bridge-curl-{}.json", std::process::id()));
    std::fs::write(&path, json!({ "text": text }).to_string()).expect("write body");
    let body_arg = format!("@{}", path.display());
    let result = Command::new("curl.exe")
        .args(["-s", "-v", "--max-time", "20", "-X", "POST", &format!("{base}/read")])
        .args(["-H", "x-vox-client: browser-extension", "-H", "content-type: application/json"])
        .args(["--data-binary", &body_arg])
        .output();
    let _ = std::fs::remove_file(&path);
    let out = result.expect("run curl.exe");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Expect: 100-continue"), "{err}");
    assert!(err.contains("HTTP/1.1 100 Continue"), "{err}");
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).expect("JSON"), json!({ "ok": true, "readId": READ_ID }));
    assert_eq!(fake.last_read().text.len(), text.len());
}
