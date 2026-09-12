//! Persistence contract for successful and failed agent turns.
//!
//! - mini-agent-h41j: the interactive UI persists tool calls and results live
//!   (`AgentEvent::ToolCall` / `AgentEvent::ToolResult`, with the real tool
//!   name and the runner's lifecycle id). Committing the turn's final
//!   response must not persist the `Done { interactions }` batch a second
//!   time.
//! - mini-agent-ibwm: both terminal handlers adopt provider identity and
//!   reasoning from the canonical batch, including after a provider failure.
//! - mini-agent-ut1v: headless `-p` persists the turn from the returned
//!   interactions and must write tool records before the assistant message
//!   (the order the interactive UI produces, so `--continue` replays match),
//!   attributing each result to its real tool name.
//! - mini-agent-i9rh: session id previews must not slice into a char boundary
//!   or past the end of a short (imported) id.

use rig::OneOrMany;
use rig::completion::Message;
use rig::message::{AssistantContent, ToolResult, ToolResultContent, UserContent};

use crate::print::{persist_headless_turn, short_session_id};
use crate::session::{MessageRole, PersistedToolMessage, Session};

fn session() -> Session {
    Session::new("anthropic", "claude-test", 200_000, "")
}

fn roles(session: &Session) -> Vec<MessageRole> {
    session.messages.iter().map(|m| m.role).collect()
}

fn tool_call_message(id: &str, name: &str, args: serde_json::Value) -> Message {
    Message::Assistant {
        id: None,
        content: OneOrMany::one(AssistantContent::tool_call(id, name, args)),
    }
}

#[tokio::test]
async fn interactive_terminal_events_preserve_provider_replay_without_duplicate_tools() {
    use crate::event::AgentEvent;
    use crate::ui::state::{AgentRunState, ChainState, PendingMainTurn, SlashState, UiContext};
    use clap::Parser;
    use rig::message::{Reasoning, ToolCall, ToolFunction};

    for failed in [false, true] {
        for native_identity in [true, false] {
            let mut session = session();
            let workspace = std::sync::Arc::new(
                crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
            );
            // Empty explicit context: this handler test must not load personal
            // prompts, credentials, memory, or contact a provider.
            let mut context = crate::context::ContextFiles {
                workspace_root: workspace.root().to_path_buf(),
                agents: None,
                prompts: Default::default(),
                current_prompt: None,
                current_prompt_name: None,
                agent_definitions: Default::default(),
                current_agent_name: None,
                current_agent_explicit: false,
                themes: Default::default(),
                current_theme_name: None,
                extra_files: Vec::new(),
                extra_file_contents: Default::default(),
                one_shot_restore: None,
                chain_declined: Vec::new(),
                #[cfg(feature = "memory")]
                memory: None,
                #[cfg(feature = "archmd")]
                architecture: None,
            };
            let cli = crate::cli::Cli::parse_from(["mini-agent", "--no-session"]);
            let cfg = crate::config::Config::default();
            let client = crate::provider::AnyClient::OpenRouter(
                rig::providers::openrouter::Client::new("unused-test-key").unwrap(),
            );
            let mut run = AgentRunState::default();
            let pending = PendingMainTurn::capture(&session, "read it");
            crate::ui::mark_main_turn_started(&mut session, &mut run, pending);
            run.is_running = true;
            let mut ui = UiContext::new(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace,
                client,
                None,
                None,
                crate::sandbox::Sandbox::new(false, "bwrap"),
                None,
            );
            let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
            let slash = SlashState {
                show_reasoning: false,
                reasoning_enabled: false,
                todo_tools_enabled: false,
            };
            let mut chain = ChainState::default();
            #[cfg(any(feature = "loop", feature = "goal"))]
            let (validation_tx, _validation_rx) = tokio::sync::mpsc::channel(1);
            let args = serde_json::json!({"path": "src/main.rs"});
            let mut call = ToolCall::new(
                "fc_provider_1".into(),
                ToolFunction {
                    name: "read".into(),
                    arguments: args.clone(),
                },
            );
            if native_identity {
                call.call_id = Some("call_provider_1".into());
            }
            let reasoning =
                Reasoning::new_with_signature("checking first", Some("signature".into()))
                    .with_id("rs_provider_1".into());
            let interactions = vec![
                Message::Assistant {
                    id: None,
                    content: OneOrMany::many(vec![
                        AssistantContent::Reasoning(reasoning.clone()),
                        AssistantContent::ToolCall(call),
                    ])
                    .unwrap(),
                },
                Message::User {
                    content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                        id: "fc_provider_1".into(),
                        call_id: native_identity.then(|| "call_provider_1".into()),
                        content: OneOrMany::one(ToolResultContent::text("fn main() {}")),
                    })),
                },
            ];
            let terminal = if failed {
                AgentEvent::error_with("provider failed after tool execution", interactions)
            } else {
                AgentEvent::Done {
                    response: "done".into(),
                    interactions,
                }
            };
            for event in [
                AgentEvent::ToolCall {
                    id: "lifecycle-1".into(),
                    name: "read".into(),
                    args: args.clone(),
                },
                AgentEvent::ToolResult {
                    id: "lifecycle-1".into(),
                    name: "read".into(),
                    output: "fn main() {}".into(),
                },
                #[cfg(feature = "subagents")]
                AgentEvent::SubagentToolCall {
                    name: "grep".into(),
                    args: serde_json::json!({"pattern":"needle", "path":"src", "nested":{"keep":"raw"}}),
                },
                AgentEvent::Token("done".into()),
                terminal,
            ] {
                crate::ui::event_handler::handle_agent_event(
                    event,
                    &mut renderer,
                    &mut run,
                    &mut ui,
                    &slash,
                    &mut chain,
                    #[cfg(any(feature = "loop", feature = "goal"))]
                    &validation_tx,
                )
                .await
                .unwrap();
            }
            let session = &*ui.session;
            assert_eq!(
                roles(session),
                [
                    MessageRole::User,
                    MessageRole::ToolCall,
                    MessageRole::ToolResult,
                    #[cfg(feature = "subagents")]
                    MessageRole::SubagentToolCall,
                    MessageRole::Assistant
                ]
            );
            assert_eq!(session.messages.last().unwrap().content, "done");
            let identifier = if native_identity {
                "call_provider_1"
            } else {
                "lifecycle-1"
            };
            assert_eq!(
                session.messages[1].tool_call_id.as_deref(),
                Some(identifier)
            );
            assert_eq!(
                session.messages[2].tool_call_id.as_deref(),
                Some(identifier)
            );
            assert_eq!(
                session.messages[1].tool,
                Some(PersistedToolMessage::Call {
                    name: "read".into(),
                    arguments: args,
                })
            );
            assert_eq!(
                session.messages[2].tool,
                Some(PersistedToolMessage::Result {
                    output: "fn main() {}".into(),
                    artifact_path: None,
                })
            );
            assert!(session.messages[2].content.starts_with("read:\n"));
            // Serialize and replay to exercise the actual continuation path,
            // including provider reasoning rather than only a side-map count.
            let saved = serde_json::to_vec(session).unwrap();
            let loaded: Session = serde_json::from_slice(&saved).unwrap();
            let history = crate::agent::runner::convert_history(&loaded);
            #[cfg(feature = "subagents")]
            {
                let record = &loaded.messages[3];
                let expected =
                    serde_json::json!({"pattern":"needle", "path":"src", "nested":{"keep":"raw"}});
                assert_eq!(
                    record.tool,
                    Some(PersistedToolMessage::Call {
                        name: "grep".into(),
                        arguments: expected.clone()
                    })
                );
                let id = record
                    .tool_call_id
                    .as_deref()
                    .expect("nested call identity");
                assert!(id.starts_with(crate::session::SUBAGENT_TOOL_CALL_ID_PREFIX));
                let replayed: Vec<_> = history
                    .iter()
                    .flat_map(|message| match message {
                        Message::Assistant { content, .. } => content
                            .iter()
                            .filter_map(|part| match part {
                                AssistantContent::ToolCall(call)
                                    if call.function.name == "grep" =>
                                {
                                    Some(call)
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .collect();
                assert_eq!(
                    replayed.len(),
                    1,
                    "nested call is neither dropped nor duplicated"
                );
                assert_eq!(replayed[0].function.arguments, expected);
                assert_eq!(replayed[0].call_id.as_deref(), Some(id));
            }

            let Message::Assistant { content, .. } = &history[1] else {
                panic!("missing replay call")
            };
            let parts: Vec<_> = content.iter().collect();
            let provenance = loaded
                .provenance_for_tool_call(identifier)
                .expect("terminal metadata retained");
            assert_eq!(provenance.reasoning[0].id.as_deref(), Some("rs_provider_1"));
            if native_identity {
                let [
                    AssistantContent::Reasoning(replayed_reasoning),
                    AssistantContent::ToolCall(replayed_call),
                ] = parts.as_slice()
                else {
                    panic!("failed={failed}: missing reasoning: {parts:?}")
                };
                assert_eq!(replayed_reasoning, &reasoning);
                assert_eq!(replayed_call.id, "fc_provider_1");
                assert_eq!(replayed_call.call_id.as_deref(), Some("call_provider_1"));
            } else {
                let [AssistantContent::ToolCall(replayed_call)] = parts.as_slice() else {
                    panic!("expected rewritten call: {parts:?}")
                };
                assert_eq!(replayed_call.id, "lifecycle-1");
                assert_eq!(replayed_call.call_id.as_deref(), Some("lifecycle-1"));
            }
            let Message::User { content } = &history[2] else {
                panic!("missing replay result")
            };
            let UserContent::ToolResult(result) = content.first() else {
                panic!("missing correlated tool output")
            };
            assert_eq!(result.call_id.as_deref(), Some(identifier));
            assert!(!run.is_running);
            assert!(run.response_buf.is_empty());
        }
    }
}

#[test]
fn headless_turn_persists_tool_records_before_assistant_message() {
    let mut session = session();
    let args = serde_json::json!({"path": "src/main.rs"});
    let interactions = vec![
        tool_call_message("provider-1", "read", args),
        Message::tool_result("provider-1", "fn main() {}"),
        Message::assistant("done"),
    ];

    persist_headless_turn(&mut session, "read it", "done", &interactions);

    assert_eq!(
        roles(&session),
        [
            MessageRole::User,
            MessageRole::ToolCall,
            MessageRole::ToolResult,
            MessageRole::Assistant,
        ],
        "headless order must match the interactive transcript order"
    );
    assert_eq!(session.messages[0].content, "read it");
    assert!(session.messages[1].content.contains("read"));
    assert_eq!(
        session.messages[1].tool_call_id.as_deref(),
        Some("provider-1")
    );
    assert!(
        session.messages[2].content.starts_with("read:\n"),
        "the result is attributed to its tool by provider id: {}",
        session.messages[2].content
    );
    assert_eq!(
        session.messages[2].tool_call_id.as_deref(),
        Some("provider-1")
    );
    assert_eq!(session.messages[3].content, "done");
    assert_eq!(
        session.messages[1].tool,
        Some(PersistedToolMessage::Call {
            name: "read".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        })
    );
    assert_eq!(
        session.messages[2].tool,
        Some(PersistedToolMessage::Result {
            output: "fn main() {}".into(),
            artifact_path: None,
        })
    );
}

#[test]
fn headless_text_only_turn_persists_single_assistant_message() {
    let mut session = session();
    persist_headless_turn(&mut session, "hi", "hello", &[Message::assistant("hello")]);
    assert_eq!(roles(&session), [MessageRole::User, MessageRole::Assistant]);
    assert_eq!(session.messages[1].content, "hello");
}

#[test]
fn headless_multi_part_tool_result_is_one_record() {
    let mut session = session();
    let result = Message::User {
        content: OneOrMany::one(UserContent::ToolResult(ToolResult {
            id: "provider-1".to_string(),
            call_id: None,
            content: OneOrMany::many(vec![
                ToolResultContent::text("first"),
                ToolResultContent::text("second"),
            ])
            .expect("two items"),
        })),
    };
    let interactions = vec![
        tool_call_message("provider-1", "grep", serde_json::json!({"pattern": "x"})),
        result,
        Message::assistant("done"),
    ];

    persist_headless_turn(&mut session, "go", "done", &interactions);

    assert_eq!(
        roles(&session),
        [
            MessageRole::User,
            MessageRole::ToolCall,
            MessageRole::ToolResult,
            MessageRole::Assistant,
        ]
    );
    assert_eq!(session.messages[2].content, "grep:\nfirst\nsecond");
}

#[test]
fn short_session_id_is_char_safe() {
    assert_eq!(short_session_id("0123456789"), "01234567");
    assert_eq!(short_session_id("abc"), "abc");
    assert_eq!(short_session_id(""), "");
    assert_eq!(short_session_id("ééééééééééé"), "éééééééé");
    assert_eq!(
        short_session_id("日本語のセッション識別子"),
        "日本語のセッショ"
    );
}
