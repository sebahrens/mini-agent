use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use rig::agent::{Agent, AgentBuilder};
use rig::completion::CompletionModel;
use smallvec::SmallVec;

use crate::agent::prompt::{
    EDIT_TOOL_PROMPT, FIND_FILES_TOOL_PROMPT, GREP_TOOL_PROMPT, JS_TOOL_PROMPT,
    LIST_DIR_TOOL_PROMPT, READ_TOOL_PROMPT, SYSTEM_PROMPT, TASK_TOOL_PROMPT, TODO_TOOL_PROMPT,
    WRITE_TOOL_PROMPT,
};
use crate::agent::tools;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
#[cfg(feature = "mcp")]
use crate::extras::mcp::McpClientManager;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::Sandbox;

fn registered_shell_capability<'a>(
    cli: &Cli,
    cfg: &Config,
    sandbox: &'a Sandbox,
) -> Option<&'a crate::sandbox::ShellCapability> {
    if !cli.tool_is_eligible(cfg, "shell") {
        return None;
    }
    sandbox.shell_capability()
}

fn is_reserved_builtin_tool_name(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "write"
            | "edit"
            | "grep"
            | "find_files"
            | "list_dir"
            | "todo_write"
            | "shell"
            | "job_status"
            | "bash"
            | "git"
            | "js"
            | "task"
            | "memory_write"
            | "memory_edit"
            | "memory_read"
            | "memory_search"
            | "advisor"
            | "lsp_diagnostics"
    )
}

fn canonical_tool_name(name: &str) -> &str {
    if name == "bash" { "shell" } else { name }
}

/// Per-file byte cap for `/add`ed context files. Exported so `add.rs` can use
/// the same value without duplicating the constant.
pub(crate) const MAX_EXTRA_FILE_BYTES: usize = 524_288;

/// Read at most `cap` bytes from `path`, decoding as UTF-8 (lossy) and appending
/// a truncation notice when the file is larger. Returns `None` if the file cannot
/// be opened, so the caller can silently skip unreadable files.
pub(crate) fn read_extra_file_bounded(path: &std::path::Path, cap: usize) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // Take cap+1 bytes so we know whether the file is larger than cap without
    // reading the whole thing.
    file.take(cap.saturating_add(1) as u64)
        .read_to_end(&mut buf)
        .ok()?;
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
    }
    let raw = String::from_utf8_lossy(&buf);
    let body = if truncated {
        let s: &str = raw.as_ref();
        let mut end = s.len().min(cap);
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        let mut t = s[..end].to_string();
        t.push_str("\n\n[truncated — file exceeded preamble size limit]");
        t
    } else {
        raw.into_owned()
    };
    Some(body)
}

/// Assemble the tool-independent system-prompt context: the base
/// `SYSTEM_PROMPT`, context files (AGENTS.md, ARCHITECTURE.md, active mode
/// prompt), working directory, `/add`ed files, memory, and the user `SUFFIX.md`.
/// [`build_registered_preamble`] adds guidance for the final registered tools.
#[cfg(test)]
pub fn build_preamble(context: &ContextFiles, reasoning_enabled: bool) -> String {
    build_preamble_for_workspace(
        context,
        reasoning_enabled,
        Some(context.workspace_root.as_path()),
    )
}

pub(crate) fn build_preamble_for_workspace(
    context: &ContextFiles,
    reasoning_enabled: bool,
    workspace_root: Option<&Path>,
) -> String {
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
    let cwd = workspace_root
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let total_len = reasoning_prefix.len()
        + SYSTEM_PROMPT.len()
        + 1
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
    let total_len = total_len + context.memory.as_deref().map_or(0, |m| m.len() + 8); // "\n\n---\n\n" + content

    let total_len = total_len + suffix.as_ref().map_or(0, |s| s.len() + 6); // "\n\n---\n\n"

    // Add extra files content to preamble budget. Content is preloaded at /add
    // time via spawn_blocking; the fallback bounded sync read handles files that
    // were injected without going through the /add path (e.g. tests).
    let extra_files_content: Vec<String> = context
        .extra_files
        .iter()
        .filter_map(|p| {
            let body = if let Some(cached) = context.extra_file_contents.get(p) {
                cached.as_ref().clone()
            } else {
                // Fallback: bounded sync read. This path should only be reached in
                // tests or when files are added without going through /add.
                read_extra_file_bounded(p, MAX_EXTRA_FILE_BYTES)?
            };
            Some(format!("Content of {}:\n{}", p.display(), body))
        })
        .collect();
    let extra_files_len: usize = extra_files_content.iter().map(|s| s.len() + 2).sum();
    let total_len = total_len + extra_files_len;

    let mut preamble = String::with_capacity(total_len);
    preamble.push_str(reasoning_prefix);
    preamble.push_str(SYSTEM_PROMPT);
    preamble.push('\n');
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
    }
    if let Some(s) = &suffix {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(s);
    }
    preamble
}

fn build_registered_preamble(
    context: &ContextFiles,
    reasoning_enabled: bool,
    workspace_root: &Path,
    sandbox: &Sandbox,
    registered_tools: &[&str],
) -> String {
    let mut preamble =
        build_preamble_for_workspace(context, reasoning_enabled, Some(workspace_root));
    let has = |name: &str| registered_tools.contains(&name);
    if has("js") {
        preamble.push_str(JS_TOOL_PROMPT);
    }
    if has("read") {
        preamble.push_str(READ_TOOL_PROMPT);
    }
    if has("write") {
        preamble.push_str(WRITE_TOOL_PROMPT);
    }
    if has("edit") {
        preamble.push_str(EDIT_TOOL_PROMPT);
    }
    if has("grep") {
        preamble.push_str(GREP_TOOL_PROMPT);
    }
    if has("find_files") {
        preamble.push_str(FIND_FILES_TOOL_PROMPT);
    }
    if has("list_dir") {
        preamble.push_str(LIST_DIR_TOOL_PROMPT);
    }
    if has("todo_write") {
        preamble.push_str(TODO_TOOL_PROMPT);
    }
    if has("task") {
        preamble.push_str(TASK_TOOL_PROMPT);
    }
    #[cfg(feature = "memory")]
    {
        if has("memory_write") {
            preamble.push_str(crate::agent::prompt::MEMORY_WRITE_TOOL_PROMPT);
        }
        if has("memory_edit") {
            preamble.push_str(crate::agent::prompt::MEMORY_EDIT_TOOL_PROMPT);
        }
        if has("memory_search") {
            preamble.push_str(crate::agent::prompt::MEMORY_SEARCH_TOOL_PROMPT);
        }
        if has("memory_read") {
            preamble.push_str(crate::agent::prompt::MEMORY_READ_TOOL_PROMPT);
        }
    }
    #[cfg(feature = "lsp")]
    if has("lsp_diagnostics") {
        preamble.push_str(crate::agent::prompt::LSP_PROMPT);
        if has("edit") || has("write") {
            preamble.push_str(crate::agent::prompt::LSP_MUTATION_PROMPT);
        }
    }
    if has("shell")
        && let Some(capability) = sandbox.shell_capability()
    {
        preamble.push_str(&capability.model_guidance());
    }
    preamble
}

fn estimated_registered_tools(cli: &Cli, cfg: &Config, sandbox: &Sandbox) -> Vec<&'static str> {
    if cli.resolve_no_tools(cfg) {
        return Vec::new();
    }
    let mut names = vec![
        "read",
        "write",
        "edit",
        "grep",
        "find_files",
        "list_dir",
        "todo_write",
    ];
    names.push("git");
    if registered_shell_capability(cli, cfg, sandbox).is_some() {
        names.push("shell");
        names.push("job_status");
    }
    #[cfg(feature = "js")]
    if cli.tool_is_eligible(cfg, "js") {
        names.push("js");
    }
    #[cfg(feature = "subagents")]
    if cfg.task_enabled.unwrap_or(true) {
        names.push("task");
    }
    #[cfg(feature = "memory")]
    names.extend([
        "memory_write",
        "memory_edit",
        "memory_read",
        "memory_search",
    ]);
    #[cfg(feature = "lsp")]
    if cli.tool_is_eligible(cfg, "lsp_diagnostics") && cfg.resolve_lsp().is_some() {
        names.push("lsp_diagnostics");
    }
    names.retain(|name| {
        if *name == "job_status" {
            cli.tool_is_eligible(cfg, "shell")
        } else {
            cli.tool_is_eligible(cfg, name)
        }
    });
    names
}

/// Conservatively estimate the registered preamble before provider calibration.
/// Runtime-unavailable optional tools can make this slightly high. Tool-schema
/// tokens are folded into the first provider usage report.
pub fn estimate_overhead(
    context: &ContextFiles,
    reasoning_enabled: bool,
    cli: &Cli,
    cfg: &Config,
    sandbox: &Sandbox,
) -> u64 {
    let registered_tools = estimated_registered_tools(cli, cfg, sandbox);
    let preamble = build_registered_preamble(
        context,
        reasoning_enabled,
        &context.workspace_root,
        sandbox,
        &registered_tools,
    );
    crate::session::Session::estimate_tokens(&preamble)
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
    let allowed: HashSet<&str> = allowlist.iter().map(|s| canonical_tool_name(s)).collect();
    for name in &allowed {
        if !tools
            .iter()
            .any(|t| canonical_tool_name(t.name().as_ref()) == *name)
        {
            tracing::warn!("--tools: unknown tool '{name}' (ignored)");
        }
    }
    tools
        .into_iter()
        .filter(|tool| {
            let name = tool.name();
            let name = canonical_tool_name(name.as_ref());
            allowed.contains(name) || (name == "job_status" && allowed.contains("shell"))
        })
        .collect()
}

#[cfg(feature = "js")]
#[allow(clippy::too_many_arguments)]
fn register_js_tool(
    tools: &mut Vec<Box<dyn rig::tool::ToolDyn>>,
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    cfg: &Config,
    containment_status: crate::sandbox::worker::WorkerContainmentStatus,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    #[cfg(feature = "skills")] skill_services: Option<
        Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) {
    register_js_tool_with_status(
        tools,
        sandbox,
        permission,
        ask_tx,
        cfg,
        containment_status,
        workspace,
        #[cfg(feature = "skills")]
        skill_services,
    );
}

#[cfg(feature = "js")]
#[allow(clippy::too_many_arguments)]
fn register_js_tool_with_status(
    tools: &mut Vec<Box<dyn rig::tool::ToolDyn>>,
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    cfg: &Config,
    containment_status: crate::sandbox::worker::WorkerContainmentStatus,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    #[cfg(feature = "skills")] skill_services: Option<
        Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
) {
    let workspace_root = workspace.root();
    use crate::extras::js::host::AllowConfig;
    use crate::extras::js::tool::JsTool;
    use crate::sandbox::worker::WorkerContainmentStatus;

    if let WorkerContainmentStatus::Unavailable {
        backend, reason, ..
    } = containment_status
    {
        tracing::warn!(backend = %backend, reason = %reason, "JavaScript tool unavailable");
        return;
    }

    let startup_base = workspace_root.to_path_buf();
    let allow_config = AllowConfig::from_settings(
        &startup_base,
        cfg.js_file_base_dir.as_deref(),
        cfg.js_read_roots.as_deref(),
        cfg.js_write_roots.as_deref(),
        cfg.js_read_unrestricted.unwrap_or(false),
        cfg.js_write_unrestricted.unwrap_or(false),
    )
    .with_workspace_binding(workspace.clone())
    .with_fetch_settings(
        cfg.js_fetch_origins.as_deref(),
        cfg.js_fetch_allow_http.unwrap_or(false),
    );
    #[cfg(feature = "skills")]
    let mut js_tool = JsTool::new(sandbox, permission, ask_tx, allow_config);

    #[cfg(not(feature = "skills"))]
    let js_tool = JsTool::new(sandbox, permission, ask_tx, allow_config);

    #[cfg(feature = "skills")]
    if let Some(services) = skill_services {
        js_tool = js_tool.with_skill_turn_context(services.turn_context());
        if let Some(telemetry) = services.telemetry() {
            js_tool = js_tool.with_shared_telemetry(telemetry);
        }
        if let Some(proposals) = services.proposal_service() {
            js_tool = js_tool.with_proposal_service(proposals);
        }
    }

    tools.push(Box::new(js_tool));
}

#[allow(clippy::too_many_arguments)]
pub async fn build_agent_inner<M: CompletionModel + 'static>(
    model: M,
    cli: &Cli,
    cfg: &Config,
    context: &ContextFiles,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    sandbox: Sandbox,
    read_tracker: tools::ReadTracker,
    reasoning_enabled: bool,
    temperature: Option<f64>,
    // Provider-specific extra body params (e.g. OpenRouter `provider.order` to
    // pin Claude to the Anthropic direct route so `cache_control` is honored).
    // `None` for providers that need no extra routing.
    additional_params: Option<serde_json::Value>,
    #[cfg(feature = "js")]
    js_worker_containment_status: crate::sandbox::worker::WorkerContainmentStatus,
    #[cfg(feature = "skills")] skill_services: Option<
        Arc<crate::extras::js::skills::session::SkillSessionServices>,
    >,
    #[cfg(feature = "mcp")] mcp_manager: Option<&McpClientManager>,
) -> Agent<M> {
    let workspace_root = workspace.root();
    let sandbox = sandbox.with_workspace_binding(workspace.clone());
    let tools_enabled = !cli.resolve_no_tools(cfg);
    let shell_tool_enabled = registered_shell_capability(cli, cfg, &sandbox).is_some();
    #[cfg(feature = "js")]
    let js_tool_enabled = cli.tool_is_eligible(cfg, "js");
    #[cfg(feature = "lsp")]
    let lsp_manager = if !cli.tool_is_eligible(cfg, "lsp_diagnostics") {
        None
    } else {
        cfg.resolve_lsp()
            .map(|c| crate::extras::lsp::LspManager::new(c, workspace.clone()))
    };

    let all_tools = if !tools_enabled {
        None
    } else {
        let max_text_file_size = cfg.max_text_file_size;
        let max_read_lines = cfg.resolve_max_read_lines();
        let max_bash_output_lines = cfg.resolve_max_bash_output_lines();
        let max_grep_results = cfg.resolve_max_grep_results();
        let max_find_results = cfg.resolve_max_find_results();
        let max_list_dir_entries = cfg.resolve_max_list_dir_entries();
        let write_tool = tools::WriteTool::new_with_tracker(
            permission.clone(),
            ask_tx.clone(),
            max_text_file_size,
            read_tracker.clone(),
        )
        .with_workspace_binding(workspace.clone());
        #[cfg(feature = "lsp")]
        let write_tool = write_tool.with_lsp(lsp_manager.clone());
        let edit_tool = tools::EditTool::new_with_tracker(
            permission.clone(),
            ask_tx.clone(),
            read_tracker.clone(),
        )
        .with_workspace_binding(workspace.clone());
        #[cfg(feature = "lsp")]
        let edit_tool = edit_tool.with_lsp(lsp_manager.clone());
        let mut base_tools: SmallVec<[Box<dyn rig::tool::ToolDyn>; 8]> = SmallVec::new();
        base_tools.push(Box::new(
            tools::ReadTool::new_with_tracker(
                permission.clone(),
                ask_tx.clone(),
                max_text_file_size,
                max_read_lines,
                read_tracker,
            )
            .with_workspace_binding(workspace.clone()),
        ));
        base_tools.push(Box::new(write_tool));
        base_tools.push(Box::new(edit_tool));
        if shell_tool_enabled {
            base_tools.push(Box::new(tools::ShellTool::new(
                permission.clone(),
                ask_tx.clone(),
                sandbox.clone(),
                max_bash_output_lines,
            )));
            base_tools.push(Box::new(tools::JobStatusTool::new(
                sandbox.clone(),
                max_bash_output_lines,
            )));
        }
        base_tools.push(Box::new(
            tools::GrepTool::new(permission.clone(), ask_tx.clone(), max_grep_results)
                .with_workspace_binding(workspace.clone()),
        ));
        base_tools.push(Box::new(
            tools::FindFilesTool::new(permission.clone(), ask_tx.clone(), max_find_results)
                .with_workspace_binding(workspace.clone()),
        ));
        base_tools.push(Box::new(
            tools::ListDirTool::new(permission.clone(), ask_tx.clone(), max_list_dir_entries)
                .with_workspace_binding(workspace.clone()),
        ));
        base_tools.push(Box::new(tools::WriteTodoList::new(
            permission.clone(),
            ask_tx.clone(),
        )));
        // Structured Git is intentionally available only when the git-worktree
        // feature is compiled in; it has no shell/raw-argv escape hatch.
        #[cfg(feature = "git-worktree")]
        if cli.tool_is_eligible(cfg, "git") {
            match crate::git::tool::GitTool::capture(
                workspace.clone(),
                sandbox.clone(),
                permission.clone(),
                ask_tx.clone(),
            ) {
                Ok(tool) => base_tools.push(Box::new(tool)),
                Err(error) => tracing::warn!(%error, "structured Git tool unavailable"),
            }
        }

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
            let task_tool = TaskTool::new(
                permission.clone(),
                ask_tx.clone(),
                cfg.deny_repeated_reads.unwrap_or(true),
            )
            .with_workspace_binding(workspace.clone());
            #[cfg(feature = "archmd")]
            let task_tool = task_tool.with_architecture(context.architecture.clone());
            all_tools.push(Box::new(task_tool));
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
            let timeouts =
                crate::extras::mcp::McpToolTimeouts::from_config_secs(cfg.mcp_tool_timeout_secs);
            let mcp_tools = manager
                .collect_tools_with_timeouts(permission.clone(), ask_tx.clone(), timeouts)
                .await;
            for t in mcp_tools {
                if is_reserved_builtin_tool_name(&rig::tool::ToolDyn::name(&t)) {
                    tracing::warn!(
                        "MCP tool skipped because its name is reserved by a built-in tool"
                    );
                } else {
                    all_tools.push(Box::new(t) as Box<dyn rig::tool::ToolDyn>);
                }
            }
        }

        #[cfg(feature = "advisor")]
        if crate::extras::advisor::with_config(|c| c.enabled).unwrap_or(false) {
            use crate::extras::advisor::AdvisorTool;
            all_tools.push(Box::new(AdvisorTool::new()));
        }

        #[cfg(feature = "lsp")]
        if let Some(lsp) = &lsp_manager {
            all_tools.push(Box::new(tools::lsp::LspTool::new(
                lsp.clone(),
                permission.clone(),
                ask_tx.clone(),
            )));
        }

        #[cfg(feature = "js")]
        if js_tool_enabled {
            register_js_tool(
                &mut all_tools,
                sandbox.clone(),
                permission.clone(),
                ask_tx.clone(),
                cfg,
                js_worker_containment_status,
                workspace.clone(),
                #[cfg(feature = "skills")]
                skill_services,
            );
        }

        let all_tools = filter_tools_by_allowlist(all_tools, &cli.tools);
        let all_tools = tools::memoize::definitions(all_tools);

        #[cfg(feature = "hooks")]
        let all_tools = crate::extras::hooks::wrap_from_global(all_tools, permission.clone());

        Some(all_tools)
    };

    let registered_tool_names: Vec<String> = all_tools
        .as_ref()
        .map(|tools| tools.iter().map(|tool| tool.name()).collect())
        .unwrap_or_default();
    let registered_tools: Vec<&str> = registered_tool_names.iter().map(String::as_str).collect();
    let preamble = build_registered_preamble(
        context,
        reasoning_enabled,
        workspace_root,
        &sandbox,
        &registered_tools,
    );
    let mut builder = AgentBuilder::new(model)
        .preamble(&preamble)
        .max_tokens(cli.resolve_max_tokens(cfg))
        .default_max_turns(cli.resolve_max_agent_turns(cfg));
    if let Some(params) = additional_params {
        builder = builder.additional_params(params);
    }
    if let Some(temp) = temperature {
        builder = builder.temperature(temp);
    }
    match all_tools {
        Some(tools) => builder.tools(tools).build(),
        None => builder.build(),
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
    workspace: &Arc<crate::paths::WorkspaceBinding>,
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    _reasoning_enabled: bool,
    temperature: Option<f64>,
    // See `build_agent_inner`: OpenRouter `provider.order` pin for `anthropic/*`.
    additional_params: Option<serde_json::Value>,
) -> Agent<M> {
    let cwd = workspace.root().display().to_string();

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
    let read_tracker = tools::ReadTracker::new(cfg.deny_repeated_reads.unwrap_or(true));
    let read_tools: Vec<Box<dyn rig::tool::ToolDyn>> = vec![
        Box::new(
            tools::ReadTool::new_with_tracker(
                permission.clone(),
                ask_tx.clone(),
                max_text_file_size,
                max_read_lines,
                read_tracker,
            )
            .with_workspace_binding(workspace.clone()),
        ),
        Box::new(
            tools::GrepTool::new(permission.clone(), ask_tx.clone(), max_grep_results)
                .with_workspace_binding(workspace.clone()),
        ),
        Box::new(
            tools::FindFilesTool::new(permission.clone(), ask_tx.clone(), max_find_results)
                .with_workspace_binding(workspace.clone()),
        ),
        Box::new(
            tools::ListDirTool::new(permission.clone(), ask_tx.clone(), max_list_dir_entries)
                .with_workspace_binding(workspace.clone()),
        ),
    ];
    let read_tools = tools::memoize::definitions(read_tools);

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

#[cfg(test)]
mod extra_file_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::context::ContextFiles;

    fn temp_file_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mini-agent-extra-file-{tag}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    fn empty_ctx() -> ContextFiles {
        ContextFiles {
            workspace_root: std::path::PathBuf::from("."),
            agents: None,
            prompts: HashMap::new(),
            current_prompt: None,
            current_prompt_name: None,
            themes: HashMap::new(),
            current_theme_name: None,
            extra_files: Vec::new(),
            extra_file_contents: HashMap::new(),
            one_shot_restore: None,
            chain_declined: Vec::new(),
            #[cfg(feature = "memory")]
            memory: None,
            #[cfg(feature = "archmd")]
            architecture: None,
        }
    }

    #[test]
    fn bounded_read_returns_full_content_under_cap() {
        let path = temp_file_path("under-cap");
        std::fs::write(&path, "hello world").unwrap();
        let content = super::read_extra_file_bounded(&path, 512).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(content, "hello world");
        assert!(!content.contains("[truncated"));
    }

    #[test]
    fn bounded_read_truncates_at_cap_without_full_allocation() {
        let path = temp_file_path("over-cap");
        let long = "a".repeat(2000);
        std::fs::write(&path, &long).unwrap();
        let cap = 1000;
        let content = super::read_extra_file_bounded(&path, cap).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(content.contains("[truncated — file exceeded preamble size limit]"));
        assert!(content.len() <= cap + 100);
    }

    #[test]
    fn bounded_read_returns_none_for_missing_file() {
        let result = super::read_extra_file_bounded(
            std::path::Path::new("/nonexistent/path/file.txt"),
            1024,
        );
        assert!(result.is_none());
    }

    #[test]
    fn preamble_uses_preloaded_content_without_disk_read() {
        // File does not exist on disk; preamble must use preloaded content.
        let fake_path = std::path::PathBuf::from("/dev/null/nonexistent.txt");
        let mut ctx = empty_ctx();
        ctx.extra_files.push(fake_path.clone());
        ctx.extra_file_contents
            .insert(fake_path, Arc::new("preloaded content".to_string()));
        let preamble = super::build_preamble_for_workspace(&ctx, false, None);
        assert!(
            preamble.contains("preloaded content"),
            "preamble must use cached content, not attempt a disk read"
        );
    }

    #[test]
    fn preamble_falls_back_to_bounded_read_for_uncached_file() {
        let path = temp_file_path("fallback");
        std::fs::write(&path, "fallback content").unwrap();
        let mut ctx = empty_ctx();
        ctx.extra_files.push(path.clone());
        // No entry in extra_file_contents — forces fallback read path
        let preamble = super::build_preamble_for_workspace(&ctx, false, None);
        std::fs::remove_file(&path).ok();
        assert!(preamble.contains("fallback content"));
    }
}

#[cfg(all(test, feature = "js"))]
mod js_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{
        build_agent_inner, build_btw_agent_inner, build_registered_preamble,
        is_reserved_builtin_tool_name, register_js_tool_with_status, registered_shell_capability,
    };
    use crate::context::ContextFiles;
    use crate::sandbox::{
        Sandbox, ShellCapability, ShellDialect,
        worker::{WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus},
    };

    fn empty_context() -> ContextFiles {
        ContextFiles {
            workspace_root: std::path::PathBuf::from("."),
            agents: None,
            prompts: HashMap::new(),
            current_prompt: None,
            current_prompt_name: None,
            themes: HashMap::new(),
            current_theme_name: None,
            extra_files: Vec::new(),
            extra_file_contents: HashMap::new(),
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

    fn workspace_binding() -> std::sync::Arc<crate::paths::WorkspaceBinding> {
        std::sync::Arc::new(
            crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
        )
    }

    fn shell_sandbox() -> Sandbox {
        let capability =
            ShellCapability::for_test(&std::env::current_exe().unwrap(), ShellDialect::Posix);
        Sandbox::new(false, "bwrap").with_resolved_shell(Some(capability))
    }

    async fn test_main_agent(
        cli: &crate::cli::Cli,
        sandbox: Sandbox,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> rig::agent::Agent<rig::test_utils::MockCompletionModel> {
        build_agent_inner(
            fake_model("shell-test"),
            cli,
            &crate::config::Config::default(),
            &empty_context(),
            workspace,
            None,
            None,
            sandbox,
            crate::agent::tools::ReadTracker::new(true),
            false,
            None,
            None,
            crate::sandbox::worker::containment_status(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "mcp")]
            None,
        )
        .await
    }

    #[tokio::test]
    async fn shell_tool_and_guidance_follow_the_same_captured_capability() {
        let workspace = workspace_binding();
        let sandbox = shell_sandbox();
        let cfg = crate::config::Config::default();
        let cli = crate::cli::Cli::default();
        let guidance = registered_shell_capability(&cli, &cfg, &sandbox)
            .unwrap()
            .model_guidance();
        assert!(guidance.contains("POSIX shell"));
        let preamble = build_registered_preamble(
            &empty_context(),
            false,
            workspace.root(),
            &sandbox,
            &["shell"],
        );
        assert!(preamble.contains("Run POSIX shell commands"));
        let agent = test_main_agent(&cli, sandbox.clone(), workspace.clone()).await;
        let definitions = agent.tool_server_handle.get_tool_defs(None).await.unwrap();
        let shell = definitions
            .iter()
            .find(|tool| tool.name == "shell")
            .unwrap();
        assert!(shell.description.contains("POSIX shell"));
        assert!(
            definitions.iter().any(|tool| tool.name == "job_status"),
            "a registered shell must include its background-job companion"
        );

        let missing = Sandbox::new(false, "bwrap").with_resolved_shell(None);
        assert!(registered_shell_capability(&cli, &cfg, &missing).is_none());
        let missing_preamble = build_registered_preamble(
            &empty_context(),
            false,
            workspace.root(),
            &missing,
            &["read"],
        );
        assert!(!missing_preamble.contains("shell commands"));
        assert!(!missing_preamble.contains("run commands"));
        let agent = test_main_agent(&cli, missing, workspace.clone()).await;
        assert!(
            !agent
                .tool_server_handle
                .get_tool_defs(None)
                .await
                .unwrap()
                .iter()
                .any(|tool| matches!(tool.name.as_str(), "shell" | "job_status"))
        );

        let no_tools = crate::cli::Cli {
            no_tools: true,
            ..crate::cli::Cli::default()
        };
        assert!(registered_shell_capability(&no_tools, &cfg, &sandbox).is_none());
        let no_tools_preamble =
            build_registered_preamble(&empty_context(), false, workspace.root(), &sandbox, &[]);
        assert!(!no_tools_preamble.contains("shell commands"));
        assert!(!no_tools_preamble.contains("**js**"));
        assert!(!no_tools_preamble.contains("**read**"));
        let agent = test_main_agent(&no_tools, sandbox.clone(), workspace.clone()).await;
        assert!(
            agent
                .tool_server_handle
                .get_tool_defs(None)
                .await
                .unwrap()
                .is_empty()
        );

        let read_only = crate::cli::Cli {
            tools: vec!["read".to_string()],
            ..crate::cli::Cli::default()
        };
        assert!(registered_shell_capability(&read_only, &cfg, &sandbox).is_none());
        let read_only_preamble = build_registered_preamble(
            &empty_context(),
            false,
            workspace.root(),
            &sandbox,
            &["read"],
        );
        assert!(!read_only_preamble.contains("shell commands"));
        assert!(read_only_preamble.contains("**read**"));
        assert!(!read_only_preamble.contains("**js**"));
        assert!(!read_only_preamble.contains("**write**"));
        let agent = test_main_agent(&read_only, sandbox, workspace).await;
        let definitions = agent.tool_server_handle.get_tool_defs(None).await.unwrap();
        assert_eq!(
            definitions
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read"]
        );
    }

    #[test]
    fn registered_preamble_names_only_registered_execution_tools() {
        let workspace = workspace_binding();
        let sandbox = shell_sandbox();
        let js =
            build_registered_preamble(&empty_context(), false, workspace.root(), &sandbox, &["js"]);
        assert!(js.contains("**js**"));
        assert!(js.contains("Use Python only when the user requests"));
        assert!(!js.contains("**bash**"));
        assert!(!js.contains("**read**"));

        let mcp_only = build_registered_preamble(
            &empty_context(),
            false,
            workspace.root(),
            &sandbox,
            &["github_search"],
        );
        assert!(!mcp_only.contains("**js**"));
        assert!(!mcp_only.contains("**read**"));
        assert!(!mcp_only.contains("shell commands"));

        #[cfg(feature = "lsp")]
        {
            let without_lsp = build_registered_preamble(
                &empty_context(),
                false,
                workspace.root(),
                &sandbox,
                &["read"],
            );
            assert!(!without_lsp.contains("lsp_diagnostics"));
            let with_lsp = build_registered_preamble(
                &empty_context(),
                false,
                workspace.root(),
                &sandbox,
                &["lsp_diagnostics"],
            );
            assert!(with_lsp.contains("lsp_diagnostics"));
            assert!(!with_lsp.contains("after supported file changes"));
            assert!(!with_lsp.contains("**edit**"));
            assert!(!with_lsp.contains("**write**"));
            let with_lsp_and_edit = build_registered_preamble(
                &empty_context(),
                false,
                workspace.root(),
                &sandbox,
                &["lsp_diagnostics", "edit"],
            );
            assert!(with_lsp_and_edit.contains("after supported file changes"));
        }
    }

    #[test]
    fn external_tools_cannot_claim_builtin_prompt_semantics() {
        for name in [
            "read",
            "write",
            "edit",
            "grep",
            "find_files",
            "list_dir",
            "todo_write",
            "bash",
            "job_status",
            "js",
            "task",
            "memory_write",
            "memory_edit",
            "memory_read",
            "memory_search",
            "advisor",
            "lsp_diagnostics",
        ] {
            assert!(is_reserved_builtin_tool_name(name), "{name}");
        }
        assert!(!is_reserved_builtin_tool_name("github_search"));
    }

    #[tokio::test]
    async fn registers_and_executes_js_tool() {
        let mut tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        let workspace = std::sync::Arc::new(
            crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
        );
        register_js_tool_with_status(
            &mut tools,
            Sandbox::new(false, "bwrap"),
            None,
            None,
            &crate::config::Config::default(),
            WorkerContainmentStatus::Available {
                backend: WorkerBackend::for_current_platform(),
                assurance: WorkerContainmentAssurance::Enforced,
            },
            workspace,
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

    #[test]
    fn unavailable_worker_containment_registers_no_js_tool_even_without_general_sandbox() {
        let mut tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        register_js_tool_with_status(
            &mut tools,
            Sandbox::new(false, "bwrap"),
            None,
            None,
            &crate::config::Config::default(),
            WorkerContainmentStatus::Unavailable {
                backend: WorkerBackend::for_current_platform(),
                assurance: WorkerContainmentAssurance::Enforced,
                reason: "backend probe failed".into(),
            },
            workspace_binding(),
            #[cfg(feature = "skills")]
            None,
        );

        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn available_worker_is_independent_of_unavailable_general_sandbox() {
        let mut tools: Vec<Box<dyn rig::tool::ToolDyn>> = Vec::new();
        register_js_tool_with_status(
            &mut tools,
            Sandbox::new(true, "missing-general-backend"),
            None,
            None,
            &crate::config::Config::default(),
            WorkerContainmentStatus::Available {
                backend: WorkerBackend::for_current_platform(),
                assurance: WorkerContainmentAssurance::Enforced,
            },
            workspace_binding(),
            #[cfg(feature = "skills")]
            None,
        );

        assert_eq!(
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["js"]
        );
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
        let containment_status = crate::sandbox::worker::containment_status();
        let first_agent = build_agent_inner(
            fake_model("provider-a"),
            &cli,
            &first_config,
            &context,
            workspace_binding(),
            None,
            None,
            Sandbox::new(false, "bwrap"),
            crate::agent::tools::ReadTracker::new(true),
            false,
            None,
            None,
            containment_status.clone(),
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
        if matches!(
            containment_status,
            WorkerContainmentStatus::Unavailable { .. }
        ) {
            assert!(!first_tools.iter().any(|tool| tool.name == "js"));
            return;
        }
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
            workspace_binding(),
            None,
            None,
            Sandbox::new(false, "bwrap"),
            crate::agent::tools::ReadTracker::new(true),
            false,
            None,
            None,
            crate::sandbox::worker::containment_status(),
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
        let workspace = Arc::new(
            crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
        );
        let agent = build_btw_agent_inner(
            fake_model("btw"),
            &crate::cli::Cli::default(),
            &crate::config::Config::default(),
            &empty_context(),
            &workspace,
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
