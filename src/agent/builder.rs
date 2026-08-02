use std::collections::HashSet;
#[cfg(feature = "skills")]
use std::sync::Arc;

use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use smallvec::SmallVec;

use crate::agent::prompt::{SYSTEM_PROMPT, TODO_TOOLS_PROMPT};
use crate::agent::tools;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::Sandbox;

/// Assemble the system-prompt preamble every request carries: the base
/// `SYSTEM_PROMPT`, tool-use guidance, context files (AGENTS.md, ARCHITECTURE.md,
/// active mode prompt), working directory, `/add`ed files, memory, and the user
/// `SUFFIX.md`. Extracted from [`build_agent_inner`] so its token cost can be
/// estimated (see [`estimate_overhead`]) without building an `Agent`.
pub fn build_preamble(context: &ContextFiles, reasoning_enabled: bool) -> String {
    let reasoning_prefix = if reasoning_enabled {
        "You reason carefully and think step-by-step.\n\n"
    } else {
        "You respond concisely without showing your reasoning.\n\n"
    };
    let suffix = crate::session::storage::load_suffix();
    let context_agents = context.agents.as_deref().unwrap_or("");
    #[cfg(feature = "archmd")]
    let context_architecture = context.architecture.as_deref().unwrap_or("");
    let context_prompt = context.current_prompt.as_deref().unwrap_or("");
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let total_len = reasoning_prefix.len()
        + SYSTEM_PROMPT.len()
        + 1
        + TODO_TOOLS_PROMPT.len()
        + if context.agents.is_some() {
            2 + context_agents.len()
        } else {
            0
        }
        + if context.current_prompt.is_some() {
            6 + context_prompt.len()
        } else {
            0
        }
        + if !cwd.is_empty() { 30 + cwd.len() } else { 0 };

    #[cfg(feature = "archmd")]
    let total_len = total_len
        + if context.architecture.is_some() {
            2 + context_architecture.len()
        } else {
            0
        };

    #[cfg(feature = "memory")]
    let total_len = total_len
        + context.memory.as_deref().map_or(0, |m| m.len() + 8) // "\n\n---\n\n" + content
        + crate::agent::prompt::MEMORY_TOOLS_PROMPT.len();

    let total_len = total_len + suffix.as_ref().map_or(0, |s| s.len() + 6); // "\n\n---\n\n"

    // Add extra files content to preamble budget. Cap each file to prevent a
    // huge file from blowing up the system prompt past the context window.
    const MAX_EXTRA_FILE_BYTES: usize = 524_288;
    let extra_files_content: Vec<String> = context
        .extra_files
        .iter()
        .filter_map(|p| {
            let content = std::fs::read_to_string(p).ok()?;
            let truncated = if content.len() > MAX_EXTRA_FILE_BYTES {
                tracing::warn!(
                    "extra file {} exceeds {} bytes, truncated for preamble",
                    p.display(),
                    MAX_EXTRA_FILE_BYTES
                );
                let mut end = MAX_EXTRA_FILE_BYTES;
                while !content.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                let mut t = content[..end].to_string();
                t.push_str("\n\n[truncated — file exceeded preamble size limit]");
                t
            } else {
                content
            };
            Some(format!("Content of {}:\n{}", p.display(), truncated))
        })
        .collect();
    let extra_files_len: usize = extra_files_content.iter().map(|s| s.len() + 2).sum();
    let total_len = total_len + extra_files_len;

    let mut preamble = String::with_capacity(total_len);
    preamble.push_str(reasoning_prefix);
    preamble.push_str(SYSTEM_PROMPT);
    preamble.push('\n');
    preamble.push_str(TODO_TOOLS_PROMPT);
    if !context_agents.is_empty() {
        preamble.push_str("\n\n");
        preamble.push_str(context_agents);
    }
    #[cfg(feature = "archmd")]
    if !context_architecture.is_empty() {
        preamble.push_str("\n\n");
        preamble.push_str(context_architecture);
    }
    if !context_prompt.is_empty() {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(context_prompt);
    }
    if !cwd.is_empty() {
        preamble.push_str("\n\nCurrent working directory: ");
        preamble.push_str(&cwd);
    }
    for content in &extra_files_content {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(content);
    }
    #[cfg(feature = "memory")]
    {
        crate::extras::memory::append_memory_block(&mut preamble, context.memory.as_deref());
        preamble.push_str(crate::agent::prompt::MEMORY_TOOLS_PROMPT);
    }
    if let Some(s) = &suffix {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(s);
    }
    preamble
}

/// Estimate the token cost of the fixed request overhead (the preamble from
/// [`build_preamble`]). Stored on the session and added to the context figure
/// before the first real calibration. Does not yet include tool-schema tokens;
/// the provider's first usage report folds those into the calibration anchor.
pub fn estimate_overhead(context: &ContextFiles, reasoning_enabled: bool) -> u64 {
    crate::session::Session::estimate_tokens(&build_preamble(context, reasoning_enabled))
}

/// Retain only the tools whose names appear in `allowlist`. An empty
/// allowlist passes everything through unchanged. Unrecognized names are
/// logged as warnings and ignored.
pub(crate) fn filter_tools_by_allowlist(
    tools: Vec<Box<dyn rig::tool::ToolDyn>>,
    allowlist: &[String],
) -> Vec<Box<dyn rig::tool::ToolDyn>> {
    if allowlist.is_empty() {
        return tools;
    }
    let allowed: HashSet<&str> = allowlist.iter().map(|s| s.as_str()).collect();
    for name in &allowed {
        if !tools.iter().any(|t| t.name() == *name) {
            tracing::warn!("--tools: unknown tool '{name}' (ignored)");
        }
    }
    tools
        .into_iter()
        .filter(|t| allowed.contains(t.name().as_str()))
        .collect()
}

#[cfg(feature = "js")]
fn register_js_tool(
    tools: &mut Vec<Box<dyn rig::tool::ToolDyn>>,
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    cfg: &Config,
    #[cfg(feature = "skills")] skill_turn_context: Option<
        Arc<crate::extras::js::skills::turn::SkillTurnContext>,
    >,
) {
    use crate::extras::js::host::AllowConfig;
    use crate::extras::js::tool::JsTool;

    let startup_base = std::env::current_dir().unwrap_or_default();
    let allow_config = AllowConfig::from_settings(
        &startup_base,
        cfg.js_file_base_dir.as_deref(),
        cfg.js_read_roots.as_deref(),
        cfg.js_write_roots.as_deref(),
        cfg.js_read_unrestricted.unwrap_or(false),
        cfg.js_write_unrestricted.unwrap_or(false),
    )
    .with_fetch_settings(
        cfg.js_fetch_origins.as_deref(),
        cfg.js_fetch_allow_http.unwrap_or(false),
    );
    #[cfg(feature = "skills")]
    let mut js_tool = {
        #[cfg(not(test))]
        {
            use crate::extras::js::skills::admission::{AdmissionEvaluator, AdmissionWorker};
            use crate::extras::js::skills::embed::Embedder;
            use crate::extras::js::skills::proposal::ProposalQueue;
            use crate::extras::js::skills::store::SkillStore;
            use crate::extras::js::skills::telemetry::TelemetryDispatcher;
            use crate::paths::AppPaths;
            use std::time::Duration;

            let workers = (|| -> Result<_, String> {
                let paths = AppPaths::from_process(Some(startup_base))
                    .map_err(|error| error.to_string())?;
                let proposal_store =
                    SkillStore::open_at(&paths).map_err(|error| error.to_string())?;
                let evaluator_store =
                    SkillStore::open_at(&paths).map_err(|error| error.to_string())?;
                let embedder = Embedder::from_config(cfg.embedding.as_ref())
                    .map_err(|error| error.to_string())?;
                let telemetry_embedder = std::sync::Arc::new(
                    Embedder::from_config(cfg.embedding.as_ref())
                        .map_err(|error| error.to_string())?,
                );
                let (coordinator, _) =
                    crate::extras::js::skills::turn::shared_coordinator(&paths, telemetry_embedder)
                        .map_err(|error| error.to_string())?;
                let telemetry = TelemetryDispatcher::spawn_with_coordinator(&paths, coordinator)
                    .map_err(|error| error.to_string())?;
                let evaluator = AdmissionEvaluator::new(
                    evaluator_store,
                    embedder,
                    format!("mini-agent-{}", std::process::id()),
                )
                .map_err(|error| error.to_string())?;
                let admission_worker =
                    AdmissionWorker::start(evaluator).map_err(|error| error.to_string())?;
                let proposal_worker =
                    ProposalQueue::start_store_worker(proposal_store, 16, Duration::from_secs(2))
                        .map_err(|error| error.to_string())?;
                Ok((proposal_worker, admission_worker, telemetry))
            })();
            match workers {
                Ok((proposal_worker, admission_worker, telemetry)) => {
                    JsTool::new_with_skill_workers(
                        sandbox,
                        permission,
                        ask_tx,
                        allow_config,
                        proposal_worker,
                        admission_worker,
                    )
                    .with_telemetry(telemetry)
                }
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        "skill proposal storage unavailable; propose_skill is disabled"
                    );
                    JsTool::new(sandbox, permission, ask_tx, allow_config)
                }
            }
        }

        #[cfg(test)]
        {
            let _ = startup_base;
            JsTool::new(sandbox, permission, ask_tx, allow_config)
        }
    };

    #[cfg(not(feature = "skills"))]
    let js_tool = JsTool::new(sandbox, permission, ask_tx, allow_config);

    #[cfg(feature = "skills")]
    if let Some(context) = skill_turn_context {
        js_tool = js_tool.with_skill_turn_context(context);
    }

    tools.push(Box::new(js_tool));
}

#[allow(clippy::too_many_arguments)]
pub async fn build_agent_inner<M: CompletionModel + 'static>(
    model: M,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    sandbox: Sandbox,
    reasoning_enabled: bool,
    temperature: Option<f64>,
    // Provider-specific extra body params (e.g. OpenRouter `provider.order` to
    // pin Claude to the Anthropic direct route so `cache_control` is honored).
    // `None` for providers that need no extra routing.
    additional_params: Option<serde_json::Value>,
    #[cfg(feature = "skills")] skill_turn_context: Option<
        Arc<crate::extras::js::skills::turn::SkillTurnContext>,
    >,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
) -> Agent<M> {
    #[cfg(feature = "lsp")]
    let lsp_manager = if cli.resolve_no_tools(cfg) {
        None
    } else {
        cfg.resolve_lsp().map(|c| {
            crate::extras::lsp::LspManager::new(c, std::env::current_dir().unwrap_or_default())
        })
    };

    #[cfg_attr(not(feature = "lsp"), allow(unused_mut))]
    let mut preamble = build_preamble(context, reasoning_enabled);
    #[cfg(feature = "lsp")]
    if lsp_manager.is_some() {
        preamble.push_str(crate::agent::prompt::LSP_PROMPT);
    }

    let mut builder = AgentBuilder::new(model).preamble(&preamble);

    if let Some(params) = additional_params {
        builder = builder.additional_params(params);
    }

    let max_tokens = cli.resolve_max_tokens(cfg);
    builder = builder.max_tokens(max_tokens);

    let max_turns = cli.resolve_max_agent_turns(cfg);
    builder = builder.default_max_turns(max_turns);

    if let Some(temp) = temperature {
        builder = builder.temperature(temp);
    }

    if cli.resolve_no_tools(cfg) {
        builder.build()
    } else {
        let max_text_file_size = cfg.max_text_file_size;
        let max_read_lines = cfg.resolve_max_read_lines();
        let max_bash_output_lines = cfg.resolve_max_bash_output_lines();
        let max_grep_results = cfg.resolve_max_grep_results();
        let max_find_results = cfg.resolve_max_find_results();
        let max_list_dir_entries = cfg.resolve_max_list_dir_entries();
        let write_tool =
            tools::WriteTool::new(permission.clone(), ask_tx.clone(), max_text_file_size);
        #[cfg(feature = "lsp")]
        let write_tool = write_tool.with_lsp(lsp_manager.clone());
        let edit_tool = tools::EditTool::new(permission.clone(), ask_tx.clone());
        #[cfg(feature = "lsp")]
        let edit_tool = edit_tool.with_lsp(lsp_manager.clone());
        let base_tools: SmallVec<[Box<dyn rig::tool::ToolDyn>; 8]> = SmallVec::from_buf([
            Box::new(tools::ReadTool::new(
                permission.clone(),
                ask_tx.clone(),
                max_text_file_size,
                max_read_lines,
            )),
            Box::new(write_tool),
            Box::new(edit_tool),
            Box::new(tools::BashTool::new(
                permission.clone(),
                ask_tx.clone(),
                sandbox.clone(),
                max_bash_output_lines,
            )),
            Box::new(tools::GrepTool::new(
                permission.clone(),
                ask_tx.clone(),
                max_grep_results,
            )),
            Box::new(tools::FindFilesTool::new(
                permission.clone(),
                ask_tx.clone(),
                max_find_results,
            )),
            Box::new(tools::ListDirTool::new(
                permission.clone(),
                ask_tx.clone(),
                max_list_dir_entries,
            )),
            Box::new(tools::WriteTodoList::new(
                permission.clone(),
                ask_tx.clone(),
            )),
        ]);

        #[cfg_attr(
            not(any(
                feature = "subagents",
                feature = "memory",
                feature = "mcp",
                feature = "advisor",
                feature = "lsp"
            )),
            allow(unused_mut)
        )]
        let mut all_tools: Vec<Box<dyn rig::tool::ToolDyn>> = base_tools.into_vec();

        #[cfg(feature = "subagents")]
        if cfg.task_enabled.unwrap_or(true) {
            use crate::extras::subagents::task_tool::TaskTool;
            all_tools.push(Box::new(TaskTool::new(permission.clone(), ask_tx.clone())));
        }

        #[cfg(feature = "memory")]
        {
            use crate::extras::memory::{MemoryEdit, MemoryRead, MemorySearch, MemoryWrite};
            all_tools.push(Box::new(MemoryWrite::new(
                permission.clone(),
                ask_tx.clone(),
            )));
            all_tools.push(Box::new(MemoryEdit::new(
                permission.clone(),
                ask_tx.clone(),
            )));
            all_tools.push(Box::new(MemoryRead::new(
                permission.clone(),
                ask_tx.clone(),
            )));
            all_tools.push(Box::new(MemorySearch::new(
                permission.clone(),
                ask_tx.clone(),
            )));
        }

        #[cfg(feature = "mcp")]
        if let Some(manager) = &mcp_manager {
            let allow_all = cfg.allow_all_mcp_calls.unwrap_or(false);
            if allow_all && let Some(ref perm) = permission {
                perm.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_allow_all_mcp_calls(true);
            }
            let mcp_tools = manager
                .collect_tools(permission.clone(), ask_tx.clone())
                .await;
            for t in mcp_tools {
                all_tools.push(Box::new(t) as Box<dyn rig::tool::ToolDyn>);
            }
        }

        #[cfg(feature = "advisor")]
        if crate::extras::advisor::with_config(|c| c.enabled).unwrap_or(false) {
            use crate::extras::advisor::AdvisorTool;
            all_tools.push(Box::new(AdvisorTool::new()));
        }

        #[cfg(feature = "lsp")]
        if let Some(lsp) = &lsp_manager {
            all_tools.push(Box::new(tools::lsp::LspTool::new(lsp.clone())));
        }

        #[cfg(feature = "js")]
        register_js_tool(
            &mut all_tools,
            sandbox,
            permission.clone(),
            ask_tx.clone(),
            cfg,
            #[cfg(feature = "skills")]
            skill_turn_context,
        );

        let all_tools = filter_tools_by_allowlist(all_tools, &cli.tools);

        #[cfg(feature = "hooks")]
        let all_tools = crate::extras::hooks::wrap_from_global(all_tools, permission.clone());

        builder.tools(all_tools).build()
    }
}

#[cfg(all(test, feature = "js"))]
mod js_tests {
    use std::collections::HashMap;

    use super::{build_agent_inner, build_btw_agent_inner, register_js_tool};
    use crate::context::ContextFiles;
    use crate::sandbox::Sandbox;

    fn empty_context() -> ContextFiles {
        ContextFiles {
            agents: None,
            prompts: HashMap::new(),
            current_prompt: None,
            current_prompt_name: None,
            themes: HashMap::new(),
            current_theme_name: None,
            extra_files: Vec::new(),
            one_shot_restore: None,
            chain_declined: Vec::new(),
            #[cfg(feature = "memory")]
            memory: None,
            #[cfg(feature = "archmd")]
            architecture: None,
        }
    }

    fn fake_model(label: &str) -> rig::test_utils::MockCompletionModel {
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text(label.to_string()),
            MockStreamEvent::final_response_with_default_usage(),
        ]])
    }

    #[tokio::test]
    async fn registers_and_executes_js_tool() {
        let mut tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        register_js_tool(
            &mut tools,
            Sandbox::new(false, "bwrap"),
            None,
            None,
            &crate::config::Config::default(),
            #[cfg(feature = "skills")]
            None,
        );

        assert_eq!(
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["js"]
        );
        let result = tools[0]
            .call(r#"{"code":"1 + 1"}"#.to_string())
            .await
            .expect("registered JS tool call failed");
        assert_eq!(result, "2");
    }

    #[tokio::test]
    async fn full_main_agent_rebuild_and_provider_retry_reuse_worker_without_policy_leakage() {
        use crate::extras::js::supervisor::JsWorkerSupervisor;
        let cli = crate::cli::Cli::default();
        let context = empty_context();
        let mut first_config = crate::config::Config {
            js_read_unrestricted: Some(true),
            js_write_unrestricted: Some(true),
            ..crate::config::Config::default()
        };
        let first_agent = build_agent_inner(
            fake_model("provider-a"),
            &cli,
            &first_config,
            &context,
            None,
            None,
            Sandbox::new(false, "bwrap"),
            false,
            None,
            None,
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "mcp")]
            None,
        )
        .await;
        let first_tools = first_agent
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("first provider tool definitions");
        assert!(first_tools.iter().any(|tool| tool.name == "js"));

        let supervisor = JsWorkerSupervisor::shared();
        assert_eq!(
            first_agent
                .tool_server_handle
                .call_tool("js", r#"{"code":"20 + 1"}"#)
                .await
                .expect("first provider JS call"),
            "21"
        );
        let generation = supervisor.generation_for_test().await.unwrap();
        let process_id = supervisor.process_id_for_test().await.unwrap();

        // Provider retry reuses the same built Agent and therefore the same
        // JsTool/supervisor lease rather than rebuilding authority.
        assert_eq!(
            first_agent
                .tool_server_handle
                .call_tool("js", r#"{"code":"20 + 2"}"#)
                .await
                .expect("provider retry JS call"),
            "22"
        );
        assert_eq!(supervisor.generation_for_test().await, Some(generation));
        assert_eq!(supervisor.process_id_for_test().await, Some(process_id));

        let external_path = std::env::temp_dir().join(format!(
            "mini-agent-builder-rebuild-{}",
            uuid::Uuid::new_v4()
        ));
        let first_write = format!(
            "write_file({:?}, 'first'); 'written'",
            external_path.to_string_lossy()
        );
        assert_eq!(
            first_agent
                .tool_server_handle
                .call_tool(
                    "js",
                    &serde_json::json!({ "code": first_write }).to_string()
                )
                .await
                .expect("first provider unrestricted write"),
            "written"
        );
        assert_eq!(std::fs::read_to_string(&external_path).unwrap(), "first");
        std::fs::remove_file(&external_path).unwrap();

        first_config.js_read_unrestricted = Some(false);
        first_config.js_write_unrestricted = Some(false);
        let rebuilt_agent = build_agent_inner(
            fake_model("provider-b"),
            &cli,
            &first_config,
            &context,
            None,
            None,
            Sandbox::new(false, "bwrap"),
            false,
            None,
            None,
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "mcp")]
            None,
        )
        .await;
        let denied = format!(
            "try {{ write_file({:?}, 'leaked'); 'allowed' }} catch (_) {{ 'denied' }}",
            external_path.to_string_lossy()
        );
        assert_eq!(
            rebuilt_agent
                .tool_server_handle
                .call_tool("js", &serde_json::json!({ "code": denied }).to_string())
                .await
                .expect("rebuilt provider policy call"),
            "denied"
        );
        assert!(!external_path.exists());
        assert_eq!(supervisor.generation_for_test().await, Some(generation));
        assert_eq!(supervisor.process_id_for_test().await, Some(process_id));
    }

    #[tokio::test]
    async fn btw_agent_actual_tool_set_omits_js() {
        let agent = build_btw_agent_inner(
            fake_model("btw"),
            &crate::cli::Cli::default(),
            &crate::config::Config::default(),
            &empty_context(),
            &None,
            &None,
            false,
            None,
            None,
        );
        let names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("btw tool definitions")
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "read"));
        assert!(!names.iter().any(|name| name == "js"), "{names:?}");
    }
}

/// Dedicated system prompt for the `/btw` side-assistant. Deliberately NOT the
/// main coding `SYSTEM_PROMPT`: that one is all about using read/write/bash
/// tools, so pairing it with "you have no tools" made the model refuse and tell
/// the user to wait for the main agent. This prompt frames `/btw` as a quick,
/// read-only Q&A helper whose only job is to answer the user's question.
const BTW_SYSTEM_PROMPT: &str = "\
You are a fast side-assistant for quick \"by the way\" questions during a coding \
session. The user pressed /btw to ask you something in parallel with the main \
assistant, WITHOUT interrupting it.

Your only job: answer the user's question directly, briefly, and helpfully, using \
the conversation so far and the project context below. Reply in the user's \
language.

Match your length to the question: greetings, thanks, or yes/no questions get a \
ONE-LINE reply. Do NOT volunteer project setup, build, run, or test instructions \
unless the user explicitly asks how to build or run. The project context below is \
background for answering; it is NOT a script to recite.

This is a read-only side channel: you have read-only tools (read, grep, \
find_files, list_dir) to look things up, but you CANNOT write files, run \
commands, or change anything, and your reply is NOT saved to the conversation. \
Use the read tools when answering needs a file you do not already have in \
context, and keep it to what the question asks. Do NOT attempt or plan the main \
task, and do NOT tell the user to wait for the main assistant; just answer what \
they asked.";

/// Max model turns for a `/btw` side question. Higher than 1 so it can read a
/// file (or grep) and then answer, but small to keep side questions quick.
const BTW_MAX_TURNS: usize = 8;

/// Builds the isolated `/btw` agent: a lightweight read-only Q&A helper with the
/// project context for reference, NO tools, and a single turn. Never mutates the
/// session.
#[allow(clippy::too_many_arguments)]
pub fn build_btw_agent_inner<M: CompletionModel + 'static>(
    model: M,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    _reasoning_enabled: bool,
    temperature: Option<f64>,
    // See `build_agent_inner`: OpenRouter `provider.order` pin for `anthropic/*`.
    additional_params: Option<serde_json::Value>,
) -> Agent<M> {
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let mut preamble = String::new();
    preamble.push_str(BTW_SYSTEM_PROMPT);

    // Project context, for reference only — NOT instructions to act on.
    let has_ctx = context.agents.as_deref().is_some_and(|s| !s.is_empty()) || !cwd.is_empty();
    if has_ctx {
        preamble.push_str("\n\n## Project context (for reference)\n");
    }
    if let Some(agents) = context.agents.as_deref()
        && !agents.is_empty()
    {
        preamble.push('\n');
        preamble.push_str(agents);
    }
    #[cfg(feature = "archmd")]
    if let Some(arch) = context.architecture.as_deref()
        && !arch.is_empty()
    {
        preamble.push_str("\n\n");
        preamble.push_str(arch);
    }
    if let Some(p) = context.current_prompt.as_deref()
        && !p.is_empty()
    {
        preamble.push_str("\n\n");
        preamble.push_str(p);
    }
    if !cwd.is_empty() {
        preamble.push_str("\n\nCurrent working directory: ");
        preamble.push_str(&cwd);
    }
    #[cfg(feature = "memory")]
    crate::extras::memory::append_memory_block(&mut preamble, context.memory.as_deref());

    if let Some(s) = crate::session::storage::load_suffix() {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(&s);
    }

    let max_tokens = cli.resolve_max_tokens(cfg);

    // Honor --no-tools: fall back to a pure-context, single-turn answer.
    if cli.resolve_no_tools(cfg) {
        let mut builder = AgentBuilder::new(model)
            .preamble(&preamble)
            .default_max_turns(1)
            .max_tokens(max_tokens);
        if let Some(params) = additional_params.clone() {
            builder = builder.additional_params(params);
        }
        if let Some(temp) = temperature {
            builder = builder.temperature(temp);
        }
        return builder.build();
    }

    // Read-only tools only (read/grep/find_files/list_dir): a side question can
    // look things up, but has no write/edit/bash, so it still has no side
    // effects to roll back and never mutates the session. Allow multiple turns
    // so it can read then answer.
    let max_text_file_size = cfg.max_text_file_size;
    let max_read_lines = cfg.resolve_max_read_lines();
    let max_grep_results = cfg.resolve_max_grep_results();
    let max_find_results = cfg.resolve_max_find_results();
    let max_list_dir_entries = cfg.resolve_max_list_dir_entries();
    let read_tools: Vec<Box<dyn rig::tool::ToolDyn>> = vec![
        Box::new(tools::ReadTool::new(
            permission.clone(),
            ask_tx.clone(),
            max_text_file_size,
            max_read_lines,
        )),
        Box::new(tools::GrepTool::new(
            permission.clone(),
            ask_tx.clone(),
            max_grep_results,
        )),
        Box::new(tools::FindFilesTool::new(
            permission.clone(),
            ask_tx.clone(),
            max_find_results,
        )),
        Box::new(tools::ListDirTool::new(
            permission.clone(),
            ask_tx.clone(),
            max_list_dir_entries,
        )),
    ];

    let mut builder = AgentBuilder::new(model)
        .preamble(&preamble)
        .default_max_turns(BTW_MAX_TURNS)
        .max_tokens(max_tokens)
        .tools(read_tools);

    if let Some(params) = additional_params {
        builder = builder.additional_params(params);
    }

    if let Some(temp) = temperature {
        builder = builder.temperature(temp);
    }

    builder.build()
}
