//! Crate-wide process-creation serialization for Windows handle inheritance.
//!
//! Rust's standard library protects its own inheritable pipe setup with a private lock. The
//! broker-only Windows worker also needs raw inheritable handles, so every production `Command`
//! terminal in this crate enters this outer boundary before standard-library or Tokio process
//! creation. The guard is held only through synchronous spawn; waits and async work happen after
//! it is released.

use std::io;
use std::process::{Child, ExitStatus, Output, Stdio};
use tokio::process::{Child as TokioChild, Command as TokioCommand};

#[cfg(feature = "mcp")]
use tokio::process::ChildStderr as TokioChildStderr;

#[cfg(windows)]
use std::sync::{Mutex, MutexGuard};

#[cfg(windows)]
static PROCESS_CREATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct CreationGuard {
    #[cfg(windows)]
    _inner: MutexGuard<'static, ()>,
}

pub(crate) fn creation_guard() -> io::Result<CreationGuard> {
    #[cfg(windows)]
    {
        return PROCESS_CREATION_LOCK
            .lock()
            .map(|guard| CreationGuard { _inner: guard })
            .map_err(|_| io::Error::other("Windows process-creation lock is poisoned"));
    }
    #[cfg(not(windows))]
    {
        Ok(CreationGuard {})
    }
}

pub(crate) trait StdCommandCreationExt {
    fn spawn_guarded(&mut self) -> io::Result<Child>;
    fn status_guarded(&mut self) -> io::Result<ExitStatus>;
    fn output_guarded(&mut self) -> io::Result<Output>;
}

impl StdCommandCreationExt for std::process::Command {
    fn spawn_guarded(&mut self) -> io::Result<Child> {
        let _guard = creation_guard()?;
        std::process::Command::spawn(self)
    }

    fn status_guarded(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.spawn_guarded()?;
        child.wait()
    }

    fn output_guarded(&mut self) -> io::Result<Output> {
        self.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = self.spawn_guarded()?;
        child.wait_with_output()
    }
}

pub(crate) trait TokioCommandCreationExt {
    fn spawn_guarded(&mut self) -> io::Result<TokioChild>;
}

impl TokioCommandCreationExt for TokioCommand {
    fn spawn_guarded(&mut self) -> io::Result<TokioChild> {
        let _guard = creation_guard()?;
        TokioCommand::spawn(self)
    }
}

#[cfg(feature = "mcp")]
pub(crate) trait RmcpCommandCreationExt {
    fn spawn_guarded(
        self,
    ) -> io::Result<(
        rmcp::transport::child_process::TokioChildProcess,
        Option<TokioChildStderr>,
    )>;
}

#[cfg(feature = "mcp")]
impl RmcpCommandCreationExt for rmcp::transport::child_process::TokioChildProcessBuilder {
    fn spawn_guarded(
        self,
    ) -> io::Result<(
        rmcp::transport::child_process::TokioChildProcess,
        Option<TokioChildStderr>,
    )> {
        let _guard = creation_guard()?;
        rmcp::transport::child_process::TokioChildProcessBuilder::spawn(self)
    }
}
