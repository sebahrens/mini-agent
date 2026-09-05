use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, BashArgs, JobAction, JobStatusArgs, PermCheck, ToolError, check_perm,
};
use crate::extras::truncate::head_lines;
use crate::sandbox::{
    BackgroundJobSnapshot, CommandLimits, CommandOutput, CommandOutputLimit, CommandStatus,
    DEFAULT_BACKGROUND_COMMAND_TIMEOUT, DEFAULT_COMMAND_LIMITS, Sandbox,
};

pub struct ShellTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub sandbox: Sandbox,
    /// `None` = no line truncation (only the byte-level command limits
    /// apply). `Some(n)` = keep the head and tail of the output within `n`
    /// lines with an omitted-count marker, on both the success path and the
    /// partial output embedded in a resource-limit error. The config default
    /// is [`crate::config::DEFAULT_MAX_BASH_OUTPUT_LINES`].
    pub max_output_lines: Option<u64>,
}

impl ShellTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        sandbox: Sandbox,
        max_output_lines: Option<u64>,
    ) -> Self {
        ShellTool {
            permission,
            ask_tx,
            sandbox,
            max_output_lines,
        }
    }
}

impl Tool for ShellTool {
    const NAME: &'static str = "shell";

    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    fn description(&self) -> String {
        let dialect = self
            .sandbox
            .shell_capability()
            .map(|capability| capability.dialect().name())
            .unwrap_or("configured shell");
        format!(
            "Execute a {dialect} command in the current working directory. Foreground commands have a hard \
             30 second deadline and bounded output. Set background=true for builds, test suites, or servers; \
             this returns a session-scoped job id for the job_status tool and uses a 24 hour maximum. The \
             optional timeout can lower the applicable deadline. Commands keep the same sandbox and permission \
             policy in either mode."
        )
    }

    fn parameters(&self) -> serde_json::Value {
        let dialect = self
            .sandbox
            .shell_capability()
            .map(|capability| capability.dialect().name())
            .unwrap_or("configured shell");
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": format!("{dialect} command to execute") },
                "timeout": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lower command deadline in milliseconds (optional; maximum 30000 foreground or 86400000 background)"
                },
                "background": {
                    "type": "boolean",
                    "description": "Start a session-scoped background job and return its id (default false)"
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        tracing::debug!(
            "tool shell start: cmd_len={}, timeout={:?}, background={}",
            args.command.len(),
            args.timeout,
            args.background,
        );
        // The complete script is the permission key and is passed unchanged to
        // the shell. Never split or tokenize it: Bash can execute nested
        // programs from syntax that ad-hoc command splitting cannot classify.
        let coaching = check_perm(&self.permission, &self.ask_tx, "shell", &args.command).await?;

        if args.background {
            let timeout = args
                .timeout
                .map(std::time::Duration::from_millis)
                .unwrap_or(DEFAULT_BACKGROUND_COMMAND_TIMEOUT)
                .min(DEFAULT_BACKGROUND_COMMAND_TIMEOUT);
            let id = self
                .sandbox
                .start_background_command(args.command, timeout)
                .await?;
            let result = format!(
                "Background job started: {id}\nUse job_status with this id to poll output or stop it."
            );
            return Ok(match coaching {
                Some(message) => format!("{message}\n\n{result}"),
                None => result,
            });
        }

        let mut limits = DEFAULT_COMMAND_LIMITS;
        if let Some(timeout_ms) = args.timeout {
            limits.timeout = limits
                .timeout
                .min(std::time::Duration::from_millis(timeout_ms));
        }
        let output = self
            .sandbox
            .output_command_with_limits(&args.command, limits)
            .await?;

        if output.status != CommandStatus::Completed {
            tracing::warn!("tool shell stopped before completion: {:?}", output.status);
            return Err(resource_limit_error(output, limits, self.max_output_lines));
        }

        let exit_code = output
            .exit_status
            .and_then(|status| status.code())
            .unwrap_or(-1);

        if exit_code != 0 {
            tracing::warn!("tool shell: non-zero exit code={}", exit_code);
        }

        let output_len = output.stdout.len() + output.stderr.len();
        let mut result = render_streams(&output.stdout, &output.stderr);
        if exit_code != 0 {
            result.push_str(&format!("\nExit code: {}", exit_code));
        }

        let result = bound_output_lines(result, self.max_output_lines);

        let result = match coaching {
            Some(msg) => format!("{}\n\n{}", msg, result),
            None => result,
        };
        tracing::debug!(
            "tool shell done: exit_code={}, output_len={}",
            exit_code,
            output_len,
        );
        Ok(result)
    }
}

pub struct JobStatusTool {
    sandbox: Sandbox,
    max_output_lines: Option<u64>,
}

impl JobStatusTool {
    pub fn new(sandbox: Sandbox, max_output_lines: Option<u64>) -> Self {
        Self {
            sandbox,
            max_output_lines,
        }
    }
}

impl Tool for JobStatusTool {
    const NAME: &'static str = "job_status";

    type Error = ToolError;
    type Args = JobStatusArgs;
    type Output = String;

    fn description(&self) -> String {
        "Poll or stop a session-scoped background shell job. Polling returns its current bounded head/tail output and terminal exit status when available. Stopping waits for process-tree cleanup before returning whenever cleanup completes within 5 seconds."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Job id returned by shell with background=true"
                },
                "action": {
                    "type": "string",
                    "enum": ["poll", "stop"],
                    "description": "Poll status/output, or stop and reap the job (default poll)"
                }
            },
            "required": ["id"]
        })
    }

    async fn call(&self, args: JobStatusArgs) -> Result<String, ToolError> {
        let snapshot = match args.action {
            JobAction::Poll => self.sandbox.background_job_snapshot(&args.id),
            JobAction::Stop => self.sandbox.stop_background_job(&args.id).await,
        }
        .map_err(ToolError::Msg)?;
        Ok(render_background_job(snapshot, self.max_output_lines))
    }
}

/// Source-compatibility alias for integrations that still construct the old
/// Rust type. The model-visible tool name is always `shell`.
pub type BashTool = ShellTool;

fn render_streams(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&stderr);
    }
    result
}

fn render_background_job(snapshot: BackgroundJobSnapshot, max_output_lines: Option<u64>) -> String {
    let mut result = format!(
        "Job: {}\nStatus: {}\nCommand: {}",
        snapshot.id,
        snapshot.status.as_str(),
        snapshot.command
    );
    if let Some(exit_code) = snapshot.exit_code {
        result.push_str(&format!("\nExit code: {exit_code}"));
    }
    let output = bound_output_lines(
        render_streams(&snapshot.stdout, &snapshot.stderr),
        max_output_lines,
    );
    if !output.is_empty() {
        result.push_str("\nOutput:\n");
        result.push_str(&output);
    }
    result
}

/// Bound `text` to at most `max_output_lines` lines, keeping the head and a
/// short tail around an omitted-count marker so the model sees how the output
/// starts and ends (exit codes and final errors live at the end). `None`
/// returns the text unchanged.
fn bound_output_lines(text: String, max_output_lines: Option<u64>) -> String {
    let Some(cap) = max_output_lines else {
        return text;
    };
    let cap = usize::try_from(cap).unwrap_or(usize::MAX).max(1);
    // Keep roughly 80 % of the budget for the head and 20 % for the tail.
    let tail = cap / 5;
    let head = cap - tail;
    let (mut bounded, total) = head_lines(&text, head);
    if total <= cap {
        return text;
    }
    let omitted = total - head - tail;
    bounded.push_str(&format!(
        "\n\n[... {omitted} lines omitted — showing the first {head} and last {tail} of {total} lines; \
         re-run with a narrower invocation or pipe through `head`/`tail`/`grep` to see the rest ...]\n"
    ));
    if tail > 0 {
        bounded.push('\n');
        bounded.push_str(
            &text
                .lines()
                .skip(total - tail)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    bounded
}

fn resource_limit_error(
    output: CommandOutput,
    limits: CommandLimits,
    max_output_lines: Option<u64>,
) -> ToolError {
    let metadata = match output.status {
        CommandStatus::TimedOut => format!(
            "[status: timed_out; timeout_ms: {}]",
            limits.timeout.as_millis()
        ),
        CommandStatus::Cancelled => "[status: cancelled]".to_string(),
        CommandStatus::OutputLimitExceeded(limit) => {
            let (name, bytes) = match limit {
                CommandOutputLimit::Stdout => ("stdout", limits.stdout_bytes),
                CommandOutputLimit::Stderr => ("stderr", limits.stderr_bytes),
                CommandOutputLimit::Combined => ("combined", limits.combined_bytes),
            };
            format!("[status: output_truncated; limit: {name}; byte_cap: {bytes}]")
        }
        CommandStatus::Failed => "[status: failed]".to_string(),
        CommandStatus::Completed => unreachable!("completed output is handled before this helper"),
    };
    let partial = bound_output_lines(
        render_streams(&output.stdout, &output.stderr),
        max_output_lines,
    );
    let message = if partial.is_empty() {
        metadata
    } else {
        format!(
            "{metadata}\nCaptured output below is partial and must not be treated as a complete command result:\n{partial}"
        )
    };
    ToolError::Msg(message)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;

    fn test_tool() -> BashTool {
        BashTool::new(None, None, Sandbox::new(false, "bwrap"), None)
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    async fn wait_for_nonempty_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::metadata(path).map_or(true, |metadata| metadata.len() == 0) {
            assert!(Instant::now() < deadline, "timed out waiting for pid file");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_process_exit(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(pid) {
            assert!(
                Instant::now() < deadline,
                "descendant process {pid} survived command cleanup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn background_id(output: &str) -> String {
        output
            .lines()
            .find_map(|line| line.strip_prefix("Background job started: "))
            .expect("background shell result must return a job id")
            .to_string()
    }

    async fn wait_for_terminal_job(tool: &JobStatusTool, id: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let output = tool
                .call(JobStatusArgs {
                    id: id.to_string(),
                    action: JobAction::Poll,
                })
                .await
                .unwrap();
            if !output.contains("Status: running") && !output.contains("Status: stopping") {
                return output;
            }
            assert!(Instant::now() < deadline, "background job did not finish");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn test_limits() -> CommandLimits {
        CommandLimits {
            timeout: Duration::from_secs(2),
            stdout_bytes: 4096,
            stderr_bytes: 4096,
            combined_bytes: 6144,
        }
    }

    #[tokio::test]
    async fn bash_resource_limits_infinite_command_uses_lower_deadline() {
        let started = Instant::now();
        let error = test_tool()
            .call(BashArgs {
                command: "while :; do :; done".to_string(),
                timeout: Some(100),
                background: false,
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.contains("[status: timed_out; timeout_ms: 100]"));
    }

    #[tokio::test]
    async fn bash_resource_limits_timeout_kills_descendant() {
        let pid_file = std::env::temp_dir().join(format!(
            "mini-agent-bash-timeout-descendant-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let command = format!(
            "sh -c 'printf \"%s\" \"$$\" > {}; while :; do sleep 1; done' & wait",
            shell_quote(&pid_file)
        );

        let error = test_tool()
            .call(BashArgs {
                command,
                timeout: Some(200),
                background: false,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("[status: timed_out; timeout_ms: 200]"));

        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        wait_for_process_exit(pid).await;
        let _ = std::fs::remove_file(pid_file);
    }

    #[tokio::test]
    async fn bash_resource_limits_receiver_loss_kills_descendant() {
        let pid_file = std::env::temp_dir().join(format!(
            "mini-agent-bash-cancel-descendant-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let command = format!(
            "sh -c 'printf \"%s\" \"$$\" > {}; while :; do sleep 1; done' & wait",
            shell_quote(&pid_file)
        );
        let handle = tokio::spawn(async move {
            test_tool()
                .call(BashArgs {
                    command,
                    timeout: None,
                    background: false,
                })
                .await
        });
        wait_for_nonempty_file(&pid_file).await;
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();

        handle.abort();
        let _ = handle.await;
        wait_for_process_exit(pid).await;
        let _ = std::fs::remove_file(pid_file);
    }

    #[tokio::test]
    async fn bash_resource_limits_stdout_only_is_bounded() {
        let limits = test_limits();
        let output = Sandbox::new(false, "bwrap")
            .output_command_with_limits("while :; do printf '0123456789abcdef'; done", limits)
            .await
            .unwrap();

        assert_eq!(
            output.status,
            CommandStatus::OutputLimitExceeded(CommandOutputLimit::Stdout)
        );
        assert_eq!(output.stdout.len(), limits.stdout_bytes);
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn bash_resource_limits_stderr_only_is_bounded() {
        let limits = test_limits();
        let output = Sandbox::new(false, "bwrap")
            .output_command_with_limits("while :; do printf '0123456789abcdef' >&2; done", limits)
            .await
            .unwrap();

        assert_eq!(
            output.status,
            CommandStatus::OutputLimitExceeded(CommandOutputLimit::Stderr)
        );
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr.len(), limits.stderr_bytes);
    }

    #[tokio::test]
    async fn bash_resource_limits_mixed_output_uses_combined_cap() {
        let limits = CommandLimits {
            stdout_bytes: 64 * 1024,
            stderr_bytes: 64 * 1024,
            ..test_limits()
        };
        let output = Sandbox::new(false, "bwrap")
            .output_command_with_limits(
                "while :; do printf '0123456789abcdef'; printf 'fedcba9876543210' >&2; done",
                limits,
            )
            .await
            .unwrap();

        assert_eq!(
            output.status,
            CommandStatus::OutputLimitExceeded(CommandOutputLimit::Combined)
        );
        assert_eq!(
            output.stdout.len() + output.stderr.len(),
            limits.combined_bytes
        );
    }

    #[tokio::test]
    async fn bash_resource_limits_exit_code_and_stream_order_are_stable() {
        let output = test_tool()
            .call(BashArgs {
                command: "printf stdout; printf stderr >&2; exit 7".to_string(),
                timeout: None,
                background: false,
            })
            .await
            .unwrap();

        assert_eq!(output, "stdout\nstderr\nExit code: 7");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_streams_bounded_output_and_reports_terminal_exit() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "printf start; sleep 0.2; printf end >&2; exit 7".to_string(),
                timeout: Some(2_000),
                background: true,
            })
            .await
            .unwrap();
        let id = background_id(&started);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let running = status
                .call(JobStatusArgs {
                    id: id.clone(),
                    action: JobAction::Poll,
                })
                .await
                .unwrap();
            if running.contains("Status: running") && running.contains("start") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "running output was not observable"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let terminal = wait_for_terminal_job(&status, &id).await;
        assert!(terminal.contains("Status: completed"), "{terminal}");
        assert!(terminal.contains("Exit code: 7"), "{terminal}");
        assert!(terminal.contains("start"), "{terminal}");
        assert!(terminal.contains("end"), "{terminal}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_stop_kills_and_reaps_the_owned_process_tree() {
        let pid_file = std::env::temp_dir().join(format!(
            "mini-agent-background-stop-descendant-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let command = format!(
            "sh -c 'printf \"%s\" \"$$\" > {}; while :; do sleep 1; done' & wait",
            shell_quote(&pid_file)
        );
        let started = shell
            .call(BashArgs {
                command,
                timeout: None,
                background: true,
            })
            .await
            .unwrap();
        let id = background_id(&started);
        wait_for_nonempty_file(&pid_file).await;
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();

        let stopped = status
            .call(JobStatusArgs {
                id,
                action: JobAction::Stop,
            })
            .await
            .unwrap();
        assert!(stopped.contains("Status: cancelled"), "{stopped}");
        wait_for_process_exit(pid).await;
        let _ = std::fs::remove_file(pid_file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_capture_keeps_head_and_tail_without_killing_noisy_job() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "seq 1 50000".to_string(),
                timeout: Some(2_000),
                background: true,
            })
            .await
            .unwrap();
        let terminal = wait_for_terminal_job(&status, &background_id(&started)).await;

        assert!(terminal.contains("Status: completed"), "{terminal}");
        assert!(terminal.contains("1\n2\n"), "{terminal}");
        assert!(terminal.contains("50000"), "{terminal}");
        assert!(terminal.contains("stdout bytes omitted"), "{terminal}");
        assert!(terminal.len() < 70 * 1024, "rolling output was not bounded");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_timeout_is_terminal_and_has_no_exit_code() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "sleep 1".to_string(),
                timeout: Some(50),
                background: true,
            })
            .await
            .unwrap();
        let terminal = wait_for_terminal_job(&status, &background_id(&started)).await;

        assert!(terminal.contains("Status: timed_out"), "{terminal}");
        assert!(!terminal.contains("Exit code:"), "{terminal}");
    }

    #[tokio::test]
    async fn background_job_unknown_id_is_rejected() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let id = format!("job-{}", uuid::Uuid::new_v4());
        let unknown = status
            .call(JobStatusArgs {
                id: id.clone(),
                action: JobAction::Poll,
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(unknown, format!("background job not found: {id}"));

        let invalid = status
            .call(JobStatusArgs {
                id: "x".repeat(128 * 1024),
                action: JobAction::Poll,
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(invalid, "invalid background job id");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_completed_background_job_preserves_its_result() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "exit 0".to_string(),
                timeout: Some(1_000),
                background: true,
            })
            .await
            .unwrap();
        let id = background_id(&started);
        let completed = wait_for_terminal_job(&status, &id).await;
        assert!(completed.contains("Status: completed"), "{completed}");

        let stopped = status
            .call(JobStatusArgs {
                id,
                action: JobAction::Stop,
            })
            .await
            .unwrap();
        assert!(stopped.contains("Status: completed"), "{stopped}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_capacity_is_bounded_and_global_cancel_reaps_every_job() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let mut ids = Vec::new();
        for _ in 0..8 {
            let started = shell
                .call(BashArgs {
                    command: "while :; do sleep 1; done".to_string(),
                    timeout: None,
                    background: true,
                })
                .await
                .unwrap();
            ids.push(background_id(&started));
        }
        assert_eq!(shell.sandbox.running_background_job_count(), 8);

        let overflow = shell
            .call(BashArgs {
                command: "exit 0".to_string(),
                timeout: None,
                background: true,
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            overflow,
            "background job capacity is full (maximum 8 running jobs)"
        );

        shell.sandbox.kill_active();
        for id in ids {
            let terminal = wait_for_terminal_job(&status, &id).await;
            assert!(terminal.contains("Status: cancelled"), "{terminal}");
        }
        assert_eq!(shell.sandbox.running_background_job_count(), 0);
        assert_eq!(shell.sandbox.active_group_count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_session_sandbox_cancels_background_jobs() {
        let pid_file = std::env::temp_dir().join(format!(
            "mini-agent-background-drop-descendant-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sandbox = Sandbox::new(false, "bwrap");
        let command = format!(
            "sh -c 'printf \"%s\" \"$$\" > {}; while :; do sleep 1; done' & wait",
            shell_quote(&pid_file)
        );
        sandbox
            .start_background_command(command, DEFAULT_BACKGROUND_COMMAND_TIMEOUT)
            .await
            .unwrap();
        wait_for_nonempty_file(&pid_file).await;
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();

        drop(sandbox);
        wait_for_process_exit(pid).await;
        let _ = std::fs::remove_file(pid_file);
    }
}
