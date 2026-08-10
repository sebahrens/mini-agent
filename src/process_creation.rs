//! Crate-wide process-creation serialization for Windows handle inheritance.
//!
//! Rust's standard library protects its own inheritable pipe setup with a private lock. The
//! broker-only Windows worker also needs raw inheritable handles, so every production `Command`
//! terminal in this crate enters this outer boundary before standard-library or Tokio process
//! creation. Spawn/status helpers release the guard after synchronous spawn. The output helper must
//! retain it through synchronous `Command::output` to preserve opaque stdio and reusable-builder
//! semantics; no guard crosses async work.

use std::io;
use std::process::{Child, ExitStatus, Output};
#[cfg(windows)]
use std::time::Duration;
use std::time::Instant;
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
        // The lock protects no data whose invariant can be corrupted. Poisoning only means a
        // previous holder unwound; retaining the recovered guard therefore preserves the actual
        // safety property (exclusive process creation) while avoiding a process-wide outage.
        let guard = PROCESS_CREATION_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        PROCESS_CREATION_LOCK.clear_poison();
        return Ok(CreationGuard { _inner: guard });
    }
    #[cfg(not(windows))]
    {
        Ok(CreationGuard {})
    }
}

pub(crate) fn creation_guard_until(deadline: Instant) -> io::Result<CreationGuard> {
    #[cfg(windows)]
    loop {
        match PROCESS_CREATION_LOCK.try_lock() {
            Ok(guard) => return Ok(CreationGuard { _inner: guard }),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let guard = poisoned.into_inner();
                PROCESS_CREATION_LOCK.clear_poison();
                return Ok(CreationGuard { _inner: guard });
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Windows process-creation lock deadline elapsed",
                    ));
                }
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = deadline;
        Ok(CreationGuard {})
    }
}

pub(crate) trait StdCommandCreationExt {
    fn spawn_guarded(&mut self) -> io::Result<Child>;
    fn spawn_guarded_until(&mut self, deadline: Instant) -> io::Result<Child>;
    fn status_guarded(&mut self) -> io::Result<ExitStatus>;
    fn output_guarded(&mut self) -> io::Result<Output>;
}

impl StdCommandCreationExt for std::process::Command {
    fn spawn_guarded(&mut self) -> io::Result<Child> {
        let _guard = creation_guard()?;
        std::process::Command::spawn(self)
    }

    fn spawn_guarded_until(&mut self, deadline: Instant) -> io::Result<Child> {
        let _guard = creation_guard_until(deadline)?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process-creation deadline elapsed",
            ));
        }
        std::process::Command::spawn(self)
    }

    fn status_guarded(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.spawn_guarded()?;
        child.wait()
    }

    fn output_guarded(&mut self) -> io::Result<Output> {
        // `Command` exposes no way to inspect or clone its configured stdio. Delegating to the
        // standard-library terminal is therefore required to preserve explicit stdio and reuse of
        // the same builder. This synchronous path holds the outer Windows lock until `output`
        // returns; it never crosses an async suspension.
        let _guard = creation_guard()?;
        std::process::Command::output(self)
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

#[cfg(any(feature = "mcp", feature = "lsp"))]
pub(crate) trait CommandWrapCreationExt {
    fn spawn_guarded(&mut self) -> io::Result<Box<dyn process_wrap::tokio::ChildWrapper>>;
}

#[cfg(any(feature = "mcp", feature = "lsp"))]
impl CommandWrapCreationExt for process_wrap::tokio::CommandWrap {
    fn spawn_guarded(&mut self) -> io::Result<Box<dyn process_wrap::tokio::ChildWrapper>> {
        let _guard = creation_guard()?;
        process_wrap::tokio::CommandWrap::spawn(self)
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

#[cfg(test)]
mod tests {
    use super::StdCommandCreationExt;
    use std::process::{Command, Stdio};

    #[cfg(windows)]
    #[test]
    fn poisoned_creation_lock_recovers_without_losing_serialization() {
        let poisoner = std::thread::spawn(|| {
            let _guard = super::PROCESS_CREATION_LOCK
                .lock()
                .expect("process-creation lock starts healthy");
            panic!("deliberately poison the process-creation lock");
        });
        assert!(poisoner.join().is_err());
        assert!(super::PROCESS_CREATION_LOCK.is_poisoned());

        let recovered = super::creation_guard().expect("poisoned lock should recover safely");
        assert!(!super::PROCESS_CREATION_LOCK.is_poisoned());

        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _guard = super::creation_guard().expect("waiter should acquire recovered lock");
            acquired_tx.send(()).expect("receiver remains live");
        });
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );

        drop(recovered);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("waiter should acquire after the recovered guard is dropped");
        waiter.join().expect("waiter should not panic");
    }

    #[test]
    fn guarded_output_preserves_explicit_stdio_across_builder_reuse() {
        let mut command = Command::new(std::env::current_exe().expect("test executable exists"));
        command
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        for _ in 0..2 {
            let output = command
                .output_guarded()
                .expect("guarded output should launch the reusable command");
            assert!(output.status.success());
            assert!(
                output.stdout.is_empty(),
                "explicit stdout must not be replaced"
            );
            assert!(
                output.stderr.is_empty(),
                "explicit stderr must not be replaced"
            );
        }
    }
}
