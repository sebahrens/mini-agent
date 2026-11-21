use super::test_support::fixture_app;
use super::*;
use crate::permission::ask::{AskRequest, UserDecision};
use crate::session::MessageRole;
use tokio::sync::oneshot;

/// Test-owned tasks are aborted even when an assertion unwinds. No native
/// processes or network requests are launched by this channel fixture.
struct OwnedRunner(Option<tokio::task::JoinHandle<()>>);
impl Drop for OwnedRunner {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}
impl OwnedRunner {
    async fn joined(mut self) {
        let result = tokio::time::timeout(Duration::from_secs(2), self.0.as_mut().unwrap())
            .await
            .expect("exact fixture runner did not settle after cancellation");
        // Keep the handle in the RAII guard until the await completes, so a
        // deadline/assertion failure still aborts rather than detaches it.
        self.0.take();
        assert!(
            result.unwrap_err().is_cancelled(),
            "the exact owned run must be aborted"
        );
    }
}
struct Retired(Arc<AtomicBool>);
impl Drop for Retired {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn environment(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
    crate::tests::ScopedProcessEnv::set(&[
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
    ])
}
fn fixture_config() -> config::Config {
    serde_json::from_value(serde_json::json!({
        "custom_providers": {"permission-fixture": {"provider_type":"openai", "base_url":"http://127.0.0.1:1", "api_style":"completions"}}
    })).unwrap()
}
fn fixture_ui<'a>(
    cli: &'a crate::cli::Cli,
    cfg: &'a config::Config,
    session: &'a mut Session,
    context: &'a mut crate::context::ContextFiles,
    workspace: Arc<crate::paths::WorkspaceBinding>,
) -> UiContext<'a> {
    UiContext {
        native_startup: None,
        native_ticket: None,
        #[cfg(feature = "hooks")]
        pending_session_start_context: None,
        cli,
        cfg,
        session,
        context,
        workspace,
        client: crate::provider::create_client(
            "permission-fixture",
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
    }
}
fn request(input: &str) -> (AskRequest, oneshot::Receiver<UserDecision>) {
    let (reply, receiver) = oneshot::channel();
    (
        AskRequest {
            workdir: None,
            diagnostic: None,
            #[cfg(feature = "subagents")]
            background: None,
            agent_name: None,
            tool: "shell".into(),
            input: input.into(),
            #[cfg(feature = "acp")]
            tool_call_id: Some(input.into()),
            suggested_pattern: None,
            additional_allow_patterns: Vec::new(),
            reply,
        },
        receiver,
    )
}
fn shown(app: &App<'_>) -> String {
    let lines = app.renderer.feed().line_count(4096);
    app.renderer
        .feed()
        .selected_text(4096, 0, lines.saturating_sub(1))
        .unwrap_or_default()
}
async fn sent<T>(sender: &mpsc::Sender<T>, message: T) {
    let result = tokio::time::timeout(Duration::from_secs(2), sender.send(message))
        .await
        .expect("fixture event channel did not accept a message before its deadline");
    assert!(result.is_ok(), "fixture event receiver closed unexpectedly");
}
async fn replied(receiver: oneshot::Receiver<UserDecision>) -> UserDecision {
    tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .expect("permission decision did not arrive before its deadline")
        .expect("permission decision sender was dropped without a decision")
}
async fn drain(app: &mut App<'_>) {
    // Cancel only this borrowed event-loop wait. The App and its owned run
    // remain live for before-answer assertions and the next polling pass.
    match tokio::time::timeout(Duration::from_millis(100), app.run_inner()).await {
        Err(_) => {}
        Ok(Ok(())) => panic!("fixture App exited before the controlled event loop deadline"),
        Ok(Err(error)) => panic!("fixture App event loop failed: {error:#}"),
    }
}
async fn runner(app: &mut App<'_>) -> (OwnedRunner, Arc<AtomicBool>, mpsc::Sender<AgentEvent>) {
    let (events, receiver) = mpsc::channel(8);
    let keep_events = events.clone();
    let retired = Arc::new(AtomicBool::new(false));
    let observed = retired.clone();
    let (started, ready) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _retired = Retired(observed);
        let _events = keep_events;
        let _ = started.send(());
        std::future::pending::<()>().await;
    });
    let task = OwnedRunner(Some(task));
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .expect("fixture runner did not signal readiness")
        .unwrap();
    app.run.is_running = true;
    app.run.main_abort = Some(task.0.as_ref().unwrap().abort_handle());
    app.run.agent_rx = Some(receiver);
    (task, retired, events)
}

#[tokio::test]
async fn actual_permission_wait_drains_btw_agent_title_logs_and_preserves_paste_before_answer() {
    let root = crate::tests::OwnedTestDirectory::new("permission-background-loop");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    session.total_cost = 0.25;
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    let pending = PendingMainTurn::capture(app.ui.session, "permission-wait prompt");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.run.is_running = true;
    let (task, retired, agent_events) = runner(&mut app).await;
    let (asks, receiver) = mpsc::channel(4);
    app.ask_rx = Some(receiver);
    let (mut ask, mut decision) = request("printf permission-reviewed");
    ask.diagnostic = Some(crate::permission::ask::AskDiagnostic {
        effective_mode: crate::permission::SecurityMode::Standard,
        selected_mode: crate::permission::SecurityMode::Yolo,
        cause: crate::permission::ask::AskCause::ConfiguredAskRule,
    });
    sent(&asks, ask.into()).await;
    drain(&mut app).await;
    assert!(app.permission_prompt.is_some());
    let header = &app.renderer.permission_prompt.as_ref().unwrap().tool;
    assert!(
        header
            .contains("effective mode: standard; selected mode: yolo; cause: configured Ask rule"),
        "{header}"
    );
    assert!(
        header.contains("prompt mode narrowed the selected mode"),
        "{header}"
    );
    let id = app
        .btw_pricing
        .start(crate::print::HeadlessPricing::from_session(
            app.ui.session,
            &cfg,
        ))
        .unwrap();
    app.btw_inflight = 1;
    sent(
        &app.btw_tx,
        BtwEvent::Done {
            id,
            response: "BACKGROUND_BTW_BEFORE_ANSWER".into(),
            input_tokens: 2,
            output_tokens: 3,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
    )
    .await;
    sent(
        &app.title_tx,
        SessionTitleResult {
            generation: app.title_generation,
            session_id: app.ui.session.id.clone(),
            title: Ok("BACKGROUND_TITLE".into()),
        },
    )
    .await;
    sent(
        &agent_events,
        AgentEvent::Token("BACKGROUND_AGENT_BEFORE_ANSWER".into()),
    )
    .await;
    sent(&app.user_tx, UserEvent::Paste("DEFERRED_USER_DRAFT".into())).await;
    drain(&mut app).await;
    let transcript = shown(&app);
    let terminal = app.renderer.captured_output();
    for marker in [
        "BACKGROUND_BTW_BEFORE_ANSWER",
        "BACKGROUND_AGENT_BEFORE_ANSWER",
    ] {
        assert!(transcript.contains(marker), "{marker}: {transcript}");
        assert!(
            terminal.contains(marker),
            "background output must be painted before approval: {marker}"
        );
    }
    assert_eq!(
        app.ui.session.generated_title.as_deref(),
        Some("BACKGROUND_TITLE")
    );
    assert!(matches!(
        decision.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(app.renderer.permission_prompt.is_some());
    assert_eq!(
        app.ui.session.total_cost, 0.25,
        "side question accounting never mutates main cost"
    );
    assert_eq!(
        app.input.buffer, "",
        "paste waits until the approval is answered"
    );
    sent(
        &app.user_tx,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
    )
    .await;
    drain(&mut app).await;
    assert!(matches!(replied(decision).await, UserDecision::Deny));
    assert!(app.run.is_running, "ordinary denial does not stop the run");
    assert!(app.permission_prompt.is_none());
    assert_eq!(app.input.buffer, "DEFERRED_USER_DRAFT");
    drop(agent_events);
    sent(&app.user_tx, UserEvent::Quit).await;
    tokio::time::timeout(Duration::from_secs(2), app.run_inner())
        .await
        .unwrap()
        .unwrap();
    task.joined().await;
    assert!(retired.load(Ordering::SeqCst));
}

#[tokio::test]
async fn actual_permission_stop_retires_exact_run_denies_backlog_and_stages_next_instruction() {
    let root = crate::tests::OwnedTestDirectory::new("permission-stop-loop");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    for (key, modifiers, progress) in [
        (KeyCode::Esc, KeyModifiers::NONE, false),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, false),
        (KeyCode::Char('d'), KeyModifiers::CONTROL, false),
        (KeyCode::Char('s'), KeyModifiers::NONE, false),
        (KeyCode::Esc, KeyModifiers::NONE, true),
        (KeyCode::Char('s'), KeyModifiers::NONE, true),
    ] {
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        session.add_message(MessageRole::User, "earlier user");
        session.add_message(MessageRole::Assistant, "earlier answer");
        session.total_cost = 0.25;
        let mut app = fixture_app(fixture_ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
        ));
        let pending = PendingMainTurn::capture(app.ui.session, "interrupted original prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        if progress {
            app.ui.session.add_tool_call_with_id(
                "permission-progress-read",
                "read",
                &serde_json::json!({"path":"fixture.txt"}),
            );
            app.ui.session.total_cost = 0.5;
        }
        app.input.load_text("recoverable follow-up draft");
        app.run
            .pending_inputs
            .extend(["stale queued first".into(), "stale queued second".into()]);
        let (task, retired, agent_events) = runner(&mut app).await;
        drop(agent_events);
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        let (ask, first) = request("printf current-request");
        sent(&asks, ask.into()).await;
        drain(&mut app).await;
        let (backlog, second) = request("printf stale-request");
        sent(&asks, backlog.into()).await;
        sent(&app.user_tx, UserEvent::Key(KeyEvent::new(key, modifiers))).await;
        drain(&mut app).await;
        assert!(matches!(replied(first).await, UserDecision::Deny));
        assert!(matches!(replied(second).await, UserDecision::Deny));
        task.joined().await;
        assert!(retired.load(Ordering::SeqCst));
        assert!(!app.run.is_running);
        assert!(
            app.run.main_abort.is_none() && app.run.agent_rx.is_none() && app.run.agent.is_none()
        );
        assert!(app.run.pending_inputs.is_empty() && app.run.pending_turn.is_none());
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert_eq!(app.ui.session.total_cost, if progress { 0.5 } else { 0.25 });
        if progress {
            assert!(app.ui.session.messages.iter().any(|message| {
                message.role == MessageRole::ToolResult
                    && message.tool_call_id.as_deref() == Some("permission-progress-read")
                    && message
                        .content
                        .contains(crate::agent::runner::UNKNOWN_TOOL_OUTCOME)
            }));
        } else {
            assert_eq!(app.ui.session.messages.len(), 2);
        }
        assert_eq!(
            app.input.buffer,
            if progress {
                "recoverable follow-up draft"
            } else {
                "interrupted original prompt"
            }
        );
        let transcript = shown(&app);
        assert!(
            transcript.contains("permission denied; run stopped"),
            "{transcript}"
        );
        assert!(
            transcript.contains("dropped 2 queued inputs"),
            "{transcript}"
        );
        // A request sent after the retired generation is never displayed
        // or granted, even if the UI receives a stale allow key afterward.
        if key == KeyCode::Char('s') {
            sent(
                &app.user_tx,
                UserEvent::Paste("Read the file instead".into()),
            )
            .await;
            drain(&mut app).await;
            assert!(!shown(&app).contains("> Read the file instead"));
            sent(
                &app.user_tx,
                UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            )
            .await;
            drain(&mut app).await;
            assert_eq!(app.input.buffer, "Read the file instead");
            assert!(
                app.ui
                    .session
                    .messages
                    .iter()
                    .all(|message| message.content != "Read the file instead")
            );
            assert!(!app.run.is_running && app.run.pending_inputs.is_empty());
            app.input.clear_buffer();
            app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            assert_eq!(
                app.input.buffer,
                if progress {
                    "recoverable follow-up draft"
                } else {
                    "interrupted original prompt"
                }
            );
            if !progress {
                app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT))
                    .await
                    .unwrap();
                assert_eq!(app.input.buffer, "recoverable follow-up draft");
            }
        }
        if key != KeyCode::Char('s') && !progress {
            app.input.clear_buffer();
            app.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            assert_eq!(app.input.buffer, "recoverable follow-up draft");
        }
        let (stale, stale_reply) = request("printf post-retirement-request");
        sent(&asks, stale.into()).await;
        drain(&mut app).await;
        assert!(matches!(replied(stale_reply).await, UserDecision::Deny));
        assert!(app.permission_prompt.is_none() && !app.run.is_running);
        assert!(app.ui.session.permission_allowlist.is_empty());
    }
}

#[tokio::test]
async fn actual_abandoned_permission_cannot_turn_a_late_allow_into_a_session_grant() {
    let root = crate::tests::OwnedTestDirectory::new("permission-abandoned-loop");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    let pending = PendingMainTurn::capture(app.ui.session, "caller-timeout prompt");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.run.is_running = true;
    let (task, retired, agent_events) = runner(&mut app).await;
    drop(agent_events);
    let (asks, receiver) = mpsc::channel(4);
    app.ask_rx = Some(receiver);
    let (ask, decision) = request("printf timed-out-request");
    sent(&asks, ask.into()).await;
    drain(&mut app).await;
    assert!(app.permission_prompt.is_some());
    drop(decision);
    sent(
        &app.user_tx,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
    )
    .await;
    drain(&mut app).await;
    assert!(app.permission_prompt.is_none() && app.renderer.permission_prompt.is_none());
    assert!(app.ui.session.permission_allowlist.is_empty());
    sent(&app.user_tx, UserEvent::Quit).await;
    tokio::time::timeout(Duration::from_secs(2), app.run_inner())
        .await
        .unwrap()
        .unwrap();
    task.joined().await;
    assert!(retired.load(Ordering::SeqCst));
}

// mini-agent-7aog4: `(p)` persists a project rule only under the
// persist_project_approvals opt-in; by default the key is inert.
#[tokio::test]
async fn actual_project_approval_key_is_inert_by_default_and_persists_when_enabled() {
    let root = crate::tests::OwnedTestDirectory::new("permission-project-approval");
    let _environment = environment(&root);
    let workspace_path = root.path().join("workspace");
    std::fs::create_dir_all(&workspace_path).unwrap();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    for enabled in [false, true] {
        let mut cfg = fixture_config();
        cfg.persist_project_approvals = enabled.then_some(true);
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&workspace_path).unwrap());
        let root_path = workspace.root().to_path_buf();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let mut app = fixture_app(fixture_ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
        ));
        let pending = PendingMainTurn::capture(app.ui.session, "project approval");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        let (ask, mut decision) = request("cargo test --lib");
        sent(&asks, ask.into()).await;
        drain(&mut app).await;
        assert_eq!(
            // The unwrapped prompt text: the painted frame hard-wraps at
            // the terminal width and can split the hint across rows.
            app.renderer
                .permission_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.options.contains("(p) always allow ")
                    && prompt.options.contains(" in this project")),
            enabled
        );
        sent(
            &app.user_tx,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
        )
        .await;
        drain(&mut app).await;
        let state = &crate::paths::process_paths_ref().unwrap().state_dir;
        if enabled {
            assert!(matches!(
                replied(decision).await,
                UserDecision::AllowAlways(_)
            ));
            let rules = crate::permission::project_approvals::load(state, &root_path).unwrap();
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].tool, "shell");
        } else {
            assert!(matches!(
                decision.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(
                app.permission_prompt.is_some(),
                "default: (p) is not an answer"
            );
            assert!(
                crate::permission::project_approvals::load(state, &root_path)
                    .unwrap()
                    .is_empty()
            );
            sent(
                &app.user_tx,
                UserEvent::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            )
            .await;
            drain(&mut app).await;
        }
        sent(&app.user_tx, UserEvent::Quit).await;
        tokio::time::timeout(Duration::from_secs(2), app.run_inner())
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn actual_permission_stages_preserve_prompt_folder_and_explicit_session_grants() {
    use crate::permission::checker::{CheckResult, PermissionChecker, PromptGrantOffer};
    let root = crate::tests::OwnedTestDirectory::new("permission-grant-loop");
    let _environment = environment(&root);
    let workspace_path = root.path().join("workspace");
    let external = root.path().join("outside");
    std::fs::create_dir_all(&workspace_path).unwrap();
    std::fs::create_dir_all(&external).unwrap();
    let target = external.join("review.txt");
    std::fs::write(&target, "review fixture").unwrap();
    let path = target.to_string_lossy().into_owned();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    for kind in ["prompt-accept", "prompt-decline", "folder", "session"] {
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&workspace_path).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let mut checker = PermissionChecker::new(
            &crate::permission::PermissionConfigs::default(),
            crate::permission::SecurityMode::Standard,
            Some(workspace_path.clone()),
            None,
        )
        .unwrap();
        if kind.starts_with("prompt-") {
            checker.set_prompt_grant_offer(Some(PromptGrantOffer {
                prompt: "review".into(),
                dir: external.clone(),
                tools: vec!["read", "edit"],
            }));
        }
        let permission = Arc::new(std::sync::Mutex::new(checker));
        let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
        ui.permission = Some(permission.clone());
        let mut app = fixture_app(ui);
        let pending = PendingMainTurn::capture(app.ui.session, "grant review prompt");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        let (mut ask, mut decision) = request(&path);
        ask.tool = "read".into();
        ask.suggested_pattern = Some(path.clone());
        ask.additional_allow_patterns = vec![format!("{}-specific-second", path)];
        sent(&asks, ask.into()).await;
        drain(&mut app).await;
        if kind == "prompt-decline" {
            assert!(
                app.renderer
                    .captured_output()
                    .contains("prompt 'review' asks")
            );
            sent(
                &app.user_tx,
                UserEvent::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            )
            .await;
            drain(&mut app).await;
            assert!(matches!(
                decision.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(
                app.permission_prompt.is_some(),
                "declining the broad prompt grant falls back to per-call approval"
            );
            assert!(app.ui.session.permission_allowlist.is_empty());
            assert!(
                permission
                    .lock()
                    .unwrap()
                    .prompt_grant_offer_for("read", &path)
                    .is_none()
            );
        }
        let answer = match kind {
            "folder" => 'f',
            "session" => 'a',
            _ => 'y',
        };
        sent(
            &app.user_tx,
            UserEvent::Key(KeyEvent::new(KeyCode::Char(answer), KeyModifiers::NONE)),
        )
        .await;
        drain(&mut app).await;
        match (kind, replied(decision).await) {
            ("session", UserDecision::AllowAlways(pattern)) => {
                assert_eq!(pattern, path);
                assert_eq!(app.ui.session.permission_allowlist.len(), 2);
                assert!(
                    app.ui
                        .session
                        .permission_allowlist
                        .iter()
                        .all(|entry| entry.tool == "read")
                );
            }
            ("prompt-decline", UserDecision::AllowOnce) => {
                assert!(app.ui.session.permission_allowlist.is_empty());
                assert_eq!(
                    permission.lock().unwrap().check_path("edit", &path),
                    CheckResult::Ask
                );
            }
            ("prompt-accept" | "folder", UserDecision::AllowOnce) => {
                assert!(!app.ui.session.permission_allowlist.is_empty());
                assert_eq!(
                    permission.lock().unwrap().check_path("edit", &path),
                    CheckResult::Allowed
                );
                assert_eq!(
                    permission.lock().unwrap().check_path("write", &path),
                    CheckResult::Ask
                );
            }
            (_, other) => panic!("unexpected grant for {kind}: {other:?}"),
        }
        assert!(app.permission_prompt.is_none() && app.run.is_running);
        sent(&app.user_tx, UserEvent::Quit).await;
        tokio::time::timeout(Duration::from_secs(2), app.run_inner())
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn keys73_actual_app_routes_remaps_keys_vim_and_retains_raw_approval() {
    let root = crate::tests::OwnedTestDirectory::new("keys73-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let mut cfg = fixture_config();
    cfg.keys = Some(std::collections::BTreeMap::from([
        ("line_start".into(), vec!["f2".into()]),
        ("interrupt".into(), vec!["f3".into()]),
        ("cycle_mode".into(), vec!["f4".into()]),
        ("toggle_vim".into(), vec!["f5".into()]),
        ("show_keys".into(), vec!["f6".into()]),
        ("delete_forward".into(), vec!["ctrl+r".into()]),
    ]));
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    app.input.load_text("雪猫 draft");
    let key = |code, modifiers| UserEvent::Key(KeyEvent::new(code, modifiers));
    let _ = app
        .handle_user_event(key(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(app.input.cursor, app.input.buffer.len());
    let _ = app
        .handle_user_event(key(KeyCode::F(2), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.input.cursor, 0);
    let reasoning = app.renderer.feed().reasoning_display();
    let _ = app
        .handle_user_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(app.input.buffer, "猫 draft");
    assert_eq!(app.renderer.feed().reasoning_display(), reasoning);
    let session_before = serde_json::to_vec(app.ui.session).unwrap();
    let _ = app
        .handle_user_event(key(KeyCode::F(6), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.renderer.captured_output().contains("line_start: f2"));
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), session_before);
    assert!(!app.run.is_running && app.run.agent.is_none());
    let _ = app
        .handle_user_event(key(KeyCode::F(5), KeyModifiers::NONE))
        .await
        .unwrap();
    let _ = app
        .handle_user_event(key(KeyCode::Esc, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.input.vim_label(), "vim:NORMAL");
    assert!(app.renderer.captured_output().contains("vim:NORMAL"));
    let pending = PendingMainTurn::capture(app.ui.session, "active request");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.run.is_running = true;
    let (ask, mut decision) = request("exact requested shell operation");
    app.begin_permission_request(ask).unwrap();
    assert!(app.permission_prompt.is_some());
    let _ = app
        .handle_user_event(key(KeyCode::F(5), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.input.vim_enabled());
    assert!(matches!(
        decision.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(app.ui.session.permission_allowlist.is_empty());
    let _ = app
        .handle_user_event(key(KeyCode::Char('n'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(replied(decision).await, UserDecision::Deny));
    app.run.is_running = false;
    crate::startup::heap_future(|| app.run_slash_command("/vim off"))
        .await
        .unwrap();
    assert!(!app.input.vim_enabled());
    assert!(matches!(
        app.handle_user_event(key(KeyCode::F(3), KeyModifiers::NONE))
            .await
            .unwrap(),
        ControlFlow::Continue(())
    ));
    assert!(app.input.buffer.is_empty());
    assert!(matches!(
        app.handle_user_event(key(KeyCode::F(3), KeyModifiers::NONE))
            .await
            .unwrap(),
        ControlFlow::Break(())
    ));
}

#[tokio::test]
async fn kbv3d_modifier_chords_and_slash_commands_navigate_the_transcript() {
    let root = crate::tests::OwnedTestDirectory::new("kbv3d-navigation");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("nav-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    for i in 0..400 {
        app.renderer
            .write_line(&format!("transcript line {i}"), C_AGENT)
            .unwrap();
    }
    app.renderer.render_viewport().unwrap();
    assert!(!app.renderer.is_scrolling());
    let alt = KeyModifiers::ALT;
    let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
    // Alt+Up pages up and Alt+Down pages back to the live tail.
    for (code, modifiers, scrolling) in [
        (KeyCode::Up, alt, true),
        (KeyCode::Down, alt, false),
        (KeyCode::Up, alt_shift, true),
        (KeyCode::Down, alt_shift, false),
        (KeyCode::Char('<'), alt, true),
        (KeyCode::Char('>'), alt, false),
        // Some terminals also report Shift for the `<` and `>` characters.
        (KeyCode::Char('<'), alt_shift, true),
        (KeyCode::Char('>'), alt_shift, false),
    ] {
        app.input.clear_buffer();
        // Same result with a non-empty draft: the chords are not editor keys.
        for c in "draft".chars() {
            app.input.handle_paste(c.to_string());
        }
        let _ = app
            .handle_user_event(UserEvent::Key(KeyEvent::new(code, modifiers)))
            .await
            .unwrap();
        assert_eq!(
            app.renderer.is_scrolling(),
            scrolling,
            "{code:?} {modifiers:?}"
        );
        assert_eq!(app.input.buffer, "draft", "{code:?} {modifiers:?}");
    }
    // Wheel events: a page notch scrolls further than a line notch.
    let _ = app
        .handle_user_event(UserEvent::ScrollPageUp)
        .await
        .unwrap();
    assert!(app.renderer.is_scrolling());
    let _ = app
        .handle_user_event(UserEvent::ScrollPageDown)
        .await
        .unwrap();
    assert!(!app.renderer.is_scrolling());
    // /top and /bottom work without any navigation key.
    app.input.clear_buffer();
    crate::startup::heap_future(|| app.run_slash_command("/top"))
        .await
        .unwrap();
    assert!(app.renderer.is_scrolling());
    crate::startup::heap_future(|| app.run_slash_command("/bottom"))
        .await
        .unwrap();
    assert!(!app.renderer.is_scrolling());
    crate::startup::heap_future(|| app.run_slash_command("/top extra"))
        .await
        .unwrap();
    assert!(!app.renderer.is_scrolling());
    assert!(app.renderer.captured_output().contains("usage: /top"));
}

#[tokio::test]
async fn keys73_actual_app_picker_fallback_retains_raw_keys_after_close() {
    let root = crate::tests::OwnedTestDirectory::new("keys73-picker-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let mut cfg = fixture_config();
    cfg.vim_mode = Some(true);
    cfg.keys = Some(std::collections::BTreeMap::from([
        ("line_start".into(), vec!["f2".into()]),
        ("undo".into(), vec!["f3".into()]),
        ("submit".into(), vec!["f4".into()]),
    ]));
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("picker-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    let key = |code, modifiers| UserEvent::Key(KeyEvent::new(code, modifiers));
    for query in ["/mo", "@read"] {
        app.input.clear_buffer();
        for c in query.chars() {
            let _ = app
                .handle_user_event(key(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert!(app.input.picker.as_ref().is_some_and(Picker::active));
        let cursor = app.input.cursor;
        for code in [KeyCode::F(2), KeyCode::F(3), KeyCode::F(4)] {
            let _ = app
                .handle_user_event(key(code, KeyModifiers::NONE))
                .await
                .unwrap();
            assert_eq!(app.input.buffer, query);
            assert_eq!(app.input.cursor, cursor);
            assert!(app.input.picker.as_ref().is_some_and(Picker::active));
            assert!(!app.run.is_running);
        }
        // Raw caret editing closes the picker, then keeps the raw key
        // for that same event despite the configured replacement.
        let _ = app
            .handle_user_event(key(KeyCode::Home, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(!app.input.picker.as_ref().is_some_and(Picker::active));
        assert_eq!(app.input.cursor, 0);
        app.input.set_cursor(app.input.buffer.len());
        let _ = app
            .handle_user_event(key(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.input.cursor, app.input.buffer.len());
        let _ = app
            .handle_user_event(key(KeyCode::F(2), KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.input.cursor, 0);
    }
    app.input.clear_buffer();
    for c in "/help".chars() {
        let _ = app
            .handle_user_event(key(KeyCode::Char(c), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert!(app.input.picker.as_ref().is_some_and(Picker::active));
    let _ = app
        .handle_user_event(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(app.input.buffer.is_empty());
    assert!(!app.input.picker.as_ref().is_some_and(Picker::active));
    assert!(!app.run.is_running);
    // /help paints its listing as one frame, so read the transcript
    // rather than the rows that frame happened to repaint.
    assert!(app.renderer.transcript_text().contains("/keys"));
}

#[tokio::test]
async fn fork75_actual_app_creates_truncated_branch_and_keeps_original_file() {
    let root = crate::tests::OwnedTestDirectory::new("fork75-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "fork test");
    #[cfg(feature = "goal")]
    {
        let mut goal = crate::extras::goal::Goal::new("fork waits for input", Vec::new()).unwrap();
        goal.context_file = Some(root.path().join("UNREAD_FORK_PLAN.md"));
        goal.autostart = true;
        session.goal_store.set(goal, false).unwrap();
    }
    session.working_dir = workspace.root().to_string_lossy().into();
    for (role, text) in [
        (MessageRole::User, "first"),
        (MessageRole::Assistant, "first answer"),
        (MessageRole::User, "second"),
        (MessageRole::Assistant, "second answer"),
    ] {
        session.add_message(role, text);
    }
    crate::session::storage::save_session(&session).unwrap();
    let original_id = session.id.clone();
    let original_path = crate::paths::process_paths_ref()
        .unwrap()
        .sessions_dir()
        .join(format!("{original_id}.json"));
    let original_bytes = std::fs::read(&original_path).unwrap();
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    crate::startup::heap_future(|| app.run_slash_command("/fork 2"))
        .await
        .unwrap();
    assert_ne!(app.ui.session.id, original_id);
    assert_eq!(app.ui.session.name, "fork test (fork)");
    assert!(!app.run.is_running);
    assert!(app.run.agent_rx.is_none());
    assert_eq!(app.ui.session.messages.len(), 3);
    assert_eq!(app.ui.session.messages[2].content, "second");
    assert!(app.run.agent.is_some());
    let fork_id = app.ui.session.id.clone();
    let restored = crate::session::storage::load_session_exact(&fork_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.messages.len(), 3);
    assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
    for invalid in ["/fork 0", "/fork 99", "/fork 2 extra"] {
        crate::startup::heap_future(|| app.run_slash_command(invalid))
            .await
            .unwrap();
        assert_eq!(app.ui.session.id, fork_id);
        assert!(!app.run.is_running);
        assert!(app.run.agent_rx.is_none());
    }
    app.run.is_running = true;
    crate::startup::heap_future(|| app.run_slash_command("/fork"))
        .await
        .unwrap();
    assert_eq!(app.ui.session.id, fork_id);
    assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
    app.run.is_running = false;
    drop(app);
    crate::session::lock::release_session(&original_id);
    crate::session::lock::release_session(&fork_id);
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn clipboard74_actual_app_stages_drops_and_restores_typed_draft_without_modal_reads() {
    use super::super::clipboard_image::{self, ClipboardImage, ClipboardImageReader};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct FakeClipboard(AtomicUsize);
    impl ClipboardImageReader for FakeClipboard {
        fn read<'a>(
            &'a self,
            _sandbox: &'a crate::sandbox::Sandbox,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = io::Result<Option<ClipboardImage>>> + Send + 'a>,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { clipboard_image::decode_png(clipboard_image::tests::png()).map(Some) })
        }
    }
    let root = crate::tests::OwnedTestDirectory::new("clipboard74-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let mut cfg = fixture_config();
    cfg.max_budget_usd = Some(0.0);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    app.input.buffer = "literal 雪".into();
    app.input.cursor = app.input.buffer.len();
    let reader = FakeClipboard(AtomicUsize::new(0));
    assert!(
        crate::startup::heap_future(|| app.paste_clipboard_image(&reader))
            .await
            .unwrap()
    );
    assert_eq!(reader.0.load(Ordering::SeqCst), 1);
    assert_eq!(app.input.buffer, "literal 雪");
    assert_eq!(app.input.cursor, "literal 雪".len());
    assert_eq!(app.ui.session.pending_media.len(), 1);
    assert!(app.ui.session.messages.is_empty());
    assert!(!app.run.is_running);
    app.refresh().unwrap();
    assert!(
        app.renderer
            .captured_output()
            .contains("[image 1: 1x1 png]")
    );
    crate::startup::heap_future(|| app.run_slash_command("/drop 1"))
        .await
        .unwrap();
    assert!(app.ui.session.pending_media.is_empty());
    assert!(clipboard_image::labels(&app.ui.session.pending_media).is_empty());
    assert!(
        crate::startup::heap_future(|| app.paste_clipboard_image(&reader))
            .await
            .unwrap()
    );

    std::fs::write(root.path().join("scan.png"), clipboard_image::tests::png()).unwrap();
    crate::startup::heap_future(|| app.run_slash_command("/add scan.png"))
        .await
        .unwrap();
    assert_eq!(app.ui.session.pending_media.len(), 2);
    let user =
        clipboard_image::user_message(&app.ui.session.pending_media, &app.input.buffer).unwrap();
    assert_eq!(user.as_ref().unwrap().blocks().len(), 3);
    let mut pending = PendingMainTurn::capture(app.ui.session, &app.input.buffer);
    pending.set_user(user.clone());
    pending.take_pending_media(app.ui.session);
    assert!(app.ui.session.pending_media.is_empty());
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    assert_eq!(app.ui.session.messages.len(), 1);
    assert_eq!(app.ui.session.messages[0].user, user);
    assert!(
        !app.run
            .pending_turn
            .as_ref()
            .unwrap()
            .has_progress(app.ui.session, "", &[])
    );
    assert_eq!(
        crate::ui::rollback_pending_main_turn(&mut app.run, app.ui.session).unwrap(),
        "literal 雪"
    );
    assert_eq!(app.ui.session.pending_media.len(), 2);
    // The subsequent image-only admission exercises a /add file image alone.
    app.ui.session.pending_media.remove(0);
    assert!(matches!(
        app.ui.session.pending_media[0],
        crate::extras::multimodal::MediaAttachment::Image {
            clipboard: None,
            ..
        }
    ));
    assert!(app.ui.session.messages.is_empty());
    assert_eq!(app.input.buffer, "literal 雪");
    let reads = reader.0.load(Ordering::SeqCst);
    app.run.is_running = true;
    assert!(!app.paste_clipboard_image(&reader).await.unwrap());
    app.run.is_running = false;
    app.input.start_command_picker();
    assert!(!app.paste_clipboard_image(&reader).await.unwrap());
    app.input.picker = None;
    app.renderer.chain_prompt = Some(ChainPrompt::new("operator approval"));
    assert!(!app.paste_clipboard_image(&reader).await.unwrap());
    assert_eq!(reader.0.load(Ordering::SeqCst), reads);
    assert!(app.run.agent_rx.is_none());
    app.renderer.chain_prompt = None;
    let too_long = "x".repeat(crate::session::user::MAX_USER_TEXT_BYTES + 1);
    crate::startup::heap_future(|| {
        app.start_main_run_prepared(&too_long, crate::question::QuestionOrigin::NewUserTurn)
    })
    .await
    .unwrap();
    assert_eq!(app.input.buffer.as_str(), too_long);
    assert_eq!(app.ui.session.pending_media.len(), 1);
    assert!(app.ui.session.messages.is_empty());
    assert!(!app.run.is_running);
    assert!(app.run.agent_rx.is_none());
    app.input.clear_buffer();
    let before = app.renderer.captured_output();
    crate::startup::heap_future(|| {
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    })
    .await
    .unwrap();
    let after = app.renderer.captured_output();
    assert!(
        after[before.len()..].contains("USD budget reached"),
        "image-only Enter must enter ordinary budget admission"
    );
    assert_eq!(app.ui.session.pending_media.len(), 1);
    assert!(app.ui.session.messages.is_empty());
    assert!(!app.run.is_running);
    assert!(app.run.agent_rx.is_none());
}

// mini-agent-sskdu.6: a user submit refused at the cap says why every time,
// not only the first time (the once-per-session latch is for automatic starts).
#[tokio::test]
async fn every_refused_submit_at_the_budget_cap_prints_a_notice() {
    let root = crate::tests::OwnedTestDirectory::new("budget-notice-twice-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let mut cfg = fixture_config();
    cfg.max_budget_usd = Some(0.0);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    for _ in 0..2 {
        app.input.buffer = "hello".into();
        app.input.cursor = app.input.buffer.len();
        crate::startup::heap_future(|| {
            app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        })
        .await
        .unwrap();
    }
    let output = app.renderer.captured_output();
    assert_eq!(output.matches("USD budget reached").count(), 2, "{output}");
    assert!(!app.run.is_running);
}

#[tokio::test]
async fn actual_mode_cycle_preserves_drafts_queue_and_unanswered_permission() {
    use crate::permission::{SecurityMode, checker::PermissionChecker};
    let root = crate::tests::OwnedTestDirectory::new("mode-cycle-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let permission = Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &crate::permission::PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(root.path().into()),
            None,
        )
        .unwrap(),
    ));
    let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
    ui.permission = Some(permission.clone());
    let mut app = fixture_app(ui);
    app.input.buffer = "typed follow-up".into();
    app.input.cursor = app.input.buffer.len();
    let cycle = || UserEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    for mode in [
        SecurityMode::Guarded,
        SecurityMode::Yolo,
        SecurityMode::Standard,
    ] {
        let _ = app.handle_user_event(cycle()).await.unwrap();
        assert_eq!(permission.lock().unwrap().mode(), mode);
        assert_eq!(
            app.renderer.notice_text(),
            Some(format!("security mode: {mode}").as_str())
        );
        assert!(
            app.renderer
                .captured_output()
                .contains(&format!("mode:{mode}"))
        );
    }
    let mut empty_picker = crate::ui::pickers::list::ListPicker::new();
    empty_picker.activate();
    app.input.picker = Some(crate::ui::input::Picker::Bang(empty_picker));
    let _ = app.handle_user_event(cycle()).await.unwrap();
    assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Standard);
    app.input.picker = None;
    app.input.start_command_picker();
    if let Some(crate::ui::input::Picker::Command(picker)) = app.input.picker.as_mut() {
        picker.selected = 1;
    }
    app.handle_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        .await
        .unwrap();
    if let Some(crate::ui::input::Picker::Command(picker)) = app.input.picker.as_ref() {
        assert_eq!(picker.selected, 0);
    } else {
        panic!("picker was closed by mode shortcut");
    }
    assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Standard);
    app.input.picker = None;
    let pending = PendingMainTurn::capture(app.ui.session, "original active prompt");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.run.is_running = true;
    app.run.pending_inputs.push_back("queued follow-up".into());
    let (ask, mut decision) = request("pending exact shell request");
    app.begin_permission_request(ask).unwrap();
    let _ = app.handle_user_event(cycle()).await.unwrap();
    assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Guarded);
    assert!(matches!(
        decision.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(app.permission_prompt.is_some());
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert_eq!(app.input.buffer, "typed follow-up");
    assert_eq!(app.run.pending_inputs.len(), 1);
    let modified = UserEvent::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    let _ = app.handle_user_event(modified).await.unwrap();
    assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Guarded);
    permission
        .lock()
        .unwrap()
        .set_prompt_mode(SecurityMode::ReadOnly);
    permission.lock().unwrap().restore_user_mode();
    assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Guarded);
    let _ = app
        .handle_user_event(UserEvent::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
        )))
        .await
        .unwrap();
    assert!(matches!(replied(decision).await, UserDecision::Deny));
}

#[tokio::test]
async fn actual_mode_cycle_uses_custom_order_and_enters_ring_from_other_modes() {
    use crate::permission::{SecurityMode, checker::PermissionChecker};
    let root = crate::tests::OwnedTestDirectory::new("mode-cycle-custom-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    for (ring, current, expected) in [
        (None, SecurityMode::ReadOnly, vec![SecurityMode::Standard]),
        (
            Some(vec!["readonly", "planwrite", "guarded"]),
            SecurityMode::Standard,
            vec![
                SecurityMode::ReadOnly,
                SecurityMode::PlanWrite,
                SecurityMode::Guarded,
                SecurityMode::ReadOnly,
            ],
        ),
        (
            Some(vec!["readonly"]),
            SecurityMode::Guarded,
            vec![SecurityMode::ReadOnly, SecurityMode::ReadOnly],
        ),
    ] {
        let mut cfg = fixture_config();
        cfg.permission_mode_cycle = ring.map(|ring| ring.into_iter().map(str::to_owned).collect());
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let permission = Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                current,
                Some(root.path().into()),
                None,
            )
            .unwrap(),
        ));
        let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
        ui.permission = Some(permission.clone());
        let mut app = fixture_app(ui);
        for mode in expected {
            let _ = app
                .handle_user_event(UserEvent::Key(KeyEvent::new(
                    KeyCode::BackTab,
                    KeyModifiers::NONE,
                )))
                .await
                .unwrap();
            assert_eq!(permission.lock().unwrap().mode(), mode);
            permission.lock().unwrap().restore_user_mode();
            assert_eq!(permission.lock().unwrap().mode(), mode);
            assert_eq!(
                app.renderer.notice_text(),
                Some(format!("security mode: {mode}").as_str())
            );
        }
    }
}

#[tokio::test]
async fn actual_workspace_edit_answer_grants_only_native_mutations_and_rejects_stale_requests() {
    use crate::permission::{
        SecurityMode,
        checker::{CheckResult, PermissionChecker, PromptGrantOffer},
    };
    let root = crate::tests::OwnedTestDirectory::new("workspace-edit-answer-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    for kind in [
        "write",
        "edit",
        "read",
        "external",
        "stale",
        "retired",
        "prompt-offer",
    ] {
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
        let target = workspace
            .root()
            .join("new.txt")
            .to_string_lossy()
            .into_owned();
        let mut checker = PermissionChecker::new(
            &crate::permission::PermissionConfigs::default(),
            SecurityMode::Restrictive,
            Some(root.path().into()),
            None,
        )
        .unwrap();
        if kind == "prompt-offer" {
            checker.set_prompt_grant_offer(Some(PromptGrantOffer {
                prompt: "review".into(),
                dir: workspace.root().into(),
                tools: vec!["read", "edit"],
            }));
        }
        let permission = Arc::new(std::sync::Mutex::new(checker));
        let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
        ui.permission = Some(permission.clone());
        let mut app = fixture_app(ui);
        let pending = PendingMainTurn::capture(app.ui.session, "grant mutation request");
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
        app.run.is_running = true;
        let outside = root
            .path()
            .parent()
            .unwrap()
            .join("ungranted-neighbor.txt")
            .to_string_lossy()
            .into_owned();
        let (mut ask, mut decision) = request(if kind == "external" {
            &outside
        } else {
            &target
        });
        ask.tool = if kind == "read" {
            "read"
        } else if kind == "write" {
            "write"
        } else {
            "edit"
        }
        .into();
        let (asks, receiver) = mpsc::channel(4);
        app.ask_rx = Some(receiver);
        sent(&asks, ask.into()).await;
        drain(&mut app).await;
        let eligible = !matches!(kind, "read" | "external");
        assert_eq!(
            app.renderer
                .captured_output()
                .contains("(w) allow writes/edits"),
            eligible
        );
        if kind == "stale" {
            drop(decision);
            let _ = app
                .handle_user_event(UserEvent::Key(KeyEvent::new(
                    KeyCode::Char('w'),
                    KeyModifiers::NONE,
                )))
                .await
                .unwrap();
            assert!(app.ui.session.permission_allowlist.is_empty());
            continue;
        }
        if kind == "retired" {
            app.run.is_running = false;
        }
        sent(
            &app.user_tx,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
        )
        .await;
        drain(&mut app).await;
        if !eligible {
            assert!(matches!(
                decision.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(app.ui.session.permission_allowlist.is_empty());
            let _ = app
                .handle_user_event(UserEvent::Key(KeyEvent::new(
                    KeyCode::Char('n'),
                    KeyModifiers::NONE,
                )))
                .await
                .unwrap();
            assert!(matches!(replied(decision).await, UserDecision::Deny));
        } else if kind == "retired" {
            assert!(matches!(replied(decision).await, UserDecision::Deny));
            assert!(app.ui.session.permission_allowlist.is_empty());
        } else {
            assert!(matches!(replied(decision).await, UserDecision::AllowOnce));
            assert_eq!(app.ui.session.permission_allowlist.len(), 4);
            assert!(
                app.ui
                    .session
                    .permission_allowlist
                    .iter()
                    .all(|entry| matches!(entry.tool.as_str(), "write" | "edit"))
            );
            for tool in ["write", "edit"] {
                assert_eq!(
                    permission.lock().unwrap().check_path(tool, &target),
                    CheckResult::Allowed
                );
            }
            for tool in ["read", "js/write_file"] {
                assert_eq!(
                    permission.lock().unwrap().check_path(tool, &target),
                    CheckResult::Ask
                );
            }
            assert_eq!(permission.lock().unwrap().mode(), SecurityMode::Restrictive);
            if kind == "prompt-offer" {
                assert!(
                    permission
                        .lock()
                        .unwrap()
                        .prompt_grant_offer_for("read", &target)
                        .is_some()
                );
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn actual_workspace_edit_answer_rejects_binding_replaced_while_prompt_waits() {
    use crate::permission::{
        SecurityMode,
        checker::{CheckResult, PermissionChecker},
    };
    let root = crate::tests::OwnedTestDirectory::new("workspace-edit-replaced-app");
    let _environment = environment(&root);
    let directory = root.path().join("workspace");
    std::fs::create_dir(&directory).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&directory).unwrap());
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let permission = Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &crate::permission::PermissionConfigs::default(),
            SecurityMode::Restrictive,
            Some(directory.clone()),
            None,
        )
        .unwrap(),
    ));
    let target = workspace
        .root()
        .join("new.txt")
        .to_string_lossy()
        .into_owned();
    let mut ui = fixture_ui(&cli, &cfg, &mut session, &mut context, workspace);
    ui.permission = Some(permission.clone());
    let mut app = fixture_app(ui);
    let pending = PendingMainTurn::capture(app.ui.session, "approve retained workspace");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.run.is_running = true;
    let (mut ask, decision) = request(&target);
    ask.tool = "write".into();
    app.begin_permission_request(ask).unwrap();
    assert!(
        app.renderer
            .captured_output()
            .contains("(w) allow writes/edits")
    );
    std::fs::rename(&directory, root.path().join("retained")).unwrap();
    std::fs::create_dir(&directory).unwrap();
    let _ = app
        .handle_user_event(UserEvent::Key(KeyEvent::new(
            KeyCode::Char('w'),
            KeyModifiers::NONE,
        )))
        .await
        .unwrap();
    assert!(matches!(replied(decision).await, UserDecision::Deny));
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert_eq!(
        permission.lock().unwrap().check_path("write", &target),
        CheckResult::Ask
    );
    assert!(!directory.join("new.txt").exists());
    assert!(app.permission_prompt.is_none());
}

#[tokio::test]
async fn actual_terminal_error_denies_displayed_permission_and_backlog_before_settlement() {
    let root = crate::tests::OwnedTestDirectory::new("permission-error-loop");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    let pending = PendingMainTurn::capture(app.ui.session, "failed permission prompt");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.run.is_running = true;
    app.run.pending_inputs.push_back("stale error queue".into());
    let (asks, receiver) = mpsc::channel(4);
    app.ask_rx = Some(receiver);
    let (first, first_reply) = request("printf error-first");
    sent(&asks, first.into()).await;
    drain(&mut app).await;
    let (second, second_reply) = request("printf error-second");
    sent(&asks, second.into()).await;
    let (events, receiver) = mpsc::channel(2);
    app.run.agent_rx = Some(receiver);
    sent(
        &events,
        AgentEvent::Error {
            message: "permission owner failed".into(),
            interactions: Vec::new(),
        },
    )
    .await;
    drop(events); // Real terminal event is followed by owned runner closure.
    drain(&mut app).await;
    assert!(matches!(replied(first_reply).await, UserDecision::Deny));
    assert!(matches!(replied(second_reply).await, UserDecision::Deny));
    assert!(app.permission_prompt.is_none() && app.renderer.permission_prompt.is_none());
    assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.pending_turn.is_none());
    assert!(app.run.pending_inputs.is_empty() && app.ui.session.permission_allowlist.is_empty());
    assert_eq!(app.input.buffer, "failed permission prompt");
    assert!(shown(&app).contains("permission owner failed"));
}

#[tokio::test(start_paused = true)]
async fn actual_permission_stop_timeout_retains_cleanup_and_blocks_next_instruction() {
    let root = crate::tests::OwnedTestDirectory::new("permission-retirement-timeout");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    let pending = PendingMainTurn::capture(app.ui.session, "unsettled original prompt");
    crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    app.run.is_running = true;
    app.run
        .pending_inputs
        .push_back("must not restart unsettled work".into());
    let (task, retired, retained_events) = runner(&mut app).await;
    let (asks, receiver) = mpsc::channel(4);
    app.ask_rx = Some(receiver);
    let (ask, decision) = request("printf unsettled-request");
    sent(&asks, ask.into()).await;
    drain(&mut app).await;
    assert!(app.permission_prompt.is_some());
    sent(
        &app.user_tx,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
    )
    .await;
    // The test holds one exact-run sender beyond cancellation. Advancing
    // Tokio time deterministically triggers the real retirement timeout.
    let error = tokio::time::timeout(Duration::from_secs(20), app.run())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("permission stop cleanup is still pending")
    );
    assert!(matches!(replied(decision).await, UserDecision::Deny));
    assert!(
        app.run.agent_rx.is_some(),
        "teardown must retain the exact unsettled receiver"
    );
    assert!(app.run.pending_inputs.is_empty() && app.run.pending_turn.is_none());
    assert!(!app.run.is_running && app.run.agent.is_none());
    assert!(app.permission_prompt.is_none() && app.renderer.permission_prompt.is_none());
    assert_eq!(app.input.buffer, "unsettled original prompt");
    assert!(
        !app.renderer
            .captured_output()
            .contains("Next user instruction (draft)")
    );
    task.joined().await;
    assert!(retired.load(Ordering::SeqCst));
    drop(retained_events); // Release the controlled cleanup obstruction.
    tokio::time::timeout(Duration::from_secs(2), app.teardown())
        .await
        .unwrap();
}

fn key(c: char) -> UserEvent {
    UserEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

/// The queued user events of a burst are applied in order with one frame
/// painted for the whole batch, ending in the sequential state.
#[tokio::test]
async fn queued_scrolls_and_keys_are_applied_in_order_with_one_frame() {
    let root = crate::tests::OwnedTestDirectory::new("user-event-batch");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut batched_context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut sequential_context = batched_context.clone();
    let mut batched_session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut sequential_session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut batched = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut batched_session,
        &mut batched_context,
        workspace.clone(),
    ));
    let mut sequential = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut sequential_session,
        &mut sequential_context,
        workspace,
    ));
    let events = || {
        let mut events = vec![UserEvent::ScrollUp; 10];
        events.extend("hello batched typing".chars().map(key));
        events.push(UserEvent::ScrollDown);
        events
    };
    for app in [&mut batched, &mut sequential] {
        for line in 0..200 {
            app.renderer
                .write_line(&format!("transcript line {line}"), Color::White)
                .unwrap();
        }
        app.refresh().unwrap();
    }
    // Let the startup Git refresh land first: it may paint its own frame.
    drain(&mut batched).await;
    let (tx, rx) = mpsc::channel(64);
    batched.user_tx = tx.clone();
    batched.user_rx = rx;
    for event in events() {
        tx.try_send(event).unwrap();
    }
    let before = batched.loop_state.frames;
    drain(&mut batched).await;
    assert_eq!(
        batched.loop_state.frames - before,
        1,
        "one frame for the whole queued burst"
    );
    let before = sequential.loop_state.frames;
    for event in events() {
        assert!(
            sequential
                .handle_user_event(event)
                .await
                .unwrap()
                .is_continue()
        );
    }
    assert!(sequential.loop_state.frames - before >= 31);
    assert_eq!(batched.input.buffer, "hello batched typing");
    assert_eq!(batched.input.buffer, sequential.input.buffer);
    assert_eq!(batched.input.cursor, sequential.input.cursor);
    assert!(batched.renderer.is_scrolling());
    assert_eq!(
        batched.renderer.is_scrolling(),
        sequential.renderer.is_scrolling()
    );
    assert_eq!(
        batched.renderer.buffer_line_at_row(0),
        sequential.renderer.buffer_line_at_row(0)
    );
    // Consecutive drags collapse to the last pointer position, and a
    // submit stops the batch so it is handled (and painted) on its own.
    batched.renderer.selection_active = true;
    batched.renderer.selection_start = Some(SelectionPoint::new(0, 0));
    for col in 1..=12 {
        tx.try_send(UserEvent::MouseDrag { row: 2, col }).unwrap();
    }
    tx.try_send(UserEvent::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    let before = batched.loop_state.frames;
    drain(&mut batched).await;
    assert_eq!(batched.loop_state.frames - before, 2);
    let idx = batched.renderer.buffer_line_at_row(2).unwrap();
    assert_eq!(
        batched.renderer.selection_end,
        Some(SelectionPoint::new(idx, batched.renderer.chat_text_col(12)))
    );
    assert!(
        batched.input.buffer.is_empty(),
        "the kill key still applied"
    );
}

/// Streaming one-line tokens faster than the stream interval repaints by
/// time, not per token, and the throttled tail is still painted promptly
/// by the event loop's persistent running tick.
#[tokio::test]
async fn streamed_one_line_tokens_repaint_at_the_stream_interval() {
    let root = crate::tests::OwnedTestDirectory::new("stream-repaint-interval");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    app.refresh().unwrap();
    let started = std::time::Instant::now();
    let mut paints = 0usize;
    for line in 0..500 {
        app.handle_agent_event(AgentEvent::Token(format!("line {line}\n").into()))
            .await
            .unwrap();
        if !app.renderer.chat_needs_redraw() {
            paints += 1;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let elapsed = started.elapsed();
    let bound = (elapsed.as_millis()
        / crate::ui::event_handler::STREAM_REPAINT_INTERVAL.as_millis()) as usize
        + 2;
    assert!(
        paints <= bound,
        "{paints} paints for 500 tokens in {elapsed:?} (bound {bound})"
    );
    assert!(paints >= 1, "the first token paints at once");
    // A token skipped by the throttle is painted by the running tick
    // within one stream interval, without any further event. Pin the
    // last paint to now so a loaded host cannot let the interval elapse
    // before the tail token arrives.
    app.run.last_stream_paint = Some(std::time::Instant::now());
    app.handle_agent_event(AgentEvent::Token("tail\n".into()))
        .await
        .unwrap();
    assert!(app.renderer.chat_needs_redraw());
    app.run.is_running = true;
    drain(&mut app).await;
    assert!(!app.renderer.chat_needs_redraw(), "the tail was painted");
    app.run.is_running = false;
}

/// A submit paints the spinner and Working activity before the turn's
/// startup work (memory refresh, prebuild, agent build) is awaited.
#[tokio::test]
async fn submission_paints_the_spinner_before_the_run_starts() {
    let root = crate::tests::OwnedTestDirectory::new("submission-spinner");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    app.refresh().unwrap();
    app.echo_main_submission("spinner prompt").unwrap();
    let before = app.renderer.captured_output().len();
    app.paint_submission_started().unwrap();
    let frame = &app.renderer.captured_output()[before..];
    assert!(
        ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            .iter()
            .any(|glyph| frame.contains(glyph)),
        "{frame:?}"
    );
    assert!(!app.run.is_running, "only the frame is drawn as running");
    assert!(app.run.agent.is_none() && app.run.agent_rx.is_none());
}

/// Git status is refreshed on an adaptive schedule: at most one dispatch
/// per interval while idle, and one right after a tool result.
#[tokio::test]
async fn git_refresh_dispatches_once_per_interval_and_after_a_tool_result() {
    let root = crate::tests::OwnedTestDirectory::new("git-poll-loop");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    // The startup refresh, then many idle loop iterations.
    drain(&mut app).await;
    let (tx, rx) = mpsc::channel(64);
    app.user_tx = tx.clone();
    app.user_rx = rx;
    for _ in 0..20 {
        tx.try_send(UserEvent::Resize).unwrap();
    }
    drain(&mut app).await;
    assert_eq!(app.loop_state.git_poll.dispatches, 1);
    // Pretend that dispatch happened a while ago: a tool result now
    // refreshes immediately instead of waiting for the interval.
    app.loop_state.git_poll = Default::default();
    app.loop_state
        .git_poll
        .start(std::time::Instant::now() - Duration::from_secs(2));
    let (_task, _retired, agent_events) = runner(&mut app).await;
    sent(
        &agent_events,
        AgentEvent::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "true"}),
        },
    )
    .await;
    drain(&mut app).await;
    assert_eq!(
        app.loop_state.git_poll.dispatches, 1,
        "a call alone is not a change"
    );
    sent(
        &agent_events,
        AgentEvent::ToolResult {
            id: "call-0".into(),
            name: "read".into(),
            output: "ok".into(),
            images: Vec::new(),
            is_error: false,
        },
    )
    .await;
    drain(&mut app).await;
    assert_eq!(
        app.loop_state.git_poll.dispatches, 1,
        "a read-only tool result cannot change the worktree"
    );
    sent(
        &agent_events,
        AgentEvent::ToolResult {
            id: "call-1".into(),
            name: "bash".into(),
            output: "ok".into(),
            images: Vec::new(),
            is_error: false,
        },
    )
    .await;
    drain(&mut app).await;
    assert_eq!(app.loop_state.git_poll.dispatches, 2);
    app.run.is_running = false;
    if let Some(abort) = app.run.main_abort.take() {
        abort.abort();
    }
}

/// Prints the budget of the per-keystroke boxed futures (run with
/// `--nocapture`); the boxes are required by the 1 MiB-stack tests.
#[tokio::test]
async fn per_event_future_sizes_are_visible() {
    let root = crate::tests::OwnedTestDirectory::new("future-sizes");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    let event = UserEvent::Key(key);
    let batch = app.handle_user_event_batch(UserEvent::Resize);
    let batch_size = std::mem::size_of_val(&batch);
    drop(batch);
    let plan = app.handle_plan_event(&event);
    let plan_size = std::mem::size_of_val(&plan);
    drop(plan);
    let standard = app.handle_standard_user_event(UserEvent::Resize);
    let standard_size = std::mem::size_of_val(&standard);
    drop(standard);
    let key_future = app.handle_key_event_with(key, None);
    let key_size = std::mem::size_of_val(&key_future);
    drop(key_future);
    eprintln!(
        "future sizes: batch={batch_size} plan={plan_size} standard={standard_size} key={key_size}"
    );
    assert!(batch_size > 0 && plan_size > 0 && standard_size > 0 && key_size > 0);
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn nvnkx255_ctrl_v_without_an_image_pastes_text_or_says_nothing_to_paste() {
    use super::super::clipboard_image::{ClipboardImage, ClipboardImageReader};
    struct NoImage;
    impl ClipboardImageReader for NoImage {
        fn read<'a>(
            &'a self,
            _sandbox: &'a crate::sandbox::Sandbox,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = io::Result<Option<ClipboardImage>>> + Send + 'a>,
        > {
            Box::pin(async { Ok(None) })
        }
    }
    let root = crate::tests::OwnedTestDirectory::new("nvnkx255-app");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    // Text on the clipboard is pasted into the draft.
    crate::startup::heap_future(|| {
        app.paste_clipboard_key(&NoImage, || async { Ok("hello".to_owned()) })
    })
    .await
    .unwrap();
    assert_eq!(app.input.buffer, "hello");
    assert!(app.ui.session.pending_media.is_empty());
    // An empty clipboard is a notice, not an error line.
    crate::startup::heap_future(|| {
        app.paste_clipboard_key(&NoImage, || async { Ok(String::new()) })
    })
    .await
    .unwrap();
    assert_eq!(app.input.buffer, "hello");
    app.refresh().unwrap();
    let shown = app.renderer.captured_output();
    assert!(shown.contains("nothing to paste"), "{shown}");
    assert!(!shown.contains("unavailable on this platform"));
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn nvnkx256_pasted_image_path_attaches_the_image_and_other_pastes_stay_text() {
    use super::super::clipboard_image;
    let root = crate::tests::OwnedTestDirectory::new("nvnkx256-app");
    let outside = crate::tests::OwnedTestDirectory::new("nvnkx256-outside");
    let _environment = environment(&root);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = fixture_config();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("permission-fixture", "test-model", 128_000, "");
    let mut app = fixture_app(fixture_ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
    ));
    let png = root.path().join("shot.png");
    std::fs::write(&png, clipboard_image::tests::png()).unwrap();
    std::fs::write(root.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(
        outside.path().join("far.png"),
        clipboard_image::tests::png(),
    )
    .unwrap();

    macro_rules! paste {
        ($app:expr, $text:expr) => {{
            let _ = crate::startup::heap_future(|| $app.handle_user_event(UserEvent::Paste($text)))
                .await
                .unwrap();
        }};
    }
    // The workspace image attaches and nothing reaches the draft.
    paste!(app, format!("{} ", png.display()));
    assert_eq!(app.ui.session.pending_media.len(), 1);
    assert!(app.input.buffer.is_empty());
    // A source file, a multi-line paste and an image outside the allowed
    // roots stay text.
    let source = root.path().join("main.rs").display().to_string();
    paste!(app, source.clone());
    assert_eq!(app.input.buffer, source.as_str());
    app.input.clear_buffer();
    paste!(app, format!("{}\nsecond", png.display()));
    assert_eq!(app.ui.session.pending_media.len(), 1);
    assert!(app.input.buffer.contains("second"));
    app.input.clear_buffer();
    let far = outside.path().join("far.png").display().to_string();
    paste!(app, far.clone());
    assert_eq!(app.ui.session.pending_media.len(), 1);
    assert_eq!(app.input.buffer, far.as_str());
}
