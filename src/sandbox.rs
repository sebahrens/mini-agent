use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(all(windows, any(feature = "mcp", feature = "lsp")))]
use process_wrap::tokio::JobObject;
#[cfg(all(unix, any(feature = "mcp", feature = "lsp")))]
use process_wrap::tokio::ProcessGroup;
#[cfg(any(feature = "mcp", feature = "lsp"))]
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};

use crate::process_creation::TokioCommandCreationExt;

#[cfg(feature = "js")]
pub(crate) mod worker;

#[cfg(target_os = "windows")]
pub(crate) mod windows;

#[cfg(all(feature = "js", target_os = "linux"))]
pub(crate) type SandboxCommand = Command;
#[cfg(unix)]
const WORKSPACE_AUTHORITY_FD: i32 = 197;

#[derive(Debug, Clone)]
pub struct Sandbox {
    enabled: bool,
    backend: String,
    shell: String,
    shell_command_arg: String,
    disabled_reason: DisabledSandboxReason,
    working_dir: Option<PathBuf>,
    workspace_binding: Option<Arc<crate::paths::WorkspaceBinding>>,
    windows_appcontainer_read_roots: Vec<PathBuf>,
    windows_appcontainer_write_roots: Vec<PathBuf>,
    active_groups: Arc<Mutex<HashSet<u32>>>,
    cancelled_groups: Arc<Mutex<HashSet<u32>>>,
    #[cfg(test)]
    complete_process_tree_for_test: bool,
    #[cfg(test)]
    explicit_shell_audit_receipts: Option<ExplicitShellAuditReceipts>,
}

/// Transfers a direct-exec workspace service and every descendant into one
/// owned lifecycle boundary. Wrapper order is significant on Windows because
/// `JobObject` observes the inner `KillOnDrop` marker.
#[cfg(any(feature = "mcp", feature = "lsp"))]
pub(crate) fn owned_workspace_service_tree(command: Command) -> CommandWrap {
    let mut command = CommandWrap::from(command);
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);
    command
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledSandboxReason {
    UserTrustedBypass,
    UnavailableDefaultFallback,
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

/// Trust boundary selected for a human-authored `!` shell command.
///
/// This is deliberately separate from model permission policy: typing `!` is
/// the authorization event. The command still uses the configured general
/// sandbox when one was selected, and names the uncontained alternative
/// explicitly when sandboxing was disabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplicitShellBoundary {
    UserTrustedBypass,
    UnavailableDefaultFallback { backend: String },
    GeneralSandbox { backend: String },
    RequestedButUnavailable { backend: String },
}

impl ExplicitShellBoundary {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::UserTrustedBypass => "user-trusted-bypass".to_string(),
            Self::UnavailableDefaultFallback { backend } => {
                format!("unsandboxed-unavailable-default-fallback:{backend}")
            }
            Self::GeneralSandbox { backend } => format!("general-sandbox:{backend}"),
            Self::RequestedButUnavailable { backend } => {
                format!("requested-but-unavailable:{backend}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplicitShellAudit {
    /// Exact opaque script after the leading `!`, including whitespace.
    pub command: String,
    /// Process working directory captured immediately before construction.
    pub cwd: PathBuf,
    pub boundary: ExplicitShellBoundary,
}

struct OwnedExplicitShellAudit {
    metadata: ExplicitShellAudit,
    dispatch: tracing::Dispatch,
    #[cfg(test)]
    receipts: Option<ExplicitShellAuditReceipts>,
}

#[cfg(test)]
pub(crate) type ExplicitShellAuditReceipts = Arc<Mutex<Vec<(ExplicitShellAudit, CommandStatus)>>>;

impl OwnedExplicitShellAudit {
    fn capture(
        metadata: ExplicitShellAudit,
        #[cfg(test)] receipts: Option<ExplicitShellAuditReceipts>,
    ) -> Self {
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        Self {
            metadata,
            dispatch,
            #[cfg(test)]
            receipts,
        }
    }

    fn emit(&self, output: &CommandOutput) {
        tracing::dispatcher::with_default(&self.dispatch, || {
            audit_explicit_shell(&self.metadata, output);
        });
        #[cfg(test)]
        if let Some(receipts) = &self.receipts {
            receipts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((self.metadata.clone(), output.status));
        }
    }
}

pub(crate) struct ExplicitShellRun {
    pub audit: ExplicitShellAudit,
    pub output: CommandOutput,
}

impl ExplicitShellRun {
    pub(crate) fn succeeded(&self) -> bool {
        self.output.status == CommandStatus::Completed
            && self
                .output
                .exit_status
                .as_ref()
                .is_some_and(ExitStatus::success)
    }

    /// One rendering policy shared by headless and TUI callers.
    pub(crate) fn rendered_output(&self) -> String {
        let mut rendered = String::new();
        if !self.output.stdout.is_empty() {
            rendered.push_str(&String::from_utf8_lossy(&self.output.stdout));
        }
        if !self.output.stderr.is_empty() {
            if !rendered.is_empty() && !rendered.ends_with('\n') {
                rendered.push('\n');
            }
            rendered.push_str(&String::from_utf8_lossy(&self.output.stderr));
        }
        let rendered = rendered.trim().to_string();
        let boundary = self.audit.boundary.label();
        let status = match self.output.status {
            CommandStatus::Completed if self.succeeded() => None,
            CommandStatus::Completed => Some(match self.output.exit_status.as_ref() {
                Some(status) => match status.code() {
                    Some(code) => {
                        format!("explicit shell exited with status {code}; boundary={boundary}")
                    }
                    None => format!("explicit shell exited by signal; boundary={boundary}"),
                },
                None => format!("explicit shell failed; boundary={boundary}"),
            }),
            CommandStatus::TimedOut => {
                Some(format!("explicit shell timed out; boundary={boundary}"))
            }
            CommandStatus::Cancelled => {
                Some(format!("explicit shell cancelled; boundary={boundary}"))
            }
            CommandStatus::OutputLimitExceeded(limit) => Some(format!(
                "explicit shell exceeded {} output limit; boundary={boundary}",
                match limit {
                    CommandOutputLimit::Stdout => "stdout",
                    CommandOutputLimit::Stderr => "stderr",
                    CommandOutputLimit::Combined => "combined",
                }
            )),
            CommandStatus::Failed => Some(format!("explicit shell failed; boundary={boundary}")),
        };
        match (rendered.is_empty(), status) {
            (true, Some(status)) => format!("[{status}]"),
            (false, Some(status)) => format!("{rendered}\n[{status}]"),
            (_, None) => rendered,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SupportCommandLimits {
    pub timeout: std::time::Duration,
}

/// Source-free metadata that the owned support-command worker carries until
/// after terminal process-tree cleanup. Keeping this outside the awaiting
/// caller makes caller-drop cancellation auditable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SupportCommandAudit {
    utility: &'static str,
    boundary: &'static str,
}

impl SupportCommandAudit {
    pub(crate) const fn new(utility: &'static str, boundary: &'static str) -> Self {
        Self { utility, boundary }
    }
}

struct OwnedSupportCommandAudit {
    metadata: SupportCommandAudit,
    cwd: PathBuf,
    dispatch: tracing::Dispatch,
}

impl OwnedSupportCommandAudit {
    fn emit(&self, output: &CommandOutput) {
        tracing::dispatcher::with_default(&self.dispatch, || {
            audit_support_command(self, output);
        });
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

#[cfg(feature = "hooks")]
pub(crate) const HOOK_SANDBOX_READY_MARKER: &[u8] = b"MINI_AGENT_HOOK_SANDBOX_READY/1\n";
#[cfg(feature = "hooks")]
const HOOK_SANDBOX_READY_SCRIPT: &str = r#"if [ ! -x "$0" ]; then exit 126; fi
printf 'MINI_AGENT_HOOK_SANDBOX_READY/1\n' >&2
exec "$0" "$@""#;

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

#[cfg(any(target_os = "linux", all(target_os = "macos", feature = "hooks")))]
fn find_trusted_system_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .filter_map(|candidate| candidate.canonicalize().ok())
        .find(|candidate| is_trusted_system_path(candidate))
}

#[cfg(all(
    feature = "hooks",
    not(any(target_os = "linux", all(target_os = "macos", feature = "hooks")))
))]
fn find_trusted_system_executable(_name: &str) -> Option<PathBuf> {
    None
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

struct OutputCommandLifecycleGuard {
    process: ProcessGroupGuard,
    audit: Option<OwnedExplicitShellAudit>,
}

/// Gives an interactive child process group foreground terminal ownership and
/// restores mini-agent's group before the TUI resumes drawing.
#[cfg(unix)]
struct ForegroundProcessGroupGuard {
    original: nix::unistd::Pid,
}

#[cfg(unix)]
impl ForegroundProcessGroupGuard {
    fn acquire(pid: Option<u32>) -> Result<Option<Self>, String> {
        use std::io::IsTerminal;

        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            return Ok(None);
        }
        let pid =
            i32::try_from(pid.ok_or_else(|| {
                "interactive support child did not expose a process ID".to_string()
            })?)
            .map_err(|_| "interactive support child process ID exceeded pid_t".to_string())?;
        let original = nix::unistd::tcgetpgrp(&stdin)
            .map_err(|error| format!("failed to read terminal process group: {error}"))?;
        if original != nix::unistd::getpgrp() {
            return Err(
                "mini-agent does not own the foreground terminal process group".to_string(),
            );
        }
        set_terminal_foreground_group(&stdin, nix::unistd::Pid::from_raw(pid))
            .map_err(|error| format!("failed to hand terminal to support child: {error}"))?;
        Ok(Some(Self { original }))
    }
}

#[cfg(unix)]
impl Drop for ForegroundProcessGroupGuard {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        if let Err(error) = set_terminal_foreground_group(&stdin, self.original) {
            tracing::warn!("support command: failed to restore terminal process group: {error}");
        }
    }
}

/// `tcsetpgrp` from a background group would normally stop the caller with
/// SIGTTOU. Ignore that signal only around the atomic handoff, then restore
/// the prior disposition immediately.
#[cfg(unix)]
#[allow(unsafe_code)]
fn set_terminal_foreground_group(
    terminal: &std::io::Stdin,
    group: nix::unistd::Pid,
) -> nix::Result<()> {
    use nix::sys::signal::{SigHandler, Signal, signal};

    // SAFETY: SIGTTOU is changed process-wide for only the two synchronous
    // operations below and the previous disposition is restored before return.
    let previous = unsafe { signal(Signal::SIGTTOU, SigHandler::SigIgn)? };
    let result = nix::unistd::tcsetpgrp(terminal, group);
    // SAFETY: `previous` came directly from the successful signal call above.
    let restore = unsafe { signal(Signal::SIGTTOU, previous) };
    result.and(restore.map(|_| ()))
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

    fn terminate_owned_group(&self) {
        if let Some(pid) = self.pid {
            kill_process_group(pid);
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

impl OutputCommandLifecycleGuard {
    fn new(process: ProcessGroupGuard, audit: Option<OwnedExplicitShellAudit>) -> Self {
        Self { process, audit }
    }

    fn finish(&mut self, output: &CommandOutput) {
        if let Some(audit) = self.audit.take() {
            audit.emit(output);
        }
        // Keep the group accounted as active until its terminal audit has
        // been emitted. Observers can then treat a zero group count as the
        // completed lifecycle boundary, including cleanup and accounting.
        self.process.disarm();
    }
}

impl Drop for OutputCommandLifecycleGuard {
    fn drop(&mut self) {
        if self.process.pid.is_none() {
            return;
        }
        // A detached runner can itself be cancelled while its caller is being
        // torn down. Close that otherwise-unobservable lifecycle in the same
        // cleanup-before-audit-before-accounting order as the normal path.
        self.process.terminate_owned_group();
        if let Some(audit) = self.audit.take() {
            audit.emit(&CommandOutput {
                exit_status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: CommandStatus::Cancelled,
            });
        }
        self.process.disarm();
    }
}

impl Sandbox {
    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn bind_workspace_cwd(&self, command: &mut Command) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let Some(workspace) = &self.workspace_binding else {
            return Ok(());
        };
        let directory = workspace
            .try_clone_directory_file()
            .map_err(|error| format!("sandbox: failed to clone workspace handle: {error}"))?;
        let fd = directory.as_raw_fd();
        unsafe {
            command.as_std_mut().pre_exec(move || {
                let _keep_directory_alive = &directory;
                if libc::dup2(fd, WORKSPACE_AUTHORITY_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // dup2 is a no-op when the source already equals the fixed
                // descriptor, so explicitly clear CLOEXEC in both cases.
                let descriptor_flags = libc::fcntl(WORKSPACE_AUTHORITY_FD, libc::F_GETFD);
                if descriptor_flags == -1
                    || libc::fcntl(
                        WORKSPACE_AUTHORITY_FD,
                        libc::F_SETFD,
                        descriptor_flags & !libc::FD_CLOEXEC,
                    ) == -1
                    || libc::fchdir(WORKSPACE_AUTHORITY_FD) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn workspace_authority_path(&self) -> Option<PathBuf> {
        self.workspace_binding
            .as_ref()
            .map(|_| PathBuf::from(format!("/proc/self/fd/{WORKSPACE_AUTHORITY_FD}")))
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn workspace_authority_path(&self) -> Option<PathBuf> {
        self.workspace_binding
            .as_ref()
            .map(|_| PathBuf::from(format!("/dev/fd/{WORKSPACE_AUTHORITY_FD}")))
    }

    #[cfg(not(unix))]
    fn workspace_authority_path(&self) -> Option<PathBuf> {
        None
    }

    #[cfg(not(unix))]
    fn bind_workspace_cwd(&self, _command: &mut Command) -> Result<(), String> {
        Ok(())
    }

    pub fn new(enabled: bool, backend: &str) -> Self {
        let backend = if backend == "restricted-token" {
            "appcontainer"
        } else {
            backend
        };
        Sandbox {
            enabled,
            backend: backend.to_string(),
            shell: "bash".to_string(),
            shell_command_arg: "-c".to_string(),
            disabled_reason: DisabledSandboxReason::UserTrustedBypass,
            working_dir: None,
            workspace_binding: None,
            windows_appcontainer_read_roots: Vec::new(),
            windows_appcontainer_write_roots: Vec::new(),
            active_groups: Arc::new(Mutex::new(HashSet::new())),
            cancelled_groups: Arc::new(Mutex::new(HashSet::new())),
            #[cfg(test)]
            complete_process_tree_for_test: false,
            #[cfg(test)]
            explicit_shell_audit_receipts: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn observe_explicit_shell_audits(&mut self) -> ExplicitShellAuditReceipts {
        let receipts = Arc::new(Mutex::new(Vec::new()));
        self.explicit_shell_audit_receipts = Some(receipts.clone());
        receipts
    }

    pub fn with_windows_appcontainer_roots(
        mut self,
        read_roots: Vec<PathBuf>,
        write_roots: Vec<PathBuf>,
    ) -> Self {
        self.windows_appcontainer_read_roots = read_roots;
        self.windows_appcontainer_write_roots = write_roots;
        self
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
            #[cfg(target_os = "windows")]
            "appcontainer" => windows::is_available(),
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
            SandboxPolicy::RequiredAndAvailable if self.backend == "appcontainer" => {
                SandboxCapabilityMatrix {
                    backend: self.backend.clone(),
                    status: "required-and-available",
                    filesystem_reads: "explicit user-file roots plus pre-existing resources readable to ALL APPLICATION PACKAGES; read confidentiality is not claimed",
                    filesystem_writes: "the sandbox adds write access only for the canonical workspace and explicit roots; pre-existing ALL APPLICATION PACKAGES grants remain ambient",
                    process_namespace: "no namespace isolation; a creation-time bounded Job owns the complete descendant tree",
                    devices: "host-readable devices remain visible; no device isolation is claimed",
                    environment: "cleared, then populated from a narrow non-credential Windows allow-list",
                    network: "AppContainer network capabilities are empty; IP network access is denied by default",
                    requested_network_policy: "default-deny AppContainer with no network capability",
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
        (cfg!(target_os = "linux") && self.backend == "bwrap"
            || cfg!(target_os = "windows") && self.backend == "appcontainer")
            && self.policy() == SandboxPolicy::RequiredAndAvailable
    }

    #[cfg(test)]
    pub(crate) fn with_complete_process_tree_for_test(mut self) -> Self {
        self.complete_process_tree_for_test = true;
        self
    }

    pub(crate) fn explicit_shell_boundary(&self) -> ExplicitShellBoundary {
        match self.policy() {
            SandboxPolicy::Disabled => match self.disabled_reason {
                DisabledSandboxReason::UserTrustedBypass => {
                    ExplicitShellBoundary::UserTrustedBypass
                }
                DisabledSandboxReason::UnavailableDefaultFallback => {
                    ExplicitShellBoundary::UnavailableDefaultFallback {
                        backend: self.backend.clone(),
                    }
                }
            },
            SandboxPolicy::RequiredAndAvailable => ExplicitShellBoundary::GeneralSandbox {
                backend: self.backend.clone(),
            },
            SandboxPolicy::RequiredButUnavailable => {
                ExplicitShellBoundary::RequestedButUnavailable {
                    backend: self.backend.clone(),
                }
            }
        }
    }

    pub fn with_shell(self, shell: &str) -> Self {
        #[cfg(target_os = "windows")]
        return self.with_shell_command_arg(shell, "-Command");
        #[cfg(not(target_os = "windows"))]
        self.with_shell_command_arg(shell, "-c")
    }

    /// Preserve why startup disabled sandboxing after an unavailable backend
    /// was inherited from defaults. This is unsandboxed, but it is not the
    /// operator's explicit `--no-sandbox` trusted bypass.
    pub(crate) fn with_unavailable_default_fallback(mut self) -> Self {
        if !self.enabled {
            self.disabled_reason = DisabledSandboxReason::UnavailableDefaultFallback;
        }
        self
    }

    /// Set the workspace used as the child CWD and sandbox write root.
    pub(crate) fn with_working_dir(mut self, working_dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(working_dir.into());
        self
    }

    fn working_dir(&self) -> std::io::Result<PathBuf> {
        self.working_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
    }

    /// Bind every command launched by this sandbox to an explicit immutable
    /// workspace. This is per-sandbox state; it never mutates process CWD.
    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        self.working_dir = Some(workspace.root().to_path_buf());
        self.workspace_binding = Some(workspace);
        self
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
        if let Some(workspace) = &self.workspace_binding {
            workspace.validate()?;
        }
        if !self.supports_immutable_executable_snapshot() {
            return Err("sandbox backend cannot bind an immutable executable snapshot".to_string());
        }
        let cwd = self
            .working_dir()
            .map_err(|error| format!("sandbox: failed to resolve working directory: {error}"))?;
        let cwd = self
            .workspace_authority_path()
            .map(Ok)
            .unwrap_or_else(|| canonical_non_root(&cwd, "working directory"))?;
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
        if let Some(workspace) = &self.workspace_binding {
            workspace.validate()?;
        }
        let requested_cwd = self
            .working_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .map_err(|error| format!("sandbox: failed to resolve working directory: {error}"))?;
        match self.policy() {
            SandboxPolicy::Disabled => {
                let mut cmd = Command::new(&self.shell);
                cmd.arg(&self.shell_command_arg).arg(command);
                cmd.current_dir(&requested_cwd);
                configure_child_lifetime(&mut cmd);
                self.bind_workspace_cwd(&mut cmd)?;
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
        let cwd = self
            .workspace_authority_path()
            .map(Ok)
            .unwrap_or_else(|| canonical_non_root(&requested_cwd, "working directory"))?;

        #[cfg(target_os = "windows")]
        if self.backend == "appcontainer" {
            let paths = crate::paths::process_paths()
                .map_err(|error| format!("sandbox: application paths are unavailable: {error}"))?;
            std::fs::create_dir_all(&paths.cache_dir).map_err(|error| {
                format!(
                    "sandbox: failed to create application cache {}: {error}",
                    paths.cache_dir.display()
                )
            })?;
            let cache_dir = canonical_non_root(&paths.cache_dir, "application cache")?;
            return windows::build_shell_helper(
                &self.shell,
                &self.shell_command_arg,
                command,
                &cwd,
                &cache_dir,
                &self.windows_appcontainer_read_roots,
                &self.windows_appcontainer_write_roots,
            );
        }

        if self.backend == "zerobox" {
            if self.workspace_binding.is_some() && cfg!(unix) {
                return Err(
                    "sandbox backend 'zerobox' cannot consume an ACP workspace handle".to_string(),
                );
            }
            let mut cmd = Command::new("zerobox");
            cmd.arg("--allow-write");
            cmd.arg(cwd.as_os_str());
            cmd.arg("--");
            cmd.arg(&self.shell);
            cmd.arg(&self.shell_command_arg);
            cmd.arg(command);
            cmd.current_dir(&cwd);
            configure_child_lifetime(&mut cmd);
            self.bind_workspace_cwd(&mut cmd)?;
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
            let mut command = self.build_seatbelt_command(seatbelt, command, &cwd, &cache_dir)?;
            if self.workspace_binding.is_some() && cfg!(unix) {
                command.current_dir("/");
            }
            self.bind_workspace_cwd(&mut command)?;
            return Ok(command);
        }

        let bwrap = bwrap_path().ok_or_else(|| {
            "sandbox backend 'bwrap' is not a trusted system executable — refusing to run unsandboxed"
                .to_string()
        })?;
        let mut command = self.build_bwrap_command(bwrap, command, &cwd, &cache_dir);
        self.bind_workspace_cwd(&mut command)?;
        Ok(command)
    }

    /// Builds a direct-exec command under the general workspace policy.
    ///
    /// Unlike [`Self::wrap_command`], no shell parses `program` or `args`.
    /// The child always starts in `cwd` with a cleared environment containing
    /// only the standard non-credential allow-list plus `explicit_env`.
    #[cfg(feature = "hooks")]
    pub(crate) fn wrap_direct_command(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
        explicit_env: &std::collections::BTreeMap<String, String>,
    ) -> Result<Command, String> {
        let cwd = canonical_non_root(cwd, "hook project directory")?;
        match self.policy() {
            SandboxPolicy::Disabled => {
                let mut cmd = Command::new(program);
                cmd.args(args).current_dir(&cwd).env_clear();
                for (key, value) in essential_env() {
                    cmd.env(key, value);
                }
                cmd.envs(explicit_env);
                configure_child_lifetime(&mut cmd);
                return Ok(cmd);
            }
            SandboxPolicy::RequiredButUnavailable => {
                return Err(format!(
                    "sandbox backend '{}' is not available — refusing to run hook unsandboxed (requested-but-unavailable)",
                    self.backend
                ));
            }
            SandboxPolicy::RequiredAndAvailable => {}
        }

        if self.backend == "zerobox" {
            let zerobox = find_trusted_system_executable("zerobox").ok_or_else(|| {
                "sandbox backend 'zerobox' is not a trusted system executable — refusing to run hook unsandboxed"
                    .to_string()
            })?;
            let readiness_shell = hook_readiness_shell()?;
            let mut cmd = Command::new(zerobox);
            cmd.arg("--allow-write")
                .arg(&cwd)
                .arg("--")
                .arg(readiness_shell)
                .arg("-c")
                .arg(HOOK_SANDBOX_READY_SCRIPT)
                .arg(program)
                .args(args)
                .current_dir(&cwd)
                .env_clear();
            for (key, value) in essential_env() {
                cmd.env(key, value);
            }
            cmd.envs(explicit_env);
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
            let readiness_shell = hook_readiness_shell()?;
            let seatbelt = seatbelt_path().ok_or_else(|| {
                "sandbox backend 'seatbelt' is not a trusted system executable — refusing to run hook unsandboxed"
                    .to_string()
            })?;
            let workspace = seatbelt_string_literal(&cwd, "hook project directory")?;
            let cache = seatbelt_string_literal(&cache_dir, "application cache")?;
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
            cmd.arg("-p")
                .arg(profile)
                .arg(readiness_shell)
                .arg("-c")
                .arg(HOOK_SANDBOX_READY_SCRIPT)
                .arg(program)
                .args(args);
            cmd.current_dir(&cwd).env_clear();
            for (key, value) in essential_env() {
                cmd.env(key, value);
            }
            cmd.envs(explicit_env).env("TMPDIR", "/private/tmp");
            configure_child_lifetime(&mut cmd);
            return Ok(cmd);
        }

        let bwrap = bwrap_path().ok_or_else(|| {
            "sandbox backend 'bwrap' is not a trusted system executable — refusing to run hook unsandboxed"
                .to_string()
        })?;
        let readiness_shell = hook_readiness_shell()?;
        let mut cmd = Command::new(bwrap);
        cmd.current_dir(&cwd).env_clear();
        for (key, value) in essential_env() {
            cmd.env(key, value);
        }
        cmd.envs(explicit_env).env("TMPDIR", "/tmp");
        for path in ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/nix"] {
            cmd.args(["--ro-bind-try", path, path]);
        }
        cmd.args(["--dir", "/etc"]);
        for path in ["/etc/localtime", "/etc/ld.so.cache"] {
            cmd.args(["--ro-bind-try", path, path]);
        }
        cmd.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"])
            .arg("--bind")
            .arg(&cwd)
            .arg(&cwd)
            .arg("--bind")
            .arg(&cache_dir)
            .arg(&cache_dir)
            .args([
                "--unshare-user",
                "--unshare-ipc",
                "--unshare-pid",
                "--unshare-net",
                "--unshare-uts",
                "--unshare-cgroup",
                "--remount-ro",
                "/",
                "--chdir",
            ])
            .arg(&cwd)
            .args(["--die-with-parent", "--"])
            .arg(readiness_shell)
            .arg("-c")
            .arg(HOOK_SANDBOX_READY_SCRIPT)
            .arg(program)
            .args(args);
        configure_child_lifetime(&mut cmd);
        Ok(cmd)
    }

    /// Build a direct-exec workspace service boundary.
    ///
    /// Unlike [`Sandbox::wrap_command`], this profile never inserts a shell and
    /// accepts only an already-resolved executable plus an ordered argv. It is
    /// intended for human-configured, workspace-capable long-lived services
    /// such as MCP stdio or LSP servers, not for the broker-only JS worker
    /// profile.
    /// The supplied environment is the complete delegated environment.
    pub(crate) fn wrap_workspace_service(
        &self,
        program: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(OsString, OsString)],
        deny_network: bool,
    ) -> Result<Command, String> {
        match self.policy() {
            SandboxPolicy::Disabled => {
                return Err("workspace-service sandbox must be explicitly requested".to_string());
            }
            SandboxPolicy::RequiredButUnavailable => {
                return Err(format!(
                    "sandbox backend '{}' is not available — refusing workspace-service launch (requested-but-unavailable)",
                    self.backend
                ));
            }
            SandboxPolicy::RequiredAndAvailable => {}
        }

        let cwd = canonical_non_root(cwd, "workspace-service working directory")?;
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
                "sandbox backend 'seatbelt' is not a trusted system executable — refusing workspace-service launch"
                    .to_string()
            })?;
            let workspace = seatbelt_string_literal(&cwd, "workspace-service working directory")?;
            let cache = seatbelt_string_literal(&cache_dir, "application cache")?;
            let network_rule = if deny_network {
                "(deny network*)"
            } else {
                "(allow network*)"
            };
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
{network_rule}"#
            );
            let mut cmd = Command::new(seatbelt);
            cmd.env_clear();
            cmd.arg("-p").arg(profile).arg("/usr/bin/env").arg("-i");
            for (key, value) in env {
                let key = key.to_str().ok_or_else(|| {
                    "workspace-service environment name is not valid UTF-8 for Seatbelt".to_string()
                })?;
                if key.contains('=') {
                    return Err(
                        "workspace-service environment name must not contain '='".to_string()
                    );
                }
                let mut assignment = OsString::from(key);
                assignment.push("=");
                assignment.push(value);
                cmd.arg(assignment);
            }
            cmd.arg(program).args(args).current_dir(&cwd);
            configure_child_lifetime(&mut cmd);
            return Ok(cmd);
        }

        if self.backend == "zerobox" {
            if deny_network {
                return Err(
                    "sandbox backend 'zerobox' cannot truthfully enforce workspace-service network denial"
                        .to_string(),
                );
            }
            let mut cmd = Command::new("zerobox");
            cmd.env_clear();
            cmd.envs(env.iter().cloned());
            cmd.arg("--allow-write")
                .arg(&cwd)
                .arg("--")
                .arg(program)
                .args(args)
                .current_dir(&cwd);
            configure_child_lifetime(&mut cmd);
            return Ok(cmd);
        }

        let bwrap = bwrap_path().ok_or_else(|| {
            "sandbox backend 'bwrap' is not a trusted system executable — refusing workspace-service launch"
                .to_string()
        })?;
        let mut cmd = Command::new(bwrap);
        cmd.env_clear();
        cmd.arg("--clearenv");
        for (key, value) in env {
            cmd.arg("--setenv").arg(key).arg(value);
        }
        for path in [
            "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/nix", "/opt",
        ] {
            cmd.args(["--ro-bind-try", path, path]);
        }
        cmd.args(["--dir", "/etc"]);
        for path in ["/etc/localtime", "/etc/ld.so.cache"] {
            cmd.args(["--ro-bind-try", path, path]);
        }
        cmd.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        cmd.arg("--bind").arg(&cwd).arg(&cwd);
        cmd.arg("--bind").arg(&cache_dir).arg(&cache_dir);
        cmd.args([
            "--unshare-user",
            "--unshare-ipc",
            "--unshare-pid",
            "--unshare-uts",
            "--unshare-cgroup",
        ]);
        if deny_network {
            cmd.arg("--unshare-net");
        }
        cmd.arg("--remount-ro")
            .arg("/")
            .arg("--chdir")
            .arg(&cwd)
            .arg("--die-with-parent")
            .arg("--")
            .arg(program)
            .args(args);
        configure_child_lifetime(&mut cmd);
        Ok(cmd)
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn wrap_direct_command(
        &self,
        program: &Path,
        arguments: &[String],
    ) -> Result<Command, String> {
        if self.policy() != SandboxPolicy::RequiredAndAvailable || self.backend != "appcontainer" {
            return Err("Windows direct process launch requires the AppContainer sandbox".into());
        }
        let cwd = canonical_non_root(
            &std::env::current_dir().map_err(|error| {
                format!("sandbox: failed to resolve working directory: {error}")
            })?,
            "working directory",
        )?;
        let paths = crate::paths::process_paths()
            .map_err(|error| format!("sandbox: application paths are unavailable: {error}"))?;
        std::fs::create_dir_all(&paths.cache_dir).map_err(|error| {
            format!(
                "sandbox: failed to create application cache {}: {error}",
                paths.cache_dir.display()
            )
        })?;
        let cache_dir = canonical_non_root(&paths.cache_dir, "application cache")?;
        windows::build_direct_helper(
            program,
            arguments,
            &cwd,
            &cache_dir,
            &self.windows_appcontainer_read_roots,
            &self.windows_appcontainer_write_roots,
        )
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
        cmd.current_dir(cwd);
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
        let sandbox_cwd = self.sandbox_cwd(cwd);
        append_bwrap_isolation(&mut cmd, &sandbox_cwd);
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
        let sandbox_cwd = self.sandbox_cwd(cwd);
        append_bwrap_isolation(&mut cmd, &sandbox_cwd);
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
        let sandbox_cwd = self.sandbox_cwd(cwd);
        if sandbox_cwd != cwd {
            cmd.args(["--dir", "/workspace"]);
        }
        cmd.arg("--bind").arg(cwd).arg(&sandbox_cwd);
        cmd.arg("--bind").arg(cache_dir).arg(cache_dir);
        cmd
    }

    fn sandbox_cwd(&self, cwd: &Path) -> PathBuf {
        if self.workspace_binding.is_some() && cfg!(unix) {
            PathBuf::from("/workspace")
        } else {
            cwd.to_path_buf()
        }
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

    /// Run one human-authored explicit shell interaction through the selected
    /// general process boundary and the common bounded output worker.
    ///
    /// `interaction` includes the leading `!`; everything after it is passed
    /// to the configured shell exactly as authored. Whitespace is inspected
    /// only to reject an empty command and is never normalized for execution.
    pub(crate) async fn run_explicit_shell(
        &self,
        interaction: &str,
        limits: CommandLimits,
        cancellation: Option<&CommandCancellation>,
    ) -> std::io::Result<ExplicitShellRun> {
        let command = interaction.strip_prefix('!').ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "explicit shell interaction must start with '!'",
            )
        })?;
        if command.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty command after '!'",
            ));
        }

        let cwd = self.working_dir()?;
        let boundary = self.explicit_shell_boundary();
        let audit = ExplicitShellAudit {
            command: command.to_string(),
            cwd: cwd.clone(),
            boundary,
        };
        let mut cmd = match self.wrap_command(command) {
            Ok(cmd) => cmd,
            Err(error) => {
                let output = CommandOutput {
                    exit_status: None,
                    stdout: Vec::new(),
                    stderr: error.into_bytes(),
                    status: CommandStatus::Failed,
                };
                audit_explicit_shell(&audit, &output);
                let run = ExplicitShellRun { audit, output };
                return Ok(run);
            }
        };
        // Apply an explicit snapshot even for the intentional uncontained
        // branch, which would otherwise inherit cwd only at the later spawn.
        cmd.current_dir(&cwd);
        let cancellation = cancellation.map(CommandCancellation::subscribe);
        let output = self
            .output_built_command_with_limits_scoped(
                cmd,
                limits,
                cancellation,
                Some(OwnedExplicitShellAudit::capture(
                    audit.clone(),
                    #[cfg(test)]
                    self.explicit_shell_audit_receipts.clone(),
                )),
                None,
            )
            .await?;
        Ok(ExplicitShellRun { audit, output })
    }

    /// Run an interactive support utility with inherited stdio but the same
    /// process-group ownership and finite-lifetime guarantees as captured
    /// commands. This is not a shell and does not use model permissions.
    pub(crate) async fn status_support_command(
        &self,
        mut cmd: Command,
        limits: SupportCommandLimits,
        audit: SupportCommandAudit,
    ) -> std::io::Result<CommandOutput> {
        let cwd = self.working_dir()?;
        cmd.current_dir(&cwd);
        configure_child_lifetime(&mut cmd);
        let (response_tx, response_rx) = oneshot::channel();
        let sandbox = self.clone();
        let audit = OwnedSupportCommandAudit {
            metadata: audit,
            cwd,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
        };
        tokio::spawn(async move {
            sandbox
                .run_built_status_command(cmd, limits, audit, response_tx)
                .await;
        });
        response_rx.await.map_err(|_| {
            std::io::Error::other("support command worker stopped before returning a result")
        })
    }

    /// Captured companion for short support-utility probes such as
    /// `lazygit --version`.
    pub(crate) async fn output_support_command(
        &self,
        mut cmd: Command,
        limits: CommandLimits,
    ) -> std::io::Result<CommandOutput> {
        let cwd = self.working_dir()?;
        cmd.current_dir(cwd);
        configure_child_lifetime(&mut cmd);
        self.output_built_command_with_limits(cmd, limits).await
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
        self.output_built_command_with_limits_scoped(cmd, limits, cancellation, None, None)
            .await
    }

    pub(crate) async fn output_built_command_with_limits(
        &self,
        cmd: Command,
        limits: CommandLimits,
    ) -> std::io::Result<CommandOutput> {
        self.output_built_command_with_limits_scoped(cmd, limits, None, None, None)
            .await
    }

    #[cfg(test)]
    pub async fn output_command(&self, command: &str) -> std::io::Result<std::process::Output> {
        let output = self
            .output_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
            .await?;
        if output.status != CommandStatus::Completed {
            return Err(std::io::Error::other("command did not complete"));
        }
        let status = output
            .exit_status
            .ok_or_else(|| std::io::Error::other("completed command had no exit status"))?;
        Ok(std::process::Output {
            status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    pub(crate) async fn output_built_command_with_input_and_limits(
        &self,
        cmd: Command,
        input: Vec<u8>,
        limits: CommandLimits,
    ) -> std::io::Result<CommandOutput> {
        self.output_built_command_with_limits_scoped(cmd, limits, None, None, Some(input))
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
        self.output_built_command_with_limits_scoped(
            cmd,
            limits,
            Some(cancellation.subscribe()),
            None,
            None,
        )
        .await
    }

    async fn output_built_command_with_limits_scoped(
        &self,
        cmd: Command,
        limits: CommandLimits,
        cancellation: Option<watch::Receiver<bool>>,
        audit: Option<OwnedExplicitShellAudit>,
        input: Option<Vec<u8>>,
    ) -> std::io::Result<CommandOutput> {
        let (response_tx, response_rx) = oneshot::channel();
        let sandbox = self.clone();
        std::mem::drop(crate::agent::runner::spawn_async_scoped(async move {
            sandbox
                .run_built_output_command(cmd, limits, cancellation, audit, input, response_tx)
                .await;
        }));
        response_rx.await.map_err(|_| {
            std::io::Error::other("command output worker stopped before returning a result")
        })
    }

    async fn run_built_output_command(
        &self,
        mut cmd: Command,
        limits: CommandLimits,
        mut cancellation: Option<watch::Receiver<bool>>,
        audit: Option<OwnedExplicitShellAudit>,
        input: Option<Vec<u8>>,
        mut response_tx: oneshot::Sender<CommandOutput>,
    ) {
        if cancellation
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow())
        {
            let output = CommandOutput {
                exit_status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: CommandStatus::Cancelled,
            };
            if let Some(audit) = &audit {
                audit.emit(&output);
            }
            let _ = response_tx.send(output);
            return;
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = match cmd.spawn_guarded() {
            Ok(child) => child,
            Err(error) => {
                let output = CommandOutput {
                    exit_status: None,
                    stdout: Vec::new(),
                    stderr: format!("failed to spawn command: {error}").into_bytes(),
                    status: CommandStatus::Failed,
                };
                if let Some(audit) = &audit {
                    audit.emit(&output);
                }
                let _ = response_tx.send(output);
                return;
            }
        };
        let pid = child.id();
        if let Some(input) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
            });
        }
        let mut lifecycle = OutputCommandLifecycleGuard::new(
            ProcessGroupGuard::new(child.id(), self.active_groups.clone()),
            audit,
        );
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
                // the command open or continue running in the background. The
                // Windows AppContainer helper has already closed its Job
                // before it exits, so there is no live tree left to terminate.
                if let Some(pid) = pid
                    && !(cfg!(windows) && self.backend == "appcontainer")
                {
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
        self.take_cancelled(pid);

        let mut captured = captured.lock().unwrap_or_else(|e| e.into_inner());
        let output = CommandOutput {
            exit_status,
            stdout: std::mem::take(&mut captured.stdout),
            stderr: std::mem::take(&mut captured.stderr),
            status: command_status,
        };
        drop(captured);
        lifecycle.finish(&output);
        let _ = response_tx.send(output);
    }

    async fn run_built_status_command(
        &self,
        mut cmd: Command,
        limits: SupportCommandLimits,
        audit: OwnedSupportCommandAudit,
        mut response_tx: oneshot::Sender<CommandOutput>,
    ) {
        let mut child = match cmd.spawn_guarded() {
            Ok(child) => child,
            Err(error) => {
                let output = CommandOutput {
                    exit_status: None,
                    stdout: Vec::new(),
                    stderr: format!("failed to spawn support command: {error}").into_bytes(),
                    status: CommandStatus::Failed,
                };
                audit.emit(&output);
                let _ = response_tx.send(output);
                return;
            }
        };
        let pid = child.id();
        let mut guard = ProcessGroupGuard::new(pid, self.active_groups.clone());
        #[cfg(unix)]
        let foreground = match ForegroundProcessGroupGuard::acquire(pid) {
            Ok(foreground) => foreground,
            Err(error) => {
                terminate_and_reap(&mut child, pid).await;
                guard.disarm();
                let output = CommandOutput {
                    exit_status: None,
                    stdout: Vec::new(),
                    stderr: error.into_bytes(),
                    status: CommandStatus::Failed,
                };
                audit.emit(&output);
                let _ = response_tx.send(output);
                return;
            }
        };
        let termination = tokio::select! {
            biased;
            status = child.wait() => CommandTermination::Exited(status),
            _ = tokio::time::sleep(limits.timeout) => CommandTermination::TimedOut,
            _ = response_tx.closed() => CommandTermination::Cancelled,
        };
        let (exit_status, status) = match termination {
            CommandTermination::Exited(Ok(status)) => {
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
                tracing::warn!("support command: failed to wait: {error}");
                terminate_and_reap(&mut child, pid).await;
                (None, CommandStatus::Failed)
            }
            CommandTermination::TimedOut => {
                terminate_and_reap(&mut child, pid).await;
                (None, CommandStatus::TimedOut)
            }
            CommandTermination::Cancelled => {
                terminate_and_reap(&mut child, pid).await;
                (None, CommandStatus::Cancelled)
            }
            CommandTermination::ReaderError(_) => unreachable!("support commands have no readers"),
        };
        guard.disarm();
        self.take_cancelled(pid);
        #[cfg(unix)]
        drop(foreground);
        let output = CommandOutput {
            exit_status,
            stdout: Vec::new(),
            stderr: Vec::new(),
            status,
        };
        audit.emit(&output);
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

#[cfg(feature = "hooks")]
fn hook_readiness_shell() -> Result<PathBuf, String> {
    find_trusted_system_executable("sh").ok_or_else(|| {
        "sandbox: no trusted system `sh` is available for the hook readiness launcher".to_string()
    })
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

fn audit_explicit_shell(audit: &ExplicitShellAudit, output: &CommandOutput) {
    let succeeded = output.status == CommandStatus::Completed
        && output.exit_status.as_ref().is_some_and(ExitStatus::success);
    let outcome = match output.status {
        CommandStatus::Completed if succeeded => "success",
        CommandStatus::Completed => "nonzero",
        CommandStatus::TimedOut => "timeout",
        CommandStatus::Cancelled => "cancelled",
        CommandStatus::OutputLimitExceeded(_) => "output-limit",
        CommandStatus::Failed => "failed",
    };
    if succeeded {
        tracing::info!(
            target: "zerostack::audit::explicit_shell",
            trust_class = "TC-EXPLICIT-USER-SHELL",
            command = audit.command,
            cwd = %audit.cwd.display(),
            boundary = audit.boundary.label(),
            outcome,
            "explicit user shell ended after process cleanup"
        );
    } else {
        tracing::warn!(
            target: "zerostack::audit::explicit_shell",
            trust_class = "TC-EXPLICIT-USER-SHELL",
            command = audit.command,
            cwd = %audit.cwd.display(),
            boundary = audit.boundary.label(),
            outcome,
            "explicit user shell ended after process cleanup"
        );
    }
}

fn audit_support_command(audit: &OwnedSupportCommandAudit, output: &CommandOutput) {
    let succeeded = output.status == CommandStatus::Completed
        && output.exit_status.as_ref().is_some_and(ExitStatus::success);
    let outcome = match output.status {
        CommandStatus::Completed if succeeded => "success",
        CommandStatus::Completed => "nonzero",
        CommandStatus::TimedOut => "timeout",
        CommandStatus::Cancelled => "cancelled",
        CommandStatus::OutputLimitExceeded(_) => "output-limit",
        CommandStatus::Failed => "failed",
    };
    if succeeded {
        tracing::info!(
            target: "zerostack::audit::support_utility",
            trust_class = "TC-SUPPORT-UTILITY",
            utility = audit.metadata.utility,
            cwd = %audit.cwd.display(),
            boundary = audit.metadata.boundary,
            outcome,
            "support utility completed after process cleanup"
        );
    } else {
        tracing::warn!(
            target: "zerostack::audit::support_utility",
            trust_class = "TC-SUPPORT-UTILITY",
            utility = audit.metadata.utility,
            cwd = %audit.cwd.display(),
            boundary = audit.metadata.boundary,
            outcome,
            "support utility ended after process cleanup"
        );
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
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let Ok(group) = i32::try_from(pid) else {
            tracing::warn!("sandbox: child process group ID {pid} is outside pid_t range");
            return;
        };
        if group <= 0 {
            tracing::warn!("sandbox: refusing invalid child process group ID {pid}");
            return;
        }
        for (signal, label) in [(Signal::SIGTERM, "SIGTERM"), (Signal::SIGKILL, "SIGKILL")] {
            if let Err(error) = killpg(Pid::from_raw(group), signal)
                && error != Errno::ESRCH
            {
                tracing::warn!(
                    "sandbox: failed to send {label} to child process group {pid}: {error}"
                );
            }
        }
    }
    #[cfg(windows)]
    windows::terminate_helper(pid);
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
        "TMPDIR",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
        "USERNAME",
        "USERPROFILE",
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

    #[cfg(unix)]
    const PATHLESS_KILLPG_CHILD: &str = "MINI_AGENT_PATHLESS_KILLPG_CHILD";

    fn disabled() -> Sandbox {
        Sandbox::new(false, "bwrap")
    }

    fn unavailable() -> Sandbox {
        Sandbox::new(true, "__no_such_backend_exists__")
    }

    #[cfg(unix)]
    #[test]
    fn kill_process_group_does_not_require_path() {
        use std::os::unix::process::CommandExt as _;

        if std::env::var_os(PATHLESS_KILLPG_CHILD).is_some() {
            let mut child_command = std::process::Command::new("/bin/sh");
            child_command
                .args(["-c", "trap '' TERM; exec /bin/sleep 60"])
                .process_group(0);
            let mut child = child_command.spawn().unwrap();
            let pid = child.id();

            kill_process_group(pid);
            for _ in 0..100 {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("PATH-independent process-group cleanup did not terminate child {pid}");
        }

        let current_exe = std::env::current_exe().unwrap();
        let mut isolated = std::process::Command::new(current_exe);
        isolated
            .args([
                "--exact",
                "sandbox::sandbox_tests::kill_process_group_does_not_require_path",
                "--nocapture",
            ])
            .env_clear()
            .env(PATHLESS_KILLPG_CHILD, "1");
        let status = isolated.status().unwrap();
        assert!(
            status.success(),
            "process-group cleanup must work when PATH and the ambient environment are absent"
        );
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
    fn windows_appcontainer_source_covers_general_process_security_contract() {
        let source = include_str!("sandbox/windows.rs").replace("\r\n", "\n");
        for required in [
            "CreateAppContainerProfile",
            "DeriveAppContainerSidFromAppContainerName",
            "DeleteAppContainerProfile",
            "PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES",
            "SECURITY_CAPABILITIES",
            "CapabilityCount: 0",
            "TokenCapabilities",
            "tcp_attempt_denied(\"127.0.0.1:9\")",
            "tcp_attempt_denied(\"1.1.1.1:9\")",
            "udp_attempt_denied(\"127.0.0.1:9\")",
            "udp_attempt_denied(\"1.1.1.1:9\")",
            "tcp_attempt_denied(\"[::1]:9\")",
            "udp_attempt_denied(\"[::1]:9\")",
            "grant_read_root",
            "grant_write_root",
            "sweep_stale_profiles",
            "MAX_STALE_PROFILE_JOURNALS",
            "terminate_and_drain_job",
            "OpenJobObjectW",
            "JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE",
            "STALE_JOB_CLEANUP_EXIT_CODE",
            "job_name",
            "wait_for_stale_job_quiescence",
            "active_job_processes",
            "wait_for_exact_probe_file",
            "creation-time Job did not contain exactly its suspended target",
            "CREATE_BREAKAWAY_FROM_JOB",
            "configure_job_ui_restrictions(job)",
            "JobObjectBasicAccountingInformation",
            "ActiveProcesses == 0",
            "reject_remote_access_path",
            "GetDriveTypeW",
            "DRIVE_REMOTE",
            "SetEvent(omitted_handle as HANDLE)",
            "GetAppContainerFolderPath",
            "NetworkIsolationGetAppContainerConfig",
            "FILE_GENERIC_READ",
            "uuid::Uuid::new_v4()",
            "GetSecurityInfo",
            "SetSecurityInfo",
            "InitializeSecurityDescriptor",
            "SetSecurityDescriptorOwner",
            "SetSecurityDescriptorDacl",
            "Global\\\\mini-agent-general-job-",
            "CreateMutexW",
            "WAIT_ABANDONED_0",
            "AclMutationGuard::acquire()?",
            "windows_file_link_count",
            "program_proof",
            "MAX_REQUEST_FEEDERS",
            "CreateDesktopW",
            "GetThreadDesktop",
            "let desktop = private_desktop(grants.sid())?",
            "startup.StartupInfo.lpDesktop",
            "ImpersonateLoggedOnUser",
            "AssignProcessToJobObject(job.raw(), process.raw())",
            "IsProcessInJob",
            "QueryInformationJobObject",
            "if let Err(error) = verify_job_membership_and_limits(job, &process)",
            "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
            "JOB_OBJECT_LIMIT_ACTIVE_PROCESS",
            "JOB_OBJECT_LIMIT_PROCESS_MEMORY",
            "GENERAL_JOB_UI_RESTRICTIONS",
            "JOB_OBJECT_UILIMIT_ALL",
            "env_clear()",
            "reject_reparse_components",
            "outside_write=denied",
            "hardlink=denied",
            "unique_profile_crash=pass",
            "authority_escape=denied",
            "authority probe failed: status=",
            "HELPER_FAILURE_STATUS_BASE",
            "HELPER_STAGE_REGULAR_TOKEN_ACCESS",
            "helper_failure_status()",
            "AUTHORITY_DESCENDANT_SPAWN_FAILED",
            "AUTHORITY_LOOPBACK_QUERY_ERROR",
            "bounded_pipe=pass",
            "acl_serialization=pass",
            "parent_death_job=pass",
            "private_desktop=pass",
            "omitted_handle=denied",
            "descendant=contained",
            "control_journal=denied",
            "configured_tool=pass",
            "breakaway=denied",
            "configured_read_roots",
            "configured_write_roots",
            "attest_cleanup_proof(&crash_proof",
            "attest_tree_has_no_explicit_sid",
            "OWNER_SECURITY_INFORMATION",
            "PROTECTED_DACL_SECURITY_INFORMATION",
            "network=denied",
            "registry=not_isolated",
            "CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT",
            "process_token_is_regular_appcontainer(process.raw())",
            "ResumeThread(thread.raw())",
            "regular AppContainer ALL_APPLICATION_PACKAGES access",
            "TokenIsAppContainer",
            "appcontainer=regular",
        ] {
            assert!(
                source.contains(required),
                "missing Windows contract: {required}"
            );
        }
        assert!(!source.contains("PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY"));
        assert!(!source.contains("PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT"));
        assert!(!source.contains("GetHandleInformation(omitted_handle as HANDLE"));
        assert!(!source.contains("run_authority_probe(args).unwrap_or(97)"));
        assert!(!source.contains("run_descendant_probe(args).unwrap_or(98)"));
        assert!(!source.contains("TokenIsLessPrivilegedAppContainer"));
        assert!(!source.contains("PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY"));
        assert!(!source.contains("PROCESS_CREATION_CHILD_PROCESS_OVERRIDE"));
        assert!(
            source.contains("const GENERAL_JOB_UI_RESTRICTIONS: u32 = JOB_OBJECT_UILIMIT_ALL;")
        );
        assert!(
            source.contains(".stdin(Stdio::from"),
            "helper requests must use inherited stdin, not argv/env/temp files"
        );
        assert!(!source.contains("GetNamedSecurityInfo"));
        assert!(!source.contains("SetNamedSecurityInfo"));
        assert!(!source.contains("S-1-5-21-3380456832"));
        assert!(!source.contains("SECURITY_CAPABILITY_INTERNET_CLIENT"));
        assert!(!source.contains("PROCESS_CREATION_CHILD_PROCESS_RESTRICTED"));
        assert!(!source.contains("DESKTOP_WRITEOBJECTS"));
        let root_policy = source
            .split("fn collect_read_roots(")
            .nth(1)
            .and_then(|source| source.split("fn canonicalize_access_roots").next())
            .expect("explicit root-policy implementation missing");
        assert!(!root_policy.contains("std::env::split_paths"));
        assert!(!root_policy.contains("CARGO_HOME"));
        assert!(!root_policy.contains("RUSTUP_HOME"));
        assert!(source.contains("build_helper_with_ready_and_roots"));
        assert!(source.contains("configured writable root overlaps a read-only root"));
        assert!(source.contains("fn grant_access_root(\n    root: &Path,\n    grants: &mut AccessGrants,\n    parent: &Handle,\n    permissions: u32,\n    share: u32,"));
        assert!(
            source.contains("FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,\n        FILE_SHARE_READ,")
        );
        assert!(source.contains(
            "FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD,\n        FILE_SHARE_READ | FILE_SHARE_WRITE,"
        ));
        assert!(!source.contains("CreateRestrictedToken"));
        assert!(!source.contains("WRITE_RESTRICTED"));
        assert!(!source.contains("RegOverridePredefKey"));
        assert!(source.contains("static GENERAL_SANDBOX_AVAILABLE: OnceLock<bool>"));
        assert!(source.contains("GENERAL_SANDBOX_AVAILABLE.get_or_init"));
        assert!(source.contains("fn run_production_preflight() -> Result<(), String>"));
        assert!(source.contains("if !is_available() || !is_available()"));

        let update = source
            .split("fn update_handle_ace(")
            .nth(1)
            .expect("ACL update implementation missing");
        let lock = update
            .find("AclMutationGuard::acquire()?")
            .expect("ACL transaction lock missing");
        let read = update
            .find("GetSecurityInfo(")
            .expect("ACL transaction read missing");
        let write = update
            .find("SetSecurityInfo(")
            .expect("ACL transaction write missing");
        assert!(lock < read && read < write);

        let helper = source
            .split("fn run_helper()")
            .nth(1)
            .and_then(|source| source.split("fn validate_ready_path").next())
            .expect("Windows helper implementation missing");
        let disarm = helper
            .find("grants.disarm_for_launch()")
            .expect("cleanup must disarm before launch");
        let launch = helper
            .find("launch_appcontainer(")
            .expect("AppContainer launch missing");
        assert!(disarm < launch);
        assert!(helper.contains("terminate_and_drain_job(&job, 126)?;\n        grants.mark_job_quiescent();\n        grants.cleanup()?;"));

        let appcontainer_launch = source
            .split("fn launch_appcontainer(")
            .nth(1)
            .and_then(|source| source.split("fn inheritable_duplicate(").next())
            .expect("AppContainer launch implementation missing");
        let create_suspended = appcontainer_launch
            .find("CreateProcessAsUserW(")
            .expect("suspended AppContainer creation missing");
        let assign_job = appcontainer_launch
            .find("AssignProcessToJobObject(job.raw(), process.raw())")
            .expect("suspended Job assignment missing");
        let configure_ui = appcontainer_launch
            .find("configure_job_ui_restrictions(job)")
            .expect("post-assignment Job UI restrictions missing");
        let resume = appcontainer_launch
            .find("ResumeThread(thread.raw())")
            .expect("attested AppContainer resume missing");
        assert!(
            create_suspended < assign_job && assign_job < configure_ui && configure_ui < resume
        );
        assert!(!appcontainer_launch.contains("PROC_THREAD_ATTRIBUTE_JOB_LIST"));
        assert!(appcontainer_launch.contains("startup.StartupInfo.hStdOutput = stdout.raw()"));

        let profile_creation = source
            .split("fn create_appcontainer_profile(")
            .nth(1)
            .and_then(|source| source.split("fn appcontainer_storage_path").next())
            .expect("AppContainer profile creation missing");
        let journal_sync = profile_creation
            .find("sync_all()")
            .expect("journal durability sync missing");
        let journal_publish = profile_creation
            .find("profile.journal_path =")
            .expect("journal publication missing");
        assert!(journal_sync < journal_publish);

        let stale_sweep = source
            .split("fn sweep_stale_profiles(")
            .nth(1)
            .and_then(|source| source.split("fn sid_text").next())
            .expect("stale profile sweep missing");
        let stale_quiescence = stale_sweep
            .find("wait_for_stale_job_quiescence")
            .expect("stale exact-Job quiescence proof missing");
        let stale_revoke = stale_sweep
            .find("revoke_tree")
            .expect("stale ACE cleanup missing");
        assert!(stale_quiescence < stale_revoke);

        let stale_job_cleanup = source
            .split("fn wait_for_stale_job_quiescence(")
            .nth(1)
            .and_then(|source| source.split("fn wait_for_job_zero(").next())
            .expect("stale exact-Job cleanup implementation missing");
        let stale_job_open = stale_job_cleanup
            .find("OpenJobObjectW")
            .expect("stale exact Job open missing");
        let stale_job_validate = stale_job_cleanup
            .find("verify_job_limits")
            .expect("stale exact Job policy validation missing");
        let stale_job_terminate = stale_job_cleanup
            .find("TerminateJobObject")
            .expect("stale exact Job termination missing");
        let stale_job_drain = stale_job_cleanup
            .find("wait_for_job_zero")
            .expect("stale exact Job drain missing");
        assert!(
            stale_job_open < stale_job_validate
                && stale_job_validate < stale_job_terminate
                && stale_job_terminate < stale_job_drain
        );

        let authority_probe = source
            .split("fn run_authority_probe(")
            .nth(1)
            .and_then(|source| source.split("fn run_descendant_probe(").next())
            .expect("authority descendant implementation missing");
        assert!(authority_probe.contains(&["descendant_command", "spawn_guarded()"].join(".")));
        assert!(authority_probe.contains(".stdin(Stdio::null())"));

        let descendant_probe = source
            .split("fn run_descendant_probe(")
            .nth(1)
            .and_then(|source| source.split("fn token_is_appcontainer(").next())
            .expect("AppContainer descendant probe implementation missing");
        assert!(descendant_probe.contains("current_token_has_zero_capabilities()"));
        assert!(descendant_probe.contains("current_token_is_appcontainer()"));
        assert!(!descendant_probe.contains("release.exists()"));

        let parent_probe = source
            .split("fn run_parent_probe(")
            .nth(1)
            .and_then(|source| source.split("fn run_authority_probe(").next())
            .expect("parent-death probe implementation missing");
        assert!(parent_probe.contains("TARGET_PROBE_ARG"));
        assert!(parent_probe.contains("TARGET_PARENT_ARG"));
        assert!(parent_probe.contains("wait_for_exact_probe_file(&tree_ready)?"));
        assert!(!parent_probe.contains("wait_for_probe_file(&tree_ready)?"));

        let runtime_probe = source
            .split("fn run_runtime_probe()")
            .nth(1)
            .and_then(|source| source.split("fn attest_completed_cleanup(").next())
            .expect("runtime AppContainer proof missing");
        let tree_ready = runtime_probe
            .find("read_exact(&mut ready)")
            .expect("parent-death descendant readiness missing");
        let parent_kill = runtime_probe
            .find("parent\n        .kill()")
            .expect("parent-death termination missing");
        let escaped_marker = runtime_probe
            .find("if marker.exists()")
            .expect("parent-death escape marker missing");
        assert!(tree_ready < parent_kill && parent_kill < escaped_marker);

        let named_job = source
            .split("fn bounded_job(")
            .nth(1)
            .and_then(|source| source.split("fn verify_job_membership_and_limits(").next())
            .expect("bounded named Job implementation missing");
        let owner_dacl = named_job
            .find("SetSecurityDescriptorDacl")
            .expect("named Job owner-only DACL missing");
        let create_job = named_job
            .find("CreateJobObjectW(&attributes")
            .expect("named Job secured creation missing");
        assert!(owner_dacl < create_job);

        let runtime_probe = source
            .split("fn run_runtime_probe(")
            .nth(1)
            .and_then(|source| source.split("fn attest_completed_cleanup").next())
            .expect("Windows runtime probe implementation missing");
        assert!(runtime_probe.contains("configured_cleanup_ready"));
        assert!(runtime_probe.contains("build_helper_with_ready_and_roots("));
        assert!(runtime_probe.contains("probe_executable.clone()"));
        assert!(!runtime_probe.contains("resolve_program(\"pwsh.exe\""));
        assert!(!runtime_probe.contains("resolve_program(\"powershell.exe\""));
        assert!(!runtime_probe.contains("resolve_program(\"cmd.exe\""));
        assert!(
            runtime_probe
                .contains("let configured_tool = configured_read.join(\"configured-tool.exe\")")
        );
        assert!(
            !runtime_probe.contains("let configured_tool = base.join(\"configured-tool.exe\")")
        );
        assert!(runtime_probe.contains("std::fs::copy(&probe_executable, &configured_tool)"));
        assert!(runtime_probe.contains("configured AppContainer tool/root probe failed: status="));
        assert!(source.contains("return Ok(53);"));
        assert!(source.contains("TARGET_CONFIGURED_SPAWN_ERROR_BASE: i32 = 0x1_0000"));
        assert!(source.contains("TARGET_CONFIGURED_WAIT_ERROR_BASE: i32 = 0x2_0000"));
        assert!(source.contains("TARGET_CONFIGURED_STDIN_DUPLICATE_ERROR_BASE: i32 = 0x3_0000"));
        assert!(source.contains("TARGET_CONFIGURED_RAW_SPAWN_ERROR_BASE: i32 = 0x7_0000"));
        assert!(source.contains("TARGET_CONFIGURED_EXECUTABLE_OPEN_ERROR_BASE: i32 = 0x8_0000"));
        assert!(source.contains("TARGET_SELF_RAW_SPAWN_ERROR_BASE: i32 = 0x9_0000"));
        assert!(source.contains("TARGET_JOB_LIMIT_QUERY_ERROR_BASE: i32 = 0xA_0000"));
        assert!(source.contains("TARGET_JOB_ACCOUNTING_QUERY_ERROR_BASE: i32 = 0xB_0000"));
        assert!(source.contains("TARGET_SELF_TOKEN_OPEN_ERROR_BASE: i32 = 0xC_0000"));
        assert!(source.contains("fn target_probe_os_error_code("));
        assert!(source.contains("fn target_probe_duplicate_handle("));
        assert!(source.contains("fn target_probe_executable_access("));
        assert!(source.contains("fn target_probe_job_status("));
        assert!(source.contains("fn target_probe_self_token_access("));
        assert!(source.contains("fn target_probe_raw_spawn("));
        assert!(source.contains("GetProcessMitigationPolicy("));
        assert!(source.contains("TOKEN_DUPLICATE | TOKEN_IMPERSONATE"));
        assert!(source.contains("policy.Anonymous.Flags } & 0b100 != 0"));
        assert!(source.contains("QueryInformationJobObject(\n            null_mut(),"));
        assert!(source.contains("let status = match child.wait()"));
        assert!(source.contains("return Ok(55);"));
        assert!(source.contains(".args([TARGET_PROBE_ARG, TARGET_NOOP_ARG])"));
        assert!(runtime_probe.contains(
            "configured_read.as_path(),\n            configured_write.as_path(),\n            configured_tool.as_path(),"
        ));
        assert!(!source.contains("fn powershell_literal("));

        let journal_root = source
            .split("fn profile_journal_root(")
            .nth(1)
            .and_then(|source| source.split("fn create_profile_journal").next())
            .expect("profile journal root implementation missing");
        assert!(journal_root.contains(".parent()"));
        assert!(journal_root.contains(".mini-agent-appcontainer-control-v1"));
        assert!(!journal_root.contains("cache.join("));

        let startup = include_str!("startup.rs");
        assert!(startup.contains("unavailable_sandbox_must_fail"));
        assert!(startup.contains("no successful production preflight"));

        let main = include_str!("main.rs");
        let validation = main
            .find("startup.validate_sandbox_availability()?")
            .expect("common sandbox validation missing");
        let acp = main
            .find("if startup.cli.acp_enabled")
            .expect("ACP dispatch missing");
        assert!(validation < acp, "ACP must not bypass sandbox validation");

        assert!(source.contains("Global\\\\mini-agent-general-sandbox-acl-v1"));
        assert!(!source.contains("Local\\\\mini-agent-general-sandbox-acl-v1"));
    }

    #[test]
    fn windows_capability_copy_reports_explicit_roots_and_default_network_denial() {
        let source = include_str!("sandbox.rs");
        assert!(source.contains(
            "AppContainer network capabilities are empty; IP network access is denied by default"
        ));
        assert!(source.contains(
            "explicit user-file roots plus pre-existing resources readable to ALL APPLICATION PACKAGES; read confidentiality is not claimed"
        ));
        assert!(source.contains(
            "the sandbox adds write access only for the canonical workspace and explicit roots; pre-existing ALL APPLICATION PACKAGES grants remain ambient"
        ));
        assert!(source.contains("default-deny AppContainer with no network capability"));
        let forbidden_registry_claim = ["registry isolation", " is enforced"].concat();
        assert!(!source.contains(&forbidden_registry_claim));
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
