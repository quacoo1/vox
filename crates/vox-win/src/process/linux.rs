//! Linux containment: a process group led by the helper, and `PR_SET_PDEATHSIG(SIGKILL)` set between fork and exec.

use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};

/// Undoes an inherited `SIGCHLD` = `SIG_IGN` (or `SA_NOCLDWAIT`), which survives `execve`: with it the kernel reaps
/// helpers by itself, so their exit can no longer be read and their pid (and group id) may be reused while we still
/// mean to signal it. Runs once, before the first spawn; nothing else in Vox relies on ignoring `SIGCHLD`.
pub(super) fn prepare() {
    // SAFETY: plain sigaction calls on zeroed structures that outlive them.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut old) != 0 {
            return;
        }
        if old.sa_sigaction == libc::SIG_IGN || old.sa_flags & libc::SA_NOCLDWAIT != 0 {
            let mut default: libc::sigaction = std::mem::zeroed();
            default.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(libc::SIGCHLD, &default, std::ptr::null_mut());
            tracing::info!(target: "addons", "reset an inherited SIGCHLD disposition so helpers can be waited for");
        }
    }
}

/// The helper's process group.
pub(super) struct Tree {
    pgid: libc::pid_t,
}

pub(super) fn spawn(mut cmd: Command) -> io::Result<(Child, Tree)> {
    let parent = std::process::id() as libc::pid_t;
    cmd.process_group(0);
    // SAFETY: the closure runs in the forked child before exec and makes only async-signal-safe calls (prctl,
    // getppid, _exit); `last_os_error` reads errno without allocating.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The parent died before the death signal was armed: nobody will ever signal or reap us.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let pgid = child.id() as libc::pid_t;
    Ok((child, Tree { pgid }))
}

impl Tree {
    /// SIGKILL to the whole group. The caller has not reaped the leader yet, so the group id cannot have been reused.
    pub(super) fn kill(&self) -> io::Result<()> {
        // SAFETY: plain syscall.
        if unsafe { libc::killpg(self.pgid, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        // ESRCH: the group is already empty.
        if e.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(e)
        }
    }
}

/// `waitid(WNOWAIT)`: the status of an exited helper, which stays a zombie (and keeps its group id) until reaped.
pub(super) fn peek_exit(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    // SAFETY: an all-zero siginfo_t is valid; waitid fills it in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: `info` outlives the call.
    if unsafe { libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid filled in a SIGCHLD siginfo (or left it zeroed, which reads as pid 0).
    let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
    if pid == 0 {
        return Ok(None);
    }
    // Rebuild the wait(2) status word that ExitStatus wraps.
    let raw = match info.si_code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_KILLED => status & 0x7f,
        libc::CLD_DUMPED => (status & 0x7f) | 0x80,
        _ => return Ok(None),
    };
    Ok(Some(ExitStatus::from_raw(raw)))
}
