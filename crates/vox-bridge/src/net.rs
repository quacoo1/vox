//! WinSock plumbing for the bridge (private; ARCHITECTURE.md §8 D-INT-16).
//!
//! - [`bind_exclusive`]: a `socket2` listener on `127.0.0.1:<port>` with `SO_EXCLUSIVEADDRUSE` set **before** `bind`
//!   (never `SO_REUSEADDR`), and [`listen_error`] mapping bind/listen failures to the exact copy of
//!   integrations.md §1.2.
//! - Event-driven, non-blocking sockets: the listener and every connection are tied to their own WinSock event with
//!   `WSAEventSelect`; every wait is `WSAWaitForMultipleEvents` over `[stop, own event]` with the remaining time to a
//!   deadline. So the accept thread costs nothing while idle, a deadline is exact (no polling), and one manual-reset
//!   [`WsaEvent`] set by `stop()` wakes every thread of a server at once.
//! - Every accepted socket gets `SO_LINGER {on, 0}` and `TCP_NODELAY` up front: whatever closes it without a FIN
//!   exchange (stop, timeouts, over-limit, errors) closes it **abortively**, so no lingering server-side connection
//!   keeps a new exclusive bind of the same port from succeeding. Two closes are graceful instead
//!   ([`Conn::shutdown_send`], [`Conn::close_graceful`]): after the client closed its side (the server is then the
//!   passive closer and gets no `TIME_WAIT`), and after a closing answer, where the server sends its FIN right behind
//!   the answer and waits for the client's (the shutdown + receive-until-0 sequence WinSock documents for
//!   `SO_EXCLUSIVEADDRUSE` ports). Either way the same port can be bound again at once (socket tests).

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4};
use std::os::windows::io::AsRawSocket;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::WinSock::{
    setsockopt, WSACloseEvent, WSACreateEvent, WSAEnumNetworkEvents, WSAEventSelect, WSAGetLastError, WSAResetEvent,
    WSASetEvent, WSAWaitForMultipleEvents, FD_ACCEPT, FD_CLOSE, FD_READ, FD_WRITE, SOCKET, SOL_SOCKET,
    SO_EXCLUSIVEADDRUSE, WSAEVENT, WSANETWORKEVENTS, WSA_WAIT_FAILED, WSA_WAIT_TIMEOUT,
};

/// `WSAEADDRINUSE`.
const ADDR_IN_USE: i32 = 10_048;
/// `WSAEACCES` (e.g. Hyper-V / WSL excluded port ranges).
const ACCESS_DENIED: i32 = 10_013;
/// `WSAEADDRNOTAVAIL`.
const ADDR_NOT_AVAILABLE: i32 = 10_049;
/// Node's default listen backlog.
const BACKLOG: i32 = 511;
/// `WSA_INFINITE`.
const INFINITE: u32 = u32::MAX;

/// The listen-error copy of integrations.md §1.2 for a failed bind/listen.
pub(crate) fn listen_error(err: &io::Error, port: u16) -> String {
    match err.raw_os_error() {
        Some(ADDR_IN_USE) => vox_core::copy::port_in_use(port),
        Some(ACCESS_DENIED) => vox_core::copy::port_blocked(port),
        Some(ADDR_NOT_AVAILABLE) => vox_core::copy::LOOPBACK_UNAVAILABLE.to_string(),
        _ => err.to_string(),
    }
}

fn raw(sock: &Socket) -> SOCKET {
    SOCKET(sock.as_raw_socket() as usize)
}

fn last_wsa_error() -> io::Error {
    // SAFETY: reads the calling thread's WinSock error slot.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0)
}

/// A bound, listening TCP socket on `127.0.0.1:<port>` (port 0 = ephemeral), exclusive.
pub(crate) fn bind_exclusive(port: u16) -> io::Result<Socket> {
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let on = 1i32.to_ne_bytes();
    // SAFETY: `sock` is a valid socket for the duration of the call; the option value is a 4-byte BOOL.
    if unsafe { setsockopt(raw(&sock), SOL_SOCKET, SO_EXCLUSIVEADDRUSE, Some(&on)) } != 0 {
        return Err(last_wsa_error());
    }
    sock.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)))?;
    sock.listen(BACKLOG)?;
    Ok(sock)
}

/// The port a listener is bound to.
pub(crate) fn bound_port(sock: &Socket) -> io::Result<u16> {
    match sock.local_addr()?.as_socket() {
        Some(SocketAddr::V4(a)) => Ok(a.port()),
        Some(SocketAddr::V6(a)) => Ok(a.port()),
        None => Err(io::Error::other("listener has no IP address")),
    }
}

/// A manual-reset WinSock event object.
#[derive(Debug)]
pub(crate) struct WsaEvent(WSAEVENT);

impl WsaEvent {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: plain object creation; the handle is owned by the returned value.
        unsafe { WSACreateEvent() }.map(Self).map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
    }

    pub(crate) fn set(&self) {
        // SAFETY: the handle is valid until `Drop`.
        if let Err(e) = unsafe { WSASetEvent(self.0) } {
            tracing::warn!(target: "bridge", "bridge: could not signal stop: {e}");
        }
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0 .0 as *mut core::ffi::c_void)
    }
}

impl Drop for WsaEvent {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `WSACreateEvent` and is closed exactly once.
        let _ = unsafe { WSACloseEvent(self.0) };
    }
}

/// Why a wait ended without the socket becoming ready.
#[derive(Debug)]
pub(crate) enum IoFail {
    /// The deadline passed.
    Timeout,
    /// The server is stopping.
    Stopped,
    /// A socket error (reset, aborted, …).
    Io(io::Error),
}

enum Wake {
    Ready,
    Timeout,
    Stopped,
}

/// Waits for `own` or `stop` until `deadline` (`None` = forever). Resets the network-event record of `sock` when it
/// woke for its own event.
fn wait(sock: &Socket, own: &WsaEvent, stop: &WsaEvent, deadline: Option<Instant>) -> Result<Wake, io::Error> {
    let timeout = match deadline {
        None => INFINITE,
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Wake::Timeout);
            }
            // Round up so a wait never ends just before its deadline (no spin on the last millisecond).
            u32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(INFINITE - 1).min(INFINITE - 1)
        }
    };
    let handles = [stop.handle(), own.handle()];
    // SAFETY: both handles are valid event objects for the duration of the call.
    let r = unsafe { WSAWaitForMultipleEvents(&handles, false, timeout, false) }.0;
    match r {
        0 => Ok(Wake::Stopped),
        1 => {
            let mut events = WSANETWORKEVENTS::default();
            // SAFETY: `sock` and `own` are valid; `events` is a properly sized out-parameter. This also resets `own`.
            if unsafe { WSAEnumNetworkEvents(raw(sock), own.0, &mut events) } != 0 {
                // SAFETY: valid handle; keeps a failed enumeration from turning into a busy loop.
                let _ = unsafe { WSAResetEvent(own.0) };
            }
            Ok(Wake::Ready)
        }
        WSA_WAIT_TIMEOUT => Ok(Wake::Timeout),
        WSA_WAIT_FAILED => Err(last_wsa_error()),
        other => Err(io::Error::other(format!("unexpected wait result {other}"))),
    }
}

/// The listening socket, event-selected for `FD_ACCEPT` (which also makes it non-blocking).
pub(crate) struct Listener {
    sock: Socket,
    event: WsaEvent,
}

/// What [`Listener::wait`] saw.
pub(crate) enum ListenerWake {
    /// Connections may be pending: call [`Listener::accept`] until it returns `None`.
    Ready,
    Stopped,
}

impl Listener {
    pub(crate) fn new(sock: Socket) -> io::Result<Self> {
        let event = WsaEvent::new()?;
        // SAFETY: valid socket and event; the association lives until the socket is closed.
        if unsafe { WSAEventSelect(raw(&sock), Some(event.0), FD_ACCEPT as i32) } != 0 {
            return Err(last_wsa_error());
        }
        Ok(Self { sock, event })
    }

    /// Blocks (without timeout and without polling) until a connection is pending or `stop` is set.
    pub(crate) fn wait(&self, stop: &WsaEvent) -> io::Result<ListenerWake> {
        match wait(&self.sock, &self.event, stop, None)? {
            Wake::Stopped => Ok(ListenerWake::Stopped),
            Wake::Ready | Wake::Timeout => Ok(ListenerWake::Ready),
        }
    }

    /// Sleeps for `pause` unless `stop` is set first (`true` = stopped). Used to back off after accept errors.
    pub(crate) fn pause(&self, stop: &WsaEvent, pause: Duration) -> bool {
        let handles = [stop.handle()];
        let ms = u32::try_from(pause.as_millis()).unwrap_or(1000);
        // SAFETY: valid event handle for the duration of the call.
        unsafe { WSAWaitForMultipleEvents(&handles, false, ms, false) }.0 == 0
    }

    /// The next pending connection, or `None` when there is none right now.
    pub(crate) fn accept(&self) -> io::Result<Option<Socket>> {
        match self.sock.accept() {
            Ok((sock, _)) => Ok(Some(sock)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// An accepted connection: non-blocking, abortive close on drop (unless [`Conn::close_graceful`]), its own event for
/// `FD_READ | FD_WRITE | FD_CLOSE`.
pub(crate) struct Conn {
    // Field order: the socket is closed before its event object.
    sock: Socket,
    event: WsaEvent,
}

impl Conn {
    pub(crate) fn new(sock: Socket) -> io::Result<Self> {
        sock.set_linger(Some(Duration::ZERO))?;
        // Best effort: Node's server sets no-delay too; failing it only costs latency.
        let _ = sock.set_tcp_nodelay(true);
        let event = WsaEvent::new()?;
        // SAFETY: valid socket and event. This replaces the FD_ACCEPT association inherited from the listener and
        // keeps the socket non-blocking.
        if unsafe { WSAEventSelect(raw(&sock), Some(event.0), (FD_READ | FD_WRITE | FD_CLOSE) as i32) } != 0 {
            return Err(last_wsa_error());
        }
        Ok(Self { sock, event })
    }

    /// Reads whatever is available (at least one byte, waiting for it until `deadline`) and appends it to `buf`.
    /// `Ok(0)` = the peer closed its side.
    pub(crate) fn read_into(&self, buf: &mut Vec<u8>, deadline: Instant, stop: &WsaEvent) -> Result<usize, IoFail> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match (&self.sock).read(&mut chunk) {
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(deadline, stop)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(IoFail::Io(e)),
            }
        }
    }

    /// Writes all of `data`, waiting for buffer space until `deadline`.
    pub(crate) fn write_all(&self, data: &[u8], deadline: Instant, stop: &WsaEvent) -> Result<(), IoFail> {
        let mut off = 0;
        while off < data.len() {
            match (&self.sock).write(&data[off..]) {
                Ok(0) => return Err(IoFail::Io(io::ErrorKind::WriteZero.into())),
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(deadline, stop)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(IoFail::Io(e)),
            }
        }
        Ok(())
    }

    /// Sends a FIN behind everything written so far (Node's `socket.end()`): the client reads the answer, then EOF.
    /// Receiving still works.
    pub(crate) fn shutdown_send(&self) -> io::Result<()> {
        self.sock.shutdown(Shutdown::Write)
    }

    /// Closes without a reset: whatever is still queued is delivered, then a FIN (if not sent yet). Only for a
    /// connection whose client has closed its side, so the server is the passive closer (`CLOSE_WAIT` → `LAST_ACK`,
    /// no `TIME_WAIT`) or the FINs have been exchanged already. If clearing the linger option fails, the close stays
    /// abortive.
    pub(crate) fn close_graceful(self) {
        let _ = self.sock.set_linger(None);
        // `self` drops here: `closesocket` returns at once (non-blocking, no linger) and the stack finishes the close.
    }

    fn wait(&self, deadline: Instant, stop: &WsaEvent) -> Result<(), IoFail> {
        match wait(&self.sock, &self.event, stop, Some(deadline)) {
            Ok(Wake::Ready) => Ok(()),
            Ok(Wake::Timeout) => Err(IoFail::Timeout),
            Ok(Wake::Stopped) => Err(IoFail::Stopped),
            Err(e) => Err(IoFail::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_error_copy() {
        let e = |code| io::Error::from_raw_os_error(code);
        assert_eq!(listen_error(&e(10_048), 5000), "Port 5000 is already in use by another program");
        assert_eq!(
            listen_error(&e(10_013), 5000),
            "Port 5000 is reserved or blocked by Windows \u{2014} choose another port"
        );
        assert_eq!(listen_error(&e(10_049), 5000), "The loopback address 127.0.0.1 is not available");
        let other = e(10_022);
        assert_eq!(listen_error(&other, 5000), other.to_string());
    }

    #[test]
    fn exclusive_bind_refuses_a_second_listener() {
        let first = bind_exclusive(0).expect("bind");
        let port = bound_port(&first).expect("port");
        assert_ne!(port, 0);
        let second = bind_exclusive(port).expect_err("port is taken");
        assert_eq!(listen_error(&second, port), vox_core::copy::port_in_use(port));
        // A plain (non-exclusive) bind cannot sit on top of ours either.
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
        drop(first);
        let again = bind_exclusive(port).expect("free again after close");
        drop(again);
    }

    #[test]
    fn stop_event_wakes_a_listener_wait() {
        let listener = Listener::new(bind_exclusive(0).expect("bind")).expect("listener");
        let stop = std::sync::Arc::new(WsaEvent::new().expect("event"));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            s2.set();
        });
        let started = Instant::now();
        assert!(matches!(listener.wait(&stop), Ok(ListenerWake::Stopped)));
        assert!(started.elapsed() < Duration::from_secs(5));
        t.join().expect("join");
        assert!(listener.pause(&stop, Duration::from_secs(5)));
    }
}
