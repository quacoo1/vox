//! Windows containment: a job object per helper that kills its whole tree when the job handle closes.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

pub(super) fn prepare() {}

/// The helper's job. Closing the handle (Drop, or any exit of Vox) kills every process in it.
pub(super) struct Tree {
    job: OwnedHandle,
}

pub(super) fn spawn(mut cmd: Command) -> io::Result<(Child, Tree)> {
    // The job first, so a failure here starts nothing.
    let job = new_job()?;
    cmd.creation_flags((CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).0);
    let mut child = cmd.spawn()?;
    // SAFETY: both handles are open for the duration of the call.
    let assigned =
        unsafe { AssignProcessToJobObject(HANDLE(job.as_raw_handle()), HANDLE(child.as_raw_handle())) };
    if let Err(e) = assigned {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e.into());
    }
    Ok((child, Tree { job }))
}

fn new_job() -> io::Result<OwnedHandle> {
    // SAFETY: no security attributes (the handle is not inheritable) and no name.
    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
    // SAFETY: CreateJobObjectW returned a handle we now own.
    let job = unsafe { OwnedHandle::from_raw_handle(job.0) };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    // SAFETY: `info` is the structure the class names, and outlives the call.
    unsafe {
        SetInformationJobObject(
            HANDLE(job.as_raw_handle()),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }?;
    Ok(job)
}

impl Tree {
    pub(super) fn kill(&self) -> io::Result<()> {
        // SAFETY: the job handle is open.
        unsafe { TerminateJobObject(HANDLE(self.job.as_raw_handle()), 1) }?;
        Ok(())
    }
}

/// Windows has no zombies: the Child's process handle keeps the pid from being reused, so a plain `try_wait` is safe.
pub(super) fn peek_exit(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    child.try_wait()
}
