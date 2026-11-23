//! Crate-wide process-creation serialization for Windows handle inheritance.
//!
//! Rust's standard library protects its own inheritable pipe setup with a private lock. The
//! broker-only Windows worker also needs raw inheritable handles, so every production `Command`
//! terminal in this crate enters this outer boundary before standard-library or Tokio process
//! creation. Spawn/status helpers release the guard after synchronous spawn. The output helper
//! spawns under the guard with explicitly piped stdout/stderr and a null stdin, releases it, and
//! only then waits for the child and drains its pipes: the lock serialises handle inheritance
//! during `CreateProcess`, so it must never cover the child's lifetime (a slow child, such as a
//! PowerShell start on a loaded ARM64 host, would otherwise starve every other spawn until its
//! deadline). No guard crosses async work.

use std::io;
use std::process::{Child, ExitStatus, Output};
#[cfg(windows)]
use std::time::{Duration, Instant};
use tokio::process::{Child as TokioChild, Command as TokioCommand};

#[cfg(windows)]
use std::sync::{Condvar, Mutex, MutexGuard};

#[cfg(windows)]
static PROCESS_CREATION_LOCK: Mutex<()> = Mutex::new(());

// Wakes deadline-bounded waiters when the creation lock is released, so they park instead of
// sleep-spinning. It carries no data and never replaces `PROCESS_CREATION_LOCK`.
#[cfg(windows)]
static CREATION_RELEASED: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

pub(crate) struct CreationGuard {
    #[cfg(windows)]
    _inner: Option<MutexGuard<'static, ()>>,
}

#[cfg(windows)]
impl Drop for CreationGuard {
    fn drop(&mut self) {
        // Release the creation lock first, then wake a waiter. A waiter checks the lock while
        // holding the notification mutex, so it cannot miss this wakeup.
        drop(self._inner.take());
        let _sync = CREATION_RELEASED
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        CREATION_RELEASED.1.notify_one();
    }
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
        return Ok(CreationGuard {
            _inner: Some(guard),
        });
    }
    #[cfg(not(windows))]
    {
        Ok(CreationGuard {})
    }
}

// Read-only native test observation, never used for authorization or launch.
// Exact ignored owner controls run serially in their owning hosted step.
#[cfg(all(windows, test))]
static CREATION_WAIT_ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(all(windows, test))]
pub(crate) fn creation_wait_attempts() -> u64 {
    CREATION_WAIT_ATTEMPTS.load(std::sync::atomic::Ordering::Acquire)
}

#[cfg(windows)]
pub(crate) fn creation_guard_until(deadline: Instant) -> io::Result<CreationGuard> {
    // The notification mutex is held across each lock check and released only by the condvar
    // wait, so a release between the check and the wait cannot be missed. The 5 ms cap is a
    // safety net for guards released by the plain blocking `creation_guard` path.
    let mut sync = CREATION_RELEASED
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        match PROCESS_CREATION_LOCK.try_lock() {
            Ok(guard) => {
                return Ok(CreationGuard {
                    _inner: Some(guard),
                });
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let guard = poisoned.into_inner();
                PROCESS_CREATION_LOCK.clear_poison();
                return Ok(CreationGuard {
                    _inner: Some(guard),
                });
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                #[cfg(test)]
                CREATION_WAIT_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Release);
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Windows process-creation lock deadline elapsed",
                    ));
                }
                let slice = Duration::from_millis(5)
                    .min(deadline.saturating_duration_since(Instant::now()));
                sync = CREATION_RELEASED
                    .1
                    .wait_timeout(sync, slice)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
        }
    }
}

/// Hold the exact creation guard through suspended assignment and resume. The
/// callback consumes the spawned child, including its failure cleanup owner;
/// it cannot return an unowned child between spawn and Job association.
#[cfg(windows)]
pub(crate) fn launch_tokio_with_creation_guard_until<R>(
    command: &mut TokioCommand,
    deadline: Instant,
    finish: impl FnOnce(TokioChild) -> Result<R, String>,
) -> io::Result<R> {
    let _guard = creation_guard_until(deadline)?;
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "sandbox: general process construction deadline elapsed",
        ));
    }
    let child = TokioCommand::spawn(command)?;
    finish(child).map_err(io::Error::other)
}

/// Retry a spawn that raced a concurrent fork holding a write descriptor to the
/// executable being exec'd.
///
/// On Linux, `execve` of a file that any process still has open for writing
/// fails with `ETXTBSY`. A freshly materialized executable (for example the
/// guarded Git runtime) has its writer closed before first use, but another
/// thread's `fork` can briefly duplicate that descriptor into a child that has
/// not yet reached its own `exec` (which closes close-on-exec descriptors).
/// The condition clears within microseconds to milliseconds, so a short bounded
/// retry is the only fully robust remedy: no lock can cover every fork in the
/// process. Other platforms and other errors are returned unchanged.
fn retry_text_file_busy<T>(mut spawn: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    #[cfg(target_os = "linux")]
    {
        const ETXTBSY: i32 = 26;
        const ATTEMPTS: u32 = 40;
        let mut delay = std::time::Duration::from_millis(1);
        for _ in 1..ATTEMPTS {
            match spawn() {
                Err(error) if error.raw_os_error() == Some(ETXTBSY) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_millis(50));
                }
                other => return other,
            }
        }
    }
    spawn()
}

// One creation attempt per call: the guard is taken and released around each
// attempt, so ETXTBSY back-off never sleeps while holding the creation lock.
fn spawn_std_once(command: &mut std::process::Command) -> io::Result<Child> {
    let _guard = creation_guard()?;
    std::process::Command::spawn(command)
}

fn spawn_tokio_once(command: &mut TokioCommand) -> io::Result<TokioChild> {
    let _guard = creation_guard()?;
    TokioCommand::spawn(command)
}

#[cfg(any(feature = "lsp", feature = "mcp"))]
fn spawn_wrap_once(
    command: &mut process_wrap::tokio::CommandWrap,
) -> io::Result<Box<dyn process_wrap::tokio::ChildWrapper>> {
    let _guard = creation_guard()?;
    process_wrap::tokio::CommandWrap::spawn(command)
}

pub(crate) trait StdCommandCreationExt {
    fn spawn_guarded(&mut self) -> io::Result<Child>;
    #[cfg(windows)]
    fn spawn_guarded_until(&mut self, deadline: Instant) -> io::Result<Child>;
    fn status_guarded(&mut self) -> io::Result<ExitStatus>;
    // Production callers are Windows-only since `/undo stash` moved to the
    // hardened Git runner (mini-agent-93gx4).
    #[cfg_attr(not(any(test, windows)), allow(dead_code))]
    /// Spawn under the creation guard, release it, then wait and collect stdout/stderr.
    fn output_guarded(&mut self) -> io::Result<Output>;
}

impl StdCommandCreationExt for std::process::Command {
    fn spawn_guarded(&mut self) -> io::Result<Child> {
        retry_text_file_busy(|| spawn_std_once(self))
    }

    #[cfg(windows)]
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
        // Same defaults as `Command::output`, made explicit so the spawn can be separated from the
        // wait: stdin is null, stdout and stderr are captured, and any stdio the caller configured
        // earlier is replaced (no caller relies on redirecting an `output_guarded` stream). The
        // creation guard is held only inside `spawn_guarded`; reading the pipes and waiting for
        // exit happen after it is released.
        self.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        self.spawn_guarded()?.wait_with_output()
    }
}

pub(crate) trait TokioCommandCreationExt {
    fn spawn_guarded(&mut self) -> io::Result<TokioChild>;
    #[cfg(windows)]
    fn spawn_guarded_until(&mut self, deadline: Instant) -> io::Result<TokioChild>;
}

impl TokioCommandCreationExt for TokioCommand {
    fn spawn_guarded(&mut self) -> io::Result<TokioChild> {
        retry_text_file_busy(|| spawn_tokio_once(self))
    }
    #[cfg(windows)]
    fn spawn_guarded_until(&mut self, deadline: Instant) -> io::Result<TokioChild> {
        let _guard = creation_guard_until(deadline)?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process-creation deadline elapsed",
            ));
        }
        if let Some(run) = crate::spend::run::Run::current()
            && run.admit_effect().is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sandbox: captured spend admission refused process launch",
            ));
        }
        TokioCommand::spawn(self)
    }
}

#[cfg(any(feature = "lsp", feature = "mcp"))]
pub(crate) trait CommandWrapCreationExt {
    fn spawn_guarded(&mut self) -> io::Result<Box<dyn process_wrap::tokio::ChildWrapper>>;
}

#[cfg(any(feature = "lsp", feature = "mcp"))]
impl CommandWrapCreationExt for process_wrap::tokio::CommandWrap {
    fn spawn_guarded(&mut self) -> io::Result<Box<dyn process_wrap::tokio::ChildWrapper>> {
        retry_text_file_busy(|| spawn_wrap_once(self))
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
    fn guarded_output_captures_stdio_and_reuses_the_builder() {
        let mut command = Command::new(std::env::current_exe().expect("test executable exists"));
        command.arg("--help").stdout(Stdio::null());

        for _ in 0..2 {
            let output = command
                .output_guarded()
                .expect("guarded output should launch the reusable command");
            assert!(output.status.success());
            assert!(
                !output.stdout.is_empty(),
                "output_guarded captures stdout even when the builder named another stream"
            );
        }
    }

    /// The creation lock must not cover a child's lifetime: while one guarded
    /// output call is still waiting on a slow child, a second guarded spawn
    /// has to complete promptly.
    #[cfg(windows)]
    #[test]
    fn guarded_output_does_not_hold_the_creation_lock_across_the_child() {
        use std::time::{Duration, Instant};
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let ping = std::path::Path::new(&root).join(r"System32\PING.EXE");
        let slow = std::thread::spawn(move || {
            // About 4 s: one probe per second against the local address.
            Command::new(ping)
                .args(["-n", "5", "127.0.0.1"])
                .output_guarded()
                .expect("slow guarded output")
        });
        // Let the slow child get past its spawn and into its wait.
        std::thread::sleep(Duration::from_millis(1500));
        let started = Instant::now();
        let mut quick = Command::new(std::env::current_exe().expect("test executable exists"));
        quick
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = quick
            .spawn_guarded_until(Instant::now() + Duration::from_secs(2))
            .expect("a second guarded spawn must not wait for the first child to exit");
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = child.wait();
        assert!(
            !slow.is_finished(),
            "the slow child must still be running when the second spawn succeeds"
        );
        assert!(slow.join().expect("slow thread").status.success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_retries_while_executable_is_briefly_open_for_writing() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("etxtbsy-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("tool.sh");
        let mut writer = std::fs::File::create(&script).unwrap();
        writer.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
        writer
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            drop(writer);
        });
        let exit = Command::new(&script)
            .stdin(Stdio::null())
            .status_guarded()
            .expect("spawn should outlast a transient ETXTBSY");
        assert!(exit.success());
        closer.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
