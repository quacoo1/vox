//! Single instance on Linux: a per-user `flock` on `$XDG_RUNTIME_DIR/<key>.lock` decides the primary; the primary
//! listens on the Unix socket `$XDG_RUNTIME_DIR/<key>.sock` and later launches send it their argv + cwd
//! (`u32` little-endian length + JSON `{"argv": [...], "cwd": "..."}`). A forwarded `--vox-hotkey <action>` (what the
//! GNOME shortcuts run, see `hotkeys`) goes to the hotkey handler instead of the CLI handler.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

/// What a later launch forwarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedArgs {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

/// The lock outcome.
pub enum InstanceRole {
    Primary(InstanceServer),
    Secondary,
}

/// The primary's socket server (bound, not accepting yet) and the lock it holds.
pub struct InstanceServer {
    lock: std::fs::File,
    listener: Option<UnixListener>,
    socket_path: PathBuf,
}

pub type ForwardHandler = Arc<dyn Fn(ForwardedArgs) + Send + Sync>;

const MAX_MESSAGE: usize = 32 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("vox-{}", unsafe { libc::getuid() })))
}

fn file_stem(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn paths(key: &str) -> (PathBuf, PathBuf) {
    let dir = runtime_dir();
    let stem = file_stem(key);
    (dir.join(format!("{stem}.lock")), dir.join(format!("{stem}.sock")))
}

impl InstanceServer {
    pub fn listen(self, on_args: ForwardHandler) -> Result<InstanceListener, crate::WinError> {
        let InstanceServer { lock, listener, socket_path } = self;
        let Some(listener) = listener else {
            return Ok(InstanceListener { stop: Arc::new(AtomicBool::new(false)), thread: None, socket_path, _lock: lock });
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("vox-instance".into())
            .spawn(move || serve(listener, &thread_stop, on_args))
            .map_err(|e| crate::WinError(format!("cannot start the instance thread: {e}")))?;
        Ok(InstanceListener { stop, thread: Some(thread), socket_path, _lock: lock })
    }
}

/// Keeps the listener thread alive; releases the lock when dropped.
pub struct InstanceListener {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    socket_path: PathBuf,
    _lock: std::fs::File,
}

impl InstanceListener {
    pub fn shutdown(mut self) {
        self.stop_thread();
    }

    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // Wake the blocking accept.
            let _ = UnixStream::connect(&self.socket_path);
            if thread.join().is_err() {
                tracing::warn!(target: "instance", "instance thread panicked");
            }
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

impl Drop for InstanceListener {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

fn serve(listener: UnixListener, stop: &AtomicBool, on_args: ForwardHandler) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        match read_message(&mut stream) {
            Some(args) => {
                if !super::hotkeys::dispatch_from_argv(&args.argv) {
                    on_args(args);
                }
            }
            None => tracing::debug!(target: "instance", "ignored an unreadable instance message"),
        }
    }
}

fn read_message(stream: &mut UnixStream) -> Option<ForwardedArgs> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    let v: Value = serde_json::from_slice(&body).ok()?;
    let argv = v.get("argv")?.as_array()?.iter().map(|a| a.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
    let cwd = PathBuf::from(v.get("cwd")?.as_str()?);
    Some(ForwardedArgs { argv, cwd })
}

/// Takes the per-user lock for `key`; as primary also binds the socket.
pub fn acquire(key: &str) -> Result<InstanceRole, crate::WinError> {
    let (lock_path, socket_path) = paths(key);
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path)?;
    // SAFETY: flock on a descriptor we own; the lock lives as long as the file.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(InstanceRole::Secondary);
    }
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(target: "instance", "cannot listen on {}: {e}", socket_path.display());
            None
        }
    };
    Ok(InstanceRole::Primary(InstanceServer { lock, listener, socket_path }))
}

/// Sends argv + cwd to the primary.
pub fn forward_to_primary(key: &str, args: &ForwardedArgs) -> Result<(), crate::WinError> {
    let (_, socket_path) = paths(key);
    let mut stream = connect_with_retry(&socket_path)?;
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let body = serde_json::to_vec(&json!({ "argv": args.argv, "cwd": args.cwd.to_string_lossy() })).map_err(|e| crate::WinError(e.to_string()))?;
    let len = u32::try_from(body.len()).map_err(|_| crate::WinError("arguments too long".into()))?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

/// The primary may still be starting (lock taken, socket not bound yet): retry for up to 5 s.
fn connect_with_retry(path: &Path) -> Result<UnixStream, crate::WinError> {
    let deadline = std::time::Instant::now() + IO_TIMEOUT;
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) if std::time::Instant::now() >= deadline => return Err(e.into()),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// The Electron build never ran on Linux.
pub fn electron_instance_running(_profile_root: &Path) -> bool {
    false
}
