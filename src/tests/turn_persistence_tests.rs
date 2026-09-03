//! Persistence contract for one completed agent turn.
//!
//! - mini-agent-h41j: the interactive UI persists tool calls and results live
//!   (`AgentEvent::ToolCall` / `AgentEvent::ToolResult`, with the real tool
//!   name and the runner's lifecycle id). Committing the turn's final
//!   response must not persist the `Done { interactions }` batch a second
//!   time.
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
use crate::session::{MessageRole, Session};
use crate::ui::event_handler::commit_turn_response;

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

#[test]
fn interactive_turn_persists_each_tool_interaction_once() {
    let mut session = session();
    session.add_message(MessageRole::User, "read it");
    // Exactly what `handle_agent_event`'s ToolCall / ToolResult arms persist
    // while the turn streams: the runner's lifecycle id and the real tool name.
    let args = serde_json::json!({"path": "src/main.rs"});
    session.add_tool_call_with_id("lifecycle-1", "read", &args);
    session.add_tool_result_with_id_and_artifact("lifecycle-1", "read", "fn main() {}");

    // The canonical provider batch the runner exposes on `Done`.
    let interactions = vec![
        tool_call_message("provider-1", "read", args),
        Message::tool_result("provider-1", "fn main() {}"),
        Message::assistant("done"),
    ];
    commit_turn_response(&mut session, "done", &interactions);

    assert_eq!(
        roles(&session),
        [
            MessageRole::User,
            MessageRole::ToolCall,
            MessageRole::ToolResult,
            MessageRole::Assistant,
        ],
        "one tool call turn yields exactly one ToolCall and one ToolResult record"
    );
    assert!(session.messages[1].content.contains("read"));
    assert_eq!(
        session.messages[1].tool_call_id.as_deref(),
        Some("lifecycle-1")
    );
    assert!(
        session.messages[2].content.starts_with("read:\n"),
        "the live record keeps the real tool name: {}",
        session.messages[2].content
    );
    assert!(
        !session
            .messages
            .iter()
            .any(|m| m.content.starts_with("unknown:")),
        "no unattributed duplicate result record"
    );
    assert_eq!(session.messages[3].content, "done");
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
