use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, BashArgs, JobAction, JobKey, JobStatusArgs, PermCheck, ToolError, check_perm,
};
use crate::extras::truncate::head_tail_lines;
use crate::sandbox::{
    BackgroundJobOutput, CommandLimits, CommandOutput, CommandOutputLimit, CommandProgress,
    CommandStatus, DEFAULT_BACKGROUND_COMMAND_TIMEOUT, DEFAULT_COMMAND_LIMITS,
    MAX_BACKGROUND_JOB_WAIT_MILLIS, OutputChannel, OutputLog, OutputTranscript, OutputWindow,
    SHELL_HARD_OUTPUT_BYTES, Sandbox, ShellOutputCapture, SpoolPolicy, TranscriptPart,
    write_private_log,
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

/// The background jobs one subagent started. A subagent shares its parent's
/// sandbox, so `job_status` is restricted to the ids recorded here (it can
/// neither see nor control the parent's or a sibling's jobs), and jobs still
/// running when the last tool holding the scope is dropped are stopped.
#[derive(Clone)]
pub struct JobScope(std::sync::Arc<JobScopeInner>);

struct JobScopeInner {
    sandbox: Sandbox,
    ids: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

#[cfg_attr(not(feature = "subagents"), allow(dead_code))]
impl JobScope {
    pub fn new(sandbox: Sandbox) -> Self {
        Self(std::sync::Arc::new(JobScopeInner {
            sandbox,
            ids: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        }))
    }

    fn record(&self, id: &str) {
        self.0
            .ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.to_string());
    }

    fn contains(&self, id: &str) -> bool {
        self.0
            .ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(id)
    }
}

impl Drop for JobScopeInner {
    fn drop(&mut self) {
        let ids = std::mem::take(
            self.ids
                .get_mut()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for id in ids {
            let _ = self.sandbox.request_stop_background_job(&id);
        }
    }
}

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
    /// `shell-allow-escalation`: offer the per-call `escalate` parameter.
    /// Default closed.
    allow_escalation: bool,
    /// Set for subagents: records the ids of the jobs this tool starts.
    job_scope: Option<JobScope>,
}

/// Longest `justification` shown in the escalation approval.
const MAX_ESCALATION_JUSTIFICATION_CHARS: usize = 400;

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
            allow_escalation: false,
            job_scope: None,
        }
    }

    /// Record every background job this tool starts in `scope` (subagents).
    #[cfg_attr(not(feature = "subagents"), allow(dead_code))]
    pub fn with_job_scope(mut self, scope: JobScope) -> Self {
        self.job_scope = Some(scope);
        self
    }

    /// Offer the `escalate` parameter (`shell-allow-escalation`, never when
    /// `sandbox-required`). Each use still needs interactive approval.
    pub fn with_escalation(mut self, enabled: bool) -> Self {
        self.allow_escalation = enabled;
        self
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

/// Linux bubblewrap gives every command its own RAM-backed `/tmp`.
pub(crate) const SHELL_PRIVATE_TMP_NOTE: &str = " /tmp (and $TMPDIR) is a private in-memory tmpfs discarded when each command ends, including background jobs' own copy: files written there do not survive to the next shell call and large temp files use RAM. Keep anything needed across calls in the workspace.";

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
        if self.sandbox.shell_private_tmp() {
            description.push_str(SHELL_PRIVATE_TMP_NOTE);
        }
        description = with_pty_limitation_note(description, PTY_SUPPORTED);
        if self.allow_escalation {
            description.push_str(
                " If the sandbox blocks a command that must run (network, a write outside the workspace), retry that one command with escalate=true and a justification: the user is asked to approve it every time, and only that command runs without the sandbox.",
            );
        }
        description
    }

    fn parameters(&self) -> serde_json::Value {
        let dialect = self
            .sandbox
            .shell_capability()
            .map(|capability| capability.dialect().name())
            .unwrap_or("configured shell");
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": format!("{dialect} command to execute") },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 86_400_000,
                    "description": format!("Lower command deadline in milliseconds (optional; maximum {} foreground or 86400000 background)", self.foreground_timeout.as_secs() * 1000)
                },
                "workdir": {
                    "type": "string",
                    "description": "Directory to run the command in: workspace-relative or absolute inside the workspace (default: the workspace root). It must exist; symlinks and .. that leave the workspace are refused. Every call starts a fresh shell, so cd does NOT persist between calls: pass workdir each time. The permission check sees the command with the directory change, so rules and approvals are for that directory."
                },
                "background": {
                    "type": "boolean",
                    "description": "Start a session-scoped background job and return its id (default false)"
                },
                "monitor": {
                    "type": "string",
                    "maxLength": MAX_UNTIL_PATTERN_BYTES,
                    "description": "Only with background=true: a regex (Rust syntax) watched against each new output line while the job runs. Matching lines queue a bounded notice (first matching line, match count, log path) delivered like a completion notice: with the next tool result, the next model request, or by waking an idle session. At most one notice per 5 seconds and 20 per job (then use job_status until=); the full output stays in the job log."
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
                    "description": "Run under a pseudo-terminal so isatty() is true and stdin/stdout/stderr are one TTY, 120x40 unless the mini-agent process sets COLUMNS/LINES (Unix, or Windows with the AppContainer sandbox backend; default false). With background=true, job_status action=input types into it, e.g. for REPLs or prompts."
                }
            },
            "required": ["command"]
        });
        if self.allow_escalation
            && let Some(properties) = schema["properties"].as_object_mut()
        {
            properties.insert(
                "escalate".into(),
                serde_json::json!({
                    "type": "boolean",
                    "description": "Run this single command WITHOUT the sandbox after the user approves it interactively (default false). Use only after a sandboxed attempt failed because of the sandbox; requires justification."
                }),
            );
            properties.insert(
                "justification".into(),
                serde_json::json!({
                    "type": "string",
                    "maxLength": MAX_ESCALATION_JUSTIFICATION_CHARS,
                    "description": "Only with escalate=true: why the command must leave the sandbox, shown to the user in the approval prompt."
                }),
            );
        }
        without_unsupported_pty(schema, PTY_SUPPORTED)
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        self.execute(args)
            .await
            .map(|(text, _)| self.with_job_notices(text))
    }
    #[cfg(any(feature = "git-worktree", test))]
    async fn call_structured(
        &self,
        args: BashArgs,
        _extensions: &rig::tool::ToolCallExtensions,
    ) -> Result<rig::tool::ToolReturn<String>, ToolError> {
        self.execute(args).await.map(|(text, metadata)| {
            let result = rig::tool::ToolReturn::success(self.with_job_notices(text));
            match metadata {
                Some(metadata) => result.with_extension(metadata),
                None => result,
            }
        })
    }
}

impl ShellTool {
    /// Append the session's pending job notices. A scoped (subagent) shell
    /// shares its parent's sandbox and so its notice queue, which belongs to
    /// the parent session: draining it here would lose the notices for good.
    fn with_job_notices(&self, text: String) -> String {
        if self.job_scope.is_some() {
            return text;
        }
        append_job_notices(&self.sandbox, text)
    }

    /// `escalate = true`: validate, judge deny rules, require a fresh
    /// interactive approval, audit, then run this one command on a clone whose
    /// sandbox is off (environment still cleared). The clone shares the job
    /// table, so background jobs stay visible to `job_status`.
    async fn execute_escalated(
        &self,
        mut args: BashArgs,
    ) -> Result<(String, Option<ForegroundResult>), ToolError> {
        if !self.allow_escalation {
            return Err(ToolError::Msg(
                "sandbox escalation is disabled: shell-allow-escalation is off (or sandbox-required is set)"
                    .into(),
            ));
        }
        if !self.sandbox.containment_requested() {
            return Err(ToolError::Msg(
                "escalate is unnecessary: the sandbox is already off for this session".into(),
            ));
        }
        let justification = args.justification.take().unwrap_or_default();
        let justification = justification.trim();
        if justification.is_empty()
            || justification.chars().count() > MAX_ESCALATION_JUSTIFICATION_CHARS
        {
            return Err(ToolError::Msg(format!(
                "escalate=true requires a justification of 1 to {MAX_ESCALATION_JUSTIFICATION_CHARS} characters explaining why the command must leave the sandbox"
            )));
        }
        // Deny rules still win; allow rules and modes cannot waive the prompt.
        if let Some(permission) = &self.permission {
            let (result, _) = permission
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .check_diagnosed(|guard| guard.check("shell", &args.command));
            if let crate::permission::checker::CheckResult::Denied(reason) = result {
                return Err(ToolError::Msg(format!("Permission denied: {reason}")));
            }
        }
        let (approval, _released) = super::concurrency::prompt_unlocked(
            super::request_sandbox_escalation_approval(&self.ask_tx, &args.command, justification),
        )
        .await;
        approval?;
        tracing::warn!(
            target: "mini_agent::audit::sandbox_escalation",
            command = %args.command,
            justification,
            background = args.background,
            "user approved running a shell command without the sandbox"
        );
        let mut escalated = Sandbox::clone(&self.sandbox);
        escalated.disable_for_escalation();
        let tool = ShellTool {
            permission: None,
            ask_tx: None,
            sandbox: escalated,
            max_output_lines: self.max_output_lines,
            foreground_timeout: self.foreground_timeout,
            output_log_dir: self.output_log_dir.clone(),
            auto_background: self.auto_background,
            allow_escalation: false,
            job_scope: self.job_scope.clone(),
        };
        args.escalate = false;
        let (text, metadata) = Box::pin(tool.execute(args)).await?;
        Ok((
            format!("[sandbox escalated: ran without the sandbox after user approval]\n{text}"),
            metadata,
        ))
    }

    async fn execute(
        &self,
        args: BashArgs,
    ) -> Result<(String, Option<ForegroundResult>), ToolError> {
        if args.escalate || args.justification.is_some() {
            if !args.escalate {
                return Err(ToolError::Msg(
                    "justification is only valid with escalate=true".into(),
                ));
            }
            return self.execute_escalated(args).await;
        }
        tracing::debug!(
            "tool shell start: cmd_len={}, timeout={:?}, background={}",
            args.command.len(),
            args.timeout,
            args.background,
        );
        let mut args = args;
        let monitor = match args.monitor.take() {
            None => None,
            Some(_) if !args.background => {
                return Err(ToolError::Msg(
                    "shell monitor requires background=true. Example: {\"command\":\"npm run dev\",\"background\":true,\"monitor\":\"ERROR|ready\"}".into(),
                ));
            }
            Some(pattern) => Some(compile_until_pattern(&pattern).map_err(
                |error| match error {
                    ToolError::Msg(text) => {
                        ToolError::Msg(text.replace("job_status until", "shell monitor"))
                    }
                    other => other,
                },
            )?),
        };
        if let Some(workdir) = args.workdir.take() {
            let plain = args.command.clone();
            args.command = self
                .sandbox
                .command_in_workdir(&workdir, &plain)
                .map_err(ToolError::Msg)?;
            // The permission key is the script with the directory change, so
            // approvals are per directory, but a deny rule on the command
            // itself still applies.
            if let Some(permission) = &self.permission {
                let (result, _) = permission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .check_diagnosed(|guard| guard.check("shell", &plain));
                if let crate::permission::checker::CheckResult::Denied(reason) = result {
                    return Err(ToolError::Msg(format!("Permission denied: {reason}")));
                }
            }
        }
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
            if let Some(scope) = &self.job_scope {
                scope.record(&id);
            }
            let monitor_note = match monitor {
                Some(regex) if self.sandbox.job_notice_queue().enabled() => {
                    let note = format!(
                        "\nMonitor: output lines matching /{}/ queue notices (at most 1 per 5 s, 20 per job).",
                        regex.as_str()
                    );
                    super::job_monitor::spawn_monitor(&self.sandbox, &id, regex);
                    note
                }
                Some(_) => "\nMonitor not started: shell-job-notifications is off.".to_string(),
                None => String::new(),
            };
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
                "Background job started: {id}\nUse job_status with this id to poll output or stop it; each poll returns only output not returned before.{log}{monitor_note}"
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
            // A full job table must not fail an ordinary short command: run it
            // in the foreground (without promotion at the deadline) instead.
            if let Some(result) = self.run_auto_background(&args, limits.timeout).await? {
                return Ok((
                    match coaching {
                        Some(message) => format!("{message}\n\n{result}"),
                        None => result,
                    },
                    None,
                ));
            }
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

        let transcript = self.spill_line_capped_output(transcript, &result);
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
    ) -> Result<Option<String>, ToolError> {
        let progress = CommandProgress::new();
        let id = match self
            .sandbox
            .start_background_command_logged(
                args.command.clone(),
                DEFAULT_BACKGROUND_COMMAND_TIMEOUT,
                self.output_log_dir.as_deref(),
                args.pty,
                args.separate_streams,
                Some(progress.clone()),
            )
            .await
        {
            Ok(id) => id,
            Err(error) if error.kind() == std::io::ErrorKind::QuotaExceeded => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if let Some(scope) = &self.job_scope {
            scope.record(&id);
        }
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
            return Ok(Some(format!(
                "[status: promoted_to_background; job_id: {id}; the command reached its {deadline_ms} ms foreground deadline and keeps running as a background job. Use job_status with this id to poll (only new output), wait or stop it]\n{rendered}"
            )));
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
        let ended = snapshot
            .exit_code
            .filter(|code| *code != 0)
            .map(|code| format!("Exit code: {code}"))
            .or_else(|| snapshot.signal.map(describe_signal));
        if let Some(ended) = ended {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&ended);
        }
        let transcript = self.spill_line_capped_output(transcript, &result);
        let mut result = bound_output_lines(result, self.max_output_lines);
        push_log_footer(&mut result, &transcript);
        Ok(Some(result))
    }
}

impl ShellTool {
    /// When the line cap will drop lines but the byte-retention log was never
    /// opened (everything still fit in memory), save the complete retained
    /// output to a private log so the omitted lines stay recoverable.
    fn spill_line_capped_output(
        &self,
        mut transcript: OutputTranscript,
        rendered: &str,
    ) -> OutputTranscript {
        let Some(cap) = self.max_output_lines else {
            return transcript;
        };
        let cap = usize::try_from(cap).unwrap_or(usize::MAX).max(1);
        if transcript.log.is_some()
            || transcript.omitted_bytes > 0
            || crate::extras::truncate::line_count(rendered) <= cap
        {
            return transcript;
        }
        let Some(dir) = self.output_log_dir.as_deref() else {
            return transcript;
        };
        let bytes: Vec<u8> = transcript
            .parts
            .iter()
            .filter_map(|part| match part {
                TranscriptPart::Output(_, bytes) => Some(bytes.as_slice()),
                TranscriptPart::Omitted(_) => None,
            })
            .flatten()
            .copied()
            .collect();
        let path = ShellOutputCapture::log_path_in(dir, "shell-output");
        match write_private_log(&path, &bytes) {
            Ok(()) => {
                transcript.log = Some(OutputLog {
                    path,
                    bytes: bytes.len() as u64,
                    complete: true,
                });
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to save line-capped command output");
                transcript.log_error = Some(error.to_string());
            }
        }
        transcript
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

/// Whether this platform can run a command under a pseudo-terminal: a Unix
/// pty, or a Windows ConPTY hosted by the AppContainer sandbox helper
/// (mini-agent-jguj1, mini-agent-msr4t). Elsewhere the model is never offered
/// `pty`, terminal input or named keys.
const PTY_SUPPORTED: bool = cfg!(any(unix, windows));

/// Added to the shell description where `pty` is not offered, so the model
/// knows why and what to do instead of guessing (mini-agent-msr4t).
const NO_PTY_NOTE: &str = " This platform has no pseudo-terminal: commands that need a TTY, such as REPLs, installers that prompt, `python -i`, or ssh/gh login flows, cannot be driven and may hang or refuse to run. Use their non-interactive flags, pass answers on the command line or through files, or ask the user to run them.";

/// The shell description, plus the no-terminal limitation where `pty` is not offered.
fn with_pty_limitation_note(mut description: String, pty_supported: bool) -> String {
    if !pty_supported {
        description.push_str(NO_PTY_NOTE);
    }
    description
}

/// Remove the pseudo-terminal parameters from a `shell` or `job_status` tool
/// schema when the platform cannot honour them.
fn without_unsupported_pty(
    mut schema: serde_json::Value,
    pty_supported: bool,
) -> serde_json::Value {
    if pty_supported {
        return schema;
    }
    if let Some(properties) = schema["properties"].as_object_mut() {
        for name in ["pty", "input", "keys"] {
            properties.remove(name);
        }
        if let Some(actions) = properties
            .get_mut("action")
            .and_then(|action| action["enum"].as_array_mut())
        {
            actions.retain(|action| action != "input");
        }
        if let Some(action) = properties.get_mut("action")
            && let Some(description) = action["description"].as_str()
        {
            action["description"] = serde_json::Value::from(
                description.replace(", or type input into a pty=true job", ""),
            );
        }
    }
    schema
}

/// Append the completion notices of background jobs that ended on their own
/// since the model last heard about them, so a long turn learns of them at
/// its next tool result (mini-agent-cw4f3). Delivered once.
const MAX_NOTICES_PER_RESULT: usize = 4;

fn append_job_notices(sandbox: &Sandbox, mut output: String) -> String {
    let notices = sandbox
        .job_notice_queue()
        .take_up_to(MAX_NOTICES_PER_RESULT);
    for message in crate::sandbox::job_notices::render_all(&notices) {
        output.push_str("\n\n");
        output.push_str(&message);
    }
    output
}

pub struct JobStatusTool {
    sandbox: Sandbox,
    max_output_lines: Option<u64>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    /// Per job, the typed text of the line not yet ended by Enter. Permission
    /// is judged on this plus the next chunk, so a command split across calls
    /// is seen whole.
    pending_input: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Jobs whose unterminated line was edited with cursor, history or escape
    /// keys, so the typed text is no longer the line the program will run.
    edited_lines: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Subagents: only the jobs recorded here are visible and controllable.
    job_scope: Option<JobScope>,
}

/// Permission text for a line the terminal's own editing (cursor, history,
/// escape keys) changed: the model cannot know what will run, so it is judged
/// as this opaque command, which an ask policy prompts for.
const EDITED_LINE_JUDGED_AS: &str = "terminal input line edited with cursor or history keys";
/// Most keys one `job_status` call may press.
const MAX_JOB_KEYS: usize = 64;

/// The first byte `job_status` input may not carry: C0 controls other than
/// LF and CR, and DEL. The terminal's line discipline and readline act on
/// them (kill line, erase, signal, and TAB completion, which rewrites the
/// word before the shell runs it), so the typed text would no longer be the
/// command that was judged.
fn disallowed_input_control(input: &str) -> Option<char> {
    input
        .chars()
        .find(|c| (c.is_control() && !matches!(c, '\n' | '\r')) || *c == '\u{7f}')
}

/// Text an interactive shell's history expansion would rewrite before running
/// it: a `!` that starts an event designator, or `^old^new` quick substitution
/// at the start of a line. The typed text would then not be the command that
/// was judged, so such input is refused.
fn history_expansion_trigger(typed: &str) -> Option<&'static str> {
    for line in typed.split(['\n', '\r']) {
        if line.starts_with('^') {
            return Some("a line starting with ^ (history quick substitution)");
        }
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '!'
                && let Some(next) = chars.peek()
                && !next.is_whitespace()
                && *next != '='
            {
                return Some("! followed by text (history expansion)");
            }
        }
    }
    None
}

impl JobStatusTool {
    pub fn new(sandbox: Sandbox, max_output_lines: Option<u64>) -> Self {
        Self {
            sandbox,
            max_output_lines,
            permission: None,
            ask_tx: None,
            pending_input: std::sync::Mutex::new(std::collections::HashMap::new()),
            edited_lines: std::sync::Mutex::new(std::collections::HashSet::new()),
            job_scope: None,
        }
    }

    /// Restrict the tool to the jobs started under `scope` (subagents).
    #[cfg_attr(not(feature = "subagents"), allow(dead_code))]
    pub fn with_job_scope(mut self, scope: JobScope) -> Self {
        self.job_scope = Some(scope);
        self
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

    /// Press named keys in a pty job. The tool renders the bytes. Ctrl-C and
    /// Ctrl-D carry no text and are always allowed; Enter submits the line
    /// typed so far and is judged like typed text; keys that rewrite the line
    /// (cursor, history, Esc, Tab completion) make the line unknowable, so it
    /// is judged as an opaque edited line, and Tab on typed text is refused.
    async fn press_keys(&self, id: &str, keys: &[JobKey]) -> Result<(), String> {
        if keys.is_empty() || keys.len() > MAX_JOB_KEYS {
            return Err(format!(
                "job_status keys must hold 1 to {MAX_JOB_KEYS} keys. Example: {{\"id\":\"job-...\",\"action\":\"input\",\"keys\":[\"ctrl_c\"]}}"
            ));
        }
        let mut bytes = Vec::new();
        for key in keys {
            bytes.extend_from_slice(key.bytes());
        }
        self.sandbox
            .background_job_input_handle(id, bytes.len())
            .map(drop)?;
        let mut text = self
            .pending_input
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .cloned()
            .unwrap_or_default();
        let mut edited = self
            .edited_lines
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(id);
        let lines = apply_job_keys(keys, &mut text, &mut edited)?;
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let (checked, _released) = super::concurrency::prompt_unlocked(check_perm(
                &self.permission,
                &self.ask_tx,
                "shell",
                &line,
            ))
            .await;
            checked.map_err(|error| error.to_string())?;
        }
        let start_text = text.clone();
        let start_edited = edited;
        if let Err(error) = self.sandbox.write_background_job_input(id, &bytes).await {
            if error.written > 0 {
                // Only the keys the program already received count; a key cut
                // mid-sequence leaves the line unknowable (edited).
                let (text, edited) =
                    line_after_partial_keys(keys, error.written, start_text, start_edited);
                self.record_pending_input(id, text, edited);
            }
            return Err(error.message);
        }
        self.record_pending_input(id, text, edited);
        self.settle_after_input(id).await;
        Ok(())
    }

    fn record_pending_input(&self, id: &str, text: String, edited: bool) {
        self.pending_input
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.to_string(), text);
        {
            let mut set = self
                .edited_lines
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if edited {
                set.insert(id.to_string());
            } else {
                set.remove(id);
            }
        }
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

/// Longest unterminated typed line remembered for permission judging.
const MAX_PENDING_INPUT_LINE_BYTES: usize = 16 * 1024;
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
        "Poll, wait for, stop, or list session-scoped background shell jobs. Poll, wait and stop return only output produced since the previous poll/wait/stop of that job (at most 64 KiB per call; a larger range shows its start and end around a byte gap), with the byte offset to continue from. Pass since=<offset> to re-read the complete stream from any offset (for example since=0). The complete output is also logged to the file named in the result, readable with read/grep, and the log outlives the job record. Wait returns when the job ends or timeout_ms expires (default 10000, maximum 60000 milliseconds); expiration leaves the job running. Wait with until=<regex> also returns as soon as output not yet read (or after since) matches the regex, and reports the matched line, so a server that never exits can be waited on for its ready message. List returns ids, commands, statuses and log paths without an id. Stop waits up to 5 seconds for process-tree cleanup."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let schema = serde_json::json!({
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
                    "description": "Only with action=input: text written to the job's terminal as typed, at most 16384 bytes; include \\n to press Enter. The text is permission-checked like a shell command, together with any unterminated line already typed. C0 control characters other than newline and carriage return (Tab, Ctrl-U, Ctrl-C, backspace, ESC, DEL) are refused because a terminal would act on them (completion, line editing, signals); so are history expansions (`!` followed by text, a line starting with `^`). Use keys for Ctrl-C, Ctrl-D, Esc and arrows. Writing gives up after 5 seconds if the program is not reading; the result shows the output produced until it goes quiet (at most 3 seconds)."
                },
                "keys": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_JOB_KEYS,
                    "items": {"type": "string", "enum": JobKey::NAMES},
                    "description": "Only with action=input, instead of input: named keys pressed in order (ctrl_c interrupts the foreground program, ctrl_d sends EOF, ctrl_z suspends it, ctrl_l redraws the screen, plus esc, arrows, page_up, page_down, home, end, delete, shift_tab, f1..f12, tab, enter, backspace for pagers, TUIs and REPLs). The tool renders the bytes. ctrl_c and ctrl_d are always allowed; enter judges the typed line like input; cursor, history, function and esc keys make the line unknowable so the next enter is judged as an opaque edited line (use ctrl_c to clear it); tab on a partly typed line is refused."
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
                },
                "until": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_UNTIL_PATTERN_BYTES,
                    "description": "Only with action=wait: a Rust-syntax regex (linear-time engine). The wait returns early, with the matched line reported, once unread output (or output after since) matches; terminal escape sequences are stripped before matching. If it never matches, the wait ends at the job's exit or timeout_ms and says so; the job keeps running."
                }
            },
            "required": []
        });
        without_unsupported_pty(schema, PTY_SUPPORTED)
    }

    async fn call(&self, args: JobStatusArgs) -> Result<String, ToolError> {
        let output = self.call_inner(args).await?;
        if self.job_scope.is_some() {
            // The notice queue belongs to the parent session.
            return Ok(output);
        }
        Ok(append_job_notices(&self.sandbox, output))
    }
}

impl JobStatusTool {
    async fn call_inner(&self, args: JobStatusArgs) -> Result<String, ToolError> {
        if let Some(timeout) = args.timeout_ms
            && (args.action != JobAction::Wait
                || !(1..=MAX_BACKGROUND_JOB_WAIT_MILLIS).contains(&timeout))
        {
            return Err(ToolError::Msg(
                "job_status timeout_ms is only valid with action=wait and must be between 1 and 60000 milliseconds. Example: {\"id\":\"job-...\",\"action\":\"wait\",\"timeout_ms\":10000}".into(),
            ));
        }
        let until = match args.until.as_deref() {
            None => None,
            Some(_) if args.action != JobAction::Wait => {
                return Err(ToolError::Msg(
                    "job_status until is only valid with action=wait. Example: {\"id\":\"job-...\",\"action\":\"wait\",\"until\":\"listening on\"}".into(),
                ));
            }
            Some(pattern) => Some(compile_until_pattern(pattern)?),
        };
        if args.input.is_some() && args.keys.is_some() {
            return Err(ToolError::Msg(
                "job_status takes either input (text) or keys (named keys), not both.".into(),
            ));
        }
        if args.keys.is_some() && args.action != JobAction::Input {
            return Err(ToolError::Msg(
                "job_status keys are only valid with action=input.".into(),
            ));
        }
        if args.input.is_some() != (args.action == JobAction::Input) && args.keys.is_none() {
            return Err(ToolError::Msg(
                "job_status input (or keys) is required with, and only valid with, action=input. Example: {\"id\":\"job-...\",\"action\":\"input\",\"input\":\"print(1+1)\\n\"}".into(),
            ));
        }
        if args.action == JobAction::List {
            if args.id.is_some() {
                return Err(ToolError::Msg(
                    "job_status action=list does not take an id. Example: {\"action\":\"list\"}"
                        .into(),
                ));
            }
            let summaries = self
                .sandbox
                .background_job_summaries()
                .into_iter()
                .filter(|job| {
                    self.job_scope
                        .as_ref()
                        .is_none_or(|scope| scope.contains(&job.id))
                })
                .collect::<Vec<_>>();
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
        let mut wait_note: Option<String> = None;
        let id = args.id.as_deref().ok_or_else(|| ToolError::Msg(
            "job_status requires an id for poll, wait or stop. Example: {\"id\":\"job-...\",\"action\":\"wait\"}".into(),
        ))?;
        if self
            .job_scope
            .as_ref()
            .is_some_and(|scope| !scope.contains(id))
        {
            return Err(ToolError::Msg(format!(
                "unknown background job '{id}': this subagent can only use jobs it started itself. Example: {{\"action\":\"list\"}}"
            )));
        }
        match args.action {
            JobAction::Poll => self.sandbox.background_job_snapshot(id).map(drop),
            JobAction::Stop => {
                self.pending_input
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(id);
                self.edited_lines
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(id);
                self.sandbox.stop_background_job(id).await.map(drop)
            }
            JobAction::Wait => {
                let wait = std::time::Duration::from_millis(args.timeout_ms.unwrap_or(10_000));
                match &until {
                    None => self.sandbox.wait_background_job(id, wait).await.map(drop),
                    Some(regex) => {
                        let mut scan_from = match args.since {
                            Some(since) => since,
                            None => self
                                .sandbox
                                .background_job_cursor(id)
                                .map_err(ToolError::Msg)?,
                        };
                        let mut found: Option<UntilMatch> = None;
                        let mut unscanned: u64 = 0;
                        let mut scan = || {
                            while found.is_none() {
                                let Ok(window) = self.sandbox.peek_background_job_window(
                                    id,
                                    scan_from,
                                    UNTIL_SCAN_BYTES,
                                ) else {
                                    break;
                                };
                                let (hit, next, lost) = scan_for_pattern_chunked(regex, &window);
                                unscanned += lost;
                                found = hit;
                                if next <= scan_from && found.is_none() {
                                    break;
                                }
                                scan_from = next;
                                if window.next <= scan_from && found.is_none() {
                                    break;
                                }
                            }
                            found.is_some()
                        };
                        let snapshot = self
                            .sandbox
                            .wait_background_job_until(id, wait, Some(UNTIL_POLL_INTERVAL), &mut scan)
                            .await
                            .map_err(ToolError::Msg)?;
                        // The wait may end on job exit or the deadline between
                        // polls with output never scanned: scan once more.
                        scan();
                        let skipped_note = if found.is_none() && unscanned > 0 {
                            format!(
                                " {unscanned} bytes of output were not available to scan (not retained or logged)."
                            )
                        } else {
                            String::new()
                        };
                        wait_note = Some(match &found {
                            Some(hit) => format!(
                                "Wait: output matched /{}/ (in bytes {}..{}): {}",
                                regex.as_str(),
                                hit.from,
                                hit.to,
                                hit.line
                            ),
                            None if snapshot.status.is_terminal() => format!(
                                "Wait: the job ended without output matching /{}/; any unread output follows.{skipped_note}",
                                regex.as_str()
                            ),
                            None => format!(
                                "Wait: output did not match /{}/ within {} ms; the job is still running.{skipped_note}",
                                regex.as_str(),
                                wait.as_millis()
                            ),
                        });
                        Ok(())
                    }
                }
            }
            JobAction::Input if args.keys.is_some() => {
                self.press_keys(id, args.keys.as_deref().unwrap_or_default())
                    .await
            }
            JobAction::Input => {
                let input = args.input.as_deref().unwrap_or_default();
                if let Some(control) = disallowed_input_control(input) {
                    return Err(ToolError::Msg(format!(
                        "job_status input contains the control character U+{:04X}, which the terminal would interpret (erase, kill-line, signal, escape) instead of typing it, so the command could differ from what was permission-checked. Send plain text with \\n only; use keys (ctrl_c, ctrl_d, esc, arrows) for named keys.",
                        control as u32
                    )));
                }
                if let Some(trigger) = history_expansion_trigger(input) {
                    return Err(ToolError::Msg(format!(
                        "job_status input contains {trigger}, which an interactive shell rewrites before running it, so the command could differ from what was permission-checked. Avoid ! and a leading ^, or quote them out of the typed text."
                    )));
                }
                // Typed text can run anything the terminal's program accepts,
                // so it is judged exactly like a shell script: deny rules
                // apply, and manual mode prompts. Unknown or finished jobs are
                // rejected before any prompt.
                self.sandbox
                    .background_job_input_handle(id, input.len())
                    .map(drop)
                    .map_err(ToolError::Msg)?;
                // The line being typed spans calls: judge it with the unterminated
                // text already sent, so `to` then `uch x\n` is `touch x`.
                let pending = self
                    .pending_input
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(id)
                    .cloned()
                    .unwrap_or_default();
                let combined = format!("{pending}{input}");
                let edited = self
                    .edited_lines
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .contains(id);
                // Judge an edited line's first Enter as the opaque edited line.
                let judged = match (edited, combined.find(['\n', '\r'])) {
                    (true, Some(end)) => format!("{EDITED_LINE_JUDGED_AS}{}", &combined[end..]),
                    _ => combined.clone(),
                };
                let tail = combined
                    .rsplit(['\n', '\r'])
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if tail.len() > MAX_PENDING_INPUT_LINE_BYTES {
                    return Err(ToolError::Msg(
                        "job_status input: the unterminated line is too long; end it with \\n".into(),
                    ));
                }
                let (checked, _released) = super::concurrency::prompt_unlocked(check_perm(
                    &self.permission,
                    &self.ask_tx,
                    "shell",
                    // Typing Enter is not part of the command text a rule
                    // would name.
                    judged.trim_end_matches(['\n', '\r']),
                ))
                .await;
                checked?;
                match self
                    .sandbox
                    .write_background_job_input(id, input.as_bytes())
                    .await
                {
                    Ok(()) => {
                        self.pending_input
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(id.to_string(), tail);
                        if input.contains(['\n', '\r']) {
                            self.edited_lines
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .remove(id);
                        }
                        // Give the program time to echo and answer.
                        self.settle_after_input(id).await;
                        Ok(())
                    }
                    Err(error) => {
                        if error.written > 0 {
                            // The program already received a prefix: record it
                            // so the follow-up is judged with the real line.
                            let accepted = String::from_utf8_lossy(
                                &input.as_bytes()[..error.written.min(input.len())],
                            );
                            let combined = format!("{pending}{accepted}");
                            let tail = combined.rsplit(['\n', '\r']).next().unwrap_or_default();
                            self.pending_input
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .insert(id.to_string(), tail.to_string());
                            if accepted.contains(['\n', '\r']) {
                                self.edited_lines
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .remove(id);
                            }
                        }
                        Err(error.message)
                    }
                }
            }
            JobAction::List => unreachable!("list returned before id lookup"),
        }
        .map_err(ToolError::Msg)?;
        let output = self
            .sandbox
            .read_background_job_output(id, args.since, JOB_OUTPUT_READ_BYTES)
            .map_err(ToolError::Msg)?;
        let rendered = render_background_job(output, self.max_output_lines);
        Ok(match wait_note {
            Some(note) => format!("{note}\n{rendered}"),
            None => rendered,
        })
    }
}

/// Track the line a key sequence leaves typed: `text` is the unterminated
/// text, `edited` marks a line rewritten by editing keys, and the returned
/// lines are the ones Enter submitted (to be permission-judged).
fn apply_job_keys(
    keys: &[JobKey],
    text: &mut String,
    edited: &mut bool,
) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for key in keys {
        match key {
            JobKey::CtrlC => {
                text.clear();
                *edited = false;
            }
            JobKey::CtrlD => {
                if !text.is_empty() {
                    *edited = true;
                }
            }
            JobKey::Backspace => {
                if !*edited {
                    text.pop();
                }
            }
            JobKey::Left | JobKey::Right | JobKey::CtrlZ => {
                if !text.is_empty() || *edited {
                    *edited = true;
                }
            }
            // Redraw only: the line editor's buffer is unchanged.
            JobKey::CtrlL => {}
            JobKey::Tab => {
                if !text.is_empty() || *edited {
                    return Err("job_status keys: tab on a partly typed line lets the shell complete it into a different command than was permission-checked. Send the full text instead, or press ctrl_c first.".into());
                }
            }
            JobKey::Enter => {
                lines.push(if *edited {
                    EDITED_LINE_JUDGED_AS.to_string()
                } else {
                    std::mem::take(text)
                });
                text.clear();
                *edited = false;
            }
            // Arrows, history, paging, Home/End/Delete, Shift-Tab, F-keys, Esc.
            other => {
                if other.edits_line() {
                    *edited = true;
                }
            }
        }
    }
    Ok(lines)
}

/// The typed line after only the first `written` bytes of `keys` reached the
/// program. A key cut mid-sequence leaves the line unknowable (edited).
fn line_after_partial_keys(
    keys: &[JobKey],
    written: usize,
    mut text: String,
    mut edited: bool,
) -> (String, bool) {
    let mut sent = 0usize;
    let mut applied = 0usize;
    for key in keys {
        let next = sent + key.bytes().len();
        if next > written {
            break;
        }
        sent = next;
        applied += 1;
    }
    let _ = apply_job_keys(&keys[..applied], &mut text, &mut edited);
    if sent < written {
        edited = true;
    }
    (text, edited)
}

/// Longest `until` regex source accepted.
const MAX_UNTIL_PATTERN_BYTES: usize = 512;
/// How often an `until` wait re-checks output (job output does not notify).
const UNTIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// Bytes of new output examined per `until` scan step.
const UNTIL_SCAN_BYTES: usize = 256 * 1024;

fn compile_until_pattern(pattern: &str) -> Result<regex::Regex, ToolError> {
    if pattern.is_empty() || pattern.len() > MAX_UNTIL_PATTERN_BYTES {
        return Err(ToolError::Msg(format!(
            "job_status until must be 1..={MAX_UNTIL_PATTERN_BYTES} bytes"
        )));
    }
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|error| {
            ToolError::Msg(format!(
                "job_status until is not a valid regex (Rust syntax): {error}"
            ))
        })
}

struct UntilMatch {
    from: u64,
    to: u64,
    line: String,
}

/// Look for `regex` in one non-advancing output window. Returns the hit (with
/// the matched line, bounded) and the offset the next scan should start from:
/// the start of the last, possibly unfinished, line so a ready message split
/// across reads is still found.
fn scan_for_pattern(regex: &regex::Regex, window: &OutputWindow) -> (Option<UntilMatch>, u64) {
    let tail_start = window.next - window.tail.len() as u64;
    let pieces = [(window.start, &window.head), (tail_start, &window.tail)];
    for (offset, bytes) in pieces {
        if bytes.is_empty() {
            continue;
        }
        let text = normalize_terminal_output(&String::from_utf8_lossy(bytes));
        if let Some(found) = regex.find(&text) {
            let line_start = text[..found.start()].rfind('\n').map_or(0, |at| at + 1);
            let line_end = text[found.start()..]
                .find('\n')
                .map_or(text.len(), |at| found.start() + at);
            let line: String = text[line_start..line_end].chars().take(300).collect();
            return (
                Some(UntilMatch {
                    from: offset,
                    to: window.next,
                    line,
                }),
                window.next,
            );
        }
    }
    let (offset, last) = if window.tail.is_empty() {
        (window.start, &window.head)
    } else {
        (tail_start, &window.tail)
    };
    let next = match last.iter().rposition(|byte| *byte == b'\n') {
        Some(at) => offset + at as u64 + 1,
        None if last.len() > 8192 => window.next - 8192,
        None => offset,
    };
    (None, next.max(window.start))
}

/// [`scan_for_pattern`] that also covers the middle of a window the capture
/// had to split (`skipped > 0`): it scans only the head and resumes after it,
/// so the whole backlog is examined in `UNTIL_SCAN_BYTES` chunks. The third
/// value counts bytes that could not be read at all (neither retained nor
/// logged) and were therefore jumped over.
fn scan_for_pattern_chunked(
    regex: &regex::Regex,
    window: &OutputWindow,
) -> (Option<UntilMatch>, u64, u64) {
    if window.skipped == 0 || window.head.is_empty() {
        let (hit, next) = scan_for_pattern(regex, window);
        let lost = if hit.is_none() { window.skipped } else { 0 };
        return (hit, next, lost);
    }
    let head_only = OutputWindow {
        start: window.start,
        head: window.head.clone(),
        skipped: 0,
        tail: Vec::new(),
        next: window.start + window.head.len() as u64,
    };
    let (hit, next) = scan_for_pattern(regex, &head_only);
    if hit.is_some() {
        return (hit, next, 0);
    }
    // Resume at the start of the head's last line so a match split across the
    // chunk boundary is still found; always make progress.
    let resume = if next > window.start {
        next
    } else {
        head_only.next
    };
    (None, resume, 0)
}

/// Source-compatibility alias for integrations that still construct the old
/// Rust type. The model-visible tool name is always `shell`.
#[cfg(test)]
pub type BashTool = ShellTool;

/// `Terminated by signal: SIGKILL (9)`; shared by foreground results, job
/// status, auto-background results and completion notices.
pub(crate) fn describe_signal(number: i32) -> String {
    #[cfg(unix)]
    let name = nix::sys::signal::Signal::try_from(number)
        .map(|signal| format!("{signal:?}"))
        .unwrap_or_else(|_| "UNKNOWN".to_string());
    #[cfg(not(unix))]
    let name = "UNKNOWN".to_string();
    format!("Terminated by signal: {name} ({number})")
}

fn render_termination(status: Option<std::process::ExitStatus>) -> Option<String> {
    let status = status?;
    if let Some(code) = status.code() {
        return (code != 0).then(|| format!("Exit code: {code}"));
    }

    if let Some(number) = crate::sandbox::exit_signal(status) {
        return Some(describe_signal(number));
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
    } else if let Some(signal) = snapshot.signal {
        result.push_str(&format!("\n{}", describe_signal(signal)));
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
    } else if transcript.omitted_bytes > 0 || transcript.log_error.is_some() {
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

    #[test]
    fn shell_description_discloses_the_private_tmp_only_under_bwrap() {
        let off = ShellTool::new(None, None, Sandbox::new(false, "bwrap"), None).description();
        assert!(!off.contains("private in-memory tmpfs"), "{off}");
        let on = ShellTool::new(None, None, Sandbox::new(true, "bwrap"), None).description();
        assert_eq!(
            on.contains("private in-memory tmpfs discarded when each command ends"),
            cfg!(target_os = "linux"),
            "{on}"
        );
        assert!(SHELL_PRIVATE_TMP_NOTE.contains("workspace"));
    }

    fn test_tool() -> BashTool {
        BashTool::new(None, None, Sandbox::new(false, "bwrap"), None)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_spawn_failure_reports_its_reason() {
        let sandbox = Sandbox::new(false, "bwrap").with_working_dir("/nonexistent-mini-agent-cwd");
        let error = BashTool::new(None, None, sandbox, None)
            .call(BashArgs {
                command: "echo hi".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to spawn command"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_commands_start_with_a_non_interactive_environment() {
        let output = test_tool()
            .call(BashArgs {
                command: "echo \"$PAGER $GIT_PAGER $GH_PAGER $GIT_TERMINAL_PROMPT $GIT_EDITOR\""
                    .to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(output.trim(), "cat cat cat 0 :");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn line_cap_saves_the_full_output_to_a_log_even_under_the_byte_cap() {
        let dir = crate::tests::support::temp::TempDir::new("line-cap-log");
        let logs = dir.path().join("logs");
        let mut shell = test_tool().with_output_log_dir(Some(logs.clone()));
        shell.max_output_lines = Some(50);
        let result = shell
            .call(BashArgs {
                command: "seq 1 5000".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.contains("lines omitted"), "{result}");
        let footer = result
            .lines()
            .find(|line| line.contains("saved to"))
            .unwrap_or_else(|| panic!("no log footer: {result}"));
        let logged = std::fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .next()
            .unwrap();
        assert!(footer.contains(&logged.display().to_string()), "{footer}");
        let body = std::fs::read_to_string(&logged).unwrap();
        assert_eq!(body.lines().count(), 5000);
        assert!(body.contains("\n2500\n"));
        // Output under the cap creates no log.
        let small = shell
            .call(BashArgs {
                command: "seq 1 10".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!small.contains("saved to"), "{small}");
        assert_eq!(std::fs::read_dir(&logs).unwrap().count(), 1);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_job_status_only_sees_and_controls_the_jobs_its_shell_started() {
        let parent = test_tool();
        let parent_started = parent
            .call(BashArgs {
                command: "sleep 30".to_string(),
                timeout: Some(60_000),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let parent_id = background_id(&parent_started);
        // The child shares the parent's sandbox (and so its job table).
        let scope = JobScope::new(parent.sandbox.clone());
        let child =
            BashTool::new(None, None, parent.sandbox.clone(), None).with_job_scope(scope.clone());
        let status = JobStatusTool::new(parent.sandbox.clone(), None).with_job_scope(scope);
        let child_started = child
            .call(BashArgs {
                command: "sleep 30".to_string(),
                timeout: Some(60_000),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let child_id = background_id(&child_started);

        let listed = status
            .call(JobStatusArgs {
                action: JobAction::List,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(listed.contains(&child_id), "{listed}");
        assert!(!listed.contains(&parent_id), "{listed}");
        for action in [JobAction::Poll, JobAction::Stop, JobAction::Wait] {
            let error = status
                .call(JobStatusArgs {
                    id: Some(parent_id.clone()),
                    action,
                    timeout_ms: None,
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("only use jobs it started"), "{error}");
        }
        assert_eq!(parent.sandbox.running_background_job_count(), 2);
        status
            .call(JobStatusArgs {
                id: Some(child_id.clone()),
                action: JobAction::Stop,
                ..Default::default()
            })
            .await
            .unwrap();
        parent
            .sandbox
            .stop_background_job(&parent_id)
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_job_scope_stops_the_jobs_a_subagent_left_running() {
        let sandbox = Sandbox::new(false, "bwrap");
        let scope = JobScope::new(sandbox.clone());
        let child = BashTool::new(None, None, sandbox.clone(), None).with_job_scope(scope.clone());
        child
            .call(BashArgs {
                command: "sleep 30".to_string(),
                timeout: Some(60_000),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(sandbox.running_background_job_count(), 1);
        drop(child);
        drop(scope);
        let deadline = Instant::now() + Duration::from_secs(5);
        while sandbox.running_background_job_count() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(sandbox.running_background_job_count(), 0);
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

    #[test]
    fn until_scan_finds_a_match_in_either_part_and_resumes_at_the_last_line() {
        let regex = compile_until_pattern("listening on \\d+").unwrap();
        let window = OutputWindow {
            start: 100,
            head: b"booting\n\x1b[32mlistening on 8080\x1b[0m\n".to_vec(),
            skipped: 0,
            tail: Vec::new(),
            next: 100 + 33,
        };
        let (hit, _) = scan_for_pattern(&regex, &window);
        let hit = hit.expect("match through terminal escapes");
        assert_eq!(hit.line, "listening on 8080");
        let in_tail = OutputWindow {
            start: 0,
            head: b"nothing here\n".to_vec(),
            skipped: 5,
            tail: b"x\nlistening on 9\n".to_vec(),
            next: 40,
        };
        assert!(scan_for_pattern(&regex, &in_tail).0.is_some());
        // A partial last line is re-scanned from its start next time.
        let partial = OutputWindow {
            start: 0,
            head: b"done\nlistening o".to_vec(),
            skipped: 0,
            tail: Vec::new(),
            next: 16,
        };
        let (none, next) = scan_for_pattern(&regex, &partial);
        assert!(none.is_none());
        assert_eq!(next, 5);
        assert!(compile_until_pattern("(").is_err());
        assert!(compile_until_pattern(&"a".repeat(MAX_UNTIL_PATTERN_BYTES + 1)).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_until_returns_when_output_matches_and_leaves_the_job_running() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "echo booting; sleep 0.5; echo listening on 8080; sleep 30".to_string(),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        let began = Instant::now();
        let waited = status
            .call(JobStatusArgs {
                id: Some(id.clone()),
                action: JobAction::Wait,
                timeout_ms: Some(20_000),
                until: Some("listening on \\d+".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(began.elapsed() < Duration::from_secs(10), "{waited}");
        assert!(
            waited.contains("Wait: output matched /listening on \\d+/"),
            "{waited}"
        );
        assert!(waited.contains("listening on 8080"), "{waited}");
        assert!(waited.contains("Status: running"), "{waited}");

        // Nothing new matches: the wait ends at its deadline and says so.
        let missed = status
            .call(JobStatusArgs {
                id: Some(id.clone()),
                action: JobAction::Wait,
                timeout_ms: Some(300),
                until: Some("never-printed".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            missed.contains("did not match /never-printed/ within 300 ms"),
            "{missed}"
        );
        assert!(missed.contains("the job is still running"), "{missed}");

        for bad in [
            JobStatusArgs {
                id: Some(id.clone()),
                action: JobAction::Poll,
                until: Some("x".into()),
                ..Default::default()
            },
            JobStatusArgs {
                id: Some(id.clone()),
                action: JobAction::Wait,
                until: Some("(".into()),
                ..Default::default()
            },
        ] {
            assert!(status.call(bad).await.is_err());
        }
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_until_scans_output_of_a_job_that_already_ended() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "echo ready; exit 0".to_string(),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        // Let the job exit before the first wait so the regex is never
        // evaluated while it is still running.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let waited = status
            .call(JobStatusArgs {
                id: Some(id),
                action: JobAction::Wait,
                timeout_ms: Some(5_000),
                until: Some("ready".into()),
                since: Some(0),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(waited.contains("Wait: output matched /ready/"), "{waited}");
        assert!(
            !waited.contains("ended without output matching"),
            "{waited}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_until_matches_a_marker_in_the_middle_of_a_large_backlog() {
        let dir = crate::tests::support::temp::TempDir::new("until-backlog");
        let shell = test_tool().with_output_log_dir(Some(dir.path().join("logs")));
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let started = shell
            .call(BashArgs {
                command: "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c 300000; \
                          echo MIDDLE_MARKER_7; \
                          yes yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy | head -c 800000; sleep 30"
                    .to_string(),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        let id = background_id(&started);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let waited = status
            .call(JobStatusArgs {
                id: Some(id.clone()),
                action: JobAction::Wait,
                timeout_ms: Some(20_000),
                until: Some("MIDDLE_MARKER_7".into()),
                since: Some(0),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            waited.contains("Wait: output matched /MIDDLE_MARKER_7/"),
            "{}",
            &waited[..waited.len().min(400)]
        );
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    #[test]
    fn chunked_until_scan_walks_the_head_of_a_split_window_and_resumes() {
        let regex = compile_until_pattern("needle").unwrap();
        let split = OutputWindow {
            start: 0,
            head: b"aaa\nbbb needle\nccc".to_vec(),
            skipped: 1000,
            tail: b"zzz\n".to_vec(),
            next: 1024,
        };
        assert!(scan_for_pattern_chunked(&regex, &split).0.is_some());
        let no_hit = OutputWindow {
            head: b"aaa\nbbb\nccc".to_vec(),
            ..split
        };
        let (hit, next, lost) = scan_for_pattern_chunked(&regex, &no_hit);
        assert!(hit.is_none());
        assert_eq!((next, lost), (8, 0), "resume at the head's last line");
        let unreadable = OutputWindow {
            head: Vec::new(),
            ..no_hit
        };
        let (_, _, lost) = scan_for_pattern_chunked(&regex, &unreadable);
        assert_eq!(lost, 1000, "unreadable bytes are reported, not hidden");
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

    #[test]
    fn partial_key_writes_track_only_the_bytes_the_program_received() {
        let enter = JobKey::Enter.bytes().len();
        // A key cut mid-sequence leaves the line unknowable.
        let cut = JobKey::Up.bytes().len() - 1;
        assert!(cut > 0);
        let (text, edited) =
            line_after_partial_keys(&[JobKey::Up], cut, "rm -rf build".to_string(), false);
        assert_eq!((text.as_str(), edited), ("rm -rf build", true));
        // Nothing delivered: state is unchanged.
        let (text, edited) =
            line_after_partial_keys(&[JobKey::Enter], 0, "rm -rf build".to_string(), false);
        assert_eq!((text.as_str(), edited), ("rm -rf build", false));
        // Enter fully delivered: the line was submitted.
        let (text, edited) =
            line_after_partial_keys(&[JobKey::Enter], enter, "rm -rf build".to_string(), false);
        assert_eq!((text.as_str(), edited), ("", false));
    }

    /// A write that times out after the terminal accepted a prefix must still
    /// record that prefix, so the follow-up chunk is judged with the real line.
    #[cfg(unix)]
    #[tokio::test]
    async fn partially_written_job_input_is_tracked_as_pending_text() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        // `sleep` never reads, so the tty input queue fills up.
        let id = start_pty_job(&shell, "sleep 30").await;
        let input = format!("rm -rf {}", "x".repeat(15_000));
        let Err(error) = job_input(&status, &id, &input).await else {
            // This platform buffered or dropped the overflow instead of blocking.
            shell.sandbox.stop_background_job(&id).await.unwrap();
            return;
        };
        assert!(error.to_string().contains("bytes were accepted"), "{error}");
        let pending = status
            .pending_input
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default();
        assert!(pending.starts_with("rm -rf x"), "{pending:?}");
        shell.sandbox.stop_background_job(&id).await.unwrap();
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

    #[cfg(unix)]
    #[tokio::test]
    async fn job_input_refuses_control_bytes_and_judges_split_lines_whole() {
        use crate::permission::{Action, PermissionConfig, ToolPerm};
        let dir = std::env::current_dir().unwrap();
        let kill = dir.join(format!("job-input-kill-{}", std::process::id()));
        let split = dir.join(format!("job-input-split-{}", std::process::id()));
        let _ = std::fs::remove_file(&kill);
        let _ = std::fs::remove_file(&split);
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
        // `ls` + Ctrl-U + `touch ...` is judged as one allowed word without the guard.
        for byte in [
            "\u{15}", "\u{8}", "\u{7f}", "\u{17}", "\u{3}", "\u{4}", "\u{1b}",
        ] {
            let error = job_input(
                &status,
                &id,
                &format!(
                    "ls{byte}touch {}\n",
                    kill.file_name().unwrap().to_string_lossy()
                ),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("control character"), "{error}");
        }
        // The deny still holds when the command is split over two calls.
        job_input(&status, &id, "to").await.unwrap();
        let error = job_input(
            &status,
            &id,
            &format!("uch {}\n", split.file_name().unwrap().to_string_lossy()),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("denied"),
            "{error}"
        );
        job_input(&status, &id, "echo alive\n").await.unwrap();
        assert!(!kill.exists() && !split.exists(), "refused input ran");
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    async fn job_keys(
        status: &JobStatusTool,
        id: &str,
        keys: &[JobKey],
    ) -> Result<String, ToolError> {
        status
            .call(JobStatusArgs {
                id: Some(id.to_string()),
                action: JobAction::Input,
                keys: Some(keys.to_vec()),
                ..Default::default()
            })
            .await
    }

    async fn start_job(shell: &BashTool, command: &str) -> String {
        let started = shell
            .call(BashArgs {
                command: command.to_string(),
                timeout: Some(30_000),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap();
        background_id(&started)
    }

    async fn wait_for_notice(shell: &BashTool) {
        tokio::time::timeout(
            Duration::from_secs(20),
            shell.sandbox.job_notice_queue().wait_pending(),
        )
        .await
        .expect("a job that ended after the turn must queue a notice");
    }

    /// A job that finishes after the turn ended queues one bounded notice,
    /// delivered once with the next tool result.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_completion_notice_is_queued_and_delivered_once() {
        let shell = test_tool();
        let id = start_job(&shell, "echo build-tail-marker; exit 3").await;
        wait_for_notice(&shell).await;
        assert_eq!(shell.sandbox.job_notice_queue().len(), 1);
        let result = shell
            .call(BashArgs {
                command: "echo unrelated".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.contains("unrelated"), "{result}");
        assert!(
            result.contains(&format!("Background shell job {id} completed")),
            "{result}"
        );
        assert!(result.contains("exit code 3"), "{result}");
        assert!(result.contains("build-tail-marker"), "{result}");
        assert!(result.contains("grants no permission"), "{result}");
        // Delivered once.
        let again = shell
            .call(BashArgs {
                command: "echo second".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!again.contains("Background shell job"), "{again}");
    }

    /// A subagent shell shares the parent's notice queue but must not drain it.
    #[cfg(unix)]
    #[tokio::test]
    async fn subagent_shell_results_leave_the_parent_notices_queued() {
        let parent = test_tool();
        let id = start_job(&parent, "echo parent-job-marker").await;
        wait_for_notice(&parent).await;
        let scope = JobScope::new(parent.sandbox.clone());
        let child = BashTool::new(None, None, parent.sandbox.clone(), None).with_job_scope(scope);
        let child_result = child
            .call(BashArgs {
                command: "echo child".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !child_result.contains("Background shell job"),
            "{child_result}"
        );
        assert_eq!(parent.sandbox.job_notice_queue().len(), 1);
        let parent_result = parent
            .call(BashArgs {
                command: "echo parent".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            parent_result.contains(&format!("Background shell job {id} completed")),
            "{parent_result}"
        );
    }

    /// A job killed by a signal has no exit code; every surface must still
    /// say how it ended.
    #[cfg(unix)]
    #[tokio::test]
    async fn signal_killed_background_job_reports_the_signal() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let id = start_job(&shell, "kill -9 $$").await;
        let terminal = wait_for_terminal_job(&status, &id).await;
        assert!(
            terminal.contains("Terminated by signal: SIGKILL (9)"),
            "{terminal}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_killed_job_notice_and_auto_background_result_name_the_signal() {
        let shell = test_tool();
        start_job(&shell, "kill -9 $$").await;
        wait_for_notice(&shell).await;
        let notice = shell
            .call(BashArgs {
                command: "echo next".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            notice.contains("Terminated by signal: SIGKILL (9)"),
            "{notice}"
        );
        let shell = test_tool().with_auto_background(true);
        let result = shell
            .call(BashArgs {
                command: "kill -9 $$".into(),
                auto_background: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            result.contains("Terminated by signal: SIGKILL (9)"),
            "{result}"
        );
    }

    /// Reading the terminal state, stopping a job, or turning notices off
    /// queues nothing the model already knows.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_notices_skip_known_stopped_and_disabled_jobs() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        // Seen through job_status wait: the notice is withdrawn.
        let seen = start_job(&shell, "echo seen").await;
        let terminal = wait_for_terminal_job(&status, &seen).await;
        assert!(terminal.contains("Status: completed"), "{terminal}");
        assert!(!terminal.contains("Background shell job"), "{terminal}");
        assert_eq!(shell.sandbox.job_notice_queue().len(), 0);
        // Stopped by the model: no notice.
        let stopped = start_job(&shell, "sleep 300").await;
        shell.sandbox.stop_background_job(&stopped).await.unwrap();
        assert_eq!(shell.sandbox.job_notice_queue().len(), 0);
        // Opt-out.
        shell.sandbox.job_notice_queue().set_enabled(false);
        let quiet = start_job(&shell, "echo quiet").await;
        let _ = wait_for_terminal_job(&status, &quiet).await;
        shell.sandbox.job_notice_queue().set_enabled(true);
        start_job(&shell, "echo loud").await;
        wait_for_notice(&shell).await;
        assert_eq!(shell.sandbox.job_notice_queue().len(), 1);
    }

    fn workdir_fixture(label: &str) -> std::path::PathBuf {
        let dir = std::env::current_dir()
            .unwrap()
            .join(format!("workdir-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workdir_runs_foreground_and_background_commands_in_the_directory() {
        let fixture = workdir_fixture("run");
        let rel = format!("{}/sub", fixture.file_name().unwrap().to_string_lossy());
        let shell = test_tool();
        let out = shell
            .call(BashArgs {
                command: "pwd".into(),
                workdir: Some(rel.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let real = fixture.join("sub").canonicalize().unwrap();
        assert!(out.contains(real.to_str().unwrap()), "{out}");
        // Absolute inside the workspace works too.
        let out = shell
            .call(BashArgs {
                command: "pwd".into(),
                workdir: Some(fixture.join("sub/deep").to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
    /// When bwrap remaps the workspace to `/workspace`, `cd` must use that
    /// path and the advertised alias spelling must be accepted.
    #[cfg(unix)]
    #[test]
    fn workdir_uses_the_sandbox_workspace_alias() {
        let fixture = workdir_fixture("alias").canonicalize().unwrap();
        let sandbox = Sandbox::new(false, "bwrap").with_working_dir(&fixture);
        let alias = std::path::Path::new("/workspace");
        let relative = sandbox
            .command_in_workdir_aliased("sub", "ls", Some(alias))
            .unwrap();
        assert_eq!(relative, "cd -- '/workspace/sub' || exit 1; ls");
        let spelled = sandbox
            .command_in_workdir_aliased("/workspace/sub/deep", "ls", Some(alias))
            .unwrap();
        assert_eq!(spelled, "cd -- '/workspace/sub/deep' || exit 1; ls");
        let root = sandbox
            .command_in_workdir_aliased("/workspace", "ls", Some(alias))
            .unwrap();
        assert_eq!(root, "cd -- '/workspace' || exit 1; ls");
        // The alias spelling cannot be used to leave the workspace.
        let escape = sandbox
            .command_in_workdir_aliased("/workspace/../..", "ls", Some(alias))
            .unwrap_err();
        assert!(escape.contains("outside the workspace"), "{escape}");
        // Without an alias the host path is still emitted.
        let host = sandbox
            .command_in_workdir_aliased("sub", "ls", None)
            .unwrap();
        assert!(host.contains(&fixture.join("sub").to_string_lossy().into_owned()));
        std::fs::remove_dir_all(&fixture).ok();
    }

            .unwrap();
        assert!(out.contains("deep"), "{out}");
        // cd does not persist; the next call starts at the workspace root.
        shell
            .call(BashArgs {
                command: "cd sub && pwd".into(),
                workdir: Some(fixture.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        let root = shell
            .call(BashArgs {
                command: "pwd".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!root.contains("workdir-run"), "{root}");
        // A background job runs there as well.
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let id = start_job(&shell, "true").await;
        let _ = wait_for_terminal_job(&status, &id).await;
        let started = shell
            .call(BashArgs {
                command: "pwd".into(),
                background: true,
                workdir: Some(rel),
                ..Default::default()
            })
            .await
            .unwrap();
        let terminal = wait_for_terminal_job(&status, &background_id(&started)).await;
        assert!(terminal.contains(real.to_str().unwrap()), "{terminal}");
        std::fs::remove_dir_all(&fixture).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workdir_refuses_escapes_links_files_and_missing_directories() {
        let fixture = workdir_fixture("escape");
        let outside = std::env::temp_dir().join(format!("workdir-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.join("link")).unwrap();
        std::fs::write(fixture.join("file.txt"), "x").unwrap();
        let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
        let shell = test_tool();
        for (workdir, needle) in [
            (format!("{name}/../.."), "outside the workspace"),
            ("..".to_string(), "outside the workspace"),
            (
                outside.to_string_lossy().into_owned(),
                "outside the workspace",
            ),
            ("/".to_string(), "outside the workspace"),
            (format!("{name}/link"), "outside the workspace"),
            (format!("{name}/file.txt"), "not a directory"),
            (format!("{name}/missing"), "cannot be used"),
            (String::new(), "non-empty"),
        ] {
            let error = shell
                .call(BashArgs {
                    command: format!("touch {}/escaped", outside.display()),
                    workdir: Some(workdir.clone()),
                    ..Default::default()
                })
                .await
                .unwrap_err();
            assert!(error.to_string().contains(needle), "{workdir}: {error}");
        }
        assert!(!outside.join("escaped").exists());
        std::fs::remove_dir_all(&fixture).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    /// The permission key carries the directory, and a deny rule on the bare
    /// command still applies when a workdir is passed.
    #[cfg(unix)]
    #[tokio::test]
    async fn workdir_permission_identity_includes_the_directory_and_deny_rules_hold() {
        use crate::permission::{Action, PermissionConfig, ToolPerm};
        let fixture = workdir_fixture("perm");
        let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
        let marker = fixture.join("sub/denied");
        let mut shell = test_tool();
        shell.permission = Some(shell_permission(
            PermissionConfig {
                bash: Some(ToolPerm::Granular(
                    [
                        ("*".to_string(), Action::Allow),
                        ("touch *".to_string(), Action::Deny),
                    ]
                    .into(),
                )),
                ..Default::default()
            },
            crate::permission::SecurityMode::Yolo,
        ));
        let error = shell
            .call(BashArgs {
                command: "touch denied".into(),
                workdir: Some(format!("{name}/sub")),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("denied"),
            "{error}"
        );
        assert!(!marker.exists());
        let ok = shell
            .call(BashArgs {
                command: "echo fine".into(),
                workdir: Some(format!("{name}/sub")),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(ok.contains("fine"), "{ok}");
        std::fs::remove_dir_all(&fixture).unwrap();
    }

    #[test]
    fn workdir_is_in_the_schema_and_documents_that_cd_does_not_persist() {
        let schema = test_tool().parameters();
        let text = schema["properties"]["workdir"]["description"]
            .as_str()
            .unwrap();
        assert!(text.contains("does NOT persist"), "{text}");
    }

    #[test]
    fn pty_parameters_are_offered_only_where_a_pty_exists() {
        let shell = test_tool().parameters();
        let status = JobStatusTool::new(test_tool().sandbox, None).parameters();
        assert_eq!(shell["properties"].get("pty").is_some(), PTY_SUPPORTED);
        assert_eq!(status["properties"].get("keys").is_some(), PTY_SUPPORTED);
        // A platform without a pty never offers pty, input or keys.
        let shell = without_unsupported_pty(test_tool().parameters(), false);
        assert!(shell["properties"].get("pty").is_none());
        assert!(shell["properties"].get("command").is_some());
        let status = without_unsupported_pty(
            JobStatusTool::new(test_tool().sandbox, None).parameters(),
            false,
        );
        for name in ["input", "keys"] {
            assert!(status["properties"].get(name).is_none(), "{name}");
        }
        let actions = status["properties"]["action"]["enum"].as_array().unwrap();
        assert!(
            !actions.iter().any(|action| action == "input"),
            "{actions:?}"
        );
        assert!(status["properties"].get("timeout_ms").is_some());
    }

    #[test]
    fn platforms_without_a_pty_tell_the_model_why_and_what_to_do() {
        let base = "Execute a command.".to_string();
        assert_eq!(with_pty_limitation_note(base.clone(), true), base);
        let note = with_pty_limitation_note(base.clone(), false);
        assert!(note.starts_with(&base));
        assert!(note.contains("no pseudo-terminal") && note.contains("non-interactive flags"));
        // The unsupported schema also drops the parameters the note replaces.
        let schema = without_unsupported_pty(
            serde_json::json!({"properties": {"pty": {}, "command": {}}}),
            false,
        );
        assert!(schema["properties"].get("pty").is_none());
    }

    #[test]
    fn extended_job_keys_have_fixed_bytes_and_a_matching_schema_enum() {
        for (name, bytes) in [
            ("page_up", &b"\x1b[5~"[..]),
            ("page_down", b"\x1b[6~"),
            ("home", b"\x1b[H"),
            ("end", b"\x1b[F"),
            ("delete", b"\x1b[3~"),
            ("shift_tab", b"\x1b[Z"),
            ("ctrl_l", b"\x0c"),
            ("ctrl_z", b"\x1a"),
            ("f1", b"\x1bOP"),
            ("f4", b"\x1bOS"),
            ("f5", b"\x1b[15~"),
            ("f10", b"\x1b[21~"),
            ("f12", b"\x1b[24~"),
        ] {
            let key: JobKey = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(key.bytes(), bytes, "{name}");
        }
        // Every advertised name parses, renders printable-free bytes, and none repeats.
        let mut seen = std::collections::HashSet::new();
        for name in JobKey::NAMES {
            let key: JobKey = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert!(
                !key.bytes().is_empty() && seen.insert(key.bytes()),
                "{name}"
            );
        }
        assert_eq!(JobKey::NAMES.len(), 30);
        assert!(
            JobKey::Home.edits_line() && JobKey::F5.edits_line() && JobKey::ShiftTab.edits_line()
        );
        assert!(!JobKey::CtrlL.edits_line() && !JobKey::Enter.edits_line());
        assert!(serde_json::from_value::<JobKey>(serde_json::json!("f13")).is_err());
    }

    /// The new keys reach a real pty as the exact escape sequences, and keys
    /// that rewrite the line make the next Enter an opaque, judged line.
    #[cfg(unix)]
    #[tokio::test]
    async fn extended_job_keys_reach_a_pty_and_mark_the_line_edited() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let id = start_pty_job(&shell, "cat -v").await;
        let mut seen = String::new();
        for key in [
            JobKey::PageUp,
            JobKey::PageDown,
            JobKey::Home,
            JobKey::End,
            JobKey::Delete,
            JobKey::ShiftTab,
            JobKey::F1,
            JobKey::F12,
            JobKey::CtrlL,
        ] {
            seen.push_str(&job_keys(&status, &id, &[key]).await.unwrap());
        }
        seen.push_str(&job_keys(&status, &id, &[JobKey::Enter]).await.unwrap());
        for echoed in [
            "^[[5~", "^[[6~", "^[[H", "^[[F", "^[[3~", "^[[Z", "^[OP", "^[[24~", "^L",
        ] {
            assert!(seen.contains(echoed), "{echoed} missing from {seen:?}");
        }
        job_keys(&status, &id, &[JobKey::CtrlC]).await.unwrap();
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    #[test]
    fn job_keys_render_fixed_bytes_and_reject_text_mixing() {
        assert_eq!(JobKey::CtrlC.bytes(), b"\x03");
        assert_eq!(JobKey::CtrlD.bytes(), b"\x04");
        assert_eq!(JobKey::Up.bytes(), b"\x1b[A");
        let args: JobStatusArgs = serde_json::from_value(serde_json::json!({
            "id": "job-1", "action": "input", "keys": ["ctrl_c", "enter"]
        }))
        .unwrap();
        assert_eq!(args.keys, Some(vec![JobKey::CtrlC, JobKey::Enter]));
        assert!(
            serde_json::from_value::<JobStatusArgs>(serde_json::json!({
                "action": "input", "keys": ["\u{3}"]
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn job_keys_validation_errors_come_before_any_job_lookup() {
        let status = JobStatusTool::new(test_tool().sandbox, None);
        let both = status
            .call(JobStatusArgs {
                id: Some("job-x".into()),
                action: JobAction::Input,
                input: Some("a".into()),
                keys: Some(vec![JobKey::Enter]),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(both.to_string().contains("not both"), "{both}");
        let wrong_action = status
            .call(JobStatusArgs {
                id: Some("job-x".into()),
                keys: Some(vec![JobKey::Enter]),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(
            wrong_action.to_string().contains("only valid"),
            "{wrong_action}"
        );
        let too_many = job_keys(&status, "job-x", &[JobKey::Left; 65])
            .await
            .unwrap_err();
        assert!(too_many.to_string().contains("1 to 64"), "{too_many}");
    }

    /// Ctrl-D ends a python REPL (EOF) and Ctrl-C interrupts a foreground
    /// `sleep` in a pty shell without killing the shell.
    #[cfg(unix)]
    #[tokio::test]
    async fn job_keys_send_eof_to_a_repl_and_interrupt_to_a_pty_shell() {
        let shell = test_tool();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        let id = start_pty_job(&shell, "python3 -i -q").await;
        let mut seen = job_input(&status, &id, "print(6*7)\n").await.unwrap();
        // Python's startup can outlast the write: poll for the answer, which
        // the echo of the typed line does not contain.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !seen.contains("42") {
            assert!(Instant::now() < deadline, "REPL never answered: {seen}");
            tokio::time::sleep(Duration::from_millis(20)).await;
            seen.push_str(&poll_job(&status, &id, None).await);
        }
        job_keys(&status, &id, &[JobKey::CtrlD]).await.unwrap();
        let terminal = wait_for_terminal_job(&status, &id).await;
        assert!(terminal.contains("Status: completed"), "{terminal}");
        assert!(terminal.contains("Exit code: 0"), "{terminal}");

        let id = start_pty_job(&shell, "sh").await;
        job_input(&status, &id, "sleep 300\n").await.unwrap();
        let started = Instant::now();
        job_keys(&status, &id, &[JobKey::CtrlC]).await.unwrap();
        let mut seen = job_input(&status, &id, "echo after-$((20+1))\n")
            .await
            .unwrap();
        while !seen.contains("after-21") {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "shell never answered after the interrupt: {seen}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            seen.push_str(&poll_job(&status, &id, None).await);
        }
        assert!(started.elapsed() < Duration::from_secs(20));
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    /// Keys are judged per policy: Enter judges the typed line, Tab on typed
    /// text is refused, and history keys make the next Enter an opaque line
    /// that a deny-by-default policy refuses. Ctrl-C needs no permission.
    #[cfg(unix)]
    #[tokio::test]
    async fn job_keys_are_permission_judged_and_cannot_rewrite_a_denied_line() {
        use crate::permission::{Action, PermissionConfig, ToolPerm};
        let dir = std::env::current_dir().unwrap();
        let typed = dir.join(format!("job-keys-typed-{}", std::process::id()));
        let _ = std::fs::remove_file(&typed);
        let shell = test_tool();
        let id = start_pty_job(&shell, "sh").await;
        let permission = shell_permission(
            PermissionConfig {
                bash: Some(ToolPerm::Granular(
                    [
                        ("*".to_string(), Action::Allow),
                        ("touch *".to_string(), Action::Deny),
                        ("terminal input line edited*".to_string(), Action::Deny),
                    ]
                    .into(),
                )),
                ..Default::default()
            },
            crate::permission::SecurityMode::Yolo,
        );
        let status =
            JobStatusTool::new(shell.sandbox.clone(), None).with_permission(Some(permission), None);
        // ctrl_c is always allowed even though the default rule denies.
        job_keys(&status, &id, &[JobKey::CtrlC]).await.unwrap();
        // Typed text then Enter is judged as the typed line.
        let name = typed.file_name().unwrap().to_string_lossy().to_string();
        job_input(&status, &id, &format!("touch {name}"))
            .await
            .unwrap_err();
        job_input(&status, &id, "echo hi").await.unwrap();
        let tab = job_keys(&status, &id, &[JobKey::Tab]).await.unwrap_err();
        assert!(
            tab.to_string().contains("tab on a partly typed line"),
            "{tab}"
        );
        // History recall then Enter would run an unjudged line: refused.
        job_keys(&status, &id, &[JobKey::CtrlC, JobKey::Up])
            .await
            .unwrap();
        let error = job_keys(&status, &id, &[JobKey::Enter]).await.unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("denied"),
            "{error}"
        );
        job_keys(&status, &id, &[JobKey::CtrlC]).await.unwrap();
        job_input(&status, &id, "echo fine\n").await.unwrap();
        assert!(!typed.exists());
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    #[test]
    fn history_expansion_triggers_are_detected() {
        assert!(history_expansion_trigger("^ls^touch f\n").is_some());
        assert!(history_expansion_trigger("ls\n^a^b\n").is_some());
        assert!(history_expansion_trigger("echo !!\n").is_some());
        assert!(history_expansion_trigger("echo !$").is_some());
        assert!(history_expansion_trigger("x != 1\nnot ! x\nwow!\n").is_none());
        assert!(history_expansion_trigger("a ^ b\n").is_none());
    }

    /// An interactive bash rewrites the line (TAB completion, history
    /// expansion) before running it, so the judged text must be refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn job_input_cannot_bypass_deny_rules_through_bash_completion_or_history() {
        use crate::permission::{Action, PermissionConfig, ToolPerm};
        if !std::path::Path::new("/bin/bash").exists() {
            return;
        }
        let dir = std::env::current_dir().unwrap();
        let tab = dir.join(format!("job-input-tab-{}", std::process::id()));
        let hist = dir.join(format!("job-input-hist-{}", std::process::id()));
        let _ = std::fs::remove_file(&tab);
        let _ = std::fs::remove_file(&hist);
        let shell = test_tool();
        let id = start_pty_job(&shell, "/bin/bash --norc --noprofile -i").await;
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
        let name = |path: &std::path::Path| path.file_name().unwrap().to_string_lossy().to_string();
        let error = job_input(&status, &id, &format!("touc\t {}\n", name(&tab)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("control character"), "{error}");
        job_input(&status, &id, "ls\n").await.unwrap();
        let error = job_input(&status, &id, &format!("^ls^touch {}\n", name(&hist)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("history"), "{error}");
        let error = job_input(&status, &id, "!!\n").await.unwrap_err();
        assert!(error.to_string().contains("history"), "{error}");
        job_input(&status, &id, "echo alive\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!tab.exists() && !hist.exists(), "rewritten input ran");
        shell.sandbox.stop_background_job(&id).await.unwrap();
    }

    #[test]
    fn disallowed_input_control_allows_only_lf_cr() {
        assert_eq!(disallowed_input_control("ab\r\nc é"), None);
        assert_eq!(disallowed_input_control("a\tb"), Some('\t'));
        assert_eq!(disallowed_input_control("a\u{15}"), Some('\u{15}'));
        assert_eq!(disallowed_input_control("\u{7f}"), Some('\u{7f}'));
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
    type EscalationPrompts = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

    /// Answers every escalation prompt with `decision`, recording the prompts.
    #[cfg(unix)]
    fn escalation_responder(
        decision: crate::permission::ask::UserDecision,
    ) -> (crate::permission::ask::AskSender, EscalationPrompts) {
        use crate::permission::ask::UserRequest;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UserRequest>(4);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                if let UserRequest::Permission(ask) = request {
                    recorded
                        .lock()
                        .unwrap()
                        .push((ask.tool.to_string(), ask.input.clone()));
                    let _ = ask.reply.send(decision.clone());
                }
            }
        });
        (tx, seen)
    }

    /// mini-agent-a1ive: escalation is default closed, always needs a fresh
    /// interactive approval, honours deny rules and runs only that command
    /// unsandboxed. the command is only asserted to run after approval.
    #[cfg(unix)]
    #[tokio::test]
    async fn escalation_is_default_closed_and_needs_approval_every_time() {
        use crate::permission::ask::UserDecision;
        use crate::permission::{Action, PermissionConfig, SecurityMode, ToolPerm};
        let dir = std::env::current_dir().unwrap();
        let marker = dir.join(format!("escalation-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let command = format!("touch {}", marker.file_name().unwrap().to_string_lossy());
        async fn call(
            tool: &ShellTool,
            command: &str,
            escalate: bool,
            justification: Option<&str>,
        ) -> Result<String, ToolError> {
            tool.call(BashArgs {
                command: command.to_string(),
                escalate,
                justification: justification.map(str::to_string),
                ..Default::default()
            })
            .await
        }
        let sandboxed = || Sandbox::new(true, "bwrap");

        // Default closed: not in the schema, refused when sent anyway.
        let closed = ShellTool::new(None, None, sandboxed(), None);
        assert!(closed.parameters()["properties"].get("escalate").is_none());
        assert!(!closed.description().contains("escalate"));
        let error = call(&closed, &command, true, Some("why"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("disabled"), "{error}");
        assert!(!marker.exists());

        // Offered when enabled; nobody to ask means refusal, not a silent run.
        let open = || ShellTool::new(None, None, sandboxed(), None).with_escalation(true);
        assert!(open().parameters()["properties"].get("escalate").is_some());
        let error = call(&open(), &command, true, Some("needs it"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("interactive approval"), "{error}");
        let error = call(&open(), &command, true, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("justification"), "{error}");
        assert!(!marker.exists());

        // The user says no.
        let (deny_tx, denied) = escalation_responder(UserDecision::Deny);
        let tool = ShellTool::new(None, Some(deny_tx), sandboxed(), None).with_escalation(true);
        let error = call(&tool, &command, true, Some("install deps"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("denied by user"), "{error}");
        assert!(!marker.exists());
        assert_eq!(denied.lock().unwrap()[0].0, "sandbox_escalation");
        assert!(denied.lock().unwrap()[0].1.contains("install deps"));

        // Allow-all permission rules do not waive the prompt, and an
        // always-grant is not remembered: both escalations prompt.
        let permission = shell_permission(
            PermissionConfig {
                bash: Some(ToolPerm::Simple(Action::Allow)),
                ..Default::default()
            },
            SecurityMode::Yolo,
        );
        let (allow_tx, asked) = escalation_responder(UserDecision::AllowAlways("*".into()));
        let tool = ShellTool::new(Some(permission), Some(allow_tx), sandboxed(), None)
            .with_escalation(true);
        let output = call(&tool, &command, true, Some("write outside root"))
            .await
            .unwrap();
        assert!(output.contains("sandbox escalated"), "{output}");
        assert!(marker.exists(), "approved command did not run");
        std::fs::remove_file(&marker).unwrap();
        call(&tool, &command, true, Some("again")).await.unwrap();
        assert_eq!(
            asked.lock().unwrap().len(),
            2,
            "always-grant must not stick"
        );
        std::fs::remove_file(&marker).unwrap();
        // Deny rules win before any prompt.
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
            SecurityMode::Yolo,
        );
        let (allow_tx, asked) = escalation_responder(UserDecision::AllowOnce);
        let tool = ShellTool::new(Some(permission), Some(allow_tx), sandboxed(), None)
            .with_escalation(true);
        let error = call(&tool, &command, true, Some("x"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.to_lowercase().contains("denied"), "{error}");
        assert!(asked.lock().unwrap().is_empty() && !marker.exists());

        // An unsandboxed session has nothing to escalate from.
        let off =
            ShellTool::new(None, None, Sandbox::new(false, "bwrap"), None).with_escalation(true);
        let error = call(&off, &command, true, Some("x"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already off"), "{error}");
    }

    /// mini-agent-tb3zw: the cap is configurable and a full job table runs an
    /// auto_background call in the foreground instead of failing it.
    #[cfg(unix)]
    #[tokio::test]
    async fn auto_background_runs_in_the_foreground_when_the_job_table_is_full() {
        let shell = BashTool::new(
            None,
            None,
            Sandbox::new(false, "bwrap").with_max_background_jobs(2),
            None,
        )
        .with_auto_background(true);
        assert_eq!(shell.sandbox.max_background_jobs(), 2);
        for _ in 0..2 {
            shell
                .call(BashArgs {
                    command: "while :; do sleep 1; done".to_string(),
                    background: true,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let explicit = shell
            .call(BashArgs {
                command: "echo nope".to_string(),
                background: true,
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(explicit.contains("maximum 2 running jobs"), "{explicit}");
        let output = shell
            .call(BashArgs {
                command: "echo short-foreground".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(output.contains("short-foreground"), "{output}");
        assert!(!output.contains("promoted_to_background"), "{output}");
        assert_eq!(shell.sandbox.running_background_job_count(), 2);
        shell.sandbox.cancel_background_jobs();
        assert_eq!(
            Sandbox::new(false, "bwrap")
                .with_max_background_jobs(1000)
                .max_background_jobs(),
            crate::sandbox::MAX_BACKGROUND_JOBS_LIMIT
        );
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
            "background job capacity is full (maximum 8 running jobs; raise shell-max-background-jobs, at most 64)"
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
