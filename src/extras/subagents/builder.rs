use crate::agent::tools;
use crate::context::agents::AgentTool;
use crate::extras::subagents::prompt;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyAgentInner, AnyModel, OpenAiAgent, OpenAiModel};
use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;

/// Report restatement is a text-only model call, even for writing personas.
#[derive(Clone, Copy)]
pub(crate) struct ReportRepairGuard;
impl<M: CompletionModel> rig::agent::AgentHook<M> for ReportRepairGuard {
    async fn on_event(
        &self,
        _: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        match event {
            rig::agent::StepEvent::CompletionCall { .. } => rig::agent::Flow::patch_request(
                rig::agent::RequestPatch::new().active_tools(Vec::<String>::new()),
            ),
            rig::agent::StepEvent::ToolCall { .. } => {
                rig::agent::Flow::terminate("task: report repair cannot execute tools")
            }
            _ => rig::agent::Flow::cont(),
        }
    }
    fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
        matches!(
            kind,
            rig::agent::StepEventKind::CompletionCall | rig::agent::StepEventKind::ToolCall
        )
    }
}

/// A local estimate/admission bound, not a provider tokenizer or an override
/// of model capacity. Observe the complete normalized request after patches;
/// never trim signed history or rewrite the request to make it fit.
#[derive(Clone, Copy)]
struct ChildContextGuard {
    window: u64,
}
impl<M: CompletionModel> rig::agent::AgentHook<M> for ChildContextGuard {
    async fn on_event(
        &self,
        ctx: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        if let rig::agent::StepEvent::CompletionRequestPrepared { request, turn } = event {
            let estimated =
                crate::agent::runner::prepared_request_input_estimate(ctx, request, turn);
            if estimated.is_none_or(|input| {
                input.saturating_add(request.max_tokens.unwrap_or(0)) > self.window
            }) {
                return rig::agent::Flow::terminate(
                    "task: child context estimate exceeds its configured/model window; use a narrower brief or raise subagent_context_window for a supported model",
                );
            }
        }
        rig::agent::Flow::cont()
    }
    fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
        kind == rig::agent::StepEventKind::CompletionRequestPrepared
    }
}

/// The parent session's authorization context inherited by a subagent.
///
/// Keeping the checker and approval channel together makes it impossible to
/// construct one of the child filesystem tools with only half of the parent's
/// authorization path. Nested subagents are deliberately unsupported because
/// the child tool set does not contain `TaskTool`; if nesting is added later,
/// this same context must be inherited rather than replaced.
#[derive(Clone)]
pub(crate) struct SubagentAuthorization {
    write_tools: Option<WriteChildTools>,
    #[cfg(feature = "multimodal")]
    image_support: tools::read_image::ImageReadSupport,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    deny_repeated_reads: bool,
    instructions: Option<std::sync::Arc<crate::context::instructions::WorkspaceInstructions>>,
    #[cfg(feature = "js")]
    read_only_js: Option<ReadOnlyJsAuthorization>,
}

/// Captured from the actual parent builder, including its tool eligibility.
#[derive(Clone)]
pub(crate) struct WriteChildTools {
    pub(crate) sandbox: crate::sandbox::Sandbox,
    pub(crate) write_enabled: bool,
    pub(crate) edit_enabled: bool,
    pub(crate) shell_enabled: bool,
    pub(crate) max_text_file_size: Option<u64>,
    pub(crate) max_shell_output_lines: Option<u64>,
    pub(crate) shell_timeout_secs: u64,
    pub(crate) diff_options: tools::mutation_diff::DiffOptions,
    pub(crate) formatter: Option<std::sync::Arc<tools::format_after_edit::FormatAfterEdit>>,
    #[cfg(feature = "lsp")]
    pub(crate) lsp: Option<crate::extras::lsp::LspManager>,
}

#[cfg(feature = "js")]
#[derive(Clone)]
struct ReadOnlyJsAuthorization {
    sandbox: crate::sandbox::Sandbox,
    containment_status: crate::sandbox::worker::WorkerContainmentStatus,
    allow_config: crate::extras::js::host::AllowConfig,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PersonaExecution {
    pub(crate) write_mode: bool,
    /// `None` installs the complete read-only child tool set. `Some` is always
    /// a narrowing parsed from trusted persona metadata.
    pub(crate) tools: Option<Vec<AgentTool>>,
    /// The user's provider `extra_body` (quick-model or global) resolved for
    /// the child's model exactly as the main agent resolves it. It is combined
    /// with the provider-specific request defaults (OpenRouter routing,
    /// Responses `reasoning`/`store`/`prompt_cache_key`, Completions
    /// `reasoning_effort`) before persona params are merged on top.
    pub(crate) provider_extra_body: Option<serde_json::Value>,
    /// Provider request parameters resolved from the persona's quick-model
    /// alias. These win over every inherited provider parameter.
    pub(crate) additional_params: Option<serde_json::Value>,
    /// Exact server -> wire-level tool names granted to this persona. `None`
    /// means no MCP authority; outer-agent connections are never inherited.
    #[cfg(feature = "mcp")]
    pub(crate) mcp_servers: Option<std::collections::BTreeMap<String, Vec<String>>>,
}

/// Provider request-body parameters a child sends before persona overrides:
/// the same per-provider shape the main agent builds in
/// `provider::build_agent_in_workspace`, so subagents keep the user's
/// `extra_body` and `[reasoning]` settings (including `store = false`).
pub(crate) fn subagent_provider_params(
    model: &AnyModel,
    cfg: &crate::config::Config,
    provider_extra_body: Option<serde_json::Value>,
    session_id: &str,
) -> Option<serde_json::Value> {
    crate::provider::provider_request_params(model, cfg, provider_extra_body, session_id)
}

impl SubagentAuthorization {
    pub(crate) fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        deny_repeated_reads: bool,
    ) -> Self {
        Self {
            write_tools: None,
            #[cfg(feature = "multimodal")]
            image_support: tools::read_image::ImageReadSupport::default(),
            permission,
            ask_tx,
            workspace: None,
            deny_repeated_reads,
            instructions: None,
            #[cfg(feature = "js")]
            read_only_js: None,
        }
    }

    pub(crate) fn with_write_tools(mut self, tools: Option<WriteChildTools>) -> Self {
        self.write_tools = tools;
        self
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    ) -> Self {
        self.workspace = workspace;
        self
    }

    #[cfg(feature = "mcp")]
    fn independent_mcp_permission(
        &self,
        cfg: &crate::config::Config,
    ) -> Result<Option<PermCheck>, String> {
        let Some(parent) = &self.permission else {
            return Ok(None);
        };
        let mode = parent
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mode();
        let permission_config = cfg.build_permission_config().map_err(|error| {
            format!("task: persona MCP permission configuration is invalid: {error}")
        })?;
        let working_dir = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf());
        let checker = crate::permission::checker::PermissionChecker::new_for_sandbox(
            &permission_config,
            mode,
            working_dir,
            cfg.permission_modes.clone(),
            crate::permission::SandboxResolution::Enforced,
        )
        .map_err(|error| format!("task: persona MCP permission scope failed: {error}"))?;
        Ok(Some(std::sync::Arc::new(std::sync::Mutex::new(checker))))
    }

    /// The authorization of a worktree-isolated write child: every tool, the
    /// sandbox and the permission checker are bound to `worktree` and to
    /// nothing else. Fails closed when the parent has no permission checker
    /// (the parent-tree denial lives there) or no write tools.
    ///
    /// * The checker is a snapshot rebound to the worktree with all session
    ///   grants dropped and a hard denial of any mutation under
    ///   `denied_roots` or of a `.git` entry.
    /// * The sandbox is rebound to the worktree, so a shell command can write
    ///   only there. If the sandbox is off, host-wide, or has an extra write
    ///   root overlapping a denied root, the shell tool is removed instead.
    /// * Parent-bound read-only JS and workspace instructions are dropped.
    #[cfg(all(feature = "git-worktree", feature = "subagents"))]
    pub(crate) fn isolated_to(
        &self,
        worktree: std::sync::Arc<crate::paths::WorkspaceBinding>,
        denied_roots: &[std::path::PathBuf],
    ) -> Result<Self, String> {
        let permission = self.permission.as_ref().ok_or_else(|| {
            "worktree isolation requires permission checks; none are active".to_string()
        })?;
        let mut write_tools = self
            .write_tools
            .clone()
            .ok_or_else(|| "worktree isolation requires the parent's write tools".to_string())?;
        let derived = permission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .derive_isolated_child(worktree.root(), denied_roots)
            .map_err(|error| error.to_string())?;
        if denied_roots
            .iter()
            .any(|root| write_tools.sandbox.may_write_under(worktree.root(), root))
        {
            write_tools.shell_enabled = false;
        }
        write_tools.sandbox = write_tools
            .sandbox
            .rebind_workspace_binding(worktree.clone())?;
        let mut isolated = self.clone();
        isolated.permission = Some(std::sync::Arc::new(std::sync::Mutex::new(derived)));
        isolated.write_tools = Some(write_tools);
        isolated.workspace = Some(worktree);
        isolated.instructions = None;
        #[cfg(feature = "js")]
        {
            isolated.read_only_js = None;
        }
        Ok(isolated)
    }

    pub(crate) fn with_workspace_instructions(
        mut self,
        instructions: Option<std::sync::Arc<crate::context::instructions::WorkspaceInstructions>>,
    ) -> Self {
        self.instructions = instructions;
        self
    }

    #[cfg(feature = "js")]
    pub(crate) fn with_read_only_js(
        mut self,
        sandbox: crate::sandbox::Sandbox,
        containment_status: crate::sandbox::worker::WorkerContainmentStatus,
        cfg: &crate::config::Config,
    ) -> Self {
        let Some(workspace) = self.workspace.clone() else {
            return self;
        };
        let allow_config = crate::extras::js::host::AllowConfig::from_settings(
            workspace.root(),
            cfg.js_file_base_dir.as_deref(),
            cfg.js_read_roots.as_deref(),
            None,
            cfg.js_read_unrestricted.unwrap_or(false),
            false,
        )
        .with_workspace_binding(workspace);
        self.read_only_js = Some(ReadOnlyJsAuthorization {
            sandbox,
            containment_status,
            allow_config,
        });
        self
    }

    /// The JS containment condition shared by tool instantiation and the
    /// required-tool preflight: a read-only JS authorization exists and its
    /// worker containment is not `Unavailable`.
    #[cfg(feature = "js")]
    fn read_only_js_available(&self) -> bool {
        self.read_only_js.as_ref().is_some_and(|authorization| {
            !matches!(
                authorization.containment_status,
                crate::sandbox::worker::WorkerContainmentStatus::Unavailable { .. }
            )
        })
    }

    #[cfg(feature = "js")]
    fn read_only_js_tool(
        &self,
        #[cfg(feature = "skills")] skill_services: Option<
            &std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
        >,
    ) -> Option<Box<dyn rig::tool::ToolDyn>> {
        if !self.read_only_js_available() {
            return None;
        }
        let authorization = self.read_only_js.as_ref()?;
        let tool = crate::extras::js::tool::JsTool::new_read_only(
            authorization.sandbox.clone(),
            self.permission.clone(),
            self.ask_tx.clone(),
            authorization.allow_config.clone(),
        );
        #[cfg(feature = "skills")]
        let tool = match skill_services {
            Some(services) => tool.with_skill_turn_context(services.turn_context()),
            None => tool,
        };
        Some(Box::new(tool))
    }

    /// The filesystem tool kinds this authorization installs, in installation
    /// order: the read-only set narrowed by `selected`, then the parent's
    /// eligible write tools when write tools and a workspace are captured.
    /// [`Self::filesystem_tools`] instantiates exactly this list.
    pub(crate) fn filesystem_tool_kinds(&self, selected: Option<&[AgentTool]>) -> Vec<AgentTool> {
        let enabled = |tool| selected.is_none_or(|selected| selected.contains(&tool));
        let mut kinds = [
            AgentTool::Read,
            AgentTool::Grep,
            AgentTool::FindFiles,
            AgentTool::ListDir,
        ]
        .into_iter()
        .filter(|tool| enabled(*tool))
        .collect::<Vec<_>>();
        if let (Some(parent), Some(_)) = (&self.write_tools, &self.workspace) {
            if parent.write_enabled && enabled(AgentTool::Write) {
                kinds.push(AgentTool::Write);
            }
            if parent.edit_enabled && enabled(AgentTool::Edit) {
                kinds.push(AgentTool::Edit);
            }
            if parent.shell_enabled && enabled(AgentTool::Shell) {
                kinds.push(AgentTool::Shell);
            }
        }
        kinds
    }

    pub(crate) fn filesystem_tools(
        &self,
        max_text_file_size: u64,
        max_read_lines: u64,
        max_grep_results: u64,
        max_find_results: u64,
        max_list_dir_entries: Option<u64>,
        selected: Option<&[AgentTool]>,
    ) -> Vec<Box<dyn rig::tool::ToolDyn>> {
        let mut read_tracker = tools::ReadTracker::new(self.deny_repeated_reads);
        #[cfg(feature = "multimodal")]
        {
            read_tracker.image_support = self.image_support;
        }
        read_tracker.instructions = self.instructions.clone();
        let read = tools::ReadTool::new_with_tracker(
            self.permission.clone(),
            self.ask_tx.clone(),
            Some(max_text_file_size),
            max_read_lines,
            read_tracker.clone(),
        );
        let grep = tools::GrepTool::new(
            self.permission.clone(),
            self.ask_tx.clone(),
            max_grep_results,
        );
        let find = tools::FindFilesTool::new(
            self.permission.clone(),
            self.ask_tx.clone(),
            max_find_results,
        );
        let list = tools::ListDirTool::new(
            self.permission.clone(),
            self.ask_tx.clone(),
            max_list_dir_entries,
        );
        let (read, grep, find, list) = if let Some(workspace) = &self.workspace {
            (
                read.with_workspace_binding(workspace.clone()),
                grep.with_workspace_binding(workspace.clone()),
                find.with_workspace_binding(workspace.clone()),
                list.with_workspace_binding(workspace.clone()),
            )
        } else {
            (read, grep, find, list)
        };
        let kinds = self.filesystem_tool_kinds(selected);
        let enabled = |tool| kinds.contains(&tool);
        let mut tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        if enabled(AgentTool::Read) {
            tools.push(Box::new(read));
        }
        if enabled(AgentTool::Grep) {
            tools.push(Box::new(grep));
        }
        if enabled(AgentTool::FindFiles) {
            tools.push(Box::new(find));
        }
        if enabled(AgentTool::ListDir) {
            tools.push(Box::new(list));
        }
        if let (Some(parent), Some(workspace)) = (&self.write_tools, &self.workspace) {
            if enabled(AgentTool::Write) {
                let tool = tools::WriteTool::new_with_tracker(
                    self.permission.clone(),
                    self.ask_tx.clone(),
                    parent.max_text_file_size,
                    read_tracker.clone(),
                )
                .with_workspace_binding(workspace.clone())
                .with_diff_options(parent.diff_options)
                .with_formatter(parent.formatter.clone());
                #[cfg(feature = "lsp")]
                let tool = tool.with_lsp(parent.lsp.clone());
                tools.push(Box::new(tool));
            }
            if enabled(AgentTool::Edit) {
                let tool = tools::EditTool::new_with_tracker(
                    self.permission.clone(),
                    self.ask_tx.clone(),
                    parent.max_text_file_size,
                    read_tracker,
                )
                .with_workspace_binding(workspace.clone())
                .with_diff_options(parent.diff_options)
                .with_formatter(parent.formatter.clone());
                #[cfg(feature = "lsp")]
                let tool = tool.with_lsp(parent.lsp.clone());
                tools.push(Box::new(tool));
            }
            if enabled(AgentTool::Shell) {
                tools.push(Box::new(
                    tools::ShellTool::new(
                        self.permission.clone(),
                        self.ask_tx.clone(),
                        parent.sandbox.clone(),
                        parent.max_shell_output_lines,
                    )
                    .with_foreground_timeout_secs(parent.shell_timeout_secs),
                ));
            }
        }
        tools
    }
}

#[cfg(feature = "mcp")]
pub(crate) async fn build_persona_mcp(
    persona: &PersonaExecution,
    cfg: &crate::config::Config,
    authorization: &SubagentAuthorization,
) -> Result<
    (
        Option<crate::extras::mcp::McpClientManager>,
        Vec<Box<dyn rig::tool::ToolDyn>>,
    ),
    String,
> {
    let Some(allowlist) = persona.mcp_servers.as_ref() else {
        return Ok((None, Vec::new()));
    };
    if allowlist.is_empty() {
        return Ok((None, Vec::new()));
    }
    let configured = cfg
        .mcp_servers
        .as_ref()
        .ok_or_else(|| "task: persona requests MCP servers but none are configured".to_string())?;
    let mut selected = std::collections::HashMap::with_capacity(allowlist.len());
    for server in allowlist.keys() {
        let config = configured
            .get(server)
            .ok_or_else(|| format!("task: persona MCP server '{server}' is not configured"))?;
        selected.insert(server.clone(), config.clone());
    }
    let workspace = authorization
        .workspace
        .as_ref()
        .ok_or_else(|| "task: persona MCP requires a captured workspace".to_string())?;
    let mut manager =
        crate::extras::mcp::McpClientManager::connect_all_in_binding(&selected, workspace).await;
    let permission = authorization.independent_mcp_permission(cfg)?;
    let timeouts = crate::extras::mcp::McpToolTimeouts::from_config_secs(cfg.mcp_tool_timeout_secs);
    let mut tools = crate::agent::builder::collect_mcp_tools(
        &manager,
        permission,
        authorization.ask_tx.clone(),
        timeouts,
    )
    .await;
    let mut discovered = std::collections::BTreeSet::new();
    tools.retain(|tool| {
        let server = tool.server_name.as_str();
        let name = tool.definition.name.as_ref();
        let allowed = allowlist
            .get(server)
            .is_some_and(|names| names.iter().any(|allowed| allowed == name));
        if allowed {
            discovered.insert((server.to_string(), name.to_string()));
        }
        allowed
    });
    let missing = allowlist
        .iter()
        .flat_map(|(server, names)| names.iter().map(move |name| (server, name)))
        .filter(|(server, name)| !discovered.contains(&(server.to_string(), name.to_string())))
        .map(|(server, name)| format!("{server}:{name}"))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        let notices = manager
            .take_notices()
            .into_iter()
            .map(|notice| notice.to_string())
            .collect::<Vec<_>>();
        manager.shutdown().await;
        let detail = if notices.is_empty() {
            String::new()
        } else {
            format!(" ({})", notices.join("; "))
        };
        return Err(format!(
            "task: persona MCP allowlist requested unavailable tool(s): {}{detail}",
            missing.join(", ")
        ));
    }
    let tools = tools
        .into_iter()
        .map(|tool| Box::new(tool) as Box<dyn rig::tool::ToolDyn>)
        .collect();
    Ok((Some(manager), tools))
}

/// The memory tools a subagent is granted: read-only access ONLY
/// (`memory_read`, `memory_search`). `memory_write` and `memory_edit` are
/// deliberately absent, so a subagent can never mutate the user's memory. This
/// is the single place the subagent memory tool set is assembled, so the
/// `subagent_memory_tool_set_excludes_memory_edit` test can guard it directly
/// instead of re-listing the tools it expects.
#[cfg(feature = "memory")]
pub(crate) fn subagent_memory_tools(
    authorization: &SubagentAuthorization,
) -> Vec<Box<dyn rig::tool::ToolDyn>> {
    vec![
        Box::new(crate::extras::memory::MemoryRead::new(
            authorization.permission.clone(),
            authorization.ask_tx.clone(),
        )),
        Box::new(crate::extras::memory::MemorySearch::new(
            authorization.permission.clone(),
            authorization.ask_tx.clone(),
        )),
    ]
}

/// The tool kinds a persona child receives, in installation order. This is the
/// single eligibility decision: [`persona_tools`] instantiates exactly these
/// kinds, and the task preflight compares `required_tools` against their names
/// without instantiating the child tool set.
pub(crate) fn persona_tool_kinds(
    authorization: &SubagentAuthorization,
    persona: &PersonaExecution,
    #[cfg(feature = "skills")] skill_services_available: bool,
) -> Vec<AgentTool> {
    let selected = persona.tools.as_deref();
    #[allow(unused_variables)]
    let enabled = |tool| selected.is_none_or(|selected: &[AgentTool]| selected.contains(&tool));
    #[allow(unused_mut)]
    let mut kinds = authorization.filesystem_tool_kinds(selected);
    #[cfg(feature = "memory")]
    kinds.extend(
        [AgentTool::MemoryRead, AgentTool::MemorySearch]
            .into_iter()
            .filter(|tool| enabled(*tool)),
    );
    #[cfg(feature = "skills")]
    if skill_services_available && enabled(AgentTool::SkillsSearch) {
        kinds.push(AgentTool::SkillsSearch);
    }
    #[cfg(feature = "js")]
    if enabled(AgentTool::Js) && authorization.read_only_js_available() {
        kinds.push(AgentTool::Js);
    }
    kinds
}

/// Names of the tools [`persona_tools`] would install, without building them.
pub(crate) fn persona_tool_names(
    authorization: &SubagentAuthorization,
    persona: &PersonaExecution,
    #[cfg(feature = "skills")] skill_services_available: bool,
) -> Vec<&'static str> {
    persona_tool_kinds(
        authorization,
        persona,
        #[cfg(feature = "skills")]
        skill_services_available,
    )
    .into_iter()
    .map(AgentTool::name)
    .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persona_tools(
    authorization: &SubagentAuthorization,
    persona: &PersonaExecution,
    max_text_file_size: u64,
    max_read_lines: u64,
    max_grep_results: u64,
    max_find_results: u64,
    max_list_dir_entries: Option<u64>,
    #[cfg(feature = "skills")] skill_services: Option<
        &std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) -> Vec<Box<dyn rig::tool::ToolDyn>> {
    #[allow(unused_variables)]
    let kinds = persona_tool_kinds(
        authorization,
        persona,
        #[cfg(feature = "skills")]
        skill_services.is_some(),
    );
    #[allow(unused_mut)]
    let mut tools = authorization.filesystem_tools(
        max_text_file_size,
        max_read_lines,
        max_grep_results,
        max_find_results,
        max_list_dir_entries,
        persona.tools.as_deref(),
    );
    #[cfg(feature = "memory")]
    if kinds
        .iter()
        .any(|kind| matches!(kind, AgentTool::MemoryRead | AgentTool::MemorySearch))
    {
        tools.extend(
            subagent_memory_tools(authorization)
                .into_iter()
                .zip([AgentTool::MemoryRead, AgentTool::MemorySearch])
                .filter_map(|(tool, kind)| kinds.contains(&kind).then_some(tool)),
        );
    }
    #[cfg(feature = "skills")]
    if kinds.contains(&AgentTool::SkillsSearch)
        && let Some(services) = skill_services
    {
        tools.push(Box::new(
            crate::extras::js::skills::search_tool::SkillsSearchTool::new(std::sync::Arc::clone(
                services,
            )),
        ));
    }
    #[cfg(feature = "js")]
    if kinds.contains(&AgentTool::Js)
        && let Some(tool) = authorization.read_only_js_tool(
            #[cfg(feature = "skills")]
            skill_services,
        )
    {
        tools.push(tool);
    }
    tools
}

/// Test entry point with the pre-hoisting signature: loads the suffix itself.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_explore_agent_inner<M: CompletionModel + 'static>(
    model: M,
    protocol: crate::context::instructions::InstructionProtocol,
    max_turns: usize,
    max_tokens: u64,
    context_window: u64,
    max_text_file_size: u64,
    max_read_lines: u64,
    max_grep_results: u64,
    max_find_results: u64,
    max_list_dir_entries: Option<u64>,
    authorization: &SubagentAuthorization,
    additional_params: Option<serde_json::Value>,
    #[cfg(feature = "archmd")] architecture: Option<&str>,
    specialization: Option<&str>,
    persona: &PersonaExecution,
    #[cfg(feature = "skills")] skill_services: Option<
        &std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) -> Agent<M> {
    let suffix = crate::session::storage::load_suffix();
    build_explore_agent_inner_with_suffix(
        model,
        protocol,
        max_turns,
        max_tokens,
        context_window,
        max_text_file_size,
        max_read_lines,
        max_grep_results,
        max_find_results,
        max_list_dir_entries,
        authorization,
        additional_params,
        #[cfg(feature = "archmd")]
        architecture,
        specialization,
        suffix.as_deref(),
        persona,
        Vec::new(),
        #[cfg(feature = "skills")]
        skill_services,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_explore_agent_inner_with_suffix<M: CompletionModel + 'static>(
    model: M,
    protocol: crate::context::instructions::InstructionProtocol,
    max_turns: usize,
    max_tokens: u64,
    context_window: u64,
    max_text_file_size: u64,
    max_read_lines: u64,
    max_grep_results: u64,
    max_find_results: u64,
    max_list_dir_entries: Option<u64>,
    authorization: &SubagentAuthorization,
    // Inherited provider request params (see `subagent_provider_params`);
    // persona params are merged on top.
    additional_params: Option<serde_json::Value>,
    #[cfg(feature = "archmd")] architecture: Option<&str>,
    // Optional specialization prompt prepended before the base explore prompt.
    specialization: Option<&str>,
    // The user's prompt suffix, loaded once per task call by the caller.
    suffix: Option<&str>,
    persona: &PersonaExecution,
    mut additional_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
    #[cfg(feature = "skills")] skill_services: Option<
        &std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) -> Agent<M> {
    let mut preamble = build_explore_preamble(
        #[cfg(feature = "archmd")]
        architecture,
        specialization,
        suffix,
    );
    if persona.write_mode {
        // Only the host's own read-only rules are replaced; authored persona
        // text is retained verbatim and never supplies execution authority.
        let policy_start = preamble
            .rfind(prompt::NON_OVERRIDABLE_EXPLORE_RULES)
            .expect("host policy is always appended");
        preamble.truncate(policy_start);
        preamble.push_str(&prompt::write_rules());
        preamble.push_str("\n\nWrite-capable child tools: write, edit, shell (only the parent's eligible tools). Implement the delegated objective; report edits, checks, and any missing evidence in the required sections.");
    }

    #[cfg(feature = "agent-skills")]
    preamble.push_str(crate::extras::skills::selection::POLICY);

    let mut captured = authorization.clone();
    if !persona.write_mode {
        captured.write_tools = None;
    }
    captured.instructions = authorization
        .instructions
        .as_ref()
        .map(|service| service.for_protocol(protocol));
    let authorization = &captured;
    if let Some(service) = &authorization.instructions {
        preamble.push_str(crate::context::instructions::POLICY);
        preamble.push_str(&service.initial_overlay());
    }

    let tools = persona_tools(
        authorization,
        persona,
        max_text_file_size,
        max_read_lines,
        max_grep_results,
        max_find_results,
        max_list_dir_entries,
        #[cfg(feature = "skills")]
        skill_services,
    );
    // Persona-scoped MCP tools are connected and narrowed by the caller
    // (`build_persona_mcp`); outer-agent MCP connections are never inherited.
    let mut tools = tools;
    tools.append(&mut additional_tools);
    let tools = tools::memoize::definitions(tools);

    #[cfg(feature = "hooks")]
    let tools = crate::extras::hooks::wrap_from_global(
        tools,
        authorization.permission.clone(),
        authorization.ask_tx.clone(),
    );

    let tools = tools::concurrency::bind(tools);

    let mut builder = AgentBuilder::new(model)
        .preamble(&preamble)
        .default_max_turns(max_turns)
        .max_tokens(max_tokens)
        .tools(tools)
        .add_hook(crate::agent::runner::ToolLoopGuard)
        .add_hook(ChildContextGuard {
            window: context_window,
        });
    builder = builder.add_hook(crate::agent::runner::ToolResultSpillHook::new(
        uuid::Uuid::new_v4().to_string(),
        None,
    ));

    builder = builder.add_hook(crate::context::instructions::WorkspaceInstructionHook(
        authorization.instructions.clone(),
    ));

    if let Some(params) =
        crate::provider::merge_extra_body(additional_params, persona.additional_params.clone())
    {
        builder = builder.additional_params(params);
    }

    builder.build()
}

fn build_explore_preamble(
    #[cfg(feature = "archmd")] architecture: Option<&str>,
    specialization: Option<&str>,
    suffix: Option<&str>,
) -> String {
    let mut preamble = String::new();
    if let Some(spec) = specialization
        && !spec.is_empty()
    {
        preamble.push_str(spec);
        preamble.push_str("\n\n---\n\n");
    }
    preamble.push_str(&prompt::explore_prompt());
    #[cfg(feature = "archmd")]
    if let Some(arch) = architecture
        && !arch.is_empty()
    {
        preamble.push_str("\n\n");
        preamble.push_str(arch);
    }
    if let Some(suffix) = suffix {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(suffix);
    }
    preamble.push_str("\n\n---\n\n");
    preamble.push_str(prompt::NON_OVERRIDABLE_EXPLORE_RULES);
    preamble
}

/// Task admission has already resolved persona/custom-provider identity and
/// clamped its captured window. Preserve it. Direct callers without that
/// snapshot can resolve only the typed backend/model, never infer a custom
/// alias from the main session; unknown models retain the safe default.
fn child_context_window(model: &AnyModel, cfg: &crate::config::Config) -> u64 {
    if let Some(window) = cfg.subagent_context_window {
        return window;
    }
    let (provider, model) = match model {
        AnyModel::OpenRouter(model, _) => ("openrouter", model.model.as_str()),
        AnyModel::OpenAI(OpenAiModel::Responses(model)) => ("openai", model.model.as_str()),
        AnyModel::OpenAI(OpenAiModel::Completions(model)) => ("openai", model.model.as_str()),
        AnyModel::Anthropic(model) => ("anthropic", model.model.as_str()),
        AnyModel::Gemini(model) => ("gemini", model.model.as_str()),
        AnyModel::Ollama(model) => ("ollama", model.model.as_str()),
        AnyModel::Copilot(model) => ("copilot", model.model.as_str()),
    };
    cfg.resolve_subagent_context_window(provider, model, None)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_explore_agent(
    model: AnyModel,
    max_turns: usize,
    cfg: &crate::config::Config,
    authorization: SubagentAuthorization,
    #[cfg(feature = "archmd")] architecture: Option<String>,
    specialization: Option<String>,
    persona: PersonaExecution,
    #[cfg(feature = "mcp")] mcp_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
    #[cfg(feature = "skills")] skill_services: Option<
        std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) -> AnyAgent {
    let suffix = crate::session::storage::load_suffix();
    build_explore_agent_with_suffix(
        model,
        max_turns,
        cfg,
        authorization,
        #[cfg(feature = "archmd")]
        architecture,
        specialization,
        suffix.as_deref(),
        persona,
        #[cfg(feature = "mcp")]
        mcp_tools,
        #[cfg(feature = "skills")]
        skill_services,
    )
    .await
}

/// [`build_explore_agent`] with the user's prompt suffix already loaded, so a
/// task call reads the suffix file once rather than once per child.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_explore_agent_with_suffix(
    model: AnyModel,
    max_turns: usize,
    cfg: &crate::config::Config,
    authorization: SubagentAuthorization,
    #[cfg(feature = "archmd")] architecture: Option<String>,
    specialization: Option<String>,
    suffix: Option<&str>,
    persona: PersonaExecution,
    #[cfg(feature = "mcp")] mcp_tools: Vec<Box<dyn rig::tool::ToolDyn>>,
    #[cfg(feature = "skills")] skill_services: Option<
        std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) -> AnyAgent {
    let max_tokens = cfg
        .max_tokens
        .into_iter()
        .fold(cfg.resolve_subagent_max_tokens(), u64::min);
    let context_window = child_context_window(&model, cfg);
    let max_text_file_size = cfg.max_text_file_size.unwrap_or(10 * 1024 * 1024);
    let max_read_lines = cfg.resolve_subagent_max_read_lines();
    let max_grep_results = cfg.resolve_subagent_max_grep_results();
    let max_find_results = cfg.resolve_subagent_max_find_results();
    let max_list_dir_entries = cfg.resolve_subagent_max_list_dir_entries();
    #[cfg(feature = "archmd")]
    let arch_ref = architecture.as_deref();
    let spec_ref = specialization.as_deref();
    let provider_params = subagent_provider_params(
        &model,
        cfg,
        persona.provider_extra_body.clone(),
        &uuid::Uuid::new_v4().to_string(),
    );
    #[cfg(feature = "multimodal")]
    let authorization = {
        let mut authorization = authorization;
        let image_params = persona
            .additional_params
            .as_ref()
            .filter(|params| params.get("model").is_some())
            .or(provider_params.as_ref());
        authorization.image_support = model.image_read_support(image_params);
        authorization
    };
    #[cfg(feature = "agent-skills")]
    let instructions = authorization.workspace.as_ref().and_then(|workspace| {
        crate::paths::process_paths().ok().map(|paths| {
            crate::extras::skills::session::InstructionOwner::default().resolve(
                paths,
                workspace.clone(),
                authorization.permission.clone(),
                cfg.no_context_files.unwrap_or(false),
            )
        })
    });
    let inner = match model {
        AnyModel::OpenRouter(m, _) => {
            AnyAgentInner::OpenRouter(build_explore_agent_inner_with_suffix(
                m,
                crate::context::instructions::InstructionProtocol::Chat,
                max_turns,
                max_tokens,
                context_window,
                max_text_file_size,
                max_read_lines,
                max_grep_results,
                max_find_results,
                max_list_dir_entries,
                &authorization,
                provider_params.clone(),
                #[cfg(feature = "archmd")]
                arch_ref,
                spec_ref,
                suffix,
                &persona,
                #[cfg(feature = "mcp")]
                mcp_tools,
                #[cfg(not(feature = "mcp"))]
                Vec::new(),
                #[cfg(feature = "skills")]
                skill_services.as_ref(),
            ))
        }
        AnyModel::OpenAI(m) => AnyAgentInner::OpenAI(match m {
            OpenAiModel::Responses(m) => {
                OpenAiAgent::Responses(build_explore_agent_inner_with_suffix(
                    m,
                    crate::context::instructions::InstructionProtocol::Responses,
                    max_turns,
                    max_tokens,
                    context_window,
                    max_text_file_size,
                    max_read_lines,
                    max_grep_results,
                    max_find_results,
                    max_list_dir_entries,
                    &authorization,
                    provider_params.clone(),
                    #[cfg(feature = "archmd")]
                    arch_ref,
                    spec_ref,
                    suffix,
                    &persona,
                    #[cfg(feature = "mcp")]
                    mcp_tools,
                    #[cfg(not(feature = "mcp"))]
                    Vec::new(),
                    #[cfg(feature = "skills")]
                    skill_services.as_ref(),
                ))
            }
            OpenAiModel::Completions(m) => {
                OpenAiAgent::Completions(build_explore_agent_inner_with_suffix(
                    m,
                    crate::context::instructions::InstructionProtocol::Chat,
                    max_turns,
                    max_tokens,
                    context_window,
                    max_text_file_size,
                    max_read_lines,
                    max_grep_results,
                    max_find_results,
                    max_list_dir_entries,
                    &authorization,
                    provider_params.clone(),
                    #[cfg(feature = "archmd")]
                    arch_ref,
                    spec_ref,
                    suffix,
                    &persona,
                    #[cfg(feature = "mcp")]
                    mcp_tools,
                    #[cfg(not(feature = "mcp"))]
                    Vec::new(),
                    #[cfg(feature = "skills")]
                    skill_services.as_ref(),
                ))
            }
        }),
        AnyModel::Anthropic(m) => AnyAgentInner::Anthropic(build_explore_agent_inner_with_suffix(
            m,
            crate::context::instructions::InstructionProtocol::Anthropic,
            max_turns,
            max_tokens,
            context_window,
            max_text_file_size,
            max_read_lines,
            max_grep_results,
            max_find_results,
            max_list_dir_entries,
            &authorization,
            provider_params.clone(),
            #[cfg(feature = "archmd")]
            arch_ref,
            spec_ref,
            suffix,
            &persona,
            #[cfg(feature = "mcp")]
            mcp_tools,
            #[cfg(not(feature = "mcp"))]
            Vec::new(),
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
        AnyModel::Gemini(m) => AnyAgentInner::Gemini(build_explore_agent_inner_with_suffix(
            m,
            crate::context::instructions::InstructionProtocol::Gemini,
            max_turns,
            max_tokens,
            context_window,
            max_text_file_size,
            max_read_lines,
            max_grep_results,
            max_find_results,
            max_list_dir_entries,
            &authorization,
            provider_params.clone(),
            #[cfg(feature = "archmd")]
            arch_ref,
            spec_ref,
            suffix,
            &persona,
            #[cfg(feature = "mcp")]
            mcp_tools,
            #[cfg(not(feature = "mcp"))]
            Vec::new(),
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
        AnyModel::Ollama(m) => AnyAgentInner::Ollama(build_explore_agent_inner_with_suffix(
            m,
            crate::context::instructions::InstructionProtocol::Ollama,
            max_turns,
            max_tokens,
            context_window,
            max_text_file_size,
            max_read_lines,
            max_grep_results,
            max_find_results,
            max_list_dir_entries,
            &authorization,
            provider_params.clone(),
            #[cfg(feature = "archmd")]
            arch_ref,
            spec_ref,
            suffix,
            &persona,
            #[cfg(feature = "mcp")]
            mcp_tools,
            #[cfg(not(feature = "mcp"))]
            Vec::new(),
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
        AnyModel::Copilot(m) => AnyAgentInner::Copilot(build_explore_agent_inner_with_suffix(
            m.clone(),
            crate::provider::copilot::instruction_protocol(&m),
            max_turns,
            max_tokens,
            context_window,
            max_text_file_size,
            max_read_lines,
            max_grep_results,
            max_find_results,
            max_list_dir_entries,
            &authorization,
            provider_params.clone(),
            #[cfg(feature = "archmd")]
            arch_ref,
            spec_ref,
            suffix,
            &persona,
            #[cfg(feature = "mcp")]
            mcp_tools,
            #[cfg(not(feature = "mcp"))]
            Vec::new(),
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
    };
    let agent = AnyAgent::with_runtime(
        inner,
        #[cfg(feature = "skills")]
        skill_services,
    );
    #[cfg(feature = "agent-skills")]
    let agent = match instructions {
        Some(services) => agent.with_instruction_services(services),
        None => agent,
    };
    agent
}

#[cfg(all(test, feature = "mcp"))]
mod persona_mcp_tests {
    use std::sync::{Arc, Mutex};

    use super::{PersonaExecution, SubagentAuthorization, build_persona_mcp};
    use crate::permission::SecurityMode;
    use crate::permission::checker::{CheckResult, PermissionChecker};

    fn workspace() -> Arc<crate::paths::WorkspaceBinding> {
        Arc::new(
            crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
        )
    }

    #[test]
    fn persona_mcp_permission_does_not_inherit_outer_allow_all() {
        let mut config = crate::config::Config {
            permission: Some(serde_json::json!({"mcp_tool": "ask"})),
            ..crate::config::Config::default()
        };
        config.permission_modes = Some(vec!["standard".to_string()]);
        let permission_config = config.build_permission_config().unwrap();
        let mut parent = PermissionChecker::new(
            &permission_config,
            SecurityMode::Standard,
            Some(workspace().root().to_path_buf()),
            config.permission_modes.clone(),
        )
        .unwrap();
        parent.set_allow_all_mcp_calls(true);
        let parent = Arc::new(Mutex::new(parent));
        let authorization = SubagentAuthorization::new(Some(parent.clone()), None, true)
            .with_workspace_binding(Some(workspace()));
        let child = authorization
            .independent_mcp_permission(&config)
            .unwrap()
            .unwrap();

        assert!(matches!(
            parent
                .lock()
                .unwrap()
                .check("mcp_tool", "mcp_tool:docs:search"),
            CheckResult::Allowed
        ));
        assert!(matches!(
            child
                .lock()
                .unwrap()
                .check("mcp_tool", "mcp_tool:docs:search"),
            CheckResult::Ask
        ));
        assert!(!Arc::ptr_eq(&parent, &child));
    }

    #[tokio::test]
    async fn persona_mcp_rejects_unknown_server_before_connecting() {
        let persona = PersonaExecution {
            mcp_servers: Some(std::collections::BTreeMap::from([(
                "missing".to_string(),
                vec!["search".to_string()],
            )])),
            ..PersonaExecution::default()
        };
        let config = crate::config::Config {
            mcp_servers: Some(std::collections::HashMap::new()),
            ..crate::config::Config::default()
        };
        let authorization =
            SubagentAuthorization::new(None, None, true).with_workspace_binding(Some(workspace()));

        let error = match build_persona_mcp(&persona, &config, &authorization).await {
            Ok(_) => panic!("unknown persona MCP server was accepted"),
            Err(error) => error,
        };
        assert!(error.contains("not configured"), "{error}");
    }
}

#[cfg(all(test, feature = "js"))]
mod js_isolation_tests {
    use super::{PersonaExecution, SubagentAuthorization, build_explore_agent_inner};
    use crate::context::agents::AgentTool;

    fn configured_authorization() -> SubagentAuthorization {
        let workspace = std::sync::Arc::new(
            crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
        );
        SubagentAuthorization::new(None, None, true)
            .with_workspace_binding(Some(workspace.clone()))
            .with_read_only_js(
                crate::sandbox::Sandbox::new(false, "bwrap").with_workspace_binding(workspace),
                crate::sandbox::worker::WorkerContainmentStatus::Available {
                    backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
                    assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
                },
                &crate::config::Config::default(),
            )
    }

    // mini-agent-f3jjk: non-pure skills are filtered out of a read-only child's bundle before
    // the tool's own guard runs, so the literal fail-closed error is unreachable end to end
    // unless the filter regresses. Inject a non-pure skill straight into the forked child
    // context and prove the real builder-wired read-only tool still refuses it.
    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn read_only_child_js_tool_refuses_an_injected_non_pure_skill() {
        use crate::extras::js::skills::turn::{ResolvedSkill, TurnSkillBundle};
        use crate::extras::js::skills::{CapabilityManifest, CapabilityScope, CapabilityTier};

        let root = crate::tests::support::temp::TempDir::new("read-only-child-guard");
        let paths = crate::paths::AppPaths::resolve(&crate::paths::PathEnvironment {
            platform: if cfg!(target_os = "macos") {
                crate::paths::PathPlatform::MacOs
            } else if cfg!(target_os = "windows") {
                crate::paths::PathPlatform::Windows
            } else {
                crate::paths::PathPlatform::Linux
            },
            home_dir: None,
            config_base: Some(root.path().join("config")),
            data_base: Some(root.path().join("data")),
            local_data_base: Some(root.path().join("local")),
            state_base: Some(root.path().join("state")),
            cache_base: Some(root.path().join("cache")),
            workspace_root: None,
            overrides: Default::default(),
        })
        .unwrap();
        let parent = crate::extras::js::skills::session::SkillSessionServices::for_test(&paths);
        let child = parent.fork_for_read_only_child();
        child.turn_context().replace(TurnSkillBundle {
            turn_id: "read-only-child".to_string(),
            query_fingerprint: "fixture".to_string(),
            embedding_model_revision: "fixture".to_string(),
            index_generation: 1,
            skills: vec![ResolvedSkill {
                id: "effectful-fixture".to_string(),
                identity_version: 2,
                abi_version: 1,
                description: "must not enter a read-only child".to_string(),
                tags: Vec::new(),
                exports: Vec::new(),
                tests: Vec::new(),
                capability: CapabilityManifest::new(
                    CapabilityTier::SideEffecting,
                    vec![CapabilityScope::WriteFile {
                        workspace_prefixes: vec!["tmp".to_string()],
                    }],
                )
                .unwrap(),
                source: String::new(),
                score_bits: 1.0_f32.to_bits(),
                rank: 1,
                route: None,
            }],
        });

        let tool = configured_authorization()
            .read_only_js_tool(Some(&child))
            .expect("the configured read-only JS tool is built");
        let error = tool
            .call(r#"{"code":"1"}"#.to_string())
            .await
            .expect_err("a non-pure skill in the child context must fail closed");
        assert!(
            error
                .to_string()
                .contains("read-only JS skill bundle contains a non-pure capability"),
            "{error}"
        );
    }

    #[test]
    fn caller_loaded_suffix_reaches_the_child_preamble_before_host_rules() {
        let model = rig::test_utils::MockCompletionModel::from_stream_turns(vec![vec![
            rig::test_utils::MockStreamEvent::text("unused"),
        ]]);
        let agent = super::build_explore_agent_inner_with_suffix(
            model,
            crate::context::instructions::InstructionProtocol::Unverified,
            2,
            16_384,
            262_144,
            1024 * 1024,
            1_000,
            1_000,
            1_000,
            Some(1_000),
            &SubagentAuthorization::new(None, None, true),
            None,
            #[cfg(feature = "archmd")]
            None,
            None,
            Some("HOISTED_SUFFIX_MARKER"),
            &PersonaExecution::default(),
            Vec::new(),
            #[cfg(feature = "skills")]
            None,
        );
        let preamble = agent.preamble.expect("child preamble");
        let suffix = preamble.find("HOISTED_SUFFIX_MARKER").unwrap();
        assert!(
            suffix
                < preamble
                    .find(super::prompt::NON_OVERRIDABLE_EXPLORE_RULES)
                    .unwrap()
        );
    }

    #[tokio::test]
    async fn subagent_without_an_authorized_worker_context_omits_js() {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("subagent"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = build_explore_agent_inner(
            model,
            crate::context::instructions::InstructionProtocol::Unverified,
            2,
            16_384,
            262_144,
            1024 * 1024,
            1_000,
            1_000,
            1_000,
            Some(1_000),
            &SubagentAuthorization::new(None, None, true),
            None,
            #[cfg(feature = "archmd")]
            None,
            None,
            &PersonaExecution::default(),
            #[cfg(feature = "skills")]
            None,
        );
        let names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("subagent tool definitions")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "read"));
        assert!(!names.iter().any(|name| name == "js"), "{names:?}");
    }

    #[tokio::test]
    async fn configured_explore_subagent_gets_the_read_only_js_profile() {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("subagent"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = build_explore_agent_inner(
            model,
            crate::context::instructions::InstructionProtocol::Unverified,
            2,
            16_384,
            262_144,
            1024 * 1024,
            1_000,
            1_000,
            1_000,
            Some(1_000),
            &configured_authorization(),
            None,
            #[cfg(feature = "archmd")]
            None,
            None,
            &PersonaExecution::default(),
            #[cfg(feature = "skills")]
            None,
        );
        let definitions = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("subagent tool definitions");
        let js = definitions
            .iter()
            .find(|tool| tool.name == "js")
            .expect("read-only js tool");
        assert!(js.description.contains("read_file(path"));
        assert!(js.description.contains("list_dir(path"));
        assert!(js.description.contains("grep(pattern"));
        assert!(!js.description.contains("write_file(path"));
        assert!(!js.description.contains("fetch(url"));
        assert!(!js.description.contains("spawn(program"));

        let narrowed_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("subagent"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let narrowed = build_explore_agent_inner(
            narrowed_model,
            crate::context::instructions::InstructionProtocol::Unverified,
            2,
            16_384,
            262_144,
            1024 * 1024,
            1_000,
            1_000,
            1_000,
            Some(1_000),
            &configured_authorization(),
            None,
            #[cfg(feature = "archmd")]
            None,
            None,
            &PersonaExecution {
                tools: Some(vec![AgentTool::Read]),
                ..PersonaExecution::default()
            },
            #[cfg(feature = "skills")]
            None,
        );
        let narrowed_names = narrowed
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("narrowed subagent tool definitions")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(narrowed_names, vec!["read"]);
    }

    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn read_only_subagent_gets_search_without_js_execution() {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let root = std::env::temp_dir().join(format!(
            "mini-agent-subagent-skill-search-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let paths = crate::paths::AppPaths::resolve(&crate::paths::PathEnvironment {
            platform: if cfg!(target_os = "macos") {
                crate::paths::PathPlatform::MacOs
            } else if cfg!(target_os = "windows") {
                crate::paths::PathPlatform::Windows
            } else {
                crate::paths::PathPlatform::Linux
            },
            home_dir: None,
            config_base: Some(root.join("config")),
            data_base: Some(root.join("data")),
            local_data_base: Some(root.join("local")),
            state_base: Some(root.join("state")),
            cache_base: Some(root.join("cache")),
            workspace_root: None,
            overrides: Default::default(),
        })
        .unwrap();
        let services = crate::extras::js::skills::session::SkillSessionServices::for_test(&paths);
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("subagent"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = build_explore_agent_inner(
            model,
            crate::context::instructions::InstructionProtocol::Unverified,
            2,
            16_384,
            262_144,
            1024 * 1024,
            1_000,
            1_000,
            1_000,
            Some(1_000),
            &SubagentAuthorization::new(None, None, true),
            None,
            #[cfg(feature = "archmd")]
            None,
            None,
            &PersonaExecution::default(),
            Some(&services),
        );
        let names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("subagent tool definitions")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();

        assert!(
            names.iter().any(|name| name == "skills_search"),
            "{names:?}"
        );
        assert!(!names.iter().any(|name| name == "js"), "{names:?}");

        let narrowed_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("subagent"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let narrowed = build_explore_agent_inner(
            narrowed_model,
            crate::context::instructions::InstructionProtocol::Unverified,
            2,
            16_384,
            262_144,
            1024 * 1024,
            1_000,
            1_000,
            1_000,
            Some(1_000),
            &SubagentAuthorization::new(None, None, true),
            None,
            #[cfg(feature = "archmd")]
            None,
            None,
            &PersonaExecution {
                tools: Some(vec![crate::context::agents::AgentTool::Read]),
                ..PersonaExecution::default()
            },
            Some(&services),
        );
        let narrowed_names = narrowed
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("narrowed subagent tool definitions")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(narrowed_names, vec!["read"]);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use crate::permission::ask::UserDecision;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{PermissionConfigs, SecurityMode};

    use super::{SubagentAuthorization, build_explore_preamble};

    #[cfg(feature = "archmd")]
    #[test]
    fn explore_preamble_uses_only_the_supplied_session_architecture() {
        let first = build_explore_preamble(Some("FIRST_SESSION_ARCHITECTURE"), None, None);
        let second = build_explore_preamble(Some("SECOND_SESSION_ARCHITECTURE"), None, None);
        assert!(first.contains("FIRST_SESSION_ARCHITECTURE"));
        assert!(!first.contains("SECOND_SESSION_ARCHITECTURE"));
        assert!(second.contains("SECOND_SESSION_ARCHITECTURE"));
        assert!(!second.contains("FIRST_SESSION_ARCHITECTURE"));
    }

    #[tokio::test]
    async fn imported_persona_tools_reach_actual_child_definitions_without_write_authority() {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        for (declared, expected) in [
            ("Read, Edit, Bash, Glob, Write", vec!["find_files", "read"]),
            ("Edit, Bash, Write, Task, Fetch, mcp_tool", vec![]),
            ("Read, Grep, Glob", vec!["find_files", "grep", "read"]),
        ] {
            let persona = crate::context::agents::parse_for_test(
                "review",
                &format!(
                    "---\nname: review\ntools: {declared}\n---\nImported investigation persona"
                ),
            );
            let model = MockCompletionModel::from_stream_turns(vec![vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ]]);
            let fixture = crate::tests::OwnedTestDirectory::new("imported-persona-child-tools");
            let authorization = parent_authorization(fixture.path(), None);
            let child = super::build_explore_agent_inner(
                model,
                crate::context::instructions::InstructionProtocol::Unverified,
                2,
                16_384,
                262_144,
                1024 * 1024,
                100,
                100,
                100,
                Some(100),
                &authorization,
                None,
                #[cfg(feature = "archmd")]
                None,
                Some(&persona.prompt),
                &super::PersonaExecution {
                    tools: persona.tools.clone(),
                    ..Default::default()
                },
                #[cfg(feature = "skills")]
                None,
            );
            let mut names = child
                .tool_server_handle
                .get_tool_defs(None)
                .await
                .unwrap()
                .into_iter()
                .map(|tool| tool.name.to_string())
                .collect::<Vec<_>>();
            names.sort_unstable();
            let mut preflight_names = super::persona_tools(
                &authorization,
                &super::PersonaExecution {
                    tools: persona.tools.clone(),
                    ..Default::default()
                },
                1024 * 1024,
                100,
                100,
                100,
                Some(100),
                #[cfg(feature = "skills")]
                None,
            )
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
            preflight_names.sort_unstable();
            assert_eq!(
                preflight_names, names,
                "preflight must use actual builder definitions"
            );
            assert_eq!(names, expected, "declared: {declared}");
        }
    }

    #[test]
    fn specialization_is_the_only_authoritative_persona() {
        let preamble = build_explore_preamble(
            #[cfg(feature = "archmd")]
            None,
            Some("You are a Rust async specialist."),
            None,
        );
        let spec_pos = preamble.find("You are a Rust async specialist.").unwrap();
        let base_pos = preamble
            .find("A specialization above this base prompt")
            .unwrap();
        assert!(
            spec_pos < base_pos,
            "specialization must precede base prompt"
        );
        assert_eq!(
            preamble.matches("You are a ").count(),
            1,
            "the base prompt must not install a second persona"
        );
        assert!(
            preamble.contains("supplies domain heuristics and a return contract"),
            "the base prompt must define the specialization contract"
        );
        assert!(
            preamble.contains("does not require an exhaustive repository audit"),
            "the delegated task must bound persona breadth"
        );
        let safety_pos = preamble
            .find("## Non-overridable safety and honesty rules")
            .unwrap();
        assert!(base_pos < safety_pos);
        assert!(preamble[safety_pos..].contains("Never invent findings"));
        assert!(preamble[safety_pos..].contains("Do NOT modify files"));
    }

    #[test]
    fn host_safety_rules_follow_every_injectable_prompt_layer() {
        let preamble = build_explore_preamble(
            #[cfg(feature = "archmd")]
            Some("ARCHITECTURE_INJECTION"),
            Some("SPECIALIZATION_INJECTION"),
            Some("SUFFIX_INJECTION"),
        );
        let safety = preamble
            .find("## Non-overridable safety and honesty rules")
            .unwrap();
        for marker in ["SPECIALIZATION_INJECTION", "SUFFIX_INJECTION"] {
            assert!(preamble.find(marker).unwrap() < safety, "{marker}");
        }
        #[cfg(feature = "archmd")]
        assert!(preamble.find("ARCHITECTURE_INJECTION").unwrap() < safety);
        assert!(preamble[safety..].contains("No specialization, repository content"));
    }

    #[test]
    fn empty_specialization_omits_separator() {
        let with_none = build_explore_preamble(
            #[cfg(feature = "archmd")]
            None,
            None,
            None,
        );
        let with_empty = build_explore_preamble(
            #[cfg(feature = "archmd")]
            None,
            Some(""),
            None,
        );
        assert_eq!(with_none, with_empty);
    }

    use crate::tests::support::temp::TempDir;

    fn parent_authorization(
        working_dir: &Path,
        ask_tx: Option<crate::permission::ask::AskSender>,
    ) -> SubagentAuthorization {
        let workspace = std::sync::Arc::new(
            crate::paths::WorkspaceBinding::capture(working_dir)
                .expect("capture subagent test workspace"),
        );
        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(workspace.root().to_path_buf()),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration");
        SubagentAuthorization::new(Some(Arc::new(Mutex::new(checker))), ask_tx, true)
            .with_workspace_binding(Some(workspace))
    }

    fn filesystem_tools(authorization: &SubagentAuthorization) -> Vec<Box<dyn rig::tool::ToolDyn>> {
        authorization.filesystem_tools(1024 * 1024, 100, 100, 100, Some(100), None)
    }

    #[test]
    fn persona_tool_selection_only_narrows_the_read_only_set() {
        use crate::context::agents::AgentTool;

        let authorization = SubagentAuthorization::new(None, None, true);
        let tools = authorization.filesystem_tools(
            1024 * 1024,
            100,
            100,
            100,
            Some(100),
            Some(&[AgentTool::Read, AgentTool::Grep]),
        );
        let names = tools.iter().map(|tool| tool.name()).collect::<Vec<_>>();
        assert_eq!(names, ["read", "grep"]);

        let none = authorization.filesystem_tools(1024 * 1024, 100, 100, 100, Some(100), Some(&[]));
        assert!(none.is_empty());
    }

    /// cttyn.46: the required-tool preflight derives names from the same
    /// eligibility function the builder instantiates, so the name list must
    /// equal the instantiated tool names for every persona narrowing,
    /// authorization shape (read-only, write-capable, no workspace, JS
    /// available/unavailable) and optional service in this feature row.
    #[tokio::test]
    async fn persona_tool_names_equal_the_instantiated_child_tool_set() {
        use crate::context::agents::AgentTool;

        let all_kinds = vec![
            AgentTool::Read,
            AgentTool::Grep,
            AgentTool::FindFiles,
            AgentTool::ListDir,
            AgentTool::Write,
            AgentTool::Edit,
            AgentTool::Shell,
            #[cfg(feature = "js")]
            AgentTool::Js,
            #[cfg(feature = "skills")]
            AgentTool::SkillsSearch,
            #[cfg(feature = "memory")]
            AgentTool::MemoryRead,
            #[cfg(feature = "memory")]
            AgentTool::MemorySearch,
        ];
        let mut narrowings: Vec<Option<Vec<AgentTool>>> =
            vec![None, Some(Vec::new()), Some(all_kinds.clone())];
        for kind in &all_kinds {
            narrowings.push(Some(vec![*kind]));
            narrowings.push(Some(
                all_kinds
                    .iter()
                    .copied()
                    .filter(|other| other != kind)
                    .collect(),
            ));
        }
        // Embedded personas narrow through their real parsed metadata too.
        narrowings.extend(crate::context::agents::embedded_tool_narrowings_for_test());

        let root = TempDir::new("tool-name-equivalence");
        #[cfg_attr(not(feature = "js"), allow(unused_mut))]
        let mut authorizations = vec![
            ("no workspace", SubagentAuthorization::new(None, None, true)),
            ("read-only", parent_authorization(root.path(), None)),
            ("write-capable", writable_parent(root.path(), None)),
        ];
        #[cfg(feature = "js")]
        {
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let sandbox = crate::sandbox::Sandbox::new(false, "bwrap")
                .with_workspace_binding(workspace.clone());
            let available = crate::sandbox::worker::WorkerContainmentStatus::Available {
                backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
                assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
            };
            let base = SubagentAuthorization::new(None, None, true)
                .with_workspace_binding(Some(workspace));
            authorizations.push((
                "js available",
                base.clone().with_read_only_js(
                    sandbox.clone(),
                    available,
                    &crate::config::Config::default(),
                ),
            ));
            authorizations.push((
                "js unavailable",
                base.with_read_only_js(
                    sandbox,
                    crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
                        backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
                        assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
                        reason: "test: containment unavailable".to_string(),
                    },
                    &crate::config::Config::default(),
                ),
            ));
        }

        #[cfg(feature = "skills")]
        let services = {
            let paths = crate::paths::AppPaths::resolve(&crate::paths::PathEnvironment {
                platform: if cfg!(target_os = "macos") {
                    crate::paths::PathPlatform::MacOs
                } else if cfg!(target_os = "windows") {
                    crate::paths::PathPlatform::Windows
                } else {
                    crate::paths::PathPlatform::Linux
                },
                home_dir: None,
                config_base: Some(root.path().join("config")),
                data_base: Some(root.path().join("data")),
                local_data_base: Some(root.path().join("local")),
                state_base: Some(root.path().join("state")),
                cache_base: Some(root.path().join("cache")),
                workspace_root: None,
                overrides: Default::default(),
            })
            .unwrap();
            crate::extras::js::skills::session::SkillSessionServices::for_test(&paths)
        };
        #[cfg(feature = "skills")]
        let service_rows = [None, Some(&services)];
        #[cfg(not(feature = "skills"))]
        let service_rows = [()];

        #[allow(unused_variables)]
        for service in service_rows {
            for (label, authorization) in &authorizations {
                for tools in &narrowings {
                    let persona = super::PersonaExecution {
                        tools: tools.clone(),
                        ..Default::default()
                    };
                    let names = super::persona_tool_names(
                        authorization,
                        &persona,
                        #[cfg(feature = "skills")]
                        service.is_some(),
                    );
                    let instantiated = super::persona_tools(
                        authorization,
                        &persona,
                        1024 * 1024,
                        100,
                        100,
                        100,
                        Some(100),
                        #[cfg(feature = "skills")]
                        service,
                    )
                    .iter()
                    .map(|tool| tool.name())
                    .collect::<Vec<_>>();
                    assert_eq!(
                        names, instantiated,
                        "authorization {label}, narrowing {tools:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn report_repair_cannot_execute_a_requested_tool() {
        use futures::StreamExt;
        use rig::agent::AgentBuilder;
        use rig::streaming::StreamingPrompt;
        use rig::test_utils::{MockCompletionModel, MockExtensionsProbeTool, MockStreamEvent};
        use rig::tool::Tool;
        let probe = MockExtensionsProbeTool::default();
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::tool_call(
                "unexpected-repair-effect",
                MockExtensionsProbeTool::NAME,
                serde_json::json!({}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model)
            .tool(probe.clone())
            .add_hook(super::ReportRepairGuard)
            .build();
        let mut stream = agent
            .stream_prompt("Restate the previous report, without tool work.")
            .max_turns(1)
            .await;
        let mut failed = false;
        while let Some(item) = stream.next().await {
            if item.is_err() {
                failed = true;
                break;
            }
        }
        assert!(
            failed,
            "a tool call in the text-only repair must be refused"
        );
        assert!(probe.observed().is_none(), "repair must not execute a tool");
    }

    fn writable_parent(
        root: &Path,
        ask: Option<crate::permission::ask::AskSender>,
    ) -> SubagentAuthorization {
        let mut authorization = parent_authorization(root, ask);
        let configs = PermissionConfigs {
            glob: serde_json::from_value(
                serde_json::json!({"read":"allow", "edit":"ask", "write":"deny", "shell":"deny"}),
            )
            .unwrap(),
            ..Default::default()
        };
        authorization.permission = Some(Arc::new(Mutex::new(
            PermissionChecker::new(
                &configs,
                SecurityMode::Standard,
                Some(root.to_path_buf()),
                Some(vec!["standard".into()]),
            )
            .unwrap(),
        )));
        authorization.with_write_tools(Some(super::WriteChildTools {
            sandbox: crate::sandbox::Sandbox::new(false, "auto").with_workspace_binding(Arc::new(
                crate::paths::WorkspaceBinding::capture(root).unwrap(),
            )),
            write_enabled: true,
            edit_enabled: true,
            shell_enabled: true,
            max_text_file_size: Some(1024 * 1024),
            max_shell_output_lines: Some(100),
            shell_timeout_secs: 5,
            diff_options: Default::default(),
            formatter: None,
            #[cfg(feature = "lsp")]
            lsp: None,
        }))
    }

    #[tokio::test]
    async fn write_child_edit_uses_parent_approval_and_shared_read_proof() {
        let root = TempDir::new("write-approval");
        let path = root.path().join("child.txt");
        std::fs::write(&path, "before\n").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let authorization = writable_parent(root.path(), Some(tx));
        let tools = filesystem_tools(&authorization);
        let read = tools.iter().find(|tool| tool.name() == "read").unwrap();
        let edit = tools.iter().find(|tool| tool.name() == "edit").unwrap();
        read.call(serde_json::json!({"path":"child.txt"}).to_string())
            .await
            .unwrap();
        let edit = crate::permission::ask::scope_subagent("implementer".into(), true,
            edit.call(serde_json::json!({"path":"child.txt", "block":"<<<<<<< SEARCH\nbefore\n=======\nafter\n>>>>>>> REPLACE"}).to_string()));
        let approve = async {
            let request = rx.recv().await.unwrap().into_permission();
            assert_eq!(request.agent_name.as_deref(), Some("implementer"));
            assert_eq!(request.tool, "edit");
            assert_eq!(
                Path::new(&request.input),
                std::fs::canonicalize(&path).unwrap()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\n");
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(edit, approve)
        })
        .await
        .expect("child approval must settle");
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
        assert!(rx.try_recv().is_err());
        assert!(crate::permission::ask::subagent_name().is_none());
    }

    #[tokio::test]
    async fn write_child_denials_and_parent_tool_selection_stay_authoritative() {
        let root = TempDir::new("write-denied");
        let existing = root.path().join("existing.txt");
        std::fs::write(&existing, "before\n").unwrap();
        let authorization = writable_parent(root.path(), None);
        let tools = filesystem_tools(&authorization);
        assert!(tools.iter().all(|tool| tool.name() != "task"));
        let write = tools.iter().find(|tool| tool.name() == "write").unwrap();
        assert!(
            write
                .call(serde_json::json!({"path":"denied.txt", "content":"never"}).to_string())
                .await
                .is_err()
        );
        assert!(!root.path().join("denied.txt").exists());
        tools
            .iter()
            .find(|tool| tool.name() == "read")
            .unwrap()
            .call(serde_json::json!({"path":"existing.txt"}).to_string())
            .await
            .unwrap();
        let error = tools.iter().find(|tool| tool.name() == "edit").unwrap()
            .call(serde_json::json!({"path":"existing.txt", "block":"<<<<<<< SEARCH\nbefore\n=======\nafter\n>>>>>>> REPLACE"}).to_string()).await.unwrap_err();
        assert!(error.to_string().contains("Permission denied"), "{error}");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "before\n");
        let shell = tools.iter().find(|tool| tool.name() == "shell").unwrap();
        assert!(
            shell
                .call(serde_json::json!({"command":"exit 0"}).to_string())
                .await
                .is_err()
        );
        let mut narrowed = authorization;
        narrowed.write_tools.as_mut().unwrap().shell_enabled = false;
        assert!(
            filesystem_tools(&narrowed)
                .iter()
                .all(|tool| tool.name() != "shell")
        );
        let selected = narrowed.filesystem_tools(
            1024,
            100,
            100,
            100,
            Some(100),
            Some(&[crate::context::agents::AgentTool::Read]),
        );
        assert_eq!(
            selected.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            ["read"]
        );
    }

    fn tool_input(name: &str, path: &Path) -> String {
        match name {
            "read" => serde_json::json!({ "path": path }).to_string(),
            "grep" => serde_json::json!({ "pattern": "SUBAGENT_SECRET", "path": path }).to_string(),
            "find_files" => serde_json::json!({ "pattern": ".*", "path": path }).to_string(),
            "list_dir" => serde_json::json!({ "path": path }).to_string(),
            other => panic!("unexpected subagent filesystem tool: {other}"),
        }
    }

    #[tokio::test]
    async fn subagent_filesystem_permissions_deny_external_paths_for_every_child_tool() {
        let container = TempDir::new("all-tools");
        let workspace = container.path().join("workspace");
        let sibling = container.path().join("workspace-sibling");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("secret.txt"), "SUBAGENT_SECRET").unwrap();

        let authorization = parent_authorization(&workspace, None);
        let tools = filesystem_tools(&authorization);
        assert_eq!(
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["read", "grep", "find_files", "list_dir"],
            "child tool set must stay read-only and must not enable nested task"
        );

        for tool in tools {
            let target = if tool.name() == "read" {
                sibling.join("secret.txt")
            } else {
                sibling.clone()
            };
            let result = tool.call(tool_input(&tool.name(), &target)).await;
            let error = result.expect_err("external child path must inherit parent denial");
            let message = error.to_string();
            assert!(
                message.contains("Permission denied (non-interactive mode)"),
                "{} bypassed parent path policy: {message}",
                tool.name(),
            );
            assert!(
                !message.contains("SUBAGENT_SECRET"),
                "{} disclosed denied file content",
                tool.name(),
            );
        }
    }

    #[tokio::test]
    async fn subagent_relative_filesystem_tools_use_the_selected_workspace() {
        let container = TempDir::new("relative-workspace");
        let workspace = container.path().join("selected");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("child.txt"), "selected child workspace").unwrap();
        let authorization = parent_authorization(&workspace, None);
        let tools = filesystem_tools(&authorization);
        let list = tools
            .into_iter()
            .find(|tool| tool.name() == "list_dir")
            .unwrap()
            .call(serde_json::json!({ "path": "." }).to_string())
            .await
            .unwrap();

        assert!(list.contains("child.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subagent_filesystem_permissions_resolve_symlink_escape_before_asking() {
        let container = TempDir::new("symlink");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let secret = external.join("secret.txt");
        let link = workspace.join("link.txt");
        std::fs::write(&secret, "SUBAGENT_SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let authorization = parent_authorization(&workspace, Some(ask_tx));
        let read_tool = filesystem_tools(&authorization)
            .into_iter()
            .find(|tool| tool.name() == "read")
            .unwrap();

        let call = read_tool.call(tool_input("read", &link));
        let answer = async {
            let request = ask_rx
                .recv()
                .await
                .expect("symlink target approval request")
                .into_permission();
            assert_eq!(request.tool.as_str(), "read");
            assert_eq!(
                PathBuf::from(&request.input),
                std::fs::canonicalize(&secret).unwrap(),
                "approval must bind to the canonical symlink target"
            );
            request.reply.send(UserDecision::Deny).unwrap();
        };

        let (result, ()) = tokio::join!(call, answer);
        let message = result
            .expect_err("denied symlink target must not be read")
            .to_string();
        assert!(message.contains("Permission denied by user"));
        assert!(!message.contains("SUBAGENT_SECRET"));
    }

    #[tokio::test]
    async fn subagent_filesystem_permissions_isolate_concurrent_ask_replies_by_path() {
        let container = TempDir::new("concurrent");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let allowed = external.join("allowed.txt");
        let denied = external.join("denied.txt");
        std::fs::write(&allowed, "ALLOWED_CHILD_SENTINEL").unwrap();
        std::fs::write(&denied, "DENIED_CHILD_SENTINEL").unwrap();

        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(2);
        let authorization = parent_authorization(&workspace, Some(ask_tx));
        let mut first_tools = filesystem_tools(&authorization);
        let mut second_tools = filesystem_tools(&authorization);
        let first_read = first_tools.remove(0);
        let second_read = second_tools.remove(0);

        let first_call = first_read.call(tool_input("read", &allowed));
        let second_call = second_read.call(tool_input("read", &denied));
        let answer = async {
            for _ in 0..2 {
                let request = ask_rx
                    .recv()
                    .await
                    .expect("isolated child approval request")
                    .into_permission();
                let request_path = PathBuf::from(&request.input);
                let decision = if request_path == std::fs::canonicalize(&allowed).unwrap() {
                    UserDecision::AllowOnce
                } else {
                    assert_eq!(request_path, std::fs::canonicalize(&denied).unwrap());
                    UserDecision::Deny
                };
                request.reply.send(decision).unwrap();
            }
        };

        let (allowed_result, denied_result, ()) = tokio::join!(first_call, second_call, answer);
        assert!(
            allowed_result
                .expect("allowed child request")
                .contains("ALLOWED_CHILD_SENTINEL")
        );
        let denied_message = denied_result.expect_err("denied child request").to_string();
        assert!(denied_message.contains("Permission denied by user"));
        assert!(!denied_message.contains("DENIED_CHILD_SENTINEL"));
    }

    #[tokio::test]
    async fn subagent_filesystem_permissions_fail_closed_when_ask_channel_closes() {
        let container = TempDir::new("closed-channel");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let secret = external.join("secret.txt");
        std::fs::write(&secret, "SUBAGENT_SECRET").unwrap();

        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
        drop(ask_rx);
        let authorization = parent_authorization(&workspace, Some(ask_tx));
        let read_tool = filesystem_tools(&authorization).remove(0);
        let error = read_tool
            .call(tool_input("read", &secret))
            .await
            .expect_err("closed approval channel must deny child read")
            .to_string();

        assert!(error.contains("Permission system unavailable"));
        assert!(!error.contains("SUBAGENT_SECRET"));
    }
}

#[cfg(test)]
mod provider_param_tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use rig::completion::{Completion, CompletionModel};

    use super::{
        PersonaExecution, SubagentAuthorization, build_explore_agent_inner,
        subagent_provider_params,
    };
    use crate::config::{
        ApiStyle, Config, CustomProviderConfig, ReasoningConfig, ReasoningEffort, ReasoningSummary,
    };
    use crate::provider::{AnyModel, OpenAiModel, create_client};

    /// Accepts one HTTP request, returns its JSON body, and answers 400 so the
    /// client fails fast without needing a provider-shaped response.
    fn capture_one_request(listener: TcpListener) -> mpsc::Receiver<serde_json::Value> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..count]);
                let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .expect("request must include Content-Length");
                break (end + 4, length);
            };
            while request.len() < header_end + content_length {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "request ended before its body");
                request.extend_from_slice(&buffer[..count]);
            }
            let body =
                serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
            tx.send(body).unwrap();
            let _ = stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        rx
    }

    fn zdr_config() -> Config {
        Config {
            extra_body: Some(serde_json::json!({"user": "zdr-tenant", "seed": 1})),
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                summary: Some(ReasoningSummary::Concise),
                encrypted_content: None,
                store: Some(false),
            }),
            ..Config::default()
        }
    }

    fn openai_model(style: ApiStyle) -> (AnyModel, mpsc::Receiver<serde_json::Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = capture_one_request(listener);
        let custom = HashMap::from([(
            "openai-capture".to_string(),
            CustomProviderConfig {
                provider_type: "openai".into(),
                inherit_builtin_key: false,
                base_url: format!("http://{address}/v1"),
                api_key_env: None,
                danger_accept_invalid_certs: None,
                api_style: Some(style),
                headers: HashMap::new(),
                timeout_secs: None,
                connect_timeout_secs: None,
                stream_idle_timeout_secs: None,
                model: None,
                prompt_caching: false,
                model_prices: std::collections::HashMap::new(),
            },
        )]);
        let client = create_client("openai-capture", Some("test-key"), &custom, None).unwrap();
        (client.completion_model("gpt-test"), requests)
    }

    async fn send_child_request<M: CompletionModel + 'static>(
        model: M,
        params: Option<serde_json::Value>,
        persona: &PersonaExecution,
        requests: mpsc::Receiver<serde_json::Value>,
    ) -> serde_json::Value {
        let agent = build_explore_agent_inner(
            model,
            crate::context::instructions::InstructionProtocol::Unverified,
            2,
            16_384,
            262_144,
            1024 * 1024,
            1_000,
            1_000,
            1_000,
            Some(1_000),
            &SubagentAuthorization::new(None, None, true),
            params,
            #[cfg(feature = "archmd")]
            None,
            None,
            persona,
            #[cfg(feature = "skills")]
            None,
        );
        let result = agent
            .completion("inspect", Vec::<rig::completion::Message>::new())
            .await
            .expect("request builder")
            .send()
            .await;
        assert!(result.is_err(), "capture server always answers 400");
        requests.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    fn persona() -> PersonaExecution {
        let config = zdr_config();
        PersonaExecution {
            write_mode: false,
            tools: None,
            provider_extra_body: config.extra_body.clone(),
            additional_params: Some(serde_json::json!({"user": "persona-tenant"})),
            #[cfg(feature = "mcp")]
            mcp_servers: None,
        }
    }

    #[tokio::test]
    async fn responses_subagent_request_keeps_store_false_reasoning_and_extra_body() {
        let config = zdr_config();
        let persona = persona();
        let (model, requests) = openai_model(ApiStyle::Responses);
        let params =
            subagent_provider_params(&model, &config, persona.provider_extra_body.clone(), "c1");
        let AnyModel::OpenAI(OpenAiModel::Responses(model)) = model else {
            panic!("expected a Responses model");
        };

        let body = send_child_request(model, params, &persona, requests).await;

        assert_eq!(body["store"], false, "{body}");
        assert_eq!(body["reasoning"]["effort"], "high", "{body}");
        assert_eq!(body["reasoning"]["summary"], "concise", "{body}");
        assert!(body["prompt_cache_key"].is_string(), "{body}");
        // Persona params win over inherited provider params.
        assert_eq!(body["user"], "persona-tenant", "{body}");
    }

    #[tokio::test]
    async fn completions_subagent_request_keeps_reasoning_effort_and_extra_body() {
        let config = zdr_config();
        let persona = persona();
        let (model, requests) = openai_model(ApiStyle::Completions);
        let params =
            subagent_provider_params(&model, &config, persona.provider_extra_body.clone(), "c1");
        let AnyModel::OpenAI(OpenAiModel::Completions(model)) = model else {
            panic!("expected a Chat Completions model");
        };

        let body = send_child_request(model, params, &persona, requests).await;

        assert_eq!(body["reasoning_effort"], "high", "{body}");
        assert!(body.get("reasoning").is_none(), "{body}");
        // Inherited user extra_body survives; persona params win on collision.
        assert_eq!(body["seed"], 1, "{body}");
        assert_eq!(body["user"], "persona-tenant", "{body}");
    }

    #[test]
    fn other_providers_inherit_extra_body_and_openrouter_keeps_routing() {
        let config = zdr_config();
        let extra = Some(serde_json::json!({"user": "zdr-tenant"}));
        for provider in ["anthropic", "gemini", "ollama"] {
            let client = create_client(provider, Some("test-key"), &HashMap::new(), None).unwrap();
            let model = client.completion_model("model");
            assert_eq!(
                subagent_provider_params(&model, &config, extra.clone(), "c1"),
                extra,
                "{provider}"
            );
        }

        let client = create_client("openrouter", Some("test-key"), &HashMap::new(), None).unwrap();
        let model = client.completion_model("anthropic/claude-sonnet-4.6");
        let params = subagent_provider_params(&model, &config, extra.clone(), "c1").unwrap();
        assert_eq!(params["user"], "zdr-tenant");
        assert_eq!(
            params["provider"]["order"],
            serde_json::json!(["Anthropic"])
        );
    }
}

#[cfg(test)]
mod settings127_context_tests {
    use super::ChildContextGuard;
    use futures::StreamExt;
    use rig::agent::AgentBuilder;
    use rig::streaming::StreamingChat;
    use rig::test_utils::{MockCompletionModel, MockExtensionsProbeTool, MockStreamEvent};

    #[tokio::test]
    async fn subagent130_direct_builder_capacity_drives_prepared_request_admission() {
        let mut cfg = crate::config::Config {
            context_window: Some(1_000_000),
            ..Default::default()
        };
        cfg.quick_models = serde_json::from_value(serde_json::json!({
            "child": {"provider":"openrouter", "model":"known-child", "context_window":1048576},
            "foreign": {"provider":"custom-alias", "model":"unknown-child", "context_window":1048576}
        })).unwrap();
        let client = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        for (name, expected_window, expected_calls) in
            [("unknown-child", 131_072, 0), ("known-child", 1_048_576, 1)]
        {
            let selected = client.completion_model(name);
            let window = super::child_context_window(&selected, &cfg);
            assert_eq!(window, expected_window);
            let model = MockCompletionModel::from_stream_turns(vec![vec![
                MockStreamEvent::text("ok"),
                MockStreamEvent::final_response_with_default_usage(),
            ]]);
            // This request exceeds the old 256-Ki child fallback. The main
            // session's 1M setting must not make the unknown child accept it.
            let preamble = "context ".repeat(150_000);
            assert!(crate::session::Session::estimate_tokens(&preamble) > 262_144);
            let agent = AgentBuilder::new(model.clone())
                .preamble(&preamble)
                .max_tokens(32_000)
                .add_hook(ChildContextGuard { window })
                .build();
            let mut stream = agent
                .stream_chat("brief", Vec::<rig::completion::Message>::new())
                .await;
            let mut failed = false;
            while let Some(item) = stream.next().await {
                failed |= item.is_err();
            }
            assert_eq!(model.request_count(), expected_calls);
            assert_eq!(failed, expected_calls == 0);
        }
        // This is the already-clamped value owned by TaskTool admission,
        // including a custom-provider/persona alias unavailable in AnyModel.
        cfg.subagent_context_window = Some(77_000);
        assert_eq!(
            super::child_context_window(&client.completion_model("known-child"), &cfg),
            77_000
        );
    }

    /// Observes the shared estimate after both prepared-request hooks ran.
    #[derive(Clone, Default)]
    struct SerializationProbe(std::sync::Arc<std::sync::Mutex<Vec<u64>>>);
    impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for SerializationProbe {
        async fn on_event(
            &self,
            ctx: &rig::agent::HookContext,
            event: rig::agent::StepEvent<'_, M>,
        ) -> rig::agent::Flow {
            if let rig::agent::StepEvent::CompletionRequestPrepared { .. } = event {
                let count = ctx
                    .scratchpad()
                    .get::<crate::agent::runner::PreparedRequestEstimate>()
                    .map_or(0, |estimate| estimate.serializations);
                self.0.lock().unwrap().push(count);
            }
            rig::agent::Flow::cont()
        }
        fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
            kind == rig::agent::StepEventKind::CompletionRequestPrepared
        }
    }

    #[tokio::test]
    async fn prepared_request_is_serialized_once_for_context_guard_and_usage_ledger() {
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("probe-1", "context_probe", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("ok"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .preamble("small")
            .max_tokens(1024)
            .tool(MockExtensionsProbeTool::default())
            .default_max_turns(4)
            .add_hook(ChildContextGuard { window: 1_000_000 })
            .build();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
        let admission = crate::agent::runner::SubagentAdmission::new(|| true, {
            let observed = observed.clone();
            move |_, fallback| {
                observed
                    .lock()
                    .unwrap()
                    .push((fallback.input_tokens, fallback.output_tokens))
            }
        });
        let ledger = crate::agent::runner::SharedUsageLedger::default().with_admission(admission);
        let probe = SerializationProbe::default();
        let mut stream = agent
            .stream_chat("brief", Vec::<rig::completion::Message>::new())
            .add_hook(ledger.clone())
            .add_hook(probe.clone())
            .await;
        while let Some(item) = stream.next().await {
            match item {
                Ok(rig::agent::MultiTurnStreamItem::CompletionCall(call)) => {
                    ledger.record_completion(call.usage)
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected stream error: {error}"),
            }
        }
        assert_eq!(model.request_count(), 2);
        // One serialization per prepared request, shared by both hooks.
        assert_eq!(*probe.0.lock().unwrap(), vec![1, 2]);
        // The ledger's text fallback is the same estimate the guard admitted.
        let requests = model.requests();
        let fallbacks = observed.lock().unwrap().clone();
        assert_eq!(fallbacks.len(), 2);
        for ((input, reserved), request) in fallbacks.into_iter().zip(requests.iter()) {
            assert_eq!(
                input,
                crate::session::Session::estimate_tokens(&serde_json::to_string(request).unwrap())
            );
            assert_eq!(reserved, 1024);
        }
    }

    #[tokio::test]
    async fn incremental_prepared_estimate_equals_full_serialization_across_turns() {
        // Wide characters in the preamble and a growing history: every call's
        // incremental estimate must equal the full-serialization estimate.
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("inc-1", "context_probe", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call("inc-2", "context_probe", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("日本語の応答 ok"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .preamble("日本語のプロンプト small")
            .max_tokens(2048)
            .tool(MockExtensionsProbeTool::default())
            .default_max_turns(6)
            .add_hook(ChildContextGuard { window: 1_000_000 })
            .build();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let admission = crate::agent::runner::SubagentAdmission::new(|| true, {
            let observed = observed.clone();
            move |_, fallback| observed.lock().unwrap().push(fallback.input_tokens)
        });
        let ledger = crate::agent::runner::SharedUsageLedger::default().with_admission(admission);
        let probe = SerializationProbe::default();
        let mut stream = agent
            .stream_chat("brief", Vec::<rig::completion::Message>::new())
            .add_hook(ledger.clone())
            .add_hook(probe.clone())
            .await;
        while let Some(item) = stream.next().await {
            match item {
                Ok(rig::agent::MultiTurnStreamItem::CompletionCall(call)) => {
                    ledger.record_completion(call.usage)
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected stream error: {error}"),
            }
        }
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        let fallbacks = observed.lock().unwrap().clone();
        assert_eq!(fallbacks.len(), 3);
        for (input, request) in fallbacks.into_iter().zip(requests.iter()) {
            assert_eq!(
                input,
                crate::session::Session::estimate_tokens(&serde_json::to_string(request).unwrap())
            );
        }
        assert_eq!(*probe.0.lock().unwrap(), vec![1, 2, 3]);
    }

    /// Re-runs the incremental estimate against edited copies of each prepared
    /// request: a mutated middle message and a different preamble must be
    /// re-measured, and the original must still match afterwards.
    #[derive(Clone, Default)]
    struct InvalidationProbe(std::sync::Arc<std::sync::Mutex<u32>>);
    impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for InvalidationProbe {
        async fn on_event(
            &self,
            ctx: &rig::agent::HookContext,
            event: rig::agent::StepEvent<'_, M>,
        ) -> rig::agent::Flow {
            if let rig::agent::StepEvent::CompletionRequestPrepared { request, .. } = event {
                let full = |request: &rig::completion::CompletionRequest| {
                    crate::session::Session::estimate_tokens(
                        &serde_json::to_string(request).unwrap(),
                    )
                };
                let incremental = |request: &rig::completion::CompletionRequest| {
                    crate::agent::runner::incremental_request_estimate(ctx, request).unwrap()
                };
                assert_eq!(incremental(request), full(request));
                if request.chat_history.len() >= 3 {
                    let mut edited = request.clone();
                    *edited.chat_history.iter_mut().nth(1).unwrap() =
                        rig::completion::Message::user("edited 日本語 middle message ".repeat(40));
                    assert_ne!(full(&edited), full(request));
                    assert_eq!(incremental(&edited), full(&edited));
                    let mut other_shell = request.clone();
                    other_shell.preamble = Some("a different, longer preamble 日本".repeat(30));
                    assert_ne!(full(&other_shell), full(request));
                    assert_eq!(incremental(&other_shell), full(&other_shell));
                    assert_eq!(incremental(request), full(request));
                    *self.0.lock().unwrap() += 1;
                }
            }
            rig::agent::Flow::cont()
        }
        fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
            kind == rig::agent::StepEventKind::CompletionRequestPrepared
        }
    }

    #[tokio::test]
    async fn incremental_estimate_remeasures_edited_prefix_and_changed_shell() {
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call("inv-1", "context_probe", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call("inv-2", "context_probe", serde_json::json!({})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model.clone())
            .preamble("preamble")
            .max_tokens(512)
            .tool(MockExtensionsProbeTool::default())
            .default_max_turns(6)
            .build();
        let probe = InvalidationProbe::default();
        let mut stream = agent
            .stream_chat("brief", Vec::<rig::completion::Message>::new())
            .add_hook(probe.clone())
            .await;
        while let Some(item) = stream.next().await {
            if let Err(error) = item {
                panic!("unexpected stream error: {error}");
            }
        }
        assert!(*probe.0.lock().unwrap() >= 2);
    }

    #[tokio::test]
    async fn subagent127_prepared_context_counts_history_tools_and_reserved_output_before_dispatch()
    {
        for (window, max_tokens, preamble, history, expected_calls) in [
            (4096, 1024, "small".to_string(), "small".to_string(), 1),
            (4096, 4096, "small".to_string(), "small".to_string(), 0),
            (
                4096,
                1024,
                "LARGE_CONTEXT ".repeat(3000),
                "small".to_string(),
                0,
            ),
            (
                4096,
                1024,
                "small".to_string(),
                "LARGE_HISTORY ".repeat(3000),
                0,
            ),
        ] {
            let model = MockCompletionModel::from_stream_turns(vec![vec![
                MockStreamEvent::text("ok"),
                MockStreamEvent::final_response_with_default_usage(),
            ]]);
            let agent = AgentBuilder::new(model.clone())
                .preamble(&preamble)
                .max_tokens(max_tokens)
                .tool(MockExtensionsProbeTool::default())
                .add_hook(ChildContextGuard { window })
                .build();
            let mut stream = agent
                .stream_chat("brief", vec![rig::completion::Message::user(history)])
                .await;
            let mut failed = false;
            while let Some(item) = stream.next().await {
                failed |= item.is_err();
            }
            assert_eq!(model.request_count(), expected_calls);
            assert_eq!(failed, expected_calls == 0);
            if expected_calls == 1 {
                assert_eq!(model.requests()[0].tools.len(), 1);
            }
        }
    }
}
