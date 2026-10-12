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
    fn global_subagent_provider_fallback_reports_configured_and_effective_models() {
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "subagent_provider":"missing-subagent-provider", "subagent_model":"cheap-model"
        }))
        .unwrap();
        let main = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let resolved = crate::extras::subagents::resolve_config(
            &cfg,
            Some("unused-test-key"),
            "openrouter",
            "main-model",
            &main,
        );
        assert_eq!(resolved.provider_name, "openrouter");
        assert_eq!(resolved.model_name, "main-model");
        let notice = resolved.fallback_notice.as_ref().unwrap();
        assert!(
            notice.contains("configured: missing-subagent-provider / cheap-model"),
            "{notice}"
        );
        assert!(notice.contains("unavailable: Unknown provider"), "{notice}");
        assert!(
            notice.contains("using main model: openrouter / main-model"),
            "{notice}"
        );
        assert!(resolved.model_status().contains(notice));
        assert_eq!(
            resolved.config.subagent_model.as_deref(),
            Some("cheap-model")
        );
    }

    #[test]
    fn global_subagent_quick_alias_fallback_uses_the_main_model_not_the_alias_model() {
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "subagent_model":"broken-alias",
            "quick_models":{"broken-alias":{"provider":"missing-subagent-provider", "model":"alias-cheap-model", "input_token_cost":1, "output_token_cost":2}}
        })).unwrap();
        let main = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let resolved = crate::extras::subagents::resolve_config(
            &cfg,
            Some("unused-test-key"),
            "openrouter",
            "main-model",
            &main,
        );
        assert_eq!(resolved.model_name, "main-model");
        let notice = resolved.fallback_notice.as_ref().unwrap();
        assert!(
            notice.contains("missing-subagent-provider / alias-cheap-model"),
            "{notice}"
        );
        assert!(
            notice.contains("using main model: openrouter / main-model"),
            "{notice}"
        );
    }

    #[test]
    fn available_global_subagent_selection_has_no_fallback_notice() {
        let cfg: crate::config::Config =
            serde_json::from_value(serde_json::json!({"subagent_model":"alternate-model"}))
                .unwrap();
        let main = crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let resolved = crate::extras::subagents::resolve_config(
            &cfg,
            Some("unused-test-key"),
            "openrouter",
            "main-model",
            &main,
        );
        assert!(resolved.fallback_notice.is_none());
        assert_eq!(resolved.model_name, "alternate-model");
        assert_eq!(
            resolved.model_status(),
            "current subagent model: openrouter / alternate-model"
        );
    }

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
    fn task_args_treats_null_empty_and_blank_handoffs_as_absent() {
        use crate::extras::subagents::task_tool::TaskArgs;

        /// (json, expected prompts, expected brief objectives)
        type Case = (
            &'static str,
            &'static [&'static str],
            Option<&'static [&'static str]>,
        );
        let accepted: &[Case] = &[
            (
                r#"{"prompts":[],"briefs":[{"objective":"y"}]}"#,
                &[],
                Some(&["y"]),
            ),
            (
                r#"{"prompts":null,"briefs":[{"objective":"y"}]}"#,
                &[],
                Some(&["y"]),
            ),
            (
                r#"{"prompts":["", "  "],"briefs":[{"objective":"y"}]}"#,
                &[],
                Some(&["y"]),
            ),
            (r#"{"prompts":["x"],"briefs":[]}"#, &["x"], None),
            (r#"{"prompts":["x"],"briefs":null}"#, &["x"], None),
            (
                r#"{"prompts":["x"],"briefs":[{"objective":" "}]}"#,
                &["x"],
                None,
            ),
            (
                r#"{"prompts":["x"],"briefs":[],"agent_type":null}"#,
                &["x"],
                None,
            ),
        ];
        for (json, prompts, briefs) in accepted {
            let args: TaskArgs = serde_json::from_str(json)
                .unwrap_or_else(|error| panic!("rejected {json}: {error}"));
            assert_eq!(args.prompts, *prompts, "{json}");
            let objectives = args.briefs.as_ref().map(|briefs| {
                briefs
                    .iter()
                    .map(|b| b.objective.as_str())
                    .collect::<Vec<_>>()
            });
            assert_eq!(objectives.as_deref(), *briefs, "{json}");
        }
    }

    #[test]
    fn task_args_rejects_ambiguous_or_unknown_handoff_fields() {
        for json in [
            r#"{"prompts":["x"],"briefs":[{"objective":"y"}]}"#,
            r#"{"prompts":["x",""],"briefs":[{"objective":"y"},{"objective":""}]}"#,
            r#"{"prompts":null,"briefs":null}"#,
            r#"{"prompts":[],"briefs":[]}"#,
            r#"{"briefs":[{"objective":"y","unknown":true}]}"#,
            r#"{"prompts":["x"],"unknown":true}"#,
        ] {
            let result: Result<crate::extras::subagents::task_tool::TaskArgs, _> =
                serde_json::from_str(json);
            assert!(result.is_err(), "accepted invalid task arguments: {json}");
        }
    }
}
