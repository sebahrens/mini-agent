use super::test_support::fixture_app;
use super::*;
use crate::session::MessageRole;

#[tokio::test]
async fn actual_rewind_confirmation_restores_shell_command_in_editor() {
    let root = crate::tests::OwnedTestDirectory::new("app-shell-rewind");
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
    session.add_message(MessageRole::User, "earlier prompt");
    session.add_message(MessageRole::Assistant, "earlier answer");
    session.add_shell_interaction("!printf héllo", "CAPTURED_SHELL_CONTENTS");
    session.add_message(MessageRole::User, "later prompt");
    let before = serde_json::to_value(&session.messages).unwrap();
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
    let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.input.set_vim_enabled(true);
    app.handle_key_event(escape).await.unwrap();
    assert_eq!(app.input.vim_label(), "vim:NORMAL");
    app.input.set_vim_enabled(false);
    assert!(app.input.picker.is_none());
    app.rewind_escape = Some(std::time::Instant::now() - Duration::from_millis(501));
    app.handle_key_event(escape).await.unwrap();
    assert!(
        app.input.picker.is_none(),
        "outside the 500ms window starts a new pair"
    );
    app.handle_key_event(escape).await.unwrap();
    assert!(app.input.picker.as_ref().is_some_and(Picker::active));
    app.handle_key_event(escape).await.unwrap(); // modal cancellation
    app.input.load_text("draft");
    app.handle_key_event(escape).await.unwrap();
    app.handle_key_event(escape).await.unwrap();
    assert!(!app.input.picker.as_ref().is_some_and(Picker::active));
    app.input.load_text("");
    app.run.is_running = true;
    app.handle_key_event(escape).await.unwrap();
    app.handle_key_event(escape).await.unwrap();
    assert!(!app.input.picker.as_ref().is_some_and(Picker::active));
    app.run.is_running = false;
    app.handle_key_event(escape).await.unwrap();
    app.handle_key_event(escape).await.unwrap();
    for code in [KeyCode::Up, KeyCode::Enter, KeyCode::Enter] {
        app.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert_eq!(app.input.buffer, "!printf héllo");
    assert_eq!(app.ui.session.messages.len(), 2);
    assert!(app.ui.session.redo());
    assert_eq!(
        serde_json::to_value(&app.ui.session.messages).unwrap(),
        before
    );

    #[cfg(unix)]
    {
        std::fs::write(root.path().join("rewound.txt"), "original").unwrap();
        let write = crate::agent::tools::write::WriteTool::new(None, None, None)
            .with_workspace(root.path());
        let tracker = crate::agent::tools::ReadTracker::new(true);
        let read = crate::agent::tools::read::ReadTool::new_with_tracker(
            None,
            None,
            None,
            100,
            tracker.clone(),
        )
        .with_workspace(root.path());
        let overwrite =
            crate::agent::tools::write::WriteTool::new_with_tracker(None, None, None, tracker)
                .with_workspace(root.path());
        use rig::tool::Tool;
        let recorder = app
            .ui
            .session
            .file_checkpoints
            .recorder(&app.ui.session.id, 2, true);
        crate::session::file_checkpoints::scope(Some(recorder), async {
            read.call(crate::agent::tools::ReadArgs {
                pages: None,
                raw_notebook: false,
                visual: false,
                full_resolution: false,
                character_offset: None,
                character_limit: None,
                path: "rewound.txt".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
            overwrite
                .call(crate::agent::tools::WriteArgs {
                    path: "rewound.txt".into(),
                    content: "modified".into(),
                    overwrite: true,
                })
                .await
                .unwrap();
            write
                .call(crate::agent::tools::WriteArgs {
                    path: "created.txt".into(),
                    content: "new".into(),
                    overwrite: false,
                })
                .await
                .unwrap();
        })
        .await;
        // A failed/preflight-conflicted restore must also discard caches:
        // later I/O failures can follow already completed file publications.
        std::fs::write(root.path().join("rewound.txt"), "operator changed").unwrap();
        app.ui.prebuild_invalidated.store(false, Ordering::SeqCst);
        app.input.load_text("");
        app.handle_key_event(escape).await.unwrap();
        app.handle_key_event(escape).await.unwrap();
        for code in [KeyCode::Up, KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
            app.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("rewound.txt")).unwrap(),
            "operator changed"
        );
        assert!(root.path().join("created.txt").exists());
        assert!(app.ui.prebuild_invalidated.load(Ordering::SeqCst));
        assert_eq!(
            serde_json::to_value(&app.ui.session.messages).unwrap(),
            before
        );
        std::fs::write(root.path().join("rewound.txt"), "modified").unwrap();
        app.input.load_text("");
        app.handle_key_event(escape).await.unwrap();
        app.handle_key_event(escape).await.unwrap();
        for code in [KeyCode::Up, KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
            app.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("rewound.txt")).unwrap(),
            "original"
        );
        assert!(!root.path().join("created.txt").exists());
        assert_eq!(
            serde_json::to_value(&app.ui.session.messages).unwrap(),
            before,
            "files-only preserves exact conversation"
        );
        assert!(app.run.agent_rx.is_none() && !app.run.is_running);
        // Recreate a file, then confirm both. The original mutable history is
        // still exactly the same picker source; only its recorded file state changed.
        let recorder = app
            .ui
            .session
            .file_checkpoints
            .recorder(&app.ui.session.id, 2, true);
        crate::session::file_checkpoints::scope(
            Some(recorder),
            write.call(crate::agent::tools::WriteArgs {
                path: "created.txt".into(),
                content: "second".into(),
                overwrite: false,
            }),
        )
        .await
        .unwrap();
        app.input.load_text("");
        app.handle_key_event(escape).await.unwrap();
        app.handle_key_event(escape).await.unwrap();
        for code in [
            KeyCode::Up,
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Enter,
        ] {
            app.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert_eq!(app.ui.session.messages.len(), 2);
        assert_eq!(app.input.buffer, "!printf héllo");
        assert!(!root.path().join("created.txt").exists());
        assert!(app.ui.session.redo());
    }
    app.input.load_text("");
    app.handle_key_event(escape).await.unwrap();
    app.handle_key_event(escape).await.unwrap();
    let identity = app.ui.session.id.clone();
    app.ui.session.id = "successor".into();
    for code in [KeyCode::Enter, KeyCode::Enter] {
        app.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert_eq!(
        app.ui.session.messages.len(),
        4,
        "stale selection cannot truncate successor history"
    );
    app.ui.session.id = identity;
}

#[tokio::test]
async fn actual_provider_error_restores_draft_without_starting_queued_requests() {
    let root = crate::tests::OwnedTestDirectory::new("app-provider-error-draft");
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
    let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
        "custom_providers": {"error-fixture": {"provider_type":"openai", "base_url":"http://127.0.0.1:1", "api_style":"completions"}}
    })).unwrap();
    for progress in [false, true] {
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("error-fixture", "test-model", 128_000, "");
        session.add_message(MessageRole::User, "earlier prompt");
        session.add_message(MessageRole::Assistant, "earlier answer");
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
                "error-fixture",
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
        let pending = PendingMainTurn::capture(app.ui.session, "failed provider prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        if progress {
            app.ui.session.add_tool_call_with_id(
                "provider-error-read",
                "read",
                &serde_json::json!({"path":"fixture.txt"}),
            );
        }
        app.input.load_text("follow-up after provider failure");
        app.run.pending_inputs.extend([
            "must not call provider first".into(),
            "must not call provider second".into(),
        ]);
        app.handle_agent_event(AgentEvent::Error {
            message: "fixture provider error".into(),
            interactions: Vec::new(),
        })
        .await
        .unwrap();
        // A queued run would install its agent, task and pending user turn
        // before sending a provider request. All admission state stays idle.
        assert!(!app.run.is_running);
        assert!(app.run.agent.is_none());
        assert!(app.run.main_abort.is_none());
        assert!(app.run.pending_turn.is_none());
        assert!(app.run.pending_inputs.is_empty());
        assert!(
            !app.ui
                .session
                .messages
                .iter()
                .any(|message| message.content.starts_with("must not call provider"))
        );
        app.run_queue_command("").unwrap();
        let lines = app.renderer.feed().line_count(4096);
        let shown = app
            .renderer
            .feed()
            .selected_text(4096, 0, lines - 1)
            .unwrap();
        assert!(shown.contains("2 queued inputs returned"), "{shown}");
        assert!(shown.contains("queue is empty"), "{shown}");
        if progress {
            assert_eq!(app.input.buffer, "follow-up after provider failure");
            assert!(
                app.ui
                    .session
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::ToolResult)
            );
        } else {
            assert_eq!(app.input.buffer, "failed provider prompt");
            assert_eq!(app.ui.session.messages.len(), 2);
            app.input.clear_buffer();
            app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            assert_eq!(app.input.buffer, "follow-up after provider failure");
        }
    }
}

#[tokio::test]
async fn actual_context_exhaustion_preserves_followup_draft_usage_and_drops_queue() {
    let root = crate::tests::OwnedTestDirectory::new("app-context-stop-draft");
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
            "MINI_AGENT_LOCAL_DATA_DIR",
            Some(root.path().join("local-data").into_os_string()),
        ),
        (
            "MINI_AGENT_STATE_DIR",
            Some(root.path().join("state").into_os_string()),
        ),
        (
            "MINI_AGENT_CACHE_DIR",
            Some(root.path().join("cache").into_os_string()),
        ),
        (
            "MINI_AGENT_CREDENTIALS_DIR",
            Some(root.path().join("credentials").into_os_string()),
        ),
    ]);
    let cli = crate::cli::Cli {
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
        "compact_enabled": true, "mid_turn_compact_threshold": 0.8
    }))
    .unwrap();
    for progress in [false, true] {
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "test-model", 128_000, "");
        session.working_dir = workspace.root().to_string_lossy().into();
        session.add_message(MessageRole::User, "earlier prompt");
        session.add_message(MessageRole::Assistant, "earlier answer");
        session.set_calibration(100, 20);
        session.total_input_tokens = 10;
        session.total_output_tokens = 2;
        session.total_real_input_tokens = 10;
        session.input_token_cost = 2.0;
        session.output_token_cost = 4.0;
        session.total_cost = 0.5;
        let before = session.clone();
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
        let pending = PendingMainTurn::capture(app.ui.session, "context-exhausted prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        app.run.awaiting_compaction_relief = true;
        if progress {
            app.ui.session.add_tool_call_with_id(
                "context-stop-read",
                "read",
                &serde_json::json!({"path":"fixture.txt"}),
            );
            app.run.response_buf = "observed partial answer".into();
        }
        app.input
            .load_text("follow-up draft héllo after context exhaustion");
        app.run.pending_inputs.extend([
            "must not start queued first".into(),
            "must not start queued second".into(),
        ]);
        app.handle_agent_event(AgentEvent::UsageDelta {
            usage: crate::event::UsageDelta {
                input_tokens: 110_000,
                output_tokens: 100,
                total_tokens: 110_100,
                ..Default::default()
            },
            context_complete: true,
            awaits_compaction_decision: true,
        })
        .await
        .unwrap();
        assert!(!app.run.is_running);
        assert!(app.run.pending_turn.is_none());
        assert!(app.run.main_abort.is_none());
        assert!(app.run.compaction_decision_tx.is_none());
        assert!(!app.run.awaiting_compaction_relief);
        assert!(app.run.pending_inputs.is_empty());
        // Exercise the ordinary queue admission point too: context stops
        // must never launch the next queued prompt on a later idle tick.
        app.finalize_turn().await.unwrap();
        assert!(app.run.agent.is_none());
        assert!(app.run.pending_turn.is_none());
        assert!(app.run.main_abort.is_none());
        assert!(
            !app.ui
                .session
                .messages
                .iter()
                .any(|message| message.content.starts_with("must not start queued"))
        );
        assert_eq!(app.ui.session.total_input_tokens, 110_010);
        assert_eq!(app.ui.session.total_output_tokens, 102);
        assert_eq!(app.ui.session.total_real_input_tokens, 110_010);
        assert!((app.ui.session.total_cost - 0.7204).abs() < 1e-12);
        let persisted = crate::session::storage::load_session_exact(&app.ui.session.id)
            .unwrap()
            .expect("context-stop settlement persists the session");
        assert_eq!(
            serde_json::to_value(&persisted.messages).unwrap(),
            serde_json::to_value(&app.ui.session.messages).unwrap()
        );
        assert_eq!(persisted.total_input_tokens, 110_010);
        assert!((persisted.total_cost - 0.7204).abs() < 1e-12);
        app.run_queue_command("").unwrap();
        let lines = app.renderer.feed().line_count(4096);
        let shown = app
            .renderer
            .feed()
            .selected_text(4096, 0, lines - 1)
            .unwrap();
        assert!(
            shown.contains("not enough context to continue this turn"),
            "{shown}"
        );
        assert!(shown.contains("2 queued inputs returned"), "{shown}");
        assert!(shown.contains("queue is empty"), "{shown}");
        if progress {
            assert_eq!(
                app.input.buffer,
                "follow-up draft héllo after context exhaustion"
            );
            assert_eq!(
                app.ui.session.messages.last().unwrap().content,
                "observed partial answer"
            );
            assert!(app.ui.session.messages.iter().any(|message| {
                message.role == MessageRole::ToolResult
                    && message.tool_call_id.as_deref() == Some("context-stop-read")
                    && message
                        .content
                        .contains(crate::agent::runner::UNKNOWN_TOOL_OUTCOME)
            }));
            assert_eq!(app.ui.session.calibrated_tokens, 110_100);
        } else {
            assert_eq!(app.input.buffer, "context-exhausted prompt");
            assert_eq!(
                serde_json::to_value(&app.ui.session.messages).unwrap(),
                serde_json::to_value(&before.messages).unwrap()
            );
            assert_eq!(
                app.ui.session.total_estimated_tokens,
                before.total_estimated_tokens
            );
            assert_eq!(app.ui.session.calibrated_tokens, before.calibrated_tokens);
            app.input.clear_buffer();
            app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            assert_eq!(
                app.input.buffer,
                "follow-up draft héllo after context exhaustion"
            );
        }
    }
}

/// mini-agent-wr9nw: an over-threshold completion is acted on during a goal
/// round and under `--no-session`, not just in an ordinary persisted turn.
#[tokio::test]
async fn mid_turn_pressure_gate_is_open_for_goal_rounds_and_no_session() {
    for (no_session, goal) in [(true, false), (false, true), (true, true)] {
        let root = crate::tests::OwnedTestDirectory::new("app-mid-turn-gate");
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
                "MINI_AGENT_LOCAL_DATA_DIR",
                Some(root.path().join("local-data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
            (
                "MINI_AGENT_CREDENTIALS_DIR",
                Some(root.path().join("credentials").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_tools: true,
            no_sandbox: true,
            no_session,
            ..Default::default()
        };
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "compact_enabled": true, "mid_turn_compact_threshold": 0.8
        }))
        .unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("openrouter", "test-model", 128_000, "");
        session.working_dir = workspace.root().to_string_lossy().into();
        session.add_message(MessageRole::User, "earlier prompt");
        session.add_message(MessageRole::Assistant, "earlier answer");
        #[cfg(feature = "loop")]
        if goal {
            session
                .goal_store
                .set(
                    crate::extras::goal::Goal::new("finish the task", Vec::new()).unwrap(),
                    false,
                )
                .unwrap();
            assert!(session.goal_store.is_active());
        }
        #[cfg(not(feature = "loop"))]
        let _ = goal;
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
        app.run.is_running = true;
        app.handle_agent_event(AgentEvent::UsageDelta {
            usage: crate::event::UsageDelta {
                input_tokens: 110_000,
                output_tokens: 100,
                total_tokens: 110_100,
                ..Default::default()
            },
            context_complete: true,
            awaits_compaction_decision: true,
        })
        .await
        .unwrap();
        assert!(
            app.run.pending_compaction_pressure.is_some(),
            "over-threshold usage must request relief (no_session={no_session}, goal={goal})"
        );
    }
}

#[tokio::test]
async fn actual_abort_and_failure_preserve_drafts_and_report_discarded_queue() {
    let root = crate::tests::OwnedTestDirectory::new("app-interrupt-drafts");
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
    for fail in [false, true] {
        for progress in [false, true] {
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("openrouter", "test-model", 128_000, "");
            session.add_message(MessageRole::User, "earlier prompt");
            session.add_message(MessageRole::Assistant, "earlier answer");
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
                skill_services: Arc::new(
                    crate::extras::js::skills::session::SkillServiceOwner::new(),
                ),
                #[cfg(feature = "mcp")]
                mcp_manager: None,
            };
            let mut app = fixture_app(ui);
            let pending = PendingMainTurn::capture(app.ui.session, "interrupted prompt");
            crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
            if progress {
                app.ui.session.add_tool_call_with_id(
                    "interrupted-read",
                    "read",
                    &serde_json::json!({"path":"fixture.txt"}),
                );
            }
            app.input.load_text("follow-up draft héllo");
            app.run
                .pending_inputs
                .extend(["queued first".into(), "queued second".into()]);
            if fail {
                app.fail_pending_main_turn().await;
            } else {
                app.abort_main_run().await.unwrap();
            }
            assert!(!app.run.is_running);
            assert!(app.run.pending_inputs.is_empty());
            app.run_queue_command("").unwrap();
            let lines = app.renderer.feed().line_count(4096);
            let shown = app
                .renderer
                .feed()
                .selected_text(4096, 0, lines - 1)
                .unwrap();
            assert!(shown.contains("2 queued inputs returned"), "{shown}");
            assert!(shown.contains("queue is empty"), "{shown}");
            if progress {
                assert_eq!(app.input.buffer, "follow-up draft héllo");
                assert!(
                    app.ui
                        .session
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::ToolResult)
                );
            } else {
                assert_eq!(app.input.buffer, "interrupted prompt");
                assert_eq!(app.ui.session.messages.len(), 2);
                app.input.clear_buffer();
                app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                    .await
                    .unwrap();
                assert_eq!(app.input.buffer, "follow-up draft héllo");
            }
        }
    }
}

/// Registers what a foreground command printed when its future is dropped, as
/// the shell tool's guard does.
struct DropRegisters(&'static str, &'static str);
impl Drop for DropRegisters {
    fn drop(&mut self) {
        crate::agent::runner::record_settled_result(self.0, self.1);
    }
}

#[tokio::test]
async fn interrupt_keeps_the_partial_output_of_a_running_shell_call() {
    let root = crate::tests::OwnedTestDirectory::new("app-interrupt-shell-output");
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
    session.add_message(MessageRole::User, "earlier prompt");
    session.add_message(MessageRole::Assistant, "earlier answer");
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
    let pending = PendingMainTurn::capture(app.ui.session, "run the ticker");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.ui.session.add_tool_call_with_id(
        "interrupt-shell-call",
        "shell",
        &serde_json::json!({"command": "for i in 1 2 3; do echo tick $i; sleep 5; done"}),
    );
    // The running turn: a task that owns the event channel (so retiring waits
    // for it to be dropped) and a command guard that registers its output
    // only when that task's future is dropped.
    let (tx, rx) = mpsc::channel(1);
    app.run.agent_rx = Some(rx);
    let task = tokio::spawn(async move {
        let _events = tx;
        let _guard = DropRegisters(
            "interrupt-shell-call",
            "[Interrupted by the user before the command finished] tick 1\ntick 2",
        );
        std::future::pending::<()>().await;
    });
    app.run.main_abort = Some(task.abort_handle());
    app.run.is_running = true;
    tokio::task::yield_now().await;
    app.abort_main_run().await.unwrap();
    let result = app
        .ui
        .session
        .messages
        .iter()
        .find(|m| {
            m.role == MessageRole::ToolResult
                && m.tool_call_id.as_deref() == Some("interrupt-shell-call")
        })
        .expect("the interrupted call is closed");
    assert!(
        result.content.contains("Interrupted by the user") && result.content.contains("tick 2"),
        "{}",
        result.content
    );
    assert!(!result.content.contains("outcome is unknown"));
}
