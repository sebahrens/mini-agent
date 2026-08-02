//! Fail-closed process ownership for the broker-only JavaScript worker.
//!
//! This launcher is intentionally separate from [`super::Sandbox`]. The
//! general command sandbox exposes the workspace and application cache, while
//! an untrusted JavaScript worker must receive only its protocol pipes. Until
//! a target-specific containment backend is delivered, production launch is
//! unavailable. The unconfined launcher in this module exists only in tests.

use std::fmt;
use std::fs::File;
use std::io;
use std::process::ExitStatus;

pub(crate) const INTERNAL_WORKER_MARKER: &str = "MINI_AGENT_INTERNAL_JS_WORKER";
pub(crate) const INTERNAL_WORKER_MARKER_VALUE: &str = "brokered-v1";

pub(crate) fn is_internal_worker_marker_present() -> bool {
    std::env::var_os(INTERNAL_WORKER_MARKER).is_some()
}

pub(crate) fn standard_streams_are_protocol_pipes() -> bool {
    // This rejects terminals, files, null devices, and sockets. It intentionally makes no
    // same-user identity claim: Unix FIFOs and Windows named/anonymous pipes share an OS type.
    platform::standard_streams_are_protocol_pipes()
}

#[cfg(target_os = "linux")]
#[path = "worker/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "worker/macos.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "worker/windows.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("the JavaScript worker launcher supports only Linux, macOS, and Windows");

// Target-specific CI constructs every variant; one host necessarily sees the
// other two as dead code until status reporting consumes this enum.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerBackend {
    Bubblewrap,
    Seatbelt,
    WindowsLpac,
}

impl WorkerBackend {
    pub(crate) const fn for_current_platform() -> Self {
        #[cfg(target_os = "linux")]
        {
            return Self::Bubblewrap;
        }
        #[cfg(target_os = "macos")]
        {
            return Self::Seatbelt;
        }
        #[cfg(target_os = "windows")]
        {
            return Self::WindowsLpac;
        }
    }
}

impl fmt::Display for WorkerBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bubblewrap => "bubblewrap",
            Self::Seatbelt => "seatbelt",
            Self::WindowsLpac => "windows-lpac",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerContainmentStatus {
    Available(WorkerBackend),
    Unavailable {
        backend: WorkerBackend,
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkerLaunchError {
    #[error("JavaScript worker containment backend {backend} is unavailable: {reason}")]
    Unavailable {
        backend: WorkerBackend,
        reason: String,
    },
    #[error("failed to launch JavaScript worker with {backend}: {source}")]
    Io {
        backend: WorkerBackend,
        #[source]
        source: io::Error,
    },
    #[error("JavaScript worker launcher did not create a {pipe} protocol pipe")]
    MissingPipe { pipe: &'static str },
}

impl WorkerLaunchError {
    pub(crate) const fn backend(&self) -> WorkerBackend {
        match self {
            Self::Unavailable { backend, .. } | Self::Io { backend, .. } => *backend,
            Self::MissingPipe { .. } => WorkerBackend::for_current_platform(),
        }
    }
}

pub(crate) trait WorkerLauncher: Send + Sync {
    fn containment_status(&self) -> WorkerContainmentStatus;
    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProductionWorkerLauncher;

impl WorkerLauncher for ProductionWorkerLauncher {
    fn containment_status(&self) -> WorkerContainmentStatus {
        platform::containment_status()
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        platform::launch()
    }
}

pub(crate) fn containment_status() -> WorkerContainmentStatus {
    ProductionWorkerLauncher.containment_status()
}

pub(crate) fn launch() -> Result<WorkerProcess, WorkerLaunchError> {
    ProductionWorkerLauncher.launch()
}

#[derive(Debug)]
pub(crate) struct WorkerProcess {
    process: platform::WorkerChild,
    pub(crate) input: File,
    pub(crate) output: File,
    pub(crate) stderr: File,
    pub(crate) backend: WorkerBackend,
}

impl WorkerProcess {
    pub(crate) fn id(&self) -> u32 {
        self.process.id()
    }

    pub(crate) fn terminate_tree(&mut self) -> io::Result<()> {
        self.process.terminate_tree()
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.process.try_wait()
    }

    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.process.wait()
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if self.process.try_wait().ok().flatten().is_some() {
            return;
        }
        // Drop is a last-resort tree kill and must never block. The
        // supervisor owns the explicit bounded reap path.
        let _ = self.process.terminate_tree();
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum TestWorkerTarget {
    LauncherProbe,
    InternalWorker {
        timeout_ms: Option<u64>,
        max_pending_jobs: Option<usize>,
    },
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct TestWorkerLauncher {
    target: TestWorkerTarget,
}

#[cfg(test)]
impl Default for TestWorkerLauncher {
    fn default() -> Self {
        Self::current_test_process()
    }
}

#[cfg(test)]
impl TestWorkerLauncher {
    pub(crate) const fn current_test_process() -> Self {
        Self {
            target: TestWorkerTarget::LauncherProbe,
        }
    }

    pub(crate) const fn internal_worker_process() -> Self {
        Self {
            target: TestWorkerTarget::InternalWorker {
                timeout_ms: None,
                max_pending_jobs: None,
            },
        }
    }

    /// Test-only resource limits for exercising worker reset behavior without slow tests.
    pub(crate) const fn internal_worker_process_with_limits(
        timeout_ms: u64,
        max_pending_jobs: usize,
    ) -> Self {
        Self {
            target: TestWorkerTarget::InternalWorker {
                timeout_ms: Some(timeout_ms),
                max_pending_jobs: Some(max_pending_jobs),
            },
        }
    }
}

#[cfg(test)]
impl WorkerLauncher for TestWorkerLauncher {
    fn containment_status(&self) -> WorkerContainmentStatus {
        WorkerContainmentStatus::Available(WorkerBackend::for_current_platform())
    }

    fn launch(&self) -> Result<WorkerProcess, WorkerLaunchError> {
        use std::process::{Command, Stdio};

        let backend = WorkerBackend::for_current_platform();
        let executable =
            std::env::current_exe().map_err(|source| WorkerLaunchError::Io { backend, source })?;
        let mut command = Command::new(executable);
        // Seed representative secrets before clearing the command environment. The bootstrap
        // child asserts these values are absent, proving the launcher does not inherit parent
        // credentials, configuration, workspace hints, or PATH.
        command
            .env("OPENROUTER_API_KEY", "A07_CREDENTIAL_CANARY_MUST_NOT_LEAK")
            .env("MINI_AGENT_CONFIG", "A07_CONFIG_CANARY_MUST_NOT_LEAK")
            .env("MINI_AGENT_WORKSPACE", "A07_WORKSPACE_CANARY_MUST_NOT_LEAK")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match self.target {
            TestWorkerTarget::LauncherProbe => {
                command.env("MINI_AGENT_TEST_WORKER_PROCESS", "1").args([
                    "--exact",
                    "sandbox::worker::tests::worker_launcher_test_child",
                    "--nocapture",
                ]);
            }
            TestWorkerTarget::InternalWorker {
                timeout_ms,
                max_pending_jobs,
            } => {
                command
                    .env(INTERNAL_WORKER_MARKER, INTERNAL_WORKER_MARKER_VALUE)
                    .args([
                        "--exact",
                        "extras::js::tests::worker_runtime::worker_bootstrap_test_child",
                        "--nocapture",
                    ]);
                if let Some(timeout_ms) = timeout_ms {
                    command.env("MINI_AGENT_TEST_WORKER_TIMEOUT_MS", timeout_ms.to_string());
                }
                if let Some(max_pending_jobs) = max_pending_jobs {
                    command.env(
                        "MINI_AGENT_TEST_WORKER_MAX_PENDING_JOBS",
                        max_pending_jobs.to_string(),
                    );
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|source| WorkerLaunchError::Io { backend, source })?;
        let Some(input) = child.stdin.take() else {
            return Err(missing_test_pipe(&mut child, "stdin"));
        };
        let Some(output) = child.stdout.take() else {
            return Err(missing_test_pipe(&mut child, "stdout"));
        };
        let Some(stderr) = child.stderr.take() else {
            return Err(missing_test_pipe(&mut child, "stderr"));
        };

        Ok(WorkerProcess {
            process: platform::WorkerChild::from_unconfined_test_child(child),
            input: child_stdin_file(input),
            output: child_stdout_file(output),
            stderr: child_stderr_file(stderr),
            backend,
        })
    }
}

#[cfg(test)]
fn missing_test_pipe(child: &mut std::process::Child, pipe: &'static str) -> WorkerLaunchError {
    #[cfg(unix)]
    super::kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
    WorkerLaunchError::MissingPipe { pipe }
}

#[cfg(all(test, unix))]
fn child_stdin_file(pipe: std::process::ChildStdin) -> File {
    use std::os::fd::OwnedFd;
    File::from(OwnedFd::from(pipe))
}

#[cfg(all(test, unix))]
fn child_stdout_file(pipe: std::process::ChildStdout) -> File {
    use std::os::fd::OwnedFd;
    File::from(OwnedFd::from(pipe))
}

#[cfg(all(test, unix))]
fn child_stderr_file(pipe: std::process::ChildStderr) -> File {
    use std::os::fd::OwnedFd;
    File::from(OwnedFd::from(pipe))
}

#[cfg(all(test, windows))]
fn child_stdin_file(pipe: std::process::ChildStdin) -> File {
    use std::os::windows::io::OwnedHandle;
    File::from(OwnedHandle::from(pipe))
}

#[cfg(all(test, windows))]
fn child_stdout_file(pipe: std::process::ChildStdout) -> File {
    use std::os::windows::io::OwnedHandle;
    File::from(OwnedHandle::from(pipe))
}

#[cfg(all(test, windows))]
fn child_stderr_file(pipe: std::process::ChildStderr) -> File {
    use std::os::windows::io::OwnedHandle;
    File::from(OwnedHandle::from(pipe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn worker_launcher_production_is_fail_closed_until_backend_delivery() {
        let WorkerContainmentStatus::Unavailable { backend, reason } = containment_status() else {
            panic!("production worker launcher must remain unavailable in A06");
        };
        assert_eq!(backend, WorkerBackend::for_current_platform());
        assert!(!reason.trim().is_empty());

        let error = launch().expect_err("production must not select the test launcher");
        assert_eq!(error.backend(), backend);
        assert!(matches!(error, WorkerLaunchError::Unavailable { .. }));
    }

    #[test]
    fn worker_launcher_test_process_owns_piped_stdio_and_can_be_reaped() {
        let mut process = TestWorkerLauncher::current_test_process()
            .launch()
            .expect("test launcher should start the current test executable");

        let mut output = BufReader::new(&process.output);
        let mut line = String::new();
        let mut ready = false;
        while output.read_line(&mut line).expect("read child output") != 0 {
            if line.contains("MINI_AGENT_TEST_WORKER_READY") {
                ready = true;
                break;
            }
            line.clear();
        }
        assert!(ready, "test child did not report a cleared environment");
        assert!(process.id() > 0);
        assert_eq!(process.backend, WorkerBackend::for_current_platform());
        let _owned_protocol_pipes = (&process.input, &process.stderr);
        assert!(
            process
                .try_wait()
                .expect("try_wait should succeed")
                .is_none()
        );
        process
            .terminate_tree()
            .expect("test child should terminate");
        let _ = process.wait().expect("test child should be reaped");
    }

    #[test]
    fn worker_launcher_test_child() {
        if std::env::var_os("MINI_AGENT_TEST_WORKER_PROCESS").is_some() {
            assert!(
                std::env::var_os("PATH").is_none(),
                "test launcher inherited the parent's environment"
            );
            println!("MINI_AGENT_TEST_WORKER_READY");
            std::thread::park_timeout(std::time::Duration::from_secs(30));
        }
    }
}
