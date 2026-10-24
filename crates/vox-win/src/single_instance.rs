//! Single instance (shell.md §3.3; ARCHITECTURE.md D-SH-5): a named mutex plus a named pipe on which the primary
//! receives `{argv, cwd}` (JSON) from later launches, both keyed on the profile, **the current user and the logon
//! session**: mutex `Local\<key>-<user SID>`, pipe `\\.\pipe\<key>-<user SID>-<session id>` (`key` =
//! `com.vox.reader` or `com.vox.reader-Demo`; the SID string from `GetTokenInformation(TokenUser)` +
//! `ConvertSidToStringSidW`, the session from `TokenSessionId`). The pipe namespace is machine-wide, so without the
//! SID user B's launches (fast user switching, RDP) could forward argv — `--read-text` content, file paths — to user
//! A's Vox; Electron's singleton never crossed users. The session id gives the pipe the same scope as the `Local\`
//! (per-session) mutex: when one user has two interactive sessions (multi-session Windows / RDS), each session has
//! its own primary and its launches reach that primary, never the other session's.
//!
//! - [`acquire`] creates the mutex **and the first pipe instance** (`CreateNamedPipeW` with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE | PIPE_REJECT_REMOTE_CLIENTS`, a DACL granting only the current user SID and
//!   SYSTEM), so there is no window in which the mutex is held but the pipe is absent: a secondary launched during a
//!   slow startup connects and writes into the pipe buffer, and the primary reads it once [`InstanceServer::listen`]
//!   runs (vox-app calls it right after `acquire`, before opening any store; shell.md §3.3).
//! - [`forward_to_primary`] connects (waits ≤ 5 s), calls `GetNamedPipeServerProcessId`, checks that the server
//!   process token's user SID and session equal ours, and only then calls `AllowSetForegroundWindow(pid)` and writes
//!   the JSON; then the secondary exits.
//! - argv is read with `std::env::args_os()` and converted with `to_string_lossy()` (an unpaired surrogate in an
//!   NTFS name becomes U+FFFD; never `env::args()`, which panics), and the pipe JSON carries those lossy strings; such
//!   a path then fails to open with the normal `Couldn’t open <name>` toast (accepted limitation).
//! - The primary handles forwarded args only after its own startup work finished (the core defers `Cli` commands
//!   and processes them FIFO after the primary's own initial `Cli`).
//!
//! Implementation: one pipe instance (`nMaxInstances = 1`, so no other process can add a server instance), served by
//! one thread with overlapped I/O: connect, read until the client closes (≤ 32 MiB, ≤ 5 s per message), disconnect,
//! hand the message to the handler, repeat; [`InstanceListener::shutdown`] wakes it through an event. A secondary that
//! finds the pipe busy waits with `WaitNamedPipeW`. Logs never contain the forwarded arguments (they may hold
//! `--read-text` content), only their count.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    LocalFree, ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, ERROR_SHARING_VIOLATION, GENERIC_READ,
    GENERIC_WRITE, HANDLE, HLOCAL, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, TokenSessionId, TokenUser, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, ResetEvent, SetEvent,
    WaitForMultipleObjects, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, FindWindowExW, GetWindowTextW, HWND_MESSAGE};

use crate::window::{string_from_wide, wide};

/// What a later launch forwarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedArgs {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

/// The lock outcome.
pub enum InstanceRole {
    /// We are the first instance and own the first pipe instance: call [`InstanceServer::listen`] right away (its
    /// handler only enqueues `Command::Cli`).
    Primary(InstanceServer),
    /// Another instance runs; forward with [`forward_to_primary`] and exit.
    Secondary,
}

/// The primary's pipe server: owns the per-user mutex and the first pipe instance (not reading yet).
pub struct InstanceServer {
    key: String,
    mutex: OwnedHandle,
    /// `None` when the pipe could not be created (logged): the app still runs as the only instance, but later launches
    /// cannot forward to it.
    pipe: Option<OwnedHandle>,
}

/// Listener callback (called on the pipe thread).
pub type ForwardHandler = Arc<dyn Fn(ForwardedArgs) + Send + Sync>;

/// The largest forwarded message accepted (a `--read-text` argument can be long).
const MAX_MESSAGE: usize = 32 * 1024 * 1024;
/// How long one connected client may take to deliver its message.
const MESSAGE_DEADLINE: Duration = Duration::from_secs(5);
/// How long a secondary waits for the primary's pipe.
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);
/// How long a secondary may take to write its message.
const WRITE_DEADLINE: Duration = Duration::from_secs(10);

impl InstanceServer {
    /// Starts the pipe thread.
    pub fn listen(self, on_args: ForwardHandler) -> Result<InstanceListener, crate::WinError> {
        let InstanceServer { key, mutex, pipe } = self;
        let Some(pipe) = pipe else {
            return Ok(InstanceListener { stop: None, thread: None, _mutex: mutex });
        };
        let stop = Arc::new(new_event()?);
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("vox-instance-pipe".into())
            .spawn(move || serve(&key, pipe, &thread_stop, on_args))
            .map_err(|e| crate::WinError(format!("cannot start the instance pipe thread: {e}")))?;
        Ok(InstanceListener { stop: Some(stop), thread: Some(thread), _mutex: mutex })
    }
}

/// Keeps the pipe thread alive.
pub struct InstanceListener {
    stop: Option<Arc<OwnedHandle>>,
    thread: Option<JoinHandle<()>>,
    /// The per-user lock, released when the listener goes away.
    _mutex: OwnedHandle,
}

impl InstanceListener {
    pub fn shutdown(mut self) {
        self.stop_thread();
    }

    fn stop_thread(&mut self) {
        if let Some(stop) = self.stop.take() {
            // SAFETY: a live event handle we own.
            if let Err(e) = unsafe { SetEvent(handle(&stop)) } {
                tracing::warn!(target: "instance", "instance pipe stop not signalled: {e}");
                return;
            }
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::warn!(target: "instance", "instance pipe thread panicked");
            }
        }
    }
}

impl Drop for InstanceListener {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

/// Takes the per-user lock for `key`; as primary also creates the first pipe instance (module docs).
pub fn acquire(key: &str) -> Result<InstanceRole, crate::WinError> {
    let sid = current_user_sid()?;
    let session = token_session(None)?;
    let name = wide(&mutex_name(key, &sid));
    // SAFETY: the name outlives the call; default security (the Local namespace is per session).
    let mutex = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .map_err(|e| crate::WinError(format!("cannot create the instance lock: {}", e.message())))?;
    // Read right after the call that set it: ERROR_ALREADY_EXISTS means another instance holds the name.
    let existed = windows::core::Error::from_thread().code() == ERROR_ALREADY_EXISTS.to_hresult();
    // SAFETY: a fresh handle we own from here on.
    let mutex = unsafe { OwnedHandle::from_raw_handle(mutex.0) };
    if existed {
        return Ok(InstanceRole::Secondary);
    }
    let pipe = match create_first_pipe(&pipe_name(key, &sid, session), &sid) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(target: "instance", "instance pipe unavailable; later launches cannot forward: {}", e.0);
            None
        }
    };
    Ok(InstanceRole::Primary(InstanceServer { key: key.to_string(), mutex, pipe }))
}

/// Sends argv + cwd to the primary of the **same user in the same session** (waits ≤ 5 s for the pipe; verifies the
/// server process's user SID and session before `AllowSetForegroundWindow` and writing). Tested between two
/// processes of one user (`forwarding_works_between_two_processes`); another user's or another session's launch
/// never reaches this primary (a different pipe name, and the checks here).
pub fn forward_to_primary(key: &str, args: &ForwardedArgs) -> Result<(), crate::WinError> {
    let own = token_user(None)?;
    let sid = sid_string(&own)?;
    let session = token_session(None)?;
    let name = wide(&pipe_name(key, &sid, session));
    let pipe = open_pipe(&name)?;
    let mut pid = 0u32;
    // SAFETY: a connected pipe handle and an out-parameter that outlives the call.
    unsafe { GetNamedPipeServerProcessId(handle(&pipe), &mut pid) }
        .map_err(|e| crate::WinError(format!("cannot identify the running Vox: {}", e.message())))?;
    let server = token_user(Some(pid))?;
    // SAFETY: both buffers hold TOKEN_USER structures whose SIDs point into the same buffers.
    if unsafe { EqualSid(user_sid(&server), user_sid(&own)) }.is_err() {
        return Err(crate::WinError("the instance pipe belongs to another user".into()));
    }
    if token_session(Some(pid))? != session {
        return Err(crate::WinError("the instance pipe belongs to another session".into()));
    }
    // SAFETY: plain value argument; lets the primary bring its window to the front.
    if let Err(e) = unsafe { AllowSetForegroundWindow(pid) } {
        tracing::debug!(target: "instance", "AllowSetForegroundWindow failed: {e}");
    }
    let message = encode_forwarded(args);
    let event = new_event()?;
    let mut written = 0usize;
    let deadline = Instant::now() + WRITE_DEADLINE;
    while written < message.len() {
        let chunk = &message[written..message.len().min(written + 64 * 1024)];
        match overlapped(&pipe, &event, None, deadline, |ov| {
            // SAFETY: `chunk` and `ov` stay alive until the operation completes (`overlapped` waits for it).
            unsafe { WriteFile(handle(&pipe), Some(chunk), None, Some(ov)) }.map(|()| Issued::Queued)
        }) {
            Io::Done(0) => return Err(crate::WinError("the running Vox stopped reading".into())),
            Io::Done(n) => written += n as usize,
            Io::Failed(e) => {
                return Err(crate::WinError(format!("cannot forward to the running Vox: {}", e.message())))
            }
            Io::TimedOut | Io::Stopped => {
                return Err(crate::WinError("timed out forwarding to the running Vox".into()))
            }
        }
    }
    Ok(())
}

/// Is an **Electron** build of Vox running on this profile? (BRIEF.md "Do not run both builds at once";
/// ARCHITECTURE.md §4.18.) Chromium's process singleton (which Electron's `requestSingleInstanceLock` uses) keeps a
/// message-only window of class `Chrome_MessageWindow` whose title is the user-data directory, so: enumerate
/// `FindWindowExW(HWND_MESSAGE, …, "Chrome_MessageWindow", NULL)` and compare each title with `profile_root`
/// case-insensitively after normalising separators and a trailing `\`. Fallback evidence, used only if no window
/// matched: `<profile_root>\lockfile` exists and cannot be opened with `FILE_SHARE_DELETE` (Chromium holds it with
/// delete-on-close). Any API failure counts as "not running". Takes < 5 ms; called once at startup by the primary
/// instance before any store is opened.
pub fn electron_instance_running(profile_root: &Path) -> bool {
    let wanted = normalize_profile_path(&profile_root.to_string_lossy());
    if wanted.is_empty() {
        return false;
    }
    let class = wide("Chrome_MessageWindow");
    let mut after = None;
    for _ in 0..4096 {
        // SAFETY: the class name outlives the call; `after` is a window returned by the previous call.
        let Ok(found) = (unsafe { FindWindowExW(Some(HWND_MESSAGE), after, PCWSTR(class.as_ptr()), PCWSTR::null()) })
        else {
            break;
        };
        let mut title = [0u16; 1024];
        // SAFETY: `title` outlives the call (another process's window: no message is sent).
        let n = unsafe { GetWindowTextW(found, &mut title) };
        if n > 0 && normalize_profile_path(&string_from_wide(&title[..n as usize])) == wanted {
            return true;
        }
        after = Some(found);
    }
    lockfile_held(&profile_root.join("lockfile"))
}

/// Chromium holds `lockfile` with `FILE_FLAG_DELETE_ON_CLOSE`: opening it without `FILE_SHARE_DELETE` fails with a
/// sharing violation while the browser process lives.
fn lockfile_held(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let name = HSTRING::from(path);
    // SAFETY: the name outlives the call; the handle (if any) is closed right away.
    match unsafe {
        CreateFileW(
            &name,
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    } {
        Ok(h) => {
            // SAFETY: closes the handle we just opened.
            drop(unsafe { OwnedHandle::from_raw_handle(h.0) });
            false
        }
        Err(e) => e.code() == ERROR_SHARING_VIOLATION.to_hresult(),
    }
}

/// Lower-case, `/` → `\`, without trailing separators.
fn normalize_profile_path(path: &str) -> String {
    path.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn mutex_name(key: &str, sid: &str) -> String {
    format!(r"Local\{key}-{sid}")
}

/// Per user **and session**, like the `Local\` mutex (module docs).
fn pipe_name(key: &str, sid: &str, session: u32) -> String {
    format!(r"\\.\pipe\{key}-{sid}-{session}")
}

/// The pipe message: `{"argv": [...], "cwd": "..."}` (UTF-8 JSON).
fn encode_forwarded(args: &ForwardedArgs) -> Vec<u8> {
    serde_json::json!({ "argv": args.argv, "cwd": args.cwd.to_string_lossy() }).to_string().into_bytes()
}

/// Parses a pipe message (shell.md §3.3: `argv` only when it is an array — non-strings dropped —, `cwd` only when a
/// string, else `fallback_cwd`). `None` when it is not a JSON object.
fn decode_forwarded(bytes: &[u8], fallback_cwd: &Path) -> Option<ForwardedArgs> {
    let text = String::from_utf8_lossy(bytes);
    let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{FEFF}')).ok()?;
    let obj = value.as_object()?;
    let argv = obj
        .get("argv")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let cwd = obj.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from).unwrap_or_else(|| fallback_cwd.to_path_buf());
    Some(ForwardedArgs { argv, cwd })
}

/// The pipe thread: one client at a time (module docs).
fn serve(key: &str, pipe: OwnedHandle, stop: &OwnedHandle, on_args: ForwardHandler) {
    let event = match new_event() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(target: "instance", "instance pipe not served: {}", e.0);
            return;
        }
    };
    let fallback_cwd = std::env::current_dir().unwrap_or_default();
    let mut failures = 0u32;
    loop {
        let connected = overlapped(&pipe, &event, Some(stop), far_future(), |ov| {
            // SAFETY: `ov` stays alive until the operation completes (`overlapped` waits for it).
            match unsafe { ConnectNamedPipe(handle(&pipe), Some(ov)) } {
                // A client that connected before this call (`ERROR_PIPE_CONNECTED`), or that already wrote and closed
                // its end (`ERROR_NO_DATA`): read what it left in the pipe.
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() || e.code() == ERROR_NO_DATA.to_hresult() => {
                    Ok(Issued::AlreadyDone)
                }
                other => other.map(|()| Issued::Queued),
            }
        });
        match connected {
            Io::Done(_) => failures = 0,
            Io::Stopped => return,
            Io::TimedOut => continue,
            Io::Failed(e) => {
                failures += 1;
                tracing::warn!(target: "instance", "instance pipe connect failed: {}", e.message());
                // SAFETY: resets our own pipe instance for the next client.
                let _ = unsafe { DisconnectNamedPipe(handle(&pipe)) };
                if failures >= 5 {
                    tracing::warn!(target: "instance", "instance pipe for {key} stopped after repeated failures");
                    return;
                }
                continue;
            }
        }
        let message = read_message(&pipe, &event, stop);
        // SAFETY: drops the client (any unread data is discarded) so the next one can connect.
        let _ = unsafe { DisconnectNamedPipe(handle(&pipe)) };
        let Some(bytes) = message else {
            // Stopped while reading.
            return;
        };
        match decode_forwarded(&bytes, &fallback_cwd) {
            Some(args) => {
                tracing::info!(target: "instance", "second instance forwarded {} arguments", args.argv.len());
                let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_args(args)));
                if call.is_err() {
                    tracing::warn!(target: "instance", "second-instance handler panicked");
                }
            }
            None if bytes.is_empty() => {}
            None => {
                tracing::warn!(target: "instance", "ignored a malformed second-instance message ({} bytes)", bytes.len())
            }
        }
    }
}

/// Reads until the client closes its end (`Some(bytes)`; empty when it timed out, failed or was too large) or the
/// listener stops (`None`).
fn read_message(pipe: &OwnedHandle, event: &OwnedHandle, stop: &OwnedHandle) -> Option<Vec<u8>> {
    let deadline = Instant::now() + MESSAGE_DEADLINE;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = overlapped(pipe, event, Some(stop), deadline, |ov| {
            // SAFETY: `buf` and `ov` stay alive until the operation completes (`overlapped` waits for it).
            unsafe { ReadFile(handle(pipe), Some(&mut buf), None, Some(ov)) }.map(|()| Issued::Queued)
        });
        match read {
            Io::Done(n) => {
                out.extend_from_slice(&buf[..(n as usize).min(buf.len())]);
                if out.len() > MAX_MESSAGE {
                    tracing::warn!(target: "instance", "second-instance message too large; ignored");
                    return Some(Vec::new());
                }
            }
            Io::Failed(e) if is_end_of_message(&e) => return Some(out),
            Io::Failed(e) => {
                tracing::warn!(target: "instance", "second-instance message unreadable: {}", e.message());
                return Some(Vec::new());
            }
            Io::TimedOut => {
                tracing::warn!(target: "instance", "second-instance message timed out");
                return Some(Vec::new());
            }
            Io::Stopped => return None,
        }
    }
}

/// The client closed its end: everything it wrote has been read.
fn is_end_of_message(e: &windows::core::Error) -> bool {
    [ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED].iter().any(|c| e.code() == c.to_hresult())
}

/// The outcome of one overlapped operation.
enum Io {
    Done(u32),
    Failed(windows::core::Error),
    TimedOut,
    Stopped,
}

/// What issuing an overlapped operation did.
enum Issued {
    /// Queued or completed through the overlapped machinery: the event is (or will be) signalled.
    Queued,
    /// Nothing was queued and nothing is left to wait for (`ConnectNamedPipe` found a client already there).
    AlreadyDone,
}

/// Issues one overlapped operation on `file` and waits for it, the `stop` event or the deadline; a cancelled
/// operation is always waited for, so the buffers it uses can be released afterwards.
fn overlapped(
    file: &OwnedHandle,
    event: &OwnedHandle,
    stop: Option<&OwnedHandle>,
    deadline: Instant,
    issue: impl FnOnce(&mut OVERLAPPED) -> windows::core::Result<Issued>,
) -> Io {
    // SAFETY: a live manual-reset event we own.
    if let Err(e) = unsafe { ResetEvent(handle(event)) } {
        return Io::Failed(e);
    }
    let mut ov = OVERLAPPED { hEvent: handle(event), ..Default::default() };
    match issue(&mut ov) {
        Ok(Issued::AlreadyDone) => return Io::Done(0),
        Ok(Issued::Queued) => {}
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
        Err(e) => return Io::Failed(e),
    }
    let mut handles = vec![handle(event)];
    if let Some(stop) = stop {
        handles.push(handle(stop));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let ms = remaining.as_millis().min(u32::MAX as u128 - 1) as u32;
    // SAFETY: live handles.
    let waited = unsafe { WaitForMultipleObjects(&handles, false, ms) };
    if waited == WAIT_OBJECT_0 {
        let mut n = 0u32;
        // SAFETY: the operation completed; `ov` is the one it used.
        return match unsafe { GetOverlappedResult(handle(file), &ov, &mut n, false) } {
            Ok(()) => Io::Done(n),
            Err(e) => Io::Failed(e),
        };
    }
    let outcome = if waited.0 == WAIT_OBJECT_0.0 + 1 {
        Io::Stopped
    } else if waited == WAIT_TIMEOUT {
        Io::TimedOut
    } else {
        Io::Failed(windows::core::Error::from_thread())
    };
    let mut n = 0u32;
    // SAFETY: cancels our own pending operation and waits until it no longer uses `ov` or its buffer.
    unsafe {
        let _ = CancelIoEx(handle(file), Some(&ov));
        let _ = GetOverlappedResult(handle(file), &ov, &mut n, true);
    }
    outcome
}

/// A deadline that never comes (the connect wait ends only with a client or the stop event).
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)
}

/// Connects to the primary's pipe, waiting while it is busy or not created yet (≤ 5 s).
fn open_pipe(name: &[u16]) -> Result<OwnedHandle, crate::WinError> {
    let deadline = Instant::now() + CONNECT_DEADLINE;
    loop {
        // SAFETY: the name outlives the call.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_WRITE.0 | FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        match opened {
            // SAFETY: a fresh handle we own.
            Ok(h) => return Ok(unsafe { OwnedHandle::from_raw_handle(h.0) }),
            Err(e) if remaining.is_zero() => {
                return Err(crate::WinError(format!("the running Vox did not answer: {}", e.message())))
            }
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                // SAFETY: the name outlives the call.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), remaining.as_millis().clamp(1, 5000) as u32) };
            }
            // The primary holds the lock but may be creating its pipe right now.
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(crate::WinError(format!("cannot reach the running Vox: {}", e.message()))),
        }
    }
}

/// The first (and only) instance of the pipe, readable by the current user and SYSTEM only.
fn create_first_pipe(name: &str, sid: &str) -> Result<OwnedHandle, crate::WinError> {
    let sddl = HSTRING::from(format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sd` receives a LocalAlloc'ed descriptor that is freed below.
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None) }
        .map_err(|e| crate::WinError(format!("cannot build the pipe security: {}", e.message())))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let name = wide(name);
    // SAFETY: the name and the security attributes outlive the call.
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            64 * 1024,
            0,
            Some(&attributes),
        )
    };
    let error = windows::core::Error::from_thread();
    // SAFETY: frees the descriptor allocated above (the pipe keeps its own copy).
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    if pipe.is_invalid() {
        return Err(crate::WinError(format!("CreateNamedPipeW failed: {}", error.message())));
    }
    // SAFETY: a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(pipe.0) })
}

fn new_event() -> Result<OwnedHandle, crate::WinError> {
    // SAFETY: an unnamed manual-reset event.
    let h = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(crate::WinError::from)?;
    // SAFETY: a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
}

fn handle(h: &OwnedHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

fn token_error(what: &str, e: windows::core::Error) -> crate::WinError {
    crate::WinError(format!("{what}: {}", e.message()))
}

/// The token of a process (`None` = ours), opened for `TOKEN_QUERY`.
fn open_token(pid: Option<u32>) -> Result<OwnedHandle, crate::WinError> {
    // SAFETY: the pseudo handle needs no closing; an opened process handle is owned below.
    let process = match pid {
        None => None,
        Some(pid) => Some(
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                // SAFETY: a fresh handle we own.
                .map(|h| unsafe { OwnedHandle::from_raw_handle(h.0) })
                .map_err(|e| token_error("cannot open the process", e))?,
        ),
    };
    let process_handle = process.as_ref().map_or_else(|| unsafe { GetCurrentProcess() }, handle);
    let mut token = HANDLE::default();
    // SAFETY: `token` receives a handle that is owned (closed) below.
    unsafe { OpenProcessToken(process_handle, TOKEN_QUERY, &mut token) }
        .map_err(|e| token_error("cannot open the process token", e))?;
    // SAFETY: a fresh handle we own.
    Ok(unsafe { OwnedHandle::from_raw_handle(token.0) })
}

/// The logon session id of a process (`None` = ours): `GetTokenInformation(TokenSessionId)`.
fn token_session(pid: Option<u32>) -> Result<u32, crate::WinError> {
    let token = open_token(pid)?;
    let mut session = 0u32;
    let mut needed = 0u32;
    // SAFETY: `session` is a DWORD-sized out-buffer that outlives the call.
    unsafe {
        GetTokenInformation(
            handle(&token),
            TokenSessionId,
            Some((&mut session as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut needed,
        )
    }
    .map_err(|e| token_error("cannot read the token session", e))?;
    Ok(session)
}

/// The `TOKEN_USER` of a process (`None` = ours), in a pointer-aligned buffer.
fn token_user(pid: Option<u32>) -> Result<Vec<u64>, crate::WinError> {
    let token = open_token(pid)?;
    let mut needed = 0u32;
    // SAFETY: a size query; the expected "insufficient buffer" error is ignored.
    let _ = unsafe { GetTokenInformation(handle(&token), TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err(crate::WinError("cannot read the token user".into()));
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes, 8-byte aligned for TOKEN_USER.
    unsafe { GetTokenInformation(handle(&token), TokenUser, Some(buf.as_mut_ptr().cast()), needed, &mut needed) }
        .map_err(|e| token_error("cannot read the token user", e))?;
    Ok(buf)
}

/// The user SID inside a `TOKEN_USER` buffer from [`token_user`].
fn user_sid(buf: &[u64]) -> PSID {
    // SAFETY: the buffer was filled by GetTokenInformation(TokenUser); the SID points into the same buffer.
    unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid }
}

/// The current user's SID as `S-1-5-21-…`.
fn current_user_sid() -> Result<String, crate::WinError> {
    sid_string(&token_user(None)?)
}

fn sid_string(token_user: &[u64]) -> Result<String, crate::WinError> {
    let mut text = PWSTR::null();
    // SAFETY: a valid SID; `text` receives a LocalAlloc'ed string that is freed below.
    unsafe { ConvertSidToStringSidW(user_sid(token_user), &mut text) }.map_err(crate::WinError::from)?;
    // SAFETY: a NUL-terminated string from the call above, read once and freed.
    let s = unsafe {
        let s = string_from_wide(text.as_wide());
        let _ = LocalFree(Some(HLOCAL(text.0 as *mut _)));
        s
    };
    if s.is_empty() {
        Err(crate::WinError("empty user SID".into()))
    } else {
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_per_user_and_per_profile() {
        let sid = "S-1-5-21-1-2-3-1001";
        assert_eq!(mutex_name("com.vox.reader", sid), r"Local\com.vox.reader-S-1-5-21-1-2-3-1001");
        // The pipe has the mutex's scope: per user and per session.
        assert_eq!(pipe_name("com.vox.reader-Demo", sid, 2), r"\\.\pipe\com.vox.reader-Demo-S-1-5-21-1-2-3-1001-2");
        assert_ne!(pipe_name("com.vox.reader", sid, 1), pipe_name("com.vox.reader", sid, 2));
        let me = current_user_sid().expect("sid");
        assert!(me.starts_with("S-1-"), "{me}");
        // Our session read from our own token equals the one read through our process id.
        let session = token_session(None).expect("own session");
        assert_eq!(token_session(Some(std::process::id())).expect("session by pid"), session);
    }

    #[test]
    fn forwarded_args_round_trip_and_bad_messages_are_tolerated() {
        let args = ForwardedArgs {
            argv: vec![r"C:\Vox\vox.exe".into(), "--read-text".into(), "héllo \"world\"\n".into()],
            cwd: PathBuf::from(r"C:\Users\Kwaku\Documents"),
        };
        let fallback = Path::new(r"C:\fallback");
        assert_eq!(decode_forwarded(&encode_forwarded(&args), fallback), Some(args));
        // argv only when an array (non-strings dropped); cwd only when a string.
        assert_eq!(
            decode_forwarded(br#"{"argv":["a",1,null,"b"],"cwd":7}"#, fallback),
            Some(ForwardedArgs { argv: vec!["a".into(), "b".into()], cwd: fallback.to_path_buf() })
        );
        assert_eq!(
            decode_forwarded(br#"{"argv":"--hidden","cwd":"D:\\x"}"#, fallback),
            Some(ForwardedArgs { argv: vec![], cwd: PathBuf::from(r"D:\x") })
        );
        assert_eq!(decode_forwarded(b"[1,2]", fallback), None);
        assert_eq!(decode_forwarded(b"not json", fallback), None);
        assert_eq!(decode_forwarded(b"", fallback), None);
        // A UTF-8 BOM and invalid UTF-8 inside strings do not lose the message.
        let mut bom = "\u{FEFF}".as_bytes().to_vec();
        bom.extend_from_slice(br#"{"argv":["x"]}"#);
        assert_eq!(decode_forwarded(&bom, fallback).map(|a| a.argv), Some(vec!["x".to_string()]));
        assert_eq!(
            decode_forwarded(b"{\"argv\":[\"a\xFFb\"]}", fallback).map(|a| a.argv),
            Some(vec!["a\u{FFFD}b".to_string()])
        );
    }

    #[test]
    fn profile_paths_compare_like_chromium_titles() {
        let p = normalize_profile_path(r"C:\Users\Kwaku\AppData\Roaming\Vox");
        assert_eq!(normalize_profile_path("c:/users/kwaku/appdata/roaming/vox/"), p);
        assert_eq!(normalize_profile_path(r"C:\USERS\KWAKU\APPDATA\ROAMING\VOX\\"), p);
        assert_ne!(normalize_profile_path(r"C:\Users\Kwaku\AppData\Roaming\Vox-Demo"), p);
        assert_eq!(normalize_profile_path("  "), "");
    }

    #[test]
    fn no_electron_instance_for_a_fresh_folder() {
        let dir = std::env::temp_dir().join(format!("vox-electron-check-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert!(!electron_instance_running(&dir));
        // A lockfile nobody holds is not evidence.
        let lock = dir.join("lockfile");
        let _ = std::fs::write(&lock, b"");
        assert!(!lockfile_held(&lock));
        // Held with delete access (as Chromium's delete-on-close handle): evidence.
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _held = std::fs::OpenOptions::new()
                .write(true)
                .share_mode(FILE_SHARE_READ.0)
                .custom_flags(0x0400_0000) // FILE_FLAG_DELETE_ON_CLOSE
                .open(&lock)
                .expect("hold lockfile");
            assert!(lockfile_held(&lock));
        }
        assert!(!lockfile_held(&lock));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same user forwarding to itself through the real mutex and pipe (no desktop needed; unique key).
    #[test]
    fn a_second_launch_forwards_to_the_primary() {
        let key = format!("com.vox.reader-test-{}-{}", std::process::id(), line!());
        let InstanceRole::Primary(server) = acquire(&key).expect("acquire") else { panic!("expected primary") };
        assert!(matches!(acquire(&key).expect("second acquire"), InstanceRole::Secondary));
        let args = ForwardedArgs {
            argv: vec!["vox.exe".into(), "--read-file".into(), r"C:\notes.md".into()],
            cwd: PathBuf::from(r"C:\work"),
        };
        // Forwarded before the listener runs: buffered in the pipe, delivered once it listens.
        let early = args.clone();
        let early_key = key.clone();
        let writer = std::thread::spawn(move || forward_to_primary(&early_key, &early));
        std::thread::sleep(Duration::from_millis(200));
        let (tx, rx) = crossbeam_channel::unbounded();
        let listener = server
            .listen(Arc::new(move |a| {
                let _ = tx.send(a);
            }))
            .expect("listen");
        writer.join().expect("writer thread").expect("forward early");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).expect("early args"), args);
        // A big message (a long --read-text) in several chunks, then two launches back to back.
        let big = ForwardedArgs {
            argv: vec!["vox.exe".into(), "--read-text".into(), "x".repeat(300_000)],
            cwd: args.cwd.clone(),
        };
        forward_to_primary(&key, &big).expect("forward big");
        forward_to_primary(&key, &args).expect("forward again");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).expect("big args"), big);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).expect("second args"), args);
        listener.shutdown();
        // The lock is released with the listener: a new launch becomes primary.
        assert!(matches!(acquire(&key).expect("after shutdown"), InstanceRole::Primary(_)));
        assert!(forward_to_primary(&format!("{key}-absent"), &args).is_err());
    }

    /// Two processes of one user: this test is the primary; it re-runs its own test executable as the secondary
    /// ([`secondary_process_helper`]), which must see the lock taken and forward its arguments.
    #[test]
    fn forwarding_works_between_two_processes() {
        let key = format!("com.vox.reader-test-{}-{}", std::process::id(), line!());
        let InstanceRole::Primary(server) = acquire(&key).expect("acquire") else { panic!("expected primary") };
        let (tx, rx) = crossbeam_channel::unbounded();
        let listener = server
            .listen(Arc::new(move |a| {
                let _ = tx.send(a);
            }))
            .expect("listen");
        let status = std::process::Command::new(std::env::current_exe().expect("test exe"))
            .args(["--ignored", "--exact", "single_instance::tests::secondary_process_helper", "--test-threads=1"])
            .env(HELPER_KEY_VAR, &key)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run the secondary");
        assert!(status.success(), "secondary failed: {status:?}");
        let got = rx.recv_timeout(Duration::from_secs(5)).expect("forwarded args");
        assert_eq!(got.argv, vec!["vox.exe".to_string(), "--read-file".into(), r"C:\from-child.md".into()]);
        assert_eq!(got.cwd, PathBuf::from(r"C:\child"));
        listener.shutdown();
    }

    const HELPER_KEY_VAR: &str = "VOX_TEST_INSTANCE_KEY";

    /// Child half of [`forwarding_works_between_two_processes`]; does nothing unless that test runs it.
    #[test]
    #[ignore = "helper: started as a second process by forwarding_works_between_two_processes"]
    fn secondary_process_helper() {
        let Ok(key) = std::env::var(HELPER_KEY_VAR) else { return };
        assert!(matches!(acquire(&key).expect("acquire"), InstanceRole::Secondary), "the child must be secondary");
        let args = ForwardedArgs {
            argv: vec!["vox.exe".into(), "--read-file".into(), r"C:\from-child.md".into()],
            cwd: PathBuf::from(r"C:\child"),
        };
        forward_to_primary(&key, &args).expect("forward");
    }

    /// `cargo test -p vox-win electron_window_is_detected -- --ignored`: a fake Chromium singleton window
    /// (`Chrome_MessageWindow`, message-only, titled with a profile path) is found case-insensitively.
    #[test]
    #[ignore = "creates a message-only window on the interactive desktop"]
    fn electron_window_is_detected() {
        use windows::core::w;
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, DestroyWindow};

        unsafe extern "system" fn plain(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            // SAFETY: default handling.
            unsafe { DefWindowProcW(h, m, wp, lp) }
        }
        let profile = std::env::temp_dir().join(format!("VoxFakeElectron-{}", std::process::id()));
        crate::window::register_class(w!("Chrome_MessageWindow"), Some(plain)).expect("class");
        let title = format!("{}\\", profile.to_string_lossy().to_uppercase());
        let window = crate::window::create_hidden_window(w!("Chrome_MessageWindow"), &title, Some(HWND_MESSAGE))
            .expect("fake singleton window");
        assert!(electron_instance_running(&profile));
        assert!(!electron_instance_running(&profile.with_file_name("SomeOtherProfile")));
        // SAFETY: our own window on this thread.
        unsafe { DestroyWindow(window) }.expect("destroy");
        assert!(!electron_instance_running(&profile));
    }

    /// Desktop checklist: `cargo test -p vox-win electron_detection_report -- --ignored --nocapture` with and without
    /// the Electron build of Vox running (expect true / false for `%APPDATA%\Vox`).
    #[test]
    #[ignore = "inspects the running Electron Vox, if any"]
    fn electron_detection_report() {
        let root = PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA")).join("Vox");
        let t = Instant::now();
        println!("electron instance running: {} ({:?})", electron_instance_running(&root), t.elapsed());
    }
}
