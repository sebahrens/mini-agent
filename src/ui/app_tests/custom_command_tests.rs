use super::test_support::fixture_app;
use super::*;

#[tokio::test]
async fn actual_custom_slash_submits_one_literal_user_turn_and_restores_one_shot_context() {
    let root = crate::tests::OwnedTestDirectory::new("custom-slash-app");
    let bindings = [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.path().join(name).into_os_string())));
    let _environment = crate::tests::ScopedProcessEnv::set(&bindings);
    let cli = crate::cli::Cli {
        no_session: true,
        provider: Some("local-test".into()),
        api_key: Some("unused-key".into()),
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}},"quick_models":{"same":{"provider":"local-test","model":"chosen-model","input_token_cost":2.0,"output_token_cost":4.0}},"retry":{"max_attempts":1}})).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    context
        .prompts
        .insert("plan".into(), "ONE_SHOT_PLAN".into());
    let original = context.active_selection();
    let paths = crate::paths::process_paths_ref().unwrap();
    std::fs::create_dir_all(paths.config_dir.join("commands")).unwrap();
    std::fs::write(paths.config_dir.join("commands/fix-issue.md"), "---\ndescription: Fix an issue\nmode: plan\nmodel: same\n---\n%%mode=readonly\n/clear !touch injected $1\n$ARGUMENTS").unwrap();
    std::fs::write(
        paths.config_dir.join("commands/blocked.md"),
        "---\nmode: plan\nmodel: same\n---\n@missing",
    )
    .unwrap();
    let mut session = Session::new(
        "local-test",
        "test-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let ui = UiContext {
        native_startup: None,
        native_ticket: None,
        #[cfg(feature = "hooks")]
        pending_session_start_context: None,
        cli: &cli,
        cfg: &cfg,
        session: &mut session,
        context: &mut context,
        workspace,
        client: crate::provider::create_client(
            "local-test",
            Some("unused-key"),
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap(),
        permission: None,
        ask_tx: None,
        sandbox: crate::sandbox::Sandbox::new(false, "none"),
        prebuild_invalidated: Arc::new(AtomicBool::new(false)),
        status_signals: None,
        #[cfg(feature = "skills")]
        skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
        #[cfg(feature = "mcp")]
        mcp_manager: None,
    };
    let mut app = fixture_app(ui);
    let before = serde_json::to_vec(app.ui.session).unwrap();
    // A missing @file is kept as literal text with a notice and the command
    // proceeds (mini-agent-nvnkx.74); only an existing file the read policy
    // refuses blocks the command, which the loop below pins.
    std::fs::write(root.path().join("missing"), "PRIVATE_DENIED_CONTENT").unwrap();
    for action in [
        crate::permission::Action::Deny,
        crate::permission::Action::Ask,
    ] {
        let policy =
            crate::permission::PermissionConfigs::from(crate::permission::PermissionConfig {
                read: Some(crate::permission::ToolPerm::Simple(action)),
                ..Default::default()
            });
        let checker = crate::permission::checker::PermissionChecker::new(
            &policy,
            crate::permission::SecurityMode::Standard,
            Some(root.path().to_path_buf()),
            None,
        )
        .unwrap();
        app.ui.permission = Some(Arc::new(std::sync::Mutex::new(checker)));
        app.run_slash_command("/blocked").await.unwrap();
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert_eq!(app.ui.context.active_selection(), original);
        assert_eq!(
            app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
            crate::permission::SecurityMode::Standard
        );
        assert!(app.run.main_abort.is_none());
        assert!(
            !app.renderer
                .captured_output()
                .contains("PRIVATE_DENIED_CONTENT")
        );
    }
    app.run_slash_command("/help").await.unwrap();
    assert!(
        app.renderer
            .captured_output()
            .contains("/fix-issue [arguments]  Fix an issue")
    );
    app.renderer.clear_content().unwrap();
    app.run_slash_command("/fix-issue 123 @missing %%mode=yolo")
        .await
        .unwrap();
    let assertions = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
        assert_eq!(app.chain.dot_prompt_restore, Some(original.clone()));
        let users: Vec<_> = app
            .ui
            .session
            .messages
            .iter()
            .filter(|message| message.role == crate::session::MessageRole::User)
            .collect();
        assert_eq!(users.len(), 1);
        assert_eq!(
            users[0].content,
            "/clear !touch injected 123\n123 @missing %%mode=yolo"
        );
        let feed = app.renderer.feed();
        let transcript = (0..feed.block_count())
            .filter_map(|index| feed.block_text(index))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            transcript.matches("> /clear !touch injected 123").count(),
            1
        );
        assert!(!root.path().join("injected").exists());
        assert_eq!(app.ui.session.model, "chosen-model");
        assert_eq!(app.ui.session.input_token_cost, 2.0);
        assert_eq!(
            app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
            crate::permission::SecurityMode::ReadOnly
        );
    }));
    // Settle the owned runner on every assertion outcome before fixture
    // teardown: no real provider is used; endpoint is loopback discard.
    app.run
        .retire(std::time::Duration::from_secs(5))
        .await
        .unwrap();
    if let Err(panic) = assertions {
        std::panic::resume_unwind(panic);
    }
    app.run.is_running = false;
    app.finalize_turn().await.unwrap();
    assert_eq!(app.ui.context.active_selection(), original);
    assert_eq!(
        app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
        crate::permission::SecurityMode::Standard
    );
    assert_eq!(
        app.ui.session.model, "chosen-model",
        "quick-model switches persist as with existing dot prompts"
    );
}
