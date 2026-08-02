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

use rustix::process::{
    DumpableBehavior, Resource, Rlimit, dumpable_behavior, getrlimit, set_dumpable_behavior,
    setrlimit,
};
#[cfg(target_arch = "x86_64")]
use seccompiler::sock_filter;
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule};

use super::{
    INTERNAL_WORKER_MARKER, INTERNAL_WORKER_MARKER_VALUE, WorkerBackend,
    WorkerContainmentAssurance, WorkerContainmentStatus, WorkerLaunchError, WorkerProcess,
};

const BACKEND: WorkerBackend = WorkerBackend::Bubblewrap;
const WORKER_PATH: &str = "/mini-agent-worker/mini-agent";
const ADDRESS_SPACE_LIMIT: u64 = 256 * 1024 * 1024;
const CPU_LIMIT_SECONDS: u64 = 35;
const FILE_DESCRIPTOR_LIMIT: u64 = 64;
const FILE_SIZE_LIMIT: u64 = 1024 * 1024;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

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
        WorkerContainmentStatus::Available {
            backend: BACKEND,
            assurance: WorkerContainmentAssurance::Enforced,
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
    set_dumpable_behavior(DumpableBehavior::NotDumpable).map_err(io::Error::from)?;
    if dumpable_behavior().map_err(io::Error::from)? != DumpableBehavior::NotDumpable {
        return Err(io::Error::other(
            "worker process remained dumpable after finalization",
        ));
    }

    rustix::thread::set_no_new_privs(true).map_err(io::Error::from)?;
    if !rustix::thread::no_new_privs().map_err(io::Error::from)? {
        return Err(io::Error::other("no_new_privs did not become irreversible"));
    }

    #[cfg(target_arch = "x86_64")]
    seccompiler::apply_filter_all_threads(&x32_syscall_deny_filter()).map_err(|error| {
        io::Error::other(format!("failed to deny the x32 syscall range: {error}"))
    })?;

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

#[cfg(target_arch = "x86_64")]
fn x32_syscall_deny_filter() -> BpfProgram {
    const LOAD_SYSCALL_NUMBER: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JUMP_IF_MASK_SET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
    const RETURN_CONSTANT: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

    vec![
        sock_filter {
            code: LOAD_SYSCALL_NUMBER,
            jt: 0,
            jf: 0,
            k: 0,
        },
        sock_filter {
            code: JUMP_IF_MASK_SET,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        sock_filter {
            code: RETURN_CONSTANT,
            jt: 0,
            jf: 0,
            k: u32::from(SeccompAction::Errno(libc::EPERM as u32)),
        },
        sock_filter {
            code: RETURN_CONSTANT,
            jt: 0,
            jf: 0,
            k: u32::from(SeccompAction::Allow),
        },
    ]
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
                return WorkerContainmentStatus::Available {
                    backend: BACKEND,
                    assurance: WorkerContainmentAssurance::Enforced,
                };
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
        if !is_trusted_runtime_file(&path) {
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
            if !is_trusted_runtime_file(&path) {
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

fn is_trusted_runtime_file(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for (index, ancestor) in path.ancestors().enumerate() {
        let Ok(metadata) = ancestor.metadata() else {
            return false;
        };
        if metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
            return false;
        }
        if index == 0 && !metadata.is_file() {
            return false;
        }
    }
    true
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
        assurance: WorkerContainmentAssurance::Enforced,
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
    run_core_limit_probe(&bwrap, &executable)?;
    run_worker_lifecycle_probes(&bwrap, &executable)?;
    Ok(())
}

#[cfg(test)]
fn run_cpu_limit_probe(bwrap: &Path, executable: &Path) -> io::Result<()> {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;

    const CPU_LIMIT_ARMED: &str = "MINI_AGENT_LINUX_CPU_LIMIT_ARMED";
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
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                cleanup_child(&mut child);
                return Err(io::Error::other("CPU limit probe timed out"));
            }
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("CPU probe stdout pipe missing"))?
        .read_to_string(&mut stdout)?;
    child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("CPU probe stderr pipe missing"))?
        .read_to_string(&mut stderr)?;
    if status.signal() != Some(libc::SIGXCPU)
        || !stdout.contains(CPU_LIMIT_ARMED)
        || !stderr.is_empty()
    {
        return Err(io::Error::other(format!(
            "CPU limit probe had the wrong outcome: status={status}, signal={:?}, armed={}, stderr={stderr:?}",
            status.signal(),
            stdout.contains(CPU_LIMIT_ARMED)
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn run_cpu_limit_child_probe() -> io::Result<()> {
    use std::io::Write;

    finalize_worker()?;
    let expected = Rlimit {
        current: Some(1),
        maximum: Some(2),
    };
    setrlimit(Resource::Cpu, expected).map_err(io::Error::from)?;
    if getrlimit(Resource::Cpu) != expected {
        return Err(io::Error::other(
            "CPU probe could not arm distinct soft and hard limits",
        ));
    }
    raw_probe::reset_cpu_limit_signal();
    println!("MINI_AGENT_LINUX_CPU_LIMIT_ARMED");
    std::io::stdout().flush()?;
    let mut value = 0_u64;
    loop {
        value = std::hint::black_box(value.wrapping_add(1));
    }
}

#[cfg(test)]
fn run_core_limit_probe(bwrap: &Path, executable: &Path) -> io::Result<()> {
    use std::io::Read;

    let mut command = broker_only_command(
        bwrap,
        executable,
        "linux-core-probe-v1",
        &[
            "--exact",
            "extras::js::tests::worker_containment::linux_core_limit_probe_child",
            "--nocapture",
        ],
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let status = wait_child_bounded(&mut child, Duration::from_secs(5), "core-limit probe")?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("core probe stdout pipe missing"))?
        .read_to_string(&mut stdout)?;
    child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("core probe stderr pipe missing"))?
        .read_to_string(&mut stderr)?;
    if !status.success()
        || !stdout.contains("MINI_AGENT_LINUX_CORE_LIMIT_PASS")
        || !stderr.is_empty()
    {
        return Err(io::Error::other(format!(
            "core-limit probe failed: status={status}, stdout={stdout:?}, stderr={stderr:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn run_core_limit_child_probe() -> io::Result<()> {
    use std::os::unix::process::ExitStatusExt;

    let probe_directory = Path::new("/tmp/mini-agent-core-limit-probe");
    std::fs::create_dir(probe_directory)?;
    let mut child = Command::new(WORKER_PATH)
        .env_clear()
        .env(INTERNAL_WORKER_MARKER, "linux-core-crash-v1")
        .args([
            "--exact",
            "extras::js::tests::worker_containment::linux_core_limit_probe_child",
            "--nocapture",
        ])
        .current_dir(probe_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let status = wait_child_bounded(&mut child, Duration::from_secs(5), "core crash child")?;
    if status.signal() != Some(libc::SIGABRT) || status.core_dumped() {
        return Err(io::Error::other(format!(
            "core crash child had the wrong outcome: status={status}, signal={:?}, core_dumped={}",
            status.signal(),
            status.core_dumped()
        )));
    }
    let artifacts = std::fs::read_dir(probe_directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    if !artifacts.is_empty() {
        return Err(io::Error::other(format!(
            "RLIMIT_CORE=0 left a core artifact: {artifacts:?}"
        )));
    }
    println!("MINI_AGENT_LINUX_CORE_LIMIT_PASS");
    Ok(())
}

#[cfg(test)]
pub(super) fn run_core_crash_child_probe() -> io::Result<()> {
    finalize_worker()?;
    assert_limit_at_most(Resource::Core, 0)?;
    if dumpable_behavior().map_err(io::Error::from)? != DumpableBehavior::NotDumpable {
        return Err(io::Error::other(
            "core crash child remained dumpable after finalization",
        ));
    }
    raw_probe::abort_for_core_probe()
}

#[cfg(test)]
fn wait_child_bounded(
    child: &mut Child,
    timeout: Duration,
    description: &str,
) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            cleanup_child(child);
            return Err(io::Error::other(format!("{description} timed out")));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
fn run_worker_lifecycle_probes(bwrap: &Path, executable: &Path) -> io::Result<()> {
    use std::io::Write;

    let mut protocol_fault = launch().map_err(|error| io::Error::other(error.to_string()))?;
    complete_hello_ready_and_run_step(&mut protocol_fault)?;
    protocol_fault.input.write_all(&0_u32.to_be_bytes())?;
    protocol_fault.input.flush()?;
    let fault_status = wait_worker_bounded(&mut protocol_fault, Duration::from_secs(5))?;
    if fault_status.success() {
        return Err(io::Error::other(
            "contained worker accepted a malformed protocol frame",
        ));
    }

    let mut terminated = launch().map_err(|error| io::Error::other(error.to_string()))?;
    complete_hello_ready_and_run_step(&mut terminated)?;
    terminated.terminate_tree()?;
    let terminated_status = wait_worker_bounded(&mut terminated, Duration::from_secs(5))?;
    if terminated_status.success() {
        return Err(io::Error::other(
            "explicit worker termination produced a successful exit",
        ));
    }

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
    let sleeper = loop {
        if child.try_wait()?.is_some() {
            return Err(io::Error::other(
                "descendant cleanup probe exited before teardown",
            ));
        }
        if let Some(identity) = controlled_sleeper_identity(child.id())? {
            break identity;
        }
        if Instant::now() >= launch_deadline {
            cleanup_child(&mut child);
            return Err(io::Error::other(
                "descendant cleanup probe did not create a contained descendant",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if process_start_time(sleeper.0) != Some(sleeper.1) {
        cleanup_child(&mut child);
        return Err(io::Error::other(
            "controlled sleeper identity disappeared before teardown",
        ));
    }
    cleanup_child(&mut child);
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    while process_start_time(sleeper.0) == Some(sleeper.1) {
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
fn complete_hello_ready_and_run_step(process: &mut WorkerProcess) -> io::Result<()> {
    use crate::extras::js::protocol::{
        BuildIdentity, InvocationId, ParentFrame, ParentHello, ParentProtocol, RunStep,
        StepOutcome, WireFrame, WorkerFrame, WorkerWireFrame, write_frame,
    };
    use std::io::Write;

    let build = BuildIdentity::current();
    let mut protocol = ParentProtocol::new(build.clone());
    let hello = WireFrame::connection(build, 0, ParentFrame::Hello(ParentHello {}));
    protocol
        .on_send(&hello)
        .map_err(|_| io::Error::other("lifecycle probe could not send Hello"))?;
    write_frame(&mut process.input, &hello)
        .map_err(|_| io::Error::other("lifecycle probe could not encode Hello"))?;
    process.input.flush()?;
    let ready: WorkerWireFrame = read_test_worker_frame_after_preamble(&mut process.output)?;
    protocol
        .on_receive(&ready)
        .map_err(|_| io::Error::other("lifecycle probe received unauthenticated Ready"))?;
    if !matches!(ready.message, WorkerFrame::Ready(_)) {
        return Err(io::Error::other(
            "lifecycle probe received a non-Ready startup frame",
        ));
    }
    let invocation = InvocationId::new("linux-lifecycle-ready-probe")
        .map_err(|_| io::Error::other("lifecycle probe invocation identity was invalid"))?;
    let run_step = WireFrame::invocation(
        BuildIdentity::current(),
        invocation,
        2,
        ParentFrame::RunStep(RunStep {
            code: "21 * 2".into(),
        }),
    );
    protocol
        .on_send(&run_step)
        .map_err(|_| io::Error::other("lifecycle probe could not send RunStep"))?;
    write_frame(&mut process.input, &run_step)
        .map_err(|_| io::Error::other("lifecycle probe could not encode RunStep"))?;
    process.input.flush()?;
    let result: WorkerWireFrame = read_test_worker_frame_after_preamble(&mut process.output)?;
    protocol
        .on_receive(&result)
        .map_err(|_| io::Error::other("lifecycle probe received an invalid StepResult"))?;
    match result.message {
        WorkerFrame::StepResult(step) if step.outcome == StepOutcome::Value("42".into()) => {}
        _ => {
            return Err(io::Error::other(
                "lifecycle probe did not complete its contained RunStep",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn read_test_worker_frame_after_preamble(
    reader: &mut impl std::io::Read,
) -> io::Result<crate::extras::js::protocol::WorkerWireFrame> {
    use crate::extras::js::protocol::{MAX_FRAME_BYTES, read_frame};

    let mut discarded = 0_usize;
    let mut window = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        window.push(byte[0]);
        if window.len() < 5 {
            continue;
        }
        let length = u32::from_be_bytes(
            window[..4]
                .try_into()
                .map_err(|_| io::Error::other("invalid Ready prefix"))?,
        ) as usize;
        if length > 0 && length <= MAX_FRAME_BYTES && window[4] == b'{' {
            let mut encoded = window[..5].to_vec();
            let mut tail = vec![0_u8; length - 1];
            reader.read_exact(&mut tail)?;
            encoded.extend_from_slice(&tail);
            if let Ok(frame) = read_frame(&mut encoded.as_slice()) {
                return Ok(frame);
            }
        }
        window.remove(0);
        discarded += 1;
        if discarded > 4096 {
            return Err(io::Error::other(
                "lifecycle probe Ready preamble exceeded its bound",
            ));
        }
    }
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
fn controlled_sleeper_identity(root: u32) -> io::Result<Option<(u32, u64)>> {
    const SLEEPER_TEST: &str =
        "extras::js::tests::worker_containment::linux_containment_sleeper_child";
    let sleepers = process_descendants(root)
        .into_iter()
        .filter(|(pid, _)| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|command_line| {
                command_line
                    .split(|byte| *byte == 0)
                    .any(|argument| argument == SLEEPER_TEST.as_bytes())
            })
        })
        .collect::<Vec<_>>();
    match sleepers.as_slice() {
        [] => Ok(None),
        [identity] => Ok(Some(*identity)),
        _ => Err(io::Error::other(
            "descendant cleanup probe created ambiguous controlled sleepers",
        )),
    }
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
    if dumpable_behavior().map_err(io::Error::from)? != DumpableBehavior::NotDumpable {
        return Err(io::Error::other("worker non-dumpability probe failed"));
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
    #[cfg(target_arch = "x86_64")]
    raw_probe::assert_x32_syscall_range_denied()?;
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

    pub(super) fn reset_cpu_limit_signal() {
        // SAFETY: This sacrificial Linux test process has no application signal handlers. Restoring
        // SIGXCPU's default action makes RLIMIT_CPU observable as one exact termination signal.
        unsafe {
            libc::signal(libc::SIGXCPU, libc::SIG_DFL);
        }
    }

    pub(super) fn abort_for_core_probe() -> ! {
        // SAFETY: This is a sacrificial Linux test process after RLIMIT_CORE=0 was read back. It
        // deliberately terminates with SIGABRT so its contained parent can verify no core artifact.
        unsafe { libc::abort() }
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn assert_x32_syscall_range_denied() -> io::Result<()> {
        // SAFETY: getpid takes no arguments and creates no resources. Adding the x32 ABI bit makes
        // this a representative request from the entire range guarded by the preceding JSET rule.
        let result =
            unsafe { libc::syscall((libc::SYS_getpid as u32 | super::X32_SYSCALL_BIT) as i64) };
        let errno = (result == -1)
            .then(|| io::Error::last_os_error().raw_os_error())
            .flatten();
        if errno != Some(libc::EPERM) {
            return Err(io::Error::other(format!(
                "x32 syscall range was not denied with EPERM: {errno:?}"
            )));
        }
        Ok(())
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
