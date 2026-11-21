use super::*;
use crate::config::ReasoningDisplay;

#[tokio::test]
async fn a_url_opens_on_a_plain_click_release_and_never_on_a_drag() {
    let root = crate::tests::OwnedTestDirectory::new("link-click-controls");
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
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = crate::config::Config::default();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = crate::session::Session::new("openrouter", "unused-test-model", 128_000, "");
    let client = crate::provider::create_client(
        "openrouter",
        Some("unused-test-key"),
        cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .unwrap();
    let ui = UiContext::new(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        client,
        None,
        None,
        crate::sandbox::Sandbox::new(false, "none"),
        None,
    );
    let mut app = test_support::fixture_app(ui);
    app.renderer.feed_mut().push_line(
        crate::ui::feed::BlockStyle::Plain,
        "see https://example.com/x ok",
    );
    let row = (0..app.renderer.visible_lines() as u16)
        .find(|row| app.renderer.buffer_line_at_row(*row) == Some(0))
        .unwrap();
    let col = 8u16;
    let opened = || OPENED_LINKS.with(|o| o.borrow().len());
    let before = opened();
    let _ = app
        .handle_user_event(UserEvent::MouseDown { row, col })
        .await
        .unwrap();
    assert_eq!(opened(), before, "button-down alone opens nothing");
    let _ = app
        .handle_user_event(UserEvent::MouseUp { row, col })
        .await
        .unwrap();
    assert_eq!(opened(), before + 1);
    // A drag across the URL selects and never opens.
    std::thread::sleep(std::time::Duration::from_millis(700));
    let _ = app
        .handle_user_event(UserEvent::MouseDown { row, col: 6 })
        .await
        .unwrap();
    assert!(app.renderer.selection_active);
    let _ = app
        .handle_user_event(UserEvent::MouseDrag { row, col: 14 })
        .await
        .unwrap();
    let _ = app
        .handle_user_event(UserEvent::MouseUp { row, col: 14 })
        .await
        .unwrap();
    assert_eq!(
        opened(),
        before + 1,
        "a drag over a URL selects, never opens"
    );
    OPENED_LINKS.with(|o| assert_eq!(o.borrow().last().unwrap(), "https://example.com/x"));
}

#[tokio::test]
async fn actual_reasoning_controls_preserve_the_turn_and_persist_every_cycle() {
    let root = crate::tests::OwnedTestDirectory::new("reasoning-app-controls");
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
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = crate::config::Config::default();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = crate::session::Session::new("openrouter", "unused-test-model", 128_000, "");
    let client = crate::provider::create_client(
        "openrouter",
        Some("unused-test-key"),
        cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .unwrap();
    let ui = UiContext::new(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        client,
        None,
        None,
        crate::sandbox::Sandbox::new(false, "none"),
        None,
    );
    let mut app = test_support::fixture_app(ui);
    app.slash.show_reasoning = true;
    app.run.is_running = true;
    app.input.buffer = "unsent draft".into();
    app.input.cursor = app.input.buffer.len();
    app.run.pending_inputs.push_back("queued request".into());
    let before = serde_json::to_value(&*app.ui.session).unwrap();
    for text in ["REASONING_PRIVATE_界", "🙂"] {
        app.handle_agent_event(AgentEvent::Reasoning(text.into()))
            .await
            .unwrap();
    }
    assert_eq!(app.renderer.feed().block_count(), 1);
    assert!(
        !app.renderer.feed().lines(100)[0]
            .text
            .contains("REASONING_PRIVATE")
    );
    let row = (0..app.renderer.visible_lines() as u16)
        .find(|row| app.renderer.buffer_line_at_row(*row) == Some(0))
        .unwrap();
    assert!(matches!(
        app.handle_user_event(UserEvent::MouseDown { row, col: 0 })
            .await
            .unwrap(),
        ControlFlow::Continue(())
    ));
    assert!(
        app.renderer.feed().lines(100)[1]
            .text
            .contains("REASONING_PRIVATE")
    );
    assert!(!app.renderer.selection_active);
    let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
    for expected in [
        ReasoningDisplay::Full,
        ReasoningDisplay::Off,
        ReasoningDisplay::Collapsed,
        ReasoningDisplay::Full,
    ] {
        app.handle_key_event(key).await.unwrap();
        assert_eq!(app.renderer.feed().reasoning_display(), expected);
        let saved: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(crate::config::config_file_path()).unwrap())
                .unwrap();
        assert_eq!(saved.resolve_reasoning_display(), expected);
    }
    assert!(app.run.agent.is_none());
    assert_eq!(serde_json::to_value(&*app.ui.session).unwrap(), before);
    assert_eq!(app.input.buffer, "unsent draft");
    assert_eq!(
        app.run.pending_inputs.front().map(String::as_str),
        Some("queued request")
    );
    assert!(app.run.is_running);
    app.handle_agent_event(AgentEvent::Token("earlier answer ".into()))
        .await
        .unwrap();
    app.handle_agent_event(AgentEvent::Reasoning("SECOND_THOUGHT".into()))
        .await
        .unwrap();
    // A malformed concurrently edited config leaves live preference and
    // transcript untouched, instead of reporting a successful save.
    std::fs::write(crate::config::config_file_path(), "broken = [").unwrap();
    app.handle_key_event(key).await.unwrap();
    assert_eq!(
        app.renderer.feed().reasoning_display(),
        ReasoningDisplay::Full
    );
    assert!(app.renderer.feed().lines(100).len() > 1);
    app.handle_agent_event(AgentEvent::Token("visible answer".into()))
        .await
        .unwrap();
    assert_eq!(app.run.response_buf, "earlier answer visible answer");
    assert!(
        (0..app.renderer.feed().block_count())
            .filter(|index| app.renderer.feed().is_streaming(*index))
            .count()
            <= 1
    );
    assert!(
        app.renderer.feed().lines(100)[0]
            .text
            .starts_with("▾ thought")
    );
    app.abort_main_run().await.unwrap();
    let complete = app.renderer.feed().lines(100);
    app.renderer
        .feed_mut()
        .tick_reasoning_at(std::time::Instant::now() + Duration::from_secs(10));
    assert!(Arc::ptr_eq(&complete, &app.renderer.feed().lines(100)));
    // Changing provider reasoning policy cannot reset the live display to
    // the immutable startup config after a saved Ctrl+R transition.
    app.slash.reasoning_enabled = true;
    for command in ["/reasoning off", "/reasoning on"] {
        app.run_slash_command(command).await.unwrap();
        assert_eq!(
            app.renderer.feed().reasoning_display(),
            ReasoningDisplay::Full
        );
        assert!(app.slash.show_reasoning);
    }
}
