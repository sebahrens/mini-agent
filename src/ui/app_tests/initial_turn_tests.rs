use rig::completion::Message;

use super::AutoTriggerTurn;
use crate::session::{MessageRole, Session};
use crate::ui::state::{AgentRunState, PendingMainTurn};

#[test]
fn initial_turn_keeps_current_prompt_separate_until_runner_starts() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    let turn = AutoTriggerTurn::prepare(
        &session,
        "current prompt",
        crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS,
    );
    let (prompt, history, pending_turn) = turn.into_runner_inputs();

    assert_eq!(prompt, "current prompt");
    assert!(history.is_empty());
    assert!(session.messages.is_empty());

    let mut run = AgentRunState::default();
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].role, MessageRole::User);
    assert_eq!(session.messages[0].content, "current prompt");
    assert_eq!(
        run.pending_turn.as_ref().map(PendingMainTurn::prompt),
        Some("current prompt")
    );
}

#[test]
fn resumed_history_precedes_current_prompt_without_duplication() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    session.add_message(MessageRole::User, "prior question");
    session.add_message(MessageRole::Assistant, "prior answer");

    let turn = AutoTriggerTurn::prepare(
        &session,
        "new question",
        crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS,
    );
    let (prompt, history, pending_turn) = turn.into_runner_inputs();

    assert_eq!(prompt, "new question");
    assert_eq!(
        history.as_ref(),
        vec![
            Message::user("prior question"),
            Message::assistant("prior answer")
        ]
    );
    assert_eq!(session.messages.len(), 2);

    let mut run = AgentRunState::default();
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    assert_eq!(session.messages.len(), 3);
    assert_eq!(session.messages[2].role, MessageRole::User);
    assert_eq!(session.messages[2].content, "new question");
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|message| message.content == "new question")
            .count(),
        1
    );
}

#[test]
fn failed_turn_rollback_restores_prompt_partial_state_and_undo() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    session.add_message(MessageRole::User, "prior question");
    session.add_message(MessageRole::Assistant, "prior answer");
    session.add_message(MessageRole::User, "rewound question");
    assert_eq!(session.rewind_to(2), 1);
    session.set_calibration(800, 40);
    let mut expected = session.clone();
    let mut run = AgentRunState::default();

    let pending_turn = PendingMainTurn::capture(&session, "retry me");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    session.add_message(MessageRole::Assistant, "partial response");
    session.add_tool_call("read", &serde_json::json!({"path": "secret.txt"}));
    session.add_tool_result("read", "partial tool output");
    session.total_input_tokens = 99;
    session.total_output_tokens = 17;
    session.total_cached_input_tokens = 23;
    session.total_cache_creation_input_tokens = 5;
    session.total_cost = 1.25;
    session
        .permission_allowlist
        .push(crate::session::PermissionAllowEntry {
            tool: "read".into(),
            pattern: "secret.txt".into(),
        });
    expected.total_input_tokens = 99;
    expected.total_output_tokens = 17;
    expected.total_cached_input_tokens = 23;
    expected.total_cache_creation_input_tokens = 5;
    expected.total_cost = 1.25;
    expected.permission_allowlist = session.permission_allowlist.clone();
    assert!(
        session
            .read_tracker
            .track_read("secret.txt", 0, 100)
            .is_none()
    );
    assert!(
        session
            .read_tracker
            .track_read("secret.txt", 0, 100)
            .is_some()
    );
    let restored = crate::ui::rollback_pending_main_turn(&mut run, &mut session);

    assert_eq!(restored.as_deref(), Some("retry me"));
    assert!(run.pending_turn.is_none());
    assert!(
        session
            .read_tracker
            .track_read("secret.txt", 0, 100)
            .is_none()
    );
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(session.redo(), expected.redo());
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn cancelled_turn_restores_the_prompt_and_pre_turn_transcript() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    session.add_message(MessageRole::User, "prior question");
    session.add_message(MessageRole::Assistant, "prior answer");
    let expected = serde_json::to_value(&session).unwrap();
    let mut run = AgentRunState::default();
    let pending_turn = PendingMainTurn::capture(&session, "cancel me");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);

    let restored = crate::ui::rollback_pending_main_turn(&mut run, &mut session);

    assert_eq!(restored.as_deref(), Some("cancel me"));
    assert_eq!(serde_json::to_value(&session).unwrap(), expected);
    assert!(run.pending_turn.is_none());
}

#[test]
fn interrupted_turn_with_progress_is_preserved_and_protocol_complete() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    session.add_message(MessageRole::User, "prior question");
    session.add_message(MessageRole::Assistant, "prior answer");
    let mut run = AgentRunState::default();
    let pending_turn = PendingMainTurn::capture(&session, "make the edit");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    session.add_tool_call_with_id(
        "interrupted-call",
        "edit",
        &serde_json::json!({"path": "src/main.rs"}),
    );
    run.response_buf = "partial explanation".to_string();

    assert!(crate::ui::preserve_pending_main_turn_progress(
        &mut run,
        &mut session
    ));

    assert!(run.pending_turn.is_some());
    assert_eq!(
        session
            .messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::User,
            MessageRole::ToolCall,
            MessageRole::ToolResult,
            MessageRole::Assistant,
        ]
    );
    let synthetic = &session.messages[4];
    assert_eq!(synthetic.tool_call_id.as_deref(), Some("interrupted-call"));
    assert!(
        synthetic
            .content
            .contains(crate::agent::runner::UNKNOWN_TOOL_OUTCOME)
    );
    assert_eq!(session.messages[5].content, "partial explanation");
    // The closing result keeps the real tool name, and an interrupt adds
    // a host row after the partial text.
    assert!(
        synthetic.content.starts_with("edit:\n"),
        "{}",
        synthetic.content
    );
    crate::ui::record_interrupted_turn_notice(&mut session);
    let notice = session.messages.last().unwrap();
    assert_eq!(notice.role, MessageRole::System);
    assert!(crate::agent::input::is_host_instruction(&notice.content));
}

#[test]
fn interrupted_batch_keeps_the_real_result_of_a_call_that_finished() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    let mut run = AgentRunState::default();
    let pending_turn = PendingMainTurn::capture(&session, "edit then test");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    session.add_tool_call_with_id("settled-edit", "edit", &serde_json::json!({}));
    session.add_tool_call_with_id("running-test", "shell", &serde_json::json!({}));
    crate::agent::runner::record_settled_result("settled-edit", "edit applied");

    assert!(crate::ui::preserve_pending_main_turn_progress(
        &mut run,
        &mut session
    ));
    let results: Vec<_> = session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::ToolResult)
        .map(|message| message.content.to_string())
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].contains("edit applied"), "{results:?}");
    assert!(
        results[1].contains(crate::agent::runner::UNKNOWN_TOOL_OUTCOME),
        "{results:?}"
    );
}

#[test]
fn interrupted_turn_does_not_duplicate_a_completed_tool_result() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    let mut run = AgentRunState::default();
    let pending_turn = PendingMainTurn::capture(&session, "inspect");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    session.add_tool_call_with_id("completed-call", "read", &serde_json::json!({}));
    session.add_tool_result_with_id("completed-call", "read", "done");

    assert!(crate::ui::preserve_pending_main_turn_progress(
        &mut run,
        &mut session
    ));
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::ToolResult)
            .count(),
        1
    );
}

#[test]
fn zero_progress_failure_remains_eligible_for_rollback() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    let mut run = AgentRunState::default();
    let pending_turn = PendingMainTurn::capture(&session, "retry me");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);

    assert!(!crate::ui::preserve_pending_main_turn_progress(
        &mut run,
        &mut session
    ));
    assert_eq!(
        crate::ui::rollback_pending_main_turn(&mut run, &mut session).as_deref(),
        Some("retry me")
    );
    assert!(session.messages.is_empty());
}

#[test]
fn presentation_failure_preserves_the_observed_turn_trace() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    let mut run = AgentRunState::default();
    let pending_turn = PendingMainTurn::capture(&session, "inspect");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    run.turn_trace.push("→ read src/main.rs".into());

    assert!(crate::ui::preserve_pending_main_turn_progress(
        &mut run,
        &mut session
    ));
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[1].role, MessageRole::Assistant);
    assert!(
        session.messages[1]
            .content
            .contains("Interrupted turn progress")
    );
    assert!(session.messages[1].content.contains("read src/main.rs"));
}

#[cfg(feature = "multimodal")]
#[test]
fn failed_turn_restores_moved_pending_media_without_snapshot_clone() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    session
        .pending_media
        .push(crate::extras::multimodal::MediaAttachment::Image {
            path: std::path::PathBuf::from("attachment.png"),
            data: vec![1, 2, 3, 4],
            mime: "image/png".into(),
            clipboard: None,
        });
    let mut pending = PendingMainTurn::capture(&session, "describe attachment");
    assert_eq!(pending.take_pending_media(&mut session).len(), 1);
    assert!(session.pending_media.is_empty());
    let mut run = AgentRunState::default();
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending);

    crate::ui::rollback_pending_main_turn(&mut run, &mut session).unwrap();

    assert_eq!(session.pending_media.len(), 1);
    assert_eq!(session.pending_media[0].size(), 4);
}

/// mini-agent-27kzu: an interrupt after a tool call completed keeps the turn's
/// effects in history, so the host "cut off" notice must close it.
#[test]
fn interrupt_after_a_completed_tool_call_keeps_the_turn_and_adds_the_host_notice() {
    let mut session = Session::new("openrouter", "test-model", 128_000, "/workspace");
    let mut run = AgentRunState::default();
    let pending_turn = PendingMainTurn::capture(&session, "edit then explain");
    crate::ui::mark_main_turn_started(&mut session, &mut run, pending_turn);
    session.add_tool_call_with_id("done-edit", "edit", &serde_json::json!({}));
    session.add_tool_result_with_id("done-edit", "edit", "applied");

    assert!(crate::ui::preserve_pending_main_turn_progress(
        &mut run,
        &mut session
    ));
    crate::ui::record_interrupted_turn_notice(&mut session);

    assert!(
        session
            .messages
            .iter()
            .any(|message| message.role == MessageRole::ToolResult
                && message.content.contains("applied")),
        "the completed call's effect stays in history"
    );
    let notice = session.messages.last().unwrap();
    assert_eq!(notice.role, MessageRole::System);
    assert!(crate::agent::input::is_host_instruction(&notice.content));
    assert!(notice.content.contains("cut off"), "{}", notice.content);
}
