use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::process::{Child, ExitStatus};

use super::{WorkerBackend, WorkerContainmentStatus, WorkerLaunchError, WorkerProcess};

const BACKEND: WorkerBackend = WorkerBackend::Bubblewrap;
const UNAVAILABLE_REASON: &str =
    "the broker-only bubblewrap/seccomp/rlimit backend has not been delivered";

pub(super) fn standard_streams_are_protocol_pipes() -> bool {
    fn is_pipe(fd: RawFd) -> bool {
        std::fs::metadata(format!("/proc/self/fd/{fd}"))
            .map(|metadata| metadata.file_type().is_fifo())
            .unwrap_or(false)
    }

    is_pipe(std::io::stdin().as_raw_fd())
        && is_pipe(std::io::stdout().as_raw_fd())
        && is_pipe(std::io::stderr().as_raw_fd())
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
        super::super::kill_process_group(self.child.id());
        self.child.kill()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}
