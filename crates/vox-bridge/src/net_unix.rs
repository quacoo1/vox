//! Unix socket plumbing for the bridge (private; the Linux counterpart of `net.rs`, same API).
//!
//! - [`bind_exclusive`]: a `socket2` listener on `127.0.0.1:<port>`. Linux never lets a second socket steal a bound
//!   port the way Windows does without `SO_EXCLUSIVEADDRUSE`, so `SO_REUSEADDR` is set (it only allows re-binding over
//!   `TIME_WAIT` leftovers).
//! - Non-blocking sockets and `poll(2)` over `[stop, socket]` with the remaining time to a deadline. The stop signal
//!   ([`WsaEvent`], named after its Windows twin) is an `eventfd` that is written once and never drained, so it stays
//!   readable and wakes every thread of a server at once.
//! - Accepted sockets get `SO_LINGER {on, 0}` and `TCP_NODELAY` up front, as on Windows.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Node's default listen backlog.
const BACKLOG: i32 = 511;

/// The listen-error copy of integrations.md §1.2 for a failed bind/listen.
pub(crate) fn listen_error(err: &io::Error, port: u16) -> String {
    match err.raw_os_error() {
        Some(libc::EADDRINUSE) => vox_core::copy::port_in_use(port),
        Some(libc::EACCES) => vox_core::copy::port_blocked(port),
        Some(libc::EADDRNOTAVAIL) => vox_core::copy::LOOPBACK_UNAVAILABLE.to_string(),
        _ => err.to_string(),
    }
}

/// A bound, listening TCP socket on `127.0.0.1:<port>` (port 0 = ephemeral).
pub(crate) fn bind_exclusive(port: u16) -> io::Result<Socket> {
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
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

/// A manual-reset event: an `eventfd` that becomes (and stays) readable once [`WsaEvent::set`] ran.
#[derive(Debug)]
pub(crate) struct WsaEvent(OwnedFd);

impl WsaEvent {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: plain fd creation; ownership moves into the OwnedFd.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh, valid descriptor owned by nobody else.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub(crate) fn set(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: writes 8 bytes from a live buffer to our own eventfd.
        if unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), one.len()) } < 0 {
            tracing::warn!(target: "bridge", "bridge: could not signal stop: {}", io::Error::last_os_error());
        }
    }

    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
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

fn timeout_ms(deadline: Option<Instant>) -> Option<i32> {
    match deadline {
        None => Some(-1),
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            // Round up so a wait never ends just before its deadline.
            Some(i32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(i32::MAX))
        }
    }
}

/// Waits until `sock` has any of `events`, `stop` is set, or `deadline` passes (`None` = forever).
fn wait(sock: RawFd, events: libc::c_short, stop: &WsaEvent, deadline: Option<Instant>) -> Result<Wake, io::Error> {
    loop {
        let Some(timeout) = timeout_ms(deadline) else { return Ok(Wake::Timeout) };
        let mut fds = [
            libc::pollfd { fd: stop.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: sock, events, revents: 0 },
        ];
        // SAFETY: `fds` is a live array of two pollfd structs.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fds[0].revents != 0 {
            return Ok(Wake::Stopped);
        }
        if r == 0 {
            return Ok(Wake::Timeout);
        }
        return Ok(Wake::Ready);
    }
}

/// The listening socket (non-blocking).
pub(crate) struct Listener {
    sock: Socket,
}

/// What [`Listener::wait`] saw.
pub(crate) enum ListenerWake {
    /// Connections may be pending: call [`Listener::accept`] until it returns `None`.
    Ready,
    Stopped,
}

impl Listener {
    pub(crate) fn new(sock: Socket) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        Ok(Self { sock })
    }

    /// Blocks (without timeout and without polling) until a connection is pending or `stop` is set.
    pub(crate) fn wait(&self, stop: &WsaEvent) -> io::Result<ListenerWake> {
        match wait(self.sock.as_raw_fd(), libc::POLLIN, stop, None)? {
            Wake::Stopped => Ok(ListenerWake::Stopped),
            Wake::Ready | Wake::Timeout => Ok(ListenerWake::Ready),
        }
    }

    /// Sleeps for `pause` unless `stop` is set first (`true` = stopped). Used to back off after accept errors.
    pub(crate) fn pause(&self, stop: &WsaEvent, pause: Duration) -> bool {
        let ms = i32::try_from(pause.as_millis()).unwrap_or(1000);
        let mut fds = [libc::pollfd { fd: stop.fd(), events: libc::POLLIN, revents: 0 }];
        // SAFETY: `fds` is a live array of one pollfd struct.
        unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) > 0 }
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

/// An accepted connection: non-blocking, abortive close on drop (unless [`Conn::close_graceful`]).
pub(crate) struct Conn {
    sock: Socket,
}

impl Conn {
    pub(crate) fn new(sock: Socket) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        sock.set_linger(Some(Duration::ZERO))?;
        let _ = sock.set_tcp_nodelay(true);
        Ok(Self { sock })
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
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(libc::POLLIN, deadline, stop)?,
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
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(libc::POLLOUT, deadline, stop)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(IoFail::Io(e)),
            }
        }
        Ok(())
    }

    /// Sends a FIN behind everything written so far (Node's `socket.end()`): the client reads the answer, then EOF.
    pub(crate) fn shutdown_send(&self) -> io::Result<()> {
        self.sock.shutdown(Shutdown::Write)
    }

    /// Closes without a reset: whatever is still queued is delivered, then a FIN (if not sent yet).
    pub(crate) fn close_graceful(self) {
        let _ = self.sock.set_linger(None);
    }

    fn wait(&self, events: libc::c_short, deadline: Instant, stop: &WsaEvent) -> Result<(), IoFail> {
        match wait(self.sock.as_raw_fd(), events, stop, Some(deadline)) {
            Ok(Wake::Ready) => Ok(()),
            Ok(Wake::Timeout) => Err(IoFail::Timeout),
            Ok(Wake::Stopped) => Err(IoFail::Stopped),
            Err(e) => Err(IoFail::Io(e)),
        }
    }
}
