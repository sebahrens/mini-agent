use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, BashArgs, JobAction, JobStatusArgs, PermCheck, ToolError, check_perm,
};
use crate::extras::truncate::head_tail_lines;
use crate::sandbox::{
    BackgroundJobOutput, CommandLimits, CommandOutput, CommandOutputLimit, CommandProgress,
    CommandStatus, DEFAULT_BACKGROUND_COMMAND_TIMEOUT, DEFAULT_COMMAND_LIMITS,
    MAX_BACKGROUND_JOB_WAIT_MILLIS, OutputChannel, OutputTranscript, SHELL_HARD_OUTPUT_BYTES,
    Sandbox, ShellOutputCapture, SpoolPolicy, TranscriptPart,
};
use compact_str::CompactString;

/// Minimum spacing of live progress previews for one foreground shell call.
const SHELL_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Trailing output lines a live progress preview shows.
pub(crate) const SHELL_PROGRESS_LINES: usize = 5;
/// Bound on announced-but-unclaimed shell calls kept in a run's scratchpad.
const MAX_PENDING_SHELL_CALLS: usize = 64;

tokio::task_local! {
    /// Internal id of the shell call executing in this task, when known.
    static SHELL_CALL_ID: Option<CompactString>;
}

/// Run one tool invocation with the internal call id it was announced under.
pub(crate) async fn scope_shell_call_id<F: std::future::Future>(
    id: Option<CompactString>,
    future: F,
) -> F::Output {
    SHELL_CALL_ID.scope(id, future).await
}

tokio::task_local! {
    /// Internal id of the non-shell tool call executing in this task (MCP
    /// calls use it to label their live progress preview).
    static TOOL_CALL_ID: Option<CompactString>;
}

/// Run one non-shell tool invocation with the internal call id it was
/// announced under.
pub(crate) async fn scope_tool_call_id<F: std::future::Future>(
    id: Option<CompactString>,
    future: F,
) -> F::Output {
    TOOL_CALL_ID.scope(id, future).await
}

/// The internal call id of the tool call running in this task, if known.
#[cfg(feature = "mcp")]
pub(crate) fn current_tool_call_id() -> Option<CompactString> {
    TOOL_CALL_ID.try_with(Clone::clone).ok().flatten()
}

/// Calls announced by [`ShellCallIdHook`] whose execution has not yet
/// claimed their id, keyed by tool name and exact argument text. This only labels a
/// display-only preview; identical concurrent commands may swap previews.
#[derive(Clone, Default)]
struct PendingShellCalls(
    std::collections::HashMap<String, std::collections::VecDeque<CompactString>>,
);

impl PendingShellCalls {
    fn len(&self) -> usize {
        self.0.values().map(std::collections::VecDeque::len).sum()
    }

    fn push(&mut self, args: &str, id: &str) {
        if self.len() >= MAX_PENDING_SHELL_CALLS {
            self.0.clear();
        }
        self.0
            .entry(args.to_owned())
            .or_default()
            .push_back(CompactString::from(id));
    }

    fn take(&mut self, args: &str) -> Option<CompactString> {
        let queue = self.0.get_mut(args)?;
        let id = queue.pop_front();
        if queue.is_empty() {
            self.0.remove(args);
        }
        id
    }

    fn forget(&mut self, args: &str, id: &str) {
        if let Some(queue) = self.0.get_mut(args) {
            queue.retain(|pending| pending != id);
            if queue.is_empty() {
                self.0.remove(args);
            }
        }
    }
}

fn pending_key(tool: &str, args: &str) -> String {
    format!("{tool}\0{args}")
}

/// Claim the internal id announced for the call to tool `tool` about to run
/// with the exact argument text `args`.
pub(crate) fn claim_call_id(
    extensions: &rig::tool::ToolCallExtensions,
    tool: &str,
    args: &str,
) -> Option<CompactString> {
    let key = pending_key(tool, args);
    extensions
        .get::<rig::agent::Scratchpad>()?
        .update(|pending: &mut PendingShellCalls| pending.take(&key))
}

/// Records each call's internal id in the run scratchpad so `shell` and MCP
/// tools can label their live progress with the id the transcript uses. Calls
/// that never execute (skipped by another hook) are forgotten on their result.
#[derive(Clone, Copy)]
pub(crate) struct ShellCallIdHook;

impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for ShellCallIdHook {
    async fn on_event(
        &self,
        context: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        match event {
            rig::agent::StepEvent::ToolCall {
                tool_name,
                internal_call_id,
                args,
                ..
            } => {
                let key = pending_key(tool_name, args);
                context
                    .scratchpad()
                    .update(|pending: &mut PendingShellCalls| pending.push(&key, internal_call_id));
            }
            rig::agent::StepEvent::ToolResult {
                tool_name,
                internal_call_id,
                args,
                ..
            } => {
                let key = pending_key(tool_name, args);
                context
                    .scratchpad()
                    .update(|pending: &mut PendingShellCalls| {
                        pending.forget(&key, internal_call_id)
                    });
            }
            _ => {}
        }
        rig::agent::Flow::cont()
    }

    fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
        matches!(
            kind,
            rig::agent::StepEventKind::ToolCall | rig::agent::StepEventKind::ToolResult
        )
    }
}

/// Run a foreground command, forwarding a throttled display-only preview of
/// its output as [`crate::event::AgentEvent::ToolProgress`]. The output
/// future wins every race, so no preview is sent once the command finished;
/// a full event channel drops the preview instead of slowing the command.
async fn output_with_progress(
    sandbox: &Sandbox,
    command: &str,
    limits: CommandLimits,
    capture: &ShellOutputCapture,
    (pty, separate_streams): (bool, bool),
    id: CompactString,
    events: tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
) -> std::io::Result<CommandOutput> {
    let progress = CommandProgress::new();
    let mut changes = progress.subscribe();
    let command_run = sandbox.output_shell_command_captured(
        command,
        limits,
        Some(progress.clone()),
        capture,
        pty,
        separate_streams,
    );
    tokio::pin!(command_run);
    let mut last_sent: Option<tokio::time::Instant> = None;
    let mut pending = false;
    loop {
        let due = last_sent.map(|sent| sent + SHELL_PROGRESS_INTERVAL);
        tokio::select! {
            biased;
            result = &mut command_run => return result,
            changed = changes.changed(), if !pending => {
                if changed.is_ok() {
                    pending = true;
                }
            }
            _ = async {
                if let Some(due) = due {
                    tokio::time::sleep_until(due).await;
                }
            }, if pending => {
                pending = false;
                last_sent = Some(tokio::time::Instant::now());
                let text = progress.tail_lines(SHELL_PROGRESS_LINES);
                if !text.is_empty() {
                    let _ = events.try_send(crate::event::AgentEvent::ToolProgress {
                        id: id.clone(),
                        text: CompactString::from(text),
                    });
                }
            }
        }
    }
}

/// Immutable observation of the actual foreground child, before display limits,
/// permission coaching or hooks rewrite model-visible text. Payload is bounded;
/// consumers must require complete successful output before interpreting it.
#[cfg(any(feature = "git-worktree", test))]
#[derive(Clone, Debug)]
pub(crate) struct ForegroundResult {
    pub status: CommandStatus,
    pub exit_code: Option<i32>,
    pub descendants_escaped: bool,
    pub stdout: Vec<u8>,
    pub stdout_complete: bool,
}

// Without the native Git/PR consumer there is no result observation to allocate.
#[cfg(not(any(feature = "git-worktree", test)))]
type ForegroundResult = std::convert::Infallible;

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
    foreground_timeout: std::time::Duration,
    /// Private directory (the session tool-output directory) receiving
    /// full-output logs when a command's output exceeds in-memory retention.
    output_log_dir: Option<std::path::PathBuf>,
    /// `shell_auto_background`: promote a foreground command that reaches
    /// its deadline to a background job instead of killing it.
    auto_background: bool,
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
            foreground_timeout: std::time::Duration::from_secs(
                crate::config::DEFAULT_SHELL_TIMEOUT_SECS,
            ),
            output_log_dir: None,
            auto_background: false,
        }
    }

    /// Default for the per-call `auto_background` flag.
    pub fn with_auto_background(mut self, enabled: bool) -> Self {
        self.auto_background = enabled;
        self
    }

    /// Spool oversized output to private logs in `dir` so the model can
    /// recover the omitted middle with `read`/`grep`.
    pub fn with_output_log_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.output_log_dir = dir;
        self
    }

    pub fn with_foreground_timeout_secs(mut self, seconds: u64) -> Self {
        self.foreground_timeout =
            std::time::Duration::from_secs(seconds.clamp(1, crate::config::MAX_SHELL_TIMEOUT_SECS));
        self
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
        let mut description = format!(
            "Execute a {dialect} command in the current working directory. Foreground commands have a hard \
             {} second deadline and bounded output. Set background=true for builds, test suites, or servers; \
             this returns a session-scoped job id for the job_status tool and uses a 24 hour maximum. The \
             optional timeout can lower the applicable deadline. Commands keep the same sandbox and permission \
             policy in either mode.",
            self.foreground_timeout.as_secs()
        );
        if let Some(alias) = self.sandbox.shell_cwd_alias() {
            description.push_str(&format!(
                " Sandboxed shell commands see this workspace at {}; file tools keep the working-directory path stated in the system prompt.",
                alias.display()
            ));
        }
        description
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
                    "minimum": 1,
                    "maximum": 86_400_000,
                    "description": format!("Lower command deadline in milliseconds (optional; maximum {} foreground or 86400000 background)", self.foreground_timeout.as_secs() * 1000)
                },
                "background": {
                    "type": "boolean",
                    "description": "Start a session-scoped background job and return its id (default false)"
                },
                "auto_background": {
                    "type": "boolean",
                    "description": "Foreground only: if the command reaches its deadline, keep it running as a background job (returning its job id and output so far) instead of killing it. Defaults to the shell_auto_background setting."
                },
                "separate_streams": {
                    "type": "boolean",
                    "description": "Capture stdout and stderr in separate pipes, labelled [stdout]/[stderr] in arrival order (interleaving across the two is approximate). Default false: on Unix both share one pipe, like 2>&1, so lines keep their exact write order and are unlabelled."
                },
                "pty": {
                    "type": "boolean",
                    "description": "Run under a pseudo-terminal so isatty() is true and stdin/stdout/stderr are one 120x40 TTY (Unix only; default false). With background=true, job_status action=input types into it, e.g. for REPLs or prompts."
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        self.execute(args).await.map(|(text, _)| text)
    }
    #[cfg(any(feature = "git-worktree", test))]
    async fn call_structured(
        &self,
        args: BashArgs,
        _extensions: &rig::tool::ToolCallExtensions,
    ) -> Result<rig::tool::ToolReturn<String>, ToolError> {
        self.execute(args).await.map(|(text, metadata)| {
            let result = rig::tool::ToolReturn::success(text);
            match metadata {
                Some(metadata) => result.with_extension(metadata),
                None => result,
            }
        })
    }
}

impl ShellTool {
    async fn execute(
        &self,
        args: BashArgs,
    ) -> Result<(String, Option<ForegroundResult>), ToolError> {
        tracing::debug!(
            "tool shell start: cmd_len={}, timeout={:?}, background={}",
            args.command.len(),
            args.timeout,
            args.background,
        );
        let maximum = if args.background {
            DEFAULT_BACKGROUND_COMMAND_TIMEOUT
        } else {
            self.foreground_timeout
        };
        if let Some(timeout) = args.timeout
            && (timeout == 0 || std::time::Duration::from_millis(timeout) > maximum)
        {
            let mode = if args.background {
                "background"
            } else {
                "foreground"
            };
            return Err(ToolError::Msg(format!(
                "shell timeout must be between 1 and {} milliseconds for {mode} commands. For longer foreground work, use a background job. Example: {{\"command\":\"cargo test\",\"background\":true,\"timeout\":120000}}",
                maximum.as_millis()
            )));
        }
        // The complete script is the permission key and is passed unchanged to
        // the shell. Never split or tokenize it: Bash can execute nested
        // programs from syntax that ad-hoc command splitting cannot classify.
        // The prompt gives the exclusive lane back; no effect precedes it and
        // the command captures no file identity, so nothing needs re-capture.
        let (coaching, _released) = super::concurrency::prompt_unlocked(check_perm(
            &self.permission,
            &self.ask_tx,
            "shell",
            &args.command,
        ))
        .await;
        let coaching = coaching?;

        if args.background {
            let timeout = args
                .timeout
                .map(std::time::Duration::from_millis)
                .unwrap_or(DEFAULT_BACKGROUND_COMMAND_TIMEOUT);
            let id = self
                .sandbox
                .start_background_command_logged(
                    args.command,
                    timeout,
                    self.output_log_dir.as_deref(),
                    args.pty,
                    args.separate_streams,
                    None,
                )
                .await?;
            let log = self
                .output_log_dir
                .as_ref()
                .map(|dir| {
                    format!(
                        "\nComplete output is logged to {}",
                        dir.join(format!("shell-{id}.log")).display()
                    )
                })
                .unwrap_or_default();
            let result = format!(
                "Background job started: {id}\nUse job_status with this id to poll output or stop it; each poll returns only output not returned before.{log}"
            );
            return Ok((
                match coaching {
                    Some(message) => format!("{message}\n\n{result}"),
                    None => result,
                },
                None,
            ));
        }

        let mut limits = DEFAULT_COMMAND_LIMITS;
        limits.timeout = self.foreground_timeout;
        if let Some(timeout_ms) = args.timeout {
            limits.timeout = std::time::Duration::from_millis(timeout_ms);
        }
        if args.auto_background.unwrap_or(self.auto_background) {
            let result = self.run_auto_background(&args, limits.timeout).await?;
            return Ok((
                match coaching {
                    Some(message) => format!("{message}\n\n{result}"),
                    None => result,
                },
                None,
            ));
        }
        // Retention never stops the child; only this ceiling does.
        limits.combined_bytes = usize::try_from(SHELL_HARD_OUTPUT_BYTES).unwrap_or(usize::MAX);
        let capture = ShellOutputCapture::new(SpoolPolicy::foreground_shell(
            self.output_log_dir
                .as_deref()
                .map(|dir| ShellOutputCapture::log_path_in(dir, "shell-output")),
        ));
        let live = SHELL_CALL_ID
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .zip(crate::agent::tools::mutation_diff::scoped_event_sender());
        let output = match live {
            Some((id, events)) => {
                output_with_progress(
                    &self.sandbox,
                    &args.command,
                    limits,
                    &capture,
                    (args.pty, args.separate_streams),
                    id,
                    events,
                )
                .await?
            }
            None => {
                self.sandbox
                    .output_shell_command_captured(
                        &args.command,
                        limits,
                        None,
                        &capture,
                        args.pty,
                        args.separate_streams,
                    )
                    .await?
            }
        };
        let transcript = capture.transcript();

        if output.status != CommandStatus::Completed {
            tracing::debug!("tool shell stopped before completion: {:?}", output.status);
            return Err(resource_limit_error(
                output.status,
                &transcript,
                limits,
                self.max_output_lines,
            ));
        }

        #[cfg(any(feature = "git-worktree", test))]
        const METADATA_STDOUT_BYTES: usize = 64 * 1024;
        #[cfg(any(feature = "git-worktree", test))]
        let metadata = {
            let stdout = transcript.channel_bytes(OutputChannel::Stdout);
            Some(ForegroundResult {
                status: output.status,
                exit_code: output.exit_status.and_then(|status| status.code()),
                descendants_escaped: output.descendants_escaped,
                stdout_complete: transcript.omitted_bytes == 0
                    && stdout.len() <= METADATA_STDOUT_BYTES,
                stdout: stdout[..stdout.len().min(METADATA_STDOUT_BYTES)].to_vec(),
            })
        };
        #[cfg(not(any(feature = "git-worktree", test)))]
        let metadata = None;
        let termination = render_termination(output.exit_status);
        if let Some(termination) = termination.as_deref() {
            tracing::debug!("tool shell: {termination}");
        }

        let output_len = transcript.total_bytes;
        let mut result = render_transcript(&transcript);
        if args.pty {
            // The log file keeps the raw bytes.
            result = normalize_terminal_output(&result);
        }
        if output.descendants_escaped {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(
                "[status: descendants_escaped; direct command result preserved; containment cleanup incomplete]",
            );
        }
        if let Some(termination) = termination {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&termination);
        }

        let mut result = bound_output_lines(result, self.max_output_lines);
        push_log_footer(&mut result, &transcript);

        let result = match coaching {
            Some(msg) => format!("{}\n\n{}", msg, result),
            None => result,
        };
        tracing::debug!(
            "tool shell done: status={:?}, output_len={}",
            output.exit_status,
            output_len,
        );
        Ok((result, metadata))
    }
}

/// Stops a job started for an auto-background call if the call is dropped
/// (turn interrupted) before the command finished or was promoted.
struct PendingAutoJob<'a> {
    sandbox: &'a Sandbox,
    id: String,
    armed: bool,
}

impl Drop for PendingAutoJob<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.sandbox.request_stop_background_job(&self.id);
        }
    }
}

impl ShellTool {
    /// Run a foreground command as a job so that reaching `deadline` promotes
    /// it to a background job (output preserved) instead of killing it. A
    /// command that finishes in time is reported like a foreground result and
    /// its job record is dropped.
    async fn run_auto_background(
        &self,
        args: &BashArgs,
        deadline: std::time::Duration,
    ) -> Result<String, ToolError> {
        let progress = CommandProgress::new();
        let id = self
            .sandbox
            .start_background_command_logged(
                args.command.clone(),
                DEFAULT_BACKGROUND_COMMAND_TIMEOUT,
                self.output_log_dir.as_deref(),
                args.pty,
                args.separate_streams,
                Some(progress.clone()),
            )
            .await?;
        let mut pending = PendingAutoJob {
            sandbox: &self.sandbox,
            id: id.clone(),
            armed: true,
        };
        let live = SHELL_CALL_ID
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .zip(crate::agent::tools::mutation_diff::scoped_event_sender());
        let started = tokio::time::Instant::now();
        let wait = async {
            loop {
                let remaining = deadline.saturating_sub(started.elapsed());
                let snapshot = self
                    .sandbox
                    .wait_background_job(&id, remaining)
                    .await
                    .map_err(ToolError::Msg)?;
                if snapshot.status.is_terminal() || started.elapsed() >= deadline {
                    return Ok::<_, ToolError>(snapshot);
                }
            }
        };
        let snapshot = match live {
            Some((call_id, events)) => forward_progress(&progress, wait, call_id, events).await?,
            None => wait.await?,
        };
        pending.armed = false;
        if !snapshot.status.is_terminal() {
            let so_far = self
                .sandbox
                .read_background_job_output(&id, None, JOB_OUTPUT_READ_BYTES)
                .map_err(ToolError::Msg)?;
            let rendered = render_background_job(so_far, self.max_output_lines);
            let deadline_ms = deadline.as_millis();
            return Ok(format!(
                "[status: promoted_to_background; job_id: {id}; the command reached its {deadline_ms} ms foreground deadline and keeps running as a background job. Use job_status with this id to poll (only new output), wait or stop it]\n{rendered}"
            ));
        }
        let transcript = snapshot.transcript.clone();
        self.sandbox.forget_background_job(&id);
        let status = match snapshot.status {
            crate::sandbox::BackgroundJobStatus::Completed => CommandStatus::Completed,
            crate::sandbox::BackgroundJobStatus::TimedOut => CommandStatus::TimedOut,
            crate::sandbox::BackgroundJobStatus::Cancelled => CommandStatus::Cancelled,
            crate::sandbox::BackgroundJobStatus::OutputLimitExceeded => {
                CommandStatus::OutputLimitExceeded(CommandOutputLimit::Combined)
            }
            _ => CommandStatus::Failed,
        };
        if status != CommandStatus::Completed {
            let mut limits = DEFAULT_COMMAND_LIMITS;
            limits.timeout = deadline;
            return Err(resource_limit_error(
                status,
                &transcript,
                limits,
                self.max_output_lines,
            ));
        }
        let mut result = render_transcript(&transcript);
        if args.pty {
            result = normalize_terminal_output(&result);
        }
        if snapshot.descendants_escaped {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(
                "[status: descendants_escaped; direct command result preserved; containment cleanup incomplete]",
            );
        }
        if let Some(code) = snapshot.exit_code.filter(|code| *code != 0) {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&format!("Exit code: {code}"));
        }
        let mut result = bound_output_lines(result, self.max_output_lines);
        push_log_footer(&mut result, &transcript);
        Ok(result)
    }
}

/// Forward throttled display-only previews of `progress` while `work` runs.
async fn forward_progress<T>(
    progress: &CommandProgress,
    work: impl std::future::Future<Output = T>,
    id: CompactString,
    events: tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
) -> T {
    let mut changes = progress.subscribe();
    tokio::pin!(work);
    let mut last_sent: Option<tokio::time::Instant> = None;
    let mut pending = false;
    loop {
        let due = last_sent.map(|sent| sent + SHELL_PROGRESS_INTERVAL);
        tokio::select! {
            biased;
            result = &mut work => return result,
            changed = changes.changed(), if !pending => {
                if changed.is_ok() {
                    pending = true;
                }
            }
            _ = async {
                if let Some(due) = due {
                    tokio::time::sleep_until(due).await;
                }
            }, if pending => {
                pending = false;
                last_sent = Some(tokio::time::Instant::now());
                let text = progress.tail_lines(SHELL_PROGRESS_LINES);
                if !text.is_empty() {
                    let _ = events.try_send(crate::event::AgentEvent::ToolProgress {
                        id: id.clone(),
                        text: CompactString::from(text),
                    });
                }
            }
        }
    }
}

pub struct JobStatusTool {
    sandbox: Sandbox,
    max_output_lines: Option<u64>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl JobStatusTool {
    pub fn new(sandbox: Sandbox, max_output_lines: Option<u64>) -> Self {
        Self {
            sandbox,
            max_output_lines,
            permission: None,
            ask_tx: None,
        }
    }

    /// Judge typed terminal input (`action=input`) like a shell command.
    pub fn with_permission(
        mut self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        self.permission = permission;
        self.ask_tx = ask_tx;
        self
    }

    /// Wait until the job's output goes quiet after input, bounded.
    async fn settle_after_input(&self, id: &str) {
        let deadline = tokio::time::Instant::now() + INPUT_SETTLE_MAX;
        let mut seen = self
            .sandbox
            .background_job_snapshot(id)
            .map(|snapshot| snapshot.transcript.total_bytes);
        while tokio::time::Instant::now() < deadline {
            let Ok(snapshot) = self
                .sandbox
                .wait_background_job(id, INPUT_QUIET_WINDOW)
                .await
            else {
                return;
            };
            let total = snapshot.transcript.total_bytes;
            if snapshot.status.is_terminal() || seen.as_ref().is_ok_and(|seen| *seen == total) {
                return;
            }
            seen = Ok(total);
        }
    }
}

/// Output must be silent this long after typed input before the result is
/// returned.
const INPUT_QUIET_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);
/// Longest `action=input` waits for the program's reply to settle.
const INPUT_SETTLE_MAX: std::time::Duration = std::time::Duration::from_secs(3);

impl Tool for JobStatusTool {
    const NAME: &'static str = "job_status";

    type Error = ToolError;
    type Args = JobStatusArgs;
    type Output = String;

    fn description(&self) -> String {
        "Poll, wait for, stop, or list session-scoped background shell jobs. Poll, wait and stop return only output produced since the previous poll/wait/stop of that job (at most 64 KiB per call; a larger range shows its start and end around a byte gap), with the byte offset to continue from. Pass since=<offset> to re-read the complete stream from any offset (for example since=0). The complete output is also logged to the file named in the result, readable with read/grep, and the log outlives the job record. Wait returns when the job ends or timeout_ms expires (default 10000, maximum 60000 milliseconds); expiration leaves the job running. List returns ids, commands, statuses and log paths without an id. Stop waits up to 5 seconds for process-tree cleanup."
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
                    "enum": ["poll", "stop", "wait", "list", "input"],
                    "description": "Poll status/output, wait for completion, stop and reap, list current session jobs, or type input into a pty=true job (default poll)"
                },
                "input": {
                    "type": "string",
                    "maxLength": 16384,
                    "description": "Only with action=input: text written to the job's terminal as typed, at most 16384 bytes; include \\n to press Enter. The text is permission-checked like a shell command. Writing gives up after 5 seconds if the program is not reading; the result shows the output produced until it goes quiet (at most 3 seconds)."
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_BACKGROUND_JOB_WAIT_MILLIS,
                    "description": "Only with action=wait: maximum wait in milliseconds (default 10000). Expiration does not stop the job."
                },
                "since": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte offset in the job's complete output to read from instead of the end of the previous read (poll, wait or stop)"
                }
            },
            "required": []
        })
    }

    async fn call(&self, args: JobStatusArgs) -> Result<String, ToolError> {
        if let Some(timeout) = args.timeout_ms
            && (args.action != JobAction::Wait
                || !(1..=MAX_BACKGROUND_JOB_WAIT_MILLIS).contains(&timeout))
        {
            return Err(ToolError::Msg(
                "job_status timeout_ms is only valid with action=wait and must be between 1 and 60000 milliseconds. Example: {\"id\":\"job-...\",\"action\":\"wait\",\"timeout_ms\":10000}".into(),
            ));
        }
        if args.input.is_some() != (args.action == JobAction::Input) {
            return Err(ToolError::Msg(
                "job_status input is required with, and only valid with, action=input. Example: {\"id\":\"job-...\",\"action\":\"input\",\"input\":\"print(1+1)\\n\"}".into(),
            ));
        }
        if args.action == JobAction::List {
            if args.id.is_some() {
                return Err(ToolError::Msg(
                    "job_status action=list does not take an id. Example: {\"action\":\"list\"}"
                        .into(),
                ));
            }
            let summaries = self.sandbox.background_job_summaries();
            if summaries.is_empty() {
                return Ok("No background jobs in this session.".into());
            }
            return Ok(summaries
                .into_iter()
                .map(|job| {
                    let exit = job
                        .exit_code
                        .map_or_else(String::new, |code| format!("; exit {code}"));
                    let log = job
                        .log_path
                        .map(|path| format!("\n  log: {}", path.display()))
                        .unwrap_or_default();
                    format!(
                        "{}: {}{exit}\n  {}{log}",
                        job.id,
                        job.status.as_str(),
                        job.command
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"));
        }
        let id = args.id.as_deref().ok_or_else(|| ToolError::Msg(
            "job_status requires an id for poll, wait or stop. Example: {\"id\":\"job-...\",\"action\":\"wait\"}".into(),
        ))?;
        match args.action {
            JobAction::Poll => self.sandbox.background_job_snapshot(id).map(drop),
            JobAction::Stop => self.sandbox.stop_background_job(id).await.map(drop),
            JobAction::Wait => self
                .sandbox
                .wait_background_job(
                    id,
                    std::time::Duration::from_millis(args.timeout_ms.unwrap_or(10_000)),
                )
                .await
                .map(drop),
            JobAction::Input => {
                let input = args.input.as_deref().unwrap_or_default();
                // Typed text can run anything the terminal's program accepts,
                // so it is judged exactly like a shell script: deny rules
                // apply, and manual mode prompts. Unknown or finished jobs are
                // rejected before any prompt.
                self.sandbox
                    .background_job_input_handle(id, input.len())
                    .map(drop)
                    .map_err(ToolError::Msg)?;
                let (checked, _released) = super::concurrency::prompt_unlocked(check_perm(
                    &self.permission,
                    &self.ask_tx,
                    "shell",
                    // Typing Enter is not part of the command text a rule
                    // would name.
                    input.trim_end_matches(['\n', '\r']),
                ))
                .await;
                checked?;
                match self
                    .sandbox
                    .write_background_job_input(id, input.as_bytes())
                    .await
                {
                    Ok(()) => {
                        // Give the program time to echo and answer.
                        self.settle_after_input(id).await;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            JobAction::List => unreachable!("list returned before id lookup"),
        }
        .map_err(ToolError::Msg)?;
        let output = self
            .sandbox
            .read_background_job_output(id, args.since, JOB_OUTPUT_READ_BYTES)
            .map_err(ToolError::Msg)?;
        Ok(render_background_job(output, self.max_output_lines))
    }
}

/// Source-compatibility alias for integrations that still construct the old
/// Rust type. The model-visible tool name is always `shell`.
#[cfg(test)]
pub type BashTool = ShellTool;

fn render_termination(status: Option<std::process::ExitStatus>) -> Option<String> {
    let status = status?;
    if let Some(code) = status.code() {
        return (code != 0).then(|| format!("Exit code: {code}"));
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(number) = status.signal() {
            let name = nix::sys::signal::Signal::try_from(number)
                .map(|signal| format!("{signal:?}"))
                .unwrap_or_else(|_| "UNKNOWN".to_string());
            return Some(format!("Terminated by signal: {name} ({number})"));
        }
    }

    Some("Process terminated without an exit code".to_string())
}

/// Make terminal output model-readable: drop CSI/OSC/DCS and other escape
/// sequences and stray control bytes, and collapse carriage-return redraws
/// (progress bars) so each line keeps only its final state.
pub(crate) fn normalize_terminal_output(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates until a final byte.
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC, DCS, SOS, PM and APC: until BEL or the string
                // terminator (ESC \).
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Charset selection and similar: intermediates, then a final.
                Some(' '..='/') => {
                    for next in chars.by_ref() {
                        if !(' '..='/').contains(&next) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\t' | '\r' => stripped.push(ch),
            ch if ch.is_control() => {}
            ch => stripped.push(ch),
        }
    }
    // Per line: CRLF is a newline; a bare CR rewinds, so only the last
    // non-empty segment survives.
    let mut out = String::with_capacity(stripped.len());
    for (index, line) in stripped.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        out.push_str(
            line.rsplit('\r')
                .find(|segment| !segment.is_empty())
                .unwrap_or(""),
        );
    }
    out
}

/// Bytes of new job output one `job_status` call returns.
const JOB_OUTPUT_READ_BYTES: usize = 64 * 1024;

fn render_background_job(output: BackgroundJobOutput, max_output_lines: Option<u64>) -> String {
    let snapshot = &output.snapshot;
    let mut result = format!(
        "Job: {}\nStatus: {}\nCommand: {}",
        snapshot.id,
        snapshot.status.as_str(),
        snapshot.command
    );
    if let Some(exit_code) = snapshot.exit_code {
        result.push_str(&format!("\nExit code: {exit_code}"));
    }
    if snapshot.descendants_escaped {
        result.push_str(
            "\nContainment warning: descendants escaped process-group cleanup; direct command result preserved",
        );
    }
    let transcript = &snapshot.transcript;
    if let Some(log) = &transcript.log {
        let state = if log.complete {
            ""
        } else {
            " (log size limit reached; later output is not logged)"
        };
        result.push_str(&format!("\nLog: {}{state}", log.path.display()));
    } else if let Some(error) = &transcript.log_error {
        result.push_str(&format!("\nLog: unavailable ({error})"));
    }
    let total = transcript.total_bytes;
    let window = &output.window;
    if window.start == window.next {
        result.push_str(&format!(
            "\nNo new output (total {total} bytes; pass since=0 to re-read from the start)."
        ));
        return result;
    }
    let lossy = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes);
        if snapshot.pty {
            normalize_terminal_output(&text)
        } else {
            text.into_owned()
        }
    };
    let mut text = lossy(&window.head);
    if window.skipped > 0 {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let resume = window.start + window.head.len() as u64;
        let hint = if transcript.log.is_some() {
            format!("read the log or pass since={resume} to page through them")
        } else {
            "they were not retained and no log is configured".to_string()
        };
        text.push_str(&format!(
            "[... {} bytes skipped; {hint} ...]\n",
            window.skipped
        ));
    }
    text.push_str(&lossy(&window.tail));
    let stripped = if snapshot.pty {
        "; terminal escapes removed and carriage-return redraws collapsed, the log keeps the raw bytes"
    } else {
        ""
    };
    result.push_str(&format!(
        "\nOutput (bytes {}..{} of {total}; next poll continues at since={}{stripped}):\n",
        window.start, window.next, window.next
    ));
    let range_hint = match &transcript.log {
        Some(log) => format!(
            "the omitted lines are inside bytes {}..{} of this job's output; read them from the log {}",
            window.start,
            window.next,
            log.path.display()
        ),
        None => format!(
            "the omitted lines are inside bytes {}..{} of this job's output and were not retained; pass since={} to re-read this window",
            window.start, window.next, window.start
        ),
    };
    result.push_str(&bound_output_lines_with_hint(
        text,
        max_output_lines,
        &range_hint,
    ));
    result
}

/// Bound `text` to at most `max_output_lines` lines, keeping the head and a
/// short tail around an omitted-count marker so the model sees how the output
/// starts and ends (exit codes and final errors live at the end). `None`
/// returns the text unchanged.
fn bound_output_lines(text: String, max_output_lines: Option<u64>) -> String {
    bound_output_lines_with_hint(
        text,
        max_output_lines,
        "re-run with a narrower invocation or pipe through `head`/`tail`/`grep` to see the rest",
    )
}

fn bound_output_lines_with_hint(text: String, max_output_lines: Option<u64>, hint: &str) -> String {
    let Some(cap) = max_output_lines else {
        return text;
    };
    let cap = usize::try_from(cap).unwrap_or(usize::MAX).max(1);
    // Keep roughly 80 % of the budget for the head and 20 % for the tail.
    let tail = (cap / 5).max(1);
    let head = cap - tail;
    head_tail_lines(&text, head, tail, |total| {
        let omitted = total - head - tail;
        format!(
            "\n\n[... {omitted} lines omitted — showing the first {head} and last {tail} of {total} lines; \
             {hint} ...]\n"
        )
    })
    .unwrap_or(text)
}

fn resource_limit_error(
    status: CommandStatus,
    transcript: &OutputTranscript,
    limits: CommandLimits,
    max_output_lines: Option<u64>,
) -> ToolError {
    let metadata = match status {
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
            format!(
                "[status: output_truncated; limit: {name}; byte_cap: {bytes}; the command was stopped because its total output reached this hard ceiling]"
            )
        }
        CommandStatus::Failed => "[status: failed]".to_string(),
        CommandStatus::Completed => unreachable!("completed output is handled before this helper"),
    };
    let mut partial = bound_output_lines(render_transcript(transcript), max_output_lines);
    push_log_footer(&mut partial, transcript);
    let message = if partial.is_empty() {
        metadata
    } else {
        format!(
            "{metadata}\nCaptured output below is partial and must not be treated as a complete command result:\n{partial}"
        )
    };
    ToolError::Msg(message)
}

/// Render a spooled transcript in arrival order. A `[stdout]`/`[stderr]`
/// label starts each run of one separately captured stream; merged
/// (`2>&1`/pty) output is unlabelled. The dropped middle is marked in place.
fn render_transcript(transcript: &OutputTranscript) -> String {
    let mut result = String::new();
    let mut current: Option<OutputChannel> = None;
    for part in &transcript.parts {
        match part {
            TranscriptPart::Output(channel, bytes) => {
                if current != Some(*channel) {
                    let label = match channel {
                        OutputChannel::Stdout => Some("[stdout]\n"),
                        OutputChannel::Stderr => Some("[stderr]\n"),
                        OutputChannel::Merged => None,
                    };
                    if let Some(label) = label {
                        if !result.is_empty() && !result.ends_with('\n') {
                            result.push('\n');
                        }
                        result.push_str(label);
                    }
                    current = Some(*channel);
                }
                result.push_str(&String::from_utf8_lossy(bytes));
            }
            TranscriptPart::Omitted(bytes) => {
                if !result.is_empty() && !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push_str(&format!(
                    "[... {bytes} bytes of output omitted between the retained head and tail ...]\n"
                ));
            }
        }
    }
    result
}

/// Tell the model where the complete output lives when bytes were dropped,
/// or that it could not be saved.
fn push_log_footer(result: &mut String, transcript: &OutputTranscript) {
    let footer = if let Some(log) = &transcript.log {
        let completeness = if log.complete {
            "complete output"
        } else {
            "output up to the log size limit"
        };
        format!(
            "[{} bytes of output in total; {completeness} ({} bytes) saved to {}; use read with offset/limit or character_offset, or grep, to inspect it]",
            transcript.total_bytes,
            log.bytes,
            log.path.display()
        )
    } else if transcript.omitted_bytes > 0 {
        let reason = transcript.log_error.as_deref().map_or_else(
            || "no output log is configured".to_string(),
            |error| format!("the output log could not be written: {error}"),
        );
        format!(
            "[{} bytes of output in total; the omitted middle is not recoverable because {reason}; re-run with a narrower invocation or redirect to a file]",
            transcript.total_bytes
        )
    } else {
        return;
    };
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    result.push_str(&footer);
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn job_line_bound_names_the_omitted_byte_range() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let bounded = bound_output_lines_with_hint(text, Some(10), "bytes 0..900 are in the log");
        assert!(bounded.contains("lines omitted"), "{bounded}");
        assert!(bounded.contains("bytes 0..900 are in the log"), "{bounded}");
        assert!(crate::session::tool_result_is_self_bounded("job_status"));
    }

    fn test_tool() -> BashTool {
        BashTool::new(None, None, Sandbox::new(false, "bwrap"), None)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_progress_streams_before_completion_and_keeps_the_result() {
        let command = "echo a; sleep 1; echo b";
        let tool = test_tool();
        let args = || BashArgs {
            command: command.to_string(),
            timeout: None,
            background: false,
            ..Default::default()
        };
        let plain = tool.execute(args()).await.unwrap().0;

        let (events, mut received) = tokio::sync::mpsc::channel(64);
        let streamed = crate::agent::tools::mutation_diff::scope_events(
            events,
            scope_shell_call_id(Some(CompactString::from("call-1")), tool.execute(args())),
        );
        tokio::pin!(streamed);
        let first = tokio::select! {
            result = &mut streamed => panic!("command finished before any progress: {result:?}"),
            event = received.recv() => event.unwrap(),
        };
        match first {
            crate::event::AgentEvent::ToolProgress { id, text } => {
                assert_eq!(id, "call-1");
                assert_eq!(text, "a");
            }
            other => panic!("unexpected event {other:?}"),
        }
        let streamed = streamed.await.unwrap().0;
        assert_eq!(streamed, plain, "the model-visible result is unchanged");
        while let Ok(event) = received.try_recv() {
            assert!(matches!(
                event,
                crate::event::AgentEvent::ToolProgress { ref id, .. } if id == "call-1"
            ));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sdk_shell_progress_is_labelled_with_the_announced_internal_call_id() {
        use futures::StreamExt;
        use rig::agent::{AgentBuilder, MultiTurnStreamItem};
        use rig::streaming::{StreamedAssistantContent, StreamingChat};
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};

        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "provider-shell",
                    "shell",
                    serde_json::json!({"command": "echo a; sleep 1; echo b"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let tools = crate::agent::tools::concurrency::bind(vec![
            Box::new(test_tool()) as Box<dyn rig::tool::ToolDyn>
        ]);
        let agent = AgentBuilder::new(model)
            .tools(tools)
            .add_hook(ShellCallIdHook)
            .build();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let mut announced = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::agent::tools::mutation_diff::scope_events(sender, async {
                let mut stream = agent
                    .stream_chat("run", Vec::<rig::completion::Message>::new())
                    .max_turns(2)
                    .await;
                while let Some(item) = stream.next().await {
                    if let MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ToolCall {
                            internal_call_id, ..
                        },
                    ) = item.unwrap()
                    {
                        announced.push(internal_call_id);
                    }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(announced.len(), 1);
        let mut previews = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let crate::event::AgentEvent::ToolProgress { id, text } = event {
                previews.push((id.to_string(), text.to_string()));
            }
        }
        assert!(
            previews
                .iter()
                .any(|(id, text)| *id == announced[0] && text == "a"),
            "expected an early preview labelled {}: {previews:?}",
            announced[0]
        );
        assert!(previews.iter().all(|(id, _)| *id == announced[0]));
    }

    #[tokio::test]
    async fn shell_call_ids_are_claimed_by_exact_arguments_in_announcement_order() {
        let pad = rig::agent::Scratchpad::default();
        pad.update(|pending: &mut PendingShellCalls| {
            pending.push(&pending_key("shell", r#"{"command":"ls"}"#), "first");
            pending.push(&pending_key("shell", r#"{"command":"pwd"}"#), "other");
            pending.push(&pending_key("shell", r#"{"command":"ls"}"#), "second");
            pending.push(&pending_key("mcp_tool", r#"{"command":"ls"}"#), "mcp");
            pending.push(&pending_key("shell", r#"{"command":"ls"}"#), "skipped");
            pending.forget(&pending_key("shell", r#"{"command":"ls"}"#), "skipped");
        });
        let mut extensions = rig::tool::ToolCallExtensions::new();
        extensions.insert(pad);
        let claim = |args| claim_call_id(&extensions, "shell", args);
        assert_eq!(
            claim_call_id(&extensions, "mcp_tool", r#"{"command":"ls"}"#).as_deref(),
            Some("mcp")
        );
        assert_eq!(claim(r#"{"command":"ls"}"#).as_deref(), Some("first"));
        assert_eq!(claim(r#"{"command":"ls"}"#).as_deref(), Some("second"));
        assert_eq!(claim(r#"{"command":"ls"}"#), None);
        assert_eq!(claim(r#"{"command":"pwd"}"#).as_deref(), Some("other"));
        assert_eq!(
            claim_call_id(&rig::tool::ToolCallExtensions::new(), "shell", "{}"),
            None
        );
    }

    #[cfg(unix)]
    struct GatedJobFixture {
        shell: ShellTool,
        gate: crate::tests::process_gate::ProcessGate,
        id: String,
        _directory: crate::tests::OwnedTestDirectory,
    }

    #[cfg(unix)]
    impl GatedJobFixture {
        async fn start() -> Self {
            let directory = crate::tests::OwnedTestDirectory::new("job-wait-list");
            let path = directory.path().join("release.fifo");
            let gate = crate::tests::process_gate::ProcessGate::new(&path).unwrap();
            let shell = test_tool();
            let mut fixture = Self {
                shell,
                gate,
                id: String::new(),
                _directory: directory,
            };
            let started = fixture
                .shell
                .call(BashArgs {
                    command: format!(
                        "printf JOB_READY; IFS= read -r release < {}; printf JOB_FINISHED",
                        shell_quote(&path)
                    ),
                    timeout: Some(30_000),
                    background: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            fixture.id = background_id(&started);
            let ready = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let snapshot = fixture
                        .shell
                        .sandbox
                        .background_job_snapshot(&fixture.id)
                        .unwrap();
                    if String::from_utf8_lossy(
                        &snapshot.transcript.channel_bytes(OutputChannel::Stdout),
                    )
                    .contains("JOB_READY")
                    {
                        break;
                    }
                    assert!(
                        !snapshot.status.is_terminal(),
                        "fixture ended before readiness"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            ready.expect("background fixture readiness deadline");
            fixture
        }

        async fn stop(&self) {
            let snapshot = tokio::time::timeout(
                Duration::from_secs(10),
                self.shell.sandbox.stop_background_job(&self.id),
            )
            .await
            .expect("owned job cleanup deadline")
            .unwrap();
            assert!(snapshot.status.is_terminal(), "owned job did not retire");
        }
    }

    #[cfg(unix)]
    impl Drop for GatedJobFixture {
        fn drop(&mut self) {
            let _ = self.gate.release();
            self.shell.sandbox.kill_active();
            self.shell.sandbox.cancel_background_jobs();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_foreground_default_expires_and_reaps_without_a_call_timeout() {
        let root = crate::tests::OwnedTestDirectory::new("configured-shell-expiry");
        let path = root.path().join("release.fifo");
        let _gate = crate::tests::process_gate::ProcessGate::new(&path).unwrap();
        let tool = test_tool().with_foreground_timeout_secs(1);
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            tool.call(BashArgs {
                command: format!(
                    "printf CONFIGURED_READY; IFS= read -r release < {}",
                    shell_quote(&path)
                ),
                timeout: None,
                background: false,
                ..Default::default()
            }),
        )
        .await
        .expect("configured default must impose its own finite deadline")
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed_out; timeout_ms: 1000"), "{error}");
        assert!(
            error.contains("CONFIGURED_READY"),
            "the real command must have reached its gate"
        );
        assert_eq!(
            tool.sandbox.active_group_count(),
            0,
            "deadline result must follow native cleanup"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_foreground_deadline_accepts_longer_calls_and_publishes_the_cap() {
        let tool = test_tool().with_foreground_timeout_secs(u64::MAX);
        assert!(tool.description().contains("600 second deadline"));
        assert!(
            tool.parameters()["properties"]["timeout"]["description"]
                .as_str()
                .unwrap()
                .contains("600000 foreground")
        );
        let output = tool
            .call(BashArgs {
                command: "printf CONFIGURED_DEADLINE".into(),
                timeout: Some(60_001),
                background: false,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(output.contains("CONFIGURED_DEADLINE"));
        assert!(
            tool.call(BashArgs {
                command: "must-not-execute".into(),
                timeout: Some(600_001),
                background: false,
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("600000")
        );
    }

    #[tokio::test]
    async fn job_wait_and_list_arguments_fail_before_job_lookup_when_invalid() {
        let tool = JobStatusTool::new(test_tool().sandbox, None);
        for action in [JobAction::Poll, JobAction::Stop, JobAction::Wait] {
            assert!(
                tool.call(JobStatusArgs {
                    id: None,
                    action,
                    timeout_ms: None,
                    since: None,
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("requires an id")
            );
        }
        for (action, timeout_ms) in [
            (JobAction::Wait, 0),
            (JobAction::Wait, 60_001),
            (JobAction::Poll, 1),
            (JobAction::List, 1),
        ] {
            assert!(
                tool.call(JobStatusArgs {
                    id: Some("not-a-job-id".into()),
                    action,
                    timeout_ms: Some(timeout_ms),
                    since: None,
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("timeout_ms")
            );
        }
        assert!(
            tool.call(JobStatusArgs {
                id: Some("ignored-id".into()),
                action: JobAction::List,
                timeout_ms: None,
                since: None,
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("does not take an id")
        );
        let args: JobStatusArgs =
            serde_json::from_value(serde_json::json!({"action":"list"})).unwrap();
        assert_eq!(
            tool.call(args).await.unwrap(),
            "No background jobs in this session."
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn job_wait_wakes_on_completion_and_list_survives_transcript_compaction() {
        let mut fixture = GatedJobFixture::start().await;
        let tool = JobStatusTool::new(fixture.shell.sandbox.clone(), None);
        let wait = tool.call(JobStatusArgs {
            id: Some(fixture.id.clone()),
            action: JobAction::Wait,
            timeout_ms: Some(60_000),
            since: None,
            ..Default::default()
        });
        tokio::pin!(wait);
        assert!(
            futures::poll!(wait.as_mut()).is_pending(),
            "gated job must still be running"
        );
        let sibling_wait = tool.call(JobStatusArgs {
            id: Some(fixture.id.clone()),
            action: JobAction::Wait,
            timeout_ms: Some(60_000),
            since: None,
            ..Default::default()
        });
        tokio::pin!(sibling_wait);
        assert!(futures::poll!(sibling_wait.as_mut()).is_pending());
        fixture.gate.release().unwrap();
        let (output, sibling) = tokio::time::timeout(
            Duration::from_secs(15),
            futures::future::join(wait, sibling_wait),
        )
        .await
        .expect("completion must wake every waiter before its 60-second wait");
        let output = output.unwrap();
        assert!(sibling.unwrap().contains("Status: completed"));
        assert!(
            output.contains("Status: completed") && output.contains("JOB_FINISHED"),
            "{output}"
        );
        let mut session = crate::session::Session::new("fixture", "model", 128_000, "");
        session.add_message(crate::session::MessageRole::User, &fixture.id);
        session.compress("job ids omitted from summary".into(), 1, 0);
        assert!(
            !session
                .messages
                .iter()
                .any(|message| message.content.contains(&fixture.id))
        );
        let rebuilt = JobStatusTool::new(fixture.shell.sandbox.clone(), None);
        let list = rebuilt
            .call(JobStatusArgs {
                id: None,
                action: JobAction::List,
                timeout_ms: None,
                since: None,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            list.contains(&fixture.id) && list.contains("completed; exit 0"),
            "{list}"
        );
        assert!(
            !list.contains("[stdout]"),
            "listing does not copy stored output"
        );
        let foreign = JobStatusTool::new(test_tool().sandbox, None);
        assert_eq!(
            foreign
                .call(JobStatusArgs {
                    id: None,
                    action: JobAction::List,
                    timeout_ms: None,
                    since: None,
                    ..Default::default()
                })
                .await
                .unwrap(),
            "No background jobs in this session."
        );
        fixture.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn job_wait_expiration_and_caller_drop_leave_the_job_running() {
        let fixture = GatedJobFixture::start().await;
        let tool = JobStatusTool::new(fixture.shell.sandbox.clone(), None);
        {
            let wait = tool.call(JobStatusArgs {
                id: Some(fixture.id.clone()),
                action: JobAction::Wait,
                timeout_ms: Some(60_000),
                since: None,
                ..Default::default()
            });
            tokio::pin!(wait);
            assert!(futures::poll!(wait.as_mut()).is_pending());
        }
        tokio::time::pause();
        let output = tool
            .call(JobStatusArgs {
                id: Some(fixture.id.clone()),
                action: JobAction::Wait,
                timeout_ms: Some(25),
                since: None,
                ..Default::default()
            })
            .await
            .unwrap();
        tokio::time::resume();
        assert!(
            output.contains("Status: running") && output.contains("JOB_READY"),
            "{output}"
        );
        assert_eq!(fixture.shell.sandbox.running_background_job_count(), 1);
        fixture.stop().await;
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

    /// Readiness polling only: returns as soon as the fixture has written.
    /// The budget covers process start-up on a heavily loaded host.
    async fn wait_for_nonempty_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
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
                    id: Some(id.to_string()),
                    timeout_ms: None,
                    since: Some(0),
                    action: JobAction::Poll,
                    ..Default::default()
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

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_resource_limits_infinite_command_uses_lower_deadline() {
        let started = Instant::now();
        let error = test_tool()
            .call(BashArgs {
                command: "while :; do :; done".to_string(),
                timeout: Some(100),
                background: false,
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.contains("[status: timed_out; timeout_ms: 100]"));
    }

    #[tokio::test]
    async fn out_of_range_shell_timeouts_are_rejected_before_starting_a_job() {
        let tool = test_tool();
        for (background, maximum) in [(false, 120_000), (true, 86_400_000)] {
            for timeout in [0, maximum + 1, u64::MAX] {
                let error = tool
                    .call(BashArgs {
                        command: "printf must-not-run".into(),
                        timeout: Some(timeout),
                        background,
                        ..Default::default()
                    })
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(&format!("between 1 and {maximum} milliseconds")));
                assert!(error.contains("Example:"));
                assert_eq!(tool.sandbox.running_background_job_count(), 0);
            }
        }
    }

    #[tokio::test]
    async fn zero_timeout_is_rejected_instead_of_becoming_an_instant_deadline() {
        let error = test_tool()
            .call(BashArgs {
                command: "printf should-not-run".to_string(),
                timeout: Some(0),
                background: false,
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("between 1 and 120000 milliseconds"));
        assert!(error.contains(r#"{"command":"cargo test","background":true,"timeout":120000}"#));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_resource_limits_timeout_kills_descendant() {
        // The descendant must exist before the timeout for this to test its
        // cleanup. On a loaded host 200 ms may pass before the shell even
        // forks it; such an attempt proves nothing, so retry with a doubled
        // timeout until the descendant has recorded its complete pid line.
        for attempt in 0..6u32 {
            let timeout_ms = 200u64 << attempt;
            let pid_file = std::env::temp_dir().join(format!(
                "mini-agent-bash-timeout-descendant-{}-{attempt}",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&pid_file);
            let command = format!(
                "sh -c 'printf \"%s\\n\" \"$$\" > {}; while :; do sleep 1; done' & wait",
                shell_quote(&pid_file)
            );

            let error = test_tool()
                .call(BashArgs {
                    command,
                    timeout: Some(timeout_ms),
                    background: false,
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("[status: timed_out; timeout_ms: {timeout_ms}]")),
                "{error}"
            );

            let recorded = std::fs::read_to_string(&pid_file).unwrap_or_default();
            let _ = std::fs::remove_file(&pid_file);
            let Some(pid) = recorded
                .strip_suffix('\n')
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                continue;
            };
            wait_for_process_exit(pid).await;
            return;
        }
        panic!("the descendant never started before the shell timeout");
    }

    #[cfg(unix)]
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
                    ..Default::default()
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_resource_limits_exit_code_and_stream_order_are_stable() {
        let output = test_tool()
            .call(BashArgs {
                command: "printf stdout; printf stderr >&2; exit 7".to_string(),
                separate_streams: true,
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(output, "[stdout]\nstdout\n[stderr]\nstderr\nExit code: 7");
        let merged = test_tool()
            .call(BashArgs {
                command: "printf stdout; printf stderr >&2; exit 7".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(merged, "stdoutstderr\nExit code: 7");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tiny_line_cap_preserves_the_nonzero_exit_status() {
        let output = BashTool::new(None, None, Sandbox::new(false, "bwrap"), Some(1))
            .call(BashArgs {
                command: "printf 'one\\ntwo\\nthree\\n'; exit 9".to_string(),
                timeout: None,
                background: false,
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(output.ends_with("Exit code: 9"), "{output}");
    }

    #[cfg(unix)]
    #[test]
    fn signal_termination_uses_the_signal_name_instead_of_minus_one() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(nix::libc::SIGTERM);
        assert_eq!(
            render_termination(Some(status)).as_deref(),
            Some("Terminated by signal: SIGTERM (15)")
        );
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
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let running = status
                .call(JobStatusArgs {
                    id: Some(id.clone()),
                    timeout_ms: None,
                    since: None,
                    action: JobAction::Poll,
                    ..Default::default()
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
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        wait_for_nonempty_file(&pid_file).await;
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();

        let stopped = status
            .call(JobStatusArgs {
                id: Some(id),
                timeout_ms: None,
                since: None,
                action: JobAction::Stop,
                ..Default::default()
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
                ..Default::default()
            })
            .await
            .unwrap();
        let terminal = wait_for_terminal_job(&status, &background_id(&started)).await;

        assert!(terminal.contains("Status: completed"), "{terminal}");
        assert!(terminal.contains("1\n2\n"), "{terminal}");
        assert!(terminal.contains("50000"), "{terminal}");
        assert!(terminal.contains("bytes skipped"), "{terminal}");
        assert!(terminal.len() < 70 * 1024, "rolling output was not bounded");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interrupted_turn_keeps_jobs_and_dropping_the_session_sandbox_reaps_them() {
        let pid_file =
            std::env::temp_dir().join(format!("mini-agent-session-job-{}", uuid::Uuid::new_v4()));
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: format!(
                    "echo $$ > {}; while :; do sleep 1; done",
                    shell_quote(&pid_file)
                ),
                timeout: Some(60_000),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        wait_for_nonempty_file(&pid_file).await;
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Esc / ACP cancel: foreground groups die, the job does not.
        shell.sandbox.kill_active();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            process_exists(pid),
            "turn interruption killed a background job"
        );
        let listed = status
            .call(JobStatusArgs {
                action: JobAction::List,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(listed.contains(&format!("{id}: running")), "{listed}");
        // Session end: the last sandbox clone cancels and reaps the job tree.
        drop(status);
        drop(shell);
        wait_for_process_exit(pid).await;
        let _ = std::fs::remove_file(pid_file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_background_promotes_a_command_at_its_deadline_and_keeps_its_output() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = Instant::now();
        let promoted = shell
            .call(BashArgs {
                command: "printf 'begin-%s\\n' ok; sleep 2; printf 'finish-%s\\n' ok".to_string(),
                timeout: Some(500),
                auto_background: Some(true),
                ..Default::default()
            })
            .await
            .expect("promotion is not an error");
        assert!(
            started.elapsed() < Duration::from_millis(1800),
            "did not return at the deadline"
        );
        assert!(
            promoted.contains("[status: promoted_to_background; job_id: job-"),
            "{promoted}"
        );
        assert!(promoted.contains("begin-ok"), "{promoted}");
        let id = promoted
            .split("job_id: ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap()
            .to_string();
        let finished = status
            .call(JobStatusArgs {
                id: Some(id.clone()),
                action: JobAction::Wait,
                timeout_ms: Some(10_000),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(finished.contains("Status: completed"), "{finished}");
        assert!(finished.contains("finish-ok"), "{finished}");
        assert!(
            !finished.contains("begin-ok"),
            "only new output after promotion: {finished}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_background_command_finishing_in_time_reads_like_a_foreground_result() {
        let shell = test_tool();
        let output = shell
            .call(BashArgs {
                command: "printf quick; exit 4".to_string(),
                auto_background: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(output, "quick\nExit code: 4");
        assert!(
            shell.sandbox.background_job_summaries().is_empty(),
            "record not dropped"
        );
        // Without the flag the deadline still kills the command.
        let error = shell
            .call(BashArgs {
                command: "sleep 5".to_string(),
                timeout: Some(300),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("[status: timed_out"), "{error}");
        // The configured default applies when the call does not say.
        let configured = test_tool().with_auto_background(true);
        let promoted = configured
            .call(BashArgs {
                command: "sleep 2".to_string(),
                timeout: Some(300),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(promoted.contains("promoted_to_background"), "{promoted}");
        configured.sandbox.cancel_background_jobs();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_an_auto_background_call_before_its_deadline_stops_the_job() {
        let shell = test_tool();
        let call = shell.call(BashArgs {
            command: "sleep 30".to_string(),
            auto_background: Some(true),
            ..Default::default()
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(300), call)
                .await
                .is_err()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let jobs = shell.sandbox.background_job_summaries();
            assert_eq!(jobs.len(), 1);
            if jobs[0].status.is_terminal() {
                assert_eq!(jobs[0].status.as_str(), "cancelled");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "interrupted call left its job running"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn command_timeout_secs_sets_the_bang_command_deadline_within_bounds() {
        crate::sandbox::set_command_timeout_secs(45);
        assert_eq!(
            crate::sandbox::configured_command_limits().timeout,
            Duration::from_secs(45)
        );
        crate::sandbox::set_command_timeout_secs(100_000);
        assert_eq!(
            crate::sandbox::configured_command_limits().timeout,
            Duration::from_secs(600)
        );
        crate::sandbox::set_command_timeout_secs(30);
        assert_eq!(
            crate::sandbox::configured_command_limits().stdout_bytes,
            DEFAULT_COMMAND_LIMITS.stdout_bytes
        );
    }

    async fn poll_job(status: &JobStatusTool, id: &str, since: Option<u64>) -> String {
        status
            .call(JobStatusArgs {
                id: Some(id.to_string()),
                timeout_ms: None,
                since,
                action: JobAction::Poll,
                ..Default::default()
            })
            .await
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_polls_return_only_new_output_and_since_rereads() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command:
                    "printf 'first-%s\\n' chunk; sleep 0.4; printf 'second-%s\\n' chunk; sleep 0.4"
                        .to_string(),
                timeout: Some(5_000),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        let deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            let output = poll_job(&status, &id, None).await;
            if output.contains("first-chunk") {
                break output;
            }
            assert!(Instant::now() < deadline, "first chunk never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(!first.contains("second-chunk"), "{first}");
        assert!(first.contains("next poll continues at since=12"), "{first}");
        let second = loop {
            let output = poll_job(&status, &id, None).await;
            if output.contains("second-chunk") {
                break output;
            }
            assert!(
                !output.contains("first-chunk"),
                "repeated old output: {output}"
            );
            assert!(Instant::now() < deadline, "second chunk never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(!second.contains("first-chunk"), "{second}");
        assert!(second.contains("bytes 12..25 of 25"), "{second}");
        let again = poll_job(&status, &id, Some(0)).await;
        assert!(again.contains("first-chunk\nsecond-chunk"), "{again}");
        let terminal = wait_for_terminal_job(&status, &id).await;
        assert!(terminal.contains("Status: completed"), "{terminal}");
        assert!(poll_job(&status, &id, None).await.contains("No new output"));
    }

    async fn job_input(status: &JobStatusTool, id: &str, input: &str) -> Result<String, ToolError> {
        status
            .call(JobStatusArgs {
                id: Some(id.to_string()),
                action: JobAction::Input,
                input: Some(input.to_string()),
                ..Default::default()
            })
            .await
    }

    /// Drive an interactive program in a `pty = true` job: one line of input,
    /// an answer that only the program (not the echo) can contain, then exit.
    async fn run_pty_repl(shell: BashTool, program: &str, line: &str, answer: &str, exit: &str) {
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: program.to_string(),
                timeout: Some(20_000),
                background: true,
                pty: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        let mut seen = job_input(&status, &id, line).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen.contains(answer) {
            assert!(Instant::now() < deadline, "program never answered: {seen}");
            tokio::time::sleep(Duration::from_millis(20)).await;
            seen.push_str(&poll_job(&status, &id, None).await);
        }
        job_input(&status, &id, exit).await.unwrap();
        let terminal = wait_for_terminal_job(&status, &id).await;
        assert!(terminal.contains("Status: completed"), "{terminal}");
        assert!(terminal.contains("Exit code: 0"), "{terminal}");
        // A finished job no longer accepts input.
        let error = job_input(&status, &id, "1\n").await.unwrap_err();
        assert!(error.to_string().contains("already finished"), "{error}");
    }

    fn shell_permission(
        config: crate::permission::PermissionConfig,
        mode: crate::permission::SecurityMode,
    ) -> PermCheck {
        use crate::permission::PermissionConfigs;
        use crate::permission::checker::PermissionChecker;
        std::sync::Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(config),
                mode,
                Some(std::env::temp_dir()),
                Some(vec!["standard".to_string(), "yolo".to_string()]),
            )
            .expect("valid permission test configuration"),
        ))
    }

    async fn start_pty_job(shell: &BashTool, command: &str) -> String {
        let started = shell
            .call(BashArgs {
                command: command.to_string(),
                timeout: Some(30_000),
                background: true,
                pty: true,
                ..Default::default()
            })
            .await
            .unwrap();
        background_id(&started)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn job_input_honours_shell_deny_rules_and_never_reaches_the_terminal() {
        use crate::permission::{Action, PermissionConfig, ToolPerm};
        let marker = std::env::current_dir()
            .unwrap()
            .join(format!("job-input-deny-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let shell = test_tool();
        let id = start_pty_job(&shell, "sh").await;
        let permission = shell_permission(
            PermissionConfig {
                bash: Some(ToolPerm::Granular(
                    [
                        ("touch *".to_string(), Action::Deny),
                        ("*".to_string(), Action::Allow),
                    ]
                    .into(),
                )),
                ..Default::default()
            },
            crate::permission::SecurityMode::Yolo,
        );
        let status =
            JobStatusTool::new(shell.sandbox.clone(), None).with_permission(Some(permission), None);
        let error = job_input(
            &status,
            &id,
            &format!("touch {}\n", marker.file_name().unwrap().to_string_lossy()),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("denied"),
            "{error}"
        );
        // An allowed line still works, proving the shell is alive and that the
        // denied line was never typed.
        let seen = job_input(&status, &id, "echo allowed-input\n")
            .await
            .unwrap();
        assert!(seen.contains("allowed-input"), "{seen}");
        assert!(!marker.exists(), "denied input reached the terminal");
        job_input(&status, &id, "exit\n").await.unwrap();
    }

    /// Default closed: with an ask policy and nobody to ask, typed input is
    /// refused rather than written.
    #[cfg(unix)]
    #[tokio::test]
    async fn job_input_is_refused_when_approval_is_required_and_unavailable() {
        use crate::permission::{Action, PermissionConfig, ToolPerm};
        let shell = test_tool();
        let id = start_pty_job(&shell, "sh").await;
        let permission = shell_permission(
            PermissionConfig {
                bash: Some(ToolPerm::Simple(Action::Ask)),
                ..Default::default()
            },
            crate::permission::SecurityMode::Standard,
        );
        let status =
            JobStatusTool::new(shell.sandbox.clone(), None).with_permission(Some(permission), None);
        job_input(&status, &id, "echo typed\n").await.unwrap_err();
        let seen = poll_job(&status, &id, Some(0)).await;
        assert!(!seen.contains("typed"), "{seen}");
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    #[test]
    fn terminal_output_is_normalised_for_the_model() {
        let raw = "\u{1b}[?2004h\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}ok\u{1b}]8;;http://x\u{1b}\\link\u{1b}(B\r\n\
                   10%\r50%\r100% done\r\nplain\u{7}\u{8}\ttab\r\n";
        assert_eq!(
            normalize_terminal_output(raw),
            "redoklink\n100% done\nplain\ttab\n"
        );
        // An unterminated sequence at the end swallows only itself.
        assert_eq!(normalize_terminal_output("a\u{1b}[31"), "a");
        assert_eq!(normalize_terminal_output("a\u{1b}]0;no end"), "a");
        assert_eq!(normalize_terminal_output("a\u{1b}"), "a");
        assert_eq!(normalize_terminal_output("héllo ✓\n"), "héllo ✓\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pty_job_output_is_normalised_while_the_log_keeps_raw_bytes() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let id = start_pty_job(&shell, "printf '\\033[31mred\\033[0m\\n1\\r2\\r3 done\\n'").await;
        let terminal = wait_for_terminal_job(&status, &id).await;
        assert!(terminal.contains("red\n3 done"), "{terminal}");
        assert!(!terminal.contains('\u{1b}'), "{terminal:?}");
        assert!(terminal.contains("terminal escapes removed"), "{terminal}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pty_background_job_drives_an_interactive_repl_through_job_status_input() {
        run_pty_repl(
            test_tool(),
            "python3 -i -q",
            "import os; print('tty', os.isatty(0), os.isatty(1), 1+1)\n",
            "tty True True 2",
            "exit()\n",
        )
        .await;
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn pty_background_job_works_inside_the_real_seatbelt_sandbox() {
        let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
        if !crate::sandbox::seatbelt_exists() {
            crate::sandbox::skip_unusable_real_sandbox_test("Seatbelt is unavailable");
            return;
        }
        let workspace = std::sync::Arc::new(
            crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
        );
        let sandbox = Sandbox::new(true, "seatbelt").with_workspace_binding(workspace);
        run_pty_repl(
            BashTool::new(None, None, sandbox, None),
            "sh -i",
            "test -t 0 && test -t 1 && echo tty-$((1+1))-ok\n",
            "tty-2-ok",
            "exit 0\n",
        )
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pty_foreground_command_sees_a_terminal_and_default_commands_do_not() {
        let tool = test_tool();
        let probe = "if test -t 0 && test -t 1 && test -t 2; then echo is-$((1+1))-tty; else echo no-$((1+1))-tty; fi";
        let pty = tool
            .call(BashArgs {
                command: probe.to_string(),
                pty: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(pty.contains("is-2-tty"), "{pty}");
        assert!(
            !pty.contains("[stdout]"),
            "pty output is one merged stream: {pty}"
        );
        let plain = tool
            .call(BashArgs {
                command: probe.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(plain.contains("no-2-tty"), "{plain}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn job_input_requires_a_pty_job_and_is_size_bounded() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "sleep 5".to_string(),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        let error = job_input(&status, &id, "hello\n").await.unwrap_err();
        assert!(error.to_string().contains("pty=true"), "{error}");
        let error = job_input(&status, &id, &"x".repeat(16 * 1024 + 1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("16384 bytes"), "{error}");
        let error = status
            .call(JobStatusArgs {
                id: Some(id.clone()),
                action: JobAction::Poll,
                input: Some("x".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("only valid with, action=input"),
            "{error}"
        );
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_output_is_fully_recoverable_from_its_private_log() {
        let dir = crate::tests::support::temp::TempDir::new("job-log");
        let logs = dir.path().join("logs");
        let shell = test_tool().with_output_log_dir(Some(logs.clone()));
        let status = JobStatusTool::new(shell.sandbox.clone(), Some(40));
        let started = shell
            .call(BashArgs {
                command: "yes 0123456789abcde | head -c 10485760".to_string(),
                timeout: Some(20_000),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        let log_path = logs.join(format!("shell-{id}.log"));
        assert!(
            started.contains(&log_path.display().to_string()),
            "{started}"
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        let terminal = loop {
            let output = poll_job(&status, &id, Some(0)).await;
            if output.contains("Status: completed") {
                break output;
            }
            assert!(Instant::now() < deadline, "job did not finish");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            terminal.contains(&format!("Log: {}", log_path.display())),
            "{terminal}"
        );
        let log = std::fs::read(&log_path).unwrap();
        assert_eq!(log.len(), 10_485_760);
        assert!(log.starts_with(b"0123456789abcde\n0123456789abcde\n"));
        // A middle range the in-memory head/tail dropped is served by the log.
        let middle = poll_job(&status, &id, Some(5_000_000)).await;
        assert!(middle.contains("bytes 5000000.."), "{middle}");
        assert!(middle.contains("0123456789abcde"), "{middle}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pruned_background_jobs_keep_their_log_path() {
        let dir = crate::tests::support::temp::TempDir::new("job-prune");
        let logs = dir.path().join("logs");
        let shell = test_tool().with_output_log_dir(Some(logs.clone()));
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let mut first = None;
        for index in 0..34 {
            let started = shell
                .call(BashArgs {
                    command: format!("printf 'job-{index}'"),
                    timeout: Some(5_000),
                    background: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            let id = background_id(&started);
            wait_for_terminal_job(&status, &id).await;
            first.get_or_insert(id);
        }
        let first = first.unwrap();
        let error = status
            .call(JobStatusArgs {
                id: Some(first.clone()),
                timeout_ms: None,
                since: None,
                action: JobAction::Poll,
                ..Default::default()
            })
            .await
            .expect_err("the oldest record is pruned");
        let message = error.to_string();
        assert!(message.contains("pruned"), "{message}");
        let path = logs.join(format!("shell-{first}.log"));
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert_eq!(std::fs::read(path).unwrap(), b"job-0");
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
                ..Default::default()
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
                id: Some(id.clone()),
                timeout_ms: None,
                since: None,
                action: JobAction::Poll,
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(unknown, format!("background job not found: {id}"));

        let invalid = status
            .call(JobStatusArgs {
                id: Some("x".repeat(128 * 1024)),
                timeout_ms: None,
                since: None,
                action: JobAction::Poll,
                ..Default::default()
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
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        let completed = wait_for_terminal_job(&status, &id).await;
        assert!(completed.contains("Status: completed"), "{completed}");

        let stopped = status
            .call(JobStatusArgs {
                id: Some(id),
                timeout_ms: None,
                since: None,
                action: JobAction::Stop,
                ..Default::default()
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
                    ..Default::default()
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
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            overflow,
            "background job capacity is full (maximum 8 running jobs)"
        );

        // Interrupting a turn kills foreground groups only; jobs survive.
        shell.sandbox.kill_active();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(shell.sandbox.running_background_job_count(), 8);
        for summary in shell.sandbox.background_job_summaries() {
            assert_eq!(summary.status.as_str(), "running");
        }
        shell.sandbox.cancel_background_jobs();
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

#[cfg(all(test, unix))]
mod foreground_metadata_tests {
    use super::*;
    use rig::tool::{Tool, ToolCallExtensions};
    fn tool(lines: Option<u64>) -> ShellTool {
        ShellTool::new(None, None, Sandbox::new(false, "none"), lines)
    }
    #[tokio::test]
    async fn actual_foreground_metadata_preserves_stdout_before_display_truncation_and_nonzero_exit()
     {
        let extensions = ToolCallExtensions::default();
        for (command, code) in [
            ("printf 'HEAD\\nBODY\\nTAIL\\n'", 0),
            (
                "printf 'https://never-claimed.invalid/pull/1\\n'; exit 3",
                3,
            ),
        ] {
            let result = Tool::call_structured(
                &tool(Some(1)),
                BashArgs {
                    command: command.into(),
                    timeout: None,
                    background: false,
                    ..Default::default()
                },
                &extensions,
            )
            .await
            .unwrap();
            let actual = result.extensions.get::<ForegroundResult>().unwrap();
            assert_eq!(actual.status, CommandStatus::Completed);
            assert_eq!(actual.exit_code, Some(code));
            assert!(actual.stdout_complete);
            assert!(!actual.descendants_escaped);
            if code == 0 {
                assert_eq!(actual.stdout, b"HEAD\nBODY\nTAIL\n");
                assert!(result.output.contains("omitted"));
            } else {
                assert_eq!(actual.stdout, b"https://never-claimed.invalid/pull/1\n");
            }
        }
    }
    #[tokio::test]
    async fn actual_foreground_metadata_marks_bounded_stdout_incomplete_and_timeout_has_no_success_metadata()
     {
        let extensions = ToolCallExtensions::default();
        let result = Tool::call_structured(
            &tool(None),
            BashArgs {
                command: "head -c 65537 /dev/zero | tr '\\000' x".into(),
                timeout: None,
                background: false,
                ..Default::default()
            },
            &extensions,
        )
        .await
        .unwrap();
        let actual = result.extensions.get::<ForegroundResult>().unwrap();
        assert_eq!(actual.stdout.len(), 64 * 1024);
        assert!(!actual.stdout_complete);
        let error = Tool::call_structured(
            &tool(None),
            BashArgs {
                command: "printf 'https://never-claimed.invalid/pull/1\\n'; sleep 5".into(),
                timeout: Some(30),
                background: false,
                ..Default::default()
            },
            &extensions,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("timed out") || error.to_string().contains("timeout"),
            "{error}"
        );
    }
    #[tokio::test]
    async fn actual_background_structured_result_has_no_foreground_observation() {
        let shell = tool(None);
        let result = Tool::call_structured(
            &shell,
            BashArgs {
                command: "printf BACKGROUND32".into(),
                timeout: None,
                background: true,
                ..Default::default()
            },
            &ToolCallExtensions::default(),
        )
        .await
        .unwrap();
        assert!(result.extensions.get::<ForegroundResult>().is_none());
        let id = result
            .output
            .lines()
            .find_map(|line| line.strip_prefix("Background job started: "))
            .unwrap();
        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            shell.sandbox.stop_background_job(id),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(snapshot.status.is_terminal());
    }
}
