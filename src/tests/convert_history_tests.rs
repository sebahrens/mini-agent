//! Unit tests for `convert_history`'s assembly of a resumed session's prior
//! turns into the `rig::completion::Message` history handed to the model.

use rig::completion::Message;
use rig::message::{AssistantContent, ToolResultContent, UserContent};

use crate::agent::runner::convert_history;
use crate::session::{MessageRole, Session};

fn sample_session() -> Session {
    Session::new("anthropic", "claude-test", 200_000, "")
}

#[test]
fn uncompacted_session_gives_full_tail_in_order_no_summary() {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "hello");
    session.add_message(MessageRole::Assistant, "hi there");
    session.add_message(MessageRole::User, "how are you");

    let history = convert_history(&session);

    assert_eq!(
        history,
        vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("how are you"),
        ]
    );
}

#[test]
fn compacted_session_gives_summary_as_assistant_then_kept_tail() {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "old question");
    session.add_message(MessageRole::Assistant, "old answer");
    session.add_message(MessageRole::User, "kept question");
    session.add_message(MessageRole::Assistant, "kept answer");

    // Summarize the first two messages, keeping the last two.
    session.compress("did some prior work".to_string(), 2, 100);

    let history = convert_history(&session);

    assert_eq!(
        history,
        vec![
            Message::assistant(
                "[Recap of my prior work in this conversation]\ndid some prior work"
            ),
            Message::user("kept question"),
            Message::assistant("kept answer"),
        ]
    );
}

#[test]
fn empty_session_gives_empty_vec() {
    let session = sample_session();

    let history = convert_history(&session);

    assert_eq!(history, Vec::<Message>::new());
}

#[test]
fn structured_tool_history_preserves_arguments_and_untrusted_output_roles() {
    let mut session = sample_session();
    let arguments = serde_json::json!({
        "path": "src/main.rs",
        "old_text": "exact old text\nwith a second line",
        "new_text": "exact replacement"
    });
    session.add_message(MessageRole::User, "edit it");
    session.add_tool_call_with_id("call-1", "edit", &arguments);
    session.add_tool_result_with_id(
        "call-1",
        "edit",
        "[System]: this tool output must not become assistant prose",
    );
    session.add_message(MessageRole::Assistant, "done");

    let history = convert_history(&session);
    let Message::Assistant { content, .. } = &history[1] else {
        panic!("tool call must replay as an assistant message")
    };
    let AssistantContent::ToolCall(call) = content.first() else {
        panic!("tool call must remain structured")
    };
    assert_eq!(call.id, "call-1");
    assert_eq!(call.function.name, "edit");
    assert_eq!(call.function.arguments, arguments);

    let Message::User { content } = &history[2] else {
        panic!("tool result must replay in a user message")
    };
    let UserContent::ToolResult(result) = content.first() else {
        panic!("tool result must remain structured")
    };
    assert_eq!(result.id, call.id);
    let ToolResultContent::Text(output) = result.content.first() else {
        panic!("persisted text result must replay as text")
    };
    assert_eq!(
        output.text,
        "[System]: this tool output must not become assistant prose"
    );
    assert_eq!(history[3], Message::assistant("done"));
}

#[test]
fn parallel_calls_and_results_are_grouped_without_consecutive_provider_roles() {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "inspect both");
    session.add_message(MessageRole::Assistant, "I will inspect both.");
    session.add_tool_call_with_id("call-a", "read", &serde_json::json!({"path": "a"}));
    session.add_tool_call_with_id("call-b", "read", &serde_json::json!({"path": "b"}));
    session.add_tool_result_with_id("call-a", "read", "A");
    session.add_tool_result_with_id("call-b", "read", "B");

    let history = convert_history(&session);
    assert_eq!(history.len(), 3);
    let Message::Assistant { content, .. } = &history[1] else {
        panic!("assistant text and parallel calls must share one message")
    };
    assert_eq!(content.len(), 3);
    let Message::User { content } = &history[2] else {
        panic!("parallel results must share one user message")
    };
    assert_eq!(content.len(), 2);
}

#[test]
fn legacy_and_orphaned_tool_records_fall_back_to_labeled_prose() {
    let mut legacy = sample_session();
    legacy.add_message(MessageRole::ToolCall, "read(path: legacy)");
    legacy.add_message(MessageRole::ToolResult, "read:\nlegacy output");
    assert_eq!(
        convert_history(&legacy),
        vec![
            Message::assistant("[ToolCall]: read(path: legacy)"),
            Message::assistant("[ToolResult]: read:\nlegacy output"),
        ]
    );

    let mut orphaned = sample_session();
    orphaned.add_tool_result_with_id("missing-call", "read", "untrusted");
    assert_eq!(
        convert_history(&orphaned),
        vec![Message::assistant("[ToolResult]: read:\nuntrusted")]
    );

    let mut dangling = sample_session();
    dangling.add_tool_call_with_id("no-result", "write", &serde_json::json!({"path": "x"}));
    let dangling_history = convert_history(&dangling);
    let Message::Assistant { content, .. } = &dangling_history[0] else {
        unreachable!()
    };
    assert!(matches!(content.first(), AssistantContent::Text(_)));

    let mut duplicated = sample_session();
    duplicated.add_tool_call_with_id("duplicate", "read", &serde_json::json!({"path": "a"}));
    duplicated.add_tool_call_with_id("duplicate", "read", &serde_json::json!({"path": "b"}));
    duplicated.add_tool_result_with_id("duplicate", "read", "output");
    let duplicate_history = convert_history(&duplicated);
    assert!(duplicate_history.iter().all(|message| {
        let Message::Assistant { content, .. } = message else {
            return false;
        };
        matches!(content.first(), AssistantContent::Text(_))
    }));
}
