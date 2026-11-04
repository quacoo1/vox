//! Contained helper processes (docs/addons-design.md, "Process model"): the add-on helpers of vox-addons are started
//! here, so that none can outlive Vox, the hard-exit paths included (`process::exit`, a crash, Task Manager).
//!
//! - Windows: `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` (Vox is a GUI-subsystem process, so a console helper would
//!   otherwise get a console window of its own), then the child goes into its own job object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`. The job handle lives in the
//!   [`ContainedChild`]; however Vox exits, the kernel closes it and kills the tree. DIE_ON_UNHANDLED_EXCEPTION also
//!   keeps a crashing helper from showing the WER "stopped working" dialog.
//! - Linux: the child leads its own process group (`process_group(0)`) and gets `PR_SET_PDEATHSIG(SIGKILL)`. The death
//!   signal fires when the *thread* that forked the child exits, not the process, so every spawn goes through the one
//!   long-lived [`Spawner`] thread, which never exits. A child whose parent died between the fork and the `prctl` sees
//!   another parent and exits.
//! - [`ContainedChild::kill_tree`] kills the helper and everything it started (the job; the process group), then reaps
//!   the helper. Dropping the child does the same. [`ContainedChild::try_exit_status`] reads the helper's exit without
//!   reaping it, so on Linux the process group id stays reserved (by the zombie) until `kill_tree` has signalled it.
//!
//! Limits, covered by the helper rules: the job is assigned right after `CreateProcess` returns, so a helper must not
//! start children before it has answered `hello` (rule 4). On Linux a grandchild that leaves the process group
//! (`setsid`, `setpgid`) escapes `kill_tree`, and grandchildren get no death signal when Vox is killed hard. Stdin EOF
//! is the third backstop: helpers exit within 1 s of it (rule 3).

use std::io;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Sender};

#[cfg(not(windows))]
mod linux;
#[cfg(not(windows))]
use linux as imp;
#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as imp;

#[cfg(test)]
mod tests;

type SpawnRequest = (Command, Sender<io::Result<ContainedChild>>);

/// The one thread that starts helpers (`vox-addons-spawn`). On Linux the helpers' death signal is tied to it, so it
/// must never exit: it lives as long as the process, and no other thread may spawn helpers.
pub struct Spawner {
    tx: Sender<SpawnRequest>,
}

impl Spawner {
    /// The process-wide spawner; the first call starts its thread.
    pub fn global() -> &'static Spawner {
        static SPAWNER: OnceLock<Spawner> = OnceLock::new();
        SPAWNER.get_or_init(|| {
            imp::prepare();
            let (tx, rx) = unbounded::<SpawnRequest>();
            // The sender lives in the static, so `rx` never disconnects and the loop never ends. A panic must not end
            // it either (that would kill every helper on Linux), so each spawn is caught.
            let thread = std::thread::Builder::new().name("vox-addons-spawn".into()).spawn(move || {
                for (cmd, reply) in rx {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn_contained(cmd)))
                        .unwrap_or_else(|_| Err(io::Error::other("the helper spawn panicked")));
                    let _ = reply.send(result);
                }
            });
            if let Err(e) = thread {
                tracing::error!(target: "addons", "cannot start the helper spawner thread: {e}");
            }
            Spawner { tx }
        })
    }

    /// Starts `cmd` contained (see the module docs) and returns once it runs. The caller sets the program, args,
    /// directory, environment and stdio; on Windows the creation flags are this module's.
    pub fn spawn(&self, cmd: Command) -> io::Result<ContainedChild> {
        let (reply_tx, reply_rx) = bounded(1);
        self.tx.send((cmd, reply_tx)).map_err(|_| io::Error::other("the helper spawner thread is not running"))?;
        reply_rx.recv().map_err(|_| io::Error::other("the helper spawner thread is not running"))?
    }
}

fn spawn_contained(cmd: Command) -> io::Result<ContainedChild> {
    let (child, tree) = imp::spawn(cmd)?;
    Ok(ContainedChild { child, tree, status: None, lost: false })
}

/// A running helper and its containment. Dropping it kills the helper and everything it started.
pub struct ContainedChild {
    child: Child,
    tree: imp::Tree,
    /// Set once the helper has been reaped; after that the tree is never signalled again (on Linux its process group
    /// id may have been reused).
    status: Option<ExitStatus>,
    /// Waiting for the helper failed (`ECHILD`: something else reaped it). Its pid may be reused, so nothing is
    /// signalled again and every call fails.
    lost: bool,
}

impl ContainedChild {
    /// The helper's process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// The helper's exit status if it has exited, without reaping it: its children may still run, and
    /// [`kill_tree`](Self::kill_tree) still reaches them.
    pub fn try_exit_status(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        if self.lost {
            return Err(lost());
        }
        imp::peek_exit(&mut self.child).inspect_err(|_| self.lost = true)
    }

    /// Waits up to `timeout` for the helper to exit (see [`try_exit_status`](Self::try_exit_status)).
    pub fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_exit_status()? {
                return Ok(Some(status));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
        }
    }

    /// Kills the helper and everything it started, then reaps the helper and returns its exit status. Calling it
    /// again returns the same status.
    pub fn kill_tree(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        if self.lost {
            return Err(lost());
        }
        if let Err(e) = self.tree.kill() {
            tracing::warn!(target: "addons", "cannot kill helper {}'s process tree: {e}", self.child.id());
        }
        // The helper itself too, so the reap below cannot wait forever on one that left its process group. It is not
        // reaped yet, so its pid is still its own.
        let _ = self.child.kill();
        let status = self.child.wait().inspect_err(|_| self.lost = true)?;
        self.status = Some(status);
        Ok(status)
    }
}

fn lost() -> io::Error {
    io::Error::other("the helper was reaped elsewhere")
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        let _ = self.kill_tree();
    }
}

/// Why a helper could not start, for the user-facing copy (docs/addons-design.md, "Spawn errors").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnBlock {
    /// The program or its folder is missing: the add-on is incomplete.
    NotFound,
    /// Security software or a policy refused to start it. Windows: access denied, antivirus, Group Policy, Smart App
    /// Control / WDAC. Linux: `EPERM` (a security module or seccomp).
    Blocked,
    /// The file cannot run here. Linux: `EACCES` (no execute bit, or the folder is on a `noexec` mount) or `ENOEXEC`.
    /// Windows: not a program for this PC (`ERROR_BAD_EXE_FORMAT`, `ERROR_EXE_MACHINE_TYPE_MISMATCH`).
    NoExec,
    Other,
}

/// Sorts a spawn error from [`Spawner::spawn`] into a [`SpawnBlock`].
pub fn classify_spawn_error(e: &io::Error) -> SpawnBlock {
    #[cfg(windows)]
    const TABLE: &[(i32, SpawnBlock)] = &[
        (2, SpawnBlock::NotFound),     // ERROR_FILE_NOT_FOUND
        (3, SpawnBlock::NotFound),     // ERROR_PATH_NOT_FOUND
        (5, SpawnBlock::Blocked),      // ERROR_ACCESS_DENIED
        (225, SpawnBlock::Blocked),    // ERROR_VIRUS_INFECTED
        (1260, SpawnBlock::Blocked),   // ERROR_ACCESS_DISABLED_BY_POLICY
        (4551, SpawnBlock::Blocked),   // ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION
        (193, SpawnBlock::NoExec),     // ERROR_BAD_EXE_FORMAT
        (216, SpawnBlock::NoExec),     // ERROR_EXE_MACHINE_TYPE_MISMATCH
    ];
    #[cfg(not(windows))]
    const TABLE: &[(i32, SpawnBlock)] = &[
        (libc::ENOENT, SpawnBlock::NotFound),
        (libc::ENOTDIR, SpawnBlock::NotFound),
        (libc::EPERM, SpawnBlock::Blocked),
        (libc::EACCES, SpawnBlock::NoExec),
        (libc::ENOEXEC, SpawnBlock::NoExec),
    ];
    match e.raw_os_error() {
        Some(code) => TABLE.iter().find(|(c, _)| *c == code).map_or(SpawnBlock::Other, |(_, block)| *block),
        None if e.kind() == io::ErrorKind::NotFound => SpawnBlock::NotFound,
        None => SpawnBlock::Other,
    }
}
