#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::process::{Child, ExitStatus};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};

use super::{WorkerBackend, WorkerContainmentStatus, WorkerLaunchError, WorkerProcess};

const BACKEND: WorkerBackend = WorkerBackend::WindowsLpac;
const UNAVAILABLE_REASON: &str =
    "the zero-capability LPAC/AppContainer creation-time Job backend has not been delivered";

pub(super) fn standard_streams_are_protocol_pipes() -> bool {
    fn is_pipe(handle: RawHandle) -> bool {
        // SAFETY: GetFileType only inspects the borrowed standard-stream handle. The handle is
        // owned by the process for this synchronous call and is neither closed nor retained.
        // Windows implements anonymous pipes using its named-pipe mechanism, so FILE_TYPE_PIPE
        // is the narrowest handle classification exposed by the OS.
        unsafe { GetFileType(handle as HANDLE) == FILE_TYPE_PIPE }
    }

    is_pipe(std::io::stdin().as_raw_handle())
        && is_pipe(std::io::stdout().as_raw_handle())
        && is_pipe(std::io::stderr().as_raw_handle())
}

pub(super) fn containment_status() -> WorkerContainmentStatus {
    WorkerContainmentStatus::Unavailable {
        backend: BACKEND,
        reason: UNAVAILABLE_REASON.to_string(),
    }
}

pub(super) fn launch() -> Result<WorkerProcess, WorkerLaunchError> {
    Err(WorkerLaunchError::Unavailable {
        backend: BACKEND,
        reason: UNAVAILABLE_REASON.to_string(),
    })
}

// This temporary std::process-backed type is reachable only from the test
// launcher. The production LPAC launcher will replace it with directly owned
// process and Job handles; the common API does not require std::process::Child.
#[derive(Debug)]
pub(super) struct WorkerChild {
    child: Child,
}

impl WorkerChild {
    #[cfg(test)]
    pub(super) fn from_unconfined_test_child(child: Child) -> Self {
        Self { child }
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn terminate_tree(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}
