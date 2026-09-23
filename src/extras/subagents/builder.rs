use crate::agent::tools;
use crate::context::agents::AgentTool;
use crate::extras::subagents::prompt;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::provider::{AnyAgent, AnyAgentInner, AnyModel, OpenAiAgent, OpenAiModel};
use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;

/// The parent session's authorization context inherited by a subagent.
///
/// Keeping the checker and approval channel together makes it impossible to
/// construct one of the child filesystem tools with only half of the parent's
/// authorization path. Nested subagents are deliberately unsupported because
/// the child tool set does not contain `TaskTool`; if nesting is added later,
/// this same context must be inherited rather than replaced.
#[derive(Clone)]
pub(crate) struct SubagentAuthorization {
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    deny_repeated_reads: bool,
    #[cfg(feature = "js")]
    read_only_js: Option<ReadOnlyJsAuthorization>,
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
    let reasoning = cfg.reasoning.as_ref();
    match model {
        AnyModel::OpenRouter(_, routing) => {
            crate::provider::merge_extra_body(routing.clone(), provider_extra_body)
        }
        AnyModel::OpenAI(OpenAiModel::Responses(_)) => {
            crate::provider::openai_responses_extra_body(provider_extra_body, session_id, reasoning)
        }
        AnyModel::OpenAI(OpenAiModel::Completions(_)) => {
            crate::provider::openai_completions_extra_body(provider_extra_body, reasoning)
        }
        AnyModel::Anthropic(_) | AnyModel::Gemini(_) | AnyModel::Ollama(_) => provider_extra_body,
    }
}

impl SubagentAuthorization {
    pub(crate) fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        deny_repeated_reads: bool,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            workspace: None,
            deny_repeated_reads,
            #[cfg(feature = "js")]
            read_only_js: None,
        }
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    ) -> Self {
        self.workspace = workspace;
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

    #[cfg(feature = "js")]
    fn read_only_js_tool(
        &self,
        #[cfg(feature = "skills")] skill_services: Option<
            &std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
        >,
    ) -> Option<Box<dyn rig::tool::ToolDyn>> {
        let authorization = self.read_only_js.as_ref()?;
        if matches!(
            authorization.containment_status,
            crate::sandbox::worker::WorkerContainmentStatus::Unavailable { .. }
        ) {
            return None;
        }
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

    pub(crate) fn filesystem_tools(
        &self,
        max_text_file_size: u64,
        max_read_lines: u64,
        max_grep_results: u64,
        max_find_results: u64,
        max_list_dir_entries: Option<u64>,
        selected: Option<&[AgentTool]>,
    ) -> Vec<Box<dyn rig::tool::ToolDyn>> {
        let read_tracker = tools::ReadTracker::new(self.deny_repeated_reads);
        let read = tools::ReadTool::new_with_tracker(
            self.permission.clone(),
            self.ask_tx.clone(),
            Some(max_text_file_size),
            max_read_lines,
            read_tracker,
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
        let enabled = |tool| selected.is_none_or(|selected| selected.contains(&tool));
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
        tools
    }
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

#[allow(clippy::too_many_arguments)]
fn build_explore_agent_inner<M: CompletionModel + 'static>(
    model: M,
    max_turns: usize,
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
    persona: &PersonaExecution,
    #[cfg(feature = "skills")] skill_services: Option<
        &std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) -> Agent<M> {
    let suffix = crate::session::storage::load_suffix();
    let preamble = build_explore_preamble(
        #[cfg(feature = "archmd")]
        architecture,
        specialization,
        suffix.as_deref(),
    );

    let tools = authorization.filesystem_tools(
        max_text_file_size,
        max_read_lines,
        max_grep_results,
        max_find_results,
        max_list_dir_entries,
        persona.tools.as_deref(),
    );
    #[cfg(feature = "memory")]
    let tools = {
        let mut tools = tools;
        if persona.tools.as_ref().is_none_or(|selected| {
            selected.contains(&AgentTool::MemoryRead) || selected.contains(&AgentTool::MemorySearch)
        }) {
            let selected = persona.tools.as_deref();
            tools.extend(
                subagent_memory_tools(authorization)
                    .into_iter()
                    .zip([AgentTool::MemoryRead, AgentTool::MemorySearch])
                    .filter_map(|(tool, kind)| {
                        selected
                            .is_none_or(|selected| selected.contains(&kind))
                            .then_some(tool)
                    }),
            );
        }
        tools
    };
    #[cfg(feature = "skills")]
    let tools = {
        let mut tools = tools;
        if persona
            .tools
            .as_ref()
            .is_none_or(|selected| selected.contains(&AgentTool::SkillsSearch))
            && let Some(services) = skill_services
        {
            tools.push(Box::new(
                crate::extras::js::skills::search_tool::SkillsSearchTool::new(
                    std::sync::Arc::clone(services),
                ),
            ));
        }
        tools
    };
    #[cfg(feature = "js")]
    let tools = {
        let mut tools = tools;
        if persona
            .tools
            .as_ref()
            .is_none_or(|selected| selected.contains(&AgentTool::Js))
            && let Some(tool) = authorization.read_only_js_tool(
                #[cfg(feature = "skills")]
                skill_services,
            )
        {
            tools.push(tool);
        }
        tools
    };
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
        .tools(tools)
        .add_hook(crate::agent::runner::ToolLoopGuard);
    #[cfg(feature = "skills")]
    if let Some(services) = skill_services {
        builder = builder.add_hook(crate::extras::js::skills::session::SkillContextHook::new(
            preamble.clone(),
            std::sync::Arc::clone(services),
        ));
    }
    builder = builder.add_hook(crate::agent::runner::ToolResultSpillHook::new(
        uuid::Uuid::new_v4().to_string(),
        None,
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_explore_agent(
    model: AnyModel,
    max_turns: usize,
    cfg: &crate::config::Config,
    authorization: SubagentAuthorization,
    #[cfg(feature = "archmd")] architecture: Option<String>,
    specialization: Option<String>,
    persona: PersonaExecution,
    #[cfg(feature = "skills")] skill_services: Option<
        std::sync::Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) -> AnyAgent {
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
    let inner = match model {
        AnyModel::OpenRouter(m, _) => AnyAgentInner::OpenRouter(build_explore_agent_inner(
            m,
            max_turns,
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
            &persona,
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
        AnyModel::OpenAI(m) => AnyAgentInner::OpenAI(match m {
            OpenAiModel::Responses(m) => OpenAiAgent::Responses(build_explore_agent_inner(
                m,
                max_turns,
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
                &persona,
                #[cfg(feature = "skills")]
                skill_services.as_ref(),
            )),
            OpenAiModel::Completions(m) => OpenAiAgent::Completions(build_explore_agent_inner(
                m,
                max_turns,
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
                &persona,
                #[cfg(feature = "skills")]
                skill_services.as_ref(),
            )),
        }),
        AnyModel::Anthropic(m) => AnyAgentInner::Anthropic(build_explore_agent_inner(
            m,
            max_turns,
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
            &persona,
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
        AnyModel::Gemini(m) => AnyAgentInner::Gemini(build_explore_agent_inner(
            m,
            max_turns,
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
            &persona,
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
        AnyModel::Ollama(m) => AnyAgentInner::Ollama(build_explore_agent_inner(
            m,
            max_turns,
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
            &persona,
            #[cfg(feature = "skills")]
            skill_services.as_ref(),
        )),
    };
    AnyAgent::with_runtime(
        inner,
        #[cfg(feature = "skills")]
        skill_services,
    )
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

    #[tokio::test]
    async fn subagent_without_an_authorized_worker_context_omits_js() {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("subagent"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = build_explore_agent_inner(
            model,
            2,
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
            2,
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
            2,
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
            2,
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
            2,
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
    use std::sync::atomic::{AtomicU64, Ordering};
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

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "mini-agent-subagent-permission-{}-{tag}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&path).expect("create subagent permission test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
                .expect("symlink target approval request");
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
                    .expect("isolated child approval request");
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
                base_url: format!("http://{address}/v1"),
                api_key_env: None,
                danger_accept_invalid_certs: None,
                api_style: Some(style),
                headers: HashMap::new(),
                timeout_secs: None,
                connect_timeout_secs: None,
                stream_idle_timeout_secs: None,
                model: None,
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
            2,
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
            tools: None,
            provider_extra_body: config.extra_body.clone(),
            additional_params: Some(serde_json::json!({"user": "persona-tenant"})),
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
