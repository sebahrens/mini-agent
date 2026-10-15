use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, BashArgs, JobAction, JobStatusArgs, PermCheck, ToolError, check_perm,
};
use crate::extras::truncate::head_tail_lines;
use crate::sandbox::{
    BackgroundJobSnapshot, CommandLimits, CommandOutput, CommandOutputLimit, CommandProgress,
    CommandStatus, DEFAULT_BACKGROUND_COMMAND_TIMEOUT, DEFAULT_COMMAND_LIMITS,
    MAX_BACKGROUND_JOB_WAIT_MILLIS, Sandbox,
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

/// Shell calls announced by [`ShellCallIdHook`] whose execution has not yet
/// claimed their id, keyed by their exact argument text. This only labels a
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

/// Claim the internal id announced for the shell call about to run with the
/// exact argument text `args`.
pub(crate) fn claim_shell_call_id(
    extensions: &rig::tool::ToolCallExtensions,
    args: &str,
) -> Option<CompactString> {
    extensions
        .get::<rig::agent::Scratchpad>()?
        .update(|pending: &mut PendingShellCalls| pending.take(args))
}

/// Records each `shell` call's internal id in the run scratchpad so the tool
/// can label its live progress with the id the transcript uses. Calls that
/// never execute (skipped by another hook) are forgotten on their result.
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
                tool_name: "shell",
                internal_call_id,
                args,
                ..
            } => context
                .scratchpad()
                .update(|pending: &mut PendingShellCalls| pending.push(args, internal_call_id)),
            rig::agent::StepEvent::ToolResult {
                tool_name: "shell",
                internal_call_id,
                args,
                ..
            } => context
                .scratchpad()
                .update(|pending: &mut PendingShellCalls| pending.forget(args, internal_call_id)),
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
    id: CompactString,
    events: tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
) -> std::io::Result<CommandOutput> {
    let progress = CommandProgress::new();
    let mut changes = progress.subscribe();
    let command_run =
        sandbox.output_command_with_limits_and_progress(command, limits, Some(progress.clone()));
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
        }
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
        let coaching = check_perm(&self.permission, &self.ask_tx, "shell", &args.command).await?;

        if args.background {
            let timeout = args
                .timeout
                .map(std::time::Duration::from_millis)
                .unwrap_or(DEFAULT_BACKGROUND_COMMAND_TIMEOUT);
            let id = self
                .sandbox
                .start_background_command(args.command, timeout)
                .await?;
            let result = format!(
                "Background job started: {id}\nUse job_status with this id to poll output or stop it."
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
        let live = SHELL_CALL_ID
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .zip(crate::agent::tools::mutation_diff::scoped_event_sender());
        let output = match live {
            Some((id, events)) => {
                output_with_progress(&self.sandbox, &args.command, limits, id, events).await?
            }
            None => {
                self.sandbox
                    .output_command_with_limits(&args.command, limits)
                    .await?
            }
        };

        if output.status != CommandStatus::Completed {
            tracing::warn!("tool shell stopped before completion: {:?}", output.status);
            return Err(resource_limit_error(output, limits, self.max_output_lines));
        }

        #[cfg(any(feature = "git-worktree", test))]
        const METADATA_STDOUT_BYTES: usize = 64 * 1024;
        #[cfg(any(feature = "git-worktree", test))]
        let metadata = Some(ForegroundResult {
            status: output.status,
            exit_code: output.exit_status.and_then(|status| status.code()),
            descendants_escaped: output.descendants_escaped,
            stdout: output.stdout[..output.stdout.len().min(METADATA_STDOUT_BYTES)].to_vec(),
            stdout_complete: output.stdout.len() <= METADATA_STDOUT_BYTES,
        });
        #[cfg(not(any(feature = "git-worktree", test)))]
        let metadata = None;
        let termination = render_termination(output.exit_status);
        if let Some(termination) = termination.as_deref() {
            tracing::warn!("tool shell: {termination}");
        }

        let output_len = output.stdout.len() + output.stderr.len();
        let mut result = render_streams(&output.stdout, &output.stderr);
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

        let result = bound_output_lines(result, self.max_output_lines);

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
        "Poll, wait for, stop, or list session-scoped background shell jobs. Poll returns current bounded head/tail output. Wait returns when the job ends or timeout_ms expires (default 10000, maximum 60000 milliseconds); expiration leaves the job running. List returns ids, commands and statuses without an id. Stop waits up to 5 seconds for process-tree cleanup."
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
                    "enum": ["poll", "stop", "wait", "list"],
                    "description": "Poll status/output, wait for completion, stop and reap, or list current session jobs (default poll)"
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_BACKGROUND_JOB_WAIT_MILLIS,
                    "description": "Only with action=wait: maximum wait in milliseconds (default 10000). Expiration does not stop the job."
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
                    format!(
                        "{}: {}{exit}\n  {}",
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
        let snapshot = match args.action {
            JobAction::Poll => self.sandbox.background_job_snapshot(id),
            JobAction::Stop => self.sandbox.stop_background_job(id).await,
            JobAction::Wait => {
                self.sandbox
                    .wait_background_job(
                        id,
                        std::time::Duration::from_millis(args.timeout_ms.unwrap_or(10_000)),
                    )
                    .await
            }
            JobAction::List => unreachable!("list returned before id lookup"),
        }
        .map_err(ToolError::Msg)?;
        Ok(render_background_job(snapshot, self.max_output_lines))
    }
}

/// Source-compatibility alias for integrations that still construct the old
/// Rust type. The model-visible tool name is always `shell`.
#[cfg(test)]
pub type BashTool = ShellTool;

fn render_streams(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut result = String::with_capacity(stdout.len() + stderr.len() + 20);
    if !stdout.is_empty() {
        result.push_str("[stdout]\n");
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("[stderr]\n");
        result.push_str(&stderr);
    }
    result
}

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
    if snapshot.descendants_escaped {
        result.push_str(
            "\nContainment warning: descendants escaped process-group cleanup; direct command result preserved",
        );
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
    let tail = (cap / 5).max(1);
    let head = cap - tail;
    head_tail_lines(&text, head, tail, |total| {
        let omitted = total - head - tail;
        format!(
            "\n\n[... {omitted} lines omitted — showing the first {head} and last {tail} of {total} lines; \
             re-run with a narrower invocation or pipe through `head`/`tail`/`grep` to see the rest ...]\n"
        )
    })
    .unwrap_or(text)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_progress_streams_before_completion_and_keeps_the_result() {
        let command = "echo a; sleep 1; echo b";
        let tool = test_tool();
        let args = || BashArgs {
            command: command.to_string(),
            timeout: None,
            background: false,
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
            pending.push(r#"{"command":"ls"}"#, "first");
            pending.push(r#"{"command":"pwd"}"#, "other");
            pending.push(r#"{"command":"ls"}"#, "second");
            pending.push(r#"{"command":"ls"}"#, "skipped");
            pending.forget(r#"{"command":"ls"}"#, "skipped");
        });
        let mut extensions = rig::tool::ToolCallExtensions::new();
        extensions.insert(pad);
        let claim = |args| claim_shell_call_id(&extensions, args);
        assert_eq!(claim(r#"{"command":"ls"}"#).as_deref(), Some("first"));
        assert_eq!(claim(r#"{"command":"ls"}"#).as_deref(), Some("second"));
        assert_eq!(claim(r#"{"command":"ls"}"#), None);
        assert_eq!(claim(r#"{"command":"pwd"}"#).as_deref(), Some("other"));
        assert_eq!(
            claim_shell_call_id(&rig::tool::ToolCallExtensions::new(), "{}"),
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
                    if String::from_utf8_lossy(&snapshot.stdout).contains("JOB_READY") {
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
            })
            .await
            .unwrap();
        assert!(output.contains("CONFIGURED_DEADLINE"));
        assert!(
            tool.call(BashArgs {
                command: "must-not-execute".into(),
                timeout: Some(600_001),
                background: false
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
                    timeout_ms: None
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
                    timeout_ms: Some(timeout_ms)
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
                timeout_ms: None
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
                    timeout_ms: None
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

    #[cfg(unix)]
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
    async fn out_of_range_shell_timeouts_are_rejected_before_starting_a_job() {
        let tool = test_tool();
        for (background, maximum) in [(false, 120_000), (true, 86_400_000)] {
            for timeout in [0, maximum + 1, u64::MAX] {
                let error = tool
                    .call(BashArgs {
                        command: "printf must-not-run".into(),
                        timeout: Some(timeout),
                        background,
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
                timeout: None,
                background: false,
            })
            .await
            .unwrap();

        assert_eq!(output, "[stdout]\nstdout\n[stderr]\nstderr\nExit code: 7");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tiny_line_cap_preserves_the_nonzero_exit_status() {
        let output = BashTool::new(None, None, Sandbox::new(false, "bwrap"), Some(1))
            .call(BashArgs {
                command: "printf 'one\\ntwo\\nthree\\n'; exit 9".to_string(),
                timeout: None,
                background: false,
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
                id: Some(id),
                timeout_ms: None,
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
                id: Some(id.clone()),
                timeout_ms: None,
                action: JobAction::Poll,
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(unknown, format!("background job not found: {id}"));

        let invalid = status
            .call(JobStatusArgs {
                id: Some("x".repeat(128 * 1024)),
                timeout_ms: None,
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
                id: Some(id),
                timeout_ms: None,
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
