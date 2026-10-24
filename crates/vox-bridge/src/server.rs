//! The listener (integrations.md §1.2, §1.7; ARCHITECTURE.md §2.7, §8 D-INT-16). Binds `127.0.0.1:<port>` only,
//! exclusively. `start` never fails; it returns [`StartResult`] with the exact listen-error strings
//! (`vox_core::copy::port_in_use` / `port_blocked` / `LOOPBACK_UNAVAILABLE` / `invalid_port`). Start/stop/restart are
//! serialized by one mutex.
//!
//! **Own accept loop (D-INT-16, a deliberate deviation from BRIEF's `tiny_http`).** tiny_http 0.12 keeps its accepted
//! sockets private (no `set_read_timeout`, no connection count, no close), drops any `Connection` response header,
//! reads header lines without a size cap (no 431), keeps reading a dropped request's body without a timeout, and on
//! drop leaves keep-alive connections alive feeding a queue nobody reads. The §1.10 rows (408 stall, 5 s keep-alive
//! close, 20 000-byte header → 431, `Connection: close` on 413/408) and a clean stop/restart are impossible with it.
//! So the server owns:
//! - a `socket2` listener: `setsockopt(SOL_SOCKET, SO_EXCLUSIVEADDRUSE, 1)` through `windows`
//!   (`Win32_Networking_WinSock`) **before** `bind`; never `SO_REUSEADDR`;
//! - an accept thread and a connection registry capped at [`BridgeLimits::max_connections`] (64; extra connections
//!   are closed right after accept);
//! - one thread per connection. It reads the header block against a deadline (10 s for the first request, measured
//!   from the accept; between requests it waits up to 5 s keep-alive idle for a first byte, then 10 s for the rest of
//!   the head), caps it at [`BridgeLimits::max_header_bytes`] (16 KiB → the raw 431 bytes), answers malformed input, a
//!   lower-case or non-standard method, or a header timeout with the raw 400 bytes, closes `CONNECT` without a
//!   response, and answers `Expect: 100-continue` with `HTTP/1.1 100 Continue` as soon as the headers are in. The head
//!   is parsed with `httparse`. Then `pipeline::check_request` and `check_body_headers`, the body (Content-Length or
//!   chunked) with a 2 MiB cap and a 15 s deadline (→ 408 with `Connection: close`), `parse_json_body`, the route, and
//!   the response. The server writes `Connection: close` (closing after the response) or `Keep-Alive: timeout=5`. An
//!   answer sent before a request's body was read (403/404/405/415/413) always closes the connection. A `HEAD`
//!   request's answer never carries a body, whatever produced it (its `Content-Length` is still the JSON's).
//! - How connections close (`SO_LINGER {on, 0}` is set on every accepted socket, so a plain drop is abortive):
//!   - after a closing answer (`Connection: close`, or a raw 400/431/417): a FIN right behind the answer (Node's
//!     `socket.end()`), so a client that reads to EOF gets EOF at once; then up to `min(keep_alive, 1 s)` for the
//!     client to close its side, discarding anything it still sends; then the close (graceful if the client closed,
//!     else abortive). So no reset ever destroys an answer the client has not read yet;
//!   - when the client closes its side (also right after sending a request, a legal half-close): the pending answer
//!     is written, then a graceful close (the server is the passive closer: no `TIME_WAIT`). A partial request head
//!     at that point gets the raw 400 first (Node's `clientError` path);
//!   - everything else (stop, quit, keep-alive/idle timeouts, over-limit, CONNECT, socket errors): abortive.
//!
//!   No close leaves an accepted connection that makes a new exclusive bind of the same port fail (re-enable, port
//!   change and back, relaunch within seconds; socket tests).
//!
//! I/O is non-blocking and event-driven (`WSAEventSelect`, private `net` module): every wait is for the socket's own
//! event or the server's stop event, with the exact time left to its deadline, so nothing polls and an idle bridge
//! costs no CPU.
//!
//! [`BridgeServer::stop`]: marks the registry closed and takes every socket whose thread waits in
//! [`BridgeHandler::read`] (those threads park their socket in the registry for the duration of the call); sets the
//! stop event, which wakes the accept thread and every connection thread at once; joins the accept thread (the
//! listening socket is closed when it ends); resets the parked sockets; and waits (≤ 2 s) until the connection
//! threads that were in socket I/O have reset theirs. It returns **without** waiting for [`BridgeHandler::read`] calls
//! (the core thread calls `stop()`, so waiting for them would deadlock); such a thread finds its socket gone when the
//! handler returns and ends without answering. So when `stop()` returns, no socket of the old server exists and the
//! same port can be bound again at once.
//!
//! Logging (target `bridge`): `bridge: listening on http://127.0.0.1:<port>` (info), `bridge: <listen error>` (warn),
//! `bridge: 504 Timed out waiting for the app` (warn), handler panics (error). Request bodies, URLs, titles and the
//! text are never logged; 4xx answers are not logged.
//!
//! Socket tests (ARCHITECTURE.md §9, with [`BridgeServer::with_limits`] and scaled-down timeouts): every §1.10 row
//! incl. `HELLO` → raw 400, a 20 000-byte header → 431, the 408 stall, the keep-alive close; open a keep-alive
//! connection, `stop()`, `start(same port)` → ok; serve a request, drop the server, bind again in the same process
//! within 100 ms → ok; 64-connection cap; `/read` 504 when the handler returns `None`; a half-closed client gets its
//! answer and a clean EOF; a closing answer is followed by EOF at once; a HEAD answer never has a body.

use std::collections::HashMap;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde_json::json;
use socket2::Socket;
use vox_core::constants::BRIDGE_MAX_BODY_BYTES;
use vox_core::player::PlayerCommand;
use vox_core::read::{BridgeReadRequest, ReadErrorCode, ReadResult};

use crate::net::{self, Conn, IoFail, Listener, ListenerWake, WsaEvent};
use crate::pipeline::{self, HttpResponse, Route};
use crate::wire::{self, ChunkedDecoder, Expect, Framing, Head, Scan};

/// What `/health` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatus {
    pub app_name: String,
    pub version: String,
    /// Player status is exactly `playing`.
    pub playing: bool,
}

/// Implemented by vox-app (core glue). Called on bridge connection threads.
pub trait BridgeHandler: Send + Sync {
    /// Starts a read and returns the core's `ReadResult`, waiting at most until `deadline` (the server passes now +
    /// 15 s). `None` = no answer in time (or the core is gone) → the server answers 504 `Timed out waiting for the
    /// app` with code `unknown`; the read is **not** cancelled (the core still runs it if it later dequeues it).
    fn read(&self, req: BridgeReadRequest, deadline: Instant) -> Option<ReadResult>;
    /// A validated player command (fire-and-forget; the HTTP answer does not wait for the player).
    fn command(&self, cmd: PlayerCommand);
    fn status(&self) -> BridgeStatus;
}

/// `start(port)` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartResult {
    pub ok: bool,
    /// Bound port on success, else the requested one.
    pub port: u16,
    pub error: Option<String>,
}

/// Connection limits and timeouts (integrations.md §1.7). `Default` = the vox-core constants; tests scale them down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeLimits {
    /// Header block deadline for the first request on a connection (10 s).
    pub headers: Duration,
    /// Idle time allowed between requests on a keep-alive connection (5 s).
    pub keep_alive: Duration,
    /// Body deadline (15 s → 408 + `Connection: close`).
    pub body: Duration,
    /// How long `/read` waits for the core (15 s → 504).
    pub handler: Duration,
    /// Idle socket timeout (20 s).
    pub idle: Duration,
    /// Live connections (64).
    pub max_connections: usize,
    /// Header block cap (16 KiB → 431).
    pub max_header_bytes: usize,
}

impl Default for BridgeLimits {
    fn default() -> Self {
        use vox_core::constants::*;
        Self {
            headers: Duration::from_millis(BRIDGE_HEADERS_TIMEOUT_MS),
            keep_alive: Duration::from_millis(BRIDGE_KEEP_ALIVE_MS),
            body: Duration::from_millis(BRIDGE_TIMEOUT_MS),
            handler: Duration::from_millis(BRIDGE_TIMEOUT_MS),
            idle: Duration::from_millis(BRIDGE_IDLE_SOCKET_MS),
            max_connections: BRIDGE_MAX_CONNECTIONS,
            max_header_bytes: BRIDGE_MAX_HEADER_BYTES,
        }
    }
}

/// How long `stop()` waits for connection threads in socket I/O to reset their sockets (they wake at once; this only
/// bounds a pathological case).
const STOP_WAIT: Duration = Duration::from_secs(2);
/// Upper bound of the time a client gets, after a closing answer and the server's FIN, to close its side.
const CLOSE_GRACE_MAX: Duration = Duration::from_secs(1);
/// Bytes a closing connection discards while it waits for the client to close.
const CLOSE_DRAIN_MAX: usize = 4 * 1024 * 1024;
/// Back-off after a failed accept (e.g. out of sockets), so a pending connection cannot spin the accept thread.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The bridge server (one per app).
pub struct BridgeServer {
    handler: Arc<dyn BridgeHandler>,
    limits: BridgeLimits,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The last port asked for (0 if never started).
    requested_port: u16,
    running: Option<Running>,
}

/// A live listener and its threads.
struct Running {
    port: u16,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

/// State shared by the accept thread and the connection threads of one listener.
struct Shared {
    handler: Arc<dyn BridgeHandler>,
    limits: BridgeLimits,
    /// The bound port (the Host check compares against it).
    port: u16,
    /// Manual-reset event set by `stop()`; every wait of this listener's threads includes it.
    stop: WsaEvent,
    stopping: AtomicBool,
    /// The accept loop ended on its own (a failed wait); `running()` then reports false so a sync restarts it.
    accept_dead: AtomicBool,
    registry: Mutex<Registry>,
    /// Signalled when a connection deregisters after `stop()`.
    drained: Condvar,
}

#[derive(Default)]
struct Registry {
    closed: bool,
    next_id: u64,
    conns: HashMap<u64, Slot>,
}

enum Slot {
    /// Its thread owns the socket (reading, writing or computing).
    Active,
    /// Its thread waits in [`BridgeHandler::read`]; the socket is parked here so `stop()` can reset it at once.
    Parked(Conn),
}

impl BridgeServer {
    pub fn new(handler: Arc<dyn BridgeHandler>) -> Self {
        Self::with_limits(handler, BridgeLimits::default())
    }

    /// A server with custom limits (socket tests).
    pub fn with_limits(handler: Arc<dyn BridgeHandler>, limits: BridgeLimits) -> Self {
        Self { handler, limits, state: Mutex::new(State::default()) }
    }

    /// Starts (or restarts on a different port; same port while running → no-op `ok`). Port 0 = ephemeral (tests).
    pub fn start(&self, port: u16) -> StartResult {
        let mut state = self.state.lock();
        state.requested_port = port;
        if let Some(running) = &state.running {
            if port != 0 && running.port == port && running.alive() {
                return StartResult { ok: true, port, error: None };
            }
        }
        // Another port (or port 0, or a dead accept loop): the old listener goes first. If the new bind fails, the
        // bridge stays stopped.
        if let Some(running) = state.running.take() {
            running.shutdown();
        }
        match self.launch(port) {
            Ok(running) => {
                let bound = running.port;
                tracing::info!(target: "bridge", "bridge: listening on http://127.0.0.1:{bound}");
                state.running = Some(running);
                StartResult { ok: true, port: bound, error: None }
            }
            Err(error) => {
                tracing::warn!(target: "bridge", "bridge: {error}");
                StartResult { ok: false, port, error: Some(error) }
            }
        }
    }

    /// Stops listening and hard-closes every connection (module docs). Never fails; never waits for in-flight
    /// [`BridgeHandler::read`] calls.
    pub fn stop(&self) {
        let mut state = self.state.lock();
        if let Some(running) = state.running.take() {
            running.shutdown();
        }
    }

    pub fn running(&self) -> bool {
        self.state.lock().running.as_ref().is_some_and(Running::alive)
    }

    /// Bound port while running, else the last requested port (0 if never started).
    pub fn port(&self) -> u16 {
        let state = self.state.lock();
        state.running.as_ref().map_or(state.requested_port, |r| r.port)
    }

    fn launch(&self, port: u16) -> Result<Running, String> {
        let sock = net::bind_exclusive(port).map_err(|e| net::listen_error(&e, port))?;
        let bound = net::bound_port(&sock).map_err(|e| e.to_string())?;
        let listener = Listener::new(sock).map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            handler: self.handler.clone(),
            limits: self.limits,
            port: bound,
            stop: WsaEvent::new().map_err(|e| e.to_string())?,
            stopping: AtomicBool::new(false),
            accept_dead: AtomicBool::new(false),
            registry: Mutex::new(Registry::default()),
            drained: Condvar::new(),
        });
        let thread_shared = shared.clone();
        // On a spawn failure the closure (and with it the listener) is dropped, which closes the socket.
        let accept = std::thread::Builder::new()
            .name("vox-bridge-accept".into())
            .spawn(move || accept_thread(listener, thread_shared))
            .map_err(|e| e.to_string())?;
        Ok(Running { port: bound, shared, accept: Some(accept) })
    }
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Running {
    fn alive(&self) -> bool {
        !self.shared.accept_dead.load(Ordering::SeqCst)
    }

    /// The stop sequence of the module docs.
    fn shutdown(mut self) {
        let shared = &self.shared;
        // 1. No new connections; take the sockets whose threads wait in `read` (never wait for those threads).
        let parked: Vec<Conn> = {
            let mut guard = shared.registry.lock();
            let reg = &mut *guard;
            reg.closed = true;
            let ids: Vec<u64> =
                reg.conns.iter().filter(|(_, slot)| matches!(slot, Slot::Parked(_))).map(|(id, _)| *id).collect();
            ids.into_iter()
                .filter_map(|id| match reg.conns.remove(&id) {
                    Some(Slot::Parked(conn)) => Some(conn),
                    _ => None,
                })
                .collect()
        };
        // 2. Wake the accept thread and every connection thread.
        shared.stopping.store(true, Ordering::SeqCst);
        shared.stop.set();
        // 3. The listening socket is closed when the accept thread ends.
        if let Some(accept) = self.accept.take() {
            if accept.join().is_err() {
                tracing::error!(target: "bridge", "bridge: the accept thread panicked");
            }
        }
        // 4. Reset the parked connections now.
        drop(parked);
        // 5. Threads in socket I/O reset theirs as soon as they wake.
        let deadline = Instant::now() + STOP_WAIT;
        let mut reg = shared.registry.lock();
        while !reg.conns.is_empty() {
            if shared.drained.wait_until(&mut reg, deadline).timed_out() {
                tracing::warn!(target: "bridge", "bridge: {} connection(s) still closing after stop", reg.conns.len());
                break;
            }
        }
    }
}

impl Shared {
    fn deregister(&self, id: u64) {
        let mut reg = self.registry.lock();
        reg.conns.remove(&id);
        if reg.closed {
            self.drained.notify_all();
        }
    }
}

fn accept_thread(listener: Listener, shared: Arc<Shared>) {
    if catch_unwind(AssertUnwindSafe(|| accept_loop(&listener, &shared))).is_err() {
        tracing::error!(target: "bridge", "bridge: the accept loop panicked");
        shared.accept_dead.store(true, Ordering::SeqCst);
    }
    // `listener` drops here: the listening socket is closed.
}

fn accept_loop(listener: &Listener, shared: &Arc<Shared>) {
    loop {
        match listener.wait(&shared.stop) {
            Ok(ListenerWake::Stopped) => return,
            Ok(ListenerWake::Ready) => {}
            Err(e) => {
                tracing::error!(target: "bridge", "bridge: {e}");
                shared.accept_dead.store(true, Ordering::SeqCst);
                return;
            }
        }
        loop {
            if shared.stopping.load(Ordering::SeqCst) {
                return;
            }
            match listener.accept() {
                Ok(Some(sock)) => admit(shared, sock),
                Ok(None) => break,
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) => {}
                Err(e) => {
                    tracing::warn!(target: "bridge", "bridge: {e}");
                    if listener.pause(&shared.stop, ACCEPT_BACKOFF) {
                        return;
                    }
                    break;
                }
            }
        }
    }
}

/// Registers an accepted socket and starts its thread, or resets it (stopped, or over the connection cap).
fn admit(shared: &Arc<Shared>, sock: Socket) {
    let accepted = Instant::now();
    let id = {
        let mut reg = shared.registry.lock();
        if reg.closed || reg.conns.len() >= shared.limits.max_connections {
            let closed = reg.closed;
            drop(reg);
            if !closed {
                tracing::debug!(target: "bridge", "bridge: connection limit reached; closing a new connection");
            }
            let _ = sock.set_linger(Some(Duration::ZERO));
            return;
        }
        let id = reg.next_id;
        reg.next_id += 1;
        reg.conns.insert(id, Slot::Active);
        id
    };
    let conn = match Conn::new(sock) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::debug!(target: "bridge", "bridge: could not set up a connection: {e}");
            shared.deregister(id);
            return;
        }
    };
    let thread_shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("vox-bridge-conn".into())
        .spawn(move || connection_thread(thread_shared, id, conn, accepted));
    if let Err(e) = spawned {
        tracing::warn!(target: "bridge", "bridge: could not start a connection thread: {e}");
        shared.deregister(id);
    }
}

fn connection_thread(shared: Arc<Shared>, id: u64, conn: Conn, accepted: Instant) {
    let connection = Connection { shared: &shared, id, conn: Some(conn), buf: Vec::new() };
    if catch_unwind(AssertUnwindSafe(|| connection.run(accepted))).is_err() {
        tracing::error!(target: "bridge", "bridge: a connection handler panicked");
    }
    // The socket was closed when `connection` was dropped (also while unwinding); now it no longer counts.
    shared.deregister(id);
}

/// How a request's handling ended.
enum Next {
    /// Answered; wait for the next request on this connection.
    KeepAlive,
    /// Answered with `Connection: close`: FIN, let the client close too, then close ([`Connection::end`]).
    Close,
    /// Reset now (client gone, CONNECT, stopping).
    Abort,
}

enum HeadRead {
    Head(Head),
    /// Answer these raw bytes, then close.
    Raw(&'static [u8]),
    /// The client closed its side between requests: close gracefully.
    PeerClosed,
    /// Close without an answer, abortively (keep-alive idle, stop, socket error).
    Gone,
}

enum BodyFail {
    /// Answer this (a 400/408/413 that closes the connection).
    Respond(HttpResponse),
    /// Malformed chunked framing: raw 400.
    Malformed,
    /// Client gone or stopping: reset.
    Gone,
}

/// One connection's request loop (runs on its own thread).
struct Connection<'a> {
    shared: &'a Shared,
    id: u64,
    /// `None` while parked in the registry during [`BridgeHandler::read`], or after `stop()` took it.
    conn: Option<Conn>,
    /// Received bytes not consumed yet (a pipelined next request may follow a body).
    buf: Vec<u8>,
}

impl Connection<'_> {
    fn run(mut self, accepted: Instant) {
        let mut first = Some(accepted);
        loop {
            if self.stopping() {
                return;
            }
            let head = match self.read_head(first.take()) {
                HeadRead::Head(head) => head,
                HeadRead::Raw(bytes) => {
                    if self.write(bytes) {
                        self.end();
                    }
                    return;
                }
                HeadRead::PeerClosed => {
                    self.close_gracefully();
                    return;
                }
                HeadRead::Gone => return,
            };
            match self.serve(head) {
                Next::KeepAlive => {}
                Next::Close => {
                    self.end();
                    return;
                }
                Next::Abort => return,
            }
        }
    }

    fn stopping(&self) -> bool {
        self.shared.stopping.load(Ordering::SeqCst)
    }

    /// Reads until a complete head is buffered. `first` = the accept time for the connection's first request.
    fn read_head(&mut self, first: Option<Instant>) -> HeadRead {
        let limits = self.shared.limits;
        // Between requests the keep-alive idle time applies until a request byte arrives (stray CR/LF, which some
        // clients send after a body, do not count: llhttp skips them without starting a message).
        let mut head_deadline = match first {
            Some(accepted) => Some(accepted + limits.headers),
            None if wire::request_started(&self.buf) => Some(Instant::now() + limits.headers),
            None => None,
        };
        let idle_deadline = Instant::now() + limits.keep_alive;
        loop {
            match wire::scan_head(&self.buf, limits.max_header_bytes) {
                Scan::Complete(head, len) => {
                    self.buf.drain(..len);
                    return HeadRead::Head(head);
                }
                Scan::Bad => return HeadRead::Raw(wire::RAW_400),
                Scan::TooLarge => return HeadRead::Raw(wire::RAW_431),
                Scan::Partial => {}
            }
            let Some(conn) = self.conn.as_ref() else { return HeadRead::Gone };
            match conn.read_into(&mut self.buf, head_deadline.unwrap_or(idle_deadline), &self.shared.stop) {
                // The client closed its side (after its last request, or with a half-close right behind a request
                // whose answer was just written). A head cut short by it is a parse error at EOF for Node: raw 400.
                Ok(0) if wire::request_started(&self.buf) => return HeadRead::Raw(wire::RAW_400),
                Ok(0) => return HeadRead::PeerClosed,
                Ok(_) => {
                    if head_deadline.is_none() && wire::request_started(&self.buf) {
                        head_deadline = Some(Instant::now() + limits.headers);
                    }
                }
                // A head that did not complete in time (or a first request that never came): raw 400.
                Err(IoFail::Timeout) if head_deadline.is_some() => return HeadRead::Raw(wire::RAW_400),
                Err(_) => return HeadRead::Gone,
            }
        }
    }

    /// One request: the pipeline of integrations.md §1.4 and the response.
    fn serve(&mut self, head: Head) -> Next {
        // Node has no `connect` listener: it closes the connection without an answer.
        if head.method == "CONNECT" {
            return Next::Abort;
        }
        match head.expect {
            Expect::Continue => {
                if !self.write(wire::CONTINUE_100) {
                    return Next::Abort;
                }
            }
            Expect::Unsupported => return if self.write(wire::RAW_417) { Next::Close } else { Next::Abort },
            Expect::None => {}
        }
        let unread_body = head.framing.has_body();
        let route = match pipeline::check_request(&head.method, &head.target, &head.headers, self.shared.port) {
            Ok(route) => route,
            Err(resp) => return self.respond(resp, &head, unread_body),
        };
        let resp = match route {
            Route::Health { head: is_head } => match self.call(|h| h.status()) {
                Some(status) => pipeline::json_response(200, &pipeline::health_body(&status), is_head),
                None => internal_error(),
            },
            Route::Read | Route::Command => {
                if let Err(resp) = pipeline::check_body_headers(&head.headers) {
                    return self.respond(resp, &head, unread_body);
                }
                let body = match self.read_body(head.framing) {
                    Ok(body) => body,
                    Err(BodyFail::Respond(resp)) => return self.respond(resp, &head, true),
                    Err(BodyFail::Malformed) => {
                        return if self.write(wire::RAW_400) { Next::Close } else { Next::Abort };
                    }
                    Err(BodyFail::Gone) => return Next::Abort,
                };
                let map = match pipeline::parse_json_body(&body) {
                    Ok(map) => map,
                    Err(resp) => return self.respond(resp, &head, false),
                };
                drop(body);
                if route == Route::Read {
                    match pipeline::parse_read_body(&map) {
                        Err(resp) => resp,
                        Ok(req) => {
                            drop(map);
                            match self.read(req) {
                                Some(resp) => resp,
                                // Stopped while the core was busy: the socket is gone.
                                None => return Next::Abort,
                            }
                        }
                    }
                } else {
                    match pipeline::parse_command_body(&map) {
                        Err(resp) => resp,
                        Ok(cmd) => match self.call(move |h| h.command(cmd)) {
                            Some(()) => pipeline::json_response(200, &json!({ "ok": true }), false),
                            None => internal_error(),
                        },
                    }
                }
            }
        };
        let unread = matches!(route, Route::Health { .. }) && unread_body;
        self.respond(resp, &head, unread)
    }

    /// Writes `resp`; `Connection: close` when the response asks for it, the client did, or a body was left unread.
    /// The answer to a `HEAD` request never carries a body, whichever path built it (integrations.md §1.4 step 5, §6:
    /// the old `sendJson` dropped the payload for every HEAD answer, the 500 of a failing `status()` included); its
    /// `Content-Length` stays that of the JSON.
    fn respond(&mut self, mut resp: HttpResponse, head: &Head, unread_body: bool) -> Next {
        if head.method.eq_ignore_ascii_case("HEAD") {
            resp.body.clear();
        }
        let close = resp.close || unread_body || !head.keep_alive;
        let bytes = wire::serialize(&resp, !close, self.shared.limits.keep_alive.as_secs());
        if !self.write(&bytes) {
            return Next::Abort;
        }
        if close {
            Next::Close
        } else {
            Next::KeepAlive
        }
    }

    fn write(&self, bytes: &[u8]) -> bool {
        if self.stopping() {
            return false;
        }
        let Some(conn) = self.conn.as_ref() else { return false };
        conn.write_all(bytes, Instant::now() + self.shared.limits.idle, &self.shared.stop).is_ok()
    }

    /// After a closing answer (`Connection: close` or a raw answer): a FIN right behind it (Node's `socket.end()`), so
    /// a client reading to EOF is done at once; then up to `min(keep_alive, 1 s)` for the client to close its side,
    /// discarding what it still sends (shutdown + receive until 0). Then a graceful close if it did; otherwise the
    /// connection drops abortively (its answer and FIN are already sent, so nothing unread is lost).
    fn end(&mut self) {
        let Some(conn) = self.conn.as_ref() else { return };
        if conn.shutdown_send().is_err() {
            return;
        }
        let deadline = Instant::now() + self.shared.limits.keep_alive.min(CLOSE_GRACE_MAX);
        let mut sink = Vec::new();
        let mut discarded = 0usize;
        while discarded <= CLOSE_DRAIN_MAX {
            sink.clear();
            match conn.read_into(&mut sink, deadline, &self.shared.stop) {
                Ok(0) => {
                    self.close_gracefully();
                    return;
                }
                Ok(n) => discarded += n,
                Err(_) => return,
            }
        }
    }

    /// Closes without a reset once the client has closed its side (module docs): an answer still in flight is
    /// delivered, then the FIN.
    fn close_gracefully(&mut self) {
        if let Some(conn) = self.conn.take() {
            conn.close_graceful();
        }
    }

    /// The body per its framing: ≤ 2 MiB, complete within the body deadline from now.
    fn read_body(&mut self, framing: Framing) -> Result<Vec<u8>, BodyFail> {
        let deadline = Instant::now() + self.shared.limits.body;
        match framing {
            Framing::Empty => Ok(Vec::new()),
            Framing::Length(n) => {
                let n = usize::try_from(n).ok().filter(|n| *n <= BRIDGE_MAX_BODY_BYTES).ok_or_else(too_large)?;
                while self.buf.len() < n {
                    self.fill(deadline)?;
                }
                let rest = self.buf.split_off(n);
                Ok(std::mem::replace(&mut self.buf, rest))
            }
            Framing::Chunked => {
                let mut decoder = ChunkedDecoder::default();
                let mut body = Vec::new();
                loop {
                    let used = decoder.decode(&self.buf, &mut body).map_err(|_| BodyFail::Malformed)?;
                    self.buf.drain(..used);
                    if body.len() > BRIDGE_MAX_BODY_BYTES {
                        return Err(too_large());
                    }
                    if decoder.is_done() {
                        return Ok(body);
                    }
                    self.fill(deadline)?;
                }
            }
        }
    }

    /// Receives more body bytes into `buf`.
    fn fill(&mut self, deadline: Instant) -> Result<(), BodyFail> {
        let Some(conn) = self.conn.as_ref() else { return Err(BodyFail::Gone) };
        match conn.read_into(&mut self.buf, deadline, &self.shared.stop) {
            Ok(0) => Err(BodyFail::Respond(closing(pipeline::error_response(400, "Request aborted", None)))),
            Ok(_) => Ok(()),
            Err(IoFail::Timeout) => Err(BodyFail::Respond(pipeline::error_response(408, "Request timed out", None))),
            Err(IoFail::Stopped) => Err(BodyFail::Gone),
            Err(IoFail::Io(e))
                if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted) =>
            {
                Err(BodyFail::Gone)
            }
            Err(IoFail::Io(_)) => {
                Err(BodyFail::Respond(closing(pipeline::error_response(400, "Request stream error", None))))
            }
        }
    }

    /// `POST /read`: the handler runs with the socket parked in the registry, so `stop()` can reset it without waiting
    /// for the core. `None` = stopped meanwhile (the socket is gone; nothing to answer).
    fn read(&mut self, req: BridgeReadRequest) -> Option<HttpResponse> {
        let shared = self.shared;
        {
            let mut reg = shared.registry.lock();
            if reg.closed {
                return None;
            }
            let conn = self.conn.take()?;
            reg.conns.insert(self.id, Slot::Parked(conn));
        }
        let deadline = Instant::now() + shared.limits.handler;
        let answer = self.call(move |h| h.read(req, deadline));
        {
            let mut guard = shared.registry.lock();
            let reg = &mut *guard;
            match reg.conns.get_mut(&self.id) {
                Some(slot) if !reg.closed => match std::mem::replace(slot, Slot::Active) {
                    Slot::Parked(conn) => self.conn = Some(conn),
                    Slot::Active => return None,
                },
                _ => return None,
            }
        }
        Some(match answer {
            Some(Some(result)) => read_result_response(&result),
            Some(None) => {
                tracing::warn!(target: "bridge", "bridge: 504 Timed out waiting for the app");
                pipeline::error_response(504, "Timed out waiting for the app", Some(ReadErrorCode::Unknown))
            }
            None => internal_error(),
        })
    }

    /// Runs a handler call; a panic is logged and becomes `None` (→ 500).
    fn call<T>(&self, f: impl FnOnce(&dyn BridgeHandler) -> T) -> Option<T> {
        let handler = self.shared.handler.as_ref();
        match catch_unwind(AssertUnwindSafe(|| f(handler))) {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::error!(target: "bridge", "bridge: handler failed: panic");
                None
            }
        }
    }
}

/// 200 with the reader's `ReadResult`, whatever its `ok` (application refusals are 200 + `ok:false`).
fn read_result_response(result: &ReadResult) -> HttpResponse {
    match serde_json::to_value(result) {
        Ok(value) => pipeline::json_response(200, &value, false),
        Err(_) => internal_error(),
    }
}

/// A handler that panicked (integrations.md §1.4 step 16/17): 500, code `unknown`. The panic message is not sent (it
/// is a developer message, not something the extension should show).
fn internal_error() -> HttpResponse {
    pipeline::error_response(500, "Internal error", Some(ReadErrorCode::Unknown))
}

fn too_large() -> BodyFail {
    BodyFail::Respond(pipeline::error_response(413, "Request body too large", Some(ReadErrorCode::TooLong)))
}

fn closing(mut resp: HttpResponse) -> HttpResponse {
    resp.close = true;
    resp
}
