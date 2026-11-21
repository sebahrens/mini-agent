use super::test_support::fixture_app;
use super::*;
#[cfg(feature = "git-worktree")]
#[tokio::test]
async fn usd_budget_actual_app_git_proposal_is_denied_before_context_actor_or_request() {
    for case in ["zero", "ledger-only", "corrupt"] {
        let root = crate::tests::OwnedTestDirectory::new("cost-git-proposal-app");
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
            no_sandbox: true,
            no_session: true,
            ..Default::default()
        };
        let mut cfg:crate::config::Config = serde_json::from_value(serde_json::json!({"max_budget_usd":0.0,"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}}})).unwrap();
        cfg.max_budget_usd = Some(if case == "zero" { 0.0 } else { 1.0 });
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "local-test",
            "fixture",
            128000,
            root.path().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((1.0, 2.0)));
        session.initialize_spend_account(None).unwrap();
        let checkpoint = serde_json::to_vec(&session).unwrap();
        let ledger =
            crate::spend::Ledger::new(&crate::paths::process_paths_ref().unwrap().state_dir);
        if case == "ledger-only" {
            session
                .spend_account
                .observe(&crate::spend::Receipt::new(
                    &session.id,
                    "local-test",
                    "fixture",
                    crate::spend::RequestKind::Turn,
                    crate::event::UsageDelta {
                        input_tokens: 2_000_000,
                        total_tokens: 2_000_000,
                        ..Default::default()
                    },
                    crate::spend::PriceSnapshot::capture(
                        Some((1.0, 2.0)),
                        Default::default(),
                        false,
                    ),
                ))
                .unwrap();
            ledger.flush(&session.spend_account).unwrap();
        } else if case == "corrupt" {
            let path = crate::paths::process_paths_ref()
                .unwrap()
                .state_dir
                .join("usage.jsonl");
            crate::fs::private_atomic_create_sync(&path, b"{\"crash\":").unwrap();
        }
        let bytes_before = std::fs::read(
            crate::paths::process_paths_ref()
                .unwrap()
                .state_dir
                .join("usage.jsonl"),
        )
        .ok();
        session = serde_json::from_slice(&checkpoint).unwrap();
        assert!(!session.spend_account.ledger_ready());
        assert_eq!(session.spend_account.snapshot().recorded_spend(), 0.0);

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
                None,
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
        let error = app.begin_git_command("/pr").await.unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::spend::run::BudgetStopped>()
                .is_some()
        );
        assert!(
            app.chain.git.work.is_none()
                && app.run.main_abort.is_none()
                && app.run.agent_rx.is_none()
        );
        assert!(!app.run.is_running && app.ui.session.messages.is_empty());
        assert_eq!(
            std::fs::read(
                crate::paths::process_paths_ref()
                    .unwrap()
                    .state_dir
                    .join("usage.jsonl")
            )
            .ok(),
            bytes_before
        );
        assert_eq!(
            app.ui.session.spend_account.snapshot().tracking_error,
            case == "corrupt"
        );
        if case == "ledger-only" {
            assert_eq!(app.ui.session.recorded_total_cost(), 2.0);
        }
    }
}

#[tokio::test]
async fn usd_budget_actual_app_cost_query_is_readonly_and_queue_stays_visible() {
    let root = crate::tests::OwnedTestDirectory::new("cost-query-app");
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
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg:crate::config::Config=serde_json::from_value(serde_json::json!({"max_budget_usd":2.0,"custom_providers":{"local-test":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}}})).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("local-test", "test", 128_000, root.path().to_str().unwrap());
    session.set_input_output_prices(Some((2.0, 4.0)));
    session.total_cost = 10.0;
    let paths = crate::paths::process_paths_ref().unwrap();
    let ledger = crate::spend::Ledger::new(&paths.state_dir);
    session.configure_spend(Some(2.0), &ledger).unwrap();
    let receipt = crate::spend::Receipt::new(
        &session.id,
        "openai",
        "model\x1b]52;c;ZXZpbA==\x07",
        crate::spend::RequestKind::Turn,
        crate::event::UsageDelta {
            input_tokens: 1_000_000,
            ..Default::default()
        },
        crate::spend::PriceSnapshot::capture(Some((2.0, 4.0)), Default::default(), false),
    );
    session.spend_account.observe(&receipt).unwrap();
    ledger.flush(&session.spend_account).unwrap();
    let ledger_before = std::fs::read(paths.state_dir.join("usage.jsonl")).unwrap();
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
    app.run.is_running = true;
    app.input.load_text(" \t/cost");
    crate::startup::heap_future(|| {
        app.handle_key_event(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ))
    })
    .await
    .unwrap();
    assert!(app.run.is_running);
    assert!(app.run.agent.is_none() && app.run.main_abort.is_none());
    assert!(app.run.pending_inputs.is_empty());
    app.run.is_running = false;
    let text = app.renderer.captured_output();
    for expected in [
        "Session recorded spend: $12.000000",
        "Legacy/imported estimate: $10.000000",
        "openai/model",
        "Today (UTC): $2.000000",
        "All time: $2.000000",
        "input 1000000",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("\x1b]52"));
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert_eq!(
        std::fs::read(paths.state_dir.join("usage.jsonl")).unwrap(),
        ledger_before
    );
    assert!(!paths.data_dir.join("sessions").exists());
    app.run.pending_inputs.push_back("queued follow-up".into());
    app.finalize_turn().await.unwrap();
    assert_eq!(
        app.run.pending_inputs.front().map(|s| s.as_str()),
        Some("queued follow-up")
    );
    assert!(!app.run.is_running && app.run.agent.is_none() && app.run.main_abort.is_none());
    assert!(
        app.renderer
            .captured_output()
            .contains("Queued input is retained")
    );
    assert_eq!(
        app.ui.session.spend_account.snapshot().recorded_spend(),
        12.0
    );
}
