use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Output;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};

#[cfg(feature = "js")]
pub(crate) mod worker;

#[cfg(feature = "js")]
pub(crate) type SandboxCommand = Command;

#[derive(Debug, Clone)]
pub struct Sandbox {
    enabled: bool,
    backend: String,
    shell: String,
    shell_command_arg: String,
    active_groups: Arc<Mutex<HashSet<u32>>>,
    cancelled_groups: Arc<Mutex<HashSet<u32>>>,
    #[cfg(test)]
    complete_process_tree_for_test: bool,
}

/// Hard bounds for one captured subprocess.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandLimits {
    pub timeout: std::time::Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub combined_bytes: usize,
}

pub(crate) const DEFAULT_COMMAND_LIMITS: CommandLimits = CommandLimits {
    timeout: std::time::Duration::from_secs(30),
    stdout_bytes: 1024 * 1024,
    stderr_bytes: 1024 * 1024,
    combined_bytes: 1536 * 1024,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandOutputLimit {
    Stdout,
    Stderr,
    Combined,
}

/// Why a captured subprocess stopped. `Completed` is the only status whose
/// stdout and stderr represent complete streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandStatus {
    Completed,
    TimedOut,
    Cancelled,
    OutputLimitExceeded(CommandOutputLimit),
    Failed,
}

pub(crate) struct CommandOutput {
    pub exit_status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: CommandStatus,
}

/// Cancellation signal for one captured subprocess operation.
///
/// Unlike [`Sandbox::kill_active`], this token never reaches other commands
/// using the same sandbox. The output worker that owns the direct child
/// observes the signal, kills that child's process group, and reaps it before
/// reporting [`CommandStatus::Cancelled`].
#[derive(Debug, Clone)]
pub(crate) struct CommandCancellation {
    sender: watch::Sender<bool>,
}

impl CommandCancellation {
    pub(crate) fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self { sender }
    }

    pub(crate) fn cancel(&self) {
        self.sender.send_replace(true);
    }

    fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.sender.subscribe()
    }
}

#[cfg(target_os = "linux")]
static BWRAP_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
#[cfg(target_os = "macos")]
static SEATBELT_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

#[cfg(target_os = "linux")]
fn bwrap_exists() -> bool {
    bwrap_path().is_some()
}

#[cfg(not(target_os = "linux"))]
fn bwrap_exists() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn bwrap_path() -> Option<&'static Path> {
    BWRAP_PATH
        .get_or_init(|| find_trusted_system_executable("bwrap"))
        .as_deref()
}

#[cfg(not(target_os = "linux"))]
fn bwrap_path() -> Option<&'static Path> {
    None
}

#[cfg(target_os = "macos")]
fn seatbelt_exists() -> bool {
    seatbelt_path().is_some()
}

#[cfg(not(target_os = "macos"))]
fn seatbelt_exists() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn seatbelt_path() -> Option<&'static Path> {
    SEATBELT_PATH
        .get_or_init(|| {
            let path = PathBuf::from("/usr/bin/sandbox-exec");
            is_trusted_system_path(&path).then_some(path)
        })
        .as_deref()
}

#[cfg(not(target_os = "macos"))]
fn seatbelt_path() -> Option<&'static Path> {
    None
}

static ZEROBOX_AVAILABLE: OnceLock<bool> = OnceLock::new();
const BWRAP_REQUESTED_NETWORK_POLICY: &str =
    "deny (unshare-net; backend absence/setup failure denies launch)";
const SEATBELT_REQUESTED_NETWORK_POLICY: &str =
    "deny (Seatbelt network*; backend absence/setup failure denies launch)";
#[cfg(feature = "js")]
const SNAPSHOT_EXECUTABLE_PATH: &str = "/run/mini-agent/spawn-executable";

fn zerobox_exists() -> bool {
    *ZEROBOX_AVAILABLE.get_or_init(|| which_cmd("zerobox"))
}

/// Explicit three-state sandbox policy — never inferred or collapsed to a bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Sandboxing was not requested; commands run unsandboxed intentionally.
    Disabled,
    /// Sandboxing was requested and the backend binary is present.
    RequiredAndAvailable,
    /// Sandboxing was requested but the backend binary is missing.
    /// Any launch attempt must be blocked — no silent fallback.
    RequiredButUnavailable,
}

/// User-visible description of the subprocess boundary selected by [`Sandbox`].
///
/// These fields describe enforced backend behavior, not aspirational feature
/// support. In particular, a requested backend that is unavailable denies
/// subprocess launch instead of reporting the backend's configured flags as
/// active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCapabilityMatrix {
    pub backend: String,
    pub status: &'static str,
    pub filesystem_reads: &'static str,
    pub filesystem_writes: &'static str,
    pub process_namespace: &'static str,
    pub devices: &'static str,
    pub environment: &'static str,
    pub network: &'static str,
    pub requested_network_policy: &'static str,
}

fn which_cmd(name: &str) -> bool {
    // Search PATH directly rather than shelling out to `which`, which may not
    // exist on minimal images (Alpine, distroless).
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(name)))
}

#[cfg(target_os = "linux")]
fn find_trusted_system_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .filter_map(|candidate| candidate.canonicalize().ok())
        .find(|candidate| is_trusted_system_path(candidate))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_trusted_system_path(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for (index, ancestor) in path.ancestors().enumerate() {
        let Ok(metadata) = ancestor.metadata() else {
            return false;
        };
        if metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
            return false;
        }
        if index == 0 && (!metadata.is_file() || metadata.permissions().mode() & 0o111 == 0) {
            return false;
        }
    }
    true
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) struct ProcessGroupGuard {
    pid: Option<u32>,
    active_groups: Arc<Mutex<HashSet<u32>>>,
}

impl ProcessGroupGuard {
    pub(crate) fn new(pid: Option<u32>, active_groups: Arc<Mutex<HashSet<u32>>>) -> Self {
        if let Some(pid) = pid {
            active_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pid);
        }
        Self { pid, active_groups }
    }

    pub(crate) fn disarm(&mut self) {
        if let Some(pid) = self.pid.take() {
            self.active_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pid);
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            self.active_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pid);
            kill_process_group(pid);
        }
    }
}

impl Sandbox {
    pub fn new(enabled: bool, backend: &str) -> Self {
        Sandbox {
            enabled,
            backend: backend.to_string(),
            shell: "bash".to_string(),
            shell_command_arg: "-c".to_string(),
            active_groups: Arc::new(Mutex::new(HashSet::new())),
            cancelled_groups: Arc::new(Mutex::new(HashSet::new())),
            #[cfg(test)]
            complete_process_tree_for_test: false,
        }
    }

    /// Explicit three-state policy derived from the requested configuration
    /// and the actual availability of the backend binary.
    pub fn policy(&self) -> SandboxPolicy {
        if !self.enabled {
            return SandboxPolicy::Disabled;
        }
        let available = match self.backend.as_str() {
            "bwrap" => bwrap_exists(),
            "seatbelt" => seatbelt_exists(),
            "zerobox" => zerobox_exists(),
            _ => false,
        };
        if available {
            SandboxPolicy::RequiredAndAvailable
        } else {
            SandboxPolicy::RequiredButUnavailable
        }
    }

    pub fn capability_matrix(&self) -> SandboxCapabilityMatrix {
        match self.policy() {
            SandboxPolicy::Disabled => SandboxCapabilityMatrix {
                backend: self.backend.clone(),
                status: "disabled",
                filesystem_reads: "host visibility inherited",
                filesystem_writes: "host permissions inherited",
                process_namespace: "host namespaces inherited",
                devices: "host devices inherited",
                environment: "parent environment inherited",
                network: "host network inherited",
                requested_network_policy: "not requested",
            },
            SandboxPolicy::RequiredButUnavailable => SandboxCapabilityMatrix {
                backend: self.backend.clone(),
                status: "requested-but-unavailable; subprocess launch denied",
                filesystem_reads: "none; subprocess launch denied",
                filesystem_writes: "none; subprocess launch denied",
                process_namespace: "none; subprocess launch denied",
                devices: "none; subprocess launch denied",
                environment: "none; subprocess launch denied",
                network: "none; subprocess launch denied",
                requested_network_policy: match self.backend.as_str() {
                    "bwrap" => BWRAP_REQUESTED_NETWORK_POLICY,
                    "seatbelt" => SEATBELT_REQUESTED_NETWORK_POLICY,
                    _ => "backend-defined; mini-agent makes no network-isolation claim",
                },
            },
            SandboxPolicy::RequiredAndAvailable if self.backend == "bwrap" => {
                SandboxCapabilityMatrix {
                    backend: self.backend.clone(),
                    status: "required-and-available",
                    filesystem_reads: "workspace, application cache, explicit read-only runtime assets, and proc kernel metadata",
                    filesystem_writes: "workspace, application cache, and private ephemeral /tmp only",
                    process_namespace: "user, PID, IPC, UTS, and cgroup namespaces isolated",
                    devices: "minimal synthetic /dev",
                    environment: "cleared, then populated from a non-credential allow-list",
                    network: "IP network denied by an isolated namespace; filesystem Unix sockets in writable binds remain reachable",
                    requested_network_policy: BWRAP_REQUESTED_NETWORK_POLICY,
                }
            }
            SandboxPolicy::RequiredAndAvailable if self.backend == "seatbelt" => {
                SandboxCapabilityMatrix {
                    backend: self.backend.clone(),
                    status: "required-and-available",
                    filesystem_reads: "host-readable files remain readable (Seatbelt read confinement is not claimed)",
                    filesystem_writes: "workspace, application cache, shared temporary directory, and /dev/null only",
                    process_namespace: "no namespace isolation; child processes inherit the Seatbelt profile",
                    devices: "host-readable devices remain readable; writes are limited to /dev/null",
                    environment: "cleared, then populated from a non-credential allow-list",
                    network: "all Seatbelt network operations denied",
                    requested_network_policy: SEATBELT_REQUESTED_NETWORK_POLICY,
                }
            }
            SandboxPolicy::RequiredAndAvailable => SandboxCapabilityMatrix {
                backend: self.backend.clone(),
                status: "required-and-available",
                filesystem_reads: "backend-defined",
                filesystem_writes: "workspace allowed; other behavior backend-defined",
                process_namespace: "backend-defined",
                devices: "backend-defined",
                environment: "backend-defined",
                network: "backend-defined; mini-agent makes no network-isolation claim",
                requested_network_policy: "backend-defined; mini-agent makes no network-isolation claim",
            },
        }
    }

    /// Whether this sandbox owns the complete descendant lifetime independently of process-group
    /// membership. JS spawn authority is issued only when this stronger boundary is available.
    pub(crate) fn owns_complete_process_tree(&self) -> bool {
        #[cfg(test)]
        if self.complete_process_tree_for_test {
            return true;
        }
        cfg!(target_os = "linux")
            && self.backend == "bwrap"
            && self.policy() == SandboxPolicy::RequiredAndAvailable
    }

    #[cfg(test)]
    pub(crate) fn with_complete_process_tree_for_test(mut self) -> Self {
        self.complete_process_tree_for_test = true;
        self
    }

    pub fn with_shell(self, shell: &str) -> Self {
        self.with_shell_command_arg(shell, "-c")
    }

    /// Selects a shell whose script flag differs from the POSIX `-c`
    /// contract. The flag is passed as one literal argument; it is never
    /// concatenated with the command text.
    pub(crate) fn with_shell_command_arg(mut self, shell: &str, command_arg: &str) -> Self {
        if !shell.is_empty() {
            self.shell = shell.to_string();
        }
        if !command_arg.is_empty() {
            self.shell_command_arg = command_arg.to_string();
        }
        self
    }

    pub fn wrap_command(&self, command: &str) -> Result<Command, String> {
        self.wrap_command_inner(command)
    }

    #[cfg(feature = "js")]
    pub(crate) fn wrap_command_with_executable_snapshot(
        &self,
        arguments: &[String],
    ) -> Result<Command, String> {
        if !self.supports_immutable_executable_snapshot() {
            return Err("sandbox backend cannot bind an immutable executable snapshot".to_string());
        }
        let cwd = std::env::current_dir()
            .map_err(|error| format!("sandbox: failed to resolve working directory: {error}"))?;
        let cwd = canonical_non_root(&cwd, "working directory")?;
        let paths = crate::paths::process_paths()
            .map_err(|error| format!("sandbox: application paths are unavailable: {error}"))?;
        std::fs::create_dir_all(&paths.cache_dir).map_err(|error| {
            format!(
                "sandbox: failed to create application cache {}: {error}",
                paths.cache_dir.display()
            )
        })?;
        let cache_dir = canonical_non_root(&paths.cache_dir, "application cache")?;
        let bwrap = bwrap_path().ok_or_else(|| {
            "sandbox backend 'bwrap' is not a trusted system executable — refusing to run unsandboxed"
                .to_string()
        })?;
        Ok(self.build_bwrap_snapshot_command(bwrap, &cwd, &cache_dir, arguments))
    }

    #[cfg(feature = "js")]
    pub(crate) fn supports_immutable_executable_snapshot(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self.policy(), SandboxPolicy::RequiredAndAvailable) && self.backend == "bwrap"
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn wrap_command_inner(&self, command: &str) -> Result<Command, String> {
        match self.policy() {
            SandboxPolicy::Disabled => {
                let mut cmd = Command::new(&self.shell);
                cmd.arg(&self.shell_command_arg).arg(command);
                configure_child_lifetime(&mut cmd);
                return Ok(cmd);
            }
            SandboxPolicy::RequiredButUnavailable => {
                return Err(format!(
                    "sandbox backend '{}' is not available — refusing to run unsandboxed (requested-but-unavailable)",
                    self.backend
                ));
            }
            SandboxPolicy::RequiredAndAvailable => {}
        }

        let cwd = std::env::current_dir()
            .map_err(|error| format!("sandbox: failed to resolve working directory: {error}"))?;
        let cwd = canonical_non_root(&cwd, "working directory")?;

        if self.backend == "zerobox" {
            let mut cmd = Command::new("zerobox");
            cmd.arg("--allow-write");
            cmd.arg(cwd.as_os_str());
            cmd.arg("--");
            cmd.arg(&self.shell);
            cmd.arg(&self.shell_command_arg);
            cmd.arg(command);
            configure_child_lifetime(&mut cmd);
            return Ok(cmd);
        }

        let paths = crate::paths::process_paths()
            .map_err(|error| format!("sandbox: application paths are unavailable: {error}"))?;
        std::fs::create_dir_all(&paths.cache_dir).map_err(|error| {
            format!(
                "sandbox: failed to create application cache {}: {error}",
                paths.cache_dir.display()
            )
        })?;
        let cache_dir = canonical_non_root(&paths.cache_dir, "application cache")?;

        if self.backend == "seatbelt" {
            let seatbelt = seatbelt_path().ok_or_else(|| {
                "sandbox backend 'seatbelt' is not a trusted system executable — refusing to run unsandboxed"
                    .to_string()
            })?;
            return self.build_seatbelt_command(seatbelt, command, &cwd, &cache_dir);
        }

        let bwrap = bwrap_path().ok_or_else(|| {
            "sandbox backend 'bwrap' is not a trusted system executable — refusing to run unsandboxed"
                .to_string()
        })?;
        Ok(self.build_bwrap_command(bwrap, command, &cwd, &cache_dir))
    }

    fn build_seatbelt_command(
        &self,
        seatbelt: &Path,
        command: &str,
        cwd: &Path,
        cache_dir: &Path,
    ) -> Result<Command, String> {
        let workspace = seatbelt_string_literal(cwd, "working directory")?;
        let cache = seatbelt_string_literal(cache_dir, "application cache")?;
        let profile = format!(
            r#"(version 1)
(deny default)
(allow process*)
(allow file-read*)
(allow file-write*
    (subpath "{workspace}")
    (subpath "{cache}")
    (subpath "/private/tmp")
    (literal "/dev/null"))
(deny network*)"#
        );

        let mut cmd = Command::new(seatbelt);
        cmd.arg("-p").arg(profile);
        // `env -i` is inside the sandbox wrapper, so callers cannot restore
        // credentials by adding environment variables to the returned command.
        cmd.arg("/usr/bin/env").arg("-i");
        for (key, value) in essential_env() {
            cmd.arg(format!("{key}={value}"));
        }
        cmd.arg("TMPDIR=/private/tmp");
        cmd.arg(&self.shell)
            .arg(&self.shell_command_arg)
            .arg(command);
        configure_child_lifetime(&mut cmd);
        Ok(cmd)
    }

    fn build_bwrap_command(
        &self,
        bwrap: &Path,
        command: &str,
        cwd: &Path,
        cache_dir: &Path,
    ) -> Command {
        let mut cmd = self.build_bwrap_base_command(bwrap, cwd, cache_dir);
        append_bwrap_isolation(&mut cmd, cwd);
        cmd.args([
            "--die-with-parent",
            "--",
            &self.shell,
            &self.shell_command_arg,
            command,
        ]);
        configure_child_lifetime(&mut cmd);
        cmd
    }

    #[cfg(feature = "js")]
    fn build_bwrap_snapshot_command(
        &self,
        bwrap: &Path,
        cwd: &Path,
        cache_dir: &Path,
        arguments: &[String],
    ) -> Command {
        let mut cmd = self.build_bwrap_base_command(bwrap, cwd, cache_dir);
        cmd.args(["--dir", "/run", "--dir", "/run/mini-agent"]);
        // fd 3 is consumed by bubblewrap while constructing this read-only executable file.
        // It is deliberately not listed under `--preserve-fds`, so neither the target nor any
        // descendant can inherit the snapshot descriptor.
        cmd.args([
            "--perms",
            "0500",
            "--ro-bind-data",
            "3",
            SNAPSHOT_EXECUTABLE_PATH,
        ]);
        append_bwrap_isolation(&mut cmd, cwd);
        cmd.args(["--die-with-parent", "--", SNAPSHOT_EXECUTABLE_PATH]);
        cmd.args(arguments);
        configure_child_lifetime(&mut cmd);
        cmd
    }

    fn build_bwrap_base_command(&self, bwrap: &Path, cwd: &Path, cache_dir: &Path) -> Command {
        let mut cmd = Command::new(bwrap);
        cmd.arg("--clearenv");
        for (key, value) in essential_env() {
            cmd.arg("--setenv").arg(key).arg(value);
        }
        cmd.args(["--setenv", "TMPDIR", "/tmp"]);
        for path in ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/nix"] {
            cmd.args(["--ro-bind-try", path, path]);
        }
        cmd.args(["--dir", "/etc"]);
        for path in ["/etc/localtime", "/etc/ld.so.cache"] {
            cmd.args(["--ro-bind-try", path, path]);
        }
        cmd.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        cmd.arg("--bind").arg(cwd).arg(cwd);
        cmd.arg("--bind").arg(cache_dir).arg(cache_dir);
        cmd
    }

    #[cfg(test)]
    pub async fn output_command(&self, command: &str) -> std::io::Result<Output> {
        let output = self
            .output_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
            .await?;
        if output.status != CommandStatus::Completed {
            return Err(std::io::Error::other(format!(
                "command did not complete: {:?}",
                output.status
            )));
        }
        let status = output
            .exit_status
            .ok_or_else(|| std::io::Error::other("completed command had no exit status"))?;
        Ok(Output {
            status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    /// Runs a command on a background task so dropping the receiver is an
    /// observable cancellation event. The worker owns the child until it has
    /// killed the process group and reaped the direct child.
    pub(crate) async fn output_command_with_limits(
        &self,
        command: &str,
        limits: CommandLimits,
    ) -> std::io::Result<CommandOutput> {
        self.output_command_with_limits_scoped(command, limits, None)
            .await
    }

    #[cfg(feature = "loop")]
    pub(crate) async fn output_command_with_limits_cancelled(
        &self,
        command: &str,
        limits: CommandLimits,
        cancellation: &CommandCancellation,
    ) -> std::io::Result<CommandOutput> {
        if cancellation.is_cancelled() {
            return Ok(CommandOutput {
                exit_status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: CommandStatus::Cancelled,
            });
        }
        self.output_command_with_limits_scoped(command, limits, Some(cancellation.subscribe()))
            .await
    }

    async fn output_command_with_limits_scoped(
        &self,
        command: &str,
        limits: CommandLimits,
        cancellation: Option<watch::Receiver<bool>>,
    ) -> std::io::Result<CommandOutput> {
        let cmd = match self.wrap_command(command) {
            Ok(cmd) => cmd,
            Err(error) => {
                return Ok(CommandOutput {
                    exit_status: None,
                    stdout: Vec::new(),
                    stderr: error.into_bytes(),
                    status: CommandStatus::Failed,
                });
            }
        };
        self.output_built_command_with_limits_scoped(cmd, limits, cancellation)
            .await
    }

    pub(crate) async fn output_built_command_with_limits(
        &self,
        cmd: Command,
        limits: CommandLimits,
    ) -> std::io::Result<CommandOutput> {
        self.output_built_command_with_limits_scoped(cmd, limits, None)
            .await
    }

    /// Cancellation-scoped variant for already-authorized commands. The returned future does
    /// not resolve with `Cancelled` until the direct child is reaped and its process group has
    /// been killed, so callers can finish durable reconciliation without leaking descendants.
    #[cfg(feature = "js")]
    pub(crate) async fn output_built_command_with_limits_cancelled(
        &self,
        cmd: Command,
        limits: CommandLimits,
        cancellation: &CommandCancellation,
    ) -> std::io::Result<CommandOutput> {
        if cancellation.is_cancelled() {
            return Ok(CommandOutput {
                exit_status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: CommandStatus::Cancelled,
            });
        }
        self.output_built_command_with_limits_scoped(cmd, limits, Some(cancellation.subscribe()))
            .await
    }

    async fn output_built_command_with_limits_scoped(
        &self,
        cmd: Command,
        limits: CommandLimits,
        cancellation: Option<watch::Receiver<bool>>,
    ) -> std::io::Result<CommandOutput> {
        let (response_tx, response_rx) = oneshot::channel();
        let sandbox = self.clone();
        tokio::spawn(async move {
            sandbox
                .run_built_output_command(cmd, limits, cancellation, response_tx)
                .await;
        });
        response_rx.await.map_err(|_| {
            std::io::Error::other("command output worker stopped before returning a result")
        })
    }

    async fn run_built_output_command(
        &self,
        mut cmd: Command,
        limits: CommandLimits,
        mut cancellation: Option<watch::Receiver<bool>>,
        mut response_tx: oneshot::Sender<CommandOutput>,
    ) {
        if cancellation
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow())
        {
            let _ = response_tx.send(CommandOutput {
                exit_status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: CommandStatus::Cancelled,
            });
            return;
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = response_tx.send(CommandOutput {
                    exit_status: None,
                    stdout: Vec::new(),
                    stderr: format!("failed to spawn command: {error}").into_bytes(),
                    status: CommandStatus::Failed,
                });
                return;
            }
        };
        let pid = child.id();
        let mut guard = ProcessGroupGuard::new(child.id(), self.active_groups.clone());
        let captured = Arc::new(Mutex::new(CapturedCommandOutput::default()));
        let (reader_error_tx, mut reader_error_rx) = mpsc::unbounded_channel();
        let stdout_handle = spawn_bounded_pipe_reader(
            child.stdout.take(),
            CommandOutputStream::Stdout,
            captured.clone(),
            limits,
            reader_error_tx.clone(),
        );
        let stderr_handle = spawn_bounded_pipe_reader(
            child.stderr.take(),
            CommandOutputStream::Stderr,
            captured.clone(),
            limits,
            reader_error_tx,
        );

        let termination = tokio::select! {
            biased;
            _ = wait_for_command_cancellation(cancellation.as_mut()) => CommandTermination::Cancelled,
            status = child.wait() => CommandTermination::Exited(status),
            Some(error) = reader_error_rx.recv() => CommandTermination::ReaderError(error),
            _ = tokio::time::sleep(limits.timeout) => CommandTermination::TimedOut,
            _ = response_tx.closed() => CommandTermination::Cancelled,
        };

        let (mut exit_status, mut command_status) = match termination {
            CommandTermination::Exited(Ok(status)) => {
                // A descendant may inherit a pipe after the shell exits. End
                // the process group before joining readers so it cannot hold
                // the command open or continue running in the background.
                if let Some(pid) = pid {
                    kill_process_group(pid);
                }
                let command_status = if self.take_cancelled(pid) {
                    CommandStatus::Cancelled
                } else {
                    CommandStatus::Completed
                };
                (Some(status), command_status)
            }
            CommandTermination::Exited(Err(error)) => {
                tracing::warn!("sandbox: failed to wait for command: {error}");
                terminate_and_reap(&mut child, pid).await;
                (None, CommandStatus::Failed)
            }
            CommandTermination::ReaderError(error) => {
                let status = match error {
                    CommandRunError::OutputLimit(limit) => {
                        CommandStatus::OutputLimitExceeded(limit)
                    }
                    CommandRunError::Read(error) => {
                        tracing::warn!("sandbox: failed to consume command output: {error}");
                        CommandStatus::Failed
                    }
                };
                terminate_and_reap(&mut child, pid).await;
                (None, status)
            }
            CommandTermination::TimedOut => {
                terminate_and_reap(&mut child, pid).await;
                (None, CommandStatus::TimedOut)
            }
            CommandTermination::Cancelled => {
                terminate_and_reap(&mut child, pid).await;
                (None, CommandStatus::Cancelled)
            }
        };

        if finish_pipe_readers(stdout_handle, stderr_handle)
            .await
            .is_err()
            && command_status == CommandStatus::Completed
        {
            command_status = CommandStatus::Failed;
        }
        if command_status == CommandStatus::Completed
            && let Ok(error) = reader_error_rx.try_recv()
        {
            command_status = match error {
                CommandRunError::OutputLimit(limit) => CommandStatus::OutputLimitExceeded(limit),
                CommandRunError::Read(error) => {
                    tracing::warn!("sandbox: failed to consume command output: {error}");
                    CommandStatus::Failed
                }
            };
        }
        if command_status != CommandStatus::Completed {
            exit_status = None;
        }
        guard.disarm();
        self.take_cancelled(pid);

        let mut captured = captured.lock().unwrap_or_else(|e| e.into_inner());
        let output = CommandOutput {
            exit_status,
            stdout: std::mem::take(&mut captured.stdout),
            stderr: std::mem::take(&mut captured.stderr),
            status: command_status,
        };
        drop(captured);
        let _ = response_tx.send(output);
    }

    pub fn kill_active(&self) {
        let groups: Vec<u32> = self
            .active_groups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        self.cancelled_groups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(groups.iter().copied());
        for pid in groups {
            kill_process_group(pid);
        }
    }

    #[allow(dead_code)]
    pub fn active_group_count(&self) -> usize {
        self.active_groups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    fn take_cancelled(&self, pid: Option<u32>) -> bool {
        pid.is_some_and(|pid| {
            self.cancelled_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pid)
        })
    }
}

async fn wait_for_command_cancellation(cancellation: Option<&mut watch::Receiver<bool>>) {
    let Some(cancellation) = cancellation else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *cancellation.borrow() {
            return;
        }
        if cancellation.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[derive(Default)]
struct CapturedCommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    combined_bytes: usize,
}

impl CapturedCommandOutput {
    fn push(
        &mut self,
        stream: CommandOutputStream,
        bytes: &[u8],
        limits: CommandLimits,
    ) -> Result<(), CommandOutputLimit> {
        let (output, stream_limit, stream_error) = match stream {
            CommandOutputStream::Stdout => (
                &mut self.stdout,
                limits.stdout_bytes,
                CommandOutputLimit::Stdout,
            ),
            CommandOutputStream::Stderr => (
                &mut self.stderr,
                limits.stderr_bytes,
                CommandOutputLimit::Stderr,
            ),
        };
        let stream_remaining = stream_limit.saturating_sub(output.len());
        let combined_remaining = limits.combined_bytes.saturating_sub(self.combined_bytes);
        let accepted = bytes.len().min(stream_remaining).min(combined_remaining);
        output.extend_from_slice(&bytes[..accepted]);
        self.combined_bytes += accepted;

        if accepted == bytes.len() {
            Ok(())
        } else if stream_remaining <= combined_remaining {
            Err(stream_error)
        } else {
            Err(CommandOutputLimit::Combined)
        }
    }
}

#[derive(Clone, Copy)]
enum CommandOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
enum CommandRunError {
    OutputLimit(CommandOutputLimit),
    Read(std::io::Error),
}

enum CommandTermination {
    Exited(std::io::Result<ExitStatus>),
    ReaderError(CommandRunError),
    TimedOut,
    Cancelled,
}

fn spawn_bounded_pipe_reader<R>(
    pipe: Option<R>,
    stream: CommandOutputStream,
    captured: Arc<Mutex<CapturedCommandOutput>>,
    limits: CommandLimits,
    error_tx: mpsc::UnboundedSender<CommandRunError>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let Some(mut pipe) = pipe else {
            return;
        };
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match pipe.read(&mut buffer).await {
                Ok(0) => return,
                Ok(read) => read,
                Err(error) => {
                    let _ = error_tx.send(CommandRunError::Read(error));
                    return;
                }
            };
            let result = captured.lock().unwrap_or_else(|e| e.into_inner()).push(
                stream,
                &buffer[..read],
                limits,
            );
            if let Err(limit) = result {
                let _ = error_tx.send(CommandRunError::OutputLimit(limit));
                return;
            }
        }
    })
}

async fn finish_pipe_readers(
    mut stdout: tokio::task::JoinHandle<()>,
    mut stderr: tokio::task::JoinHandle<()>,
) -> std::io::Result<()> {
    let joined = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        (&mut stdout)
            .await
            .map_err(|error| std::io::Error::other(format!("stdout reader failed: {error}")))?;
        (&mut stderr)
            .await
            .map_err(|error| std::io::Error::other(format!("stderr reader failed: {error}")))
    })
    .await;
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            stdout.abort();
            stderr.abort();
            Err(error)
        }
        Err(_) => {
            stdout.abort();
            stderr.abort();
            Err(std::io::Error::other(
                "command pipe readers did not stop after process termination",
            ))
        }
    }
}

async fn terminate_and_reap(child: &mut Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        kill_process_group(pid);
    }
    let _ = child.start_kill();
    if let Err(error) = child.wait().await {
        tracing::warn!("sandbox: failed to reap terminated command: {error}");
    }
}

pub(crate) fn configure_child_lifetime(cmd: &mut Command) {
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
}

pub(crate) fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let group = format!("-{}", pid);
        let _ = std::process::Command::new("kill")
            .args(["-TERM", "--", &group])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn essential_env() -> Vec<(&'static str, String)> {
    let preserve = [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "LC_ALL",
        "COLORTERM",
        "NO_COLOR",
    ];
    let mut vars = Vec::with_capacity(preserve.len());
    for name in &preserve {
        if let Ok(val) = std::env::var(name) {
            vars.push((*name, val));
        }
    }
    vars
}

fn append_bwrap_isolation(command: &mut Command, cwd: &Path) {
    command.args([
        "--unshare-user",
        "--unshare-ipc",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-uts",
        "--unshare-cgroup",
        "--remount-ro",
        "/",
        "--chdir",
    ]);
    command.arg(cwd);
}

fn canonical_non_root(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "sandbox: failed to resolve {label} {}: {error}",
            path.display()
        )
    })?;
    if canonical.parent().is_none() {
        return Err(format!(
            "sandbox: refusing to expose filesystem root as {label}"
        ));
    }
    Ok(canonical)
}

fn seatbelt_string_literal(path: &Path, label: &str) -> Result<String, String> {
    let value = path.to_str().ok_or_else(|| {
        format!(
            "sandbox: {label} {} is not valid UTF-8 for the Seatbelt profile",
            path.display()
        )
    })?;
    if value.chars().any(char::is_control) {
        return Err(format!(
            "sandbox: {label} contains control characters that cannot be represented safely in a Seatbelt profile"
        ));
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;

    fn disabled() -> Sandbox {
        Sandbox::new(false, "bwrap")
    }

    fn unavailable() -> Sandbox {
        Sandbox::new(true, "__no_such_backend_exists__")
    }

    #[test]
    fn sandbox_disabled_policy_is_disabled() {
        assert_eq!(disabled().policy(), SandboxPolicy::Disabled);
        assert!(!disabled().owns_complete_process_tree());
        assert!(
            disabled()
                .with_complete_process_tree_for_test()
                .owns_complete_process_tree()
        );
    }

    #[test]
    fn sandbox_custom_shell_command_arg_is_a_literal_operand() {
        let command = disabled()
            .with_shell_command_arg("custom-shell", "-Command")
            .wrap_command("literal; script")
            .unwrap();
        assert_eq!(command.as_std().get_program(), "custom-shell");
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(
            args,
            [
                std::ffi::OsStr::new("-Command"),
                std::ffi::OsStr::new("literal; script")
            ]
        );
    }

    #[test]
    fn sandbox_unavailable_policy_is_required_but_unavailable() {
        assert_eq!(
            unavailable().policy(),
            SandboxPolicy::RequiredButUnavailable
        );
    }

    #[test]
    fn sandbox_unavailable_wrap_command_returns_error() {
        let err = unavailable().wrap_command("echo must not run").unwrap_err();
        assert!(
            err.contains("__no_such_backend_exists__"),
            "error should name the backend: {err}"
        );
        assert!(
            err.contains("unavailable"),
            "error should mention unavailability: {err}"
        );
    }

    #[tokio::test]
    async fn sandbox_unavailable_output_command_fails_without_running() {
        let output = unavailable()
            .output_command_with_limits("echo must not run", DEFAULT_COMMAND_LIMITS)
            .await
            .unwrap();
        assert_eq!(output.status, CommandStatus::Failed);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unavailable"),
            "stderr should contain unavailability message: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "stdout must be empty for a blocked command"
        );
    }

    #[test]
    fn sandbox_disabled_wrap_command_succeeds() {
        let cmd = disabled().wrap_command("echo ok");
        assert!(
            cmd.is_ok(),
            "disabled sandbox must always produce a command"
        );
    }

    #[test]
    fn linux_sandbox_policy_command_matches_capability_matrix() {
        let sandbox = Sandbox::new(true, "bwrap");
        let cmd = sandbox.build_bwrap_command(
            Path::new("/usr/bin/bwrap"),
            "printf sandboxed",
            Path::new("/workspace"),
            Path::new("/cache/mini-agent"),
        );
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        for flag in [
            "--clearenv",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup",
            "--unshare-net",
            "--dev",
            "--proc",
            "--remount-ro",
        ] {
            assert!(args.iter().any(|arg| arg == flag), "missing {flag}");
        }
        assert!(
            !args
                .windows(3)
                .any(|args| args[0] == "--ro-bind" && args[1] == "/" && args[2] == "/"),
            "the host root must never be visible inside the sandbox"
        );
        assert!(args.windows(3).any(|args| {
            args[0] == "--bind" && args[1] == "/workspace" && args[2] == "/workspace"
        }));
        assert!(args.windows(3).any(|args| {
            args[0] == "--bind" && args[1] == "/cache/mini-agent" && args[2] == "/cache/mini-agent"
        }));
        for credential_variable in [
            "OPENROUTER_API_KEY",
            "SSH_AUTH_SOCK",
            "SSH_ASKPASS",
            "GIT_ASKPASS",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            assert!(
                !args.iter().any(|arg| arg == credential_variable),
                "{credential_variable} must not be forwarded"
            );
        }

        let matrix = sandbox.capability_matrix();
        assert_eq!(
            matrix.requested_network_policy,
            BWRAP_REQUESTED_NETWORK_POLICY
        );
    }

    #[test]
    fn macos_seatbelt_policy_command_matches_capability_matrix() {
        let sandbox = Sandbox::new(true, "seatbelt");
        let command = sandbox
            .build_seatbelt_command(
                Path::new("/usr/bin/sandbox-exec"),
                "printf sandboxed",
                Path::new("/workspace"),
                Path::new("/cache/mini-agent"),
            )
            .unwrap();
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let profile = args
            .windows(2)
            .find_map(|args| (args[0] == "-p").then_some(args[1].as_str()))
            .expect("Seatbelt profile argument");

        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains(r#"(subpath "/workspace")"#));
        assert!(profile.contains(r#"(subpath "/cache/mini-agent")"#));
        assert!(!profile.contains("(allow network"));
        assert!(
            args.windows(2)
                .any(|args| args[0] == "/usr/bin/env" && args[1] == "-i"),
            "Seatbelt child environment must be cleared inside the wrapper"
        );
        for credential_variable in [
            "OPENROUTER_API_KEY",
            "SSH_AUTH_SOCK",
            "SSH_ASKPASS",
            "GIT_ASKPASS",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.starts_with(&format!("{credential_variable}="))),
                "{credential_variable} must not be forwarded"
            );
        }

        let matrix = SandboxCapabilityMatrix {
            backend: "seatbelt".to_string(),
            status: "required-and-available",
            filesystem_reads: "host-readable files remain readable (Seatbelt read confinement is not claimed)",
            filesystem_writes: "workspace, application cache, shared temporary directory, and /dev/null only",
            process_namespace: "no namespace isolation; child processes inherit the Seatbelt profile",
            devices: "host-readable devices remain readable; writes are limited to /dev/null",
            environment: "cleared, then populated from a non-credential allow-list",
            network: "all Seatbelt network operations denied",
            requested_network_policy: SEATBELT_REQUESTED_NETWORK_POLICY,
        };
        assert_eq!(matrix.network, "all Seatbelt network operations denied");
    }

    #[test]
    fn seatbelt_profile_escapes_paths_and_rejects_controls() {
        assert_eq!(
            seatbelt_string_literal(Path::new(r#"/tmp/a\"b"#), "probe").unwrap(),
            r#"/tmp/a\\\"b"#
        );
        let error = seatbelt_string_literal(Path::new("/tmp/a\nb"), "probe").unwrap_err();
        assert!(error.contains("control characters"));
    }

    #[test]
    fn linux_sandbox_policy_unknown_backend_is_unavailable() {
        assert_eq!(
            unavailable().policy(),
            SandboxPolicy::RequiredButUnavailable
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_sandbox_policy_enforces_real_backend() {
        if !bwrap_exists() {
            eprintln!("skipping real Linux sandbox probe because bwrap is not installed");
            return;
        }

        let unique = uuid::Uuid::new_v4();
        let host_secret = std::env::temp_dir().join(format!("mini-agent-host-secret-{unique}"));
        std::fs::write(&host_secret, b"must stay hidden").unwrap();
        let workspace_probe = std::env::current_dir()
            .unwrap()
            .join(format!(".mini-agent-sandbox-write-{unique}"));
        let workspace_probe_name = workspace_probe.file_name().unwrap().to_string_lossy();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let loopback_port = listener.local_addr().unwrap().port();

        let script = format!(
            r#"
if cat /tmp/{host_secret_name} >/dev/null 2>&1; then exit 10; fi
if printf denied >/etc/mini-agent-sandbox-policy-probe 2>/dev/null; then exit 11; fi
test -r Cargo.toml || exit 12
printf workspace > {workspace_probe_name}
test "$(cat {workspace_probe_name})" = workspace || exit 13
rm -f {workspace_probe_name}
test -c /dev/null || exit 14
test ! -e /dev/sda || exit 15
test -z "${{MINI_AGENT_SANDBOX_SECRET+x}}" || exit 16
if (exec 3<>/dev/tcp/127.0.0.1/{loopback_port}) 2>/dev/null; then exit 17; fi
if (exec 3<>/dev/tcp/1.1.1.1/53) 2>/dev/null; then exit 18; fi
printf LINUX_SANDBOX_POLICY_PASS
"#,
            host_secret_name = host_secret.file_name().unwrap().to_string_lossy(),
            workspace_probe_name = workspace_probe_name,
        );

        let sandbox = Sandbox::new(true, "bwrap");
        let mut command = sandbox.wrap_command(&script).unwrap();
        command.env("MINI_AGENT_SANDBOX_SECRET", "must-not-cross-clearenv");
        let output = sandbox
            .output_built_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
            .await
            .unwrap();

        drop(listener);
        let _ = std::fs::remove_file(&host_secret);
        let _ = std::fs::remove_file(&workspace_probe);
        assert_eq!(
            output.status,
            CommandStatus::Completed,
            "sandbox probe did not complete: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.exit_status.is_some_and(|status| status.success()),
            "sandbox probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"LINUX_SANDBOX_POLICY_PASS");
        assert_eq!(
            sandbox.capability_matrix().network,
            "IP network denied by an isolated namespace; filesystem Unix sockets in writable binds remain reachable"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_sandbox_policy_backend_setup_failure_is_fail_closed() {
        if !bwrap_exists() {
            eprintln!("skipping real Linux sandbox probe because bwrap is not installed");
            return;
        }

        let marker = std::env::current_dir().unwrap().join(format!(
            ".mini-agent-sandbox-setup-failure-{}",
            uuid::Uuid::new_v4()
        ));
        let marker_name = marker.file_name().unwrap().to_string_lossy();
        let sandbox =
            Sandbox::new(true, "bwrap").with_shell("/__mini_agent_missing_sandbox_shell__");
        let output = sandbox
            .output_command_with_limits(&format!("touch {marker_name}"), DEFAULT_COMMAND_LIMITS)
            .await
            .unwrap();

        assert!(
            !output.exit_status.is_some_and(|status| status.success()),
            "backend setup failure must not report success"
        );
        assert!(
            !marker.exists(),
            "the requested command must not run after backend setup failure"
        );
        let _ = std::fs::remove_file(&marker);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_seatbelt_policy_enforces_real_backend() {
        if !seatbelt_exists() {
            panic!("the supported macOS Seatbelt backend is unavailable");
        }

        let unique = uuid::Uuid::new_v4();
        let workspace_probe = std::env::current_dir()
            .unwrap()
            .join(format!(".mini-agent-seatbelt-write-{unique}"));
        let workspace_probe_name = workspace_probe.file_name().unwrap().to_string_lossy();
        let escape_link = std::env::current_dir()
            .unwrap()
            .join(format!(".mini-agent-seatbelt-escape-{unique}"));
        let escape_link_name = escape_link.file_name().unwrap().to_string_lossy();
        let outside_probe = std::env::current_dir()
            .unwrap()
            .parent()
            .expect("test repository must not be filesystem root")
            .join(format!(".mini-agent-seatbelt-denied-{unique}"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let loopback_port = listener.local_addr().unwrap().port();

        let script = format!(
            r#"
if touch {outside_probe} 2>/dev/null; then exit 10; fi
ln -s {outside_probe} {escape_link_name} || exit 11
if printf escaped > {escape_link_name} 2>/dev/null; then exit 12; fi
rm -f {escape_link_name}
printf workspace > {workspace_probe_name}
test "$(cat {workspace_probe_name})" = workspace || exit 13
rm -f {workspace_probe_name}
test -z "${{MINI_AGENT_SANDBOX_SECRET+x}}" || exit 14
if (exec 3<>/dev/tcp/127.0.0.1/{loopback_port}) 2>/dev/null; then exit 15; fi
printf MACOS_SEATBELT_POLICY_PASS
"#,
            outside_probe = outside_probe.to_string_lossy(),
            escape_link_name = escape_link_name,
            workspace_probe_name = workspace_probe_name,
        );

        let sandbox = Sandbox::new(true, "seatbelt");
        let mut command = sandbox.wrap_command(&script).unwrap();
        command.env("MINI_AGENT_SANDBOX_SECRET", "must-not-cross-env-i");
        let output = sandbox
            .output_built_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
            .await
            .unwrap();

        drop(listener);
        let _ = std::fs::remove_file(&outside_probe);
        let _ = std::fs::remove_file(&workspace_probe);
        let _ = std::fs::remove_file(&escape_link);
        assert_eq!(
            output.status,
            CommandStatus::Completed,
            "Seatbelt probe did not complete: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.exit_status.is_some_and(|status| status.success()),
            "Seatbelt probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"MACOS_SEATBELT_POLICY_PASS");
        let matrix = sandbox.capability_matrix();
        assert_eq!(matrix.network, "all Seatbelt network operations denied");
        assert!(matrix.filesystem_reads.contains("not claimed"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_seatbelt_setup_failure_is_fail_closed() {
        if !seatbelt_exists() {
            panic!("the supported macOS Seatbelt backend is unavailable");
        }

        let marker = std::env::current_dir().unwrap().join(format!(
            ".mini-agent-seatbelt-setup-failure-{}",
            uuid::Uuid::new_v4()
        ));
        let marker_name = marker.file_name().unwrap().to_string_lossy();
        let sandbox =
            Sandbox::new(true, "seatbelt").with_shell("/__mini_agent_missing_sandbox_shell__");
        let output = sandbox
            .output_command_with_limits(&format!("touch {marker_name}"), DEFAULT_COMMAND_LIMITS)
            .await
            .unwrap();

        assert!(
            !output.exit_status.is_some_and(|status| status.success()),
            "backend setup failure must not report success"
        );
        assert!(
            !marker.exists(),
            "the requested command must not run after backend setup failure"
        );
        let _ = std::fs::remove_file(&marker);
    }
}
