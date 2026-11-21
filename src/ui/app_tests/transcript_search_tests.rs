use super::test_support::fixture_app;
use super::*;
use crate::session::MessageRole;
use crate::ui::feed::BlockStyle;

#[tokio::test]
async fn search_finds_a_row_far_back_steps_wraps_and_escape_restores_the_view() {
    let root = crate::tests::OwnedTestDirectory::new("app-transcript-search");
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
    let mut session = Session::new("openrouter", "test-model", 128_000, "");
    session.add_message(MessageRole::User, "hello");
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
            "openrouter",
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
    for index in 0..300 {
        let text = match index {
            40 | 200 => format!("line {index} has the NEEDLE inside"),
            _ => format!("filler line {index}"),
        };
        app.renderer.feed_mut().push_block(BlockStyle::Agent, text);
    }
    app.renderer.render_viewport().unwrap();
    assert_eq!(app.renderer.scroll_offset(), 0);

    let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    app.start_transcript_search(None).unwrap();
    for c in "needle".chars() {
        app.handle_key_event(key(c)).await.unwrap();
    }
    // Typing jumps to the first match at or below the viewport top; with the
    // viewport at the bottom that is the match nearest the end.
    let visible_text = |app: &App<'_>| {
        let width = app.renderer.max_line_width();
        let lines = app.renderer.feed().lines(width);
        let total = lines.len();
        let visible = app.renderer.visible_lines();
        let top = total.saturating_sub(app.renderer.scroll_offset() + visible);
        (top..(top + visible).min(total))
            .map(|row| lines[row].text.to_string())
            .collect::<Vec<_>>()
    };
    let shows = |app: &App<'_>, needle: &str| visible_text(app).iter().any(|t| t.contains(needle));
    assert!(
        shows(&app, "line 200") || shows(&app, "line 40"),
        "{:?}",
        visible_text(&app)
    );
    // Enter stops typing and steps; stepping walks both matches and wraps.
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    let first = shows(&app, "line 40");
    app.handle_key_event(key('n')).await.unwrap();
    let second = shows(&app, "line 40");
    assert_ne!(first, second, "the second step moved to the other match");
    app.handle_key_event(key('n')).await.unwrap();
    assert_eq!(shows(&app, "line 40"), first, "the third step wraps around");
    assert!(app.renderer.selection_active, "the match is highlighted");

    // Esc restores the offset the search started from.
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.transcript_search.is_none());
    assert_eq!(app.renderer.scroll_offset(), 0);
    assert!(app.renderer.permission_prompt.is_none());
}
