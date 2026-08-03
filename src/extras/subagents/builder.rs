use crate::agent::tools;
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
}

impl SubagentAuthorization {
    pub(crate) fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>) -> Self {
        Self {
            permission,
            ask_tx,
            workspace: None,
        }
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    ) -> Self {
        self.workspace = workspace;
        self
    }

    fn filesystem_tools(
        &self,
        max_text_file_size: u64,
        max_read_lines: u64,
        max_grep_results: u64,
        max_find_results: u64,
        max_list_dir_entries: Option<u64>,
    ) -> Vec<Box<dyn rig::tool::ToolDyn>> {
        let read = tools::ReadTool::new(
            self.permission.clone(),
            self.ask_tx.clone(),
            Some(max_text_file_size),
            max_read_lines,
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
        vec![
            Box::new(read),
            Box::new(grep),
            Box::new(find),
            Box::new(list),
        ]
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
    // OpenRouter `provider.order` pin for `anthropic/*` (see `AnyClient::completion_model`).
    additional_params: Option<serde_json::Value>,
    #[cfg(feature = "archmd")] architecture: Option<&str>,
) -> Agent<M> {
    let mut preamble = build_explore_preamble(
        #[cfg(feature = "archmd")]
        architecture,
    );

    if let Some(s) = crate::session::storage::load_suffix() {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(&s);
    }

    let tools = authorization.filesystem_tools(
        max_text_file_size,
        max_read_lines,
        max_grep_results,
        max_find_results,
        max_list_dir_entries,
    );
    #[cfg(feature = "memory")]
    let tools = {
        let mut tools = tools;
        tools.extend(subagent_memory_tools(authorization));
        tools
    };

    #[cfg(feature = "hooks")]
    let tools = crate::extras::hooks::wrap_from_global(tools, authorization.permission.clone());

    let mut builder = AgentBuilder::new(model)
        .preamble(&preamble)
        .default_max_turns(max_turns)
        .tools(tools);

    if let Some(params) = additional_params {
        builder = builder.additional_params(params);
    }

    builder.build()
}

fn build_explore_preamble(#[cfg(feature = "archmd")] architecture: Option<&str>) -> String {
    let mut preamble = prompt::explore_prompt();
    #[cfg(feature = "archmd")]
    if let Some(arch) = architecture
        && !arch.is_empty()
    {
        preamble.push_str("\n\n");
        preamble.push_str(arch);
    }
    preamble
}

pub(crate) async fn build_explore_agent(
    model: AnyModel,
    max_turns: usize,
    cfg: &crate::config::Config,
    authorization: SubagentAuthorization,
    #[cfg(feature = "archmd")] architecture: Option<String>,
) -> AnyAgent {
    let max_text_file_size = cfg.max_text_file_size.unwrap_or(10 * 1024 * 1024);
    let max_read_lines = cfg.resolve_subagent_max_read_lines();
    let max_grep_results = cfg.resolve_subagent_max_grep_results();
    let max_find_results = cfg.resolve_subagent_max_find_results();
    let max_list_dir_entries = cfg.resolve_subagent_max_list_dir_entries();
    #[cfg(feature = "archmd")]
    let arch_ref = architecture.as_deref();
    let inner = match model {
        AnyModel::OpenRouter(m, extra) => AnyAgentInner::OpenRouter(build_explore_agent_inner(
            m,
            max_turns,
            max_text_file_size,
            max_read_lines,
            max_grep_results,
            max_find_results,
            max_list_dir_entries,
            &authorization,
            extra,
            #[cfg(feature = "archmd")]
            arch_ref,
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
                None,
                #[cfg(feature = "archmd")]
                arch_ref,
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
                None,
                #[cfg(feature = "archmd")]
                arch_ref,
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
            None,
            #[cfg(feature = "archmd")]
            arch_ref,
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
            None,
            #[cfg(feature = "archmd")]
            arch_ref,
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
            None,
            #[cfg(feature = "archmd")]
            arch_ref,
        )),
    };
    AnyAgent::without_skills(inner)
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
        let first = build_explore_preamble(Some("FIRST_SESSION_ARCHITECTURE"));
        let second = build_explore_preamble(Some("SECOND_SESSION_ARCHITECTURE"));
        assert!(first.contains("FIRST_SESSION_ARCHITECTURE"));
        assert!(!first.contains("SECOND_SESSION_ARCHITECTURE"));
        assert!(second.contains("SECOND_SESSION_ARCHITECTURE"));
        assert!(!second.contains("FIRST_SESSION_ARCHITECTURE"));
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
        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(working_dir.to_path_buf()),
            Some(vec!["standard".to_string()]),
        );
        SubagentAuthorization::new(Some(Arc::new(Mutex::new(checker))), ask_tx)
    }

    fn filesystem_tools(authorization: &SubagentAuthorization) -> Vec<Box<dyn rig::tool::ToolDyn>> {
        authorization.filesystem_tools(1024 * 1024, 100, 100, 100, Some(100))
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
