//! Tests for the `subagents` feature.
//!
//! Run with: cargo test --features subagents
//!
//! These tests cover the pure-logic portions that don't require an actual
//! LLM: argument parsing and per-agent policy capture. Scheduler, rendering,
//! and bounded response coverage lives beside the production task implementation.

#[cfg(test)]
mod tests {
    #[test]
    fn task_tools_capture_each_agent_builds_repeated_read_policy() {
        let denying_agent = crate::extras::subagents::task_tool::TaskTool::new(None, None, true);
        let allowing_agent = crate::extras::subagents::task_tool::TaskTool::new(None, None, false);

        assert!(denying_agent.repeated_read_policy_for_test());
        assert!(!allowing_agent.repeated_read_policy_for_test());
    }

    // -----------------------------------------------------------------------
    // TaskArgs deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn task_args_deserializes_multiple_prompts() {
        let json = r#"{"prompts": ["explore auth module", "find api routes"]}"#;
        let args: crate::extras::subagents::task_tool::TaskArgs =
            serde_json::from_str(json).unwrap();
        assert_eq!(args.prompts.len(), 2);
        assert!(args.briefs.is_none());
        assert_eq!(args.prompts[0], "explore auth module");
        assert_eq!(args.prompts[1], "find api routes");
    }

    #[test]
    fn task_args_single_prompt() {
        let json = r#"{"prompts": ["one thing"]}"#;
        let args: crate::extras::subagents::task_tool::TaskArgs =
            serde_json::from_str(json).unwrap();
        assert_eq!(args.prompts.len(), 1);
        assert_eq!(args.prompts[0], "one thing");
    }

    #[test]
    fn task_args_empty_prompts_deserializes() {
        // The struct itself allows an empty vec; TaskTool::call rejects it.
        let json = r#"{"prompts": []}"#;
        let args: crate::extras::subagents::task_tool::TaskArgs =
            serde_json::from_str(json).unwrap();
        assert!(args.prompts.is_empty());
    }

    #[test]
    fn task_args_missing_prompts_is_error() {
        let json = r#"{}"#;
        let result: Result<crate::extras::subagents::task_tool::TaskArgs, _> =
            serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn task_args_deserializes_structured_briefs() {
        let json = r#"{
            "briefs": [{
                "objective": "audit authentication",
                "files": ["src/auth.rs"],
                "constraints": ["read only"],
                "expected_sections": ["attack path"]
            }],
            "agent_type": "rust-security-review"
        }"#;
        let args: crate::extras::subagents::task_tool::TaskArgs =
            serde_json::from_str(json).unwrap();
        assert!(args.prompts.is_empty());
        let briefs = args.briefs.unwrap();
        assert_eq!(briefs.len(), 1);
        assert_eq!(briefs[0].objective, "audit authentication");
        assert_eq!(briefs[0].files, ["src/auth.rs"]);
    }

    #[test]
    fn task_args_rejects_ambiguous_or_unknown_handoff_fields() {
        for json in [
            r#"{"prompts":["x"],"briefs":[{"objective":"y"}]}"#,
            r#"{"briefs":[{"objective":"y","unknown":true}]}"#,
            r#"{"prompts":["x"],"unknown":true}"#,
        ] {
            let result: Result<crate::extras::subagents::task_tool::TaskArgs, _> =
                serde_json::from_str(json);
            assert!(result.is_err(), "accepted invalid task arguments: {json}");
        }
    }
}
