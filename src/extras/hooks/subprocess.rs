use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;

use crate::process_creation::TokioCommandCreationExt;
use crate::sandbox::{
    HOOK_SANDBOX_READY_MARKER, ProcessGroupGuard, Sandbox, SandboxPolicy, kill_process_group,
};

use super::settings::HookTrust;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookDiagnostics {
    pub containment: &'static str,
    pub environment: &'static str,
    pub filesystem: &'static str,
    pub network: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct HookPolicy {
    trust: HookTrust,
    sandbox: Sandbox,
    env: std::collections::BTreeMap<String, String>,
}

impl HookPolicy {
    pub(crate) fn new(
        trust: HookTrust,
        sandbox_backend: &str,
        env: std::collections::BTreeMap<String, String>,
    ) -> Self {
        Self {
            trust,
            sandbox: Sandbox::new(trust == HookTrust::Sandboxed, sandbox_backend),
            env,
        }
    }

    pub(crate) fn diagnostics(&self) -> HookDiagnostics {
        match (self.trust, self.sandbox.policy()) {
            (HookTrust::Trusted, _) => HookDiagnostics {
                containment: "trusted-bypass; sandbox-not-requested",
                environment: "minimal-explicit",
                filesystem: "ambient-trusted-bypass",
                network: "ambient-trusted-bypass",
            },
            (_, SandboxPolicy::RequiredButUnavailable) => HookDiagnostics {
                containment: "requested-but-unavailable; launch-denied",
                environment: "none; launch-denied",
                filesystem: "none; launch-denied",
                network: "none; launch-denied",
            },
            (_, SandboxPolicy::RequiredAndAvailable) => {
                let capabilities = self.sandbox.capability_matrix();
                HookDiagnostics {
                    containment: "required-and-available",
                    environment: "minimal-explicit",
                    filesystem: capabilities.filesystem_writes,
                    network: capabilities.network,
                }
            }
            (_, SandboxPolicy::Disabled) => HookDiagnostics {
                containment: "invalid-policy; launch-denied",
                environment: "none; launch-denied",
                filesystem: "none; launch-denied",
                network: "none; launch-denied",
            },
        }
    }

    fn launch_denied_diagnostics(&self) -> HookDiagnostics {
        HookDiagnostics {
            containment: match (self.trust, self.sandbox.policy()) {
                (HookTrust::Sandboxed, SandboxPolicy::RequiredButUnavailable) => {
                    "requested-but-unavailable; launch-denied"
                }
                (HookTrust::Sandboxed, _) => "required-policy; launch-denied",
                (HookTrust::Trusted, _) => "trusted-bypass-not-entered; launch-denied",
            },
            environment: "none; launch-denied",
            filesystem: "none; launch-denied",
            network: "none; launch-denied",
        }
    }

    fn spawn_failure_status(&self) -> HookStatus {
        if self.trust == HookTrust::Sandboxed {
            HookStatus::PolicyDenied
        } else {
            HookStatus::Failed
        }
    }

    /// A sandboxed command becomes "started" only after the trusted inner
    /// launcher has entered containment, verified the target executable is
    /// visible, and emitted its readiness record immediately before `exec`.
    fn classify_spawned_output(&self, mut output: HookOutput) -> HookOutput {
        if self.trust == HookTrust::Trusted {
            return output;
        }
        if let Some(offset) = output
            .stderr
            .windows(HOOK_SANDBOX_READY_MARKER.len())
            .position(|window| window == HOOK_SANDBOX_READY_MARKER)
        {
            output
                .stderr
                .drain(offset..offset + HOOK_SANDBOX_READY_MARKER.len());
            output.started = true;
            return output;
        }
        output.started = false;
        output.exit_code = None;
        output.stdout.clear();
        output.stderr = b"sandbox wrapper failed before hook launch readiness".to_vec();
        output.status = HookStatus::PolicyDenied;
        output.diagnostics = self.launch_denied_diagnostics();
        output
    }

    #[cfg(test)]
    pub(crate) fn classify_completed_output_for_test(&self, output: HookOutput) -> HookOutput {
        self.classify_spawned_output(output)
    }

    fn explicit_env(
        &self,
        project_dir: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, String> {
        let mut env = self.env.clone();
        let mut portable_keys = std::collections::HashSet::new();
        for key in env.keys() {
            if key.is_empty()
                || key.contains('=')
                || key.contains('\0')
                || key.eq_ignore_ascii_case("ZEROSTACK_PROJECT_DIR")
            {
                return Err(format!("invalid or reserved environment key {key:?}"));
            }
            if !portable_keys.insert(key.to_ascii_uppercase()) {
                return Err(format!(
                    "hook environment keys collide case-insensitively: {key:?}"
                ));
            }
        }
        if env.values().any(|value| value.contains('\0')) {
            return Err("hook environment values cannot contain NUL".to_string());
        }
        env.insert("ZEROSTACK_PROJECT_DIR".to_string(), project_dir.to_string());
        Ok(env)
    }
}

/// Hard output limits for one hook subprocess.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HookLimits {
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub combined_bytes: usize,
}

pub(crate) const DEFAULT_HOOK_LIMITS: HookLimits = HookLimits {
    stdout_bytes: 1024 * 1024,
    stderr_bytes: 1024 * 1024,
    combined_bytes: 1536 * 1024,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputLimit {
    Stdout,
    Stderr,
    Combined,
}

/// How the hook subprocess stopped. A completed hook may still have a
/// non-zero `exit_code`; timeout and output-limit failures are separate so
/// callers never interpret partial output as a complete hook response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookStatus {
    Completed,
    TimedOut,
    OutputLimitExceeded(OutputLimit),
    Failed,
    PolicyDenied,
}

/// Result of running a hook subprocess.
pub(crate) struct HookOutput {
    /// True only after the OS accepted creation of the direct child or
    /// containment wrapper. Preflight/policy/spawn failures remain false.
    pub started: bool,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: HookStatus,
    pub diagnostics: HookDiagnostics,
}

const MISSING_ARGS_ERROR: &str =
    "hook command requires an `args` field (use an empty array for no arguments)";

#[derive(Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    combined_bytes: usize,
}

impl CapturedOutput {
    fn push(
        &mut self,
        stream: OutputStream,
        bytes: &[u8],
        limits: HookLimits,
    ) -> Result<(), OutputLimit> {
        let (output, stream_limit, stream_error) = match stream {
            OutputStream::Stdout => (&mut self.stdout, limits.stdout_bytes, OutputLimit::Stdout),
            OutputStream::Stderr => (&mut self.stderr, limits.stderr_bytes, OutputLimit::Stderr),
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
            Err(OutputLimit::Combined)
        }
    }
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
enum RunError {
    OutputLimit(OutputLimit),
    Read(std::io::Error),
    Wait(std::io::Error),
}

/// Pure: builds the direct exec-form invocation for a hook command.
///
/// Requiring `args` prevents `command` from being interpreted by a shell.
/// Callers that intentionally need shell behavior must make that explicit by
/// setting `command` to the shell executable and passing the script in `args`.
pub(crate) fn build_hook_invocation(
    command: &str,
    args: Option<&[String]>,
) -> Result<(String, Vec<String>), &'static str> {
    match args {
        Some(args) => Ok((command.to_string(), args.to_vec())),
        None => Err(MISSING_ARGS_ERROR),
    }
}

/// Spawns the hook as a subprocess in its own process group, writes
/// `stdin_json` to its stdin then closes it, concurrently drains bounded
/// stdout/stderr, waits up to `timeout`, and kills and reaps the whole process
/// group on any resource or I/O failure. `async: true` handling (run in the
/// background, ignore the decision) is the caller's responsibility.
/// `project_dir` is exposed to the hook as `$ZEROSTACK_PROJECT_DIR`. `args`
/// is required so the command always uses direct exec form (see
/// [`build_hook_invocation`]).
#[cfg(test)]
pub(crate) async fn run_hook(
    command: &str,
    args: Option<&[String]>,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
) -> HookOutput {
    let policy = HookPolicy::new(
        HookTrust::Trusted,
        "unused",
        std::collections::BTreeMap::new(),
    );
    run_hook_with_policy_and_limits(
        command,
        args,
        stdin_json,
        timeout,
        project_dir,
        &policy,
        DEFAULT_HOOK_LIMITS,
    )
    .await
}

async fn run_hook_with_policy_and_limits(
    command: &str,
    args: Option<&[String]>,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
    policy: &HookPolicy,
    limits: HookLimits,
) -> HookOutput {
    let (program, args) = match build_hook_invocation(command, args) {
        Ok(invocation) => invocation,
        Err(message) => {
            return HookOutput {
                started: false,
                exit_code: None,
                stdout: Vec::new(),
                stderr: message.as_bytes().to_vec(),
                status: HookStatus::PolicyDenied,
                diagnostics: policy.launch_denied_diagnostics(),
            };
        }
    };
    let project_dir = match std::fs::canonicalize(project_dir) {
        Ok(path) if path.parent().is_some() => path,
        Ok(_) => {
            return HookOutput {
                started: false,
                exit_code: None,
                stdout: Vec::new(),
                stderr: b"hook project directory cannot be the filesystem root".to_vec(),
                status: HookStatus::PolicyDenied,
                diagnostics: policy.launch_denied_diagnostics(),
            };
        }
        Err(error) => {
            return HookOutput {
                started: false,
                exit_code: None,
                stdout: Vec::new(),
                stderr: format!("failed to resolve hook project directory: {error}").into_bytes(),
                status: HookStatus::PolicyDenied,
                diagnostics: policy.launch_denied_diagnostics(),
            };
        }
    };
    let project_dir_text = project_dir.to_string_lossy();
    let program_path = std::path::Path::new(&program);
    let program = if program_path.is_relative() && program_path.components().count() > 1 {
        let resolved = match project_dir.join(program_path).canonicalize() {
            Ok(path) if path.starts_with(&project_dir) => path,
            Ok(_) => {
                return HookOutput {
                    started: false,
                    exit_code: None,
                    stdout: Vec::new(),
                    stderr: b"relative hook executable escapes the project directory".to_vec(),
                    status: HookStatus::PolicyDenied,
                    diagnostics: policy.launch_denied_diagnostics(),
                };
            }
            Err(error) => {
                return HookOutput {
                    started: false,
                    exit_code: None,
                    stdout: Vec::new(),
                    stderr: format!("failed to resolve relative hook executable: {error}")
                        .into_bytes(),
                    status: HookStatus::PolicyDenied,
                    diagnostics: policy.launch_denied_diagnostics(),
                };
            }
        };
        resolved.to_string_lossy().into_owned()
    } else {
        program
    };
    let explicit_env = match policy.explicit_env(&project_dir_text) {
        Ok(env) => env,
        Err(message) => {
            return HookOutput {
                started: false,
                exit_code: None,
                stdout: Vec::new(),
                stderr: message.into_bytes(),
                status: HookStatus::PolicyDenied,
                diagnostics: policy.launch_denied_diagnostics(),
            };
        }
    };
    let mut cmd =
        match policy
            .sandbox
            .wrap_direct_command(&program, &args, &project_dir, &explicit_env)
        {
            Ok(command) => command,
            Err(message) => {
                return HookOutput {
                    started: false,
                    exit_code: None,
                    stdout: Vec::new(),
                    stderr: message.into_bytes(),
                    status: HookStatus::PolicyDenied,
                    diagnostics: policy.launch_denied_diagnostics(),
                };
            }
        };
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = match cmd.spawn_guarded() {
        Ok(child) => child,
        Err(e) => {
            return HookOutput {
                started: false,
                exit_code: None,
                stdout: Vec::new(),
                stderr: format!("failed to spawn hook: {e}").into_bytes(),
                status: policy.spawn_failure_status(),
                diagnostics: policy.launch_denied_diagnostics(),
            };
        }
    };

    let active_groups: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    let pid = child.id();
    let mut guard = ProcessGroupGuard::new(pid, active_groups);
    let stdin_pipe = child.stdin.take();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let captured = Arc::new(Mutex::new(CapturedOutput::default()));

    let run = async {
        let write_stdin = async move {
            if let Some(mut stdin) = stdin_pipe {
                if let Err(e) = stdin.write_all(stdin_json).await {
                    // Preserve the existing best-effort stdin semantics: a hook
                    // that closes stdin may still produce a valid result.
                    tracing::warn!("hooks: failed to write hook stdin: {e}");
                }
            }
            Ok::<(), RunError>(())
        };
        let read_stdout = capture_pipe(stdout_pipe, OutputStream::Stdout, captured.clone(), limits);
        let read_stderr = capture_pipe(stderr_pipe, OutputStream::Stderr, captured.clone(), limits);
        let wait = async { child.wait().await.map_err(RunError::Wait) };
        let (_, _, _, status) = tokio::try_join!(write_stdin, read_stdout, read_stderr, wait)?;
        Ok::<_, RunError>(status)
    };

    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(status)) => {
            // The direct child has exited and been reaped. Kill any descendants
            // that deliberately closed their inherited pipes before outliving
            // the hook.
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            guard.disarm();
            policy.classify_spawned_output(output_from_capture(
                &captured,
                status.code(),
                HookStatus::Completed,
                policy.diagnostics(),
            ))
        }
        Ok(Err(error)) => {
            terminate_and_reap(&mut child, pid).await;
            guard.disarm();
            let status = match error {
                RunError::OutputLimit(limit) => HookStatus::OutputLimitExceeded(limit),
                RunError::Read(error) => {
                    tracing::warn!("hooks: failed to consume hook output: {error}");
                    HookStatus::Failed
                }
                RunError::Wait(error) => {
                    tracing::warn!("hooks: failed to wait for hook subprocess: {error}");
                    HookStatus::Failed
                }
            };
            policy.classify_spawned_output(output_from_capture(
                &captured,
                None,
                status,
                policy.diagnostics(),
            ))
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            guard.disarm();
            policy.classify_spawned_output(output_from_capture(
                &captured,
                None,
                HookStatus::TimedOut,
                policy.diagnostics(),
            ))
        }
    }
}

async fn capture_pipe<R>(
    pipe: Option<R>,
    stream: OutputStream,
    captured: Arc<Mutex<CapturedOutput>>,
    limits: HookLimits,
) -> Result<(), RunError>
where
    R: AsyncRead + Unpin,
{
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    loop {
        let read = pipe.read(&mut buffer).await.map_err(RunError::Read)?;
        if read == 0 {
            return Ok(());
        }
        captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(stream, &buffer[..read], limits)
            .map_err(RunError::OutputLimit)?;
    }
}

async fn terminate_and_reap(child: &mut Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        kill_process_group(pid);
    }
    let _ = child.start_kill();
    if let Err(error) = child.wait().await {
        tracing::warn!("hooks: failed to reap terminated hook subprocess: {error}");
    }
}

fn output_from_capture(
    captured: &Arc<Mutex<CapturedOutput>>,
    exit_code: Option<i32>,
    status: HookStatus,
    diagnostics: HookDiagnostics,
) -> HookOutput {
    let mut captured = captured.lock().unwrap_or_else(|e| e.into_inner());
    HookOutput {
        started: true,
        exit_code,
        stdout: std::mem::take(&mut captured.stdout),
        stderr: std::mem::take(&mut captured.stderr),
        status,
        diagnostics,
    }
}

pub(crate) async fn run_hook_with_policy(
    command: &str,
    args: Option<&[String]>,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
    policy: &HookPolicy,
) -> HookOutput {
    run_hook_with_policy_and_limits(
        command,
        args,
        stdin_json,
        timeout,
        project_dir,
        policy,
        DEFAULT_HOOK_LIMITS,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn run_hook_with_limits(
    command: &str,
    args: Option<&[String]>,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
    limits: HookLimits,
) -> HookOutput {
    let policy = HookPolicy::new(
        HookTrust::Trusted,
        "unused",
        std::collections::BTreeMap::new(),
    );
    run_hook_with_policy_and_limits(
        command,
        args,
        stdin_json,
        timeout,
        project_dir,
        &policy,
        limits,
    )
    .await
}

/// Runs an `if` condition using its documented shell-command semantics.
pub(crate) async fn run_shell_condition(
    condition: &str,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
    policy: &HookPolicy,
) -> HookOutput {
    let (shell, flag) = if cfg!(windows) {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };
    let args = vec![flag.to_string(), condition.to_string()];
    run_hook_with_policy(shell, Some(&args), stdin_json, timeout, project_dir, policy).await
}
