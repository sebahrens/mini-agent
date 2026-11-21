use super::test_support::fixture_app;
use super::*;

fn transcript(app: &App<'_>) -> Vec<String> {
    let feed = app.renderer.feed();
    (0..feed.block_count())
        .filter_map(|index| feed.block_text(index).map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn actual_dot_handler_preserves_literals_and_echoes_one_shot_once() {
    let root = crate::tests::OwnedTestDirectory::new("dot-command-handler");
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(root.path().join("config").into_os_string()),
        ),
        (
            "MINI_AGENT_DATA_DIR",
            Some(root.path().join("data").into_os_string()),
        ),
        (
            "MINI_AGENT_STATE_DIR",
            Some(root.path().join("state").into_os_string()),
        ),
        (
            "MINI_AGENT_CACHE_DIR",
            Some(root.path().join("cache").into_os_string()),
        ),
    ]);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let cfg = crate::config::Config {
        prompt_to_model: None,
        ..Default::default()
    };
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    context
        .prompts
        .insert("plan".into(), "Test plan prompt".into());
    let original = context.active_selection();
    let mut session = Session::new("anthropic", "test-model", 128_000, "");
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
            "anthropic",
            Some("unused-test-key"),
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
    for literal in [
        ".gitignore is wrong",
        "  .gitignore is wrong",
        "...continue",
        ".file.md",
        "quoted .plan foo",
    ] {
        let mut text = compact_str::CompactString::new(literal);
        assert!(
            !app.handle_dot_command(&mut text).await.unwrap(),
            "{literal}"
        );
        assert_eq!(text, literal);
        assert_eq!(app.ui.context.active_selection(), original);
        assert!(
            transcript(&app).is_empty(),
            "literal handling must defer its single echo to submission"
        );
    }
    for (shortcut, message) in [
        (".plan foo", "foo"),
        ("  .plan foo", "foo"),
        (".plan /help", "/help"),
        (".plan !literal", "!literal"),
    ] {
        app.renderer.clear_content().unwrap();
        let mut text = compact_str::CompactString::new(shortcut);
        assert!(!app.handle_dot_command(&mut text).await.unwrap());
        assert_eq!(text, message);
        assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
        assert_eq!(app.chain.dot_prompt_restore, Some(original.clone()));
        assert!(transcript(&app).is_empty());
        app.echo_main_submission(&text).unwrap();
        assert_eq!(
            transcript(&app)
                .iter()
                .filter(|line| line.starts_with("> "))
                .cloned()
                .collect::<Vec<_>>(),
            vec![format!("> {message}")]
        );
        app.ui.context.restore_selection(original.clone());
    }
    app.renderer.clear_content().unwrap();
    let mut typo = compact_str::CompactString::new(".unknownprompt");
    assert!(app.handle_dot_command(&mut typo).await.unwrap());
    assert!(
        transcript(&app)
            .iter()
            .any(|line| line == "error: unknown prompt 'unknownprompt'")
    );
    assert_eq!(app.ui.context.active_selection(), original);
    let mut picker = compact_str::CompactString::new("  .");
    assert!(app.handle_dot_command(&mut picker).await.unwrap());
    assert_eq!(app.input.buffer, ".");
    let mut switch = compact_str::CompactString::new(" .plan  ");
    assert!(app.handle_dot_command(&mut switch).await.unwrap());
    assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
    assert!(
        transcript(&app)
            .iter()
            .any(|line| line == "switched to prompt 'plan'")
    );
    assert!(
        app.ui.session.messages.is_empty(),
        "command-only shortcuts never submit an empty provider message"
    );
}
