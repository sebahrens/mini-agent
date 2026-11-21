use super::test_support::fixture_app;
use super::*;
/// The session catalogue is read on the blocking pool; poll it in as the
/// event loop's background tick does, then repaint.
async fn finish_session_catalogue(app: &mut App<'_>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.input.picker.as_ref().is_some_and(Picker::is_loading) {
            if app
                .input
                .picker
                .as_mut()
                .is_some_and(Picker::poll_background)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("session catalogue loads");
    assert!(!app.input.picker.as_ref().is_some_and(Picker::is_loading));
    app.refresh().unwrap();
}
fn environment(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
    crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_STATE_DIR",
            Some(root.path().join("state").into_os_string()),
        ),
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(root.path().join("config").into_os_string()),
        ),
        (
            "MINI_AGENT_DATA_DIR",
            Some(root.path().join("data").into_os_string()),
        ),
        (
            "MINI_AGENT_CACHE_DIR",
            Some(root.path().join("cache").into_os_string()),
        ),
    ])
}
fn ui<'a>(
    cli: &'a crate::cli::Cli,
    cfg: &'a config::Config,
    session: &'a mut Session,
    context: &'a mut crate::context::ContextFiles,
    workspace: Arc<crate::paths::WorkspaceBinding>,
) -> UiContext<'a> {
    UiContext::new(
        cli,
        cfg,
        session,
        context,
        workspace,
        crate::provider::create_client(
            "anthropic",
            Some("unused-local-key"),
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap(),
        None,
        None,
        crate::sandbox::Sandbox::new(false, "none"),
        None,
    )
}
fn store(session: &Session) {
    crate::session::storage::atomic_write(
        &crate::session::storage::session_path(&session.id).unwrap(),
        &serde_json::to_string(session).unwrap(),
    )
    .unwrap();
}
#[tokio::test]
async fn actual_app_picker_scope_toggle_and_exact_tool_history_resume_use_owned_renderer() {
    let root = crate::tests::OwnedTestDirectory::new("app-session-picker");
    let _env = environment(&root);
    let foreign = root.path().join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let cli = crate::cli::Cli {
        no_session: true,
        tools: vec!["read".into()],
        ..Default::default()
    };
    let cfg = config::Config {
        session_title_model: Some("off".into()),
        ..Default::default()
    };
    let mut current = Session::new("anthropic", "test", 128_000, "");
    current.working_dir = root.path().to_string_lossy().into();
    current.add_message(crate::session::MessageRole::User, "CURRENT_PICKER_ROW");
    current.add_tool_call_with_id(
        "captured-call",
        "read",
        &serde_json::json!({"path":"a.txt"}),
    );
    current.add_tool_result_with_id("captured-call", "read", "CAPTURED_TOOL_BODY");
    current.permission_allowlist = vec![crate::session::PermissionAllowEntry {
        tool: "bash".into(),
        pattern: "incoming-command".into(),
    }];
    store(&current);
    let mut other = Session::new("anthropic", "test", 128_000, "");
    other.working_dir = foreign.to_string_lossy().into();
    other.add_message(crate::session::MessageRole::User, "FOREIGN_PICKER_ROW");
    other.permission_allowlist = vec![crate::session::PermissionAllowEntry {
        tool: "bash".into(),
        pattern: "*".into(),
    }];
    store(&other);
    let mut session = Session::new("anthropic", "test", 128_000, "");
    session.working_dir = root.path().to_string_lossy().into();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut app = fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
    let mut checker = crate::permission::checker::PermissionChecker::new(
        &crate::permission::PermissionConfigs::default(),
        crate::permission::SecurityMode::Standard,
        Some(root.path().to_owned()),
        None,
    )
    .unwrap();
    checker.add_session_allowlist("bash".into(), "outgoing-command");
    app.ui.permission = Some(Arc::new(std::sync::Mutex::new(checker)));
    app.run_slash_command("/sessions").await.unwrap();
    app.refresh().unwrap();
    assert!(app.renderer.captured_output().contains("loading sessions"));
    finish_session_catalogue(&mut app).await;
    let rendered = app.renderer.captured_output();
    assert!(rendered.contains("Current workspace"));
    assert!(rendered.contains("CURRENT_PICKER_ROW"));
    assert!(!rendered.contains("FOREIGN_PICKER_ROW"));
    assert!(matches!(
        app.handle_user_event(UserEvent::Key(KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )))
        .await
        .unwrap(),
        ControlFlow::Continue(())
    ));
    finish_session_catalogue(&mut app).await;
    let rendered = app.renderer.captured_output();
    assert!(rendered.contains("All workspaces"));
    assert!(rendered.contains("FOREIGN_PICKER_ROW"));
    assert!(matches!(
        app.handle_user_event(UserEvent::Paste("CURRENT_PICKER_ROW".into()))
            .await
            .unwrap(),
        ControlFlow::Continue(())
    ));
    assert!(matches!(
        app.handle_user_event(UserEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .await
        .unwrap(),
        ControlFlow::Continue(())
    ));
    assert_eq!(app.ui.session.id, current.id);
    assert_eq!(app.ui.session.messages.len(), 3);
    assert_eq!(
        app.ui.session.messages[2].content,
        current.messages[2].content
    );
    assert_eq!(
        app.ui.session.messages[2].tool_call_id.as_deref(),
        Some("captured-call")
    );
    assert!(app.input.picker.is_none());
    {
        let mut permission = app.ui.permission.as_ref().unwrap().lock().unwrap();
        assert_eq!(
            permission.check("bash", "incoming-command"),
            crate::permission::checker::CheckResult::Allowed
        );
        assert_eq!(
            permission.check("bash", "outgoing-command"),
            crate::permission::checker::CheckResult::Ask
        );
    }
    app.run_slash_command("/sessions --all").await.unwrap();
    finish_session_catalogue(&mut app).await;
    assert!(matches!(
        app.handle_user_event(UserEvent::Paste("FOREIGN_PICKER_ROW".into()))
            .await
            .unwrap(),
        ControlFlow::Continue(())
    ));
    assert!(matches!(
        app.handle_user_event(UserEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .await
        .unwrap(),
        ControlFlow::Continue(())
    ));
    assert_ne!(app.ui.session.id, other.id);
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert_eq!(app.ui.session.working_dir, current.working_dir);
    assert_eq!(
        app.ui
            .permission
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .check("bash", "incoming-command"),
        crate::permission::checker::CheckResult::Ask
    );
    let foreign_fork = app.ui.session.id.clone();
    app.run_slash_command("/sessions").await.unwrap();
    finish_session_catalogue(&mut app).await;
    assert!(matches!(
        app.handle_user_event(UserEvent::Paste("CURRENT_PICKER_ROW".into()))
            .await
            .unwrap(),
        ControlFlow::Continue(())
    ));
    app.run.is_running = true;
    assert!(matches!(
        app.handle_user_event(UserEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .await
        .unwrap(),
        ControlFlow::Continue(())
    ));
    assert_eq!(
        app.ui.session.id, foreign_fork,
        "busy run cannot publish a selected successor"
    );
}
#[test]
fn actual_session_picker_event_loop_runs_on_one_mib_stack() {
    std::thread::Builder::new()
        .name("session-picker-one-mib".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::startup::heap_future(|| async {
                    let root = crate::tests::OwnedTestDirectory::new("picker-one-mib");
                    let _env = environment(&root);
                    let workspace =
                        Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
                    let cli = crate::cli::Cli {
                        no_session: true,
                        tools: vec!["read".into()],
                        ..Default::default()
                    };
                    let cfg = config::Config {
                        session_title_model: Some("off".into()),
                        ..Default::default()
                    };
                    let mut saved = Session::new("anthropic", "test", 128_000, "");
                    saved.working_dir = root.path().to_string_lossy().into();
                    saved.add_message(crate::session::MessageRole::User, "ONE_MIB_SAVED_ROW");
                    store(&saved);
                    let mut session = Session::new("anthropic", "test", 128_000, "");
                    session.working_dir = root.path().to_string_lossy().into();
                    let mut context =
                        crate::context::load(true).for_workspace_binding(true, &workspace);
                    let mut app =
                        fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
                    let catalogue =
                        crate::session::catalogue::list(&app.ui.workspace, false).unwrap();
                    app.input
                        .start_session_picker(catalogue, app.ui.workspace.clone(), false);
                    app.deferred_user_events.extend([
                        UserEvent::Paste("ONE_MIB_SAVED_ROW".into()),
                        UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                        UserEvent::Paste("/fork 1".into()),
                        UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                        UserEvent::Quit,
                    ]);
                    tokio::time::timeout(
                        Duration::from_secs(3),
                        crate::startup::heap_future(|| app.run_inner()),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert_ne!(app.ui.session.id, saved.id);
                    assert_eq!(app.ui.session.messages.len(), 1);
                    assert!(
                        app.renderer
                            .captured_output()
                            .contains("forked into session")
                    );
                    assert!(app.renderer.captured_output().contains("ONE_MIB_SAVED_ROW"));
                    assert!(app.input.picker.is_none());
                }));
        })
        .unwrap()
        .join()
        .unwrap();
}
#[cfg(all(feature = "git-worktree", feature = "mcp"))]
#[test]
fn worktree116_actual_app_switch_and_mcp_rebind_run_on_one_mib_stack() {
    std::thread::Builder::new()
        .name("worktree-mcp-one-mib".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::startup::heap_future(|| async {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    #[cfg(feature = "hooks")]
                    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
                    let root = crate::tests::OwnedTestDirectory::new("worktree116");
                    let bindings = ["MINI_AGENT_CONFIG_DIR", "MINI_AGENT_DATA_DIR", "MINI_AGENT_LOCAL_DATA_DIR", "MINI_AGENT_STATE_DIR", "MINI_AGENT_CACHE_DIR", "MINI_AGENT_CREDENTIALS_DIR"]
                        .map(|name| (name, Some(root.path().join(name).into_os_string())));
                    let _environment = crate::tests::ScopedProcessEnv::set(&bindings);
                    let repository = root.path().join("repository");
                    let base = root.path().join("worktrees");
                    std::fs::create_dir(&repository).unwrap();
                    std::fs::create_dir(&base).unwrap();
                    let git = crate::git::runner::GitRunner::default();
                    for arguments in [vec!["init", "-b", "main"], vec!["-c", "user.name=Owned116", "-c", "user.email=owned116@example.invalid", "commit", "--allow-empty", "-m", "initial"]] {
                        git.run(&repository, "worktree116 fixture", arguments, crate::git::runner::QUERY_LIMITS).await.unwrap();
                    }
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let address = listener.local_addr().unwrap();
                    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let observed = requests.clone();
                    let (stop, mut server_stop) = tokio::sync::oneshot::channel::<()>();
                    // Poll the owned loopback responder in this same join, with no detached task.
                    let server = async move {
                        loop {
                            let accepted = tokio::select! {
                                _ = &mut server_stop => break,
                                result = listener.accept() => result.unwrap(),
                            };
                            let (mut stream, _) = accepted;
                            let mut bytes = Vec::new();
                            while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                                let mut chunk = [0; 1024];
                                let count = stream.read(&mut chunk).await.unwrap();
                                assert_ne!(count, 0);
                                bytes.extend_from_slice(&chunk[..count]);
                                assert!(bytes.len() <= 16 * 1024);
                            }
                            assert!(bytes.starts_with(b"POST /mcp "), "only MCP initialize is expected");
                            observed.fetch_add(1, Ordering::SeqCst);
                            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        }
                    };
                    let cli = crate::cli::Cli { no_session: true, no_context_files: true, no_tools: true, wt_base_dir: Some(base.to_str().unwrap().to_owned()), ..Default::default() };
                    let cfg: config::Config = serde_json::from_value(serde_json::json!({
                        "session_title_model":"off", "lsp":{"enabled":false},
                        "mcp_servers":{"owned116":{"url":format!("http://{address}/mcp"),"allow_local":true}},
                        "custom_providers":{"owned116":{"provider_type":"openai","base_url":"http://127.0.0.1:9/v1","api_style":"completions"}}
                    })).unwrap();
                    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&repository).unwrap());
                    let mut session = Session::new("owned116", "unused-model", 128_000, repository.to_str().unwrap());
                    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
                    let client = crate::provider::create_client("owned116", Some("unused-local-key"), cfg.custom_providers_map(), cfg.api_keys.as_ref()).unwrap();
                    let ui = UiContext::new(&cli, &cfg, &mut session, &mut context, workspace.clone(), client, None, None, crate::sandbox::Sandbox::new(false,"none").with_workspace_binding(workspace), None);
                    let mut app = fixture_app(ui);
                    app.deferred_user_events.extend([
                        UserEvent::Paste("/worktree owned116".into()),
                        UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                        UserEvent::Quit,
                    ]);
                    let run = async {
                        let result = crate::startup::heap_future(|| app.run_inner()).await;
                        let _ = stop.send(());
                        result
                    };
                    let (_, result) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(server, run) }).await.unwrap();
                    result.unwrap();
                    assert!(requests.load(Ordering::SeqCst) > 0, "actual MCP handshake path must be polled");
                    assert_eq!(app.ui.workspace.root(), base.join("owned116"));
                    app.ui.workspace.validate().unwrap();
                    assert!(app.renderer.captured_output().contains("worktree created: branch 'owned116'"));
                    assert!(!app.run.is_running && app.run.agent_rx.is_none());
                }));
        })
        .unwrap()
        .join()
        .unwrap();
}
#[test]
fn actual_standard_app_event_loop_runs_on_one_mib_stack() {
    standard_app_one_mib_case(false);
    standard_app_one_mib_case(true);
}

fn standard_app_one_mib_case(pending_permission: bool) {
    std::thread::Builder::new()
        .name("standard-app-one-mib".into())
        .stack_size(1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::startup::heap_future(|| async {
                    let root = crate::tests::OwnedTestDirectory::new("standard-app-one-mib");
                    eprintln!("standard App stack fixture: {}", root.path().display());
                    let _env = environment(&root);
                    let workspace =
                        Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
                    let cli = crate::cli::Cli {
                        no_session: true,
                        no_tools: true,
                        ..Default::default()
                    };
                    let cfg = config::Config {
                        session_title_model: Some("off".into()),
                        ..Default::default()
                    };
                    let mut session = Session::new("anthropic", "test", 128_000, "");
                    session.working_dir = root.path().to_string_lossy().into();
                    let mut context =
                        crate::context::load(true).for_workspace_binding(true, &workspace);
                    let mut app =
                        fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
                    let captured_workspace = app.ui.workspace.clone();
                    let captured_prompt = app.ui.context.current_prompt_name.clone();
                    let session_id = app.ui.session.id.clone();
                    let checker = crate::permission::checker::PermissionChecker::new(
                        &crate::permission::PermissionConfigs::default(),
                        crate::permission::SecurityMode::Standard,
                        Some(root.path().to_owned()),
                        None,
                    )
                    .unwrap();
                    let permission = Arc::new(std::sync::Mutex::new(checker));
                    app.ui.permission = Some(permission.clone());
                    let mut decision = if pending_permission {
                        let (reply, decision) = tokio::sync::oneshot::channel();
                        app.begin_permission_request(crate::permission::ask::AskRequest {
                            workdir: None,
                            diagnostic: None,
                            #[cfg(feature = "subagents")]
                            background: None,
                            agent_name: None,
                            tool: "bash".into(),
                            input: "owned unanswered command".into(),
                            #[cfg(feature = "acp")]
                            tool_call_id: Some("owned-stack-call".into()),
                            suggested_pattern: None,
                            additional_allow_patterns: Vec::new(),
                            reply,
                        })
                        .unwrap();
                        Some(decision)
                    } else {
                        None
                    };
                    let events = [
                        UserEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
                        UserEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
                        UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                        UserEvent::Paste("/quit".into()),
                        UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    ];
                    let sender = app.user_tx.clone();
                    let feed = async move {
                        for event in events {
                            sender.send(event).await.unwrap();
                        }
                    };
                    tokio::time::timeout(Duration::from_secs(3), async {
                        let (_, result) =
                            tokio::join!(feed, crate::startup::heap_future(|| app.run_inner()));
                        result
                    })
                    .await
                    .unwrap()
                    .unwrap();
                    assert!(app.renderer.captured_output().contains("/quit"));
                    assert!(app.deferred_user_events.is_empty());
                    assert!(matches!(
                        app.user_rx.try_recv(),
                        Err(mpsc::error::TryRecvError::Empty)
                    ));
                    assert!(!app.run.is_running);
                    assert!(app.permission_prompt.is_none());
                    assert!(app.run.pending_turn.is_none());
                    assert!(app.ui.session.messages.is_empty());
                    assert_eq!(app.ui.session.id, session_id);
                    assert_eq!(app.ui.context.current_prompt_name, captured_prompt);
                    assert!(Arc::ptr_eq(&app.ui.workspace, &captured_workspace));
                    assert!(app.ui.session.permission_allowlist.is_empty());
                    assert_eq!(
                        permission.lock().unwrap().mode(),
                        crate::permission::SecurityMode::Guarded
                    );
                    if let Some(decision) = decision.as_mut() {
                        assert!(matches!(
                            decision.try_recv().unwrap(),
                            crate::permission::ask::UserDecision::Deny
                        ));
                    }
                }));
        })
        .unwrap()
        .join()
        .unwrap();
}
#[tokio::test]
async fn actual_app_title_retirement_joins_owned_work_charges_once_and_rejects_stale_result() {
    let root = crate::tests::OwnedTestDirectory::new("app-title-retirement");
    let _env = environment(&root);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let cfg = config::Config::default();
    let mut session = Session::new("anthropic", "test", 128_000, "");
    session.working_dir = root.path().to_string_lossy().into();
    session.input_token_cost = 2.0;
    session.output_token_cost = 4.0;
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut app = fixture_app(ui(&cli, &cfg, &mut session, &mut context, workspace));
    let ledger = crate::session::title::UsageLedger::default();
    ledger.observe(rig::completion::Usage {
        input_tokens: 100,
        output_tokens: 5,
        total_tokens: 105,
        ..Default::default()
    });
    let (ready, started) = tokio::sync::oneshot::channel();
    let (retired, observed) = tokio::sync::oneshot::channel();
    struct Retired(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Retired {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    let task = tokio::spawn(async move {
        let _retired = Retired(Some(retired));
        let _ = ready.send(());
        std::future::pending::<()>().await;
    });
    app.title_generation = 4;
    app.title_work = Some(SessionTitleWork {
        session_id: app.ui.session.id.clone(),
        generation: 4,
        workspace: app.ui.workspace.clone(),
        provider: "anthropic".into(),
        pricing: crate::session::title::TitlePricing {
            known: true,
            input: 2.0,
            output: 4.0,
            cache: Default::default(),
            anthropic_native: true,
        },
        ledger,
        work_scope: crate::agent::runner::AgentWorkScope::new(),
        task: Some(task),
    });
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    app.ui.session.input_token_cost = 200.0;
    app.retire_session_title().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), observed)
        .await
        .unwrap()
        .unwrap();
    assert!(app.title_work.is_none());
    assert_eq!(app.ui.session.total_input_tokens, 100);
    assert!((app.ui.session.total_cost - 0.00022).abs() < 1e-12);
    app.apply_session_title(SessionTitleResult {
        generation: 4,
        session_id: app.ui.session.id.clone(),
        title: Ok("STALE_TITLE".into()),
    })
    .await
    .unwrap();
    assert!(app.ui.session.generated_title.is_none());
    app.retire_session_title().await.unwrap();
    assert_eq!(app.ui.session.total_input_tokens, 100);
}
