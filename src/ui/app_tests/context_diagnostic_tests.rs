use super::test_support::fixture_app;
use super::*;

fn saved_files(directory: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut entries: Vec<_> = match std::fs::read_dir(directory) {
        Ok(entries) => entries.map(|entry| entry.unwrap().file_name()).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("cannot inspect owned persistence root: {error}"),
    };
    entries.sort();
    entries
}

#[tokio::test]
async fn actual_context_and_doctor_are_pure_cold_busy_and_built_without_provider_requests() {
    let root = crate::tests::OwnedTestDirectory::new("context-diagnostic-app");
    let environment = [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.path().join(name).into_os_string())));
    let _environment = crate::tests::ScopedProcessEnv::set(&environment);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cli = crate::cli::Cli {
        tools: vec!["read".into()],
        no_sandbox: true,
        api_key: Some("owned-unused-key".into()),
        system_prompt: Some("OVERRIDE_PERSONA".into()),
        append_system_prompt: Some("CAPTURED_APPEND_界".into()),
        ..Default::default()
    };
    let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({"custom_providers":{"context-local-test":{"provider_type":"openai","base_url":format!("http://{address}/v1"),"api_style":"completions"}}})).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    context.agents = Some("CAPTURED_AGENTS_界".into());
    context.current_prompt = Some("CAPTURED_PROMPT_界".into());
    let added = root.path().join("added.md");
    std::fs::write(&added, "CAPTURED_EXTRA_界").unwrap();
    context.extra_files.push(added.clone());
    context
        .extra_file_contents
        .insert(added.clone(), Arc::new("CAPTURED_EXTRA_界".into()));
    let paths = crate::paths::process_paths_ref().unwrap();
    let suffix = paths.suffix_file();
    std::fs::create_dir_all(suffix.parent().unwrap()).unwrap();
    std::fs::write(&suffix, "CAPTURED_SUFFIX_界").unwrap();
    let mut session = Session::new(
        "context-local-test",
        "test-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    session.add_message(crate::session::MessageRole::User, "PRIVATE_USER_CONTENT");
    #[cfg(feature = "goal")]
    {
        let mut goal = crate::extras::goal::Goal::new("PENDING_FIRST_ROUND", Vec::new()).unwrap();
        goal.context_file = Some(root.path().join("UNREAD_PENDING_PLAN.md"));
        goal.autostart = true;
        session.goal_store.set(goal, false).unwrap();
    }

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
            "context-local-test",
            Some("owned-unused-key"),
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
    let (overhead, parts) = crate::agent::builder::estimate_overhead_breakdown(
        app.ui.context,
        false,
        &cli,
        &cfg,
        &app.ui.sandbox,
        None,
    );
    app.ui.session.overhead_tokens = overhead;
    app.ui.session.overhead_parts = parts;
    let stored_before = saved_files(&paths.sessions_dir());
    let before = serde_json::to_vec(app.ui.session).unwrap();
    let overhead_before = (
        app.ui.session.overhead_tokens,
        app.ui.session.overhead_parts.clone(),
    );
    app.run.is_running = true;
    assert_eq!(
        super::super::classify_submission(true, "/context", &app.ui.context.prompts),
        super::super::SubmitAction::Run
    );
    app.input.load_text(" \t/context");
    crate::startup::heap_future(|| {
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    })
    .await
    .unwrap();
    assert!(app.run.agent.is_none() && app.run.main_abort.is_none());
    assert!(app.run.pending_inputs.is_empty());
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert_eq!(
        (
            app.ui.session.overhead_tokens,
            app.ui.session.overhead_parts.clone()
        ),
        overhead_before
    );
    assert_eq!(
        saved_files(&paths.sessions_dir()),
        stored_before,
        "diagnostic must not save session"
    );
    // Listings print as one frame, so rows that scroll past inside it are
    // never painted; read content from the transcript, and check that
    // neither the transcript nor the painted bytes leak private text.
    let output = app.renderer.transcript_text();
    assert!(output.contains("Context composition:") && output.contains("agent not built"));
    assert!(!output.contains("PRIVATE_USER_CONTENT"));
    assert!(
        !app.renderer
            .captured_output()
            .contains("PRIVATE_USER_CONTENT")
    );
    app.input.load_text(" \t/doctor");
    crate::startup::heap_future(|| {
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    })
    .await
    .unwrap();
    let output = app.renderer.transcript_text();
    assert!(
        output.contains("price source: captured session rates")
            && output.contains("Terminal attachment:")
    );
    assert!(app.run.agent.is_none() && app.run.main_abort.is_none());
    assert!(app.run.pending_inputs.is_empty() && app.run.is_running);
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert_eq!(saved_files(&paths.sessions_dir()), stored_before);
    app.run.is_running = false;
    std::fs::create_dir_all(root.path().join(crate::product::PROJECT_DIRECTORY)).unwrap();
    std::fs::write(
        root.path()
            .join(crate::product::PROJECT_DIRECTORY)
            .join("config.toml"),
        "model='test-model'\n",
    )
    .unwrap();
    crate::config::load::trust_project_config(
        &root
            .path()
            .join(crate::product::PROJECT_DIRECTORY)
            .join("config.toml"),
        &paths.project_config_trust_file(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("CLAUDE.local.md"),
        "PRIVATE_LOCAL_INSTRUCTION_BODY",
    )
    .unwrap();
    let built = {
        let build_context = app.ui.agent_build_ctx();
        crate::startup::heap_future(|| build_context.rebuild_agent("test-model", false)).await
    };
    let definitions = crate::startup::heap_future(|| built.context_tool_definitions())
        .await
        .unwrap();
    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == "read")
    );
    let definition_count = definitions.len();
    let snapshot = built.context_snapshot().unwrap();
    let preamble = built.context_preamble_for_test();
    assert!(preamble.contains("PRIVATE_LOCAL_INSTRUCTION_BODY"));
    assert_eq!(
        snapshot
            .instruction_sources
            .as_ref()
            .unwrap()
            .diagnostics()
            .unwrap()
            .source_count,
        1
    );
    assert_eq!(snapshot.bytes, preamble.len());
    assert_eq!(snapshot.tokens, Session::estimate_tokens(preamble));
    let mut cursor = 0;
    let mut reconstructed = String::new();
    for ((label, range), (token_label, tokens)) in snapshot.ranges.iter().zip(&snapshot.parts) {
        assert_eq!(range.start, cursor);
        assert_eq!(label, token_label);
        assert!(preamble.is_char_boundary(range.start) && preamble.is_char_boundary(range.end));
        assert_eq!(*tokens, Session::estimate_tokens(&preamble[range.clone()]));
        reconstructed.push_str(&preamble[range.clone()]);
        cursor = range.end;
    }
    assert_eq!(cursor, preamble.len());
    assert_eq!(reconstructed.as_bytes(), preamble.as_bytes());
    let markers = [
        "OVERRIDE_PERSONA",
        "CAPTURED_AGENTS_界",
        "CAPTURED_PROMPT_界",
        "CAPTURED_EXTRA_界",
        "CAPTURED_APPEND_界",
        "CAPTURED_SUFFIX_界",
    ];
    let positions: Vec<_> = markers
        .iter()
        .map(|marker| preamble.find(marker).unwrap())
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        snapshot
            .parts
            .iter()
            .any(|(label, _)| *label == "Repository instructions")
    );
    let saved_snapshot = snapshot.parts.clone();
    let saved_tokens = snapshot.tokens;
    app.run.agent = Some(built);
    // Changes after build cannot alter the diagnostic or cause a reread.
    std::fs::remove_file(&suffix).unwrap();
    std::fs::remove_file(&added).unwrap();
    std::fs::remove_file(root.path().join("CLAUDE.local.md")).unwrap();
    #[cfg(unix)]
    {
        drop(crate::tests::process_gate::ProcessGate::new(&suffix).unwrap());
        drop(crate::tests::process_gate::ProcessGate::new(&added).unwrap());
        drop(
            crate::tests::process_gate::ProcessGate::new(&root.path().join("CLAUDE.local.md"))
                .unwrap(),
        );
    }
    app.renderer.clear_content().unwrap();
    crate::startup::heap_future(|| app.run_slash_command("/context verbose"))
        .await
        .unwrap();
    let output = app.renderer.captured_output();
    assert!(output.contains("actual built-agent preamble snapshot"));
    assert!(output.contains("CLAUDE.local.md"));
    assert!(!output.contains("PRIVATE_LOCAL_INSTRUCTION_BODY"));
    assert!(output.contains(&format!(
        "{definition_count} already-registered definitions"
    )));
    assert!(
        app.run.main_abort.is_none() && !app.run.is_running,
        "read-only diagnostics must not start a pending goal round"
    );

    assert!(!output.contains("CAPTURED_") && !output.contains("PRIVATE_USER_CONTENT"));
    assert_eq!(
        app.run
            .agent
            .as_ref()
            .unwrap()
            .context_snapshot()
            .unwrap()
            .parts,
        saved_snapshot
    );
    assert_eq!(
        app.run
            .agent
            .as_ref()
            .unwrap()
            .context_snapshot()
            .unwrap()
            .tokens,
        saved_tokens
    );
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert!(saved_files(&paths.sessions_dir()) == stored_before);
    crate::startup::heap_future(|| app.run_slash_command("/doctor"))
        .await
        .unwrap();
    // Traverse the actual report scrollback; rich feature reports are
    // intentionally longer than one viewport.
    for _ in 0..4 {
        crate::startup::heap_future(|| {
            app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
        })
        .await
        .unwrap();
        app.renderer.render_viewport().unwrap();
    }
    let output = app.renderer.captured_output();
    assert!(output.contains("Directories:") && output.contains("Provider and model:"));
    assert!(!output.contains("CAPTURED_") && !output.contains("PRIVATE_USER_CONTENT"));
    assert!(app.run.main_abort.is_none() && !app.run.is_running);
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert_eq!(saved_files(&paths.sessions_dir()), stored_before);
    app.renderer.scroll_to_bottom().unwrap();
    crate::startup::heap_future(|| app.run_slash_command("/doctor unexpected"))
        .await
        .unwrap();
    assert!(app.renderer.captured_output().contains("usage: /doctor"));
    crate::startup::heap_future(|| app.run_slash_command("/context unexpected"))
        .await
        .unwrap();
    assert!(app.renderer.captured_output().contains("usage: /context"));
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert!(
        tokio::time::timeout(Duration::from_millis(80), listener.accept())
            .await
            .is_err(),
        "diagnostic must not warm provider cache or make a model request"
    );
}
