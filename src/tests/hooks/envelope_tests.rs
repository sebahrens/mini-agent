use crate::extras::hooks::HookCtx;
use crate::extras::hooks::envelope::{EventFields, build_envelope};

fn ctx() -> HookCtx {
    HookCtx {
        session_id: "sess-1".into(),
        session_path: "/tmp/sess.json".into(),
        cwd: "/repo".into(),
        permission_mode: "yolo".into(),
    }
}

#[test]
fn pre_tool_use_envelope_carries_tool_fields_and_common_fields() {
    let envelope = build_envelope(
        &ctx(),
        "PreToolUse",
        EventFields::PreToolUse {
            tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
        },
    );
    assert_eq!(envelope["hook_event_name"], "PreToolUse");
    assert_eq!(envelope["session_id"], "sess-1");
    assert_eq!(envelope["session_path"], "/tmp/sess.json");
    assert_eq!(envelope["cwd"], "/repo");
    assert_eq!(envelope["permission_mode"], "yolo");
    assert_eq!(envelope["tool_name"], "bash");
    assert_eq!(envelope["tool_input"]["command"], "ls");
}

#[test]
fn envelope_never_contains_transcript_path() {
    let variants = [
        EventFields::PreToolUse {
            tool_name: "bash".into(),
            tool_input: serde_json::json!({}),
        },
        EventFields::UserPromptSubmit {
            prompt: "hi".into(),
        },
        EventFields::SessionStart {
            source: "startup".into(),
        },
    ];
    for fields in variants {
        let envelope = build_envelope(&ctx(), "SomeEvent", fields);
        assert!(envelope.get("transcript_path").is_none());
        assert!(envelope.get("session_path").is_some());
    }
}

#[test]
fn stop_envelope_in_loop_mode_carries_loop_fields() {
    let envelope = build_envelope(
        &ctx(),
        "Stop",
        EventFields::Stop {
            stop_hook_active: true,
            loop_iteration: Some(3),
            loop_active: Some(true),
            goal_id: Some("g-1".into()),
            goal_status: Some("active".into()),
            goal_round: Some(7),
        },
    );
    assert_eq!(envelope["stop_hook_active"], true);
    assert_eq!(envelope["loop_iteration"], 3);
    assert_eq!(envelope["loop_active"], true);
    assert_eq!(envelope["goal_id"], "g-1");
    assert_eq!(envelope["goal_status"], "active");
    assert_eq!(envelope["goal_round"], 7);
}

#[test]
fn stop_envelope_outside_loop_mode_has_null_loop_fields() {
    let envelope = build_envelope(
        &ctx(),
        "Stop",
        EventFields::Stop {
            stop_hook_active: false,
            loop_iteration: None,
            loop_active: None,
            goal_id: None,
            goal_status: None,
            goal_round: None,
        },
    );
    assert!(envelope["loop_iteration"].is_null());
    assert!(envelope["loop_active"].is_null());
    // A hook written before goals existed sees exactly what it saw before.
    assert!(envelope["goal_id"].is_null());
    assert!(envelope["goal_status"].is_null());
    assert!(envelope["goal_round"].is_null());
}

#[test]
fn subagent_stop_envelope_carries_agent_type_and_stop_hook_active() {
    let envelope = build_envelope(
        &ctx(),
        "SubagentStop",
        EventFields::SubagentStop {
            stop_hook_active: false,
            agent_type: "explore".into(),
            agent_source: "compiled-in explorer".into(),
        },
    );
    assert_eq!(envelope["agent_type"], "explore");
    assert_eq!(envelope["agent_source"], "compiled-in explorer");
    assert_eq!(envelope["stop_hook_active"], false);
}
