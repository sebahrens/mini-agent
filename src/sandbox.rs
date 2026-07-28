use std::collections::HashSet;
use std::process::{ExitStatus, Stdio};
#[cfg(test)]
use std::process::Output;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct Sandbox {
    enabled: bool,
    backend: String,
    shell: String,
    active_groups: Arc<Mutex<HashSet<u32>>>,
    cancelled_groups: Arc<Mutex<HashSet<u32>>>,
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

static BWRAP_AVAILABLE: OnceLock<bool> = OnceLock::new();

fn bwrap_exists() -> bool {
    *BWRAP_AVAILABLE.get_or_init(|| which_cmd("bwrap"))
}

static ZEROBOX_AVAILABLE: OnceLock<bool> = OnceLock::new();

fn zerobox_exists() -> bool {
    *ZEROBOX_AVAILABLE.get_or_init(|| which_cmd("zerobox"))
}

fn which_cmd(name: &str) -> bool {
    // Search PATH directly rather than shelling out to `which`, which may not
    // exist on minimal images (Alpine, distroless).
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
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
            active_groups: Arc::new(Mutex::new(HashSet::new())),
            cancelled_groups: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Returns true if the sandbox is enabled and the backend binary is
    /// actually available. When false, commands run unsandboxed — the UI
    /// should surface this to the user.
    pub fn is_effectively_sandboxed(&self) -> bool {
        if !self.enabled {
            return false;
        }
        if self.backend == "zerobox" {
            zerobox_exists()
        } else {
            bwrap_exists()
        }
    }

    pub fn with_shell(mut self, shell: &str) -> Self {
        if !shell.is_empty() {
            self.shell = shell.to_string();
        }
        self
    }

    pub fn wrap_command(&self, command: &str) -> Command {
        if !self.enabled {
            let mut cmd = Command::new(&self.shell);
            cmd.arg("-c").arg(command);
            configure_child_lifetime(&mut cmd);
            return cmd;
        }

        let cwd = std::env::current_dir().unwrap_or_default();

        if self.backend == "zerobox" {
            if !zerobox_exists() {
                tracing::warn!("sandbox: zerobox not found, running unsandboxed");
                let mut cmd = Command::new(&self.shell);
                cmd.arg("-c").arg(command);
                configure_child_lifetime(&mut cmd);
                return cmd;
            }
            let mut cmd = Command::new("zerobox");
            cmd.arg("--allow-write");
            cmd.arg(cwd.as_os_str());
            cmd.arg("--");
            cmd.arg(&self.shell);
            cmd.arg("-c");
            cmd.arg(command);
            configure_child_lifetime(&mut cmd);
            return cmd;
        }

        if !bwrap_exists() {
            tracing::warn!("sandbox: bwrap not found, running unsandboxed");
            let mut cmd = Command::new(&self.shell);
            cmd.arg("-c").arg(command);
            configure_child_lifetime(&mut cmd);
            return cmd;
        }

        let mut cmd = Command::new("bwrap");
        cmd.arg("--clearenv");
        for (k, v) in essential_env() {
            cmd.arg("--setenv").arg(k).arg(v);
        }
        match std::fs::canonicalize("/etc/resolv.conf") {
            Ok(target) => {
                cmd.arg("--ro-bind-try");
                cmd.arg(target);
                cmd.arg("/etc/resolv.conf");
            }
            Err(e) => {
                tracing::warn!(
                    "sandbox: no resolver file could be mounted: could not resolve /etc/resolv.conf: {}",
                    e
                );
            }
        }
        // must bind /etc/resolv.conf before /.
        cmd.args(["--ro-bind", "/", "/", "--bind"]);
        cmd.arg(cwd.as_os_str());
        cmd.arg(cwd.as_os_str());
        // Bind ~/.cache (or $XDG_CACHE_HOME) as writable after "/" bind
        if let Some(cache_dir) = dirs::cache_dir() {
            if let Err(e) = std::fs::create_dir_all(&cache_dir) {
                tracing::warn!(
                    "sandbox: failed to create cache dir {}: {e}",
                    cache_dir.display()
                );
            }
            cmd.arg("--bind");
            cmd.arg(cache_dir.as_os_str());
            cmd.arg(cache_dir.as_os_str());
        }
        cmd.args([
            "--ro-bind",
            "/sys",
            "/sys",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
        ]);
        cmd.args([
            "--unshare-ipc",
            "--unshare-pid",
            "--unshare-uts",
            "--unshare-cgroup",
            "--die-with-parent",
            &self.shell,
            "-c",
            command,
        ]);
        configure_child_lifetime(&mut cmd);
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
        let (response_tx, response_rx) = oneshot::channel();
        let sandbox = self.clone();
        let command = command.to_string();
        tokio::spawn(async move {
            sandbox
                .run_output_command(command, limits, response_tx)
                .await;
        });
        response_rx.await.map_err(|_| {
            std::io::Error::other("command output worker stopped before returning a result")
        })
    }

    async fn run_output_command(
        &self,
        command: String,
        limits: CommandLimits,
        mut response_tx: oneshot::Sender<CommandOutput>,
    ) {
        let mut cmd = self.wrap_command(&command);
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
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSH_ASKPASS",
        "GIT_ASKPASS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "EDITOR",
        "VISUAL",
        "LD_LIBRARY_PATH",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "GOPATH",
        "GOROOT",
        "VIRTUAL_ENV",
        "JAVA_HOME",
        "NODE_PATH",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
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
