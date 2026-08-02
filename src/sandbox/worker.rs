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
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) const INTERNAL_WORKER_MARKER: &str = "MINI_AGENT_INTERNAL_JS_WORKER";
pub(crate) const INTERNAL_WORKER_MARKER_VALUE: &str = "brokered-v1";
#[cfg(target_os = "linux")]
pub(crate) const LINUX_PREFLIGHT_MARKER_VALUE: &str = "linux-preflight-v1";

pub(crate) fn is_internal_worker_marker_present() -> bool {
    std::env::var_os(INTERNAL_WORKER_MARKER).is_some()
}

pub(crate) fn standard_streams_are_protocol_pipes() -> bool {
    // This rejects terminals, files, null devices, and sockets. It intentionally makes no
    // same-user identity claim: Unix FIFOs and Windows named/anonymous pipes share an OS type.
    platform::standard_streams_are_protocol_pipes()
}

pub(crate) fn finalize_internal_worker() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        return platform::finalize_worker();
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_linux_containment_probe() -> io::Result<()> {
    platform::run_containment_probe()
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_linux_containment_child_probe() -> io::Result<()> {
    platform::run_containment_child_probe()
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_linux_cpu_limit_child_probe() -> io::Result<()> {
    platform::run_cpu_limit_child_probe()
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_linux_core_limit_child_probe() -> io::Result<()> {
    platform::run_core_limit_child_probe()
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_linux_core_crash_child_probe() -> io::Result<()> {
    platform::run_core_crash_child_probe()
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
    Available {
        backend: WorkerBackend,
        assurance: WorkerContainmentAssurance,
    },
    Unavailable {
        backend: WorkerBackend,
        assurance: WorkerContainmentAssurance,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerContainmentAssurance {
    Enforced,
    DeprecatedBestEffort,
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
    #[cfg(test)]
    reap_observer: Option<Arc<AtomicUsize>>,
    #[cfg(test)]
    force_tree_termination_error: bool,
}

impl WorkerProcess {
    pub(crate) fn id(&self) -> u32 {
        self.process.id()
    }

    pub(crate) fn terminate_tree(&mut self) -> io::Result<()> {
        let result = self.process.terminate_tree();
        #[cfg(test)]
        if self.force_tree_termination_error {
            return Err(io::Error::other("forced tree-termination failure"));
        }
        result
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.process.try_wait()?;
        if status.is_some() {
            self.notify_reaped();
        }
        Ok(status)
    }

    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.process.wait()?;
        self.notify_reaped();
        Ok(status)
    }

    /// Terminates the complete containment tree and waits a bounded time for its root to reap.
    pub(crate) fn terminate_and_reap(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        let root_status = self.try_wait()?;
        // Teardown must target the containment tree even if the root has already exited: an old
        // descendant may still own a protocol-pipe clone and otherwise outlive its generation.
        let mut termination_error = self.terminate_tree().err();
        if let Some(status) = root_status {
            return match termination_error.take() {
                Some(error) => Err(error),
                None => Ok(status),
            };
        }

        loop {
            if let Some(status) = self.try_wait()? {
                return match termination_error.take() {
                    Some(error) => Err(error),
                    None => Ok(status),
                };
            }
            if Instant::now() >= deadline {
                return Err(termination_error.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "JavaScript worker did not exit before the bounded reap deadline",
                    )
                }));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(test)]
    pub(crate) fn observe_reap_for_test(&mut self, observer: Arc<AtomicUsize>) {
        assert!(
            self.reap_observer.is_none(),
            "reap observer already installed"
        );
        observer.fetch_add(1, Ordering::AcqRel);
        self.reap_observer = Some(observer);
    }

    #[cfg(test)]
    pub(crate) fn force_tree_termination_error_for_test(&mut self) {
        self.force_tree_termination_error = true;
    }

    #[cfg(test)]
    fn notify_reaped(&mut self) {
        if let Some(observer) = self.reap_observer.take() {
            observer.fetch_sub(1, Ordering::AcqRel);
        }
    }

    #[cfg(not(test))]
    fn notify_reaped(&mut self) {}
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        // This is the last-resort path for cancellation by caller-future drop. It is bounded so
        // synchronous destruction cannot hang an async executor indefinitely.
        let _ = self.terminate_and_reap(Duration::from_millis(500));
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn terminate_worker_process_group(pid: u32) -> io::Result<()> {
    let process_group = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "worker PID exceeds pid_t"))?;
    // SAFETY: kill is a synchronous syscall. A negative, nonzero PID addresses exactly the
    // process group created for this worker; no pointer or borrowed memory crosses the call.
    if unsafe { libc::kill(-process_group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // The complete group is already gone, which satisfies teardown.
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestSupervisorStartup {
    Healthy,
    ExitBeforeReady,
    MalformedReady,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum TestWorkerTarget {
    LauncherProbe,
    InternalWorker {
        timeout_ms: Option<u64>,
        max_pending_jobs: Option<usize>,
        supervisor_script: bool,
        stderr_bytes: usize,
        startup: TestSupervisorStartup,
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
                supervisor_script: false,
                stderr_bytes: 0,
                startup: TestSupervisorStartup::Healthy,
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
                supervisor_script: false,
                stderr_bytes: 0,
                startup: TestSupervisorStartup::Healthy,
            },
        }
    }

    /// Launch the real protocol-pipe child with a test-only scripted worker body.
    pub(crate) const fn scripted_internal_worker(stderr_bytes: usize) -> Self {
        Self {
            target: TestWorkerTarget::InternalWorker {
                timeout_ms: None,
                max_pending_jobs: None,
                supervisor_script: true,
                stderr_bytes,
                startup: TestSupervisorStartup::Healthy,
            },
        }
    }

    pub(crate) const fn scripted_internal_worker_with_startup(
        stderr_bytes: usize,
        startup: TestSupervisorStartup,
    ) -> Self {
        Self {
            target: TestWorkerTarget::InternalWorker {
                timeout_ms: None,
                max_pending_jobs: None,
                supervisor_script: true,
                stderr_bytes,
                startup,
            },
        }
    }
}

#[cfg(test)]
impl WorkerLauncher for TestWorkerLauncher {
    fn containment_status(&self) -> WorkerContainmentStatus {
        WorkerContainmentStatus::Available {
            backend: WorkerBackend::for_current_platform(),
            assurance: if cfg!(target_os = "macos") {
                WorkerContainmentAssurance::DeprecatedBestEffort
            } else {
                WorkerContainmentAssurance::Enforced
            },
        }
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
                supervisor_script,
                stderr_bytes,
                startup,
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
                if supervisor_script {
                    command.env("MINI_AGENT_TEST_SUPERVISOR_SCRIPT", "1").env(
                        "MINI_AGENT_TEST_SUPERVISOR_STDERR_BYTES",
                        stderr_bytes.to_string(),
                    );
                    command.env(
                        "MINI_AGENT_TEST_SUPERVISOR_STARTUP",
                        match startup {
                            TestSupervisorStartup::Healthy => "healthy",
                            TestSupervisorStartup::ExitBeforeReady => "exit-before-ready",
                            TestSupervisorStartup::MalformedReady => "malformed-ready",
                        },
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
            reap_observer: None,
            force_tree_termination_error: false,
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

#[cfg(unix)]
fn child_stdin_file(pipe: std::process::ChildStdin) -> File {
    use std::os::fd::OwnedFd;
    File::from(OwnedFd::from(pipe))
}

#[cfg(unix)]
fn child_stdout_file(pipe: std::process::ChildStdout) -> File {
    use std::os::fd::OwnedFd;
    File::from(OwnedFd::from(pipe))
}

#[cfg(unix)]
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
    #[cfg(target_os = "macos")]
    fn worker_launcher_production_is_fail_closed_until_backend_delivery() {
        let WorkerContainmentStatus::Unavailable {
            backend,
            assurance: _,
            reason,
        } = containment_status()
        else {
            panic!("macOS production worker launcher must remain unavailable");
        };
        assert_eq!(backend, WorkerBackend::for_current_platform());
        assert!(!reason.trim().is_empty());

        let error = launch().expect_err("production must not select the test launcher");
        assert_eq!(error.backend(), backend);
        assert!(matches!(error, WorkerLaunchError::Unavailable { .. }));
    }

    #[test]
    fn windows_lpac_gate_source_keeps_truthful_matrix_and_exact_acl_policy() {
        let source = include_str!("worker/windows.rs");
        assert_eq!(
            source.matches("probe: ProbeKind::Harness,").count(),
            3,
            "every supported destination needs a full harness probe"
        );
        assert_eq!(
            source
                .matches("probe: ProbeKind::ImageLoadingOnly,")
                .count(),
            2,
            "installed-image evidence must remain separate from containment evidence"
        );
        assert!(source.contains("source_expected: contract.source_location"));
        assert!(source.contains("destination_expected: contract.destination"));
        assert!(source.contains("mapped_file_mask(ace.Mask)"));
        assert!(source.contains("package_allow_set_is_exact(&appcontainer_allows)"));
        assert!(!source.contains("ProbeKind::VersionBinary"));

        let specification = include_str!("../../docs/specs/phase-6-brokered-js-runtime.md");
        assert!(specification.contains(
            "cargo install --locked --no-default-features --features js --path . --debug"
        ));
        assert!(specification.contains(
            "cargo test --locked --no-default-features --features js windows_lpac_can_load_current_exe_with_only_protocol_handles -- --ignored --nocapture --exact"
        ));
        assert!(specification.contains(
            "prove only that `CreateProcessW` accepts the production image with the requested"
        ));
        assert!(specification.contains("They do not prove the resulting token"));
    }

    #[test]
    fn windows_production_launcher_source_keeps_creation_time_authority_closed() {
        let source = include_str!("worker/windows.rs");
        let creation_source = include_str!("../process_creation.rs");
        for required in [
            "PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES",
            "PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY",
            "PROC_THREAD_ATTRIBUTE_JOB_LIST",
            "PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY",
            "PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY",
            "PROC_THREAD_ATTRIBUTE_HANDLE_LIST",
            "PROCESS_CREATION_CHILD_PROCESS_RESTRICTED",
            "PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT",
            "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
            "JOB_OBJECT_LIMIT_ACTIVE_PROCESS",
            "JOB_OBJECT_LIMIT_PROCESS_MEMORY",
            "JOB_OBJECT_LIMIT_PROCESS_TIME",
            "JOB_OBJECT_UILIMIT_ALL",
            "ProcessMemoryLimit = PROCESS_MEMORY_LIMIT_BYTES",
            "PerProcessUserTimeLimit = PROCESS_CPU_LIMIT_100NS",
            "AttributeList::new(6)",
            "TRUE,\n                EXTENDED_STARTUPINFO_PRESENT",
            "WorkerChild::contained(process, job",
        ] {
            assert!(
                source.contains(required),
                "Windows production launcher lost required primitive {required}"
            );
        }
        assert!(source.contains("pipes.child_input.clear_inherit()?"));
        assert!(source.contains("pipes.child_output.clear_inherit()?"));
        assert!(source.contains("pipes.child_error.clear_inherit()?"));
        assert!(source.contains("drop(inheritance_guard);"));
        assert!(
            source
                .matches("crate::process_creation::creation_guard()?")
                .count()
                >= 3
        );
        assert!(creation_source.contains("static PROCESS_CREATION_LOCK: Mutex<()>"));
        assert!(creation_source.contains("trait StdCommandCreationExt"));
        assert!(creation_source.contains("trait TokioCommandCreationExt"));
        assert!(creation_source.contains("trait RmcpCommandCreationExt"));
        assert!(source.contains("Windows LPAC runtime containment probe has not passed"));
        assert!(!source.contains("PREFLIGHT.get_or_init"));
        assert!(source.contains("WorkerLaunchError::Unavailable {"));
        assert!(source.contains("crate::process_creation::creation_guard()?"));
        assert!(source.contains("Capabilities: null_mut(),\n            CapabilityCount: 0"));
        assert!(!source.contains("AssignProcessToJobObject"));
        assert!(!source.contains("PROC_THREAD_ATTRIBUTE_PARENT_PROCESS"));
        assert!(!source.contains("PROCESS_CREATION_MITIGATION_POLICY_WIN32K"));
        assert!(!source.contains("PROCESS_CREATION_MITIGATION_POLICY_PROHIBIT_DYNAMIC_CODE"));
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
    fn worker_root_exit_does_not_mask_tree_termination_failure() {
        let mut process = TestWorkerLauncher::current_test_process()
            .launch()
            .expect("test launcher should start the current test executable");
        process
            .terminate_tree()
            .expect("test worker group should terminate");
        process.wait().expect("test worker root should reap");
        process.force_tree_termination_error_for_test();

        let error = process
            .terminate_and_reap(Duration::from_millis(50))
            .expect_err("a cached root status must not hide tree-termination failure");
        assert_eq!(error.to_string(), "forced tree-termination failure");
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
