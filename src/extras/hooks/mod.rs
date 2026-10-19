pub(crate) mod channel;
pub(crate) mod decorator;
pub(crate) mod dispatcher;
pub(crate) mod envelope;
pub(crate) mod normalize;
pub(crate) mod pins;
pub(crate) mod settings;
pub(crate) mod subprocess;
pub(crate) mod trust;

tokio::task_local! {
    // Host-only scope: formatting repairs perform no executable lifecycle work.
    static FORMAT_REPAIR: ();
}

pub(crate) async fn format_repair_scope<F: std::future::Future>(future: F) -> F::Output {
    FORMAT_REPAIR.scope((), future).await
}

/// Outcome of a non-tool-permission hook dispatch: lifecycle events (`Stop`,
/// `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `SubagentStart`,
/// `SubagentStop`) and constrained `PostToolUse` redaction. Carries no rig
/// type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No hook expressed an opinion (or none matched); proceed unchanged.
    Continue,
    /// Block the action; `reason` is surfaced as user feedback or as the next
    /// instruction to the model.
    Block { reason: String },
    /// Replace model-visible content through an event-specific safe mechanism.
    Rewrite { content: String },
    /// Bounded context from a configured lifecycle hook; never a grant.
    AdditionalContext { notices: Vec<String> },
}

/// Permission decision vocabulary for blockable tool events. Variants are
/// declared least to most severe so `Ord`/`max` picks the most severe of
/// several hook results, per the deterministic-merge requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Allow,
    Defer,
    Ask,
    Deny,
}

/// Result of dispatching `PreToolUse`: the merged permission verdict plus any
/// input rewrite requested by a matching hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreDecision {
    pub verdict: Verdict,
    pub reason: Option<String>,
    pub updated_input: Option<serde_json::Value>,
}

/// Context common to every hook dispatch, used to assemble the stdin
/// envelope. `session_path` intentionally replaces Claude Code's
/// `transcript_path`. Carries no rig type.
#[derive(Debug, Clone)]
pub struct HookCtx {
    pub session_id: String,
    pub session_path: String,
    pub cwd: String,
    pub permission_mode: String,
}

/// `--loop` iteration/active state, threaded through the runner so the
/// `Stop` hook envelope can report `loop_iteration`/`loop_active`. `None`
/// outside loop mode (plain `-p` one-shot runs, or interactive sessions).
#[derive(Clone, Copy, Debug)]
pub struct LoopInfo {
    /// 1-based `--loop` iteration count.
    pub iteration: u32,
    /// Whether the loop will continue after this iteration.
    pub active: bool,
}

static PROCESS_SESSION_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static ACTIVE_SESSION_CONTEXT: std::sync::Mutex<Option<(String, String)>> =
    std::sync::Mutex::new(None);
static ACTIVE_WORKSPACE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
static ACTIVE_PERMISSION_MODE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn set_active_permission_mode(mode: crate::permission::SecurityMode) {
    *ACTIVE_PERMISSION_MODE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(mode.to_string());
}

/// Fixture-only ownership of the process selection, separate from a turn's
/// task-local execution root. Drop runs before its temporary workspace dies.
#[cfg(test)]
pub(crate) struct TestWorkspaceSelection(Option<std::path::PathBuf>);
#[cfg(test)]
pub(crate) fn retain_test_workspace_selection() -> TestWorkspaceSelection {
    TestWorkspaceSelection(
        ACTIVE_WORKSPACE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    )
}
#[cfg(test)]
impl Drop for TestWorkspaceSelection {
    fn drop(&mut self) {
        *ACTIVE_WORKSPACE.lock().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub(crate) fn set_active_workspace(path: &std::path::Path) {
    let selected = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    *ACTIVE_WORKSPACE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(selected);
    if let Some(dispatcher) = get_dispatcher()
        && let Err(error) = dispatcher.rebind_execution_root(path)
    {
        // The dispatcher records an invalid execution state and will refuse
        // to launch hooks until a valid workspace is selected.
        tracing::error!(error = %error, "hooks: failed to rebind execution workspace");
    }
}

/// Sets the process-wide hook execution root used by dispatches that run
/// outside a turn bound to its own root: `Some(root)` rebinds it, `None`
/// disables it (fail-closed) because concurrent turns in different
/// workspaces leave no single correct shared root.
#[cfg(feature = "acp")]
pub(crate) fn bind_shared_execution_root(root: Option<&std::path::Path>) {
    match root {
        Some(root) => set_active_workspace(root),
        None => {
            if let Some(dispatcher) = get_dispatcher() {
                dispatcher.invalidate_shared_execution_root(
                    "hooks: turns in several workspaces are active; hook dispatch outside a \
                     workspace-bound turn is refused",
                );
            }
        }
    }
}

/// The workspace hook contexts report: the current turn's own hook root when
/// it has one (see `AgentWorkScope::set_hook_execution_root`), else the
/// process-wide selection.
pub(crate) fn active_workspace() -> std::path::PathBuf {
    if let Some(root) = crate::agent::runner::current_hook_execution_root() {
        return root;
    }
    ACTIVE_WORKSPACE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

pub(crate) fn set_active_session(session_id: &str) {
    let session_path = crate::session::storage::session_path(session_id)
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    *ACTIVE_SESSION_CONTEXT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some((session_id.to_string(), session_path));
}

/// Best-effort session identity for `HookCtx` until the session-lifecycle
/// seam supplies zerostack's real session id/path.
pub(crate) fn session_context() -> (String, String) {
    if let Some(context) = ACTIVE_SESSION_CONTEXT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return context;
    }
    let session_id = PROCESS_SESSION_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone();
    (session_id, String::new())
}

/// Runs a brokered host effect through the same process-wide PreToolUse guard
/// rail as ordinary rig tools. `Ask` fails closed because the broker's normal
/// permission check remains the only owner of interactive approval state.
#[cfg(feature = "js")]
pub(crate) async fn gate_brokered_pre_tool_use(
    tool_name: &str,
    input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let Some(dispatcher) = get_dispatcher() else {
        return Ok(input);
    };
    gate_brokered_pre_tool_use_with(&dispatcher, &best_effort_ctx(), tool_name, input).await
}

#[cfg(any(feature = "js", test))]
pub(crate) async fn gate_brokered_pre_tool_use_with(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    tool_name: &str,
    input: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let decision = dispatcher
        .dispatch_pre_tool_use(ctx, tool_name, input.clone())
        .await;
    match decision.verdict {
        Verdict::Deny => Err(decision
            .reason
            .unwrap_or_else(|| "denied by hook".to_string())),
        Verdict::Ask => Err(decision
            .reason
            .unwrap_or_else(|| "hook requires explicit approval".to_string())),
        Verdict::Allow | Verdict::Defer => Ok(decision.updated_input.unwrap_or(input)),
    }
}

static DISPATCHER: std::sync::Mutex<Option<std::sync::Arc<dispatcher::HookDispatcher>>> =
    std::sync::Mutex::new(None);

/// Installs the process-wide `HookDispatcher`, built once from loaded and
/// trust-filtered config. Tool builders (`agent::builder`,
/// `extras::subagents::builder`) read it via [`get_dispatcher`].
pub(crate) fn init_dispatcher(dispatcher: dispatcher::HookDispatcher) {
    *DISPATCHER.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(dispatcher));
}

pub(crate) fn get_dispatcher() -> Option<std::sync::Arc<dispatcher::HookDispatcher>> {
    #[cfg(test)]
    if crate::tests::fake_model::dispatcher_guard::hidden_from_current_thread() {
        return None;
    }
    DISPATCHER.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Nonblocking metadata from the existing dispatcher, without constructing,
/// dispatching or revalidating executable hooks. None means not initialized.
pub(crate) fn diagnostic_hook_metadata()
-> Result<Option<dispatcher::HookDiagnosticMetadata>, &'static str> {
    let dispatcher = DISPATCHER
        .try_lock()
        .map_err(|_| "dispatcher metadata busy or unavailable")?;
    Ok(dispatcher
        .as_ref()
        .map(|dispatcher| dispatcher.diagnostic_metadata()))
}

// File tools refuse to modify the hook files the installed dispatcher has
// content-bound (see `is_pinned_by_installed_dispatcher`): a rewrite would only
// make the hook fail closed until restart.

/// True when `relative` (a workspace-relative spelling) names one of `pinned`.
/// Compared case-insensitively with `/` and `\` as equal separators, so a
/// case-insensitive volume cannot be used to spell around the protection.
pub(crate) fn is_pinned_hook_file(pinned: &[std::path::PathBuf], relative: &str) -> bool {
    let relative = normalize_pinned_spelling(relative.trim_start_matches("./"));
    pinned
        .iter()
        .any(|path| normalize_pinned_spelling(&path.to_string_lossy()) == relative)
}

/// The comparison spelling [`is_pinned_hook_file`] uses for both sides.
pub(crate) fn normalize_pinned_spelling(text: &str) -> String {
    text.replace('\\', "/").to_lowercase()
}

/// [`is_pinned_hook_file`] against the installed dispatcher's pins, which are
/// normalized once at load: no pinned-path list is cloned or re-lowercased.
pub(crate) fn is_pinned_by_installed_dispatcher(relative: &str) -> bool {
    let relative = normalize_pinned_spelling(relative.trim_start_matches("./"));
    get_dispatcher().is_some_and(|dispatcher| dispatcher.is_pinned_normalized(&relative))
}

/// Test-only: clears the process-global dispatcher so a test that installed one
/// via [`init_dispatcher`] can't leak it into other tests sharing the binary.
#[cfg(test)]
pub(crate) fn reset_dispatcher() {
    *DISPATCHER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Wraps `tools` with the process-global dispatcher's guard rail, or returns
/// them unchanged when no hooks are configured. The single weave point shared
/// by the main-agent and subagent builders (design D5).
pub(crate) fn wrap_from_global(
    tools: Vec<Box<dyn rig::tool::ToolDyn>>,
    permission: Option<crate::permission::checker::PermCheck>,
    ask_tx: Option<crate::permission::ask::AskSender>,
) -> Vec<Box<dyn rig::tool::ToolDyn>> {
    match get_dispatcher() {
        Some(dispatcher) => decorator::wrap_all(tools, dispatcher, permission, ask_tx),
        None => tools,
    }
}

/// Builder-owned MCP provenance, captured while concrete `McpTool` fields
/// remain available. Other tools retain the legacy wrapper API and envelopes.
#[cfg(feature = "mcp")]
pub(crate) fn wrap_from_global_with_mcp(
    tools: Vec<Box<dyn rig::tool::ToolDyn>>,
    permission: Option<crate::permission::checker::PermCheck>,
    ask_tx: Option<crate::permission::ask::AskSender>,
    mcp: std::collections::HashMap<String, normalize::McpToolIdentity>,
) -> Vec<Box<dyn rig::tool::ToolDyn>> {
    if mcp.is_empty() {
        return wrap_from_global(tools, permission, ask_tx);
    }
    match get_dispatcher() {
        Some(dispatcher) => {
            decorator::wrap_all_with_mcp(tools, dispatcher, permission, ask_tx, mcp)
        }
        None => tools,
    }
}

/// Outcome of gating a user prompt through `UserPromptSubmit` hooks.
pub(crate) enum PromptGate {
    /// The original prompt should be sent to the model.
    Proceed(String),
    /// The literal prompt is unchanged; context has its own canonical row.
    WithContext {
        prompt: String,
        notices: Vec<String>,
    },
    /// A hook blocked the prompt; `String` is feedback to show the user
    /// instead of sending anything to the model.
    Blocked(String),
}

/// Who submitted a prompt, as the `UserPromptSubmit` envelope's `origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptOrigin {
    /// Typed (or queued, or steered) by the human.
    User,
    /// Fired by the session `/cron` scheduler.
    Scheduled,
}

impl PromptOrigin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Scheduled => "scheduled",
        }
    }

    /// The origin of the run whose spend profile is in scope: a scheduled main
    /// turn carries [`crate::spend::RequestKind::Scheduled`].
    pub(crate) fn of_current_run() -> Self {
        match crate::spend::run::Run::current().map(|run| run.kind()) {
            Some(crate::spend::RequestKind::Scheduled) => Self::Scheduled,
            _ => Self::User,
        }
    }
}

/// Testable core: dispatches `UserPromptSubmit` against an explicit
/// dispatcher/context and decides whether the prompt proceeds.
#[cfg(test)]
pub(crate) async fn gate_user_prompt(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    prompt: String,
) -> PromptGate {
    gate_user_prompt_from(dispatcher, ctx, prompt, PromptOrigin::User).await
}

/// [`gate_user_prompt`] for an explicit prompt origin.
pub(crate) async fn gate_user_prompt_from(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    prompt: String,
    origin: PromptOrigin,
) -> PromptGate {
    if FORMAT_REPAIR.try_with(|_| ()).is_ok() {
        return PromptGate::Proceed(prompt);
    }
    let decision = crate::startup::heap_future(|| {
        dispatcher.dispatch(
            "UserPromptSubmit",
            None,
            ctx,
            envelope::EventFields::UserPromptSubmit {
                prompt: prompt.clone(),
                origin: origin.as_str(),
            },
        )
    })
    .await;
    match decision {
        Decision::Continue => PromptGate::Proceed(prompt),
        Decision::Block { reason } => PromptGate::Blocked(reason),
        Decision::Rewrite { content } => PromptGate::Proceed(format!("{content}\n\n{prompt}")),
        Decision::AdditionalContext { notices } => PromptGate::WithContext { prompt, notices },
    }
}

/// Builds a `HookCtx` from process/session state for lifecycle seams that do
/// not directly own the `PermissionChecker`. The checker updates the shared
/// mode whenever startup, `/mode`, or a prompt directive changes it.
pub(crate) fn best_effort_ctx() -> HookCtx {
    let (session_id, session_path) = session_context();
    let cwd = active_workspace().display().to_string();
    HookCtx {
        session_id,
        session_path,
        cwd,
        permission_mode: ACTIVE_PERMISSION_MODE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| "standard".to_string()),
    }
}

/// Production entry point: reads the process-wide dispatcher and gates
/// `prompt` through `UserPromptSubmit` hooks. A prompt passes through
/// unchanged when no dispatcher is installed.
pub(crate) async fn dispatch_user_prompt_submit(prompt: String) -> PromptGate {
    dispatch_user_prompt_submit_from(prompt, PromptOrigin::User).await
}

/// [`dispatch_user_prompt_submit`] for an explicit prompt origin.
pub(crate) async fn dispatch_user_prompt_submit_from(
    prompt: String,
    origin: PromptOrigin,
) -> PromptGate {
    let Some(dispatcher) = get_dispatcher() else {
        return PromptGate::Proceed(prompt);
    };
    gate_user_prompt_from(&dispatcher, &best_effort_ctx(), prompt, origin).await
}

/// The active goal, as a `Stop` hook sees it.
#[derive(Debug, Clone)]
pub struct GoalHookInfo {
    pub id: String,
    pub status: String,
    pub round: u64,
}

/// Outcome of dispatching `Stop`: either release (send the final response) or
/// force continuation with the hook's reason as the next instruction.
pub(crate) enum StopGate {
    Release,
    Continue { reason: String },
}

/// Testable core: dispatches `Stop` against an explicit dispatcher/context.
pub(crate) async fn gate_stop(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    stop_hook_active: bool,
    loop_iteration: Option<u64>,
    loop_active: Option<bool>,
    goal: Option<GoalHookInfo>,
) -> StopGate {
    if FORMAT_REPAIR.try_with(|_| ()).is_ok() {
        return StopGate::Release;
    }
    let decision = dispatcher
        .dispatch(
            "Stop",
            None,
            ctx,
            envelope::EventFields::Stop {
                stop_hook_active,
                loop_iteration,
                loop_active,
                goal_id: goal.as_ref().map(|g| g.id.clone()),
                goal_status: goal.as_ref().map(|g| g.status.clone()),
                goal_round: goal.as_ref().map(|g| g.round),
            },
        )
        .await;
    match decision {
        Decision::Block { reason } => StopGate::Continue { reason },
        Decision::Continue | Decision::Rewrite { .. } | Decision::AdditionalContext { .. } => {
            StopGate::Release
        }
    }
}

/// Production entry point: reads the process-wide dispatcher and gates
/// completion through `Stop` hooks. Releases immediately when no dispatcher
/// is installed.
pub(crate) async fn dispatch_stop(
    stop_hook_active: bool,
    loop_iteration: Option<u64>,
    loop_active: Option<bool>,
    goal: Option<GoalHookInfo>,
) -> StopGate {
    let Some(dispatcher) = get_dispatcher() else {
        return StopGate::Release;
    };
    gate_stop(
        &dispatcher,
        &best_effort_ctx(),
        stop_hook_active,
        loop_iteration,
        loop_active,
        goal,
    )
    .await
}

/// Testable core: dispatches `SessionStart`. Informational only (not
/// blockable per the hook-lifecycle-events spec), so the decision is ignored.
/// Exact runtime output of the owned SessionStart dispatch. This carrier has
/// no deserializer or cloning API: stored prose cannot impersonate a new hook
/// delivery when a native controller restores the old feed before adoption.
pub(crate) struct OwnedSessionStartContext {
    notices: Vec<String>,
}

impl OwnedSessionStartContext {
    pub(crate) fn into_notices(self) -> Vec<String> {
        self.notices
    }

    #[cfg(test)]
    pub(crate) fn for_test(notices: Vec<String>) -> Self {
        Self { notices }
    }
}

pub(crate) async fn dispatch_session_start_with(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    source: &str,
) -> OwnedSessionStartContext {
    let decision = dispatcher
        .dispatch(
            "SessionStart",
            None,
            ctx,
            envelope::EventFields::SessionStart {
                source: source.to_string(),
            },
        )
        .await;
    OwnedSessionStartContext {
        notices: match decision {
            Decision::AdditionalContext { notices } => notices,
            _ => Vec::new(), // SessionStart remains informational, never blockable.
        },
    }
}

/// Production entry point: reads the process-wide dispatcher and dispatches
/// `SessionStart` with `source` (`startup`, `resume`, `clear`, or `compact`).
/// No-op when no dispatcher is installed.
pub(crate) async fn dispatch_session_start(source: &str) -> OwnedSessionStartContext {
    let Some(dispatcher) = get_dispatcher() else {
        return OwnedSessionStartContext {
            notices: Vec::new(),
        };
    };
    dispatch_session_start_with(&dispatcher, &best_effort_ctx(), source).await
}

/// Use the same append-only representation as existing canonical System rows.
/// Provider-specific converters must not lift these notices into a retained
/// signed top-level system prefix. Imported notices remain transcript data.
pub(crate) fn append_context_history(
    history: std::sync::Arc<[rig::completion::Message]>,
    notices: &[String],
) -> std::sync::Arc<[rig::completion::Message]> {
    crate::agent::input::append_context_history(history, notices)
}

pub(crate) fn append_context_session(session: &mut crate::session::Session, notices: &[String]) {
    for notice in notices {
        session.add_message(crate::session::MessageRole::System, notice);
    }
}

/// Observational events share the current tool/compaction work owner. They
/// cannot answer an approval or replace a tool's execution/result contract.
pub(crate) async fn dispatch_pre_compact(auto: bool, instructions: Option<&str>) {
    let Some(dispatcher) = get_dispatcher() else {
        return;
    };
    if FORMAT_REPAIR.try_with(|_| ()).is_ok() {
        return;
    }
    let trigger = if auto { "auto" } else { "manual" };
    let ctx = best_effort_ctx();
    let _ = crate::startup::heap_future(|| {
        dispatcher.dispatch(
            "PreCompact",
            Some(trigger),
            &ctx,
            envelope::EventFields::PreCompact {
                trigger: trigger.into(),
                custom_instructions: instructions.map(str::to_owned),
            },
        )
    })
    .await;
}

pub(crate) async fn dispatch_permission_request(tool: &str, input: &str) {
    let Some(dispatcher) = get_dispatcher() else {
        return;
    };
    if FORMAT_REPAIR.try_with(|_| ()).is_ok() {
        return;
    }
    let ctx = best_effort_ctx();
    let canonical = normalize::canonical_tool_name(tool);
    let _ = crate::startup::heap_future(|| {
        dispatcher.dispatch(
            "PermissionRequest",
            Some(&canonical),
            &ctx,
            envelope::EventFields::PermissionRequest {
                tool_name: canonical.clone(),
                tool_input: serde_json::from_str(input)
                    .unwrap_or_else(|_| serde_json::Value::String(input.into())),
            },
        )
    })
    .await;
    let _ = crate::startup::heap_future(|| {
        dispatcher.dispatch(
            "Notification",
            Some("permission_prompt"),
            &ctx,
            envelope::EventFields::Notification {
                message: format!("Permission requested for {canonical}"),
                notification_type: "permission_prompt".into(),
            },
        )
    })
    .await;
}

/// Testable core: dispatches `SessionEnd`. Informational only (not
/// blockable per the hook-lifecycle-events spec), so the decision is ignored.
pub(crate) async fn dispatch_session_end_with(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    reason: &str,
) {
    let _ = dispatcher
        .dispatch(
            "SessionEnd",
            None,
            ctx,
            envelope::EventFields::SessionEnd {
                reason: reason.to_string(),
            },
        )
        .await;
}

/// Production entry point: reads the process-wide dispatcher and dispatches
/// `SessionEnd` with `reason`. No-op when no dispatcher is installed.
pub(crate) async fn dispatch_session_end(reason: &str) {
    let Some(dispatcher) = get_dispatcher() else {
        return;
    };
    dispatch_session_end_with(&dispatcher, &best_effort_ctx(), reason).await;
}

/// Testable core: dispatches `SubagentStart`, matching on the normalized agent
/// type. Returns hook-injected context to prepend to the child's prompt, if
/// any.
#[cfg(any(feature = "subagents", test))]
pub(crate) async fn gate_subagent_start(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    agent_type: &str,
    agent_source: &str,
) -> Option<String> {
    let canonical = normalize::canonical_tool_name(agent_type);
    let decision = dispatcher
        .dispatch(
            "SubagentStart",
            Some(&canonical),
            ctx,
            envelope::EventFields::SubagentStart {
                agent_type: canonical.clone(),
                agent_source: agent_source.to_string(),
                agent_mode: crate::permission::ask::subagent_mode().into(),
            },
        )
        .await;
    match decision {
        Decision::Rewrite { content } => Some(content),
        Decision::Continue | Decision::Block { .. } | Decision::AdditionalContext { .. } => None,
    }
}

/// Production entry point: reads the process-wide dispatcher and gates
/// subagent start for `agent_type`. Returns `None` when no dispatcher is
/// installed.
#[cfg(feature = "subagents")]
pub(crate) async fn dispatch_subagent_start(
    agent_type: &str,
    agent_source: &str,
) -> Option<String> {
    let dispatcher = get_dispatcher()?;
    let ctx = best_effort_ctx();
    crate::startup::heap_future(|| gate_subagent_start(&dispatcher, &ctx, agent_type, agent_source))
        .await
}

/// Outcome of dispatching `SubagentStop`: release the child's result, or
/// force it to continue with the hook's reason as the next instruction.
#[cfg(any(feature = "subagents", test))]
pub(crate) enum SubagentStopGate {
    Release,
    Continue { reason: String },
}

/// Testable core: dispatches `SubagentStop`, matching on the normalized agent
/// type.
#[cfg(any(feature = "subagents", test))]
pub(crate) async fn gate_subagent_stop(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    agent_type: &str,
    agent_source: &str,
    stop_hook_active: bool,
) -> SubagentStopGate {
    let canonical = normalize::canonical_tool_name(agent_type);
    let decision = dispatcher
        .dispatch(
            "SubagentStop",
            Some(&canonical),
            ctx,
            envelope::EventFields::SubagentStop {
                stop_hook_active,
                agent_type: canonical.clone(),
                agent_source: agent_source.to_string(),
            },
        )
        .await;
    match decision {
        Decision::Block { reason } => SubagentStopGate::Continue { reason },
        Decision::Continue | Decision::Rewrite { .. } | Decision::AdditionalContext { .. } => {
            SubagentStopGate::Release
        }
    }
}

/// Production entry point: reads the process-wide dispatcher and gates
/// subagent completion for `agent_type`. Releases immediately when no
/// dispatcher is installed.
#[cfg(feature = "subagents")]
pub(crate) async fn dispatch_subagent_stop(
    agent_type: &str,
    agent_source: &str,
    stop_hook_active: bool,
) -> SubagentStopGate {
    let Some(dispatcher) = get_dispatcher() else {
        return SubagentStopGate::Release;
    };
    let ctx = best_effort_ctx();
    crate::startup::heap_future(|| {
        gate_subagent_stop(
            &dispatcher,
            &ctx,
            agent_type,
            agent_source,
            stop_hook_active,
        )
    })
    .await
}

/// Testable core of the `hooks test` dry-run: dispatches `PreToolUse` against
/// an explicit dispatcher/context and formats a human-readable report.
pub(crate) async fn hooks_test_dry_run_with(
    dispatcher: &dispatcher::HookDispatcher,
    ctx: &HookCtx,
    tool_name: &str,
    tool_input: serde_json::Value,
) -> String {
    let decision = dispatcher
        .dispatch_pre_tool_use(ctx, tool_name, tool_input)
        .await;
    format!(
        "hooks test PreToolUse tool={tool_name:?}\n  verdict: {:?}\n  reason: {}\n  updated_input: {}",
        decision.verdict,
        decision.reason.as_deref().unwrap_or("(none)"),
        decision
            .updated_input
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(none)".to_string()),
    )
}

/// Production entry point: reads the process-wide dispatcher and dry-runs
/// `PreToolUse` for `tool_name`/`tool_input`, for the `hooks test` CLI flag.
pub(crate) async fn hooks_test_dry_run(tool_name: &str, tool_input: serde_json::Value) -> String {
    let Some(dispatcher) = get_dispatcher() else {
        return "hooks: no dispatcher installed (--no-hooks, disableAllHooks, or no settings.json hooks found)".to_string();
    };
    hooks_test_dry_run_with(&dispatcher, &best_effort_ctx(), tool_name, tool_input).await
}

#[cfg(all(test, unix))]
mod event60_tests {
    use super::*;

    struct RestoreDispatcher(Option<std::sync::Arc<dispatcher::HookDispatcher>>);
    impl Drop for RestoreDispatcher {
        fn drop(&mut self) {
            *DISPATCHER.lock().unwrap_or_else(|error| error.into_inner()) = self.0.take();
        }
    }

    #[tokio::test]
    async fn hooks60_actual_permission_transport_observes_without_answering_and_compact_has_source()
    {
        let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
        let _environment = crate::tests::ScopedProcessEnv::set(&[]);
        let root = crate::tests::OwnedTestDirectory::new("hooks60-events");
        // Mutable observation files are outside the execution workspace; they
        // are output fixtures, not workspace-resident executable arguments.
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut config = settings::HooksConfig::new();
        for event in [
            "PermissionRequest",
            "Notification",
            "PreCompact",
            "SessionStart",
        ] {
            config.insert(event.into(), vec![settings::HookGroup {
                matcher: None,
                hooks: vec![settings::HookHandler {
                    kind: "command".into(), command: Some("/bin/sh".into()),
                    args: Some(vec!["-c".into(), "cat > \"$1\"; printf '%s' '{\"decision\":\"block\",\"permissionDecision\":\"allow\"}'".into(), "event60".into(), root.path().join(event).display().to_string()]),
                    timeout: Some(3), is_async: false, condition: None, once: false,
                    trust: settings::HookTrust::Trusted, env: Default::default(),
                }],
            }]);
        }
        let _restore = RestoreDispatcher(get_dispatcher());
        init_dispatcher(
            dispatcher::HookDispatcher::from_config_with_backend_and_root(
                &config, "none", &workspace,
            )
            .unwrap(),
        );
        let scope = crate::agent::runner::AgentWorkScope::new();
        scope.set_hook_execution_root(&workspace);
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let (reply, mut decision) = tokio::sync::oneshot::channel();
        scope
            .run(async {
                crate::permission::ask::send_permission_request(
                    &sender,
                    crate::permission::ask::AskRequest {
                        diagnostic: None,
                        #[cfg(feature = "subagents")]
                        background: None,
                        agent_name: Some("implementer".into()),
                        tool: "shell".into(),
                        input: "literal request".into(),
                        #[cfg(feature = "acp")]
                        tool_call_id: Some("call60".into()),
                        suggested_pattern: None,
                        additional_allow_patterns: Vec::new(),
                        reply,
                    },
                )
                .await
                .unwrap();
                assert!(matches!(
                    decision.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ));
                let request = receiver.recv().await.unwrap().into_permission();
                assert_eq!(request.agent_name.as_deref(), Some("implementer"));
                assert_eq!(request.tool, "shell");
                assert_eq!(request.input, "literal request");
                let _ = request
                    .reply
                    .send(crate::permission::ask::UserDecision::Deny);
                assert!(matches!(
                    decision.await.unwrap(),
                    crate::permission::ask::UserDecision::Deny
                ));
                dispatch_pre_compact(false, Some("retain decisions")).await;
                dispatch_session_start("compact").await;
            })
            .await;
        cleanup.settle().await;
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(root.path().join(name)).unwrap()).unwrap()
        };
        let permission = read("PermissionRequest");
        assert_eq!(permission["tool_name"], "shell");
        assert_eq!(permission["tool_input"], "literal request");
        assert_eq!(
            read("Notification")["notification_type"],
            "permission_prompt"
        );
        assert_eq!(read("PreCompact")["trigger"], "manual");
        assert_eq!(
            read("PreCompact")["custom_instructions"],
            "retain decisions"
        );
        assert_eq!(read("SessionStart")["source"], "compact");

        #[cfg(feature = "js")]
        {
            use crate::extras::js::tool::{PermissionBridgeError, PermissionBridgeOwner};
            use crate::extras::js::types::{PermCancellation, PermissionDenial};
            use crate::permission::{Action, PermissionConfig, PermissionConfigs, ToolPerm};
            let permission = std::sync::Arc::new(std::sync::Mutex::new(
                crate::permission::checker::PermissionChecker::new(
                    &PermissionConfigs::from(PermissionConfig {
                        read: Some(ToolPerm::Simple(Action::Ask)),
                        doom_loop: Some(Action::Allow),
                        ..Default::default()
                    }),
                    crate::permission::SecurityMode::Standard,
                    Some(workspace.clone()),
                    Some(vec!["standard".into()]),
                )
                .unwrap(),
            ));
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            // Create the long-lived parent receiver outside the turn scope.
            let owner = PermissionBridgeOwner::new(
                Some(permission),
                Some(sender),
                std::time::Duration::from_secs(5),
            );
            let scope = crate::agent::runner::AgentWorkScope::new();
            scope.set_hook_execution_root(&workspace);
            let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
            scope
                .run(async {
                    let bridge = owner.bridge().for_invocation(PermCancellation::new());
                    let check = bridge.check_async("read", "pending.txt");
                    let answer = async {
                        let request = receiver.recv().await.unwrap().into_permission();
                        assert_eq!(request.tool, "read");
                        assert_eq!(request.input, "pending.txt");
                        request
                            .reply
                            .send(crate::permission::ask::UserDecision::Deny)
                            .unwrap();
                    };
                    let (result, ()) = tokio::join!(check, answer);
                    assert_eq!(
                        result,
                        Err(PermissionBridgeError::Denied(PermissionDenial::User))
                    );
                })
                .await;
            drop(owner);
            cleanup.settle().await;
            assert_eq!(read("PermissionRequest")["tool_name"], "read");
            assert_eq!(read("PermissionRequest")["tool_input"], "pending.txt");
        }
    }
}
