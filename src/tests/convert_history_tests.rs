//! Unit tests for `convert_history`'s assembly of a resumed session's prior
//! turns into the `rig::completion::Message` history handed to the model.

use rig::completion::Message;
use rig::message::{AssistantContent, ToolResultContent, UserContent};

use crate::agent::runner::{
    convert_history, convert_history_shared_with_tool_result_retention,
    convert_history_with_tool_result_retention,
};
use crate::session::{
    CLEARED_TOOL_RESULT_NOTICE, MessageRole, PersistedCallProvenance, PersistedReasoning,
    PersistedReasoningBlock, Session,
};

fn sample_session() -> Session {
    Session::new("anthropic", "claude-test", 200_000, "")
}

/// A complete Responses-API reasoning item: an `rs_...` id, a summary block and
/// the encrypted payload the API returns when reasoning is not stored.
fn reasoning_item(id: &str) -> PersistedReasoning {
    PersistedReasoning {
        id: Some(id.into()),
        blocks: vec![
            PersistedReasoningBlock::Summary {
                text: "checking the file first".into(),
            },
            PersistedReasoningBlock::Encrypted {
                data: "gAAAAABm-opaque".into(),
            },
        ],
    }
}

/// A session whose single tool call was persisted with the provider's own
/// `function_call` item id, its `call_id`, and the reasoning item emitted with
/// it — the shape both run modes write since mini-agent-wzv1.
fn session_with_native_call_and_reasoning() -> Session {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "edit it");
    session.add_message(MessageRole::Assistant, "I will edit it.");
    session.add_tool_call_with_id("call_abc123", "edit", &serde_json::json!({"path": "a.rs"}));
    session.add_tool_result_with_id("call_abc123", "edit", "ok");
    session.record_tool_call_provenance(
        "call_abc123",
        PersistedCallProvenance {
            provider_item_id: Some("fc_abc123".into()),
            provider_call_id: Some("call_abc123".into()),
            reasoning: vec![reasoning_item("rs_abc123")],
        },
    );
    session
}

#[test]
fn shared_history_conversion_reuses_snapshot_until_the_session_changes() {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "first question");
    let first = convert_history_shared_with_tool_result_retention(&session, 8);
    let repeated = convert_history_shared_with_tool_result_retention(&session, 8);
    assert!(std::sync::Arc::ptr_eq(&first, &repeated));

    session.add_message(MessageRole::Assistant, "first answer");
    let changed = convert_history_shared_with_tool_result_retention(&session, 8);
    assert!(!std::sync::Arc::ptr_eq(&first, &changed));
    assert_eq!(changed.len(), first.len() + 1);
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

#[test]
fn old_tool_results_are_cleared_without_dropping_calls_or_mutating_the_session() {
    let mut session = sample_session();
    for index in 0..4 {
        let id = format!("call-{index}");
        session.add_tool_call_with_id(
            &id,
            "read",
            &serde_json::json!({"path": format!("file-{index}")}),
        );
        session.add_tool_result_with_id(&id, "read", &format!("output-{index}"));
    }
    let durable_outputs = session
        .messages
        .iter()
        .filter_map(|message| match &message.tool {
            Some(crate::session::PersistedToolMessage::Result { output, .. }) => {
                Some(output.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    let history = convert_history_with_tool_result_retention(&session, 2);
    let calls = history
        .iter()
        .map(|message| match message {
            Message::Assistant { content, .. } => content
                .iter()
                .filter(|item| matches!(item, AssistantContent::ToolCall(_)))
                .count(),
            Message::User { .. } | Message::System { .. } => 0,
        })
        .sum::<usize>();
    let outputs = history
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content
                .iter()
                .filter_map(|item| match item {
                    UserContent::ToolResult(result) => match result.content.first() {
                        ToolResultContent::Text(text) => Some(text.text),
                        ToolResultContent::Image(_) => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Message::Assistant { .. } | Message::System { .. } => Vec::new(),
        })
        .collect::<Vec<_>>();

    assert_eq!(calls, 4, "all structured calls must remain replayable");
    assert_eq!(
        outputs,
        vec![
            CLEARED_TOOL_RESULT_NOTICE.to_string(),
            CLEARED_TOOL_RESULT_NOTICE.to_string(),
            "output-2".to_string(),
            "output-3".to_string(),
        ]
    );
    assert_eq!(
        durable_outputs,
        session
            .messages
            .iter()
            .filter_map(|message| match &message.tool {
                Some(crate::session::PersistedToolMessage::Result { output, .. }) => {
                    Some(output.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        "request-time pruning must not rewrite the durable transcript"
    );
}

/// The Responses API pairs a `function_call` with its output by `call_id`, and
/// rejects the request outright when a replayed call carries none.
#[test]
fn structured_tool_history_carries_call_id_for_the_responses_api() {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "edit it");
    session.add_tool_call_with_id("call-1", "edit", &serde_json::json!({"path": "a.rs"}));
    session.add_tool_result_with_id("call-1", "edit", "ok");

    let history = convert_history(&session);
    let Message::Assistant {
        content: assistant, ..
    } = &history[1]
    else {
        panic!("tool call must replay as an assistant message")
    };
    let AssistantContent::ToolCall(call) = assistant.first() else {
        panic!("tool call must remain structured")
    };
    let Message::User { content } = &history[2] else {
        panic!("tool result must replay in a user message")
    };
    let UserContent::ToolResult(result) = content.first() else {
        panic!("tool result must remain structured")
    };

    assert_eq!(call.call_id.as_deref(), Some("call-1"));
    assert_eq!(result.call_id.as_deref(), Some("call-1"));
    assert_eq!(call.call_id, result.call_id);
    assert!(
        !assistant
            .iter()
            .any(|item| matches!(item, AssistantContent::Reasoning(_))),
        "a call with no persisted reasoning must not invent one"
    );
}

/// A native `fc_...` id makes the Responses API demand the reasoning item that
/// was emitted with it. A session that stored no reasoning item for the call —
/// every session written before the provenance map existed, and every provider
/// that never sends one — must not present the call as a stored provider item.
#[test]
fn native_function_call_ids_are_rewritten_so_replay_needs_no_reasoning_item() {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "edit it");
    session.add_tool_call_with_id("fc_abc123", "edit", &serde_json::json!({"path": "a.rs"}));
    session.add_tool_result_with_id("fc_abc123", "edit", "ok");

    let history = convert_history(&session);
    let Message::Assistant {
        content: assistant, ..
    } = &history[1]
    else {
        panic!("tool call must replay as an assistant message")
    };
    let AssistantContent::ToolCall(call) = assistant.first() else {
        panic!("tool call must remain structured")
    };
    let Message::User { content } = &history[2] else {
        panic!("tool result must replay in a user message")
    };
    let UserContent::ToolResult(result) = content.first() else {
        panic!("tool result must remain structured")
    };

    assert!(
        !call.id.starts_with("fc_"),
        "a native function_call item id must not be replayed: {}",
        call.id
    );
    assert_eq!(call.call_id.as_deref(), Some("call_abc123"));
    assert_eq!(result.call_id, call.call_id);
    assert_eq!(result.id, call.id);
    assert!(
        !assistant
            .iter()
            .any(|item| matches!(item, AssistantContent::Reasoning(_))),
        "the rewritten identity must replay without a reasoning item"
    );
}

/// Provenance alone is not enough: without a replayable reasoning item (an old
/// record, or a provider that reports a `call_id` but no reasoning) the native
/// `fc_...` id would still be rejected, so the rewrite must still apply.
#[test]
fn provenance_without_a_reasoning_item_keeps_the_rewritten_identity() {
    let mut session = sample_session();
    session.add_message(MessageRole::User, "edit it");
    session.add_tool_call_with_id("call_abc123", "edit", &serde_json::json!({"path": "a.rs"}));
    session.add_tool_result_with_id("call_abc123", "edit", "ok");
    session.record_tool_call_provenance(
        "call_abc123",
        PersistedCallProvenance {
            provider_item_id: Some("fc_abc123".into()),
            provider_call_id: Some("call_abc123".into()),
            // A reasoning item with no provider id cannot be replayed as a
            // stored item, so it cannot license the native call id either.
            reasoning: vec![PersistedReasoning {
                id: None,
                blocks: vec![PersistedReasoningBlock::Summary {
                    text: "thinking".into(),
                }],
            }],
        },
    );

    let history = convert_history(&session);
    let Message::Assistant { content, .. } = &history[1] else {
        panic!("tool call must replay as an assistant message")
    };
    let AssistantContent::ToolCall(call) = content.first() else {
        panic!("tool call must remain structured")
    };
    assert_eq!(call.id, "call_abc123");
    assert_eq!(call.call_id.as_deref(), Some("call_abc123"));
    assert!(
        !content
            .iter()
            .any(|item| matches!(item, AssistantContent::Reasoning(_))),
        "an unidentified reasoning item must not be replayed"
    );
}

/// With the reasoning item persisted, the native provider ids are replayed
/// verbatim and the reasoning precedes the `function_call` it belongs to,
/// inside the same assistant message — the exact pairing the Responses API
/// validates.
#[test]
fn persisted_reasoning_replays_immediately_before_its_native_function_call() {
    let session = session_with_native_call_and_reasoning();

    let history = convert_history(&session);
    let Message::Assistant { content, .. } = &history[1] else {
        panic!("tool call must replay as an assistant message")
    };
    let items: Vec<&AssistantContent> = content.iter().collect();
    let [
        AssistantContent::Text(text),
        AssistantContent::Reasoning(reasoning),
        AssistantContent::ToolCall(call),
    ] = items.as_slice()
    else {
        panic!("expected assistant text, then reasoning, then the call: {items:?}")
    };
    assert_eq!(text.text, "I will edit it.");
    assert_eq!(reasoning.id.as_deref(), Some("rs_abc123"));
    assert_eq!(
        reasoning.content,
        vec![
            rig::message::ReasoningContent::Summary("checking the file first".to_string()),
            rig::message::ReasoningContent::Encrypted("gAAAAABm-opaque".to_string()),
        ],
        "the summary and the encrypted payload must both survive the round trip"
    );
    assert_eq!(
        call.id, "fc_abc123",
        "the native item id is replayable once its reasoning item is present"
    );
    assert_eq!(call.call_id.as_deref(), Some("call_abc123"));

    let Message::User { content } = &history[2] else {
        panic!("tool result must replay in a user message")
    };
    let UserContent::ToolResult(result) = content.first() else {
        panic!("tool result must remain structured")
    };
    assert_eq!(result.id, "fc_abc123");
    assert_eq!(result.call_id.as_deref(), Some("call_abc123"));
}

/// The same end-to-end guard as the rewritten path: rig's Responses request
/// builder must accept every replayed message, including the reasoning item.
#[test]
fn replayed_reasoning_and_native_ids_convert_into_responses_api_input_items() {
    use rig::providers::openai::responses_api::InputItem;

    let mut session = session_with_native_call_and_reasoning();
    session.add_message(MessageRole::Assistant, "done");
    session.add_message(MessageRole::User, "now the next thing");

    let mut saw_reasoning_before_call = false;
    for message in convert_history(&session) {
        let items = match <Vec<InputItem>>::try_from(message.clone()) {
            Ok(items) => items,
            Err(error) => panic!("replayed history must build Responses input items: {error:?}"),
        };
        // `InputItem`'s content is private; its serialized `type` tag is the
        // wire shape the request actually carries.
        let kinds = items
            .iter()
            .map(|item| {
                serde_json::to_value(item)
                    .ok()
                    .and_then(|value| value["type"].as_str().map(str::to_string))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let reasoning = kinds.iter().position(|kind| kind == "reasoning");
        let call = kinds.iter().position(|kind| kind == "function_call");
        if let (Some(reasoning), Some(call)) = (reasoning, call) {
            assert!(
                reasoning < call,
                "the reasoning input item must precede its function_call: {kinds:?}"
            );
            saw_reasoning_before_call = true;
        }
    }
    assert!(
        saw_reasoning_before_call,
        "the persisted reasoning item must reach the Responses request"
    );
}

/// End-to-end guard: rig's Responses request builder converts every history
/// item through `TryFrom`, and returns `RequestError` before any HTTP call when
/// a tool call or result is missing its `call_id`.
#[test]
fn replayed_tool_history_converts_into_responses_api_input_items() {
    use rig::providers::openai::responses_api::InputItem;

    let mut session = sample_session();
    session.add_message(MessageRole::User, "edit it");
    session.add_tool_call_with_id("fc_abc123", "edit", &serde_json::json!({"path": "a.rs"}));
    session.add_tool_result_with_id("fc_abc123", "edit", "ok");
    session.add_message(MessageRole::Assistant, "done");
    session.add_message(MessageRole::User, "now the next thing");

    for message in convert_history(&session) {
        let converted = <Vec<InputItem>>::try_from(message.clone());
        assert!(
            converted.is_ok(),
            "replayed history must build Responses input items: {:?}",
            converted.err()
        );
    }

    // The provider `call_id` is the identifier both run modes persist, so the
    // same session recorded under it must convert just as cleanly.
    let mut by_call_id = sample_session();
    by_call_id.add_message(MessageRole::User, "edit it");
    by_call_id.add_tool_call_with_id("call_abc123", "edit", &serde_json::json!({"path": "a.rs"}));
    by_call_id.add_tool_result_with_id("call_abc123", "edit", "ok");
    for message in convert_history(&by_call_id) {
        let converted = <Vec<InputItem>>::try_from(message.clone());
        assert!(
            converted.is_ok(),
            "a call_id-keyed record must build Responses input items: {:?}",
            converted.err()
        );
    }
}
