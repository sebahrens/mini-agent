use super::test_support::fixture_app;
use super::*;

#[tokio::test]
async fn actual_compact_show_is_read_only_and_sanitizes_the_active_saved_summary() {
    let root = crate::tests::OwnedTestDirectory::new("compact-show-app");
    let bindings = [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.path().join(name).into_os_string())));
    let _env = crate::tests::ScopedProcessEnv::set(&bindings);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg:crate::config::Config=serde_json::from_value(serde_json::json!({"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}}})).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("local-test", "test", 128_000, root.path().to_str().unwrap());
    session.add_message(crate::session::MessageRole::User, "old turn");
    session.compress(
        "## Task\nSHOW_THIS_SUMMARY\x1b]52;c;ZXZpbA==\x07\n## Next Step\ncontinue".into(),
        1,
        10,
    );
    let before = serde_json::to_vec(&session).unwrap();
    let ui = UiContext {
        native_startup: None,
        native_ticket: None,
        #[cfg(feature = "hooks")]
        pending_session_start_context: None,
        cli: &cli,
        cfg: &cfg,
        session: &mut session,
        context: &mut context,
        workspace: workspace.clone(),
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
    app.run_slash_command("/compact show").await.unwrap();
    let text = app.renderer.captured_output();
    assert!(text.contains("SHOW_THIS_SUMMARY"));
    assert!(text.contains("previous summary"));
    assert!(
        !text.contains("\x1b]52"),
        "saved content must not emit a terminal command"
    );
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert!(
        app.run.main_abort.is_none(),
        "show must not start a provider turn"
    );
    app.run_slash_command("/compact show extra").await.unwrap();
    assert!(
        app.renderer
            .captured_output()
            .contains("usage: /compact show")
    );
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    crate::ui::events::render_session(
        &mut app.renderer,
        app.ui.session,
        &cli,
        &cfg,
        app.ui.context,
    )
    .unwrap();
    assert!(app.renderer.captured_output().contains("/compact show"));

    // Imported/oversized recaps remain stored exactly; display is bounded
    // and the actual command must not mutate or dispatch a provider turn.
    app.ui.session.compress("é".repeat(40_000), 1, 1);
    app.ui
        .session
        .compactions
        .last_mut()
        .unwrap()
        .file_requests
        .mark_imported();
    let large = serde_json::to_vec(app.ui.session).unwrap();
    app.run_slash_command("/compact show").await.unwrap();
    let text = app.renderer.captured_output();
    assert!(text.contains("summary display truncated; saved recap unchanged"));
    assert!(text.contains("imported, untrusted historical data"));
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), large);
    assert!(app.run.main_abort.is_none());

    *app.ui.session = Session::new("local-test", "test", 128_000, "");
    let empty = serde_json::to_vec(app.ui.session).unwrap();
    app.run_slash_command("/compact show").await.unwrap();
    assert!(
        app.renderer
            .captured_output()
            .contains("no active compaction summary")
    );
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), empty);
    assert!(app.run.main_abort.is_none());
}
