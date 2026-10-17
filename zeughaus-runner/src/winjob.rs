//! Windows job objects: process trees that end together.
//!
//! Windows ends neither the children of a process that exits nor the
//! processes of a console that closes the way a hangup does on unix. The
//! runner and the `timeout` a CI job runs under therefore each hold a job
//! that kills everything in it when its last handle closes.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

/// A job whose processes all end when it is dropped.
pub struct Job(OwnedHandle);

impl Job {
    pub fn kill_on_close() -> io::Result<Job> {
        // SAFETY: no security attributes and no name: an anonymous job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle this function owns from here on.
        let job = Job(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
        // SAFETY: a plain-data struct of integers, valid when zeroed.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the pointer and size describe `limits`, which outlives the call.
        let set = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Puts `process` into the job; the processes it starts from then on are
    /// in it too.
    pub fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: both handles are open for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.raw(), process as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Puts this process into the job.
    pub fn assign_self(&self) -> io::Result<()> {
        // SAFETY: the pseudo handle of the current process needs no closing.
        self.assign(unsafe { GetCurrentProcess() } as RawHandle)
    }

    /// Ends every process in the job with `code`.
    pub fn terminate(&self, code: u32) {
        // SAFETY: the job handle is open.
        unsafe { TerminateJobObject(self.raw(), code) };
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

/// The process that started this one, while it still runs, as a handle that
/// can be waited on. `None` when it is gone already or cannot be opened.
pub fn parent() -> Option<OwnedHandle> {
    // SAFETY: a process snapshot of the whole system; closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: a fresh handle owned from here on.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as RawHandle) };
    // SAFETY: plain data; `dwSize` is set as the API requires.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: no arguments.
    let me = unsafe { GetCurrentProcessId() };
    let raw = snapshot.as_raw_handle() as HANDLE;
    // SAFETY: the snapshot is open and `entry` is a sized PROCESSENTRY32W.
    let mut more = unsafe { Process32FirstW(raw, &mut entry) } != 0;
    while more {
        if entry.th32ProcessID == me {
            // The parent is waiting for this process, so the pid cannot
            // have been reused yet.
            // SAFETY: a plain open by pid; null on failure.
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, entry.th32ParentProcessID) };
            return (!handle.is_null())
                // SAFETY: a fresh handle owned from here on.
                .then(|| unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(raw, &mut entry) } != 0;
    }
    None
}

/// Whether the process behind `handle` has ended.
pub fn has_exited(handle: &OwnedHandle) -> bool {
    // SAFETY: a zero wait on an open process handle.
    unsafe { WaitForSingleObject(handle.as_raw_handle() as HANDLE, 0) == WAIT_OBJECT_0 }
}
