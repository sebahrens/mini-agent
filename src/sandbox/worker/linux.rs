use std::io;
use std::process::{Child, ExitStatus};

use super::{WorkerBackend, WorkerContainmentStatus, WorkerLaunchError, WorkerProcess};

const BACKEND: WorkerBackend = WorkerBackend::Bubblewrap;
const UNAVAILABLE_REASON: &str =
    "the broker-only bubblewrap/seccomp/rlimit backend has not been delivered";

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
