// Append this review-only module to src/startup.rs.
// Root owns Cargo coordination. Filter: continuation_runtime_a_probe.
#[cfg(all(test, feature = "goal"))]
mod continuation_runtime_a_probes {
    use super::*;

    fn round() -> HeadlessRounds {
        HeadlessRounds {
            turn: crate::agent::runner::HeadlessTurn {
                response: "FIRST_REPLY_AUDIT_MARKER".into(),
                usage: rig::completion::Usage {
                    input_tokens: 3, output_tokens: 2, total_tokens: 5, ..Default::default()
                },
                interactions: Vec::new(), failure: None,
            },
            prompt: "FIRST_PROMPT_AUDIT_MARKER".into(), persisted: false,
            relief_usage: Default::default(), relief_interactions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn continuation_runtime_a_probe_ephemeral_finalizer_keeps_in_memory_history() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let root = crate::tests::OwnedTestDirectory::new("audit-ephemeral-history");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            ("MINI_AGENT_CONFIG_DIR", Some(root.path().join("config").into_os_string())),
            ("MINI_AGENT_DATA_DIR", Some(root.path().join("data").into_os_string())),
            ("MINI_AGENT_STATE_DIR", Some(root.path().join("state").into_os_string())),
            ("MINI_AGENT_CACHE_DIR", Some(root.path().join("cache").into_os_string())),
        ]);
        let workspace = std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let cli = Cli {
            provider: Some("openrouter".into()), model: Some("openai/gpt-4o-mini".into()),
            api_key: Some("unused-audit-key".into()), no_context_files: true,
            message: vec!["FIRST_PROMPT_AUDIT_MARKER".into()],
            no_tools: true, no_sandbox: true, no_session: true, print: true,
            input_format: Some(crate::cli::InputFormat::StreamJson),
            output: Some(crate::cli::OutputFormat::StreamJson),
            ..Default::default()
        };
        let cfg = Config { session_title_model: Some("off".into()), ..Default::default() };
        let mut startup = Startup::init(cli, cfg, crate::paths::process_paths_ref().unwrap().clone(), workspace, false, false, false).await.unwrap();
        let identity = (startup.session.id.to_string(), startup.session.name.to_string());
        let output = crate::print::HeadlessOutputPolicy::from_cli(&startup.cli);
        startup.finish_print_model("FIRST_PROMPT_AUDIT_MARKER".into(), None, output, identity, None,
            Default::default(), round(), crate::output_schema::SchemaOutcome::default()).await.unwrap();
        let next_history = crate::agent::runner::convert_history(&startup.session);
        assert!(format!("{next_history:?}").contains("FIRST_REPLY_AUDIT_MARKER"),
            "stream-json's next input receives no preceding ephemeral turn: {next_history:?}");
        assert_eq!(startup.session.total_input_tokens, 3);
    }

    #[test]
    fn continuation_runtime_a_probe_ephemeral_goal_round_keeps_retained_history() {
        let mut session = Session::new("openrouter", "test", 128_000, "");
        let rounds = round();
        crate::extras::goal::driver::persist_round(
            &mut session, &rounds.prompt, rounds.turn, &Config::default(), true,
        ).unwrap();
        assert!(!session.messages.is_empty(),
            "next_round(Continue) converts this same session, but no completed round was recorded");
        assert_eq!(session.total_input_tokens, 3);
    }
    #[tokio::test]
    async fn continuation_runtime_a_probe_failed_goal_save_does_not_duplicate_final_round() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let root = crate::tests::OwnedTestDirectory::new("audit-goal-save-retry");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            ("MINI_AGENT_CONFIG_DIR", Some(root.path().join("config").into_os_string())),
            ("MINI_AGENT_DATA_DIR", Some(root.path().join("data").into_os_string())),
            ("MINI_AGENT_STATE_DIR", Some(root.path().join("state").into_os_string())),
            ("MINI_AGENT_CACHE_DIR", Some(root.path().join("cache").into_os_string())),
        ]);
        let workspace = std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let cli = Cli {
            provider: Some("openrouter".into()), model: Some("openai/gpt-4o-mini".into()),
            api_key: Some("unused-audit-key".into()), no_context_files: true,
            message: vec!["FIRST_PROMPT_AUDIT_MARKER".into()],
            no_tools: true, no_sandbox: true, print: true,
            ..Default::default()
        };
        let cfg = Config { session_title_model: Some("off".into()), ..Default::default() };
        let paths = crate::paths::process_paths_ref().unwrap();
        let mut startup = Startup::init(cli, cfg, paths.clone(), workspace, false, false, false).await.unwrap();
        let sessions = paths.sessions_dir();
        // Only the isolated owned test tree is changed. Simulate one failed
        // round write, then repair it before the actual print finalizer retries.
        if sessions.exists() { std::fs::remove_dir_all(&sessions).unwrap(); }
        std::fs::create_dir_all(sessions.parent().unwrap()).unwrap();
        std::fs::write(&sessions, "blocks the first save").unwrap();
        let rounds = round();
        assert!(crate::extras::goal::driver::persist_round(
            &mut startup.session, &rounds.prompt, rounds.turn, &startup.cfg, false,
        ).is_err());
        assert_eq!(startup.session.messages.len(), 2, "the failed save already committed live history");
        std::fs::remove_file(&sessions).unwrap();
        // On persist_round error, run_headless_goal_rounds_inner leaves
        // persisted=false; its returned turn then reaches this finalizer.
        let identity = (startup.session.id.to_string(), startup.session.name.to_string());
        let output = crate::print::HeadlessOutputPolicy::from_cli(&startup.cli);
        startup.finish_print_model("FIRST_PROMPT_AUDIT_MARKER".into(), None, output, identity, None,
            Default::default(), round(), crate::output_schema::SchemaOutcome::default()).await.unwrap();
        let prompts = startup.session.messages.iter().filter(|m| m.content == "FIRST_PROMPT_AUDIT_MARKER").count();
        assert_eq!(prompts, 1, "retrying persistence must not append the final round again");
        assert_eq!(startup.session.total_input_tokens, 3, "one observed round must be charged once");
        crate::session::lock::release_session(&startup.session.id);
    }

    #[cfg(feature = "subagents")]
    #[tokio::test]
    async fn continuation_runtime_a_probe_stream_turn_does_not_permanently_close_task_pool() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let root = crate::tests::OwnedTestDirectory::new("audit-ephemeral-history");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            ("MINI_AGENT_CONFIG_DIR", Some(root.path().join("config").into_os_string())),
            ("MINI_AGENT_DATA_DIR", Some(root.path().join("data").into_os_string())),
            ("MINI_AGENT_STATE_DIR", Some(root.path().join("state").into_os_string())),
            ("MINI_AGENT_CACHE_DIR", Some(root.path().join("cache").into_os_string())),
        ]);
        let workspace = std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let cli = Cli {
            provider: Some("openrouter".into()), model: Some("openai/gpt-4o-mini".into()),
            api_key: Some("unused-audit-key".into()), no_context_files: true,
            message: vec!["FIRST_PROMPT_AUDIT_MARKER".into()],
            no_tools: true, no_sandbox: true, no_session: true, print: true,
            input_format: Some(crate::cli::InputFormat::StreamJson),
            output: Some(crate::cli::OutputFormat::StreamJson),
            ..Default::default()
        };
        let cfg = Config { session_title_model: Some("off".into()), ..Default::default() };
        let mut startup = Startup::init(cli, cfg, crate::paths::process_paths_ref().unwrap().clone(), workspace, false, false, false).await.unwrap();
        let pool = crate::extras::subagents::background::Pool::for_session(&mut startup.session, 1).unwrap();
        assert!(pool.ensure_open().is_ok());
        let identity = (startup.session.id.to_string(), startup.session.name.to_string());
        let output = crate::print::HeadlessOutputPolicy::from_cli(&startup.cli);
        startup.finish_print_model("FIRST_PROMPT_AUDIT_MARKER".into(), None, output, identity, None,
            Default::default(), round(), crate::output_schema::SchemaOutcome::default()).await.unwrap();
        let next_pool = crate::extras::subagents::background::Pool::for_session(&mut startup.session, 1).unwrap();
        assert!(next_pool.ensure_open().is_ok(),
            "a stream-json turn is not session exit; the next prompt must retain task admission");
    }

}
