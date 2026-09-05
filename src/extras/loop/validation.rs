//! Bounded execution for configured validation commands.
//!
//! Loop validators and completion verification gates call this module so
//! sandbox selection, resource limits, stream rendering, and failure
//! diagnostics stay identical across headless and interactive surfaces.

use std::process::ExitStatus;

use crate::sandbox::{
    CommandCancellation, CommandLimits, CommandOutput, CommandOutputLimit, CommandStatus,
    DEFAULT_COMMAND_LIMITS, Sandbox,
};

/// Loop validators inherit the same hard process budget as the Bash tool.
/// Keeping this as a distinct constant makes the loop policy explicit and lets
/// tests supply smaller limits without weakening production bounds.
pub(crate) const LOOP_VALIDATION_LIMITS: CommandLimits = DEFAULT_COMMAND_LIMITS;
const COMMAND_DISPLAY_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationStatus {
    Success { exit_code: Option<i32> },
    NonZeroExit { exit_code: Option<i32> },
    TimedOut,
    Cancelled,
    OutputLimitExceeded(CommandOutputLimit),
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidationResult {
    pub status: ValidationStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    limits: CommandLimits,
    diagnostic_truncated: bool,
}

/// Operation-scoped cancellation for exactly one loop validator.
///
/// Clones all address the same validation worker. Cancelling does not touch
/// Bash, JS, `/btw`, or other commands that happen to share its [`Sandbox`].
#[derive(Debug, Clone)]
pub(crate) struct ValidationCancellation {
    command: CommandCancellation,
}

impl ValidationCancellation {
    pub(crate) fn cancel(&self) {
        self.command.cancel();
    }
}

/// A validation that has its cancellation identity before it starts running.
/// This lets signal and UI routing retain a scoped handle while another task
/// owns and awaits the subprocess lifecycle.
pub(crate) struct ValidationOperation {
    sandbox: Sandbox,
    command: String,
    limits: CommandLimits,
    cancellation: ValidationCancellation,
}

impl ValidationOperation {
    pub(crate) fn cancellation(&self) -> ValidationCancellation {
        self.cancellation.clone()
    }

    pub(crate) async fn wait(self) -> ValidationResult {
        run_with_limits_and_cancellation(
            &self.sandbox,
            &self.command,
            self.limits,
            &self.cancellation,
        )
        .await
    }
}

impl ValidationResult {
    pub(crate) fn succeeded(&self) -> bool {
        matches!(
            self.status,
            ValidationStatus::Success { exit_code: Some(0) }
        )
    }

    fn from_command_output(output: CommandOutput, limits: CommandLimits) -> Self {
        let status = match output.status {
            CommandStatus::Completed => completed_status(output.exit_status),
            CommandStatus::TimedOut => ValidationStatus::TimedOut,
            CommandStatus::Cancelled => ValidationStatus::Cancelled,
            CommandStatus::OutputLimitExceeded(limit) => {
                ValidationStatus::OutputLimitExceeded(limit)
            }
            CommandStatus::Failed => ValidationStatus::Failed,
        };
        let (stdout, stderr, diagnostic_truncated) =
            bound_captured_streams(output.stdout, output.stderr, limits);
        Self {
            status,
            stdout,
            stderr,
            limits,
            diagnostic_truncated,
        }
    }

    fn failed(error: std::io::Error, limits: CommandLimits) -> Self {
        let (stdout, stderr, diagnostic_truncated) = bound_captured_streams(
            Vec::new(),
            format!("validation runner failed: {error}").into_bytes(),
            limits,
        );
        Self {
            status: ValidationStatus::Failed,
            stdout,
            stderr,
            limits,
            diagnostic_truncated,
        }
    }

    /// Stable, bounded text stored in the loop transcript and next prompt.
    /// Stream labels avoid ambiguity, invalid UTF-8 is replaced, and terminal
    /// control sequences cannot escape into headless output or the TUI.
    pub(crate) fn render(&self) -> String {
        let mut remaining = self.limits.combined_bytes;
        let stdout_max = self.limits.stdout_bytes.min(remaining);
        let (stdout, stdout_truncated) = sanitize_bytes(&self.stdout, stdout_max);
        remaining = remaining.saturating_sub(stdout.len());
        let stderr_max = self.limits.stderr_bytes.min(remaining);
        let (stderr, stderr_truncated) = sanitize_bytes(&self.stderr, stderr_max);
        let diagnostic_truncated =
            self.diagnostic_truncated || stdout_truncated || stderr_truncated;

        let mut rendered = self.metadata(diagnostic_truncated);
        append_stream(&mut rendered, "stdout", &stdout, !self.stdout.is_empty());
        append_stream(&mut rendered, "stderr", &stderr, !self.stderr.is_empty());
        rendered
    }

    /// Tail-focused diagnostic for model continuation prompts. Keep the
    /// structured status line even when verbose build output is clipped.
    pub(crate) fn render_tail(&self, max_chars: usize) -> String {
        let rendered = self.render();
        let char_count = rendered.chars().count();
        if char_count <= max_chars {
            return rendered;
        }
        let tail: String = rendered
            .chars()
            .skip(char_count.saturating_sub(max_chars))
            .collect();
        format!(
            "{}\n[verification diagnostic clipped to final {max_chars} characters]\n{tail}",
            self.metadata(true)
        )
    }

    fn metadata(&self, diagnostic_truncated: bool) -> String {
        let captured = format!(
            "stdout_bytes={} stderr_bytes={} diagnostic_truncated={}",
            self.stdout.len(),
            self.stderr.len(),
            diagnostic_truncated
        );
        match self.status {
            ValidationStatus::Success { exit_code } => format!(
                "[validation status=success exit_code={} {captured}]",
                render_exit_code(exit_code)
            ),
            ValidationStatus::NonZeroExit { exit_code } => format!(
                "[validation status=nonzero_exit exit_code={} {captured}]",
                render_exit_code(exit_code)
            ),
            ValidationStatus::TimedOut => format!(
                "[validation status=timed_out timeout_ms={} {captured}]",
                self.limits.timeout.as_millis()
            ),
            ValidationStatus::Cancelled => {
                format!("[validation status=cancelled {captured}]")
            }
            ValidationStatus::OutputLimitExceeded(limit) => {
                let (stream, byte_cap) = match limit {
                    CommandOutputLimit::Stdout => ("stdout", self.limits.stdout_bytes),
                    CommandOutputLimit::Stderr => ("stderr", self.limits.stderr_bytes),
                    CommandOutputLimit::Combined => ("combined", self.limits.combined_bytes),
                };
                format!(
                    "[validation status=output_truncated limit={stream} byte_cap={byte_cap} {captured}]"
                )
            }
            ValidationStatus::Failed => format!("[validation status=failed {captured}]"),
        }
    }
}

fn completed_status(status: Option<ExitStatus>) -> ValidationStatus {
    match status {
        Some(status) if status.success() => ValidationStatus::Success {
            exit_code: status.code(),
        },
        Some(status) => ValidationStatus::NonZeroExit {
            exit_code: status.code(),
        },
        None => ValidationStatus::Failed,
    }
}

fn render_exit_code(code: Option<i32>) -> String {
    code.map_or_else(|| "signal".to_string(), |code| code.to_string())
}

fn append_stream(rendered: &mut String, name: &str, safe: &str, was_present: bool) {
    if !was_present {
        return;
    }
    rendered.push_str("\n[");
    rendered.push_str(name);
    rendered.push_str("]\n");
    rendered.push_str(safe);
}

fn sanitize_bytes(bytes: &[u8], max_bytes: usize) -> (String, bool) {
    let decoded = String::from_utf8_lossy(bytes);
    let mut result = String::with_capacity(decoded.len());
    let mut truncated = false;
    let mut chars = decoded.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            match chars.next() {
                Some('[') | Some(']') => {
                    for next in &mut chars {
                        if next.is_ascii_alphabetic() || next == '~' {
                            break;
                        }
                    }
                }
                Some(_) => {}
                None => break,
            }
        } else if character.is_ascii_control()
            && character != '\n'
            && character != '\t'
            && character != '\r'
        {
            continue;
        } else {
            let encoded_bytes = character.len_utf8();
            if result.len().saturating_add(encoded_bytes) > max_bytes {
                truncated = true;
                break;
            }
            result.push(character);
        }
    }
    (result, truncated)
}

pub(crate) fn display_command(command: &str) -> String {
    let (mut safe, _) = sanitize_bytes(command.as_bytes(), COMMAND_DISPLAY_BYTES);
    if command.len() > COMMAND_DISPLAY_BYTES {
        safe.push_str("…[command display truncated]");
    }
    safe
}

fn bound_captured_streams(
    mut stdout: Vec<u8>,
    mut stderr: Vec<u8>,
    limits: CommandLimits,
) -> (Vec<u8>, Vec<u8>, bool) {
    let original_len = stdout.len().saturating_add(stderr.len());
    stdout.truncate(limits.stdout_bytes.min(limits.combined_bytes));
    let remaining = limits.combined_bytes.saturating_sub(stdout.len());
    stderr.truncate(limits.stderr_bytes.min(remaining));
    let bounded_len = stdout.len().saturating_add(stderr.len());
    (stdout, stderr, bounded_len < original_len)
}

pub(crate) fn start(sandbox: &Sandbox, command: &str) -> ValidationOperation {
    start_with_limits(sandbox, command, LOOP_VALIDATION_LIMITS)
}

pub(crate) fn start_with_limits(
    sandbox: &Sandbox,
    command: &str,
    limits: CommandLimits,
) -> ValidationOperation {
    ValidationOperation {
        sandbox: sandbox.clone(),
        command: command.to_string(),
        limits,
        cancellation: ValidationCancellation {
            command: CommandCancellation::new(),
        },
    }
}

#[cfg(test)]
pub(crate) async fn run_with_limits(
    sandbox: &Sandbox,
    command: &str,
    limits: CommandLimits,
) -> ValidationResult {
    start_with_limits(sandbox, command, limits).wait().await
}

async fn run_with_limits_and_cancellation(
    sandbox: &Sandbox,
    command: &str,
    limits: CommandLimits,
    cancellation: &ValidationCancellation,
) -> ValidationResult {
    let sandbox = sandbox.clone();

    match sandbox
        .output_command_with_limits_cancelled(command, limits, &cancellation.command)
        .await
    {
        Ok(output) => ValidationResult::from_command_output(output, limits),
        Err(error) => ValidationResult::failed(error, limits),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::*;

    fn limits(timeout: Duration) -> CommandLimits {
        CommandLimits {
            timeout,
            stdout_bytes: 128,
            stderr_bytes: 128,
            combined_bytes: 192,
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        #[cfg(target_os = "macos")]
        let temp_dir = PathBuf::from("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let temp_dir = std::env::temp_dir();
        temp_dir.join(format!(
            "mini-agent-loop-validation-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for {label}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse()
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for validator descendant pid file"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn assert_process_gone(pid: u32) {
        wait_until(|| !process_exists(pid), "validator descendant cleanup").await;
    }

    #[tokio::test]
    async fn loop_validation_process_limits_success_nonzero_mixed_and_invalid_utf8_are_structured()
    {
        let sandbox = Sandbox::new(false, "bwrap");

        let success = run_with_limits(
            &sandbox,
            "printf 'ok\\n'; printf '\\377bad\\n\\033[31mred\\033[0m\\001' >&2",
            limits(Duration::from_secs(2)),
        )
        .await;
        assert_eq!(
            success.status,
            ValidationStatus::Success { exit_code: Some(0) }
        );
        let rendered = success.render();
        assert!(rendered.starts_with("[validation status=success exit_code=0"));
        assert!(rendered.contains("[stdout]\nok\n"));
        assert!(rendered.contains("[stderr]\n�bad\nred"));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x01'));

        let nonzero = run_with_limits(
            &sandbox,
            "printf 'partial'; printf 'problem' >&2; exit 23",
            limits(Duration::from_secs(2)),
        )
        .await;
        assert_eq!(
            nonzero.status,
            ValidationStatus::NonZeroExit {
                exit_code: Some(23)
            }
        );
        let rendered = nonzero.render();
        assert!(rendered.contains("status=nonzero_exit exit_code=23"));
        assert!(rendered.contains("[stdout]\npartial"));
        assert!(rendered.contains("[stderr]\nproblem"));
    }

    #[tokio::test]
    async fn loop_validation_process_limits_stdout_stderr_and_combined_floods_are_bounded() {
        let sandbox = Sandbox::new(false, "bwrap");

        let stdout = run_with_limits(
            &sandbox,
            "while :; do printf '0123456789abcdef'; done",
            CommandLimits {
                timeout: Duration::from_secs(2),
                stdout_bytes: 32,
                stderr_bytes: 128,
                combined_bytes: 160,
            },
        )
        .await;
        assert_eq!(
            stdout.status,
            ValidationStatus::OutputLimitExceeded(CommandOutputLimit::Stdout)
        );
        assert_eq!(stdout.stdout.len(), 32);
        assert!(stdout.render().contains("limit=stdout byte_cap=32"));

        let stderr = run_with_limits(
            &sandbox,
            "while :; do printf '0123456789abcdef' >&2; done",
            CommandLimits {
                timeout: Duration::from_secs(2),
                stdout_bytes: 128,
                stderr_bytes: 32,
                combined_bytes: 160,
            },
        )
        .await;
        assert_eq!(
            stderr.status,
            ValidationStatus::OutputLimitExceeded(CommandOutputLimit::Stderr)
        );
        assert_eq!(stderr.stderr.len(), 32);
        assert!(stderr.render().contains("limit=stderr byte_cap=32"));

        let mixed = run_with_limits(
            &sandbox,
            "(while :; do printf 'stdout-output'; done) & \
             (while :; do printf 'stderr-output' >&2; done) & wait",
            CommandLimits {
                timeout: Duration::from_secs(2),
                stdout_bytes: 128,
                stderr_bytes: 128,
                combined_bytes: 48,
            },
        )
        .await;
        assert_eq!(
            mixed.status,
            ValidationStatus::OutputLimitExceeded(CommandOutputLimit::Combined)
        );
        assert_eq!(mixed.stdout.len() + mixed.stderr.len(), 48);
        assert!(mixed.render().contains("limit=combined byte_cap=48"));
        assert_eq!(sandbox.active_group_count(), 0);
    }

    #[tokio::test]
    async fn loop_validation_process_limits_timeout_kills_ignored_term_process_tree_and_recovers() {
        let pid_file = temp_path("timeout-pid");
        let sandbox = Sandbox::new(false, "bwrap");
        let command = format!(
            "trap '' TERM; (trap '' TERM; while :; do sleep 1; done) & \
             child=$!; printf '%s' \"$child\" > {}; wait",
            shell_quote(&pid_file)
        );

        let result = run_with_limits(&sandbox, &command, limits(Duration::from_millis(150))).await;
        let pid = wait_for_pid(&pid_file).await;
        assert_eq!(result.status, ValidationStatus::TimedOut);
        assert!(result.render().contains("status=timed_out timeout_ms=150"));
        assert_process_gone(pid).await;
        assert_eq!(sandbox.active_group_count(), 0);

        let recovery =
            run_with_limits(&sandbox, "printf recovered", limits(Duration::from_secs(1))).await;
        assert_eq!(
            recovery.status,
            ValidationStatus::Success { exit_code: Some(0) }
        );
        assert!(recovery.render().contains("[stdout]\nrecovered"));
        let _ = std::fs::remove_file(pid_file);
    }

    async fn assert_scoped_cancellation_preserves_unrelated_command(sandbox: Sandbox, label: &str) {
        let validator_pid_file = temp_path(&format!("{label}-validator-pid"));
        let unrelated_pid_file = temp_path(&format!("{label}-unrelated-pid"));
        let unrelated_stop_file = temp_path(&format!("{label}-unrelated-stop"));
        let unrelated_command = format!(
            "printf '%s' \"$$\" > {}; while [ ! -f {} ]; do :; done",
            shell_quote(&unrelated_pid_file),
            shell_quote(&unrelated_stop_file)
        );
        let mut unrelated_task = tokio::spawn({
            let sandbox = sandbox.clone();
            async move {
                sandbox
                    .output_command_with_limits(
                        &unrelated_command,
                        CommandLimits {
                            timeout: Duration::from_secs(5),
                            stdout_bytes: 128,
                            stderr_bytes: 128,
                            combined_bytes: 192,
                        },
                    )
                    .await
            }
        });
        let unrelated_pid = tokio::select! {
            pid = wait_for_pid(&unrelated_pid_file) => pid,
            result = &mut unrelated_task => {
                let result = result
                    .expect("unrelated task panicked before creating its pid file")
                    .expect("unrelated command runner failed before creating its pid file");
                panic!("unrelated command ended before creating its pid file: status={:?} stderr={}",
                    result.status, String::from_utf8_lossy(&result.stderr));
            }
        };

        let validator_command = format!(
            "(while :; do :; done) & child=$!; printf '%s' \"$child\" > {}; wait",
            shell_quote(&validator_pid_file)
        );
        let operation =
            start_with_limits(&sandbox, &validator_command, limits(Duration::from_secs(5)));
        let cancellation = operation.cancellation();
        let mut validation_task = tokio::spawn(operation.wait());

        let validator_pid = tokio::select! {
            pid = wait_for_pid(&validator_pid_file) => pid,
            result = &mut validation_task => {
                let result = result.expect("validation task panicked before creating its pid file");
                panic!("validation ended before creating its pid file: {}", result.render());
            }
        };
        wait_until(
            || sandbox.active_group_count() == 2,
            "validator and unrelated process groups",
        )
        .await;
        cancellation.cancel();
        let cancelled = tokio::time::timeout(Duration::from_secs(2), validation_task)
            .await
            .expect("cancelled validation remained blocked")
            .expect("validation task panicked");
        assert_eq!(cancelled.status, ValidationStatus::Cancelled);
        assert_process_gone(validator_pid).await;
        assert!(
            process_exists(unrelated_pid),
            "scoped validation cancellation killed an unrelated sandbox command"
        );
        assert!(!unrelated_task.is_finished());
        assert_eq!(sandbox.active_group_count(), 1);

        std::fs::write(&unrelated_stop_file, b"stop").unwrap();
        let unrelated = tokio::time::timeout(Duration::from_secs(2), unrelated_task)
            .await
            .expect("unrelated command did not finish")
            .expect("unrelated command task panicked")
            .expect("unrelated command runner failed");
        assert_eq!(unrelated.status, CommandStatus::Completed);
        assert_eq!(sandbox.active_group_count(), 0);

        let recovery =
            run_with_limits(&sandbox, "printf next", limits(Duration::from_secs(1))).await;
        assert_eq!(
            recovery.status,
            ValidationStatus::Success { exit_code: Some(0) }
        );
        let _ = std::fs::remove_file(validator_pid_file);
        let _ = std::fs::remove_file(unrelated_pid_file);
        let _ = std::fs::remove_file(unrelated_stop_file);
    }

    #[tokio::test]
    async fn loop_validation_process_limits_scoped_cancellation_preserves_unrelated_command() {
        assert_scoped_cancellation_preserves_unrelated_command(
            Sandbox::new(false, "bwrap"),
            "disabled",
        )
        .await;
    }

    #[cfg(target_os = "macos")]
    fn explicit_seatbelt_capability_denial(result: &ValidationResult) -> bool {
        if !matches!(
            result.status,
            ValidationStatus::Failed | ValidationStatus::NonZeroExit { .. }
        ) {
            return false;
        }
        let stderr = String::from_utf8_lossy(&result.stderr);
        stderr.contains("sandbox_apply: Operation not permitted")
            || stderr.contains("sandbox backend 'seatbelt' is not available")
            || stderr.contains("sandbox backend 'seatbelt' is not a trusted system executable")
    }

    #[cfg(target_os = "macos")]
    async fn seatbelt_capability_available(sandbox: &Sandbox) -> bool {
        let probe = run_with_limits(
            sandbox,
            "printf mini-agent-seatbelt-capability",
            limits(Duration::from_secs(1)),
        )
        .await;
        if probe.status == (ValidationStatus::Success { exit_code: Some(0) })
            && probe.stdout == b"mini-agent-seatbelt-capability"
        {
            return true;
        }
        if explicit_seatbelt_capability_denial(&probe) {
            return false;
        }
        panic!(
            "Seatbelt capability probe failed unexpectedly: {}",
            probe.render()
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn loop_validation_process_limits_seatbelt_scoped_cancellation_preserves_unrelated_command()
     {
        let sandbox = Sandbox::new(true, "seatbelt");
        if sandbox.policy() != crate::sandbox::SandboxPolicy::RequiredAndAvailable {
            return;
        }
        if !seatbelt_capability_available(&sandbox).await {
            return;
        }
        assert_scoped_cancellation_preserves_unrelated_command(sandbox, "seatbelt").await;
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn loop_validation_process_limits_seatbelt_probe_never_skips_cleanup_failures() {
        let cleanup_failure = ValidationResult {
            status: ValidationStatus::TimedOut,
            stdout: Vec::new(),
            stderr: b"sandbox_apply: Operation not permitted".to_vec(),
            limits: limits(Duration::from_secs(1)),
            diagnostic_truncated: false,
        };
        assert!(!explicit_seatbelt_capability_denial(&cleanup_failure));
    }

    #[tokio::test]
    async fn loop_validation_process_limits_dropped_parent_cleans_process_tree() {
        let pid_file = temp_path("drop-pid");
        let sandbox = Sandbox::new(false, "bwrap");
        let command = format!(
            "(while :; do sleep 1; done) & child=$!; printf '%s' \"$child\" > {}; wait",
            shell_quote(&pid_file)
        );
        let task = tokio::spawn({
            let sandbox = sandbox.clone();
            async move { run_with_limits(&sandbox, &command, limits(Duration::from_secs(5))).await }
        });

        let pid = wait_for_pid(&pid_file).await;
        task.abort();
        let _ = task.await;
        wait_until(
            || sandbox.active_group_count() == 0,
            "cancelled validation worker cleanup",
        )
        .await;
        assert_process_gone(pid).await;
        let _ = std::fs::remove_file(pid_file);
    }

    #[tokio::test]
    async fn loop_validation_process_limits_unavailable_sandbox_fails_closed() {
        let failure_limits = CommandLimits {
            timeout: Duration::from_secs(1),
            stdout_bytes: 512,
            stderr_bytes: 512,
            combined_bytes: 512,
        };
        let result = run_with_limits(
            &Sandbox::new(true, "__mini_agent_missing_loop_sandbox__"),
            "printf must-not-run",
            failure_limits,
        )
        .await;
        assert_eq!(result.status, ValidationStatus::Failed);
        let rendered = result.render();
        assert!(rendered.starts_with("[validation status=failed"));
        assert!(rendered.contains("requested-but-unavailable"));
        assert!(!rendered.contains("[stdout]"));

        let oversized_backend = format!("missing-{}", "x".repeat(4_096));
        let oversized = run_with_limits(
            &Sandbox::new(true, &oversized_backend),
            "printf must-not-run",
            failure_limits,
        )
        .await;
        assert_eq!(oversized.status, ValidationStatus::Failed);
        assert!(oversized.render().contains("diagnostic_truncated=true"));
        assert!(oversized.stderr.len() <= 512);

        let pre_cancelled = start_with_limits(
            &Sandbox::new(true, "__mini_agent_missing_loop_sandbox__"),
            "printf must-not-run",
            failure_limits,
        );
        pre_cancelled.cancellation().cancel();
        assert_eq!(
            pre_cancelled.wait().await.status,
            ValidationStatus::Cancelled
        );
    }

    #[test]
    fn loop_validation_process_limits_utf8_sanitization_cannot_expand_past_byte_budget() {
        let invalid = vec![0xff; 512];
        let (sanitized, truncated) = sanitize_bytes(&invalid, 64);
        assert!(sanitized.len() <= 64);
        assert!(std::str::from_utf8(sanitized.as_bytes()).is_ok());
        assert!(truncated);

        let command = format!("printf '\x1b[31mred\x1b[0m'; {}", "x".repeat(1_024));
        let displayed = display_command(&command);
        assert!(!displayed.contains('\x1b'));
        assert!(displayed.contains("command display truncated"));
        assert!(displayed.len() <= COMMAND_DISPLAY_BYTES + 32);
    }

    #[test]
    fn loop_validation_process_limits_headless_and_interactive_use_one_runner() {
        let headless = include_str!("headless.rs");
        let interactive = include_str!("../../ui/event_handler.rs");
        assert!(headless.contains("loop_mod::validation::start(sandbox, cmd)"));
        assert!(interactive.contains("loop::validation::start(&ui.sandbox, &cmd)"));
        assert!(!headless.contains("tokio::process::Command::new"));
        assert!(!interactive.contains("tokio::process::Command::new"));
    }
}
