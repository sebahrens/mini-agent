use compact_str::CompactString;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

#[cfg(feature = "acp")]
tokio::task_local! {
    static CURRENT_TOOL_CALL: Option<String>;
    static TOOL_CALL_CONTEXT: std::cell::RefCell<std::collections::HashMap<String, std::collections::VecDeque<String>>>;
}

#[cfg(feature = "acp")]
pub(crate) async fn scope_tool_call_context<F: std::future::Future>(future: F) -> F::Output {
    TOOL_CALL_CONTEXT
        .scope(
            std::cell::RefCell::new(std::collections::HashMap::new()),
            future,
        )
        .await
}

#[cfg(feature = "acp")]
pub(crate) fn record_tool_call(tool: &str, id: &str) {
    let _ = TOOL_CALL_CONTEXT.try_with(|context| {
        context
            .borrow_mut()
            .entry(tool.to_string())
            .or_default()
            .push_back(id.to_string());
    });
}

#[cfg(feature = "acp")]
pub(crate) fn finish_tool_call(tool: &str, id: &str) {
    let _ = TOOL_CALL_CONTEXT.try_with(|context| {
        let mut context = context.borrow_mut();
        if let Some(ids) = context.get_mut(tool)
            && let Some(index) = ids.iter().position(|candidate| candidate == id)
        {
            ids.remove(index);
        }
    });
}

#[cfg(feature = "acp")]
fn take_tool_call_id(tool: &str) -> Option<String> {
    TOOL_CALL_CONTEXT
        .try_with(|context| {
            context
                .borrow_mut()
                .get_mut(tool)
                .and_then(std::collections::VecDeque::pop_front)
        })
        .ok()
        .flatten()
}

/// Rig enters registered tool futures in batch order. Claim the pending ID
/// before a call can suspend (including on the concurrency lane), then keep it
/// local to that future through every hook, path, or repeated approval check.
#[cfg(feature = "acp")]
pub(crate) async fn scope_tool_call<F: std::future::Future>(tool: String, future: F) -> F::Output {
    let id = take_tool_call_id(&tool);
    CURRENT_TOOL_CALL.scope(id, future).await
}

#[cfg(feature = "acp")]
pub(crate) fn current_tool_call_id() -> Option<String> {
    CURRENT_TOOL_CALL.try_with(Clone::clone).ok().flatten()
}

pub type AskSender = mpsc::Sender<UserRequest>;
pub type AskReceiver = mpsc::Receiver<UserRequest>;

tokio::task_local! {
    static SUBAGENT_ORIGIN: (String, bool);
}

#[cfg(feature = "subagents")]
pub(crate) async fn scope_subagent<F: std::future::Future>(
    name: String,
    write: bool,
    future: F,
) -> F::Output {
    SUBAGENT_ORIGIN.scope((name, write), future).await
}

pub(crate) fn subagent_name() -> Option<String> {
    SUBAGENT_ORIGIN.try_with(|origin| origin.0.clone()).ok()
}

#[cfg(all(feature = "hooks", any(feature = "subagents", test)))]
pub(crate) fn subagent_mode() -> &'static str {
    if SUBAGENT_ORIGIN.try_with(|origin| origin.1).unwrap_or(false) {
        "write"
    } else {
        "read"
    }
}

#[cfg(feature = "mcp")]
tokio::task_local! {static MCP_ASK_ORIGIN: Option<crate::extras::mcp::elicitation::AskOrigin>;}
#[cfg(feature = "mcp")]
pub(crate) async fn scope_mcp_ask<F: std::future::Future>(
    origin: Option<crate::extras::mcp::elicitation::AskOrigin>,
    future: F,
) -> F::Output {
    MCP_ASK_ORIGIN.scope(origin, future).await
}

#[cfg(feature = "mcp")]
pub(crate) fn current_mcp_origin_valid() -> bool {
    MCP_ASK_ORIGIN
        .try_with(|origin| origin.as_ref().is_none_or(|origin| (origin.valid)()))
        .unwrap_or(true)
}

/// Frontend transport only. Question replies cannot be used as approvals.
#[derive(Debug)]
pub enum UserRequest {
    Permission(AskRequest),
    Question(QuestionRequest),
    #[cfg(feature = "mcp")]
    McpElicitation(crate::extras::mcp::elicitation::Request),
    #[cfg(feature = "mcp")]
    McpPermission(crate::extras::mcp::elicitation::PermissionRequest),
}

impl From<AskRequest> for UserRequest {
    fn from(request: AskRequest) -> Self {
        #[cfg(feature = "mcp")]
        if let Some(origin) = MCP_ASK_ORIGIN.try_with(Clone::clone).ok().flatten() {
            return Self::McpPermission(crate::extras::mcp::elicitation::PermissionRequest {
                request,
                origin,
            });
        }
        Self::Permission(request)
    }
}

impl UserRequest {
    #[cfg(feature = "subagents")]
    pub(crate) fn background_for(&self, session: &crate::session::Session) -> bool {
        match self {
            Self::Permission(request) => request
                .background
                .as_ref()
                .is_some_and(|origin| origin.valid(session)),
            #[cfg(feature = "mcp")]
            Self::McpPermission(request) => request
                .request
                .background
                .as_ref()
                .is_some_and(|origin| origin.valid(session)),
            _ => false,
        }
    }

    pub(crate) fn cancel(self) {
        match self {
            Self::Permission(request) => {
                let _ = request.reply.send(UserDecision::Deny);
            }
            Self::Question(request) => {
                let _ = request
                    .reply
                    .send(crate::question::QuestionAnswer::Cancelled);
            }
            #[cfg(feature = "mcp")]
            Self::McpElicitation(request) => {
                let _ = request
                    .question
                    .reply
                    .send(crate::question::QuestionAnswer::Cancelled);
            }
            #[cfg(feature = "mcp")]
            Self::McpPermission(request) => {
                let _ = request.request.reply.send(UserDecision::Deny);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn into_permission(self) -> AskRequest {
        match self {
            Self::Permission(request) => request,
            Self::Question(_) => panic!("expected permission request, received question"),
            #[cfg(feature = "mcp")]
            Self::McpElicitation(_) => panic!("expected permission request, received MCP form"),
            #[cfg(feature = "mcp")]
            Self::McpPermission(request) => request.request,
        }
    }
}

#[derive(Debug)]
pub struct QuestionRequest {
    pub(crate) id: String,
    pub(crate) questions: crate::question::QuestionBatch,
    #[cfg(feature = "acp")]
    pub(crate) tool_call_id: Option<String>,
    pub(crate) reply: oneshot::Sender<crate::question::QuestionAnswer>,
}

/// Closed display-only classification captured with the actual policy decision.
/// It cannot authorize a tool, persist a grant, or contain configured secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskCause {
    ModePolicy,
    ConfiguredAskRule,
    #[cfg(feature = "hooks")]
    PreToolUseHook,
    DestructiveShell,
    DoomLoop,
    ExternalDirectory,
    GitExecutionMetadata,
}

impl AskCause {
    pub fn label(self) -> &'static str {
        match self {
            Self::ModePolicy => "mode policy",
            Self::ConfiguredAskRule => "configured Ask rule",
            #[cfg(feature = "hooks")]
            Self::PreToolUseHook => "PreToolUse hook Ask",
            Self::DestructiveShell => "destructive shell classification",
            Self::DoomLoop => "repeated-call policy",
            Self::ExternalDirectory => "external-directory policy",
            Self::GitExecutionMetadata => "Git execution-metadata protection",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskDiagnostic {
    pub effective_mode: super::SecurityMode,
    pub selected_mode: super::SecurityMode,
    pub cause: AskCause,
}

impl std::fmt::Display for AskDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "effective mode: {}; selected mode: {}; cause: {}",
            self.effective_mode,
            self.selected_mode,
            self.cause.label()
        )?;
        if self.effective_mode != self.selected_mode {
            f.write_str("; prompt mode narrowed the selected mode")?;
        }
        Ok(())
    }
}

impl AskDiagnostic {
    pub(crate) fn noninteractive_error(self) -> String {
        format!("Permission denied (non-interactive mode); {self}")
    }
}

#[derive(Debug)]
pub struct AskRequest {
    /// Snapshot from the decision, never reconstructed from current UI state.
    pub diagnostic: Option<AskDiagnostic>,
    #[cfg(feature = "subagents")]
    pub(crate) background: Option<crate::extras::subagents::background::AskOrigin>,
    /// Display attribution only; tool/input and grant patterns remain exact.
    pub agent_name: Option<String>,
    pub tool: CompactString,
    pub input: String,
    /// Internal call identity emitted to ACP for this exact tool call.
    #[cfg(feature = "acp")]
    pub tool_call_id: Option<String>,
    /// Optional caller-supplied AllowAlways scope when the operation knows a
    /// safer boundary than the generic UI heuristic.
    pub suggested_pattern: Option<String>,
    /// Additional scopes persisted with AllowAlways. This lets a project-tree
    /// grant cover both the exact root and its descendants without widening to
    /// the parent directory.
    pub additional_allow_patterns: Vec<String>,
    pub reply: oneshot::Sender<UserDecision>,
}

/// Keep hook observation inside the caller's existing cancellable work scope,
/// before the frontend adopts the exact reply sender. Hook output never grants.
#[cfg_attr(not(feature = "subagents"), allow(unused_mut))]
pub(crate) async fn send_permission_request(
    sender: &AskSender,
    mut request: AskRequest,
) -> Result<(), mpsc::error::SendError<UserRequest>> {
    #[cfg(feature = "subagents")]
    {
        request.background =
            crate::spend::run::Run::current().and_then(|run| run.background_ask_origin());
    }
    #[cfg(feature = "hooks")]
    if !request.reply.is_closed() {
        crate::startup::heap_future(|| {
            crate::extras::hooks::dispatch_permission_request(&request.tool, &request.input)
        })
        .await;
    }
    sender.send(request.into()).await
}

#[derive(Debug, Clone)]
pub enum UserDecision {
    AllowOnce,
    AllowAlways(String),
    Deny,
}

#[cfg(all(test, feature = "acp"))]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rig::agent::AgentBuilder;
    use rig::test_utils::{MockCompletionModel, MockStreamEvent};
    use rig::tool::Tool;

    use super::*;
    use crate::agent::tools::{ToolError, check_perm};
    use crate::permission::checker::{PermCheck, PermissionChecker};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    struct PermissionProbe {
        permission: PermCheck,
        ask_tx: AskSender,
        release_slow: Arc<tokio::sync::Notify>,
    }

    #[derive(serde::Deserialize)]
    struct ProbeArgs {
        input: String,
    }

    impl Tool for PermissionProbe {
        const NAME: &'static str = "read";
        type Error = ToolError;
        type Args = ProbeArgs;
        type Output = String;

        fn description(&self) -> String {
            "Exercise the production approval path".into()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]})
        }

        async fn call(&self, args: ProbeArgs) -> Result<String, ToolError> {
            let input = args.input;
            if input == "slow" {
                self.release_slow.notified().await;
            }
            let permission = Some(self.permission.clone());
            let ask_tx = Some(self.ask_tx.clone());
            check_perm(&permission, &ask_tx, "read", &input).await?;
            if input == "fast" {
                check_perm(&permission, &ask_tx, "read", "fast-again").await?;
                self.release_slow.notify_one();
            }
            Ok(input)
        }
    }

    #[tokio::test]
    async fn approval_identity_follows_dispatched_calls_across_parallel_and_repeated_checks() {
        let permission = Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(PermissionConfig {
                    read: Some(ToolPerm::Granular(HashMap::from([
                        ("allowed".into(), Action::Allow),
                        ("*".into(), Action::Ask),
                    ]))),
                    ..PermissionConfig::default()
                }),
                SecurityMode::Standard,
                Some(std::env::current_dir().unwrap()),
                Some(vec!["standard".into()]),
            )
            .unwrap(),
        ));
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(4);
        let tools = crate::agent::tools::concurrency::bind(vec![Box::new(PermissionProbe {
            permission: permission.clone(),
            ask_tx: ask_tx.clone(),
            release_slow: Arc::new(tokio::sync::Notify::new()),
        })]);
        let mut batch: Vec<_> = ["allowed", "slow", "fast"]
            .into_iter()
            .map(|input| {
                MockStreamEvent::tool_call(input, "read", serde_json::json!({"input": input}))
            })
            .collect();
        batch.push(MockStreamEvent::final_response_with_default_usage());
        let model = MockCompletionModel::from_stream_turns(vec![
            batch,
            vec![
                MockStreamEvent::text("finished"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model)
            .tools(tools)
            .default_max_turns(3)
            .build();
        let mut runner = crate::agent::runner::spawn_agent(
            agent,
            "check approvals".into(),
            Vec::new(),
            crate::retry::RetryConfig::default(),
            None,
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut calls = HashMap::new();
        let mut approvals = Vec::new();
        let mut results = HashMap::new();
        let mut finished = false;
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    Some(ask) = ask_rx.recv() => {
                        let ask = ask.into_permission();
                        let decision = if ask.input == "slow" { UserDecision::Deny } else { UserDecision::AllowOnce };
                        approvals.push((ask.input, ask.tool_call_id));
                        let _ = ask.reply.send(decision);
                    }
                    event = runner.event_rx.recv() => match event {
                        Some(crate::event::AgentEvent::ToolCall { id, args, .. }) => {
                            calls.insert(args["input"].as_str().unwrap().to_owned(), id.to_string());
                        }
                        Some(crate::event::AgentEvent::ToolResult { id, output, .. }) => {
                            results.insert(id.to_string(), output.to_string());
                        }
                        Some(crate::event::AgentEvent::Done { .. }) => finished = true,
                        None => break,
                        Some(_) => {}
                    }
                }
            }
        }).await;
        runner.abort_handle.abort();
        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            while runner.event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(outcome.is_ok(), "parallel approval fixture must terminate");
        assert!(settled.is_ok(), "runner cleanup must settle");
        assert!(finished);
        assert_eq!(calls.len(), 3);
        assert_eq!(
            approvals,
            vec![
                ("fast".into(), Some(calls["fast"].clone())),
                ("fast-again".into(), Some(calls["fast"].clone())),
                ("slow".into(), Some(calls["slow"].clone())),
            ]
        );
        assert_eq!(results.len(), 3);
        assert_eq!(results[&calls["allowed"]], "allowed");
        assert_eq!(results[&calls["fast"]], "fast");
        assert!(results[&calls["slow"]].contains("Permission denied by user"));

        // Direct integrations lack runner lifecycle IDs. Both legacy entry
        // points must retain the synthetic-approval fallback after a turn.
        let direct = crate::agent::tools::concurrency::bind(vec![Box::new(PermissionProbe {
            permission,
            ask_tx,
            release_slow: Arc::new(tokio::sync::Notify::new()),
        })]);
        for with_extensions in [false, true] {
            let call = async {
                let args = serde_json::json!({"input": "fast"}).to_string();
                if with_extensions {
                    direct[0]
                        .call_with_extensions(args, &rig::tool::ToolCallExtensions::new())
                        .await
                } else {
                    direct[0].call(args).await
                }
            };
            let respond = async {
                let mut ids = Vec::new();
                for _ in 0..2 {
                    let ask = ask_rx.recv().await.unwrap().into_permission();
                    ids.push(ask.tool_call_id);
                    let _ = ask.reply.send(UserDecision::AllowOnce);
                }
                ids
            };
            let (result, ids) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(call, respond)
            })
            .await
            .expect("direct approval fallback must settle");
            assert_eq!(result.unwrap(), "fast");
            assert_eq!(ids, vec![None, None]);
        }
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use crate::permission::checker::{PermCheck, PermissionChecker};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
    use std::sync::{Arc, Mutex};

    const BENIGN_HEREDOC: &str = "python3 - <<'PY'\nfrom pathlib import Path\nprint(Path('README.md').read_text()[:20])\nPY\necho done\nwc -l README.md | tail -1 > /tmp/owned-report.txt";

    fn checker(mode: SecurityMode, action: Option<Action>) -> PermCheck {
        Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(PermissionConfig {
                    bash: action.map(ToolPerm::Simple),
                    ..Default::default()
                }),
                mode,
                std::env::current_dir().ok(),
                Some(
                    SecurityMode::NAMES
                        .iter()
                        .map(|name| name.to_string())
                        .collect(),
                ),
            )
            .unwrap(),
        ))
    }

    async fn expect_ask(permission: PermCheck, input: &str, expected: AskDiagnostic) {
        let (sender, mut receiver) = mpsc::channel(2);
        let optional_permission = Some(permission.clone());
        let optional_sender = Some(sender);
        let call =
            crate::agent::tools::check_perm(&optional_permission, &optional_sender, "shell", input);
        tokio::pin!(call);
        let request = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                request = receiver.recv() => request.expect("approval channel closed").into_permission(),
                result = &mut call => panic!("completed before approval: {result:?}"),
            }
        }).await.expect("actual checker approval was not delivered");
        assert_eq!(request.tool, "shell");
        assert_eq!(request.input, input);
        assert_eq!(request.diagnostic, Some(expected));
        // The displayed snapshot must survive later policy changes while the
        // exact request is queued; never re-read a renderer's current mode.
        permission.lock().unwrap().set_mode(SecurityMode::Yolo);
        assert_eq!(request.diagnostic, Some(expected));
        request.reply.send(UserDecision::Deny).unwrap();
        assert!(
            call.await
                .unwrap_err()
                .to_string()
                .contains("Permission denied by user")
        );
        assert!(receiver.try_recv().is_err(), "one decision sends one Ask");
    }

    #[tokio::test]
    async fn permission126_benign_heredoc_yolo_zero_ask_standard_one() {
        let permission = Some(checker(SecurityMode::Yolo, None));
        let (sender, mut receiver) = mpsc::channel(1);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::agent::tools::check_perm(&permission, &Some(sender), "shell", BENIGN_HEREDOC),
        )
        .await
        .expect("unmatched Yolo check must not wait for approval")
        .unwrap();
        assert!(
            receiver.try_recv().is_err(),
            "unmatched Yolo must send zero Ask"
        );
        expect_ask(
            checker(SecurityMode::Standard, None),
            BENIGN_HEREDOC,
            AskDiagnostic {
                effective_mode: SecurityMode::Standard,
                selected_mode: SecurityMode::Standard,
                cause: AskCause::ModePolicy,
            },
        )
        .await;
    }

    #[tokio::test]
    async fn permission126_explicit_ask_destructive_and_prompt_narrowing_are_distinct() {
        expect_ask(
            checker(SecurityMode::Yolo, Some(Action::Ask)),
            BENIGN_HEREDOC,
            AskDiagnostic {
                effective_mode: SecurityMode::Yolo,
                selected_mode: SecurityMode::Yolo,
                cause: AskCause::ConfiguredAskRule,
            },
        )
        .await;
        expect_ask(
            checker(SecurityMode::Yolo, None),
            "rm -rf owned-output",
            AskDiagnostic {
                effective_mode: SecurityMode::Yolo,
                selected_mode: SecurityMode::Yolo,
                cause: AskCause::DestructiveShell,
            },
        )
        .await;
        let narrowed = checker(SecurityMode::Yolo, None);
        assert!(
            narrowed
                .lock()
                .unwrap()
                .set_prompt_mode(SecurityMode::Standard)
        );
        let expected = AskDiagnostic {
            effective_mode: SecurityMode::Standard,
            selected_mode: SecurityMode::Yolo,
            cause: AskCause::ModePolicy,
        };
        assert!(
            expected
                .to_string()
                .contains("prompt mode narrowed the selected mode")
        );
        expect_ask(narrowed, BENIGN_HEREDOC, expected).await;
    }

    #[test]
    fn permission126_path_and_repeated_call_causes_follow_actual_decisions() {
        let root = crate::tests::OwnedTestDirectory::new("permission126-cause");
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(PermissionConfig {
                doom_loop: Some(Action::Ask),
                ..Default::default()
            }),
            SecurityMode::Standard,
            Some(root.path().to_owned()),
            None,
        )
        .unwrap();
        let target = root
            .path()
            .join(".git/config")
            .to_string_lossy()
            .into_owned();
        let (result, diagnostic) =
            checker.check_diagnosed(|checker| checker.check_bound_path("write", &target));
        assert_eq!(result, crate::permission::checker::CheckResult::Ask);
        assert_eq!(diagnostic.cause, AskCause::GitExecutionMetadata);
        let mut saw_doom_loop = false;
        for _ in 0..8 {
            let (result, diagnostic) =
                checker.check_diagnosed(|checker| checker.check("shell", "echo repeated"));
            assert_eq!(result, crate::permission::checker::CheckResult::Ask);
            saw_doom_loop |= diagnostic.cause == AskCause::DoomLoop;
        }
        assert!(saw_doom_loop, "repeated-call Ask was not attributed");
        let external = crate::tests::OwnedTestDirectory::new("permission126-external");
        let input = external
            .path()
            .join("data.txt")
            .to_string_lossy()
            .into_owned();
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(PermissionConfig {
                external_directory: Some(std::collections::HashMap::from([(
                    format!("{}/**", external.path().display()),
                    Action::Ask,
                )])),
                ..Default::default()
            }),
            SecurityMode::Standard,
            Some(root.path().to_owned()),
            None,
        )
        .unwrap();
        let (result, diagnostic) =
            checker.check_diagnosed(|checker| checker.check_path("read", &input));
        assert_eq!(result, crate::permission::checker::CheckResult::Ask);
        assert_eq!(diagnostic.cause, AskCause::ExternalDirectory);
    }

    #[tokio::test]
    async fn permission126_deny_precedence_and_headless_cause_are_preserved() {
        let (sender, mut receiver) = mpsc::channel(1);
        let denied = crate::agent::tools::check_perm(
            &Some(checker(SecurityMode::Yolo, Some(Action::Deny))),
            &Some(sender),
            "shell",
            BENIGN_HEREDOC,
        )
        .await
        .unwrap_err();
        assert_eq!(
            denied.to_string(),
            "Permission denied: Blocked by deny rule"
        );
        assert!(receiver.try_recv().is_err());
        let error = crate::agent::tools::check_perm(
            &Some(checker(SecurityMode::Yolo, Some(Action::Ask))),
            &None,
            "shell",
            BENIGN_HEREDOC,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Permission denied (non-interactive mode); effective mode: yolo; selected mode: yolo; cause: configured Ask rule"
        );
        assert!(!error.to_string().contains("README"));
    }
}
