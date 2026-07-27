use serde_json::{Value, json};

use super::HookCtx;

/// Event-specific fields for the stdin envelope, per the hook-dispatch spec's
/// "stdin envelope schema" requirement.
pub(crate) enum EventFields {
    PreToolUse {
        tool_name: String,
        tool_input: Value,
    },
    PostToolUse {
        tool_name: String,
        tool_input: Value,
        tool_response: String,
    },
    PostToolUseFailure {
        tool_name: String,
        tool_input: Value,
        error: String,
    },
    UserPromptSubmit {
        prompt: String,
    },
    Stop {
        stop_hook_active: bool,
        loop_iteration: Option<u64>,
        loop_active: Option<bool>,
    },
    SessionStart {
        source: String,
    },
    SessionEnd {
        reason: String,
    },
    SubagentStart {
        agent_type: String,
    },
    SubagentStop {
        stop_hook_active: bool,
        agent_type: String,
    },
}

/// Assembles the single flat JSON stdin envelope for a hook invocation:
/// common fields from `ctx`, `hook_event_name`, and the event-specific
/// fields flattened as top-level keys. Never emits `transcript_path`.
pub(crate) fn build_envelope(ctx: &HookCtx, hook_event_name: &str, fields: EventFields) -> Value {
    let mut envelope = json!({
        "session_id": ctx.session_id,
        "session_path": ctx.session_path,
        "cwd": ctx.cwd,
        "permission_mode": ctx.permission_mode,
        "hook_event_name": hook_event_name,
    });

    let extra = match fields {
        EventFields::PreToolUse {
            tool_name,
            tool_input,
        } => json!({ "tool_name": tool_name, "tool_input": tool_input }),
        EventFields::PostToolUse {
            tool_name,
            tool_input,
            tool_response,
        } => json!({
            "tool_name": tool_name,
            "tool_input": tool_input,
            "tool_response": tool_response,
        }),
        EventFields::PostToolUseFailure {
            tool_name,
            tool_input,
            error,
        } => json!({
            "tool_name": tool_name,
            "tool_input": tool_input,
            "error": error,
        }),
        EventFields::UserPromptSubmit { prompt } => json!({ "prompt": prompt }),
        EventFields::Stop {
            stop_hook_active,
            loop_iteration,
            loop_active,
        } => json!({
            "stop_hook_active": stop_hook_active,
            "loop_iteration": loop_iteration,
            "loop_active": loop_active,
        }),
        EventFields::SessionStart { source } => json!({ "source": source }),
        EventFields::SessionEnd { reason } => json!({ "reason": reason }),
        EventFields::SubagentStart { agent_type } => json!({ "agent_type": agent_type }),
        EventFields::SubagentStop {
            stop_hook_active,
            agent_type,
        } => json!({
            "stop_hook_active": stop_hook_active,
            "agent_type": agent_type,
        }),
    };

    if let (Value::Object(base), Value::Object(extra)) = (&mut envelope, extra) {
        base.extend(extra);
    }

    envelope
}
