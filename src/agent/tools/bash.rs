use rig::tool::Tool;

use crate::agent::tools::{AskSender, BashArgs, PermCheck, ToolError, check_perm};
use crate::extras::truncate::head_lines;
use crate::sandbox::{
    CommandLimits, CommandOutput, CommandOutputLimit, CommandStatus, DEFAULT_COMMAND_LIMITS,
    Sandbox,
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
            "Execute a {dialect} command in the current working directory. Commands have a hard 30 second \
             deadline and bounded output. The optional timeout can only lower the deadline. Complete \
             output is decoded with UTF-8 replacement and returned as stdout followed by stderr."
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
                    "description": "Lower command deadline in milliseconds (optional; maximum 30000)"
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        tracing::debug!(
            "tool shell start: cmd_len={}, timeout={:?}",
            args.command.len(),
            args.timeout,
        );
        // The complete script is the permission key and is passed unchanged to
        // the shell. Never split or tokenize it: Bash can execute nested
        // programs from syntax that ad-hoc command splitting cannot classify.
        let coaching = check_perm(&self.permission, &self.ask_tx, "shell", &args.command).await?;

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
            })
            .await
            .unwrap();

        assert_eq!(output, "stdout\nstderr\nExit code: 7");
    }
}
