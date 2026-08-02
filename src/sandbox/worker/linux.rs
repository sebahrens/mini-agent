//! Linux broker-only worker containment.
//!
//! Bubblewrap supplies an empty filesystem root and isolated namespaces. The
//! already-exec'd worker then installs irreversible rlimits, `no_new_privs`,
//! and a seccomp deny filter before it emits `Ready` or reads an untrusted
//! request. This module never uses the workspace-readable general sandbox.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule};

use super::{
    INTERNAL_WORKER_MARKER, INTERNAL_WORKER_MARKER_VALUE, WorkerBackend, WorkerContainmentStatus,
    WorkerLaunchError, WorkerProcess,
};

const BACKEND: WorkerBackend = WorkerBackend::Bubblewrap;
const WORKER_PATH: &str = "/mini-agent-worker/mini-agent";
const ADDRESS_SPACE_LIMIT: u64 = 256 * 1024 * 1024;
const CPU_LIMIT_SECONDS: u64 = 35;
const FILE_DESCRIPTOR_LIMIT: u64 = 64;
const FILE_SIZE_LIMIT: u64 = 1024 * 1024;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

static STATUS: OnceLock<WorkerContainmentStatus> = OnceLock::new();

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
    STATUS.get_or_init(probe_containment).clone()
}

pub(super) fn launch() -> Result<WorkerProcess, WorkerLaunchError> {
    match containment_status() {
        WorkerContainmentStatus::Available(BACKEND) => {}
        WorkerContainmentStatus::Available(backend) => {
            return Err(WorkerLaunchError::Unavailable {
                backend,
                reason: "worker containment preflight selected the wrong backend".into(),
            });
        }
        WorkerContainmentStatus::Unavailable { backend, reason } => {
            return Err(WorkerLaunchError::Unavailable { backend, reason });
        }
    }

    let bwrap = trusted_bwrap().ok_or_else(|| WorkerLaunchError::Unavailable {
        backend: BACKEND,
        reason: "trusted bubblewrap executable is unavailable".into(),
    })?;
    let executable = worker_executable()?;
    let mut command = broker_only_command(
        &bwrap,
        &executable,
        INTERNAL_WORKER_MARKER_VALUE,
        production_worker_args(),
    )?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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
        process: WorkerChild { child },
        input: super::child_stdin_file(input),
        output: super::child_stdout_file(output),
        stderr: super::child_stderr_file(stderr),
        backend: BACKEND,
    })
}

#[cfg(not(test))]
fn production_worker_args() -> &'static [&'static str] {
    &[]
}

#[cfg(test)]
fn production_worker_args() -> &'static [&'static str] {
    &[
        "--exact",
        "extras::js::tests::worker_runtime::worker_bootstrap_test_child",
        "--nocapture",
    ]
}

pub(super) fn finalize_worker() -> io::Result<()> {
    set_limit(Resource::As, ADDRESS_SPACE_LIMIT)?;
    set_limit(Resource::Cpu, CPU_LIMIT_SECONDS)?;
    set_limit(Resource::Nofile, FILE_DESCRIPTOR_LIMIT)?;
    set_limit(Resource::Core, 0)?;
    set_limit(Resource::Fsize, FILE_SIZE_LIMIT)?;

    rustix::thread::set_no_new_privs(true).map_err(io::Error::from)?;
    if !rustix::thread::no_new_privs().map_err(io::Error::from)? {
        return Err(io::Error::other("no_new_privs did not become irreversible"));
    }

    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(|error| io::Error::other(format!("unsupported seccomp architecture: {error}")))?;
    let rules: BTreeMap<i64, Vec<SeccompRule>> = denied_syscalls()
        .into_iter()
        .map(|syscall| (syscall, Vec::new()))
        .collect();
    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|error| io::Error::other(format!("invalid seccomp policy: {error}")))?
    .try_into()
    .map_err(|error| io::Error::other(format!("failed to compile seccomp policy: {error}")))?;
    seccompiler::apply_filter_all_threads(&filter)
        .map_err(|error| io::Error::other(format!("failed to install seccomp policy: {error}")))
}

fn set_limit(resource: Resource, ceiling: u64) -> io::Result<()> {
    let existing = getrlimit(resource);
    let effective = existing
        .current
        .unwrap_or(ceiling)
        .min(existing.maximum.unwrap_or(ceiling))
        .min(ceiling);
    let expected = Rlimit {
        current: Some(effective),
        maximum: Some(effective),
    };
    setrlimit(resource, expected).map_err(io::Error::from)?;
    if getrlimit(resource) != expected {
        return Err(io::Error::other(format!(
            "kernel did not retain required {resource:?} ceiling"
        )));
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn denied_syscalls() -> [i64; 14] {
    [
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
    ]
}

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
fn denied_syscalls() -> [i64; 12] {
    [
        libc::SYS_clone,
        libc::SYS_clone3,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
    ]
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
)))]
fn denied_syscalls() -> [i64; 0] {
    []
}

fn probe_containment() -> WorkerContainmentStatus {
    if denied_syscalls().is_empty() {
        return unavailable("seccompiler does not support this Linux architecture");
    }
    let Some(bwrap) = trusted_bwrap() else {
        return unavailable("trusted bubblewrap executable is unavailable");
    };
    let Ok(executable) = worker_executable() else {
        return unavailable("current worker executable could not be resolved");
    };
    let Ok(mut command) = broker_only_command(
        &bwrap,
        &executable,
        super::LINUX_PREFLIGHT_MARKER_VALUE,
        preflight_args(),
    ) else {
        return unavailable("broker-only bubblewrap command could not be constructed");
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return unavailable("bubblewrap containment preflight could not start");
    };
    let deadline = Instant::now() + PREFLIGHT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return WorkerContainmentStatus::Available(BACKEND);
            }
            Ok(Some(_)) => {
                return unavailable("namespace, limit, or seccomp preflight failed");
            }
            Err(_) => {
                cleanup_child(&mut child);
                return unavailable("namespace, limit, or seccomp preflight could not be reaped");
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                cleanup_child(&mut child);
                return unavailable("bubblewrap containment preflight timed out");
            }
        }
    }
}

#[cfg(not(test))]
fn preflight_args() -> &'static [&'static str] {
    &[]
}

#[cfg(test)]
fn preflight_args() -> &'static [&'static str] {
    &[
        "--exact",
        "extras::js::tests::worker_containment::linux_containment_preflight_child",
        "--nocapture",
    ]
}

fn broker_only_command(
    bwrap: &Path,
    executable: &Path,
    marker_value: &str,
    extra_args: &[&str],
) -> Result<Command, WorkerLaunchError> {
    let runtime_files = trusted_runtime_files()?;
    let runtime_directories = runtime_directories(&runtime_files);
    let mut command = Command::new(bwrap);
    command.env_clear().args([
        "--clearenv",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--cap-drop",
        "ALL",
        "--die-with-parent",
        "--new-session",
        "--close-fds",
        "--dir",
        "/mini-agent-worker",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--chdir",
        "/tmp",
    ]);

    for directory in runtime_directories {
        command.arg("--dir").arg(directory);
    }
    for runtime_file in runtime_files {
        command
            .arg("--ro-bind")
            .arg(&runtime_file)
            .arg(runtime_file);
    }
    command
        .arg("--ro-bind")
        .arg(executable)
        .arg(WORKER_PATH)
        .args([
            "--setenv",
            INTERNAL_WORKER_MARKER,
            marker_value,
            "--",
            WORKER_PATH,
        ]);
    command.args(extra_args);
    command.process_group(0);
    Ok(command)
}

fn trusted_runtime_files() -> Result<Vec<PathBuf>, WorkerLaunchError> {
    let mappings =
        std::fs::read_to_string("/proc/self/maps").map_err(|source| WorkerLaunchError::Io {
            backend: BACKEND,
            source,
        })?;
    let mut files = std::collections::BTreeSet::new();
    for line in mappings.lines() {
        let Some(path_start) = line.find('/') else {
            continue;
        };
        let path = PathBuf::from(&line[path_start..]);
        if !is_system_runtime_path(&path) {
            continue;
        }
        if !path.is_file() || !super::super::is_trusted_system_path(&path) {
            return Err(WorkerLaunchError::Unavailable {
                backend: BACKEND,
                reason: "the loaded system runtime closure contains an untrusted path".into(),
            });
        }
        files.insert(path);
    }
    for interpreter in interpreter_aliases() {
        let path = PathBuf::from(interpreter);
        if path.exists() {
            if !path.is_file() || !super::super::is_trusted_system_path(&path) {
                return Err(WorkerLaunchError::Unavailable {
                    backend: BACKEND,
                    reason: "the system runtime interpreter alias is untrusted".into(),
                });
            }
            files.insert(path);
        }
    }
    if files.is_empty() {
        return Err(WorkerLaunchError::Unavailable {
            backend: BACKEND,
            reason: "the loaded system runtime closure is empty".into(),
        });
    }
    Ok(files.into_iter().collect())
}

#[cfg(target_arch = "x86_64")]
fn interpreter_aliases() -> &'static [&'static str] {
    &["/lib64/ld-linux-x86-64.so.2"]
}

#[cfg(target_arch = "aarch64")]
fn interpreter_aliases() -> &'static [&'static str] {
    &["/lib/ld-linux-aarch64.so.1"]
}

#[cfg(target_arch = "riscv64")]
fn interpreter_aliases() -> &'static [&'static str] {
    &["/lib/ld-linux-riscv64-lp64d.so.1"]
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
)))]
fn interpreter_aliases() -> &'static [&'static str] {
    &[]
}

fn is_system_runtime_path(path: &Path) -> bool {
    [
        Path::new("/lib"),
        Path::new("/lib64"),
        Path::new("/usr/lib"),
        Path::new("/usr/lib64"),
    ]
    .into_iter()
    .any(|root| path.starts_with(root))
}

fn runtime_directories(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut directories = std::collections::BTreeSet::new();
    for file in files {
        let mut parent = file.parent();
        while let Some(directory) = parent {
            if directory == Path::new("/") {
                break;
            }
            directories.insert(directory.to_path_buf());
            parent = directory.parent();
        }
    }
    directories.into_iter().collect()
}

fn worker_executable() -> Result<PathBuf, WorkerLaunchError> {
    let executable = std::env::current_exe().map_err(|source| WorkerLaunchError::Io {
        backend: BACKEND,
        source,
    })?;
    executable
        .canonicalize()
        .map_err(|source| WorkerLaunchError::Io {
            backend: BACKEND,
            source,
        })
}

fn trusted_bwrap() -> Option<PathBuf> {
    super::super::find_trusted_system_executable("bwrap")
}

fn unavailable(reason: &str) -> WorkerContainmentStatus {
    WorkerContainmentStatus::Unavailable {
        backend: BACKEND,
        reason: reason.into(),
    }
}

fn cleanup_failed_launch(child: &mut Child, pipe: &'static str) -> WorkerLaunchError {
    cleanup_child(child);
    WorkerLaunchError::MissingPipe { pipe }
}

fn cleanup_child(child: &mut Child) {
    super::super::kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
pub(super) fn run_containment_probe() -> io::Result<()> {
    use std::io::Read;
    use std::net::{TcpListener, UdpSocket};
    use std::os::unix::net::UnixListener;

    let bwrap = trusted_bwrap()
        .ok_or_else(|| io::Error::other("trusted bubblewrap executable is unavailable"))?;
    let executable = worker_executable().map_err(|error| io::Error::other(error.to_string()))?;
    let tcp = TcpListener::bind("127.0.0.1:0")?;
    tcp.set_nonblocking(true)?;
    let udp = UdpSocket::bind("127.0.0.1:0")?;
    udp.set_nonblocking(true)?;
    let unix_path = std::env::temp_dir().join(format!(
        "mini-agent-linux-worker-probe-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let sentinel_path = unix_path.with_extension("skill-sentinel");
    const SENTINEL: &[u8] = b"host skill/config sentinel";
    std::fs::write(&sentinel_path, SENTINEL)?;
    let unix = UnixListener::bind(&unix_path)?;
    unix.set_nonblocking(true)?;
    let marker = format!(
        "linux-probe-v1:{}:{}:{}:{}",
        tcp.local_addr()?.port(),
        udp.local_addr()?.port(),
        unix_path.display(),
        sentinel_path.display()
    );
    let mut command = broker_only_command(
        &bwrap,
        &executable,
        &marker,
        &[
            "--exact",
            "extras::js::tests::worker_containment::linux_containment_probe_child",
            "--nocapture",
        ],
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                cleanup_child(&mut child);
                let _ = std::fs::remove_file(&unix_path);
                let _ = std::fs::remove_file(&sentinel_path);
                return Err(io::Error::other("Linux containment probe timed out"));
            }
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("probe stdout pipe missing"))?
        .read_to_string(&mut stdout)?;
    child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("probe stderr pipe missing"))?
        .read_to_string(&mut stderr)?;
    let tcp_isolated =
        matches!(tcp.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock);
    let mut datagram = [0_u8; 1];
    let udp_isolated =
        matches!(udp.recv(&mut datagram), Err(error) if error.kind() == io::ErrorKind::WouldBlock);
    let unix_isolated =
        matches!(unix.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock);
    let sentinel_isolated =
        std::fs::read(&sentinel_path).is_ok_and(|contents| contents == SENTINEL);
    let _ = std::fs::remove_file(&unix_path);
    let _ = std::fs::remove_file(&sentinel_path);

    if !status.success()
        || !stdout.contains("MINI_AGENT_LINUX_CONTAINMENT_PASS")
        || !stderr.is_empty()
        || !tcp_isolated
        || !udp_isolated
        || !unix_isolated
        || !sentinel_isolated
    {
        return Err(io::Error::other(format!(
            "Linux containment probe failed: status={status}, tcp={tcp_isolated}, udp={udp_isolated}, unix={unix_isolated}, sentinel={sentinel_isolated}, stdout={stdout:?}, stderr={stderr:?}"
        )));
    }
    run_cpu_limit_probe(&bwrap, &executable)?;
    run_worker_lifecycle_probes(&bwrap, &executable)?;
    Ok(())
}

#[cfg(test)]
fn run_cpu_limit_probe(bwrap: &Path, executable: &Path) -> io::Result<()> {
    let mut command = broker_only_command(
        bwrap,
        executable,
        "linux-cpu-probe-v1",
        &[
            "--exact",
            "extras::js::tests::worker_containment::linux_cpu_limit_probe_child",
            "--nocapture",
        ],
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => {
                return Err(io::Error::other(
                    "CPU limit probe returned instead of being terminated",
                ));
            }
            Some(_) => return Ok(()),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                cleanup_child(&mut child);
                return Err(io::Error::other("CPU limit probe timed out"));
            }
        }
    }
}

#[cfg(test)]
pub(super) fn run_cpu_limit_child_probe() -> io::Result<()> {
    finalize_worker()?;
    set_limit(Resource::Cpu, 1)?;
    let mut value = 0_u64;
    loop {
        value = std::hint::black_box(value.wrapping_add(1));
    }
}

#[cfg(test)]
fn run_worker_lifecycle_probes(bwrap: &Path, executable: &Path) -> io::Result<()> {
    use std::io::Write;

    let mut protocol_fault = launch().map_err(|error| io::Error::other(error.to_string()))?;
    protocol_fault.input.write_all(&0_u32.to_be_bytes())?;
    protocol_fault.input.flush()?;
    let fault_status = wait_worker_bounded(&mut protocol_fault, Duration::from_secs(5))?;
    if fault_status.success() {
        return Err(io::Error::other(
            "contained worker accepted a malformed protocol frame",
        ));
    }

    let mut terminated = launch().map_err(|error| io::Error::other(error.to_string()))?;
    terminated.terminate_tree()?;
    let _ = wait_worker_bounded(&mut terminated, Duration::from_secs(5))?;

    let mut command = broker_only_command(
        bwrap,
        executable,
        "linux-descendant-probe-v1",
        &[
            "--exact",
            "extras::js::tests::worker_containment::linux_descendant_cleanup_probe_child",
            "--nocapture",
        ],
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let launch_deadline = Instant::now() + Duration::from_secs(5);
    let descendants = loop {
        if child.try_wait()?.is_some() {
            return Err(io::Error::other(
                "descendant cleanup probe exited before teardown",
            ));
        }
        let descendants = process_descendants(child.id());
        if descendants.len() >= 2 {
            break descendants;
        }
        if Instant::now() >= launch_deadline {
            cleanup_child(&mut child);
            return Err(io::Error::other(
                "descendant cleanup probe did not create a contained descendant",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    cleanup_child(&mut child);
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    while descendants
        .iter()
        .any(|(pid, start_time)| process_start_time(*pid) == Some(*start_time))
    {
        if Instant::now() >= cleanup_deadline {
            return Err(io::Error::other(
                "contained descendant survived parent teardown",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(test)]
fn wait_worker_bounded(process: &mut WorkerProcess, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = process.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = process.terminate_tree();
            let _ = process.wait();
            return Err(io::Error::other("contained worker teardown timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
fn process_descendants(root: u32) -> Vec<(u32, u64)> {
    let mut processes = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return processes;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some((parent, start_time)) = process_identity(pid) {
            processes.push((pid, parent, start_time));
        }
    }
    let mut owners = std::collections::BTreeSet::from([root]);
    loop {
        let before = owners.len();
        for (pid, parent, _) in &processes {
            if owners.contains(parent) {
                owners.insert(*pid);
            }
        }
        if owners.len() == before {
            break;
        }
    }
    processes
        .into_iter()
        .filter_map(|(pid, _, start_time)| {
            (pid != root && owners.contains(&pid)).then_some((pid, start_time))
        })
        .collect()
}

#[cfg(test)]
fn process_identity(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

#[cfg(test)]
fn process_start_time(pid: u32) -> Option<u64> {
    process_identity(pid).map(|(_, start_time)| start_time)
}

#[cfg(test)]
pub(super) fn run_containment_child_probe() -> io::Result<()> {
    use std::io::Write;
    use std::net::{TcpStream, UdpSocket};
    use std::os::unix::net::UnixStream;

    let marker = std::env::var(INTERNAL_WORKER_MARKER)
        .map_err(|_| io::Error::other("Linux probe marker is absent"))?;
    let mut fields = marker.splitn(5, ':');
    if fields.next() != Some("linux-probe-v1") {
        return Err(io::Error::other("Linux probe marker is invalid"));
    }
    let tcp_port: u16 = fields
        .next()
        .ok_or_else(|| io::Error::other("TCP probe port is absent"))?
        .parse()
        .map_err(|_| io::Error::other("TCP probe port is invalid"))?;
    let udp_port: u16 = fields
        .next()
        .ok_or_else(|| io::Error::other("UDP probe port is absent"))?
        .parse()
        .map_err(|_| io::Error::other("UDP probe port is invalid"))?;
    let unix_path = fields
        .next()
        .ok_or_else(|| io::Error::other("Unix probe path is absent"))?;
    let sentinel_path = fields
        .next()
        .ok_or_else(|| io::Error::other("skill/config sentinel path is absent"))?;

    let environment: Vec<_> = std::env::vars_os().collect();
    if environment.len() != 1
        || environment[0].0 != INTERNAL_WORKER_MARKER
        || environment[0].1 != std::ffi::OsStr::new(&marker)
    {
        return Err(io::Error::other(format!(
            "worker environment was not reduced to the exact internal marker: {:?}",
            environment.iter().map(|(key, _)| key).collect::<Vec<_>>()
        )));
    }
    if !super::standard_streams_are_protocol_pipes() {
        return Err(io::Error::other(
            "worker standard streams are not exact protocol pipes",
        ));
    }
    for entry in std::fs::read_dir("/proc/self/fd")? {
        let entry = entry?;
        let Some(descriptor) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            return Err(io::Error::other(
                "worker exposed an invalid descriptor entry",
            ));
        };
        if descriptor <= std::io::stderr().as_raw_fd() {
            continue;
        }
        let target = std::fs::read_link(entry.path())?;
        if target != Path::new("/proc/self/fd")
            && target != PathBuf::from(format!("/proc/{}/fd", std::process::id()))
        {
            return Err(io::Error::other(format!(
                "worker inherited unexpected descriptor {descriptor} -> {}",
                target.display()
            )));
        }
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
    if std::fs::read(workspace.join("Cargo.toml")).is_ok()
        || std::fs::read_dir(workspace.join(".beads")).is_ok()
        || std::fs::write(workspace.join("linux-worker-escape"), b"escape").is_ok()
        || std::fs::read(sentinel_path).is_ok()
    {
        return Err(io::Error::other(
            "worker retained workspace or Beads authority",
        ));
    }
    let private_tmp = Path::new("/tmp/mini-agent-worker-private");
    std::fs::write(private_tmp, b"private")?;
    std::fs::remove_file(private_tmp)?;
    for device in ["/dev/mem", "/dev/sda", "/dev/nvme0n1"] {
        if std::fs::File::open(device).is_ok() {
            return Err(io::Error::other(format!(
                "worker opened host device {device}"
            )));
        }
    }
    if TcpStream::connect(("127.0.0.1", tcp_port)).is_ok() || UnixStream::connect(unix_path).is_ok()
    {
        return Err(io::Error::other("worker reached a host network listener"));
    }
    if let Ok(udp) = UdpSocket::bind("127.0.0.1:0") {
        let _ = udp.send_to(b"x", ("127.0.0.1", udp_port));
    }

    finalize_worker()?;
    assert_limit_at_most(Resource::As, ADDRESS_SPACE_LIMIT)?;
    assert_limit_at_most(Resource::Cpu, CPU_LIMIT_SECONDS)?;
    assert_limit_at_most(Resource::Nofile, FILE_DESCRIPTOR_LIMIT)?;
    assert_limit_at_most(Resource::Core, 0)?;
    assert_limit_at_most(Resource::Fsize, FILE_SIZE_LIMIT)?;
    for resource in [
        Resource::As,
        Resource::Cpu,
        Resource::Nofile,
        Resource::Core,
        Resource::Fsize,
    ] {
        assert_limit_is_irreversible(resource)?;
    }
    if !rustix::thread::no_new_privs().map_err(io::Error::from)? {
        return Err(io::Error::other("worker no_new_privs probe failed"));
    }
    let process_status = std::fs::read_to_string("/proc/self/status")?;
    if !process_status.lines().any(|line| line == "NoNewPrivs:\t1")
        || !process_status.lines().any(|line| line == "Seccomp:\t2")
    {
        return Err(io::Error::other(
            "worker kernel status lacks required restrictions",
        ));
    }
    for capability_set in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        if !process_status
            .lines()
            .any(|line| line == format!("{capability_set}\t0000000000000000"))
        {
            return Err(io::Error::other(format!(
                "worker retained a non-empty or unknown {capability_set} capability set"
            )));
        }
    }

    raw_probe::assert_denied_syscalls()?;
    for socket_result in [
        std::net::TcpListener::bind("127.0.0.1:0").map(|_| ()),
        UdpSocket::bind("127.0.0.1:0").map(|_| ()),
        std::os::unix::net::UnixListener::bind("/tmp/worker-listener").map(|_| ()),
    ] {
        if !matches!(socket_result, Err(ref error) if error.raw_os_error() == Some(libc::EPERM)) {
            return Err(io::Error::other(format!(
                "worker created a socket or received the wrong denial: {socket_result:?}"
            )));
        }
    }
    if std::process::Command::new(WORKER_PATH)
        .arg("--version")
        .status()
        .is_ok()
    {
        return Err(io::Error::other(
            "worker created an exec child after readiness",
        ));
    }
    if std::thread::Builder::new().spawn(|| {}).is_ok() {
        return Err(io::Error::other("worker created a thread after readiness"));
    }

    let mut prohibited_memory = Vec::<u8>::new();
    if prohibited_memory
        .try_reserve_exact(ADDRESS_SPACE_LIMIT as usize)
        .is_ok()
    {
        return Err(io::Error::other(
            "worker reserved memory beyond its address-space ceiling",
        ));
    }

    raw_probe::ignore_file_size_signal();
    let oversized_file = std::fs::File::create("/tmp/oversized-worker-file")?;
    let oversized_result = oversized_file.set_len(FILE_SIZE_LIMIT + 1);
    if !matches!(oversized_result, Err(ref error) if error.raw_os_error() == Some(libc::EFBIG))
        || oversized_file.metadata()?.len() > FILE_SIZE_LIMIT
    {
        return Err(io::Error::other(format!(
            "worker file-size ceiling was not enforced: {oversized_result:?}"
        )));
    }

    let mut descriptors = Vec::new();
    let descriptor_limit_hit = loop {
        match std::fs::File::open("/dev/null") {
            Ok(file) if descriptors.len() < FILE_DESCRIPTOR_LIMIT as usize + 8 => {
                descriptors.push(file)
            }
            Ok(_) => break false,
            Err(error) => break error.raw_os_error() == Some(libc::EMFILE),
        }
    };
    if !descriptor_limit_hit {
        return Err(io::Error::other(
            "worker file-descriptor ceiling was not enforced",
        ));
    }
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
fn assert_limit_at_most(resource: Resource, ceiling: u64) -> io::Result<()> {
    let limit = getrlimit(resource);
    if limit.current != limit.maximum || limit.current.is_none_or(|value| value > ceiling) {
        return Err(io::Error::other(format!(
            "worker {resource:?} limit exceeds required ceiling: {limit:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
fn assert_limit_is_irreversible(resource: Resource) -> io::Result<()> {
    let limit = getrlimit(resource);
    let ceiling = limit
        .maximum
        .ok_or_else(|| io::Error::other("worker hard limit remained infinite"))?;
    let raised = ceiling
        .checked_add(1)
        .ok_or_else(|| io::Error::other("worker hard limit cannot be probed"))?;
    if setrlimit(
        resource,
        Rlimit {
            current: Some(raised),
            maximum: Some(raised),
        },
    )
    .is_ok()
    {
        return Err(io::Error::other(format!(
            "worker raised its {resource:?} hard limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(unsafe_code)]
mod raw_probe {
    use std::io;

    pub(super) fn assert_denied_syscalls() -> io::Result<()> {
        for syscall in super::denied_syscalls() {
            let errno = raw_denied_syscall(syscall);
            if errno != Some(libc::EPERM) {
                return Err(io::Error::other(format!(
                    "worker syscall {syscall} was not denied with EPERM: {errno:?}"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn ignore_file_size_signal() {
        // SAFETY: This Linux-only adversarial test changes SIGXFSZ to SIG_IGN only after the
        // worker's finalizer has installed its permanent limits. Ignoring the signal lets the
        // probe observe the kernel's EFBIG result without weakening the file-size ceiling.
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        }
    }

    fn raw_denied_syscall(syscall: i64) -> Option<i32> {
        // SAFETY: This is a Linux-only adversarial test executed after the seccomp filter is
        // installed. The filter returns EPERM before inspecting arguments. If the filter is
        // absent, child-producing calls use exit-only child branches and invalid exec pointers;
        // the parent branch reaps any accidental child before reporting the failed probe.
        let result = unsafe {
            match syscall {
                #[cfg(target_arch = "x86_64")]
                libc::SYS_fork | libc::SYS_vfork => libc::syscall(syscall),
                libc::SYS_clone => libc::syscall(syscall, libc::SIGCHLD, 0, 0, 0, 0),
                libc::SYS_clone3 => libc::syscall(syscall, std::ptr::null::<u8>(), 0),
                libc::SYS_execve => libc::syscall(
                    syscall,
                    std::ptr::null::<u8>(),
                    std::ptr::null::<u8>(),
                    std::ptr::null::<u8>(),
                ),
                libc::SYS_execveat => libc::syscall(
                    syscall,
                    libc::AT_FDCWD,
                    std::ptr::null::<u8>(),
                    std::ptr::null::<u8>(),
                    std::ptr::null::<u8>(),
                    0,
                ),
                libc::SYS_socket => libc::syscall(
                    syscall,
                    libc::AF_NETLINK,
                    libc::SOCK_DGRAM,
                    libc::NETLINK_ROUTE,
                ),
                libc::SYS_socketpair => libc::syscall(
                    syscall,
                    libc::AF_UNIX,
                    libc::SOCK_STREAM,
                    0,
                    std::ptr::null_mut::<libc::c_int>(),
                ),
                libc::SYS_unshare => libc::syscall(syscall, libc::CLONE_NEWUSER),
                libc::SYS_setns => libc::syscall(syscall, -1, 0),
                libc::SYS_mount => libc::syscall(
                    syscall,
                    std::ptr::null::<u8>(),
                    std::ptr::null::<u8>(),
                    std::ptr::null::<u8>(),
                    0,
                    std::ptr::null::<u8>(),
                ),
                libc::SYS_umount2 => libc::syscall(syscall, std::ptr::null::<u8>(), 0),
                libc::SYS_pivot_root => {
                    libc::syscall(syscall, std::ptr::null::<u8>(), std::ptr::null::<u8>())
                }
                libc::SYS_chroot => libc::syscall(syscall, std::ptr::null::<u8>()),
                _ => return None,
            }
        };
        if result == 0 {
            #[cfg(target_arch = "x86_64")]
            let created_child = syscall == libc::SYS_clone
                || syscall == libc::SYS_fork
                || syscall == libc::SYS_vfork;
            #[cfg(not(target_arch = "x86_64"))]
            let created_child = syscall == libc::SYS_clone;
            if created_child {
                // SAFETY: Only an accidentally created probe child observes zero for these
                // child-producing calls. `_exit` avoids touching clone/vfork-shared Rust state.
                unsafe { libc::_exit(125) };
            }
            return None;
        }
        if result > 0 {
            if syscall == libc::SYS_socket {
                // SAFETY: A positive socket result is an fd created only by this probe.
                unsafe { libc::close(result as libc::c_int) };
                return None;
            }
            // SAFETY: A positive result is a child PID created only by this probe. Waiting for
            // that exact PID closes the failure path without affecting unrelated processes.
            unsafe {
                libc::waitpid(result as libc::pid_t, std::ptr::null_mut(), 0);
            }
            return None;
        }
        io::Error::last_os_error().raw_os_error()
    }
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
