use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::sandbox::{ProcessGroupGuard, configure_child_lifetime, kill_process_group};

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
}

/// Result of running a hook subprocess.
pub(crate) struct HookOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: HookStatus,
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
pub(crate) async fn run_hook(
    command: &str,
    args: Option<&[String]>,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
) -> HookOutput {
    run_hook_with_limits(
        command,
        args,
        stdin_json,
        timeout,
        project_dir,
        DEFAULT_HOOK_LIMITS,
    )
    .await
}

pub(crate) async fn run_hook_with_limits(
    command: &str,
    args: Option<&[String]>,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
    limits: HookLimits,
) -> HookOutput {
    let (program, args) = match build_hook_invocation(command, args) {
        Ok(invocation) => invocation,
        Err(message) => {
            return HookOutput {
                exit_code: None,
                stdout: Vec::new(),
                stderr: message.as_bytes().to_vec(),
                status: HookStatus::Failed,
            };
        }
    };
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.env("ZEROSTACK_PROJECT_DIR", project_dir);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    configure_child_lifetime(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return HookOutput {
                exit_code: None,
                stdout: Vec::new(),
                stderr: format!("failed to spawn hook: {e}").into_bytes(),
                status: HookStatus::Failed,
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
            output_from_capture(&captured, status.code(), HookStatus::Completed)
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
            output_from_capture(&captured, None, status)
        }
        Err(_) => {
            terminate_and_reap(&mut child, pid).await;
            guard.disarm();
            output_from_capture(&captured, None, HookStatus::TimedOut)
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
) -> HookOutput {
    let mut captured = captured.lock().unwrap_or_else(|e| e.into_inner());
    HookOutput {
        exit_code,
        stdout: std::mem::take(&mut captured.stdout),
        stderr: std::mem::take(&mut captured.stderr),
        status,
    }
}

/// Runs an `if` condition using its documented shell-command semantics.
pub(crate) async fn run_shell_condition(
    condition: &str,
    stdin_json: &[u8],
    timeout: std::time::Duration,
    project_dir: &str,
) -> HookOutput {
    let (shell, flag) = if cfg!(windows) {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };
    let args = vec![flag.to_string(), condition.to_string()];
    run_hook(shell, Some(&args), stdin_json, timeout, project_dir).await
}
