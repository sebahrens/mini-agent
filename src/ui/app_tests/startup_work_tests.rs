use super::*;

#[test]
fn only_plain_editing_defers_startup_work() {
    let key = |code, modifiers| UserEvent::Key(KeyEvent::new(code, modifiers));
    assert!(defers_startup_work(&key(
        KeyCode::Char('a'),
        KeyModifiers::NONE
    )));
    assert!(defers_startup_work(&key(
        KeyCode::Char('A'),
        KeyModifiers::SHIFT
    )));
    assert!(defers_startup_work(&key(
        KeyCode::Backspace,
        KeyModifiers::NONE
    )));
    assert!(defers_startup_work(&UserEvent::Paste("text".into())));
    assert!(defers_startup_work(&UserEvent::Resize));
    assert!(!defers_startup_work(&key(
        KeyCode::Enter,
        KeyModifiers::NONE
    )));
    assert!(!defers_startup_work(&key(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL
    )));
    assert!(!defers_startup_work(&key(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL
    )));
    assert!(!defers_startup_work(&key(KeyCode::Tab, KeyModifiers::NONE)));
    assert!(!defers_startup_work(&UserEvent::Quit));
}

#[cfg(feature = "hooks")]
#[tokio::test]
async fn deferred_session_start_task_is_joined_before_work() {
    let root = crate::tests::OwnedTestDirectory::new("deferred-session-start");
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let cfg: config::Config = serde_json::from_value(serde_json::json!({
        "custom_providers": {"startup-fixture": {"provider_type":"openai", "base_url":"http://127.0.0.1:1", "api_style":"completions"}}
    }))
    .unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("startup-fixture", "test-model", 128_000, "");
    let ui = UiContext {
        native_startup: None,
        native_ticket: None,
        pending_session_start_context: None,
        cli: &cli,
        cfg: &cfg,
        session: &mut session,
        context: &mut context,
        workspace,
        client: crate::provider::create_client(
            "startup-fixture",
            Some("unused-local-fixture-key"),
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
    let mut app = test_support::fixture_app(ui);
    let before = app.ui.session.messages.len();
    app.session_start_task = Some(tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        crate::extras::hooks::OwnedSessionStartContext::for_test(vec![
            "deferred hook context".into(),
        ])
    }));
    assert!(app.startup_work_pending());
    app.finish_startup_work().await.unwrap();
    assert!(!app.startup_work_pending());
    assert!(
        app.ui.session.messages.len() > before,
        "hook context is adopted before the first turn"
    );
    // Idempotent once joined.
    app.finish_startup_work().await.unwrap();
}
