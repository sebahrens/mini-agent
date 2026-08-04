use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{
    WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus, WorkerLaunchError,
    WorkerProcess,
};

#[path = "macos/stale_sweep.rs"]
mod stale_sweep;

const BACKEND: WorkerBackend = WorkerBackend::Seatbelt;
const ASSURANCE: WorkerContainmentAssurance = WorkerContainmentAssurance::DeprecatedBestEffort;
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const SW_VERS: &str = "/usr/bin/sw_vers";
const GUARDIAN_MARKER_VALUE: &str = "brokered-v1";
const HOSTED_LIFECYCLE_MARKER_VALUE: &str = "production-binary-v1";
const HOSTED_PARENT_DEATH_MARKER: &str = "MINI_AGENT_INTERNAL_MACOS_PARENT_DEATH_CANARY";
const HOSTED_PARENT_DEATH_MARKER_VALUE: &str = "production-binary-v1";
const HOSTED_PROBE_MARKER: &str = "MINI_AGENT_INTERNAL_MACOS_CONTAINMENT_PROBE";
const HOSTED_PROBE_MARKER_VALUE: &str = "contained-v1";
const HOSTED_WORKSPACE_SENTINEL: &str = "MINI_AGENT_MACOS_WORKSPACE_SENTINEL";
const HOSTED_SKILL_SENTINEL: &str = "MINI_AGENT_MACOS_SKILL_SENTINEL";
const HOSTED_CREDENTIAL_SENTINEL: &str = "MINI_AGENT_MACOS_CREDENTIAL_SENTINEL";
const HOSTED_ORIGINAL_EXECUTABLE: &str = "MINI_AGENT_MACOS_ORIGINAL_EXECUTABLE";
const HOSTED_ONE_TIME_IMAGE: &str = "MINI_AGENT_MACOS_ONE_TIME_IMAGE";
const HOSTED_GUARDIAN_PGID: &str = "MINI_AGENT_MACOS_GUARDIAN_PGID";
const HOSTED_DESCRIPTOR_BOUND: &str = "MINI_AGENT_MACOS_DESCRIPTOR_BOUND";
const HOSTED_PASS_RECORD: &str = "MACOS_CONTAINMENT_MATRIX_V1=passed";
// Only majors that passed the exact non-libtest production-binary matrix are enabled. Ready alone
// is never availability evidence. macOS 26 passed on 26.5.2; other majors remain fail closed.
const VALIDATED_MACOS_MAJORS: &[u32] = &[26];
// Darwin maps the dyld shared-cache address range into every process. Leave enough virtual-address
// headroom for that non-resident mapping; the inherited hard limit may impose a stricter ceiling.
// QuickJS retains its independent 64 MiB allocator cap.
const ADDRESS_SPACE_LIMIT: libc::rlim_t = 1024 * 1024 * 1024 * 1024;
const CPU_LIMIT_SECONDS: libc::rlim_t = 35;
const FILE_DESCRIPTOR_LIMIT: libc::rlim_t = 64;
const FILE_SIZE_LIMIT: libc::rlim_t = 1024 * 1024;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(20);
const SWEEP_CONTENTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_GUARDIAN_PROFILE_BYTES: usize = 4 * 1024;
const MAX_GUARDIAN_PATH_BYTES: usize = 4 * 1024;
const MAX_GUARDIAN_WORKER_ARGUMENTS: usize = 16;
const MAX_GUARDIAN_WORKER_ARGUMENT_BYTES: usize = 4 * 1024;
const MAX_PROBE_DESCRIPTOR_BOUND: RawFd = 1_048_576;

static STATUS: OnceLock<WorkerContainmentStatus> = OnceLock::new();

struct HostedProbePaths {
    workspace: PathBuf,
    skill: PathBuf,
    credential: PathBuf,
    root: PathBuf,
    skill_directory: PathBuf,
    credential_directory: PathBuf,
}

impl HostedProbePaths {
    fn create() -> io::Result<Self> {
        let identifier = uuid::Uuid::new_v4();
        let workspace = std::env::current_dir()?
            .join(format!(".mini-agent-macos-workspace-sentinel-{identifier}"));
        let root = std::env::temp_dir().join(format!("mini-agent-macos-matrix-{identifier}"));
        let skill_directory = root.join("skill-store");
        let credential_directory = root.join("credentials");
        let paths = Self {
            workspace,
            skill: skill_directory.join("sentinel"),
            credential: credential_directory.join("sentinel"),
            root,
            skill_directory,
            credential_directory,
        };
        let result = (|| {
            create_private_directory(&paths.root)?;
            create_private_directory(&paths.skill_directory)?;
            create_private_directory(&paths.credential_directory)?;
            for path in [&paths.workspace, &paths.skill, &paths.credential] {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)?;
                file.write_all(b"macos-containment-sentinel-v1")?;
                file.sync_all()?;
                sync_parent_directory(path)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            paths.cleanup_best_effort();
            return Err(error);
        }
        Ok(paths)
    }

    fn from_environment() -> io::Result<Self> {
        Ok(Self {
            workspace: required_probe_path(HOSTED_WORKSPACE_SENTINEL)?,
            skill: required_probe_path(HOSTED_SKILL_SENTINEL)?,
            credential: required_probe_path(HOSTED_CREDENTIAL_SENTINEL)?,
            root: PathBuf::new(),
            skill_directory: PathBuf::new(),
            credential_directory: PathBuf::new(),
        })
    }

    fn verify_unchanged(&self) -> io::Result<()> {
        for path in [&self.workspace, &self.skill, &self.credential] {
            if std::fs::read(path)? != b"macos-containment-sentinel-v1" {
                return Err(io::Error::other("macOS containment sentinel changed"));
            }
        }
        Ok(())
    }

    fn cleanup(mut self) -> io::Result<()> {
        self.verify_unchanged()?;
        for path in [&self.workspace, &self.skill, &self.credential] {
            std::fs::remove_file(path)?;
            sync_parent_directory(path)?;
        }
        std::fs::remove_dir(&self.skill_directory)?;
        std::fs::remove_dir(&self.credential_directory)?;
        std::fs::remove_dir(&self.root)?;
        sync_parent_directory(&self.root)?;
        self.root.clear();
        Ok(())
    }

    fn cleanup_best_effort(&self) {
        let _ = std::fs::remove_file(&self.workspace);
        let _ = std::fs::remove_file(&self.skill);
        let _ = std::fs::remove_file(&self.credential);
        let _ = std::fs::remove_dir(&self.skill_directory);
        let _ = std::fs::remove_dir(&self.credential_directory);
        let _ = std::fs::remove_dir(&self.root);
    }
}

impl Drop for HostedProbePaths {
    fn drop(&mut self) {
        if !self.root.as_os_str().is_empty() {
            self.cleanup_best_effort();
        }
    }
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("probe path has no parent"))?,
    )?
    .sync_all()
}

fn required_probe_path(key: &'static str) -> io::Result<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "hosted probe path is missing"))
}

#[allow(unsafe_code)]
pub(super) fn standard_streams_are_protocol_pipes() -> bool {
    fn is_pipe(fd: RawFd) -> bool {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `metadata` points to writable storage for the synchronous fstat call.
        (unsafe { libc::fstat(fd, metadata.as_mut_ptr()) }) == 0
            && (unsafe { metadata.assume_init() }.st_mode & libc::S_IFMT) == libc::S_IFIFO
    }

    is_pipe(std::io::stdin().as_raw_fd())
        && is_pipe(std::io::stdout().as_raw_fd())
        && is_pipe(std::io::stderr().as_raw_fd())
}

pub(super) fn containment_status() -> WorkerContainmentStatus {
    STATUS.get_or_init(probe_containment).clone()
}

fn probe_containment() -> WorkerContainmentStatus {
    // A libtest executable enters its generated test harness before mini-agent's synchronous main,
    // so it cannot serve as the trusted same-executable guardian used by production. Unit tests
    // exercise the allowlist and construction; the required installed-binary marker exercises the
    // complete live preflight without substituting a test-only launcher.
    #[cfg(test)]
    return WorkerContainmentStatus::Unavailable {
        backend: BACKEND,
        assurance: ASSURANCE,
        reason: "the Rust test harness is not a production macOS worker executable".into(),
    };

    #[cfg(not(test))]
    {
        if let Some(reason) = availability_error() {
            return WorkerContainmentStatus::Unavailable {
                backend: BACKEND,
                assurance: ASSURANCE,
                reason,
            };
        }
        let result = worker_executable()
            .map_err(|error| io::Error::other(error.to_string()))
            .and_then(run_full_containment_preflight);
        match result {
            Ok(()) => WorkerContainmentStatus::Available {
                backend: BACKEND,
                assurance: ASSURANCE,
            },
            Err(_) => WorkerContainmentStatus::Unavailable {
                backend: BACKEND,
                assurance: ASSURANCE,
                reason: "the scoped one-time-image Seatbelt live preflight failed".into(),
            },
        }
    }
}

pub(super) fn launch() -> Result<WorkerProcess, WorkerLaunchError> {
    launch_executable(worker_executable()?, production_worker_args())
}

#[cfg(test)]
pub(super) fn launch_executable_for_benchmark(
    executable: &std::path::Path,
) -> Result<WorkerProcess, WorkerLaunchError> {
    let executable = executable
        .canonicalize()
        .map_err(|source| WorkerLaunchError::Io {
            backend: BACKEND,
            source,
        })?;
    launch_executable(executable, &[])
}

fn availability_error() -> Option<String> {
    availability_error_for(
        trusted_system_executable(Path::new(SANDBOX_EXEC)),
        macos_major_version(),
    )
}

fn availability_error_for(
    trusted_sandbox_exec: bool,
    macos_major: Result<u32, String>,
) -> Option<String> {
    if !trusted_sandbox_exec {
        return Some(missing_sandbox_exec_reason());
    }
    match macos_major {
        Ok(major) if VALIDATED_MACOS_MAJORS.contains(&major) => None,
        Ok(major) => Some(unavailable_reason_for_major(major)),
        Err(reason) => Some(format!(
            "the undocumented/deprecated best-effort MAC policy is disabled because the macOS major version could not be validated: {reason}"
        )),
    }
}

fn missing_sandbox_exec_reason() -> String {
    format!(
        "the undocumented/deprecated best-effort MAC policy is unavailable because {SANDBOX_EXEC} is missing or untrusted"
    )
}

#[cfg(test)]
fn unavailable_reason_from_version_probe(macos_major: Result<u32, String>) -> String {
    match macos_major {
        Ok(major) => unavailable_reason_for_major(major),
        Err(reason) => format!(
            "the undocumented/deprecated best-effort MAC policy is disabled because the macOS major version could not be validated: {reason}"
        ),
    }
}

fn unavailable_reason_for_major(major: u32) -> String {
    format!(
        "the undocumented/deprecated best-effort MAC policy is disabled on unvalidated macOS major version {major}"
    )
}

fn worker_executable() -> Result<PathBuf, WorkerLaunchError> {
    std::env::current_exe().map_err(|source| WorkerLaunchError::Io {
        backend: BACKEND,
        source,
    })
}

#[cfg(not(test))]
fn production_worker_args() -> &'static [&'static str] {
    &[]
}

fn preflight_worker_args() -> &'static [&'static str] {
    production_worker_args()
}

#[cfg(test)]
fn production_worker_args() -> &'static [&'static str] {
    &[
        "--exact",
        "extras::js::tests::worker_runtime::worker_bootstrap_test_child",
        "--nocapture",
    ]
}

fn publication_root() -> Result<PathBuf, WorkerLaunchError> {
    let root = std::env::temp_dir().join(format!(
        "mini-agent-js-worker-publications-{}",
        current_uid()
    ));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(WorkerLaunchError::Io {
                backend: BACKEND,
                source,
            });
        }
    }
    Ok(root)
}

fn launch_executable(
    executable: PathBuf,
    worker_args: &[&str],
) -> Result<WorkerProcess, WorkerLaunchError> {
    match containment_status() {
        WorkerContainmentStatus::Available {
            backend: BACKEND,
            assurance: WorkerContainmentAssurance::DeprecatedBestEffort,
        } => {}
        WorkerContainmentStatus::Available { backend, .. } => {
            return Err(WorkerLaunchError::Unavailable {
                backend,
                reason: "worker containment preflight selected the wrong backend".into(),
            });
        }
        WorkerContainmentStatus::Unavailable {
            backend, reason, ..
        } => {
            return Err(WorkerLaunchError::Unavailable { backend, reason });
        }
    }

    launch_executable_unchecked(executable, worker_args)
}

fn launch_executable_unchecked(
    executable: PathBuf,
    worker_args: &[&str],
) -> Result<WorkerProcess, WorkerLaunchError> {
    launch_executable_unchecked_with_probe(executable, worker_args, None)
}

fn launch_executable_unchecked_with_probe(
    executable: PathBuf,
    worker_args: &[&str],
    probe: Option<&HostedProbePaths>,
) -> Result<WorkerProcess, WorkerLaunchError> {
    let root = publication_root()?;
    retry_busy_sweep(Instant::now() + SWEEP_CONTENTION_TIMEOUT, || {
        stale_sweep::sweep_production_publications(&root)
    })
    .map_err(|source| WorkerLaunchError::Io {
        backend: BACKEND,
        source,
    })?;
    let image =
        one_time_image::OneTimeWorkerImage::prepare_from(&executable, &root).map_err(|source| {
            WorkerLaunchError::Io {
                backend: BACKEND,
                source,
            }
        })?;
    let profile = seatbelt_profile(image.image_path()).map_err(|source| WorkerLaunchError::Io {
        backend: BACKEND,
        source,
    })?;

    let (heartbeat_parent, heartbeat_guardian) =
        UnixStream::pair().map_err(|source| WorkerLaunchError::Io {
            backend: BACKEND,
            source,
        })?;
    let guardian_descriptor = heartbeat_guardian.as_raw_fd();
    let mut command = Command::new(&executable);
    command
        .env_clear()
        .env(super::MACOS_GUARDIAN_MARKER, GUARDIAN_MARKER_VALUE)
        .arg(&profile)
        .arg(image.image_path())
        .arg("--")
        .args(worker_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(probe) = probe {
        command
            .env(HOSTED_PROBE_MARKER, HOSTED_PROBE_MARKER_VALUE)
            .env(HOSTED_WORKSPACE_SENTINEL, &probe.workspace)
            .env(HOSTED_SKILL_SENTINEL, &probe.skill)
            .env(HOSTED_CREDENTIAL_SENTINEL, &probe.credential)
            .env(HOSTED_ORIGINAL_EXECUTABLE, &executable)
            .env(HOSTED_ONE_TIME_IMAGE, image.image_path());
    }
    configure_guardian_spawn(&mut command, guardian_descriptor).map_err(|source| {
        WorkerLaunchError::Io {
            backend: BACKEND,
            source,
        }
    })?;

    let mut child = command.spawn().map_err(|source| WorkerLaunchError::Io {
        backend: BACKEND,
        source,
    })?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| cleanup_failed_launch(&mut child, "stdin"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| cleanup_failed_launch(&mut child, "stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| cleanup_failed_launch(&mut child, "stderr"))?;

    Ok(WorkerProcess {
        process: WorkerChild {
            child,
            image: Some(image),
            heartbeat: Some(heartbeat_parent),
            #[cfg(test)]
            unconfined_test_child: false,
        },
        input: super::child_stdin_file(input),
        output: super::child_stdout_file(output),
        stderr: super::child_stderr_file(stderr),
        backend: BACKEND,
        #[cfg(test)]
        reap_observer: None,
        #[cfg(test)]
        force_tree_termination_error: false,
        #[cfg(test)]
        authenticated_ready_observer: None,
        #[cfg(test)]
        force_authenticated_ready_finalization_error: false,
        #[cfg(test)]
        parent_write_observer: None,
    })
}

pub(super) fn maybe_run_guardian() -> Option<ExitCode> {
    if std::env::var_os(super::MACOS_GUARDIAN_MARKER).as_deref()
        != Some(std::ffi::OsStr::new(GUARDIAN_MARKER_VALUE))
    {
        return None;
    }
    Some(match run_guardian() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=worker_exit");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_bootstrap");
            ExitCode::FAILURE
        }
    })
}

pub(super) fn maybe_run_hosted_lifecycle() -> Option<ExitCode> {
    if std::env::var_os(HOSTED_PARENT_DEATH_MARKER).as_deref()
        == Some(std::ffi::OsStr::new(HOSTED_PARENT_DEATH_MARKER_VALUE))
    {
        run_parent_death_canary_child();
    }
    if std::env::var_os(super::MACOS_HOSTED_LIFECYCLE_MARKER).as_deref()
        != Some(std::ffi::OsStr::new(HOSTED_LIFECYCLE_MARKER_VALUE))
    {
        return None;
    }
    Some(match run_hosted_containment_matrix() {
        Ok(()) => {
            println!("{HOSTED_PASS_RECORD}");
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::FAILURE,
    })
}

fn run_full_containment_preflight(executable: PathBuf) -> io::Result<()> {
    let probes = HostedProbePaths::create().inspect_err(|_error| {
        eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=sentinel_setup");
    })?;
    probes.verify_unchanged().inspect_err(|_error| {
        eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=sentinel_precheck");
    })?;
    let result = (|| {
        let mut process = launch_executable_unchecked_with_probe(
            executable.clone(),
            preflight_worker_args(),
            Some(&probes),
        )
        .map_err(|error| {
            eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=launch");
            io::Error::other(error.to_string())
        })?;
        let diagnostic_drain = spawn_closed_probe_diagnostic_drain(&process.stderr)?;
        let authentication = authenticate_ready_and_probe(&mut process, true);
        if authentication.is_err() {
            let _ = process.terminate_and_reap(PREFLIGHT_TIMEOUT);
        }
        let closed_code = diagnostic_drain.join().ok().flatten();
        if let Some(code) = closed_code {
            eprintln!("{code}");
        }
        if let Err(error) = authentication {
            if closed_code.is_none() {
                eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=bootstrap");
            }
            eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=worker_attestation");
            return Err(error);
        }
        probes.verify_unchanged().inspect_err(|_error| {
            eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=sentinel_postcheck");
        })?;
        probe_guardian_parent_death(&executable, &probes).inspect_err(|_error| {
            eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=parent_death");
        })?;
        probes.verify_unchanged()
    })();
    let cleanup = probes.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

fn spawn_closed_probe_diagnostic_drain(
    stderr: &std::fs::File,
) -> io::Result<std::thread::JoinHandle<Option<&'static str>>> {
    let mut stderr = stderr.try_clone()?;
    std::thread::Builder::new()
        .name("macos-probe-diagnostic".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let _ = (&mut stderr).take(512).read_to_end(&mut bytes);
            parse_closed_probe_diagnostic(&bytes)
        })
}

fn parse_closed_probe_diagnostic(bytes: &[u8]) -> Option<&'static str> {
    let text = std::str::from_utf8(bytes).ok()?;
    const CODES: &[&str] = &[
        "MACOS_CONTAINMENT_PROBE_FAILED=marker",
        "MACOS_CONTAINMENT_PROBE_FAILED=workspace_input",
        "MACOS_CONTAINMENT_PROBE_FAILED=skill_input",
        "MACOS_CONTAINMENT_PROBE_FAILED=credential_input",
        "MACOS_CONTAINMENT_PROBE_FAILED=original_input",
        "MACOS_CONTAINMENT_PROBE_FAILED=image_input",
        "MACOS_CONTAINMENT_PROBE_FAILED=process_group_input",
        "MACOS_CONTAINMENT_PROBE_FAILED=descriptor_input",
        "MACOS_CONTAINMENT_PROBE_FAILED=workspace_sentinel",
        "MACOS_CONTAINMENT_PROBE_FAILED=skill_sentinel",
        "MACOS_CONTAINMENT_PROBE_FAILED=credential_sentinel",
        "MACOS_CONTAINMENT_PROBE_FAILED=network",
        "MACOS_CONTAINMENT_PROBE_FAILED=fork",
        "MACOS_CONTAINMENT_PROBE_FAILED=alternate_exec",
        "MACOS_CONTAINMENT_PROBE_FAILED=original_exec",
        "MACOS_CONTAINMENT_PROBE_FAILED=image_exec",
        "MACOS_CONTAINMENT_PROBE_FAILED=dev_fd_exec",
        "MACOS_CONTAINMENT_PROBE_FAILED=descriptors",
        "MACOS_CONTAINMENT_PROBE_FAILED=rlimits",
        "MACOS_CONTAINMENT_PROBE_FAILED=process_group",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_bootstrap",
        "MACOS_CONTAINMENT_PROBE_FAILED=worker_exit",
        "MACOS_CONTAINMENT_PROBE_FAILED=worker_spawn",
        "MACOS_CONTAINMENT_PROBE_FAILED=worker_limits",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_arguments",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_environment",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_heartbeat",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_group",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_monitor",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_streams",
        "MACOS_CONTAINMENT_PROBE_FAILED=guardian_sandbox_exec",
    ];
    text.lines()
        .find_map(|line| CODES.iter().copied().find(|code| line == *code))
}

fn run_hosted_containment_matrix() -> io::Result<()> {
    run_full_containment_preflight(
        worker_executable().map_err(|error| io::Error::other(error.to_string()))?,
    )
}

#[allow(unsafe_code)]
fn probe_guardian_parent_death(executable: &Path, probes: &HostedProbePaths) -> io::Result<()> {
    let publication_root =
        publication_root().map_err(|error| io::Error::other(error.to_string()))?;
    let output = Command::new(executable)
        .env_clear()
        .env(HOSTED_PARENT_DEATH_MARKER, HOSTED_PARENT_DEATH_MARKER_VALUE)
        .env(HOSTED_WORKSPACE_SENTINEL, &probes.workspace)
        .env(HOSTED_SKILL_SENTINEL, &probes.skill)
        .env(HOSTED_CREDENTIAL_SENTINEL, &probes.credential)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() || output.stdout.len() > 96 {
        return Err(io::Error::other("macOS parent-death canary child failed"));
    }
    let (guardian, orphan) = parse_parent_death_record(&output.stdout)
        .ok_or_else(|| io::Error::other("macOS parent-death canary record was invalid"))?;
    let orphan_path = publication_root.join(&orphan);
    let orphan_metadata = std::fs::symlink_metadata(&orphan_path)?;
    if !orphan_metadata.is_dir()
        || orphan_metadata.uid() != current_uid()
        || orphan_metadata.mode() & 0o7777 != 0o700
    {
        return Err(io::Error::other(
            "macOS parent-death publication identity was invalid",
        ));
    }
    let orphan_identity = (orphan_metadata.dev(), orphan_metadata.ino());
    let deadline = Instant::now() + PREFLIGHT_TIMEOUT;
    loop {
        // SAFETY: signal zero performs an existence check without modifying the process group.
        if unsafe { libc::kill(-guardian, 0) } < 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            retry_busy_sweep(Instant::now() + SWEEP_CONTENTION_TIMEOUT, || {
                stale_sweep::sweep_hosted_parent_death_publications(&publication_root)
            })?;
            if std::fs::symlink_metadata(&orphan_path).is_ok() || orphan_identity == (0, 0) {
                return Err(io::Error::other(
                    "macOS parent-death publication identity survived recovery",
                ));
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "macOS guardian process group survived parent death",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn retry_busy_sweep(
    deadline: Instant,
    mut sweep: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    loop {
        match sweep() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "one-time worker publication root remained busy",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

fn parse_parent_death_record(bytes: &[u8]) -> Option<(libc::pid_t, std::ffi::OsString)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut fields = text.split_ascii_whitespace();
    let guardian = fields
        .next()?
        .parse::<libc::pid_t>()
        .ok()
        .filter(|value| *value > 0)?;
    let orphan = std::ffi::OsString::from(fields.next()?);
    if fields.next().is_some()
        || !orphan
            .to_str()
            .is_some_and(is_canonical_publication_directory_name)
    {
        return None;
    }
    Some((guardian, orphan))
}

#[allow(unsafe_code)]
fn run_parent_death_canary_child() -> ! {
    let result = (|| {
        let probes = HostedProbePaths::from_environment()?;
        let executable =
            worker_executable().map_err(|error| io::Error::other(error.to_string()))?;
        let mut process = launch_executable_unchecked_with_probe(
            executable,
            preflight_worker_args(),
            Some(&probes),
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        authenticate_ready_and_probe(&mut process, false)?;
        let guardian = process.id();
        let publication = process
            .process
            .publication_directory_name()?
            .to_str()
            .ok_or_else(|| io::Error::other("macOS parent-death publication name was not UTF-8"))?;
        println!("{guardian} {publication}");
        std::io::stdout().flush()?;
        std::mem::forget(process);
        Ok::<(), io::Error>(())
    })();
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 1 }) }
}

#[allow(unsafe_code)]
fn run_guardian() -> io::Result<ExitStatus> {
    if !standard_streams_are_protocol_pipes() {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_streams");
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "macOS guardian bootstrap was not trusted",
        ));
    }
    if !trusted_system_executable(Path::new(SANDBOX_EXEC)) {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_sandbox_exec");
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "macOS guardian bootstrap was not trusted",
        ));
    }
    let (profile, image, worker_arguments) = parse_guardian_arguments(std::env::args_os().skip(1))
        .inspect_err(|_error| {
            eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_arguments");
        })?;
    let probe_environment = if std::env::var_os(HOSTED_PROBE_MARKER).as_deref()
        == Some(std::ffi::OsStr::new(HOSTED_PROBE_MARKER_VALUE))
    {
        Some([
            required_probe_environment(HOSTED_WORKSPACE_SENTINEL)?,
            required_probe_environment(HOSTED_SKILL_SENTINEL)?,
            required_probe_environment(HOSTED_CREDENTIAL_SENTINEL)?,
            required_probe_environment(HOSTED_ORIGINAL_EXECUTABLE)?,
            required_probe_environment(HOSTED_ONE_TIME_IMAGE)?,
        ])
    } else {
        None
    };

    let heartbeat = guardian_heartbeat().inspect_err(|_error| {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_heartbeat");
    })?;
    let process_group = current_process_group().inspect_err(|_error| {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_group");
    })?;
    let descriptor_bound = finalize_guardian_process().inspect_err(|_error| {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=worker_limits");
    })?;
    std::thread::Builder::new()
        .name("macos-worker-parent-death".into())
        .spawn(move || {
            let mut heartbeat = heartbeat;
            let mut byte = [0_u8; 1];
            while heartbeat.read(&mut byte).is_ok_and(|read| read != 0) {}
            // SAFETY: the guardian owns this dedicated process group. EOF means its parent died
            // or intentionally dropped ownership, so killing the complete group is fail closed.
            unsafe { libc::kill(-process_group, libc::SIGKILL) };
        })
        .inspect_err(|_error| {
            eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_monitor");
        })?;

    let mut worker = Command::new(SANDBOX_EXEC);
    worker
        .env_clear()
        .env(
            super::INTERNAL_WORKER_MARKER,
            super::INTERNAL_WORKER_MARKER_VALUE,
        )
        .args([std::ffi::OsStr::new("-p"), profile.as_os_str()])
        .arg(&image)
        .args(worker_arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(environment) = probe_environment {
        worker
            .env(HOSTED_PROBE_MARKER, HOSTED_PROBE_MARKER_VALUE)
            .env(HOSTED_GUARDIAN_PGID, process_group.to_string())
            .env(HOSTED_DESCRIPTOR_BOUND, descriptor_bound.to_string());
        for (key, value) in environment {
            worker.env(key, value);
        }
    }
    worker
        .spawn()
        .inspect_err(|_error| {
            eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=worker_spawn");
        })?
        .wait()
}

fn required_probe_environment(key: &'static str) -> io::Result<(&'static str, std::ffi::OsString)> {
    std::env::var_os(key)
        .map(|value| (key, value))
        .ok_or_else(|| {
            eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=guardian_environment");
            io::Error::new(io::ErrorKind::InvalidInput, "hosted probe input is missing")
        })
}

fn parse_guardian_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> io::Result<(std::ffi::OsString, PathBuf, Vec<std::ffi::OsString>)> {
    use std::os::unix::ffi::OsStrExt;

    let profile = arguments.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "guardian profile is missing")
    })?;
    if profile.as_os_str().as_bytes().len() > MAX_GUARDIAN_PROFILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guardian profile exceeds its bound",
        ));
    }
    let image =
        PathBuf::from(arguments.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "guardian image is missing")
        })?);
    if image.as_os_str().as_bytes().len() > MAX_GUARDIAN_PATH_BYTES
        || !image.is_absolute()
        || image.file_name() != Some(std::ffi::OsStr::new("worker-image"))
        || !image
            .parent()
            .and_then(Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(is_canonical_publication_directory_name)
        || image
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            != Some(
                std::ffi::OsString::from(format!(
                    "mini-agent-js-worker-publications-{}",
                    current_uid()
                ))
                .as_os_str(),
            )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guardian image path is not a scoped publication",
        ));
    }
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guardian argument separator is missing",
        ));
    }
    let worker_arguments = arguments.collect::<Vec<_>>();
    if worker_arguments.len() > MAX_GUARDIAN_WORKER_ARGUMENTS
        || worker_arguments.iter().any(|argument| {
            argument.as_os_str().as_bytes().len() > MAX_GUARDIAN_WORKER_ARGUMENT_BYTES
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guardian worker arguments exceed their bounds",
        ));
    }
    let configured_arguments = production_worker_args();
    if worker_arguments.len() != configured_arguments.len()
        || worker_arguments
            .iter()
            .zip(configured_arguments)
            .any(|(actual, expected)| actual != expected)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guardian worker arguments were not parent-configured",
        ));
    }
    let expected_profile = seatbelt_profile(&image)?;
    if profile.as_os_str().as_bytes() != expected_profile.as_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guardian profile does not match its one-time image",
        ));
    }
    Ok((profile, image, worker_arguments))
}

fn is_canonical_publication_directory_name(name: &str) -> bool {
    let Some(uuid) = name.strip_prefix(".mini-agent-js-worker-") else {
        return false;
    };
    uuid::Uuid::parse_str(uuid).is_ok_and(|parsed| parsed.hyphenated().to_string() == uuid)
}

#[allow(unsafe_code)]
fn guardian_heartbeat() -> io::Result<std::fs::File> {
    // SAFETY: descriptor 3 is installed by the parent immediately before this trusted guardian
    // exec. Ownership transfers exactly once to File.
    let file = unsafe { std::fs::File::from_raw_fd(3) };
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[allow(unsafe_code)]
fn current_process_group() -> io::Result<libc::pid_t> {
    let process = unsafe { libc::getpid() };
    let group = unsafe { libc::getpgrp() };
    if group <= 0 || process <= 0 || group != process {
        Err(io::Error::other(
            "guardian does not own its dedicated process group",
        ))
    } else {
        Ok(group)
    }
}

fn seatbelt_profile(image: &Path) -> io::Result<String> {
    let image = image.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker image path is not UTF-8",
        )
    })?;
    if image.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker image path contains a control character",
        ));
    }
    let image = image.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal "{image}"))
(allow file-read* (literal "{image}") (subpath "/System/Library") (subpath "/usr/lib"))
(allow file-read-data (literal "/"))
(allow file-read-data file-write-data (vnode-type FIFO))
(allow sysctl-read)
(allow signal (target self))
(allow process-info* (target self))"#
    ))
}

#[allow(unsafe_code)]
fn inherited_descriptor_bound() -> io::Result<RawFd> {
    let open_max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if open_max <= 3 || open_max > i64::from(MAX_PROBE_DESCRIPTOR_BOUND) {
        return Err(io::Error::other(
            "could not determine the inherited descriptor bound",
        ));
    }
    Ok(open_max as RawFd)
}

#[allow(unsafe_code)]
fn configure_guardian_spawn(command: &mut Command, guardian_descriptor: RawFd) -> io::Result<()> {
    // Keep the post-fork closure syscall-only and do not close Rust's private exec-error pipe.
    // The trusted guardian closes inherited descriptors and applies limits immediately after exec,
    // before it starts a thread or launches the untrusted worker.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(guardian_descriptor, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            let heartbeat_flags = libc::fcntl(3, libc::F_GETFD);
            if heartbeat_flags < 0
                || libc::fcntl(3, libc::F_SETFD, heartbeat_flags & !libc::FD_CLOEXEC) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[allow(unsafe_code)]
fn finalize_guardian_process() -> io::Result<RawFd> {
    let descriptor_bound = inherited_descriptor_bound()?;
    for descriptor in 4..descriptor_bound {
        if unsafe { libc::close(descriptor) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(error);
            }
        }
    }
    set_limit(libc::RLIMIT_AS, ADDRESS_SPACE_LIMIT)?;
    set_limit(libc::RLIMIT_CPU, CPU_LIMIT_SECONDS)?;
    set_limit(libc::RLIMIT_NOFILE, FILE_DESCRIPTOR_LIMIT)?;
    set_limit(libc::RLIMIT_CORE, 0)?;
    set_limit(libc::RLIMIT_FSIZE, FILE_SIZE_LIMIT)?;
    Ok(descriptor_bound)
}

#[allow(unsafe_code)]
fn set_limit(resource: libc::c_int, value: libc::rlim_t) -> io::Result<()> {
    let mut inherited = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(resource, &mut inherited) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // An unprivileged child cannot raise a hard limit inherited from the host.
    // Retaining the stricter value still leaves the worker unable to relax it.
    let value = bounded_limit(value, inherited.rlim_max);
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `limit` is initialized for the synchronous Darwin `setrlimit` call.
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut observed = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(resource, &mut observed) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if observed.rlim_cur != value || observed.rlim_max != value {
        return Err(io::Error::other(
            "macOS worker resource limit readback did not match",
        ));
    }
    Ok(())
}

const fn bounded_limit(requested: libc::rlim_t, inherited_max: libc::rlim_t) -> libc::rlim_t {
    if requested < inherited_max {
        requested
    } else {
        inherited_max
    }
}

pub(super) fn attest_hosted_worker_containment() -> bool {
    macro_rules! require_probe {
        ($condition:expr, $code:literal) => {
            if !$condition {
                eprintln!(concat!("MACOS_CONTAINMENT_PROBE_FAILED=", $code));
                return false;
            }
        };
    }
    if std::env::var_os(HOSTED_PROBE_MARKER).as_deref()
        != Some(std::ffi::OsStr::new(HOSTED_PROBE_MARKER_VALUE))
    {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=marker");
        return false;
    }
    let Some(workspace) = std::env::var_os(HOSTED_WORKSPACE_SENTINEL).map(PathBuf::from) else {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=workspace_input");
        return false;
    };
    let Some(skill) = std::env::var_os(HOSTED_SKILL_SENTINEL).map(PathBuf::from) else {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=skill_input");
        return false;
    };
    let Some(credential) = std::env::var_os(HOSTED_CREDENTIAL_SENTINEL).map(PathBuf::from) else {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=credential_input");
        return false;
    };
    let Some(original) = std::env::var_os(HOSTED_ORIGINAL_EXECUTABLE).map(PathBuf::from) else {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=original_input");
        return false;
    };
    let Some(image) = std::env::var_os(HOSTED_ONE_TIME_IMAGE).map(PathBuf::from) else {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=image_input");
        return false;
    };
    let Some(expected_group) = std::env::var(HOSTED_GUARDIAN_PGID)
        .ok()
        .and_then(|value| value.parse::<libc::pid_t>().ok())
    else {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=process_group_input");
        return false;
    };
    let Some(descriptor_bound) = std::env::var(HOSTED_DESCRIPTOR_BOUND)
        .ok()
        .and_then(|value| value.parse::<RawFd>().ok())
        .filter(|value| (4..=MAX_PROBE_DESCRIPTOR_BOUND).contains(value))
    else {
        eprintln!("MACOS_CONTAINMENT_PROBE_FAILED=descriptor_input");
        return false;
    };

    require_probe!(sentinel_access_is_denied(&workspace), "workspace_sentinel");
    require_probe!(sentinel_access_is_denied(&skill), "skill_sentinel");
    require_probe!(
        sentinel_access_is_denied(&credential),
        "credential_sentinel"
    );
    require_probe!(network_connection_matrix_is_denied(), "network");
    require_probe!(fork_is_denied(), "fork");
    require_probe!(
        executable_fails_with(Path::new("/bin/true"), &[libc::EPERM, libc::EACCES]),
        "alternate_exec"
    );
    require_probe!(
        executable_fails_with(&original, &[libc::EPERM, libc::EACCES]),
        "original_exec"
    );
    // The parent has already completed the descriptor/inode-verified unlink. Depending on whether
    // Seatbelt rejects traversal of the now-missing private leaf before pathname resolution,
    // Darwin reports either absence or a policy denial. All three outcomes remain fail closed.
    require_probe!(
        executable_fails_with(&image, &[libc::ENOENT, libc::EPERM, libc::EACCES]),
        "image_exec"
    );
    require_probe!(
        ["/dev/fd/0", "/dev/fd/1", "/dev/fd/2"]
            .into_iter()
            .all(|path| executable_fails_with(
                Path::new(path),
                &[libc::EPERM, libc::EACCES, libc::ENOENT, libc::ENOEXEC]
            )),
        "dev_fd_exec"
    );
    require_probe!(
        exact_protocol_descriptors_are_open(descriptor_bound),
        "descriptors"
    );
    require_probe!(resource_limits_match(), "rlimits");
    require_probe!(guardian_group_matches(expected_group), "process_group");
    true
}

fn sentinel_access_is_denied(path: &Path) -> bool {
    io_error_is_policy_denial(std::fs::File::open(path))
        && io_error_is_policy_denial(std::fs::OpenOptions::new().write(true).open(path))
}

fn io_error_is_policy_denial<T>(result: io::Result<T>) -> bool {
    result
        .is_err_and(|error| matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)))
}

#[allow(unsafe_code)]
fn network_connection_matrix_is_denied() -> bool {
    for (domain, socket_type) in [
        (libc::AF_INET, libc::SOCK_STREAM),
        (libc::AF_INET, libc::SOCK_DGRAM),
        (libc::AF_INET6, libc::SOCK_STREAM),
        (libc::AF_INET6, libc::SOCK_DGRAM),
    ] {
        let descriptor = unsafe { libc::socket(domain, socket_type, 0) };
        if descriptor < 0 {
            if io_error_is_policy_denial(io::Result::<()>::Err(io::Error::last_os_error())) {
                continue;
            }
            return false;
        }
        let denied = if domain == libc::AF_INET {
            let address = libc::sockaddr_in {
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as u8,
                sin_port: 9_u16.to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
                },
                sin_zero: [0; 8],
            };
            connection_is_policy_denied(
                descriptor,
                std::ptr::from_ref(&address).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        } else {
            let address = libc::sockaddr_in6 {
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as u8,
                sin6_port: 9_u16.to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr {
                    s6_addr: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                },
                sin6_scope_id: 0,
            };
            connection_is_policy_denied(
                descriptor,
                std::ptr::from_ref(&address).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        unsafe { libc::close(descriptor) };
        if !denied {
            return false;
        }
    }
    true
}

#[allow(unsafe_code)]
fn connection_is_policy_denied(
    descriptor: RawFd,
    address: *const libc::sockaddr,
    address_length: libc::socklen_t,
) -> bool {
    if unsafe { libc::connect(descriptor, address, address_length) } == 0 {
        return false;
    }
    io_error_is_policy_denial(io::Result::<()>::Err(io::Error::last_os_error()))
}

#[allow(unsafe_code)]
fn fork_is_denied() -> bool {
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::_exit(127) };
    }
    if child > 0 {
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        return false;
    }
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM) | Some(libc::EACCES)
    )
}

fn executable_fails_with(path: &Path, expected_errors: &[i32]) -> bool {
    let spawned = Command::new(path)
        .env_clear()
        // Reuse the already-attested protocol pipes. Opening `/dev/null` is itself denied by the
        // profile and would make this canary measure stdio setup instead of the requested exec.
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    match spawned {
        Err(error) => error
            .raw_os_error()
            .is_some_and(|code| expected_errors.contains(&code)),
        Ok(mut child) => {
            // Some Darwin versions return from posix_spawn before Seatbelt
            // rejects image activation. Accept only a prompt unsuccessful
            // child outcome; success proves the alternate image executed, and
            // a live child is killed and treated as a failed canary.
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return !status.success(),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return false;
                    }
                }
            }
        }
    }
}

#[allow(unsafe_code)]
fn exact_protocol_descriptors_are_open(bound: RawFd) -> bool {
    for descriptor in 0..bound {
        let result = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if descriptor <= 2 {
            if result < 0 {
                return false;
            }
        } else if result >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EBADF) {
            return false;
        }
    }
    true
}

#[allow(unsafe_code)]
fn resource_limits_match() -> bool {
    [
        (libc::RLIMIT_AS, ADDRESS_SPACE_LIMIT),
        (libc::RLIMIT_CPU, CPU_LIMIT_SECONDS),
        (libc::RLIMIT_NOFILE, FILE_DESCRIPTOR_LIMIT),
        (libc::RLIMIT_CORE, 0),
        (libc::RLIMIT_FSIZE, FILE_SIZE_LIMIT),
    ]
    .into_iter()
    .all(|(resource, expected)| {
        let mut observed = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        (unsafe { libc::getrlimit(resource, &mut observed) }) == 0
            && observed.rlim_cur == observed.rlim_max
            && observed.rlim_cur <= expected
            && (expected == 0 || observed.rlim_cur > 0)
    })
}

#[allow(unsafe_code)]
fn guardian_group_matches(expected: libc::pid_t) -> bool {
    expected > 0
        && (unsafe { libc::getpgrp() }) == expected
        && (unsafe { libc::getpid() }) != expected
}

fn cleanup_failed_launch(child: &mut Child, pipe: &'static str) -> WorkerLaunchError {
    let _ = super::terminate_worker_process_group(child.id());
    let _ = child.wait();
    WorkerLaunchError::MissingPipe { pipe }
}

fn authenticate_ready_and_probe(
    process: &mut WorkerProcess,
    graceful_teardown: bool,
) -> io::Result<()> {
    use crate::extras::js::protocol::{
        BuildIdentity, ContainmentAttestation, ContainmentProbe, ParentFrame, ParentProtocol,
        WireFrame, WorkerFrame, write_frame,
    };

    let build = BuildIdentity::current();
    let mut protocol = ParentProtocol::new(build.clone());
    let hello = WireFrame::connection(build.clone(), 0, ParentFrame::Hello(protocol.hello()));
    protocol
        .on_send(&hello)
        .map_err(|_| io::Error::other("macOS preflight rejected Hello"))?;
    write_frame(&mut process.input, &hello)
        .map_err(|_| io::Error::other("macOS preflight could not encode Hello"))?;
    process.input.flush()?;

    let ready = read_worker_frame_bounded(&mut process.output, PREFLIGHT_TIMEOUT).inspect_err(
        |_error| {
            eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=ready_read");
        },
    )?;
    protocol
        .on_receive(&ready)
        .map_err(|_| io::Error::other("macOS preflight received unauthenticated Ready"))?;
    if !matches!(ready.message, WorkerFrame::Ready(_)) {
        return Err(io::Error::other(
            "macOS preflight received a non-Ready frame",
        ));
    }
    process
        .finalize_authenticated_ready()
        .inspect_err(|_error| {
            eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=image_unlink");
        })?;

    let containment = WireFrame::connection(
        build.clone(),
        2,
        ParentFrame::ContainmentProbe(ContainmentProbe {}),
    );
    protocol
        .on_send(&containment)
        .map_err(|_| io::Error::other("macOS hosted containment probe was rejected"))?;
    write_frame(&mut process.input, &containment)
        .map_err(|_| io::Error::other("macOS hosted containment probe could not be encoded"))?;
    process.input.flush()?;
    let attested = read_worker_frame_bounded(&mut process.output, PREFLIGHT_TIMEOUT).inspect_err(
        |_error| {
            eprintln!("MACOS_CONTAINMENT_MATRIX_FAILED=attestation_read");
        },
    )?;
    protocol
        .on_receive(&attested)
        .map_err(|_| io::Error::other("macOS hosted containment attestation was invalid"))?;
    if !matches!(
        attested.message,
        WorkerFrame::ContainmentAttested(ContainmentAttestation::Passed)
    ) {
        return Err(io::Error::other(
            "macOS hosted containment attestation did not pass",
        ));
    }
    if !graceful_teardown {
        return Ok(());
    }

    let shutdown = WireFrame::connection(build, 4, ParentFrame::Shutdown);
    protocol
        .on_send(&shutdown)
        .map_err(|_| io::Error::other("macOS preflight rejected graceful Shutdown"))?;
    write_frame(&mut process.input, &shutdown)
        .map_err(|_| io::Error::other("macOS preflight could not encode graceful Shutdown"))?;
    process.input.flush()?;

    let deadline = Instant::now() + PREFLIGHT_TIMEOUT;
    loop {
        if let Some(status) = process.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(
                    "macOS preflight rejected graceful Shutdown",
                ))
            };
        }
        if Instant::now() >= deadline {
            let _ = process.terminate_and_reap(PREFLIGHT_TIMEOUT);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "macOS preflight graceful Shutdown timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[allow(unsafe_code)]
fn read_worker_frame_bounded(
    reader: &mut std::fs::File,
    timeout: Duration,
) -> io::Result<crate::extras::js::protocol::WorkerWireFrame> {
    use crate::extras::js::protocol::{MAX_FRAME_BYTES, read_frame};

    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let deadline = Instant::now() + timeout;
    let mut encoded = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "macOS preflight EOF",
                ));
            }
            Ok(read) => encoded.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }

        let mut offset = 0_usize;
        while encoded.len().saturating_sub(offset) >= 5 {
            let length = u32::from_be_bytes(
                encoded[offset..offset + 4]
                    .try_into()
                    .map_err(|_| io::Error::other("invalid macOS Ready prefix"))?,
            ) as usize;
            if length > 0 && length <= MAX_FRAME_BYTES && encoded[offset + 4] == b'{' {
                let end = offset.saturating_add(4).saturating_add(length);
                if end <= encoded.len()
                    && let Ok(frame) = read_frame(&mut &encoded[offset..end])
                {
                    return Ok(frame);
                }
            }
            offset += 1;
            if offset > 4096 {
                return Err(io::Error::other(
                    "macOS preflight Ready preamble exceeded its bound",
                ));
            }
        }
        if encoded.len() > MAX_FRAME_BYTES + 4096 {
            return Err(io::Error::other("macOS preflight Ready exceeded its bound"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "macOS preflight Ready timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[allow(unsafe_code)]
fn current_uid() -> u32 {
    // SAFETY: getuid takes no arguments and has no failure mode.
    unsafe { libc::getuid() }
}

fn trusted_system_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_file()
        && metadata.uid() == 0
        && metadata.mode() & 0o022 == 0
        && metadata.mode() & 0o111 != 0
}

fn macos_major_version() -> Result<u32, String> {
    if !trusted_system_executable(Path::new(SW_VERS)) {
        return Err(format!("{SW_VERS} is missing or untrusted"));
    }
    let output = std::process::Command::new(SW_VERS)
        .env_clear()
        .arg("-productVersion")
        .output()
        .map_err(|_| format!("{SW_VERS} could not be executed"))?;
    if !output.status.success() {
        return Err(format!("{SW_VERS} rejected the version query"));
    }
    parse_macos_major(&output.stdout)
}

fn parse_macos_major(version: &[u8]) -> Result<u32, String> {
    let version = std::str::from_utf8(version)
        .map_err(|_| "the version was not UTF-8".to_string())?
        .trim();
    let major = version
        .split('.')
        .next()
        .filter(|component| !component.is_empty())
        .ok_or_else(|| "the version was empty".to_string())?
        .parse::<u32>()
        .map_err(|_| "the major version was not numeric".to_string())?;
    if major == 0 {
        return Err("the major version was zero".to_string());
    }
    Ok(major)
}

#[derive(Debug)]
pub(super) struct WorkerChild {
    child: Child,
    image: Option<one_time_image::OneTimeWorkerImage>,
    heartbeat: Option<UnixStream>,
    #[cfg(test)]
    unconfined_test_child: bool,
}

impl WorkerChild {
    #[cfg(test)]
    pub(super) fn from_unconfined_test_child(child: Child) -> Self {
        Self {
            child,
            image: None,
            heartbeat: None,
            unconfined_test_child: true,
        }
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    fn publication_directory_name(&self) -> io::Result<&std::ffi::OsStr> {
        self.image
            .as_ref()
            .map(|image| image.directory_name())
            .ok_or_else(|| io::Error::other("macOS worker has no owned publication"))
    }

    pub(super) fn finalize_authenticated_ready(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if self.unconfined_test_child {
            return Ok(());
        }
        let image = self
            .image
            .as_mut()
            .ok_or_else(|| io::Error::other("macOS worker has no authenticated publication"))?;
        image.unlink_after_exec()
    }

    pub(super) fn retire_after_reap(&mut self) -> io::Result<()> {
        let Some(image) = self.image.as_mut() else {
            return Ok(());
        };
        image.retire_after_reap()?;
        self.image.take();
        Ok(())
    }

    pub(super) fn terminate_tree(&mut self) -> io::Result<()> {
        self.heartbeat.take();
        super::terminate_worker_process_group(self.child.id())
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

mod one_time_image {
    use super::*;
    #[cfg(test)]
    use core_foundation::array::{
        CFArray, CFArrayGetCount, CFArrayGetTypeID, CFArrayGetValueAtIndex, CFArrayRef,
    };
    #[cfg(test)]
    use core_foundation::base::{CFGetTypeID, CFTypeRef, OSStatus, TCFType};
    #[cfg(test)]
    use core_foundation::data::{
        CFData, CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID, CFDataRef,
    };
    #[cfg(test)]
    use core_foundation::dictionary::{
        CFDictionary, CFDictionaryGetValueIfPresent, CFDictionaryRef,
    };
    #[cfg(test)]
    use core_foundation::string::{CFString, CFStringRef};
    #[cfg(test)]
    use core_foundation::url::CFURL;
    #[cfg(test)]
    use security_framework::os::macos::code_signing::{Flags, SecRequirement, SecStaticCode};
    #[cfg(test)]
    use security_framework_sys::base::errSecSuccess;
    #[cfg(test)]
    use security_framework_sys::code_signing::{
        SecCSFlags, SecStaticCodeCheckValidity, SecStaticCodeRef,
    };
    use sha2::{Digest, Sha256};
    use std::ffi::{CString, OsStr, OsString, c_char, c_int, c_void};
    use std::io::{Read, Seek, Write};
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::PathBuf;

    // Darwin ABI constants from `<sys/fcntl.h>` and `<sys/unistd.h>`. Keep these scoped to the
    // macOS-only module; the crate intentionally has no direct `libc` dependency. These match the
    // already-audited descriptor-relative constants in `src/fs.rs`.
    const OPEN_NOFOLLOW: i32 = 0x100;
    const OPEN_DIRECTORY: i32 = 0x10_0000;
    const OPEN_CLOEXEC: i32 = 0x100_0000;
    const OPEN_CREATE: i32 = 0x200;
    const OPEN_EXCLUSIVE: i32 = 0x800;
    const OPEN_READ_ONLY: i32 = 0;
    const OPEN_READ_WRITE: i32 = 2;
    const AT_REMOVE_DIRECTORY: i32 = 0x80;
    const LOCK_EXCLUSIVE: c_int = 2;
    const LOCK_NONBLOCKING: c_int = 4;
    const LOCK_UNLOCK: c_int = 8;
    const WOULD_BLOCK_ERRNO: c_int = 35;
    const INTERRUPTED_ERRNO: c_int = 4;
    const FILE_DESCRIPTOR_CLOEXEC: i32 = 1;
    const FCNTL_GET_DESCRIPTOR_FLAGS: i32 = 1;
    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const INVALID_ARGUMENT_ERRNO: c_int = 22;
    const NO_ENTRY_ERRNO: c_int = 2;
    const ONE_TIME_IMAGE_NAME: &str = "worker-image";
    const ONE_TIME_LEASE_NAME: &str = "lease";
    const ONE_TIME_DIRECTORY_PREFIX: &str = ".mini-agent-js-worker-";
    const DARWIN_DIRENT_NAME_CAPACITY: usize = 1_024;
    const DIRECTORY_ENTRY_LIMIT: usize = 1_024;
    #[cfg(test)]
    const CDHASH_LENGTH: usize = 20;
    // Four supported Mach-O architecture families with two digest alternatives fit in eight
    // entries. Rejecting more keeps framework output and requirement construction tightly bounded.
    #[cfg(test)]
    const MAX_CODE_IDENTITY_HASHES: usize = 8;
    // Eight exact 20-byte CDHash clauses require at most 428 ASCII bytes, including separators.
    #[cfg(test)]
    const MAX_CODE_IDENTITY_REQUIREMENT_BYTES: usize = 512;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FileIdentity {
        device: u64,
        inode: u64,
        len: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        changed_seconds: i64,
        changed_nanoseconds: i64,
        sha256: [u8; 32],
    }

    impl FileIdentity {
        fn from_metadata_and_digest(metadata: &std::fs::Metadata, sha256: [u8; 32]) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                len: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
                sha256,
            }
        }

        fn matches_metadata(&self, metadata: &std::fs::Metadata) -> bool {
            self.device == metadata.dev()
                && self.inode == metadata.ino()
                && self.len == metadata.len()
                && self.modified_seconds == metadata.mtime()
                && self.modified_nanoseconds == metadata.mtime_nsec()
                && self.changed_seconds == metadata.ctime()
                && self.changed_nanoseconds == metadata.ctime_nsec()
        }

        fn matches_unlinked_inode(&self, metadata: &std::fs::Metadata) -> bool {
            self.device == metadata.dev()
                && self.inode == metadata.ino()
                && self.len == metadata.len()
                && self.modified_seconds == metadata.mtime()
                && self.modified_nanoseconds == metadata.mtime_nsec()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    #[cfg(test)]
    struct MacCodeIdentity {
        cdhashes: Vec<Vec<u8>>,
    }

    #[cfg(test)]
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn SecCodeCopySigningInformation(
            code: SecStaticCodeRef,
            flags: SecCSFlags,
            information: *mut CFDictionaryRef,
        ) -> OSStatus;
        static kSecCodeInfoCdHashes: CFStringRef;
    }

    #[cfg(test)]
    impl MacCodeIdentity {
        #[allow(unsafe_code)]
        fn from_pinned_path(
            path: &Path,
            file: &mut std::fs::File,
            expected: &FileIdentity,
        ) -> io::Result<Self> {
            verify_code_identity_path(path, file, expected)?;

            let url = CFURL::from_path(path, false).ok_or_else(|| {
                permission_denied("worker static code path could not be represented")
            })?;
            let code = SecStaticCode::from_path(&url, Flags::NONE)
                .map_err(|_| permission_denied("worker static code object could not be created"))?;
            let mut signing_information = std::ptr::null();
            let information_status = unsafe {
                // SAFETY: `code` is a live static-code object and `signing_information` points to
                // writable storage for the create-rule dictionary returned by Security.framework.
                SecCodeCopySigningInformation(
                    code.as_concrete_TypeRef(),
                    Flags::NONE.bits(),
                    &mut signing_information,
                )
            };
            require_security_success(
                information_status,
                "worker signing identity was unavailable",
            )?;
            if signing_information.is_null() {
                return Err(permission_denied("worker signing identity was unavailable"));
            }
            let information: CFDictionary = unsafe {
                // SAFETY: successful `SecCodeCopySigningInformation` returns a retained
                // dictionary. It remains untyped until the one documented value read below.
                CFDictionary::wrap_under_create_rule(signing_information)
            };
            let cdhashes_key_ref = unsafe {
                // SAFETY: Security.framework exports `kSecCodeInfoCdHashes` as an immortal
                // CFString constant on every supported macOS version.
                kSecCodeInfoCdHashes
            };
            if cdhashes_key_ref.is_null() {
                return Err(permission_denied("worker signing identity was unavailable"));
            }
            let mut cdhashes_value = std::ptr::null();
            let cdhashes_present = unsafe {
                // SAFETY: `information` is a live CFDictionary and the checked framework key is
                // a valid CFString. The out-pointer receives a borrowed dictionary value.
                CFDictionaryGetValueIfPresent(
                    information.as_concrete_TypeRef(),
                    cdhashes_key_ref.cast(),
                    &mut cdhashes_value,
                )
            };
            if cdhashes_present == 0 || cdhashes_value.is_null() {
                return Err(permission_denied("worker signing identity was unavailable"));
            }
            let cdhashes = unsafe {
                // SAFETY: Security.framework documents this dictionary value as a CoreFoundation
                // object. The parser validates its concrete type and every element type before
                // using any type-specific accessors.
                parse_cdhash_array(cdhashes_value.cast())
            }?;

            // Signing information is only an untrusted candidate until the same static-code
            // object validates against an exact requirement derived from every returned CDHash.
            let requirement_text = exact_cdhash_requirement(&cdhashes)?;
            let requirement: SecRequirement = requirement_text.parse().map_err(|_| {
                permission_denied("worker signing identity requirement was invalid")
            })?;
            let validation_flags = Flags::CHECK_ALL_ARCHITECTURES | Flags::STRICT_VALIDATE;
            let validation_status = unsafe {
                // SAFETY: `code` and `requirement` retain valid Security.framework objects and
                // the flags request strict validation of every Mach-O architecture.
                SecStaticCodeCheckValidity(
                    code.as_concrete_TypeRef(),
                    validation_flags.bits(),
                    requirement.as_concrete_TypeRef(),
                )
            };
            require_security_success(
                validation_status,
                "worker static code signature was invalid",
            )?;

            verify_code_identity_path(path, file, expected)?;
            Ok(Self { cdhashes })
        }
    }

    #[cfg(test)]
    #[allow(unsafe_code)]
    unsafe fn parse_cdhash_array(value: CFTypeRef) -> io::Result<Vec<Vec<u8>>> {
        if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { CFArrayGetTypeID() } {
            return Err(permission_denied("worker signing identity was malformed"));
        }

        let array = value.cast() as CFArrayRef;
        let count = unsafe { CFArrayGetCount(array) };
        let count = usize::try_from(count)
            .ok()
            .filter(|count| (1..=MAX_CODE_IDENTITY_HASHES).contains(count))
            .ok_or_else(|| permission_denied("worker signing identity count was invalid"))?;

        // Validate every element's concrete type before invoking any CFData accessor. This keeps
        // a malformed heterogeneous array from reaching a typed CoreFoundation operation.
        let mut data_values = Vec::with_capacity(count);
        for index in 0..count {
            let element = unsafe { CFArrayGetValueAtIndex(array, index as isize) } as CFTypeRef;
            if element.is_null() || unsafe { CFGetTypeID(element) } != unsafe { CFDataGetTypeID() }
            {
                return Err(permission_denied("worker signing identity was malformed"));
            }
            data_values.push(element.cast() as CFDataRef);
        }

        let mut cdhashes = Vec::with_capacity(count);
        for data in data_values {
            if unsafe { CFDataGetLength(data) } != CDHASH_LENGTH as isize {
                return Err(permission_denied(
                    "worker signing identity hash length was invalid",
                ));
            }
            let bytes = unsafe { CFDataGetBytePtr(data) };
            if bytes.is_null() {
                return Err(permission_denied("worker signing identity was malformed"));
            }
            let cdhash = unsafe { std::slice::from_raw_parts(bytes, CDHASH_LENGTH) }.to_vec();
            if cdhashes.contains(&cdhash) {
                return Err(permission_denied(
                    "worker signing identity contained duplicate hashes",
                ));
            }
            cdhashes.push(cdhash);
        }
        cdhashes.sort();
        Ok(cdhashes)
    }

    #[cfg(test)]
    fn exact_cdhash_requirement(cdhashes: &[Vec<u8>]) -> io::Result<String> {
        if !(1..=MAX_CODE_IDENTITY_HASHES).contains(&cdhashes.len())
            || cdhashes.iter().any(|cdhash| cdhash.len() != CDHASH_LENGTH)
        {
            return Err(permission_denied("worker signing identity was malformed"));
        }

        let mut canonical = cdhashes.to_vec();
        canonical.sort();
        if canonical.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(permission_denied(
                "worker signing identity contained duplicate hashes",
            ));
        }
        let requirement = canonical
            .iter()
            .map(|cdhash| format!("cdhash H\"{}\"", hex_bytes(cdhash)))
            .collect::<Vec<_>>()
            .join(" or ");
        if requirement.len() > MAX_CODE_IDENTITY_REQUIREMENT_BYTES {
            return Err(permission_denied(
                "worker signing identity requirement was too large",
            ));
        }
        Ok(requirement)
    }

    #[cfg(test)]
    fn hex_bytes(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(HEX[usize::from(byte >> 4)] as char);
            encoded.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        encoded
    }

    #[cfg(test)]
    fn require_security_success(status: OSStatus, message: &'static str) -> io::Result<()> {
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(permission_denied(message))
        }
    }

    #[cfg(test)]
    fn verify_code_identity_path(
        path: &Path,
        file: &mut std::fs::File,
        expected: &FileIdentity,
    ) -> io::Result<()> {
        let descriptor_metadata = file.metadata()?;
        let path_metadata = std::fs::symlink_metadata(path)?;
        if !descriptor_metadata.is_file()
            || path_metadata.file_type().is_symlink()
            || !path_metadata.is_file()
            || !expected.matches_metadata(&descriptor_metadata)
            || !expected.matches_metadata(&path_metadata)
        {
            return Err(permission_denied(
                "worker static code path no longer matched the pinned image",
            ));
        }
        if hash_file(file)? != expected.sha256 {
            return Err(permission_denied(
                "worker static code bytes no longer matched the pinned image",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn code_identity_for_path(path: &Path) -> io::Result<MacCodeIdentity> {
        let mut file = open_read_only(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(permission_denied(
                "worker static code path was not a regular file",
            ));
        }
        let sha256 = hash_file(&mut file)?;
        let expected = FileIdentity::from_metadata_and_digest(&metadata, sha256);
        MacCodeIdentity::from_pinned_path(path, &mut file, &expected)
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct WorkerImageProof {
        source: FileIdentity,
        image: FileIdentity,
    }

    /// A byte-proven, one-link executable copy in an unguessable user-private directory.
    ///
    /// The retained directory and image descriptors are both `FD_CLOEXEC`. The image descriptor
    /// pins the proven inode only in the parent; it is closed immediately after the parent removes
    /// the sole pathname following the child's post-exec acknowledgement. Production containment
    /// remains unavailable until the complete Seatbelt matrix is validated.
    #[derive(Debug)]
    pub(super) struct OneTimeWorkerImage {
        root: std::fs::File,
        directory_name: OsString,
        #[cfg(test)]
        directory_path: PathBuf,
        image_path: PathBuf,
        directory: std::fs::File,
        lease: Option<std::fs::File>,
        image: Option<std::fs::File>,
        proof: WorkerImageProof,
        linked: bool,
        lease_linked: bool,
        directory_linked: bool,
        retired: bool,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PreparationFaultStage {
        Created,
        Copied,
        Synced,
        PermissionsSealed,
        BeforeReopen,
        Reopened,
        Hashed,
        MetadataValidated,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct InodeIdentity {
        device: u64,
        inode: u64,
    }

    impl InodeIdentity {
        fn from_metadata(metadata: &std::fs::Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }

        fn matches(self, metadata: &std::fs::Metadata) -> bool {
            self.device == metadata.dev() && self.inode == metadata.ino()
        }
    }

    /// Owns a partially published directory/image pair until every proof has succeeded.
    ///
    /// Cleanup follows descriptor-observed inode identities rather than stable pathnames. If an
    /// unowned replacement prevents complete cleanup, `cleanup` preserves it and returns an error.
    struct PartialPublicationGuard<'a> {
        root: &'a std::fs::File,
        directory: &'a std::fs::File,
        directory_identity: InodeIdentity,
        image: Option<std::fs::File>,
        image_identity: Option<InodeIdentity>,
        lease: Option<std::fs::File>,
        lease_identity: InodeIdentity,
        active: bool,
    }

    impl<'a> PartialPublicationGuard<'a> {
        fn new(
            root: &'a std::fs::File,
            directory: &'a std::fs::File,
            directory_metadata: &std::fs::Metadata,
            lease: std::fs::File,
            image: std::fs::File,
        ) -> io::Result<Self> {
            let lease_identity = InodeIdentity::from_metadata(&lease.metadata()?);
            Ok(Self {
                root,
                directory,
                directory_identity: InodeIdentity::from_metadata(directory_metadata),
                image: Some(image),
                image_identity: None,
                lease: Some(lease),
                lease_identity,
                active: true,
            })
        }

        fn bind_image_identity(&mut self) -> io::Result<()> {
            let metadata = self.image()?.metadata()?;
            self.image_identity = Some(InodeIdentity::from_metadata(&metadata));
            Ok(())
        }

        fn image(&self) -> io::Result<&std::fs::File> {
            self.image
                .as_ref()
                .ok_or_else(|| io::Error::other("partial worker image descriptor is closed"))
        }

        fn image_mut(&mut self) -> io::Result<&mut std::fs::File> {
            self.image
                .as_mut()
                .ok_or_else(|| io::Error::other("partial worker image descriptor is closed"))
        }

        fn adopt_reopened_image(&mut self, image: std::fs::File) -> io::Result<()> {
            let retained_metadata = self.image()?.metadata()?;
            let reopened_metadata = image.metadata()?;
            let expected = self.image_identity.ok_or_else(|| {
                io::Error::other("partial worker image identity was not recorded")
            })?;
            if !expected.matches(&retained_metadata) || !expected.matches(&reopened_metadata) {
                return Err(permission_denied(
                    "reopened one-time worker image did not match the created inode",
                ));
            }
            // Assignment drops the original writable descriptor only after the read-only
            // descriptor has proven it references the same still-pinned vnode.
            self.image = Some(image);
            Ok(())
        }

        fn commit(&mut self) -> io::Result<(std::fs::File, std::fs::File)> {
            let image = self
                .image
                .take()
                .ok_or_else(|| io::Error::other("partial worker image descriptor is closed"))?;
            let lease = self
                .lease
                .take()
                .ok_or_else(|| io::Error::other("publication lease descriptor is closed"))?;
            self.active = false;
            Ok((image, lease))
        }

        fn cleanup(&mut self) -> io::Result<()> {
            if !self.active {
                return Ok(());
            }
            self.active = false;

            let image_identity = match self.image_identity {
                Some(identity) => identity,
                None => InodeIdentity::from_metadata(&self.image()?.metadata()?),
            };
            let mut removed_image = false;
            for name in directory_entry_names(self.directory)? {
                let name = OsStr::from_bytes(&name);
                let Ok(candidate) = open_at(
                    self.directory,
                    name,
                    OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                    0,
                ) else {
                    continue;
                };
                if image_identity.matches(&candidate.metadata()?) {
                    unlink_at(self.directory, name, 0)?;
                    removed_image = true;
                    break;
                }
            }

            if !removed_image {
                let was_already_unlinked = self
                    .image
                    .as_ref()
                    .map(|image| image.metadata())
                    .transpose()?
                    .is_some_and(|metadata| metadata.nlink() == 0);
                if !was_already_unlinked {
                    return Err(permission_denied(
                        "could not locate the created worker image inode during cleanup",
                    ));
                }
            }
            if let Some(image) = self.image.as_ref()
                && image.metadata()?.nlink() != 0
            {
                return Err(permission_denied(
                    "created worker image retained a link after cleanup",
                ));
            }
            self.image.take();

            if let Some(lease) = self.lease.as_ref() {
                let mut removed_lease = false;
                for name in directory_entry_names(self.directory)? {
                    let name = OsStr::from_bytes(&name);
                    let Ok(candidate) = open_at(
                        self.directory,
                        name,
                        OPEN_READ_WRITE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                        0,
                    ) else {
                        continue;
                    };
                    if self.lease_identity.matches(&candidate.metadata()?) {
                        unlink_at(self.directory, name, 0)?;
                        removed_lease = true;
                        break;
                    }
                }
                if !removed_lease && lease.metadata()?.nlink() != 0 {
                    return Err(permission_denied(
                        "could not locate the publication lease inode during cleanup",
                    ));
                }
                if lease.metadata()?.nlink() != 0 {
                    return Err(permission_denied(
                        "publication lease retained a link after cleanup",
                    ));
                }
            }
            self.lease.take();

            self.directory.sync_all()?;

            if !directory_entry_names(self.directory)?.is_empty() {
                return Err(permission_denied(
                    "created worker directory contains an unowned replacement",
                ));
            }

            for name in directory_entry_names(self.root)? {
                let name = OsStr::from_bytes(&name);
                let Ok(candidate) = open_at(
                    self.root,
                    name,
                    OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                    0,
                ) else {
                    continue;
                };
                if self.directory_identity.matches(&candidate.metadata()?) {
                    unlink_at(self.root, name, AT_REMOVE_DIRECTORY)?;
                    self.root.sync_all()?;
                    return Ok(());
                }
            }

            Err(permission_denied(
                "could not locate the created worker directory during cleanup",
            ))
        }
    }

    impl Drop for PartialPublicationGuard<'_> {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    impl OneTimeWorkerImage {
        pub(super) fn prepare_from(source: &Path, private_root: &Path) -> io::Result<Self> {
            Self::prepare_from_with_fault(source, private_root, |_, _| Ok(()))
        }

        fn prepare_from_with_fault<F>(
            source: &Path,
            private_root: &Path,
            mut fault: F,
        ) -> io::Result<Self>
        where
            F: FnMut(PreparationFaultStage, &Path) -> io::Result<()>,
        {
            let supplied_root = std::fs::symlink_metadata(private_root)?;
            validate_private_directory(&supplied_root, "one-time image root")?;
            let private_root = std::fs::canonicalize(private_root)?;
            let root_before = std::fs::symlink_metadata(&private_root)?;
            validate_private_directory(&root_before, "one-time image root")?;
            ensure_same_identity(
                &supplied_root,
                &root_before,
                "one-time image root changed during resolution",
            )?;
            let root = open_directory(&private_root)?;
            let root_opened = root.metadata()?;
            ensure_same_identity(&root_before, &root_opened, "one-time image root changed")?;
            ensure_no_extended_acl(&root, "one-time image root")?;
            let root_after = std::fs::symlink_metadata(&private_root)?;
            ensure_same_identity(&root_opened, &root_after, "one-time image root changed")?;
            let _root_publication_lock = RootPublicationLock::acquire(&root)?;

            let source_before = std::fs::symlink_metadata(source)?;
            validate_source_image(&source_before)?;
            let mut source_file = open_read_only(source)?;
            let source_opened = source_file.metadata()?;
            validate_source_image(&source_opened)?;
            ensure_stable_source_metadata(
                &source_before,
                &source_opened,
                "worker source changed before copy",
            )?;
            ensure_no_extended_acl(&source_file, "worker source")?;

            let (directory_name, directory_path) =
                create_unique_private_directory(&root, &private_root)?;
            root.sync_all()?;
            let directory_before = std::fs::symlink_metadata(&directory_path)?;
            validate_private_directory(&directory_before, "one-time image directory")?;
            let directory = open_directory_at(&root, &directory_name)?;
            let directory_opened = directory.metadata()?;
            ensure_same_identity(
                &directory_before,
                &directory_opened,
                "one-time image directory changed",
            )?;
            ensure_no_extended_acl(&directory, "one-time image directory")?;

            let lease = open_at(
                &directory,
                OsStr::new(ONE_TIME_LEASE_NAME),
                OPEN_READ_WRITE | OPEN_CREATE | OPEN_EXCLUSIVE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                0o600,
            )?;
            validate_publication_lease(&lease)?;
            lock_exclusive_nonblocking(&lease)?;
            directory.sync_all()?;

            let image_path = directory_path.join(ONE_TIME_IMAGE_NAME);
            let image_writer = open_at(
                &directory,
                OsStr::new(ONE_TIME_IMAGE_NAME),
                OPEN_READ_WRITE | OPEN_CREATE | OPEN_EXCLUSIVE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                0o600,
            )?;
            // Ownership begins immediately after the exclusive create succeeds. Every subsequent
            // fallible stage is routed through an explicit cleanup result before returning.
            let mut publication = PartialPublicationGuard::new(
                &root,
                &directory,
                &directory_opened,
                lease,
                image_writer,
            )?;
            let result = (|| {
                publication.bind_image_identity()?;
                fault(PreparationFaultStage::Created, &image_path)?;
                ensure_no_extended_acl(publication.image()?, "writable one-time worker image")?;
                let source_sha256 = copy_and_hash(&mut source_file, publication.image_mut()?)?;
                fault(PreparationFaultStage::Copied, &image_path)?;
                publication.image_mut()?.flush()?;
                publication.image()?.sync_all()?;
                fault(PreparationFaultStage::Synced, &image_path)?;
                publication
                    .image()?
                    .set_permissions(std::fs::Permissions::from_mode(0o500))?;
                publication.image()?.sync_all()?;
                fault(PreparationFaultStage::PermissionsSealed, &image_path)?;

                let source_after_handle = source_file.metadata()?;
                validate_source_image(&source_after_handle)?;
                ensure_stable_source_metadata(
                    &source_opened,
                    &source_after_handle,
                    "worker source mutated during copy",
                )?;
                ensure_no_extended_acl(&source_file, "worker source")?;
                let source_after_path = std::fs::symlink_metadata(source)?;
                validate_source_image(&source_after_path)?;
                ensure_stable_source_metadata(
                    &source_opened,
                    &source_after_path,
                    "worker source path changed during copy",
                )?;

                fault(PreparationFaultStage::BeforeReopen, &image_path)?;
                let image = open_at(
                    &directory,
                    OsStr::new(ONE_TIME_IMAGE_NAME),
                    OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                    0,
                )?;
                publication.adopt_reopened_image(image)?;
                fault(PreparationFaultStage::Reopened, &image_path)?;
                let image_opened = publication.image()?.metadata()?;
                validate_published_image(&image_opened)?;
                ensure_no_extended_acl(publication.image()?, "sealed one-time worker image")?;
                let image_sha256 = hash_file(publication.image_mut()?)?;
                fault(PreparationFaultStage::Hashed, &image_path)?;
                if source_sha256 != image_sha256 || source_opened.len() != image_opened.len() {
                    return Err(permission_denied(
                        "one-time worker image did not match its source",
                    ));
                }
                let image_after = std::fs::symlink_metadata(&image_path)?;
                ensure_stable_metadata(
                    &image_opened,
                    &image_after,
                    "one-time worker image changed after publication",
                )?;
                ensure_exact_directory_entries(
                    &directory,
                    &[
                        OsStr::new(ONE_TIME_LEASE_NAME),
                        OsStr::new(ONE_TIME_IMAGE_NAME),
                    ],
                )?;
                directory.sync_all()?;
                fault(PreparationFaultStage::MetadataValidated, &image_path)?;

                let root = root.try_clone()?;
                let directory = directory.try_clone()?;
                let proof = WorkerImageProof {
                    source: FileIdentity::from_metadata_and_digest(&source_opened, source_sha256),
                    image: FileIdentity::from_metadata_and_digest(&image_opened, image_sha256),
                };
                let (image, lease) = publication.commit()?;

                Ok(Self {
                    root,
                    directory_name: directory_name.clone(),
                    #[cfg(test)]
                    directory_path: directory_path.clone(),
                    image_path: image_path.clone(),
                    directory,
                    lease: Some(lease),
                    image: Some(image),
                    proof,
                    linked: true,
                    lease_linked: true,
                    directory_linked: true,
                    retired: false,
                })
            })();

            match result {
                Ok(image) => Ok(image),
                Err(error) => match publication.cleanup() {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(io::Error::new(
                        cleanup_error.kind(),
                        format!(
                            "one-time worker image preparation failed: {error}; cleanup failed: {cleanup_error}"
                        ),
                    )),
                },
            }
        }

        pub(super) fn image_path(&self) -> &Path {
            &self.image_path
        }

        pub(super) fn directory_name(&self) -> &OsStr {
            &self.directory_name
        }

        #[cfg(test)]
        fn directory_path(&self) -> &Path {
            &self.directory_path
        }

        #[cfg(test)]
        fn proof(&self) -> &WorkerImageProof {
            &self.proof
        }

        #[cfg(test)]
        fn image_descriptor_has_close_on_exec(&self) -> io::Result<bool> {
            let image = self
                .image
                .as_ref()
                .ok_or_else(|| io::Error::other("one-time worker image descriptor is closed"))?;
            descriptor_has_close_on_exec(image.as_raw_fd())
        }

        #[cfg(test)]
        fn directory_descriptor_has_close_on_exec(&self) -> io::Result<bool> {
            descriptor_has_close_on_exec(self.directory.as_raw_fd())
        }

        #[cfg(test)]
        fn root_descriptor_has_close_on_exec(&self) -> io::Result<bool> {
            descriptor_has_close_on_exec(self.root.as_raw_fd())
        }

        /// Remove the sole executable pathname after the child has acknowledged successful exec.
        pub(super) fn unlink_after_exec(&mut self) -> io::Result<()> {
            if !self.linked {
                return Err(io::Error::other(
                    "one-time worker image is already unlinked",
                ));
            }
            let image = self
                .image
                .as_mut()
                .ok_or_else(|| io::Error::other("one-time worker image descriptor is closed"))?;
            let opened = image.metadata()?;
            if !self.proof.image.matches_metadata(&opened) || opened.nlink() != 1 {
                return Err(permission_denied(
                    "one-time worker image inode changed before unlink",
                ));
            }
            ensure_no_extended_acl(&self.directory, "one-time image directory")?;
            ensure_no_extended_acl(image, "sealed one-time worker image")?;
            ensure_exact_directory_entries(
                &self.directory,
                &[
                    OsStr::new(ONE_TIME_LEASE_NAME),
                    OsStr::new(ONE_TIME_IMAGE_NAME),
                ],
            )?;
            let path_file = open_at(
                &self.directory,
                OsStr::new(ONE_TIME_IMAGE_NAME),
                OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                0,
            )?;
            let path_metadata = path_file.metadata()?;
            if !self.proof.image.matches_metadata(&path_metadata) || path_metadata.nlink() != 1 {
                return Err(permission_denied(
                    "one-time worker image path was replaced before unlink",
                ));
            }
            if hash_file(image)? != self.proof.image.sha256 {
                return Err(permission_denied(
                    "one-time worker image bytes changed before unlink",
                ));
            }
            drop(path_file);

            unlink_at(&self.directory, OsStr::new(ONE_TIME_IMAGE_NAME), 0)?;
            match open_at(
                &self.directory,
                OsStr::new(ONE_TIME_IMAGE_NAME),
                OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                0,
            ) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => return Err(permission_denied("one-time worker image remained linked")),
                Err(error) => return Err(error),
            }
            ensure_exact_directory_entries(&self.directory, &[OsStr::new(ONE_TIME_LEASE_NAME)])?;
            let unlinked = image.metadata()?;
            // Removing the last name legitimately changes ctime, so post-unlink identity excludes it.
            if !self.proof.image.matches_unlinked_inode(&unlinked) || unlinked.nlink() != 0 {
                return Err(permission_denied(
                    "one-time worker image retained an executable link after unlink",
                ));
            }
            self.directory.sync_all()?;
            self.image.take();
            self.linked = false;
            Ok(())
        }

        /// Explicitly retire the lease and publication directory after the exact child was reaped.
        /// Every unlink and durability failure is returned to the lifecycle owner.
        pub(super) fn retire_after_reap(&mut self) -> io::Result<()> {
            if self.retired {
                return Ok(());
            }
            if self.linked || self.image.is_some() {
                return Err(io::Error::other(
                    "one-time worker image remained executable at retirement",
                ));
            }

            if self.lease_linked {
                let lease = self.lease.as_ref().ok_or_else(|| {
                    io::Error::other("one-time worker publication lease is closed")
                })?;
                validate_publication_lease(lease)?;
                ensure_exact_directory_entries(
                    &self.directory,
                    &[OsStr::new(ONE_TIME_LEASE_NAME)],
                )?;
                let path_lease = open_at(
                    &self.directory,
                    OsStr::new(ONE_TIME_LEASE_NAME),
                    OPEN_READ_WRITE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                    0,
                )?;
                let retained = lease.metadata()?;
                let path = path_lease.metadata()?;
                if retained.dev() != path.dev() || retained.ino() != path.ino() {
                    return Err(permission_denied(
                        "one-time worker publication lease was replaced",
                    ));
                }
                drop(path_lease);
                unlink_at(&self.directory, OsStr::new(ONE_TIME_LEASE_NAME), 0)?;
                self.lease_linked = false;
            }
            self.directory.sync_all()?;
            if let Some(lease) = self.lease.as_ref()
                && lease.metadata()?.nlink() != 0
            {
                return Err(permission_denied(
                    "one-time worker publication lease retained a link",
                ));
            }
            self.lease.take();

            if self.directory_linked {
                ensure_exact_directory_entries(&self.directory, &[])?;
                let path_directory = open_directory_at(&self.root, &self.directory_name)?;
                let retained = self.directory.metadata()?;
                let path = path_directory.metadata()?;
                if retained.dev() != path.dev() || retained.ino() != path.ino() {
                    return Err(permission_denied(
                        "one-time worker publication directory was replaced",
                    ));
                }
                drop(path_directory);
                unlink_at(&self.root, &self.directory_name, AT_REMOVE_DIRECTORY)?;
                self.directory_linked = false;
            }
            self.root.sync_all()?;
            self.retired = true;
            Ok(())
        }

        fn cleanup(&mut self) {
            if self.linked {
                let can_unlink = open_at(
                    &self.directory,
                    OsStr::new(ONE_TIME_IMAGE_NAME),
                    OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                    0,
                )
                .and_then(|file| file.metadata())
                .map(|metadata| {
                    self.proof.image.matches_metadata(&metadata) && metadata.nlink() == 1
                })
                .unwrap_or(false);
                if can_unlink
                    && unlink_at(&self.directory, OsStr::new(ONE_TIME_IMAGE_NAME), 0).is_ok()
                {
                    self.linked = false;
                }
            }
            self.image.take();
            if let Some(lease) = self.lease.as_ref() {
                let path_metadata = open_at(
                    &self.directory,
                    OsStr::new(ONE_TIME_LEASE_NAME),
                    OPEN_READ_WRITE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                    0,
                )
                .and_then(|file| file.metadata());
                let retained_metadata = lease.metadata();
                let can_unlink = match (path_metadata, retained_metadata) {
                    (Ok(path), Ok(retained)) => {
                        path.dev() == retained.dev() && path.ino() == retained.ino()
                    }
                    _ => false,
                };
                if can_unlink
                    && unlink_at(&self.directory, OsStr::new(ONE_TIME_LEASE_NAME), 0).is_ok()
                {
                    self.lease_linked = false;
                }
            }
            self.lease.take();
            let _ = self.directory.sync_all();
            if unlink_at(&self.root, &self.directory_name, AT_REMOVE_DIRECTORY).is_ok() {
                self.directory_linked = false;
            }
            let _ = self.root.sync_all();
        }
    }

    impl Drop for OneTimeWorkerImage {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "one-time worker path component contains NUL",
            )
        })
    }

    #[allow(unsafe_code)]
    fn mkdir_at(directory: &std::fs::File, name: &OsStr, mode: u16) -> io::Result<()> {
        unsafe extern "C" {
            fn mkdirat(directory: c_int, path: *const c_char, mode: u16) -> c_int;
        }
        let name = c_name(name)?;
        // SAFETY: `directory` owns a valid descriptor and `name` is NUL-terminated for this call.
        if unsafe { mkdirat(directory.as_raw_fd(), name.as_ptr(), mode) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    type DarwinOpenAt = unsafe extern "C" fn(c_int, *const c_char, c_int, ...) -> c_int;

    #[allow(clashing_extern_declarations, unsafe_code)]
    unsafe extern "C" {
        #[link_name = "openat"]
        fn one_time_openat(directory: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
    }

    const _: DarwinOpenAt = one_time_openat;

    #[allow(unsafe_code)]
    fn open_at(
        directory: &std::fs::File,
        name: &OsStr,
        flags: i32,
        mode: u16,
    ) -> io::Result<std::fs::File> {
        let name = c_name(name)?;
        // SAFETY: `directory` and `name` remain valid for the call. A successful descriptor is new
        // ownership and is transferred exactly once to `File`. Darwin declares `openat` variadic;
        // `mode_t` is `u16`, but C default argument promotion requires passing it as `c_int`.
        let descriptor = unsafe {
            one_time_openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags,
                c_int::from(mode),
            )
        };
        if descriptor < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: `openat` returned a new owned descriptor.
            Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
        }
    }

    struct RootPublicationLock<'a> {
        root: &'a std::fs::File,
    }

    impl<'a> RootPublicationLock<'a> {
        fn acquire(root: &'a std::fs::File) -> io::Result<Self> {
            lock_exclusive_nonblocking(root)?;
            Ok(Self { root })
        }
    }

    impl Drop for RootPublicationLock<'_> {
        fn drop(&mut self) {
            let _ = unlock_file(self.root);
        }
    }

    #[allow(unsafe_code)]
    fn lock_exclusive_nonblocking(file: &std::fs::File) -> io::Result<()> {
        unsafe extern "C" {
            fn flock(descriptor: c_int, operation: c_int) -> c_int;
        }
        for _ in 0..8 {
            // SAFETY: `file` owns a live descriptor for the duration of the synchronous call.
            if unsafe { flock(file.as_raw_fd(), LOCK_EXCLUSIVE | LOCK_NONBLOCKING) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(INTERRUPTED_ERRNO) => continue,
                Some(WOULD_BLOCK_ERRNO) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "one-time worker publication lock is busy",
                    ));
                }
                _ => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "one-time worker publication lock was repeatedly interrupted",
        ))
    }

    #[allow(unsafe_code)]
    fn unlock_file(file: &std::fs::File) -> io::Result<()> {
        unsafe extern "C" {
            fn flock(descriptor: c_int, operation: c_int) -> c_int;
        }
        // SAFETY: `file` owns a live descriptor for the duration of the synchronous call.
        if unsafe { flock(file.as_raw_fd(), LOCK_UNLOCK) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[allow(unsafe_code)]
    fn unlink_at(directory: &std::fs::File, name: &OsStr, flags: i32) -> io::Result<()> {
        unsafe extern "C" {
            fn unlinkat(directory: i32, path: *const std::os::raw::c_char, flags: i32) -> i32;
        }
        let name = c_name(name)?;
        // SAFETY: `directory` owns a valid descriptor and `name` is NUL-terminated for this call.
        if unsafe { unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[repr(C)]
    struct DarwinDirectoryStream {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct DarwinDirectoryEntry {
        inode: u64,
        seek_offset: u64,
        record_length: u16,
        name_length: u16,
        file_type: u8,
        name: [c_char; DARWIN_DIRENT_NAME_CAPACITY],
    }

    fn ensure_exact_directory_entries(
        directory: &std::fs::File,
        expected: &[&OsStr],
    ) -> io::Result<()> {
        let mut entries = directory_entry_names(directory)?;
        let mut expected = expected
            .iter()
            .map(|name| name.as_bytes().to_vec())
            .collect::<Vec<_>>();
        entries.sort();
        expected.sort();
        if entries == expected {
            Ok(())
        } else {
            Err(permission_denied(
                "one-time worker directory entries did not match the publication protocol",
            ))
        }
    }

    /// Enumerate from a new open file description rooted at `directory`, never through its pathname.
    // This local declaration names the same Darwin ABI pointer layout used by the repository's other
    // descriptor-bound directory reader. Rust treats the two private opaque pointer types as nominally
    // different even though their C ABI is identical.
    #[allow(unsafe_code, clashing_extern_declarations)]
    fn directory_entry_names(directory: &std::fs::File) -> io::Result<Vec<Vec<u8>>> {
        unsafe extern "C" {
            #[cfg_attr(target_arch = "x86_64", link_name = "fdopendir$INODE64")]
            #[cfg_attr(target_arch = "aarch64", link_name = "fdopendir")]
            fn one_time_fdopendir(descriptor: c_int) -> *mut DarwinDirectoryStream;
            #[cfg_attr(target_arch = "x86_64", link_name = "readdir$INODE64")]
            #[cfg_attr(target_arch = "aarch64", link_name = "readdir")]
            fn one_time_readdir(directory: *mut DarwinDirectoryStream)
            -> *mut DarwinDirectoryEntry;
            #[link_name = "closedir"]
            fn one_time_closedir(directory: *mut DarwinDirectoryStream) -> c_int;
            fn __error() -> *mut c_int;
        }

        let directory_copy = open_at(
            directory,
            OsStr::new("."),
            OPEN_READ_ONLY | OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )?;
        let descriptor = directory_copy.into_raw_fd();
        // SAFETY: `descriptor` is a new owned directory descriptor. On success `fdopendir` assumes
        // ownership; on failure it remains ours and is reconstructed below exactly once.
        let stream = unsafe { one_time_fdopendir(descriptor) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed and therefore did not consume the valid owned descriptor.
            drop(unsafe { std::fs::File::from_raw_fd(descriptor) });
            return Err(error);
        }

        let result = (|| {
            let mut names = Vec::new();
            loop {
                // SAFETY: `__error` returns the calling thread's live errno storage.
                unsafe { *__error() = 0 };
                // SAFETY: `stream` remains live until `closedir` below.
                let entry = unsafe { one_time_readdir(stream) };
                if entry.is_null() {
                    // SAFETY: the thread-local errno pointer remains valid for this read.
                    let error_number = unsafe { *__error() };
                    if error_number == 0 {
                        break;
                    }
                    return Err(io::Error::from_raw_os_error(error_number));
                }

                // SAFETY: `readdir` returned a live Darwin `dirent`; `d_namlen` cannot exceed the
                // fixed `d_name` capacity declared by the Darwin ABI.
                let name_length = unsafe { (*entry).name_length as usize };
                if name_length >= DARWIN_DIRENT_NAME_CAPACITY {
                    return Err(permission_denied(
                        "one-time worker directory returned an invalid entry name",
                    ));
                }
                // SAFETY: the validated length is within the live entry's fixed name array.
                let name = unsafe {
                    std::slice::from_raw_parts((*entry).name.as_ptr().cast::<u8>(), name_length)
                };
                if name != b"." && name != b".." {
                    if names.len() == DIRECTORY_ENTRY_LIMIT {
                        return Err(permission_denied(
                            "one-time worker directory exceeded the bounded entry limit",
                        ));
                    }
                    names.push(name.to_vec());
                }
            }
            Ok(names)
        })();

        // SAFETY: `stream` is live and owned here; this closes the descriptor consumed by
        // `fdopendir` exactly once.
        let close_result = unsafe { one_time_closedir(stream) };
        match result {
            Err(error) => Err(error),
            Ok(_) if close_result != 0 => Err(io::Error::last_os_error()),
            Ok(names) => Ok(names),
        }
    }

    fn create_unique_private_directory(
        root: &std::fs::File,
        root_path: &Path,
    ) -> io::Result<(OsString, PathBuf)> {
        for _ in 0..16 {
            let name = OsString::from(format!(
                "{ONE_TIME_DIRECTORY_PREFIX}{}",
                uuid::Uuid::new_v4()
            ));
            match mkdir_at(root, &name, 0o700) {
                Ok(()) => return Ok((name.clone(), root_path.join(name))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique one-time worker directory",
        ))
    }

    fn open_directory(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC)
            .open(path)
    }

    fn open_directory_at(directory: &std::fs::File, name: &OsStr) -> io::Result<std::fs::File> {
        open_at(
            directory,
            name,
            OPEN_READ_ONLY | OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )
    }

    fn open_read_only(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_NOFOLLOW | OPEN_CLOEXEC)
            .open(path)
    }

    fn validate_private_directory(metadata: &std::fs::Metadata, label: &str) -> io::Result<()> {
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != current_uid()
            || metadata.mode() & 0o077 != 0
        {
            return Err(permission_denied(label));
        }
        Ok(())
    }

    fn validate_source_image(metadata: &std::fs::Metadata) -> io::Result<()> {
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || !source_owner_is_trusted(metadata.uid(), current_uid())
            || metadata.nlink() != 1
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
        {
            return Err(permission_denied(
                "worker source is not a stable private executable",
            ));
        }
        Ok(())
    }

    fn source_owner_is_trusted(owner: u32, user: u32) -> bool {
        owner == user || owner == 0
    }

    fn validate_published_image(metadata: &std::fs::Metadata) -> io::Result<()> {
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != current_uid()
            || metadata.nlink() != 1
            || metadata.mode() & 0o777 != 0o500
        {
            return Err(permission_denied(
                "published worker image is not a private one-link executable",
            ));
        }
        Ok(())
    }

    fn validate_publication_lease(file: &std::fs::File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != current_uid()
            || metadata.nlink() != 1
            || metadata.mode() & 0o7777 != 0o600
            || !descriptor_has_close_on_exec(file.as_raw_fd())?
        {
            return Err(permission_denied(
                "publication lease is not a private one-link close-on-exec file",
            ));
        }
        ensure_no_extended_acl(file, "publication lease")
    }

    #[repr(C)]
    struct DarwinAcl {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct DarwinAclEntry {
        _private: [u8; 0],
    }

    fn extended_acl_present_from_first_entry(result: c_int, errno: c_int) -> io::Result<bool> {
        match (result, errno) {
            (0, _) => Ok(true),
            (-1, INVALID_ARGUMENT_ERRNO) => Ok(false),
            _ => Err(io::Error::from_raw_os_error(errno)),
        }
    }

    #[allow(unsafe_code)]
    fn ensure_no_extended_acl(file: &std::fs::File, label: &str) -> io::Result<()> {
        unsafe extern "C" {
            fn acl_get_fd_np(descriptor: c_int, acl_type: c_int) -> *mut DarwinAcl;
            fn acl_get_entry(
                acl: *mut DarwinAcl,
                entry_id: c_int,
                entry: *mut *mut DarwinAclEntry,
            ) -> c_int;
            fn acl_free(object: *mut c_void) -> c_int;
            fn __error() -> *mut c_int;
        }

        // SAFETY: `file` owns a live descriptor and the requested ACL type is the Darwin extended ACL.
        let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(NO_ENTRY_ERRNO) {
                Ok(())
            } else {
                Err(error)
            };
        }

        let mut entry = std::ptr::null_mut();
        // SAFETY: `__error` returns this thread's live errno storage; `acl` remains owned until freed.
        unsafe { *__error() = 0 };
        // SAFETY: `acl` is live and `entry` points to valid writable storage for the borrowed result.
        let result = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
        // SAFETY: the thread-local errno pointer remains valid for this read.
        let errno = unsafe { *__error() };
        // SAFETY: `acl_get_fd_np` returned this allocation, which is released exactly once here.
        let free_result = unsafe { acl_free(acl.cast::<c_void>()) };
        if free_result != 0 {
            return Err(io::Error::last_os_error());
        }
        if extended_acl_present_from_first_entry(result, errno)? {
            Err(permission_denied(format!(
                "{label} has extended ACL entries"
            )))
        } else {
            Ok(())
        }
    }

    fn ensure_same_identity(
        expected: &std::fs::Metadata,
        actual: &std::fs::Metadata,
        message: &'static str,
    ) -> io::Result<()> {
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(permission_denied(message));
        }
        Ok(())
    }

    fn ensure_stable_metadata(
        expected: &std::fs::Metadata,
        actual: &std::fs::Metadata,
        message: &'static str,
    ) -> io::Result<()> {
        ensure_same_identity(expected, actual, message)?;
        if expected.len() != actual.len()
            || expected.mtime() != actual.mtime()
            || expected.mtime_nsec() != actual.mtime_nsec()
            || expected.ctime() != actual.ctime()
            || expected.ctime_nsec() != actual.ctime_nsec()
        {
            return Err(permission_denied(message));
        }
        Ok(())
    }

    fn ensure_stable_source_metadata(
        expected: &std::fs::Metadata,
        actual: &std::fs::Metadata,
        message: &'static str,
    ) -> io::Result<()> {
        ensure_stable_metadata(expected, actual, message)?;
        if expected.uid() != actual.uid()
            || expected.gid() != actual.gid()
            || expected.mode() != actual.mode()
            || expected.nlink() != actual.nlink()
        {
            return Err(permission_denied(message));
        }
        Ok(())
    }

    fn copy_and_hash(
        source: &mut std::fs::File,
        target: &mut std::fs::File,
    ) -> io::Result<[u8; 32]> {
        source.seek(std::io::SeekFrom::Start(0))?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
            target.write_all(&buffer[..read])?;
        }
        Ok(digest.finalize().into())
    }

    fn hash_file(file: &mut std::fs::File) -> io::Result<[u8; 32]> {
        file.seek(std::io::SeekFrom::Start(0))?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(digest.finalize().into())
    }

    fn permission_denied(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message.into())
    }

    #[allow(unsafe_code)]
    fn current_uid() -> u32 {
        unsafe extern "C" {
            fn getuid() -> u32;
        }
        // SAFETY: `getuid` takes no arguments and has no failure mode.
        unsafe { getuid() }
    }

    #[allow(unsafe_code)]
    fn descriptor_has_close_on_exec(descriptor: RawFd) -> io::Result<bool> {
        unsafe extern "C" {
            fn fcntl(descriptor: i32, command: i32, ...) -> i32;
        }
        // SAFETY: `descriptor` is owned by a live `File`; `F_GETFD` takes no variadic argument.
        let flags = unsafe { fcntl(descriptor, FCNTL_GET_DESCRIPTOR_FLAGS) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags & FILE_DESCRIPTOR_CLOEXEC != 0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

        struct TestRoot {
            path: PathBuf,
        }

        impl TestRoot {
            fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "mini-agent-a25-one-time-image-test-{}",
                    uuid::Uuid::new_v4()
                ));
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700).create(&path).unwrap();
                Self { path }
            }

            fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
                let path = self.path.join(name);
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o700)
                    .open(&path)
                    .unwrap();
                file.write_all(bytes).unwrap();
                file.sync_all().unwrap();
                path
            }
        }

        impl Drop for TestRoot {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }

        fn cdhash_array(hashes: &[Vec<u8>]) -> CFArray<CFData> {
            let values = hashes
                .iter()
                .map(|hash| CFData::from_buffer(hash))
                .collect::<Vec<_>>();
            CFArray::from_CFTypes(&values)
        }

        #[allow(unsafe_code)]
        fn parse_test_cdhash_value<T: TCFType>(value: &T) -> io::Result<Vec<Vec<u8>>> {
            // SAFETY: every test passes a live CoreFoundation object for the duration of the call.
            unsafe { parse_cdhash_array(value.as_CFTypeRef()) }
        }

        #[test]
        fn code_identity_cdhash_parser_rejects_wrong_outer_type() {
            let wrong_type = CFString::new("not an array");

            let error = parse_test_cdhash_value(&wrong_type).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn code_identity_cdhash_parser_rejects_wrong_element_type() {
            let value = CFString::new("not data");
            let wrong_elements = CFArray::from_CFTypes(&[value]);

            let error = parse_test_cdhash_value(&wrong_elements).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn code_identity_cdhash_parser_rejects_empty_short_and_long_hashes() {
            for hashes in [vec![], vec![vec![0x11; 19]], vec![vec![0x22; 21]]] {
                let array = cdhash_array(&hashes);
                let error = parse_test_cdhash_value(&array).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
        }

        #[test]
        fn code_identity_cdhash_parser_rejects_excessive_count() {
            let hashes = (0..=MAX_CODE_IDENTITY_HASHES)
                .map(|index| vec![index as u8; CDHASH_LENGTH])
                .collect::<Vec<_>>();
            let array = cdhash_array(&hashes);

            let error = parse_test_cdhash_value(&array).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn code_identity_cdhash_parser_rejects_duplicates() {
            let duplicate = vec![0x33; CDHASH_LENGTH];
            let array = cdhash_array(&[duplicate.clone(), duplicate]);

            let error = parse_test_cdhash_value(&array).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn code_identity_cdhash_requirement_accepts_bounded_multi_hash_or() {
            let higher = vec![0xab; CDHASH_LENGTH];
            let lower = vec![0x01; CDHASH_LENGTH];
            let array = cdhash_array(&[higher, lower]);
            let parsed = parse_test_cdhash_value(&array).unwrap();

            let requirement = exact_cdhash_requirement(&parsed).unwrap();

            assert_eq!(
                requirement,
                format!(
                    "cdhash H\"{}\" or cdhash H\"{}\"",
                    "01".repeat(CDHASH_LENGTH),
                    "ab".repeat(CDHASH_LENGTH)
                )
            );
            assert!(requirement.len() <= MAX_CODE_IDENTITY_REQUIREMENT_BYTES);
        }

        #[test]
        #[ignore = "requires a host whose Security.framework trusts sealed system binaries"]
        fn code_identity_matches_a_signed_system_source_and_exact_one_time_copy() {
            let root = TestRoot::new();
            let source = Path::new("/bin/ls");
            let image = OneTimeWorkerImage::prepare_from(source, &root.path).unwrap();

            let source_identity = code_identity_for_path(source).unwrap();
            let image_identity = code_identity_for_path(image.image_path()).unwrap();

            assert_eq!(source_identity, image_identity);
        }

        #[test]
        fn code_identity_rejects_a_tampered_signed_copy() {
            let root = TestRoot::new();
            let source = Path::new("/bin/ls");
            let image = OneTimeWorkerImage::prepare_from(source, &root.path).unwrap();
            let image_path = image.image_path();
            let mut bytes = std::fs::read(image_path).unwrap();
            let changed = bytes.len().min(4_096).saturating_sub(1);
            bytes[changed] ^= 1;
            std::fs::set_permissions(image_path, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(image_path, bytes).unwrap();
            std::fs::set_permissions(image_path, std::fs::Permissions::from_mode(0o500)).unwrap();

            let error = code_identity_for_path(image_path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn code_identity_rejects_an_unsigned_or_malformed_executable() {
            let root = TestRoot::new();
            let malformed = root.source("unsigned-worker", b"not a signed Mach-O executable");

            let error = code_identity_for_path(&malformed).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        #[ignore = "requires a host whose Security.framework trusts sealed system binaries"]
        fn code_identity_distinguishes_different_signed_system_executables() {
            let first = code_identity_for_path(Path::new("/bin/ls")).unwrap();
            let second = code_identity_for_path(Path::new("/bin/sleep")).unwrap();

            assert_ne!(first, second);
        }

        #[test]
        fn one_time_worker_image_is_private_unique_exact_and_close_on_exec() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"exact worker image bytes");
            let first = OneTimeWorkerImage::prepare_from(&source, &root.path).unwrap();
            let second = OneTimeWorkerImage::prepare_from(&source, &root.path).unwrap();

            assert_ne!(first.image_path(), second.image_path());
            assert_eq!(first.proof().source.sha256, first.proof().image.sha256);
            assert_eq!(first.proof().source.len, first.proof().image.len);
            assert_ne!(first.proof().source.inode, first.proof().image.inode);
            assert!(first.image_descriptor_has_close_on_exec().unwrap());
            assert!(first.directory_descriptor_has_close_on_exec().unwrap());
            assert!(first.root_descriptor_has_close_on_exec().unwrap());
            assert!(
                descriptor_has_close_on_exec(first.lease.as_ref().unwrap().as_raw_fd()).unwrap()
            );
            let mut entries = directory_entry_names(&first.directory).unwrap();
            entries.sort();
            assert_eq!(
                entries,
                vec![
                    ONE_TIME_LEASE_NAME.as_bytes().to_vec(),
                    ONE_TIME_IMAGE_NAME.as_bytes().to_vec(),
                ]
            );
            let lease = first.lease.as_ref().unwrap().metadata().unwrap();
            assert_eq!(lease.mode() & 0o7777, 0o600);
            assert_eq!(lease.nlink(), 1);
            assert_eq!(
                std::fs::read(first.image_path()).unwrap(),
                b"exact worker image bytes"
            );

            let directory = std::fs::symlink_metadata(first.directory_path()).unwrap();
            assert!(directory.is_dir());
            assert_eq!(directory.mode() & 0o777, 0o700);
            assert_eq!(directory.uid(), std::fs::metadata(&source).unwrap().uid());
            let image = std::fs::symlink_metadata(first.image_path()).unwrap();
            assert!(image.is_file());
            assert_eq!(image.nlink(), 1);
            assert_eq!(image.mode() & 0o777, 0o500);
        }

        #[test]
        fn one_time_worker_image_rejects_symlink_and_hard_link_sources() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let symlink = root.path.join("source-symlink");
            std::os::unix::fs::symlink(&source, &symlink).unwrap();
            assert!(OneTimeWorkerImage::prepare_from(&symlink, &root.path).is_err());

            let hard_link = root.path.join("source-hard-link");
            std::fs::hard_link(&source, &hard_link).unwrap();
            assert!(OneTimeWorkerImage::prepare_from(&source, &root.path).is_err());
        }

        #[test]
        fn one_time_worker_image_rejects_a_symlink_root() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let root_link = root.path.with_extension("symlink");
            std::os::unix::fs::symlink(&root.path, &root_link).unwrap();

            assert!(OneTimeWorkerImage::prepare_from(&source, &root_link).is_err());
            std::fs::remove_file(root_link).unwrap();
        }

        #[test]
        fn one_time_worker_image_accepts_only_user_or_root_owned_sources() {
            let user = current_uid();
            assert!(source_owner_is_trusted(user, user));
            assert!(source_owner_is_trusted(0, user));
            let unrelated = if user == 1 { 2 } else { 1 };
            assert!(!source_owner_is_trusted(unrelated, user));
        }

        #[test]
        fn darwin_openat_contract_is_variadic_and_promotes_mode() {
            let _abi: DarwinOpenAt = one_time_openat;
            let root = TestRoot::new();
            let directory = open_directory(&root.path).unwrap();
            let name = OsStr::new("openat-mode-contract");
            let file = open_at(
                &directory,
                name,
                OPEN_READ_WRITE | OPEN_CREATE | OPEN_EXCLUSIVE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                0o600,
            )
            .unwrap();
            assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
            drop(file);
            unlink_at(&directory, name, 0).unwrap();
        }

        #[test]
        fn source_metadata_contract_rejects_permission_and_link_changes() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let original = std::fs::metadata(&source).unwrap();

            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o500)).unwrap();
            let changed_mode = std::fs::metadata(&source).unwrap();
            assert!(ensure_stable_source_metadata(&original, &changed_mode, "changed").is_err());

            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::hard_link(&source, root.path.join("source-hard-link")).unwrap();
            let changed_links = std::fs::metadata(&source).unwrap();
            assert!(ensure_stable_source_metadata(&original, &changed_links, "changed").is_err());
        }

        #[test]
        fn extended_acl_result_contract_rejects_present_entries() {
            assert!(extended_acl_present_from_first_entry(0, 0).unwrap());
            assert!(!extended_acl_present_from_first_entry(-1, INVALID_ARGUMENT_ERRNO).unwrap());
            assert!(extended_acl_present_from_first_entry(-1, 5).is_err());
        }

        #[test]
        fn partial_publication_faults_clean_owned_image_and_directory() {
            for injected_stage in [
                PreparationFaultStage::Created,
                PreparationFaultStage::Copied,
                PreparationFaultStage::Synced,
                PreparationFaultStage::PermissionsSealed,
                PreparationFaultStage::BeforeReopen,
                PreparationFaultStage::Reopened,
                PreparationFaultStage::Hashed,
                PreparationFaultStage::MetadataValidated,
            ] {
                let root = TestRoot::new();
                let source = root.source("source-worker", b"worker");
                let error = OneTimeWorkerImage::prepare_from_with_fault(
                    &source,
                    &root.path,
                    |observed_stage, _| {
                        if observed_stage == injected_stage {
                            Err(io::Error::other(format!(
                                "injected fault after {injected_stage:?}"
                            )))
                        } else {
                            Ok(())
                        }
                    },
                )
                .unwrap_err();

                assert!(error.to_string().contains("injected fault"));
                let entries = std::fs::read_dir(&root.path)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>();
                assert_eq!(
                    entries,
                    vec![OsString::from("source-worker")],
                    "owned publication artifacts remained after {injected_stage:?}"
                );
            }
        }

        #[test]
        fn partial_publication_cleanup_preserves_replacement_and_reports_failure() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let mut replacement_path = None;
            let mut created_inode_path = None;

            let error = OneTimeWorkerImage::prepare_from_with_fault(
                &source,
                &root.path,
                |stage, image_path| {
                    if stage != PreparationFaultStage::Created {
                        return Ok(());
                    }
                    let renamed = image_path.with_file_name("renamed-created-image");
                    std::fs::rename(image_path, &renamed)?;
                    std::fs::write(image_path, b"unowned replacement")?;
                    replacement_path = Some(image_path.to_owned());
                    created_inode_path = Some(renamed);
                    Err(io::Error::other("fault after pathname replacement"))
                },
            )
            .unwrap_err();

            let replacement_path = replacement_path.unwrap();
            let created_inode_path = created_inode_path.unwrap();
            assert!(
                error
                    .to_string()
                    .contains("fault after pathname replacement")
            );
            assert!(error.to_string().contains("cleanup failed"));
            assert!(!created_inode_path.exists());
            assert_eq!(
                std::fs::read(&replacement_path).unwrap(),
                b"unowned replacement"
            );
            assert!(replacement_path.parent().unwrap().is_dir());
        }

        #[test]
        fn before_reopen_fault_keeps_original_vnode_pinned_during_replacement_cleanup() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let mut replacement_path = None;
            let mut created_inode_path = None;

            let error = OneTimeWorkerImage::prepare_from_with_fault(
                &source,
                &root.path,
                |stage, image_path| {
                    if stage != PreparationFaultStage::BeforeReopen {
                        return Ok(());
                    }
                    let renamed = image_path.with_file_name("created-image-before-reopen");
                    std::fs::rename(image_path, &renamed)?;
                    std::fs::write(image_path, b"replacement-before-reopen")?;
                    replacement_path = Some(image_path.to_owned());
                    created_inode_path = Some(renamed);
                    Err(io::Error::other("fault before read-only reopen"))
                },
            )
            .unwrap_err();

            let replacement_path = replacement_path.unwrap();
            let created_inode_path = created_inode_path.unwrap();
            assert!(error.to_string().contains("fault before read-only reopen"));
            assert!(error.to_string().contains("cleanup failed"));
            assert!(!created_inode_path.exists());
            assert_eq!(
                std::fs::read(&replacement_path).unwrap(),
                b"replacement-before-reopen"
            );
        }

        #[test]
        fn partial_publication_cleanup_rmdirs_only_the_owned_directory_identity() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let mut replacement_directory = None;
            let mut relocated_owned_directory = None;

            let error = OneTimeWorkerImage::prepare_from_with_fault(
                &source,
                &root.path,
                |stage, image_path| {
                    if stage != PreparationFaultStage::Created {
                        return Ok(());
                    }
                    let directory = image_path.parent().unwrap();
                    let relocated = directory.with_file_name("relocated-created-directory");
                    std::fs::rename(directory, &relocated)?;
                    let mut builder = std::fs::DirBuilder::new();
                    builder.mode(0o700).create(directory)?;
                    std::fs::write(directory.join("replacement-marker"), b"replacement")?;
                    replacement_directory = Some(directory.to_owned());
                    relocated_owned_directory = Some(relocated);
                    Err(io::Error::other("fault after directory replacement"))
                },
            )
            .unwrap_err();

            let replacement_directory = replacement_directory.unwrap();
            let relocated_owned_directory = relocated_owned_directory.unwrap();
            assert!(
                error
                    .to_string()
                    .contains("fault after directory replacement")
            );
            assert!(!error.to_string().contains("cleanup failed"));
            assert!(!relocated_owned_directory.exists());
            assert_eq!(
                std::fs::read(replacement_directory.join("replacement-marker")).unwrap(),
                b"replacement"
            );
        }

        #[test]
        fn one_time_worker_image_unlink_is_identity_bound_and_drop_cleans_up() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let mut image = OneTimeWorkerImage::prepare_from(&source, &root.path).unwrap();
            let image_path = image.image_path().to_owned();
            let directory_path = image.directory_path().to_owned();
            image.unlink_after_exec().unwrap();
            assert!(!image_path.exists());
            image.retire_after_reap().unwrap();
            assert!(!directory_path.exists());

            let mut replaced = OneTimeWorkerImage::prepare_from(&source, &root.path).unwrap();
            let replacement_path = replaced.image_path().to_owned();
            let original_path = replaced.directory_path().join("original-image");
            std::fs::rename(&replacement_path, &original_path).unwrap();
            std::fs::write(&replacement_path, b"replacement").unwrap();
            assert!(replaced.unlink_after_exec().is_err());
            assert_eq!(std::fs::read(&replacement_path).unwrap(), b"replacement");
            drop(replaced);
            assert_eq!(std::fs::read(&replacement_path).unwrap(), b"replacement");
        }

        #[test]
        fn one_time_worker_image_rejects_unlink_when_directory_has_an_extra_entry() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let mut image = OneTimeWorkerImage::prepare_from(&source, &root.path).unwrap();
            let extra_path = image.directory_path().join("unexpected");
            std::fs::write(&extra_path, b"unexpected").unwrap();

            assert!(image.unlink_after_exec().is_err());
            assert!(image.image_path().exists());
            assert!(extra_path.exists());
        }

        #[test]
        fn explicit_retirement_propagates_unowned_entry_failure_after_unlink() {
            let root = TestRoot::new();
            let source = root.source("source-worker", b"worker");
            let mut image = OneTimeWorkerImage::prepare_from(&source, &root.path).unwrap();
            let directory = image.directory_path().to_owned();
            image.unlink_after_exec().unwrap();
            std::fs::write(directory.join("unexpected"), b"unexpected").unwrap();

            let error = image.retire_after_reap().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(directory.exists());
            assert_eq!(
                std::fs::read(directory.join("unexpected")).unwrap(),
                b"unexpected"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_major_parser_is_strict_and_allowlist_is_explicit() {
        assert_eq!(parse_macos_major(b"15.7.5\n").unwrap(), 15);
        assert_eq!(parse_macos_major(b"26.5.2\n").unwrap(), 26);
        for invalid in [b"".as_slice(), b"0.1", b"future", b".15"] {
            assert!(parse_macos_major(invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(VALIDATED_MACOS_MAJORS, &[26]);
    }

    #[test]
    fn unavailable_reasons_distinguish_missing_unknown_and_supported_probes() {
        let missing = missing_sandbox_exec_reason();
        assert!(missing.contains("sandbox-exec"));
        assert!(missing.contains("missing or untrusted"));
        assert!(!missing.contains("stable exact worker-image"));

        let failed_version =
            unavailable_reason_from_version_probe(Err("version probe failed".into()));
        assert!(failed_version.contains("major version could not be validated"));
        assert!(failed_version.contains("version probe failed"));

        let unknown = unavailable_reason_from_version_probe(Ok(15));
        assert!(unknown.contains("unvalidated macOS major version 15"));
        assert!(!unknown.contains("stable exact worker-image"));

        let macos_15 = availability_error_for(true, Ok(15)).unwrap();
        assert!(macos_15.contains("unvalidated macOS major version 15"));
        assert_eq!(availability_error_for(true, Ok(26)), None);
        assert!(
            availability_error_for(false, Ok(26))
                .unwrap()
                .contains("sandbox-exec")
        );
    }

    #[test]
    fn guardian_arguments_are_bounded_and_bound_to_the_exact_profile_image_pair() {
        let image = PathBuf::from(format!(
            "/private/tmp/mini-agent-js-worker-publications-{}/\
             .mini-agent-js-worker-550e8400-e29b-41d4-a716-446655440000/worker-image",
            current_uid()
        ));
        let profile = seatbelt_profile(&image).unwrap();
        let parsed = parse_guardian_arguments(
            std::iter::once(profile.clone().into())
                .chain(std::iter::once(image.clone().into_os_string()))
                .chain(std::iter::once("--".into()))
                .chain(
                    production_worker_args()
                        .iter()
                        .map(std::ffi::OsString::from),
                ),
        )
        .unwrap();
        assert_eq!(parsed.0, std::ffi::OsString::from(profile));
        assert_eq!(parsed.1, image);

        let wrong_profile = parse_guardian_arguments(
            ["(version 1)".into(), parsed.1.into_os_string(), "--".into()].into_iter(),
        )
        .unwrap_err();
        assert_eq!(wrong_profile.kind(), io::ErrorKind::PermissionDenied);

        let unexpected_worker_argument = parse_guardian_arguments(
            [
                seatbelt_profile(&image).unwrap().into(),
                image.into_os_string(),
                "--".into(),
                "unexpected".into(),
            ]
            .into_iter(),
        )
        .unwrap_err();
        assert_eq!(
            unexpected_worker_argument.kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn seatbelt_profile_allows_only_the_dyld_root_directory_lookup() {
        let image = PathBuf::from(format!(
            "/private/tmp/mini-agent-js-worker-publications-{}/\
             .mini-agent-js-worker-550e8400-e29b-41d4-a716-446655440000/worker-image",
            current_uid()
        ));
        let profile = seatbelt_profile(&image).unwrap();

        assert!(profile.contains("(allow file-read-data (literal \"/\"))"));
        assert!(!profile.contains("(subpath \"/\")"));
        assert!(!profile.contains("allow network"));
    }

    #[test]
    fn guardian_pre_exec_preserves_the_command_error_channel() {
        let (_heartbeat_parent, heartbeat_guardian) = UnixStream::pair().unwrap();
        let mut command =
            Command::new("/private/tmp/mini-agent-definitely-missing-guardian-executable");
        configure_guardian_spawn(&mut command, heartbeat_guardian.as_raw_fd()).unwrap();

        let error = command
            .spawn()
            .expect_err("a missing guardian executable must report its exact spawn error");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn macos_hosted_lifecycle_marker_is_exact() {
        assert_eq!(HOSTED_LIFECYCLE_MARKER_VALUE, "production-binary-v1");
    }

    #[test]
    fn closed_probe_diagnostic_accepts_guardian_bootstrap_only_as_an_exact_marker() {
        assert_eq!(
            parse_closed_probe_diagnostic(b"MACOS_CONTAINMENT_PROBE_FAILED=guardian_bootstrap\n"),
            Some("MACOS_CONTAINMENT_PROBE_FAILED=guardian_bootstrap")
        );
        for rejected in [
            b"guardian bootstrap failed with raw details".as_slice(),
            b"MACOS_CONTAINMENT_PROBE_FAILED=guardian_bootstrap: raw details",
            b"MACOS_CONTAINMENT_PROBE_FAILED=unlisted",
            b"\xffMACOS_CONTAINMENT_PROBE_FAILED=guardian_bootstrap",
        ] {
            assert_eq!(parse_closed_probe_diagnostic(rejected), None);
        }

        assert_eq!(
            parse_closed_probe_diagnostic(
                b"MACOS_CONTAINMENT_PROBE_FAILED=worker_limits\n\
                  MACOS_CONTAINMENT_PROBE_FAILED=guardian_bootstrap\n"
            ),
            Some("MACOS_CONTAINMENT_PROBE_FAILED=worker_limits")
        );
    }

    #[test]
    fn parent_death_record_is_exact_and_concurrency_safe() {
        let name = ".mini-agent-js-worker-550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(
            parse_parent_death_record(format!("1234 {name}\n").as_bytes()),
            Some((1234, std::ffi::OsString::from(name)))
        );
        for rejected in [
            format!("0 {name}"),
            "1234 not-canonical".into(),
            format!("1234 {name} unexpected"),
            format!("1234 {}", name.to_uppercase()),
        ] {
            assert_eq!(parse_parent_death_record(rejected.as_bytes()), None);
        }
    }

    #[test]
    fn busy_stale_sweep_is_retried_but_other_errors_fail_closed() {
        let mut attempts = 0;
        retry_busy_sweep(Instant::now() + Duration::from_secs(1), || {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempts, 3);

        let error = retry_busy_sweep(Instant::now() + Duration::from_secs(1), || {
            Err(io::Error::new(io::ErrorKind::InvalidData, "malformed"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn guardian_limits_preserve_stricter_inherited_hard_limits() {
        assert_eq!(bounded_limit(64, 32), 32);
        assert_eq!(bounded_limit(64, 128), 64);
        assert_eq!(bounded_limit(0, 128), 0);
    }
}
