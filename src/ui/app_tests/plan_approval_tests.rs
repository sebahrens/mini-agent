use super::test_support::fixture_app;
use super::*;
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode};
use crate::session::MessageRole;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PLAN: &str = "Plan雪: update only the selected flag.\nVerify its test. $(literal)";

/// The `/cron` fire path reuses this module's owned provider fixtures.
mod cron_app_tests {
    include!("../cron_app_tests.rs");
}

struct OwnedApp<'a>(App<'a>);
impl<'a> std::ops::Deref for OwnedApp<'a> {
    type Target = App<'a>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for OwnedApp<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for OwnedApp<'_> {
    fn drop(&mut self) {
        self.0.cancel_pending_permissions();
        self.0.run.cancel_validation();
        if let Some(abort) = self.0.run.main_abort.take() {
            abort.abort();
        }
        self.0.ui.sandbox.kill_active();
    }
}
fn owned_app(ui: UiContext<'_>) -> OwnedApp<'_> {
    OwnedApp(fixture_app(ui))
}

struct Provider {
    address: std::net::SocketAddr,
    requests: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    task: tokio::task::JoinSet<()>,
}
impl Provider {
    async fn new(responses: Vec<&'static str>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let mut task = tokio::task::JoinSet::new();
        task.spawn(async move {
            for text in responses {
                tokio::time::timeout(Duration::from_secs(15), async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let (header, length) = loop {
                        let mut chunk = [0; 4096];
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert_ne!(count, 0, "owned provider header closed");
                        bytes.extend_from_slice(&chunk[..count]);
                        assert!(bytes.len() <= 256 * 1024);
                        if let Some(header) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            let header_text = String::from_utf8_lossy(&bytes[..header]);
                            let length = header_text.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap();
                            break (header + 4, length);
                        }
                    };
                    assert!(length <= 256 * 1024);
                    while bytes.len() < header + length {
                        let mut chunk = [0; 4096];
                        let count = stream.read(&mut chunk).await.unwrap();
                        assert_ne!(count, 0, "owned provider body closed");
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    captured.lock().unwrap().push(serde_json::from_slice(&bytes[header..header+length]).unwrap());
                    if text == "hold" {
                        std::future::pending::<()>().await;
                    }
                    if text == "hold-open" {
                        let chunk = serde_json::json!({"id":"owned-plan80","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"STREAM_WITHOUT_FINAL"},"finish_reason":null}]});
                        let body = format!("data: {chunk}\n\n");
                        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n", body.len()).as_bytes()).await.unwrap();
                        std::future::pending::<()>().await;
                    }
                    let (status, body, kind) = if text == "error" {
                        ("400 Bad Request", "{\"error\":{\"message\":\"owned planning failure\"}}".into(), "application/json")
                    } else if text == "context-error" {
                        ("400 Bad Request", "{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"This model's maximum context length is 1000 tokens\"}}".into(), "application/json")
                    } else {
                        let delta = if matches!(text, "question-batch" | "question-again") {
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("{text}-id"),"type":"function","function":{"name":"ask_user_question","arguments":serde_json::json!({"questions":[
                                {"question":"Which branch?","options":[{"label":"main","description":"Primary branch"}],"allow_free_text":false},
                                {"question":"What should change?","allow_free_text":true}
                            ]}).to_string()}}]})
                        } else if text == "skills-search" {
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"instruction-search-one","type":"function","function":{"name":"skills_search","arguments":"{\"query\":\"code review\"}"}}]})
                        } else if text == "todo-progress67" {
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"todo67","type":"function","function":{"name":"todo_write","arguments":serde_json::json!({"todos":[
                                {"content":"done","status":"completed","priority":"medium"},
                                {"content":"cancelled","status":"cancelled","priority":"medium"},
                                {"content":"pending","status":"pending","priority":"medium"}
                            ]}).to_string()}}]})
                        } else if text == "unknown-tool" {
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"unknown-cost-initial","type":"function","function":{"name":"unknown_unregistered_cost_tool","arguments":"{}"}}]})
                        } else if text == "cap-write" {
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"write-cost-initial","type":"function","function":{"name":"write","arguments":"{\"path\":\"effect.txt\",\"content\":\"ACTUAL_OWNED_EFFECT\"}"}}]})
                        } else if text == "pressure-read" {
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"read-cost-compact","type":"function","function":{"name":"read","arguments":"{\"path\":\"held.txt\"}"}}]})
                        } else if text == "ask-read" {
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"read-plan80","type":"function","function":{"name":"read","arguments":"{\"path\":\"held.txt\"}"}}]})
                        } else if let Some(spec) = text.strip_prefix("wakeup:") {
                            // `wakeup:<delay_secs>:<noop>` calls the turn-scoped cron tool.
                            let (delay, noop) = spec.split_once(':').unwrap();
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("wakeup-{delay}-{noop}"),"type":"function","function":{"name":"schedule_wakeup","arguments":serde_json::json!({"delay_secs":delay.parse::<i64>().unwrap(),"reason":"owned reason","noop":noop == "true"}).to_string()}}]})
                        } else { serde_json::json!({"role":"assistant","content":text}) };
                        let first = serde_json::json!({"id":"owned-plan80","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                        let last = serde_json::json!({"id":"owned-plan80","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":if matches!(text,"ask-read"|"cap-write"|"unknown-tool"|"pressure-read"|"question-batch"|"question-again"|"skills-search"|"todo-progress67") || text.starts_with("wakeup:") {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":if text=="pressure-read" {100000} else {9},"completion_tokens":4,"total_tokens":if text=="pressure-read" {100004} else {13}}});
                        ("200 OK",format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),"text/event-stream")
                    };
                    let wire = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                    stream.write_all(wire.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }).await.expect("owned local provider must be bounded");
            }
        });
        Self {
            address,
            requests,
            task,
        }
    }
    fn config(&self) -> config::Config {
        serde_json::from_value(serde_json::json!({
            "session_title_model":"off", "compact":false,
            "custom_providers":{"plan80":{"provider_type":"openai","base_url":format!("http://{}/v1",self.address),"api_style":"completions"}}
        })).unwrap()
    }
    fn calls(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
    async fn finish(mut self) {
        while let Some(result) = self.task.join_next().await {
            result.unwrap();
        }
    }
    async fn cancel(mut self) {
        self.task.shutdown().await;
    }
}

struct Roots {
    directory: crate::tests::OwnedTestDirectory,
    _environment: crate::tests::ScopedProcessEnv,
}
impl Roots {
    fn new() -> Self {
        let directory = crate::tests::OwnedTestDirectory::new("actual-plan-approval");
        let bindings = [
            "MINI_AGENT_CONFIG_DIR",
            "MINI_AGENT_DATA_DIR",
            "MINI_AGENT_LOCAL_DATA_DIR",
            "MINI_AGENT_STATE_DIR",
            "MINI_AGENT_CACHE_DIR",
            "MINI_AGENT_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(directory.path().join(name).into_os_string())));
        let environment = crate::tests::ScopedProcessEnv::set(&bindings);
        Self {
            directory,
            _environment: environment,
        }
    }
    fn workspace(&self) -> Arc<crate::paths::WorkspaceBinding> {
        Arc::new(crate::paths::WorkspaceBinding::capture(self.directory.path()).unwrap())
    }
}
fn ui<'a>(
    cli: &'a crate::cli::Cli,
    cfg: &'a config::Config,
    session: &'a mut Session,
    context: &'a mut crate::context::ContextFiles,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    mode: SecurityMode,
) -> UiContext<'a> {
    let policy = PermissionConfigs::from(PermissionConfig {
        default: Some(Action::Deny),
        ..Default::default()
    });
    let checker = crate::permission::checker::PermissionChecker::new(
        &policy,
        mode,
        Some(workspace.root().to_owned()),
        None,
    )
    .unwrap();
    let client = crate::provider::create_client(
        "plan80",
        Some("unused-local-key"),
        cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .unwrap();
    UiContext::new(
        cli,
        cfg,
        session,
        context,
        workspace.clone(),
        client,
        Some(Arc::new(std::sync::Mutex::new(checker))),
        None,
        crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace),
        None,
    )
}
async fn user(app: &mut App<'_>, event: UserEvent) {
    app.user_tx.send(event).await.unwrap();
    let delivered = app.user_rx.recv().await.unwrap();
    assert!(matches!(
        crate::startup::heap_future(|| app.handle_user_event(delivered))
            .await
            .unwrap(),
        ControlFlow::Continue(())
    ));
}
async fn key(app: &mut App<'_>, code: KeyCode) {
    user(app, UserEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))).await;
}
pub(super) async fn drive_native_steer_fixture(
    ui: UiContext<'_>,
    stop_at_boundary: bool,
) -> Session {
    let mut app = owned_app(ui);
    user(&mut app, UserEvent::Paste("original steering task".into())).await;
    key(&mut app, KeyCode::Enter).await;
    assert!(app.run.is_running);
    user(
        &mut app,
        UserEvent::Paste("/steer redirect the running task".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    assert!(app.run.pending_inputs.is_empty());
    if stop_at_boundary {
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.run.is_running {
                let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                crate::startup::heap_future(|| app.handle_agent_event(event))
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        assert!(app.input.buffer.contains("redirect the running task"));
        assert!(
            !app.renderer
                .captured_output()
                .contains("steering delivered")
        );
    } else {
        assert!(finish_turn(&mut app).await);
        assert!(app.run.native_blocked.is_none());
        assert!(
            app.renderer
                .captured_output()
                .contains("steering delivered")
        );
    }
    let sessions = crate::paths::WorkspaceBinding::capture(
        &crate::paths::process_paths_ref().unwrap().sessions_dir(),
    )
    .unwrap();
    let saved = crate::session::storage::load_session_exact_read_only(
        &sessions,
        app.ui.session.id.as_ref(),
        &app.ui.workspace,
    )
    .unwrap()
    .unwrap();
    #[cfg(feature = "hooks")]
    if !stop_at_boundary {
        app.finish_session_end().await.unwrap();
    }
    saved
}

#[tokio::test]
async fn steering_compaction_presentation_failure_restores_pending_text() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(Vec::new()).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    let pending = PendingMainTurn::capture(app.ui.session, "original task");
    {
        let app = &mut app.0;
        crate::ui::mark_main_turn_started(app.ui.session, &mut app.run, pending);
    }
    let steering = crate::agent::steering::Steering::default();
    steering.enable();
    steering
        .submit("retained redirect after compact error")
        .unwrap();
    app.run.steering = Some(steering);
    app.run.is_running = true;
    app.run.pending_compaction_pressure = Some(0.9);
    app.renderer.fail_next_write_for_test();
    let error = app
        .handle_agent_event(AgentEvent::CompactionBoundary {
            interactions: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("owned test output failure"));
    assert!(!app.run.is_running);
    assert!(
        app.input
            .buffer
            .contains("retained redirect after compact error")
    );
    assert!(app.input.buffer.contains("original task"));
    assert_eq!(provider.calls().len(), 0);
    drop(app);
    provider.finish().await;
}

/// With `running_input = steer`, a run that holds no steering handle (any
/// run started through a path that skipped `adopt_started_runner`) must
/// queue the typed message rather than bounce it back into the editor.
#[tokio::test]
async fn steer_mode_enter_queues_when_run_has_no_steering_handle() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["hold-open"]).await;
    let mut cfg = provider.config();
    cfg.running_input = Some(crate::config::RunningInput::Steer);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan49",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    user(&mut app, UserEvent::Paste("original task".into())).await;
    key(&mut app, KeyCode::Enter).await;
    // The shared start path records the steering handle.
    assert!(app.run.is_running);
    assert!(app.run.steering.is_some());
    app.run.steering = None;
    user(&mut app, UserEvent::Paste("typed during goal".into())).await;
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(
        app.run.pending_inputs.pop_front().as_deref(),
        Some("typed during goal")
    );
    assert!(app.input.buffer.is_empty());
    drop(app);
    provider.cancel().await;
}

#[tokio::test]
async fn steering_without_another_boundary_restores_draft_on_finish_or_cancel() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for cancel in [false, true] {
        let roots = Roots::new();
        let provider = Provider::new(vec![if cancel {
            "hold-open"
        } else {
            "FINAL_WITHOUT_TOOL"
        }])
        .await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        user(&mut app, UserEvent::Paste("original task".into())).await;
        key(&mut app, KeyCode::Enter).await;
        user(
            &mut app,
            UserEvent::Paste("/steer retained redirect".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        app.input.load_text("existing draft");
        if cancel {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !app
                    .renderer
                    .captured_output()
                    .contains("STREAM_WITHOUT_FINAL")
                {
                    let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                    crate::startup::heap_future(|| app.handle_agent_event(event))
                        .await
                        .unwrap();
                }
            })
            .await
            .unwrap();
            app.abort_main_run().await.unwrap();
        } else {
            assert!(finish_turn(&mut app).await);
        }
        assert!(app.input.buffer.contains("retained redirect"));
        assert!(app.input.buffer.contains("existing draft"));
        assert!(
            !app.ui
                .session
                .messages
                .iter()
                .any(|row| row.role == MessageRole::User
                    && row.content.contains("retained redirect"))
        );
        assert!(!app.run.is_running);
        assert_eq!(provider.calls().len(), 1);
        drop(app);
        provider.cancel().await;
    }
}

#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
#[tokio::test]
async fn steering_cancel_during_prompt_hook_retains_text_and_reaps_owned_process() {
    use crate::tests::process_state::{ProcessIdentity, ProcessState};
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let _workspace = crate::extras::hooks::retain_test_workspace_selection();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::extras::hooks::reset_dispatcher();
        }
    }
    let _reset = Reset;
    let roots = Roots::new();
    let pid = roots.directory.path().join("steer-hook.pid");
    let settings = serde_json::json!({"UserPromptSubmit":[{"hooks":[{"type":"command","command":"/bin/sh","args":["-c",r#"input=$(cat); case "$input" in *redirect*) printf '%s\n' "$$" > "$1"; sleep 30;; esac"#,"owned-steering-hook",pid],"trust":"trusted","timeout":60}]}]});
    let hooks = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
    crate::extras::hooks::init_dispatcher(
        crate::extras::hooks::dispatcher::HookDispatcher::from_config(&hooks).unwrap(),
    );
    crate::extras::hooks::set_active_workspace(roots.directory.path());
    let provider = Provider::new(vec!["ask-read", "MUST_NOT_DISPATCH"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        tools: vec!["read".into()],
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    user(&mut app, UserEvent::Paste("original task".into())).await;
    key(&mut app, KeyCode::Enter).await;
    user(
        &mut app,
        UserEvent::Paste("/steer retained redirect".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    let identity = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(&pid)
                && value.ends_with('\n') {
                break ProcessIdentity::capture(value.trim().parse().unwrap()).unwrap();
            }
            tokio::select! {
                event = app.run.agent_rx.as_mut().unwrap().recv() => {
                    let event = event.unwrap();
                    assert!(!matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. } | AgentEvent::SteerConsumed { .. }));
                    crate::startup::heap_future(|| app.handle_agent_event(event)).await.unwrap();
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    }).await.unwrap();
    assert!(matches!(
        identity.state().unwrap(),
        ProcessState::Live { .. }
    ));
    app.abort_main_run().await.unwrap();
    assert_eq!(identity.state().unwrap(), ProcessState::Gone);
    assert!(app.input.buffer.contains("retained redirect"));
    assert_eq!(provider.calls().len(), 1);
    assert!(
        !app.ui
            .session
            .messages
            .iter()
            .any(|row| row.role == MessageRole::User && row.content.contains("retained redirect"))
    );
    drop(app);
    provider.cancel().await;
}

#[cfg(all(feature = "hooks", any(target_os = "macos", target_os = "linux")))]
#[tokio::test]
async fn steering_user_prompt_hooks_block_or_append_separate_canonical_context() {
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let _workspace = crate::extras::hooks::retain_test_workspace_selection();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::extras::hooks::reset_dispatcher();
        }
    }
    let _reset = Reset;
    for blocked in [false, true] {
        let roots = Roots::new();
        let decision = if blocked {
            r#"printf '%s' '{"decision":"block","reason":"STEERING79_BLOCKED"}'"#
        } else {
            r#"printf '%s' '{"additionalContext":"STEERING79_CONTEXT"}'"#
        };
        let command = format!("input=$(cat); case \"$input\" in *redirect*) {decision};; esac");
        let settings = serde_json::json!({"UserPromptSubmit":[{"hooks":[{"type":"command","command":"/bin/sh","args":["-c",command],"trust":"trusted","timeout":5}]}]});
        let hooks = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
        crate::extras::hooks::init_dispatcher(
            crate::extras::hooks::dispatcher::HookDispatcher::from_config(&hooks).unwrap(),
        );
        crate::extras::hooks::set_active_workspace(roots.directory.path());
        let provider = Provider::new(vec!["ask-read", "AFTER_HOOK"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        user(&mut app, UserEvent::Paste("original task".into())).await;
        key(&mut app, KeyCode::Enter).await;
        user(&mut app, UserEvent::Paste("/steer redirect literal".into())).await;
        key(&mut app, KeyCode::Enter).await;
        assert!(finish_turn(&mut app).await);
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].to_string().contains("redirect literal"), !blocked);
        assert_eq!(
            app.ui
                .session
                .messages
                .iter()
                .filter(|row| row.role == MessageRole::User
                    && row.content.as_str() == "redirect literal")
                .count(),
            usize::from(!blocked)
        );
        if blocked {
            assert!(
                app.renderer
                    .captured_output()
                    .contains("STEERING79_BLOCKED")
            );
        } else {
            assert!(calls[1].to_string().contains("STEERING79_CONTEXT"));
            assert_eq!(
                app.ui
                    .session
                    .messages
                    .iter()
                    .filter(|row| row.role == MessageRole::System
                        && row.content.contains("STEERING79_CONTEXT"))
                    .count(),
                1
            );
        }
        drop(app);
        provider.finish().await;
        crate::extras::hooks::reset_dispatcher();
    }
}

#[tokio::test]
async fn steering_enter_mode_and_explicit_command_use_one_pending_slot() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for mode in [
        crate::config::RunningInput::Queue,
        crate::config::RunningInput::Steer,
    ] {
        let roots = Roots::new();
        let provider = Provider::new(vec!["ask-read", "AFTER_STEER", "AFTER_QUEUE"]).await;
        let mut cfg = provider.config();
        cfg.running_input = Some(mode);
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["read".into()],
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        user(&mut app, UserEvent::Paste("original task".into())).await;
        key(&mut app, KeyCode::Enter).await;
        user(&mut app, UserEvent::Paste("redirect literal".into())).await;
        key(&mut app, KeyCode::Enter).await;
        if mode == crate::config::RunningInput::Queue {
            assert_eq!(
                app.run.pending_inputs.pop_front().as_deref(),
                Some("redirect literal")
            );
            user(
                &mut app,
                UserEvent::Paste("/steer explicit redirect".into()),
            )
            .await;
            key(&mut app, KeyCode::Enter).await;
        }
        let selected = if mode == crate::config::RunningInput::Queue {
            "explicit redirect"
        } else {
            "redirect literal"
        };
        user(&mut app, UserEvent::Paste("/steer second pending".into())).await;
        key(&mut app, KeyCode::Enter).await;
        // Steering is a queue (mini-agent-nvnkx.18): the second message is
        // accepted and both reach the next request.
        assert_eq!(app.input.buffer.as_str(), "");
        assert!(app.run.pending_inputs.is_empty());
        assert!(finish_turn(&mut app).await);
        assert_eq!(provider.calls().len(), 2);
        assert!(provider.calls()[1].to_string().contains(selected));
        assert!(provider.calls()[1].to_string().contains("second pending"));
        assert!(!provider.calls()[0].to_string().contains(selected));
        assert_eq!(
            app.ui
                .session
                .messages
                .iter()
                .filter(|row| row.role == MessageRole::User && row.content.as_str() == selected)
                .count(),
            1
        );
        drop(app);
        provider.cancel().await;
    }
}

pub(super) async fn drive_native_fixture_turns(
    ui: UiContext<'_>,
    prompts: &[&str],
    stale_end: bool,
) -> Vec<Session> {
    let mut app = owned_app(ui);
    let mut saved = Vec::new();
    for prompt in prompts {
        user(&mut app, UserEvent::Paste((*prompt).into())).await;
        key(&mut app, KeyCode::Enter).await;
        assert!(
            app.run.is_running,
            "actual Enter did not start the native actor"
        );
        assert!(finish_turn(&mut app).await, "native actor did not complete");
        assert!(
            app.run.native_owner.is_none(),
            "actor ticket outlived retirement"
        );
        assert!(
            app.run.native_blocked.is_none(),
            "valid native context was refused"
        );
        assert!(
            app.run.pending_turn.is_none(),
            "completed literal turn did not settle"
        );
        let sessions = crate::paths::WorkspaceBinding::capture(
            &crate::paths::process_paths_ref().unwrap().sessions_dir(),
        )
        .unwrap();
        saved.push(
            crate::session::storage::load_session_exact_read_only(
                &sessions,
                app.ui.session.id.as_ref(),
                &app.ui.workspace,
            )
            .unwrap()
            .unwrap(),
        );
    }
    #[cfg(feature = "hooks")]
    if stale_end {
        let path = crate::paths::process_paths_ref()
            .unwrap()
            .sessions_dir()
            .join(format!("{}.json", app.ui.session.id));
        let before = std::fs::read(&path).unwrap();
        app.ui
            .permission
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .set_mode(SecurityMode::Yolo);
        assert!(
            app.finish_session_end().await.is_err(),
            "changed live permission policy must refuse cleanup effects"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    } else {
        app.finish_session_end().await.unwrap();
    }
    #[cfg(not(feature = "hooks"))]
    assert!(!stale_end);
    saved
}

/// One live App through a script of native steps. A step starting with `/`
/// is a slash command (`/model x`, `/compact`); anything else is a prompt
/// that must start an actor and complete without a native refusal. With
/// `observe`, the exact saved session is read back after every step.
pub(super) async fn drive_native_script(
    ui: UiContext<'_>,
    steps: &[String],
    observe: bool,
) -> Vec<Session> {
    let mut app = owned_app(ui);
    let mut saved = Vec::new();
    for step in steps {
        user(&mut app, UserEvent::Paste(step.clone())).await;
        key(&mut app, KeyCode::Enter).await;
        if step.starts_with('/') {
            assert!(!app.run.is_running, "slash command started a turn: {step}");
        } else {
            assert!(
                app.run.is_running,
                "actual Enter did not start the native actor for {step:?}"
            );
            assert!(
                finish_turn(&mut app).await,
                "native actor did not complete step {step:?}"
            );
            assert!(app.run.native_owner.is_none());
            assert!(
                app.run.pending_turn.is_none(),
                "completed literal turn did not settle"
            );
        }
        assert!(
            app.run.native_blocked.is_none(),
            "native context was refused after {step:?}"
        );
        if observe {
            let sessions = crate::paths::WorkspaceBinding::capture(
                &crate::paths::process_paths_ref().unwrap().sessions_dir(),
            )
            .unwrap();
            saved.push(
                crate::session::storage::load_session_exact_read_only(
                    &sessions,
                    app.ui.session.id.as_ref(),
                    &app.ui.workspace,
                )
                .unwrap()
                .unwrap(),
            );
        }
    }
    #[cfg(feature = "hooks")]
    app.finish_session_end().await.unwrap();
    saved
}

/// One live App through native prompts with the session title left at its
/// default (generated by the session model after the first turn). The first
/// turn's title result is applied while the session is idle, exactly as the
/// event loop does, before the next prompt is submitted
/// (mini-agent-nvnkx.262).
pub(super) async fn drive_native_title_fixture(
    ui: UiContext<'_>,
    prompts: &[String],
) -> Vec<Session> {
    let mut app = owned_app(ui);
    let load = |app: &App<'_>| {
        let sessions = crate::paths::WorkspaceBinding::capture(
            &crate::paths::process_paths_ref().unwrap().sessions_dir(),
        )
        .unwrap();
        crate::session::storage::load_session_exact_read_only(
            &sessions,
            app.ui.session.id.as_ref(),
            &app.ui.workspace,
        )
        .unwrap()
        .unwrap()
    };
    let mut saved = Vec::new();
    for (index, prompt) in prompts.iter().enumerate() {
        user(&mut app, UserEvent::Paste(prompt.clone())).await;
        key(&mut app, KeyCode::Enter).await;
        assert!(app.run.is_running, "Enter did not start the native actor");
        assert!(finish_turn(&mut app).await, "native actor did not complete");
        assert!(
            app.run.native_blocked.is_none(),
            "native context was refused after {prompt:?}"
        );
        if index == 0 {
            let titled = tokio::time::timeout(Duration::from_secs(10), app.title_rx.recv())
                .await
                .expect("the title result arrives")
                .expect("title channel open");
            app.apply_session_title(titled).await.unwrap();
            assert!(app.ui.session.generated_title.is_some());
            assert!(
                app.run.pending_turn.is_none(),
                "title application must not reopen the turn"
            );
        }
        saved.push(load(&app));
    }
    #[cfg(feature = "hooks")]
    app.finish_session_end().await.unwrap();
    saved
}

pub(super) async fn drive_native_effort_fixture(ui: UiContext<'_>) -> Vec<Session> {
    use crate::config::types::ReasoningEffort as E;
    let mut app = owned_app(ui);
    let load = |app: &App<'_>| {
        let sessions = crate::paths::WorkspaceBinding::capture(
            &crate::paths::process_paths_ref().unwrap().sessions_dir(),
        )
        .unwrap();
        crate::session::storage::load_session_exact_read_only(
            &sessions,
            app.ui.session.id.as_ref(),
            &app.ui.workspace,
        )
        .unwrap()
        .unwrap()
    };
    let mut saved = Vec::new();
    user(
        &mut app,
        UserEvent::Paste("first literal TUI prompt".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    assert!(app.run.is_running);
    app.run_slash_command("/effort max").await.unwrap();
    assert_eq!(
        app.ui.session.reasoning_controls.effort, None,
        "active input must retain its captured effort"
    );
    assert!(finish_turn(&mut app).await);
    let first = load(&app);
    assert_eq!(
        (first.total_input_tokens, first.total_output_tokens),
        (10, 2)
    );
    let before = serde_json::to_value(&*app.ui.session).unwrap();
    for invalid in ["/effort high extra", "/effort made-up", "/reasoning off"] {
        app.run_slash_command(invalid).await.unwrap();
        assert_eq!(
            serde_json::to_value(&*app.ui.session).unwrap(),
            before,
            "unsupported control changed canonical state"
        );
    }
    app.run_slash_command("/effort").await.unwrap();
    assert_eq!(
        serde_json::to_value(&*app.ui.session).unwrap(),
        before,
        "query mutated the session"
    );
    let next = crate::provider::effort::SessionReasoning {
        effort: Some(E::Max),
        ..Default::default()
    };
    let mut imported: Session = serde_json::from_value(before.clone()).unwrap();
    assert!(
        app.run
            .native_session_end
            .as_mut()
            .unwrap()
            .update_reasoning(&mut imported, next)
            .await
            .is_err(),
        "same-ID imported data must not borrow the retired owner's value permit"
    );
    let mut stale = app.ui.session.clone();
    stale.total_input_tokens += 1;
    assert!(
        app.run
            .native_session_end
            .as_mut()
            .unwrap()
            .update_reasoning(&mut stale, next)
            .await
            .is_err(),
        "an old owner must not upgrade an edited snapshot"
    );
    assert_eq!(serde_json::to_value(&*app.ui.session).unwrap(), before);
    assert_eq!(
        serde_json::to_value(load(&app)).unwrap(),
        serde_json::to_value(&first).unwrap()
    );
    app.run_slash_command("/effort max").await.unwrap();
    assert_eq!(app.ui.session.reasoning_controls.effort, Some(E::Max));
    let changed = load(&app);
    assert_eq!(changed.reasoning_controls.effort, Some(E::Max));
    assert_eq!(changed.native_frontier.refusal(), None);
    assert_eq!(
        serde_json::to_value(changed.native_history_snapshot()).unwrap(),
        serde_json::to_value(first.native_history_snapshot()).unwrap()
    );
    assert_eq!(
        (changed.total_input_tokens, changed.total_output_tokens),
        (10, 2)
    );
    saved.push(first);
    user(
        &mut app,
        UserEvent::Paste("second literal TUI prompt".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    assert!(finish_turn(&mut app).await);
    let second = load(&app);
    assert_eq!(second.reasoning_controls.effort, Some(E::Max));
    assert_eq!(
        (second.total_input_tokens, second.total_output_tokens),
        (20, 4)
    );
    saved.push(second);
    app.run_slash_command("/effort default").await.unwrap();
    let reset = load(&app);
    assert_eq!(reset.reasoning_controls.effort, None);
    assert_eq!(reset.native_frontier.refusal(), None);
    assert_eq!(
        (reset.total_input_tokens, reset.total_output_tokens),
        (20, 4)
    );
    app.run
        .native_session_end
        .as_mut()
        .unwrap()
        .fail_next_control_publication();
    app.run_slash_command("/effort medium").await.unwrap();
    let failed = load(&app);
    assert_eq!(
        failed.native_frontier.refusal(),
        Some(crate::session::native_replay::RestartReason::InFlight)
    );
    assert_eq!(
        (failed.total_input_tokens, failed.total_output_tokens),
        (20, 4)
    );
    assert_eq!(
        failed.reasoning_controls.effort,
        Some(E::Medium),
        "the sealed current value is durably denied before its final checkpoint"
    );
    user(
        &mut app,
        UserEvent::Paste("literal prompt after failed effort save".into()),
    )
    .await;
    let refused = crate::startup::heap_future(|| {
        app.handle_user_event(UserEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
    })
    .await;
    assert!(
        refused.is_err(),
        "failed control publication must stop the next request before dispatch"
    );
    assert!(!app.run.is_running);
    assert_eq!(
        (
            app.ui.session.total_input_tokens,
            app.ui.session.total_output_tokens
        ),
        (20, 4)
    );
    assert!(load(&app).native_frontier.refusal().is_some());
    saved
}

#[cfg(feature = "agent-skills")]
pub(super) async fn drive_native_instruction_fixture(
    ui: UiContext<'_>,
    package: &std::path::Path,
) -> Vec<Session> {
    let mut app = owned_app(ui);
    let mut generations = Vec::new();
    let mut saved = Vec::new();
    for _ in 0..2 {
        crate::startup::heap_future(|| app.run_slash_command("/reviewer code review"))
            .await
            .unwrap();
        assert!(app.run.is_running);
        assert!(finish_turn(&mut app).await);
        let selected = app
            .run
            .instruction_turn
            .as_ref()
            .unwrap()
            .prepare("code review")
            .await
            .unwrap();
        assert_eq!(selected.selected.len(), 1);
        generations.push(selected.generation);
        let sessions = crate::paths::WorkspaceBinding::capture(
            &crate::paths::process_paths_ref().unwrap().sessions_dir(),
        )
        .unwrap();

        saved.push(
            crate::session::storage::load_session_exact_read_only(
                &sessions,
                app.ui.session.id.as_ref(),
                &app.ui.workspace,
            )
            .unwrap()
            .unwrap(),
        );
    }
    assert!(
        generations[1] > generations[0],
        "actual catalogue refresh must change generation"
    );
    std::fs::write(package.join("SKILL.md"), "---\nname: reviewer\ndescription: Review code carefully\n---\nCHANGED_NATIVE_REVIEW_GUIDANCE\n").unwrap();
    crate::startup::heap_future(|| app.run_slash_command("/reviewer code review"))
        .await
        .unwrap();
    assert!(app.run.is_running);
    assert!(
        !finish_turn(&mut app).await,
        "changed authored body must not complete old native chain"
    );
    assert!(app.ui.session.native_frontier.refusal().is_some());
    assert_eq!(
        (
            app.ui.session.total_input_tokens,
            app.ui.session.total_output_tokens
        ),
        (20, 4)
    );
    saved
}

async fn begin(app: &mut App<'_>) {
    user(app, UserEvent::Paste(".plan add the selected flag".into())).await;
    key(app, KeyCode::Enter).await;
    assert!(app.run.is_running && app.chain.plan.run.is_some());
    assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
}
async fn finish_turn(app: &mut App<'_>) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
            let success = matches!(event, AgentEvent::Done { .. });
            let terminal = success
                || matches!(
                    event,
                    AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. }
                );
            crate::startup::heap_future(|| app.handle_agent_event(event))
                .await
                .unwrap();
            if terminal {
                assert!(app.run.agent_rx.is_none() && app.run.main_abort.is_none());
                return success;
            }
        }
    })
    .await
    .unwrap()
}

/// mini-agent-5eoy7.3: the host directive reaches only the request of the
/// turn it was set for; the transcript, rewind targets, the saved session
/// and the next turn's replayed history only ever carry the literal text.
#[tokio::test]
async fn turn_directive53_reaches_one_request_and_never_the_transcript() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["FIRST_DONE53", "SECOND_DONE53"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_context_files: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    app.run.next_turn_directive = Some("ROUTE_MARK53: delegate to persona reviewer".into());
    crate::startup::heap_future(|| {
        app.start_main_run(
            "LITERAL_MARK53 please review",
            crate::question::QuestionOrigin::NewUserTurn,
        )
    })
    .await
    .unwrap();
    assert!(app.run.next_turn_directive.is_none());
    assert!(finish_turn(&mut app).await);
    crate::startup::heap_future(|| {
        app.start_main_run(
            "second literal turn",
            crate::question::QuestionOrigin::NewUserTurn,
        )
    })
    .await
    .unwrap();
    assert!(finish_turn(&mut app).await);

    let calls = provider.calls();
    assert_eq!(calls.len(), 2);
    let first = calls[0]["messages"].as_array().unwrap();
    let last = first.last().unwrap();
    assert_eq!(last["role"], "user");
    let parts = last["content"].as_array().expect("multi-part user content");
    assert_eq!(parts[0]["text"], "LITERAL_MARK53 please review");
    let directive = parts.last().unwrap()["text"].as_str().unwrap();
    assert!(directive.starts_with(crate::agent::input::TURN_DIRECTIVE_LABEL));
    assert!(directive.ends_with("ROUTE_MARK53: delegate to persona reviewer"));
    // The next request replays the saved literal turn, never the directive.
    let second = serde_json::to_string(&calls[1]["messages"]).unwrap();
    assert!(second.contains("LITERAL_MARK53 please review"));
    assert!(!second.contains("ROUTE_MARK53"), "{second}");

    let users: Vec<_> = app
        .ui
        .session
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .map(|m| m.content.to_string())
        .collect();
    assert_eq!(
        users,
        vec![
            "LITERAL_MARK53 please review".to_owned(),
            "second literal turn".to_owned()
        ]
    );
    assert!(
        super::super::rewind_targets(app.ui.session)
            .iter()
            .all(|(_, text)| !text.contains("ROUTE_MARK53"))
    );
    // A saved session (what --continue/--resume reload) carries no
    // directive text, so neither does its replayed history.
    let saved = serde_json::to_vec(app.ui.session).unwrap();
    assert!(!String::from_utf8_lossy(&saved).contains("ROUTE_MARK53"));
    let restored: Session = serde_json::from_slice(&saved).unwrap();
    let replay = serde_json::to_string(&crate::agent::runner::convert_history(&restored)).unwrap();
    assert!(replay.contains("LITERAL_MARK53 please review"));
    assert!(!replay.contains("ROUTE_MARK53"));
    drop(app);
    provider.finish().await;
}

/// mini-agent-5eoy7.5 (3): only `force` arms the typed marker, and it
/// names the real `task` tool.
#[cfg(feature = "subagents")]
#[test]
fn mention_tool_choice55_force_arms_task_and_hint_arms_nothing() {
    assert_eq!(
        super::mention_first_request_tool(&crate::config::Config::default()),
        None
    );
    let force = crate::config::Config {
        agent_mention_tool_choice: Some(crate::config::AgentMentionToolChoice::Force),
        ..Default::default()
    };
    assert_eq!(super::mention_first_request_tool(&force), Some("task"));
    let hint = crate::config::Config {
        agent_mention_tool_choice: Some(crate::config::AgentMentionToolChoice::Hint),
        ..Default::default()
    };
    assert_eq!(super::mention_first_request_tool(&hint), None);
}

/// A refused start still consumes the directive so it cannot attach to a
/// later, unrelated turn.
#[tokio::test]
async fn turn_directive53_is_consumed_by_a_refused_start() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_context_files: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    // A tiny window makes the local size preflight refuse the start.
    let mut session = Session::new(
        "plan80",
        "owned-model",
        1_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    app.run.next_turn_directive = Some("ROUTE_MARK53".into());
    app.run.next_turn_first_request_tool = Some("task");
    let too_large = "word ".repeat(20_000);
    crate::startup::heap_future(|| {
        app.start_main_run(&too_large, crate::question::QuestionOrigin::NewUserTurn)
    })
    .await
    .unwrap();
    assert!(!app.run.is_running);
    assert!(app.ui.session.messages.is_empty());
    assert!(app.run.next_turn_directive.is_none());
    assert!(app.run.next_turn_first_request_tool.is_none());
    assert!(provider.calls().is_empty());
    drop(app);
    provider.finish().await;
}

/// mini-agent-5eoy7.4: `@agent:` submissions through the real TUI submit
/// path. `rust-maintainer` is a compiled-in persona every workspace sees.
#[cfg(feature = "subagents")]
mod agent_mentions54 {
    use super::*;

    fn feed_text(app: &App<'_>) -> String {
        let lines = app.renderer.feed().lines(500);
        (0..lines.len())
            .map(|index| lines[index].text.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn users(app: &App<'_>) -> Vec<String> {
        app.ui
            .session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .map(|m| m.content.to_string())
            .collect()
    }
    /// The host directive block of one captured request, if any.
    fn directive(call: &serde_json::Value) -> Option<String> {
        let last = call["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["role"], "user");
        let parts = last["content"].as_array()?;
        let text = parts.last()?["text"].as_str()?.to_owned();
        text.starts_with(crate::agent::input::TURN_DIRECTIVE_LABEL)
            .then_some(text)
    }
    fn first_text(call: &serde_json::Value) -> String {
        let last = call["messages"].as_array().unwrap().last().unwrap().clone();
        match &last["content"] {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(parts) => parts[0]["text"].as_str().unwrap().to_owned(),
            other => panic!("unexpected content {other}"),
        }
    }

    /// Submit `typed` with Enter and, when a turn starts, finish it.
    /// Returns the captured requests and the app's user messages, feed
    /// text and editor buffer.
    async fn submit(
        typed: &str,
        responses: Vec<&'static str>,
        setup: impl FnOnce(&std::path::Path),
    ) -> (Vec<serde_json::Value>, Vec<String>, String, String) {
        submit_with(typed, responses, setup, None).await
    }

    /// [`submit`] with an `agent_mention_tool_choice` policy; `Some`
    /// also enables the real tool set so the `task` tool is advertised.
    async fn submit_with(
        typed: &str,
        responses: Vec<&'static str>,
        setup: impl FnOnce(&std::path::Path),
        tool_choice: Option<crate::config::AgentMentionToolChoice>,
    ) -> (Vec<serde_json::Value>, Vec<String>, String, String) {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        setup(roots.directory.path());
        let starts = !responses.is_empty();
        let provider = Provider::new(responses).await;
        let mut cfg = provider.config();
        cfg.agent_mention_tool_choice = tool_choice;
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: tool_choice.is_none(),
            no_context_files: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        user(&mut app, UserEvent::Paste(typed.into())).await;
        key(&mut app, KeyCode::Enter).await;
        assert!(app.run.next_turn_directive.is_none());
        assert_eq!(app.run.is_running, starts);
        if starts {
            assert!(finish_turn(&mut app).await);
        }
        let result = (
            provider.calls(),
            users(&app),
            feed_text(&app),
            app.input.buffer.to_string(),
        );
        drop(app);
        provider.finish().await;
        result
    }

    /// mini-agent-5eoy7.5 (3): under `force`, a mention turn's first
    /// request names the `task` tool in `tool_choice` (OpenAI completions
    /// wire); a plain turn and a `hint` mention turn never force.
    #[tokio::test]
    async fn mention_tool_choice55_force_reaches_only_mention_turns() {
        use crate::config::AgentMentionToolChoice::{Force, Hint};
        // The `task` tool registers only once the process-wide subagent
        // runtime is installed. Other tests do that as a side effect, so
        // this test passed only by suite order; install it explicitly
        // (idempotent) so it also passes when run alone.
        if !crate::extras::subagents::is_initialized() {
            let cfg = crate::config::Config::default();
            let client = crate::provider::create_client(
                "openrouter",
                Some("unused-test-key"),
                cfg.custom_providers_map(),
                cfg.api_keys.as_ref(),
            )
            .unwrap();
            crate::extras::subagents::install(crate::extras::subagents::resolve_config(
                &cfg,
                Some("unused-test-key"),
                "openrouter",
                "owned-model",
                &client,
            ));
        }
        let typed = "@agent:rust-maintainer audit src/auth.rs";
        let (calls, ..) = submit_with(typed, vec!["FORCED55"], |_| {}, Some(Force)).await;
        assert_eq!(calls.len(), 1);
        assert!(calls[0]["tools"].to_string().contains("\"task\""));
        assert_eq!(
            calls[0]["tool_choice"],
            serde_json::json!({"type":"function","function":{"name":"task"}})
        );
        assert!(directive(&calls[0]).is_some());
        for (typed, policy) in [
            ("plain question about src/auth.rs", Force),
            ("@agent:rust-maintainer audit src/auth.rs", Hint),
        ] {
            let (calls, ..) = submit_with(typed, vec!["UNFORCED55"], |_| {}, Some(policy)).await;
            assert_eq!(calls.len(), 1);
            let choice = &calls[0]["tool_choice"];
            assert!(choice.is_null() || choice == "auto", "{typed}: {choice}");
        }
    }

    #[tokio::test]
    async fn canonical_mention_sends_typed_text_plus_routing_directive() {
        let typed = "@agent:rust-maintainer audit src/auth.rs";
        let (calls, users, feed, buffer) = submit(typed, vec!["MENTION_DONE54"], |_| {}).await;
        assert_eq!(users, vec![typed.to_owned()]);
        assert!(feed.contains(typed), "{feed}");
        assert!(buffer.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(first_text(&calls[0]), typed);
        let directive = directive(&calls[0]).expect("routing directive");
        assert!(directive.contains("agent_type \"rust-maintainer\" via the task tool"));
        assert!(!users[0].contains("Host routing note"));
    }

    #[tokio::test]
    async fn resolvable_alias_is_canonicalised_everywhere() {
        let (calls, users, feed, _) =
            submit("@agent-rust-maintainer x", vec!["ALIAS_DONE54"], |_| {}).await;
        assert_eq!(users, vec!["@agent:rust-maintainer x".to_owned()]);
        assert!(feed.contains("@agent:rust-maintainer x"), "{feed}");
        assert_eq!(first_text(&calls[0]), "@agent:rust-maintainer x");
        assert!(
            directive(&calls[0])
                .unwrap()
                .contains("\"rust-maintainer\"")
        );
    }

    #[tokio::test]
    async fn unknown_name_is_refused_and_the_draft_restored() {
        let (calls, users, feed, buffer) = submit("@agent:nope x", vec![], |_| {}).await;
        assert!(calls.is_empty());
        assert!(users.is_empty());
        assert_eq!(buffer, "@agent:nope x");
        assert!(
            feed.contains("unknown agent '@agent:nope'; valid agents: "),
            "{feed}"
        );
        assert!(feed.contains("rust-maintainer"), "{feed}");
    }

    #[tokio::test]
    async fn embedded_agent_text_is_sent_untouched_without_directive() {
        let typed = "mail me at a@agent:b";
        let (calls, users, _, _) = submit(typed, vec!["PLAIN_DONE54"], |_| {}).await;
        assert_eq!(users, vec![typed.to_owned()]);
        assert_eq!(first_text(&calls[0]), typed);
        assert_eq!(directive(&calls[0]), None);
    }

    #[tokio::test]
    async fn untrusted_project_persona_is_refused() {
        let (calls, users, feed, buffer) = submit("@agent:projectonly x", vec![], |root| {
            for layer in [".zerostack/agents", ".claude/agents", ".mini-agent/agents"] {
                std::fs::create_dir_all(root.join(layer)).unwrap();
                std::fs::write(root.join(layer).join("projectonly.md"), "PROJECT_PERSONA54")
                    .unwrap();
            }
        })
        .await;
        assert!(calls.is_empty() && users.is_empty());
        assert_eq!(buffer, "@agent:projectonly x");
        assert!(
            feed.contains("unknown agent '@agent:projectonly'"),
            "{feed}"
        );
    }

    /// The queue-replay site resolves mentions like a direct submit.
    #[tokio::test]
    async fn queued_mention_is_canonicalised_and_routed() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(vec!["FIRST_DONE54", "QUEUED_DONE54"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            no_context_files: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        user(&mut app, UserEvent::Paste("first plain turn".into())).await;
        key(&mut app, KeyCode::Enter).await;
        assert!(app.run.is_running);
        app.run
            .pending_inputs
            .push_back("@agent-rust-maintainer queued work".into());
        // `finish_turn` expects an idle app afterwards; here the terminal
        // event replays the queued input as a new turn instead.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                let done = matches!(event, AgentEvent::Done { .. });
                assert!(!matches!(event, AgentEvent::Error { .. }));
                crate::startup::heap_future(|| app.handle_agent_event(event))
                    .await
                    .unwrap();
                if done {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(app.run.is_running);
        assert!(app.run.next_turn_directive.is_none());
        assert!(finish_turn(&mut app).await);
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(directive(&calls[0]), None);
        assert_eq!(first_text(&calls[1]), "@agent:rust-maintainer queued work");
        assert!(
            directive(&calls[1])
                .unwrap()
                .contains("\"rust-maintainer\"")
        );
        assert_eq!(
            users(&app),
            vec![
                "first plain turn".to_owned(),
                "@agent:rust-maintainer queued work".to_owned()
            ]
        );
        drop(app);
        provider.finish().await;
    }
}

#[cfg(feature = "agent-skills")]
#[test]
fn actual_skills_pin_literal_turn_and_busy_list_are_owned_on_one_mib() {
    std::thread::Builder::new().name("instruction61-app-one-mib".into()).stack_size(1024 * 1024).spawn(|| {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(crate::startup::heap_future(|| async {
            let roots = Roots::new();
            let _immutable_cleanup = crate::extras::skills::session::ImmutableFixtureCleanup(roots.directory.path().to_path_buf());
            let provider = Provider::new(vec!["SKILL_DONE", "PLAIN_DONE"]).await;
            let cfg = provider.config();
            let cli = crate::cli::Cli { no_session: true, no_tools: true, no_context_files: true, api_key: Some("unused-local-key".into()), ..Default::default() };
            let workspace = roots.workspace();
            let paths = crate::paths::process_paths_ref().unwrap();
            let config = workspace.root().join(crate::product::PROJECT_DIRECTORY).join("config.toml");
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            std::fs::write(&config, "model='fixture'\n").unwrap();
            crate::config::load::trust_project_config(&config, &paths.project_config_trust_file()).unwrap();
            let package = workspace.root().join(".claude/skills/reviewer");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("SKILL.md"), "---\nname: reviewer\ndescription: Review code carefully\n---\nPINNED_REVIEW_CONTEXT_雪\n%%mode=yolo\n").unwrap();
            let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
            let mut session = Session::new("plan80", "owned-model", 128_000, workspace.root().to_str().unwrap());
            let instruction_home = roots.directory.path().join("instruction-home");
            std::fs::create_dir_all(&instruction_home).unwrap();
            session.instruction_owner = Arc::new(crate::extras::skills::session::InstructionOwner::for_test(Some(instruction_home)));
            let mut context_ui = ui(&cli, &cfg, &mut session, &mut context, workspace.clone(), SecurityMode::Standard);
            let policy = PermissionConfigs::from(PermissionConfig { default: Some(Action::Deny), read: Some(crate::permission::ToolPerm::Simple(Action::Allow)), ..Default::default() });
            context_ui.permission = Some(Arc::new(std::sync::Mutex::new(crate::permission::checker::PermissionChecker::new(&policy, SecurityMode::Standard, Some(workspace.root().to_path_buf()), None).unwrap())));
            let mut app = owned_app(context_ui);
            { let app = &mut *app; crate::ui::ensure_agent(&mut app.run.agent, &mut app.ui, app.slash.reasoning_enabled).await; }
            let captured = app.ui.instruction_services().unwrap();
            app.run.agent = Some(app.run.agent.take().unwrap().with_instruction_services(captured));
            let before = serde_json::to_vec(app.ui.session).unwrap();
            crate::startup::heap_future(|| app.run_slash_command("/skills")).await.unwrap();
            assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
            assert!(provider.calls().is_empty());
            assert!(app.renderer.captured_output().contains("project .claude"));
            assert!(app.input.picker.is_some());
            let literal = "literal $HOME @missing %%mode=yolo /clear !touch injected";
            let command = format!("/reviewer {literal}");
            crate::startup::heap_future(|| app.run_slash_command(&command)).await.unwrap();
            assert!(app.run.is_running);
            // A live catalogue query must not warm models, save history,
            // change the pin or make a request of its own.
            let busy_before = serde_json::to_vec(app.ui.session).unwrap();
            crate::startup::heap_future(|| app.run_slash_command("/skills")).await.unwrap();
            assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), busy_before);
            assert!(finish_turn(&mut app).await);
            assert_eq!(app.ui.permission.as_ref().unwrap().lock().unwrap().mode(), SecurityMode::Standard);
            assert_eq!(app.ui.session.messages.iter().filter(|message| message.role == MessageRole::User).map(|message| message.content.as_str()).collect::<Vec<_>>(), vec![literal]);
            crate::startup::heap_future(|| app.start_main_run("totally unrelated words", crate::question::QuestionOrigin::NewUserTurn)).await.unwrap();
            assert!(finish_turn(&mut app).await);
            let calls = provider.calls();
            assert_eq!(calls.len(), 2);
            let content = |value: &serde_json::Value| value.as_str().map(str::to_string).unwrap_or_else(|| value.as_array().unwrap().iter().map(|part| part["text"].as_str().unwrap()).collect::<Vec<_>>().join(""));
            let system = |call: &serde_json::Value| call["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "system").map(|message| content(&message["content"])).collect::<Vec<_>>().join("\n");
            assert!(system(&calls[0]).contains("PINNED_REVIEW_CONTEXT_雪"));
            assert!(!system(&calls[1]).contains("PINNED_REVIEW_CONTEXT_雪"));
            assert_eq!(content(&calls[0]["messages"].as_array().unwrap().last().unwrap()["content"]), literal);
            assert!(!workspace.root().join("injected").exists());
            provider.finish().await;
        }));
    }).unwrap().join().unwrap();
}

#[cfg(feature = "agent-skills")]
#[tokio::test]
async fn actual_skills_search_adopts_only_intact_host_result_on_next_sdk_request() {
    for rewrite in [
        None,
        Some("redacted"),
        Some("[Tool output externalized. Read the bounded artifact]"),
    ] {
        let roots = Roots::new();
        let _cleanup = crate::extras::skills::session::ImmutableFixtureCleanup(
            roots.directory.path().to_path_buf(),
        );
        let provider = Provider::new(vec!["skills-search", "SEARCH_DONE"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_context_files: true,
            tools: vec!["skills_search".into()],
            api_key: Some("unused-local-key".into()),
            ..Default::default()
        };
        let workspace = roots.workspace();
        let paths = crate::paths::process_paths_ref().unwrap();
        let package = roots.directory.path().join("reviewer");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("SKILL.md"),
            "---\nname: reviewer\ndescription: code review\n---\nSEARCH_SELECTED_INSTRUCTION\n",
        )
        .unwrap();
        crate::extras::skills::import_agent_skill(&package, paths).unwrap();
        let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.instruction_owner = Arc::new(
            crate::extras::skills::session::InstructionOwner::for_test(None),
        );
        let context_ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            workspace.clone(),
            SecurityMode::Standard,
        );
        let mut app = owned_app(context_ui);
        {
            let app = &mut *app;
            crate::ui::ensure_agent(&mut app.run.agent, &mut app.ui, app.slash.reasoning_enabled)
                .await;
        }
        let services = app.ui.instruction_services().unwrap();
        let mut agent = app
            .run
            .agent
            .take()
            .unwrap()
            .with_instruction_services(services);
        if let Some(text) = rewrite {
            agent = agent.with_instruction_test_result_rewrite(text);
        }
        app.run.agent = Some(agent);
        crate::startup::heap_future(|| {
            app.start_main_run(
                "unrelated initial topic",
                crate::question::QuestionOrigin::NewUserTurn,
            )
        })
        .await
        .unwrap();
        assert!(finish_turn(&mut app).await);
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        let first = serde_json::to_string(&calls[0]["messages"][0]).unwrap();
        let second = serde_json::to_string(&calls[1]["messages"][0]).unwrap();
        assert!(!first.contains("SEARCH_SELECTED_INSTRUCTION"));
        assert_eq!(
            second.contains("SEARCH_SELECTED_INSTRUCTION"),
            rewrite.is_none(),
            "{second}"
        );
        let result = serde_json::to_string(&calls[1]["messages"]).unwrap();
        if let Some(text) = rewrite {
            assert!(result.contains(text));
            assert!(!result.contains("agent_skills"));
        } else {
            assert!(result.contains("reviewer"));
        }
        assert_eq!(
            app.ui
                .session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .count(),
            1
        );
        provider.finish().await;
    }
}

#[cfg(feature = "agent-skills")]
#[tokio::test]
async fn actual_skill_pin_raw_override_fails_before_history_or_http() {
    let roots = Roots::new();
    let _cleanup = crate::extras::skills::session::ImmutableFixtureCleanup(
        roots.directory.path().to_path_buf(),
    );
    let provider = Provider::new(vec![]).await;
    let mut cfg = provider.config();
    cfg.extra_body =
        Some(serde_json::json!({"messages":[{"role":"user","content":"raw override"}]}));
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_context_files: true,
        api_key: Some("unused-local-key".into()),
        ..Default::default()
    };
    let workspace = roots.workspace();
    let paths = crate::paths::process_paths_ref().unwrap();
    let package = roots.directory.path().join("reviewer");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("SKILL.md"),
        "---\nname: reviewer\ndescription: code review\n---\nPIN_RAW_GUARDED\n",
    )
    .unwrap();
    crate::extras::skills::import_agent_skill(&package, paths).unwrap();
    let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.instruction_owner = Arc::new(
        crate::extras::skills::session::InstructionOwner::for_test(None),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut files,
        workspace,
        SecurityMode::Standard,
    ));
    let before = serde_json::to_vec(app.ui.session).unwrap();
    crate::startup::heap_future(|| app.run_slash_command("/reviewer literal args"))
        .await
        .unwrap();
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
    assert!(app.renderer.captured_output().contains("incompatible"));
    assert!(provider.calls().is_empty());
    provider.finish().await;
}

#[tokio::test]
async fn busy67_actual_picker_native_write_error_is_propagated() {
    let roots = Roots::new();
    let workspace = roots.workspace();
    let provider = Provider::new(Vec::new()).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_context_files: true,
        no_tools: true,
        api_key: Some("unused-local-key".into()),
        ..Default::default()
    };
    let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("plan80", "owned-model", 128_000, "picker-native-error");
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut files,
        workspace,
        SecurityMode::Standard,
    ));
    let native_fault = crate::ui::pickers::test_surface::NativeFaultGuard::new(true);
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
        app.user_tx
            .send(UserEvent::Key(KeyEvent::new(
                KeyCode::Char('/'),
                KeyModifiers::NONE,
            )))
            .await
            .unwrap();
        let event = app.user_rx.recv().await.unwrap();
        let error = crate::startup::heap_future(|| app.handle_user_event(event))
            .await
            .unwrap_err();
        let cause = error
            .downcast_ref::<std::io::Error>()
            .expect("picker IO failure retains its exact cause");
        assert_eq!(cause.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            cause.to_string(),
            "controlled native picker writer would block"
        );
        assert_eq!(
            native_fault.counts(),
            (0, 1),
            "renderer-owned geometry, then the actual native writer failure"
        );
        assert!(matches!(
            app.input.picker,
            Some(crate::ui::input::Picker::Command(_))
        ));
        assert!(app.ui.session.messages.is_empty() && app.ui.context.extra_files.is_empty());
        assert!(provider.calls().is_empty());
    }))
    .await;
    provider.cancel().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn busy67_actual_add_completion_tracks_captured_workspace_without_chdir() {
    let roots = Roots::new();
    let second = crate::tests::OwnedTestDirectory::new("add67-second-root");
    let second_workspace =
        Arc::new(crate::paths::WorkspaceBinding::capture(second.path()).unwrap());
    let process_cwd = std::env::current_dir().unwrap();
    let first_workspace = roots.workspace();
    std::fs::write(
        first_workspace.root().join("only_first67.txt"),
        "FIRST_ONLY_CONTENT",
    )
    .unwrap();
    let chosen = "only_second67 雪.txt";
    std::fs::write(second_workspace.root().join(chosen), "SECOND_ONLY_CONTENT").unwrap();
    let provider = Provider::new(Vec::new()).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_context_files: true,
        no_tools: true,
        api_key: Some("unused-local-key".into()),
        ..Default::default()
    };
    let mut files = crate::context::load(true).for_workspace_binding(true, &first_workspace);
    let mut session = Session::new("plan80", "owned-model", 128_000, "completion67");
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut files,
        first_workspace,
        SecurityMode::Standard,
    ));
    let native_fault = crate::ui::pickers::test_surface::NativeFaultGuard::new(false);
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
        for code in [
            KeyCode::Char('/'),
            KeyCode::Char('a'),
            KeyCode::Char('d'),
            KeyCode::Char('d'),
            KeyCode::Enter,
        ] {
            key(&mut app, code).await;
            #[cfg(feature = "mcp")]
            if code == KeyCode::Char('/') {
                app.input
                    .set_mcp_resources(vec!["mcp:foreign:urn%3AONLY_RESOURCE67".into()]);
            }
        }
        busy67_finish_file_walk(&mut app).await;
        let Some(crate::ui::input::Picker::File(picker)) = &app.input.picker else {
            panic!("add command picker missing")
        };
        assert!(
            picker
                .matches
                .iter()
                .any(|path| path == std::path::Path::new("only_first67.txt"))
        );
        assert!(
            !picker
                .matches
                .iter()
                .any(|path| path == std::path::Path::new("Cargo.toml")
                    || path.to_string_lossy().contains("mcp:"))
        );
        key(&mut app, KeyCode::Esc).await;
        app.input.clear_buffer();
        // This is the same captured UiContext field changed by workspace/session
        // activation; the process cwd is deliberately left unchanged.
        app.ui.workspace = second_workspace.clone();
        app.ui.sandbox = app
            .ui
            .sandbox
            .clone()
            .with_workspace_binding(second_workspace.clone());
        user(&mut app, UserEvent::Paste("/add only_second67".into())).await;
        key(&mut app, KeyCode::Tab).await;
        busy67_finish_file_walk(&mut app).await;
        let Some(crate::ui::input::Picker::File(picker)) = &app.input.picker else {
            panic!("captured add picker missing")
        };
        assert_eq!(picker.matches, vec![std::path::PathBuf::from(chosen)]);
        key(&mut app, KeyCode::Enter).await;
        let selected = app.input.buffer.to_string();
        assert_eq!(
            crate::ui::slash::split_args(&selected).unwrap(),
            vec!["/add", chosen]
        );
        assert!(app.ui.context.extra_files.is_empty() && app.ui.session.messages.is_empty());
        crate::startup::heap_future(|| app.run_slash_command(&selected))
            .await
            .unwrap();
        let expected = second_workspace.root().join(chosen);
        assert_eq!(app.ui.context.extra_files, vec![expected.clone()]);
        assert_eq!(
            app.ui.context.extra_file_contents[&expected].as_str(),
            "SECOND_ONLY_CONTENT"
        );
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);
        assert!(provider.calls().is_empty());
        assert_eq!(
            native_fault.counts(),
            (0, 0),
            "captured picker cannot access native geometry or writer"
        );
        let rendered = app.renderer.captured_output();
        assert!(rendered.contains("/add") && rendered.contains(chosen));
    }))
    .await;
    provider.cancel().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn busy67_finish_file_walk(app: &mut App<'_>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Some(crate::ui::input::Picker::File(picker)) = &mut app.input.picker else {
                panic!("file picker missing")
            };
            picker.try_finish_loading();
            if !picker.is_loading() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[cfg(feature = "subagents")]
#[tokio::test]
async fn background56_actual_app_approval_survives_parent_and_tasks_views_are_inert() {
    let roots = Roots::new();
    let provider = Provider::new(vec![]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_context_files: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("plan80", "owned-model", 32000, "background");
    session.working_dir = workspace.root().to_string_lossy().into();
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace.clone(),
        SecurityMode::Standard,
    ));
    let (sender, receiver) = mpsc::channel(32);
    app.ask_rx = Some(receiver);
    let policy = PermissionConfigs::from(PermissionConfig {
        default: Some(Action::Ask),
        ..Default::default()
    });
    let permission = Arc::new(std::sync::Mutex::new(
        crate::permission::checker::PermissionChecker::new(
            &policy,
            SecurityMode::Standard,
            Some(workspace.root().into()),
            None,
        )
        .unwrap(),
    ));
    app.ui.permission = Some(permission.clone());
    let run =
        crate::spend::run::Run::for_session(app.ui.session, &cfg, crate::spend::RequestKind::Turn);
    let pool = run.background_pool().unwrap();
    let recorder = app
        .ui
        .session
        .file_checkpoints
        .recorder(&app.ui.session.id, 0, true);
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
        for (key_code, permitted) in [(KeyCode::Char('y'), true), (KeyCode::Char('n'), false)] {
            let permission = Some(permission.clone());
            let sender = Some(sender.clone());
            let id = pool
                .reserve(&workspace, &run, &recorder)
                .unwrap()
                .start(
                    async move {
                        crate::agent::tools::check_perm(&permission, &sender, "read", "owned.txt")
                            .await
                            .map(|_| "approved exact child read".to_string())
                            .map_err(|e| e.to_string())
                    },
                    run.clone(),
                )
                .unwrap();
            let request =
                tokio::time::timeout(Duration::from_secs(2), app.ask_rx.as_mut().unwrap().recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert!(request.background_for(app.ui.session));
            app.background_requests.push_back(request);
            app.cancel_pending_permissions();
            assert_eq!(app.background_requests.len(), 1);
            let request = app.background_requests.pop_front().unwrap();
            app.begin_user_request(request).unwrap();
            assert!(
                app.permission_prompt.is_some(),
                "idle frontend must adopt a live child's ordinary Ask"
            );
            assert!(!app.run.is_running);
            app.cancel_pending_permissions();
            assert!(
                app.permission_prompt.is_some(),
                "foreground retirement cannot deny a different live scope"
            );
            key(&mut app, key_code).await;
            tokio::time::timeout(Duration::from_secs(2), async {
                while pool.result(&id).unwrap().status.is_active() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                pool.result(&id).unwrap().status,
                if permitted {
                    crate::extras::subagents::background::Status::Completed
                } else {
                    crate::extras::subagents::background::Status::Failed
                }
            );
        }
        let before = serde_json::to_vec(app.ui.session).unwrap();
        for command in ["/tasks", "/tasks invalid"] {
            crate::startup::heap_future(|| app.run_slash_command(command))
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_vec(app.ui.session).unwrap(),
                before,
                "views must not save, start a goal or add history"
            );
        }
        assert!(provider.calls().is_empty());
        assert!(app.renderer.captured_output().contains("completed"));
        assert_eq!(app.ui.session.background_task_counts(), (0, 2));
    }))
    .await;
    app.teardown_owners().await;
    provider.cancel().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[cfg(feature = "subagents")]
pub(super) async fn drive_native_background_fixture(
    ui: UiContext<'_>,
    child: crate::extras::subagents::SubagentConfig,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
) -> Vec<Session> {
    use rig::tool::Tool;
    let mut app = owned_app(ui);
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
        user(
            &mut app,
            UserEvent::Paste("first background parent prompt".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        assert!(finish_turn(&mut app).await);
        let first: Session =
            serde_json::from_value(serde_json::to_value(&*app.ui.session).unwrap()).unwrap();
        let cfg = app.ui.cfg;
        let run = crate::spend::run::Run::for_session(
            app.ui.session,
            cfg,
            crate::spend::RequestKind::Turn,
        );
        let pool = run.background_pool().unwrap();
        let tool = crate::extras::subagents::task_tool::TaskTool::new(None, None, false)
            .with_workspace_binding(app.ui.workspace.clone());
        let recorder = app
            .ui
            .session
            .file_checkpoints
            .recorder(&app.ui.session.id, 0, false);
        let launch = crate::extras::subagents::scope_test_config(
            child,
            crate::session::file_checkpoints::scope(
                Some(recorder),
                crate::spend::run::scope(
                    Some(run),
                    tool.call(crate::extras::subagents::task_tool::TaskArgs {
                        prompts: vec![
                            "Read owned-child.txt and report its literal contents".into(),
                        ],
                        briefs: None,
                        agent_type: None,
                        run_in_background: true,
                        options: Default::default(),
                    }),
                ),
            ),
        );
        let launch = tokio::time::timeout(Duration::from_secs(5), launch)
            .await
            .unwrap()
            .unwrap();
        assert!(launch.starts_with("Background task started:"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while calls.load(std::sync::atomic::Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            pool.active_count(),
            1,
            "actual child followup must remain held across the next main prompt"
        );
        assert!(
            app.ui.session.recorded_total_cost() > first.recorded_total_cost(),
            "actual accepted child usage must advance the same account"
        );
        app.run_slash_command("/tasks").await.unwrap();
        assert!(app.renderer.captured_output().contains("running"));
        user(
            &mut app,
            UserEvent::Paste("second background parent prompt".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        assert!(
            finish_turn(&mut app).await,
            "next native prompt must overlap the live child"
        );
        assert!(app.run.native_blocked.is_none());
        assert_eq!(pool.active_count(), 1);
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while pool.active_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let row = pool.list().pop().unwrap();
        assert_eq!(
            row.status,
            crate::extras::subagents::background::Status::Completed,
            "{row:?}"
        );
        let notices = pool.pending_notices().unwrap();
        assert_eq!(notices.messages().len(), 1);
        drop(notices);
        assert!(
            pool.pending_notices().is_some(),
            "preparation failure/drop cannot consume a notice"
        );
        let second: Session =
            serde_json::from_value(serde_json::to_value(&*app.ui.session).unwrap()).unwrap();
        user(
            &mut app,
            UserEvent::Paste("third background parent prompt".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        assert!(
            finish_turn(&mut app).await,
            "completion notice must enter ordinary native admission"
        );
        assert!(
            pool.pending_notices().is_none(),
            "accepted next turn delivers the exact completion once"
        );
        assert!(
            app.ui
                .session
                .messages
                .iter()
                .any(|row| row.content.contains("[Background task "))
        );
        let third: Session =
            serde_json::from_value(serde_json::to_value(&*app.ui.session).unwrap()).unwrap();
        vec![first, second, third]
    }))
    .await;
    release.notify_one();
    app.teardown_owners().await;
    if outcome.is_ok() {
        app.finish_session_end().await.unwrap();
    }
    match outcome {
        Ok(sessions) => sessions,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn busy67_actual_jobs_list_and_stop_keep_the_running_job_owned() {
    let roots = Roots::new();
    let provider = Provider::new(vec!["hold-open"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_context_files: true,
        no_tools: true,
        api_key: Some("unused-local-key".into()),
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("plan80", "owned-model", 128_000, "job control");
    let ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut files,
        workspace,
        SecurityMode::Standard,
    );
    let mut app = owned_app(ui);
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
        user(&mut app, UserEvent::Paste("held main turn".into())).await;
        key(&mut app, KeyCode::Enter).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.calls().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let id = app
            .ui
            .sandbox
            .start_background_command(
                "printf BG_%s READY67; sleep 60".into(),
                Duration::from_secs(20),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !app
                .ui
                .sandbox
                .background_job_snapshot(&id)
                .unwrap()
                .transcript
                .channel_bytes(crate::sandbox::OutputChannel::Stdout)
                .windows(10)
                .any(|bytes| bytes == b"BG_READY67")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let before = serde_json::to_vec(app.ui.session).unwrap();
        crate::startup::heap_future(|| app.run_slash_command("/jobs"))
            .await
            .unwrap();
        assert!(app.renderer.captured_output().contains(&id));
        assert!(
            !app.renderer.captured_output().contains("BG_READY67"),
            "job output is not listing metadata"
        );
        let stop_command = format!("/jobs stop {id}");
        crate::startup::heap_future(|| app.run_slash_command(&stop_command))
            .await
            .unwrap();
        assert!(
            app.renderer
                .captured_output()
                .contains("stop requested; the job remains owned")
        );
        assert_eq!(
            app.ui.sandbox.background_job_summaries().len(),
            1,
            "stop cannot release the registry record"
        );
        let terminal = app
            .ui
            .sandbox
            .wait_background_job(&id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            terminal.status,
            crate::sandbox::BackgroundJobStatus::Cancelled
        );
        assert!(!terminal.descendants_escaped);
        assert_eq!(app.ui.sandbox.running_background_job_count(), 0);
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert!(app.run.is_running && app.run.agent_rx.is_some());
        assert_eq!(provider.calls().len(), 1);
    }))
    .await;
    crate::startup::heap_future(|| app.abort_main_run())
        .await
        .unwrap();
    provider.cancel().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn busy67_actual_views_preserve_held_run_and_reject_mutating_selectors() {
    std::thread::Builder::new()
        .name("busy67-readonly-one-mib".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::startup::heap_future(busy67_actual_views_case));
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn busy67_actual_views_case() {
    let roots = Roots::new();
    let provider = Provider::new(vec!["todo-progress67", "hold-open"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_context_files: true,
        tools: vec!["todo_write".into()],
        api_key: Some("unused-local-key".into()),
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut saved = Session::new(
        "plan80",
        "saved-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    saved.name = "SAVED_METADATA67".into();
    saved.working_dir = workspace.root().to_string_lossy().into();
    saved.add_message(crate::session::MessageRole::User, "saved first message");
    crate::session::storage::save_session(&saved).unwrap();
    let saved_path = crate::session::storage::session_path(&saved.id).unwrap();
    let saved_bytes = std::fs::read(&saved_path).unwrap();
    let mut ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut files,
        workspace.clone(),
        SecurityMode::Standard,
    );
    let policy = PermissionConfigs::from(PermissionConfig {
        default: Some(Action::Allow),
        ..Default::default()
    });
    ui.permission = Some(Arc::new(std::sync::Mutex::new(
        crate::permission::checker::PermissionChecker::new(
            &policy,
            SecurityMode::Standard,
            Some(workspace.root().to_owned()),
            None,
        )
        .unwrap(),
    )));
    let mut app = owned_app(ui);
    app.slash.todo_tools_enabled = true;
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
        user(&mut app, UserEvent::Paste("held ordinary turn".into())).await;
        key(&mut app, KeyCode::Enter).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                let ready =
                    matches!(&event, AgentEvent::ToolResult { name, .. } if name=="todo_write");
                crate::startup::heap_future(|| app.handle_agent_event(event))
                    .await
                    .unwrap();
                if ready {
                    break;
                }
            }
            while provider.calls().len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(app.ui.session.todos.progress(), Some((2, 3)));
        let (_, footer) = app.renderer.last_statusline();
        assert!(
            footer.iter().flatten().any(|span| matches!(
                span, crate::ui::statusline::StatusSpan::Text { text, .. }
                if text == "todo:2/3"
            )),
            "the actual tool result refreshes the sticky footer before another input"
        );
        app.run.pending_inputs.push_back("queued next turn".into());
        let before = serde_json::to_vec(app.ui.session).unwrap();
        let model = app.ui.session.model.clone();
        let id = app.ui.session.id.clone();
        let account = app.ui.session.spend_account.snapshot();
        for command in ["/help", "/model", "/sessions", "/sessions --all", "/jobs"] {
            crate::startup::heap_future(|| app.run_slash_command(command))
                .await
                .unwrap();
        }
        key(&mut app, KeyCode::Char('?')).await;
        assert!(app.input.buffer.is_empty());
        assert!(app.renderer.captured_output().contains("SAVED_METADATA67"));
        assert!(app.renderer.captured_output().contains("keys:"));
        assert!(
            app.renderer
                .captured_output()
                .contains("current model: owned-model")
        );
        assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
        assert_eq!(std::fs::read(&saved_path).unwrap(), saved_bytes);
        assert!(Arc::ptr_eq(&app.ui.workspace, &workspace));
        assert_eq!(app.ui.session.spend_account.snapshot(), account);
        assert_eq!(app.run.pending_inputs.len(), 1);
        assert!(app.run.is_running && app.run.agent_rx.is_some() && app.run.main_abort.is_some());
        assert_eq!(
            provider.calls().len(),
            2,
            "views must not warm another model or start queued work"
        );
        for command in [
            "/model changed",
            &format!("/sessions {}", saved.id),
            &format!("/sessions delete {}", saved.id),
        ] {
            crate::startup::heap_future(|| app.run_slash_command(command))
                .await
                .unwrap();
        }
        let catalogue = crate::session::catalogue::list_readonly(&workspace, false).unwrap();
        app.input
            .start_session_picker(catalogue, workspace.clone(), false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&saved_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(
            app.handle_session_picker_event(&UserEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            )))
            .await
            .unwrap()
        );
        assert_eq!(app.ui.session.id, id);
        assert_eq!(app.ui.session.model, model);
        assert_eq!(std::fs::read(&saved_path).unwrap(), saved_bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&saved_path).unwrap().permissions().mode() & 0o777,
                0o644,
                "stale selector must reject before loading or repairing its record"
            );
        }
        assert_eq!(provider.calls().len(), 2);
    }))
    .await;
    crate::startup::heap_future(|| app.abort_main_run())
        .await
        .unwrap();
    provider.cancel().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_plan_turn_asks_before_second_request_and_restores_exact_operator_ceiling() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for mode in [SecurityMode::Standard, SecurityMode::ReadOnly] {
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN, "IMPLEMENTED"]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(&cli, &cfg, &mut session, &mut context, workspace, mode));
        begin(&mut app).await;
        app.input.load_text("typed follow-up雪");
        app.run
            .pending_inputs
            .extend(["stale queued one".into(), "stale queued two".into()]);
        assert!(finish_turn(&mut app).await);
        assert_eq!(provider.calls().len(), 1);
        assert!(
            app.renderer
                .captured_output()
                .contains(crate::ui::plan::QUESTION)
        );
        assert_eq!(app.chain.plan.pending.as_ref().unwrap().plan, PLAN);
        assert_eq!(app.input.buffer, "typed follow-up雪");
        assert_eq!(app.run.pending_inputs.len(), 2);
        let granted = serde_json::to_value(&app.ui.session.permission_allowlist).unwrap();
        let approved = if mode == SecurityMode::ReadOnly {
            key(&mut app, KeyCode::Char('e')).await;
            app.input
                .load_text("Edited approved plan雪\n.plan !literal");
            key(&mut app, KeyCode::Enter).await;
            assert_eq!(provider.calls().len(), 1);
            "Edited approved plan雪\n.plan !literal"
        } else {
            PLAN
        };
        key(&mut app, KeyCode::Char('y')).await;
        assert!(app.run.is_running);
        assert!(app.run.pending_inputs.is_empty());
        assert_eq!(
            app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
            mode
        );
        assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("code"));
        assert!(finish_turn(&mut app).await);
        assert_eq!(
            serde_json::to_value(&app.ui.session.permission_allowlist).unwrap(),
            granted
        );
        assert!(
            crate::agent::tools::check_perm(&app.ui.permission, &None, "write", "selected.txt")
                .await
                .is_err()
        );
        let calls = provider.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].to_string().contains("Planning Mode"));
        assert!(calls[1].to_string().contains("Coding Mode"));
        let submitted = app
            .ui
            .session
            .messages
            .iter()
            .rfind(|message| message.role == MessageRole::User)
            .unwrap()
            .content
            .to_string();
        assert!(submitted.starts_with("Execute this approved plan"));
        assert!(submitted.ends_with(approved));
        assert_eq!(
            calls[1]["messages"].as_array().unwrap().last().unwrap()["content"].as_str(),
            Some(submitted.as_str())
        );
        assert!(
            calls[1]
                .to_string()
                .contains("Approved plan (literal text)")
        );
        assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
        app.input.clear_buffer();
        key(&mut app, KeyCode::Char('y')).await;
        assert_eq!(
            provider.calls().len(),
            2,
            "duplicate Y cannot admit a new turn"
        );
        provider.finish().await;
    }
}

#[tokio::test]
async fn usd_budget_actual_approved_plan_retains_literal_draft_without_second_request() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![PLAN]).await;
    let mut cfg = provider.config();
    // The owned provider reports 9 input and 4 output tokens. This is a
    // real exhausted paid cap, rather than an unknown-price refusal.
    cfg.max_budget_usd = Some(34.0 / 1_000_000.0);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.set_input_output_prices(Some((2.0, 4.0)));
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    begin(&mut app).await;
    assert!(finish_turn(&mut app).await);
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(app.chain.plan.pending.as_ref().unwrap().plan, PLAN);
    let before = serde_json::to_vec(app.ui.session).unwrap();
    assert!(
        (app.ui.session.spend_account.snapshot().recorded_spend() - cfg.max_budget_usd.unwrap())
            .abs()
            < 1e-12
    );
    key(&mut app, KeyCode::Char('y')).await;
    assert_eq!(provider.calls().len(), 1);
    assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
    assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
    assert!(app.run.pending_inputs.is_empty());
    assert!(
        app.input.buffer.starts_with("Execute this approved plan")
            && app.input.buffer.ends_with(PLAN)
    );
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    assert!(app.ui.session.permission_allowlist.is_empty());
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn usd_budget_initial_interactive_terminal_persists_receipt_before_another_turn() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![PLAN, "IMPLEMENTED"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.set_input_output_prices(Some((2.0, 4.0)));
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    begin(&mut app).await;
    assert!(finish_turn(&mut app).await);
    let ledger = crate::spend::Ledger::new(&roots.directory.path().join("MINI_AGENT_STATE_DIR"));
    let first = ledger.summary_for_session(&app.ui.session.id).unwrap();
    assert_eq!(
        first.session.records, 1,
        "terminal settlement must persist this turn without a future request"
    );
    assert_eq!(first.session.input_tokens, 9);
    assert_eq!(first.session.output_tokens, 4);
    assert!((first.session.known_cost - 0.000034).abs() < 1e-12);
    assert!(!app.ui.session.spend_account.has_pending());
    assert!(!app.ui.session.spend_account.snapshot().tracking_error);
    key(&mut app, KeyCode::Char('y')).await;
    assert!(finish_turn(&mut app).await);
    let second = ledger.summary_for_session(&app.ui.session.id).unwrap();
    assert_eq!(second.session.records, 2);
    assert_eq!(second.session.input_tokens, 18);
    assert_eq!(second.session.output_tokens, 8);
    assert!((second.session.known_cost - 0.000068).abs() < 1e-12);
    assert!(
        (app.ui.session.spend_account.snapshot().recorded_spend() - second.session.known_cost)
            .abs()
            < 1e-12
    );
    assert_eq!(provider.calls().len(), 2);
    assert!(app.ui.session.permission_allowlist.is_empty());
    crate::session::storage::save_session(app.ui.session).unwrap();
    let saved = crate::session::storage::load_session_exact(&app.ui.session.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (saved.total_input_tokens, saved.total_output_tokens),
        (18, 8)
    );
    assert!((saved.total_cost - 0.000068).abs() < 1e-12);
    assert!(!saved.spend_account.snapshot().tracking_error);
    drop(app);
    provider.finish().await;
}

#[tokio::test]
async fn usd_budget_initial_interactive_write_has_same_policy_positive_and_cap_negative() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for capped in [false, true] {
        let roots = Roots::new();
        let provider = Provider::new(if capped {
            vec!["cap-write"]
        } else {
            vec!["cap-write", "WRITE_COMPLETED"]
        })
        .await;
        let mut cfg = provider.config();
        cfg.max_budget_usd = capped.then_some(0.000034);
        let cli = crate::cli::Cli {
            no_session: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let effect = workspace.root().join("effect.txt");
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let mut ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace.clone(),
            SecurityMode::Standard,
        );
        // Both cases grant the same single owned write path; the ceiling
        // alone decides whether the actual registered WriteTool runs.
        let policy = PermissionConfigs::from(PermissionConfig {
            default: Some(Action::Deny),
            write: Some(crate::permission::ToolPerm::Granular(
                std::collections::HashMap::from([(
                    effect.to_string_lossy().into_owned(),
                    Action::Allow,
                )]),
            )),
            ..Default::default()
        });
        ui.permission = Some(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &policy,
                SecurityMode::Standard,
                Some(workspace.root().to_owned()),
                None,
            )
            .unwrap(),
        )));
        let mut app = owned_app(ui);
        user(
            &mut app,
            UserEvent::Paste("write the selected owned file".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        assert_eq!(finish_turn(&mut app).await, !capped);
        assert_eq!(effect.exists(), !capped);
        if !capped {
            assert_eq!(
                std::fs::read_to_string(&effect).unwrap(),
                "ACTUAL_OWNED_EFFECT"
            );
        }
        assert_eq!(provider.calls().len(), if capped { 1 } else { 2 });
        let ledger =
            crate::spend::Ledger::new(&roots.directory.path().join("MINI_AGENT_STATE_DIR"));
        let totals = ledger
            .summary_for_session(&app.ui.session.id)
            .unwrap()
            .session;
        assert_eq!(totals.records, if capped { 1 } else { 2 });
        assert!((totals.known_cost - if capped { 0.000034 } else { 0.000068 }).abs() < 1e-12);
        assert_eq!(
            app.ui.session.total_input_tokens,
            if capped { 9 } else { 18 }
        );
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert!(!app.ui.session.spend_account.has_pending());
        assert!(!app.ui.session.spend_account.snapshot().tracking_error);
        drop(app);
        provider.finish().await;
    }
}

#[test]
fn usd_budget_actual_midturn_compaction_reuses_account_and_prices_before_http_and_write() {
    std::thread::Builder::new()
        .name("midcompact-budget-one-mib".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::startup::heap_future(|| {
                    midturn_compaction_budget_matrix()
                }))
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn midturn_compaction_budget_matrix() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for (capped, exhaust_at_boundary) in [(true, false), (false, false), (true, true)] {
        let roots = Roots::new();
        let provider = Provider::new(vec![
            "pressure-read",
            "PAID_CONTEXT_SUMMARY",
            "cap-write",
            "WRITE_COMPLETED",
        ])
        .await;
        let mut cfg = provider.config();
        cfg.compact_enabled = Some(true);
        cfg.mid_turn_compact_threshold = Some(0.75);
        cfg.max_budget_usd = Some(if capped { 0.200030 } else { 1.0 });
        let cli = crate::cli::Cli::default();
        let workspace = roots.workspace();
        let effect = workspace.root().join("effect.txt");
        let read = workspace.root().join("held.txt");
        std::fs::write(&read, "owned context retained through restart").unwrap();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        // Known prior context explains the provider's pressure; without
        // it calibration correctly classifies 100k unexplained tokens as
        // an irreducible prompt floor and never requests compaction.
        session.add_message(MessageRole::User, &"x".repeat(80_000));
        session.add_message(MessageRole::Assistant, &"y".repeat(80_000));
        session.set_input_output_prices(Some((2.0, 4.0)));
        let id = session.id.clone();
        let mut ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace.clone(),
            SecurityMode::Standard,
        );
        let policy = PermissionConfigs::from(PermissionConfig {
            default: Some(Action::Deny),
            read: Some(crate::permission::ToolPerm::Granular(
                std::collections::HashMap::from([(
                    read.to_string_lossy().into_owned(),
                    Action::Allow,
                )]),
            )),
            write: Some(crate::permission::ToolPerm::Granular(
                std::collections::HashMap::from([(
                    effect.to_string_lossy().into_owned(),
                    Action::Allow,
                )]),
            )),
            ..Default::default()
        });
        ui.permission = Some(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &policy,
                SecurityMode::Standard,
                Some(workspace.root().to_owned()),
                None,
            )
            .unwrap(),
        )));
        let mut app = owned_app(ui);
        user(
            &mut app,
            UserEvent::Paste("read held.txt then write effect.txt".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        let ledger =
            crate::spend::Ledger::new(&roots.directory.path().join("MINI_AGENT_STATE_DIR"));
        let mut boundaries = 0;
        let mut read_succeeded = false;
        let succeeded = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                let success = matches!(event, AgentEvent::Done { .. });
                let terminal = success || matches!(event, AgentEvent::Error { .. });
                if matches!(&event, AgentEvent::ToolResult { name, is_error, .. } if name.as_str() == "read" && !*is_error) {
                    read_succeeded = true;
                }
                if matches!(event, AgentEvent::CompactionBoundary { .. }) {
                    assert!(read_succeeded, "the real allowed read completed before restart");
                    boundaries += 1;
                    let first = ledger.summary_for_session(&id).unwrap().session;
                    assert_eq!(
                        (first.records, first.input_tokens, first.output_tokens),
                        (1, 100000, 4)
                    );
                    assert!((first.known_cost - 0.200016).abs() < 1e-12);
                    assert_eq!(provider.calls().len(), 1);
                    assert!(!app.ui.session.spend_account.has_pending());
                    assert!(!app.ui.session.spend_account.snapshot().tracking_error);
                    if exhaust_at_boundary {
                        // A sibling paid request can settle after this
                        // runner parks but before compaction admission.
                        let usage = crate::event::UsageDelta { input_tokens: 9, output_tokens: 4, ..Default::default() };
                        crate::spend::run::Run::for_session(app.ui.session, &cfg, crate::spend::RequestKind::Turn).observe(usage);
                        app.ui.session.charge_usage_delta(usage, false);
                    }
                }
                crate::startup::heap_future(|| app.handle_agent_event(event))
                    .await
                    .unwrap();
                if terminal || !app.run.is_running {
                    break success;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            boundaries,
            1,
            "the actual SDK tool result must reach the requested restart boundary; rendered={}",
            app.renderer.captured_output()
        );
        eprintln!(
            "midcompact witness: capped={capped} boundaries={boundaries} succeeded={succeeded} posts={} effect={} rows={} account_spend={:.6} session_spend={:.6}",
            provider.calls().len(),
            effect.exists(),
            ledger.summary_for_session(&id).unwrap().session.records,
            app.ui.session.spend_account.snapshot().recorded_spend(),
            app.ui.session.total_cost
        );
        assert_eq!(succeeded, !capped);
        assert_eq!(
            provider.calls().len(),
            if exhaust_at_boundary {
                1
            } else if capped {
                2
            } else {
                4
            },
            "the admitted summary crosses the cap before a write or a third HTTP request"
        );
        assert_eq!(
            effect.exists(),
            !capped,
            "the same ordinary allowed write must only run with admitted spend"
        );
        if !capped {
            assert_eq!(
                std::fs::read_to_string(&effect).unwrap(),
                "ACTUAL_OWNED_EFFECT"
            );
        }
        for body in provider.calls() {
            assert_eq!(body["model"], "owned-model");
        }
        assert_eq!(app.ui.session.id, id);
        assert!(app.ui.session.permission_allowlist.is_empty());
        let totals = ledger.summary_for_session(&id).unwrap().session;
        assert_eq!(totals.records, if capped { 2 } else { 4 });
        assert_eq!(
            (totals.input_tokens, totals.output_tokens),
            if capped { (100009, 8) } else { (100027, 16) }
        );
        assert!((totals.known_cost - if capped { 0.200050 } else { 0.200118 }).abs() < 1e-12);
        assert!(
            (app.ui.session.spend_account.snapshot().recorded_spend() - totals.known_cost).abs()
                < 1e-12
        );
        assert_eq!(app.ui.session.total_input_tokens, totals.input_tokens);
        assert!(!app.ui.session.spend_account.has_pending());
        assert!(!app.ui.session.spend_account.snapshot().tracking_error);
        let mut compaction_receipts = 0;
        for line in std::fs::read_to_string(
            roots
                .directory
                .path()
                .join("MINI_AGENT_STATE_DIR/usage.jsonl"),
        )
        .unwrap()
        .lines()
        {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(row["session_id"], id.as_str());
            assert_eq!(row["provider"], "plan80");
            assert_eq!(row["model"], "owned-model");
            assert_eq!(row["prices"]["input"], 2.0);
            assert_eq!(row["prices"]["output"], 4.0);
            if row["kind"] == "compaction" {
                compaction_receipts += 1;
            } else {
                assert_eq!(row["kind"], "turn");
            }
        }
        assert_eq!(
            compaction_receipts,
            usize::from(!exhaust_at_boundary),
            "only admitted summaries produce compaction receipts"
        );
        if exhaust_at_boundary {
            assert!(
                app.renderer
                    .captured_output()
                    .contains("context relief failed")
            );
            assert!(app.run.main_abort.is_none() && app.run.agent_rx.is_none());
            assert!(app.run.pending_turn.is_none());
            assert!(!app.run.awaiting_compaction_relief);
            assert!(app.ui.session.compactions.is_empty());
            user(&mut app, UserEvent::Paste("operator can still type".into())).await;
            assert!(app.input.buffer.contains("operator can still type"));
        }
        drop(app);
        if capped {
            provider.cancel().await;
        } else {
            provider.finish().await;
        }
    }
}

/// Direct consumer seam control: the existing account is already at its
/// ceiling, so the restart must refuse HTTP even before another response.
#[tokio::test]
async fn usd_budget_midcompact_direct_entry_exhausted_account_refuses_http() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["cap-write", "WRITE_COMPLETED"]).await;
    let mut cfg = provider.config();
    cfg.max_budget_usd = Some(0.000034);
    let cli = crate::cli::Cli::default();
    let workspace = roots.workspace();
    let effect = workspace.root().join("effect.txt");
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.set_input_output_prices(Some((2.0, 4.0)));
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Yolo,
    ));
    user(
        &mut app,
        UserEvent::Paste("write the selected owned file".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    assert!(!finish_turn(&mut app).await);
    let ledger = crate::spend::Ledger::new(&roots.directory.path().join("MINI_AGENT_STATE_DIR"));
    assert_eq!(
        ledger
            .summary_for_session(&app.ui.session.id)
            .unwrap()
            .session
            .records,
        1
    );
    assert_eq!(provider.calls().len(), 1);
    assert!(app.ui.session.spend_account.snapshot().exhausted());
    assert!(!app.ui.session.spend_account.has_pending());
    // This is the actual UI function with a known exhausted checkpoint;
    // it does not pretend the preceding actor emitted a pressure event.
    let core = &mut app.0;
    crate::startup::heap_future(|| {
        mid_turn_compact_and_respawn(
            0.7,
            &[],
            &mut core.renderer,
            &mut core.run,
            &mut core.ui,
            &core.slash,
        )
    })
    .await
    .unwrap();
    assert!(!finish_turn(&mut app).await);
    assert_eq!(provider.calls().len(), 1);
    let totals = ledger
        .summary_for_session(&app.ui.session.id)
        .unwrap()
        .session;
    assert_eq!(
        (totals.records, totals.input_tokens, totals.output_tokens),
        (1, 9, 4)
    );
    assert!((totals.known_cost - 0.000034).abs() < 1e-12);
    assert!((app.ui.session.spend_account.snapshot().recorded_spend() - 0.000034).abs() < 1e-12);
    assert!(!app.ui.session.spend_account.has_pending());
    assert!(!app.ui.session.spend_account.snapshot().tracking_error);
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert!(!effect.exists());
    drop(app);
    provider.cancel().await;
}

#[tokio::test]
async fn usd_budget_initial_interactive_invalid_tool_and_cancel_retain_incomplete_without_fabricated_receipt()
 {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for response in ["unknown-tool", "hold-open"] {
        let roots = Roots::new();
        // An unknown tool name is answered with a corrective tool result
        // (mini-agent-sskdu.246), so that turn needs a second response.
        let provider = Provider::new(if response == "unknown-tool" {
            vec![response, "recovered"]
        } else {
            vec![response]
        })
        .await;
        let mut cfg = provider.config();
        cfg.max_budget_usd = Some(1.0);
        let cli = crate::cli::Cli {
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.set_input_output_prices(Some((2.0, 4.0)));
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        if response == "hold-open" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !app
                    .renderer
                    .captured_output()
                    .contains("STREAM_WITHOUT_FINAL")
                {
                    let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
                    assert!(!matches!(
                        event,
                        AgentEvent::Done { .. } | AgentEvent::Error { .. }
                    ));
                    crate::startup::heap_future(|| app.handle_agent_event(event))
                        .await
                        .unwrap();
                }
            })
            .await
            .unwrap();
            app.run.pending_inputs.push_back("never dispatched".into());
            user(
                &mut app,
                UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            )
            .await;
            assert!(app.run.agent_rx.is_none() && app.run.main_abort.is_none());
            assert!(app.run.pending_inputs.is_empty());
        } else {
            // The unknown tool is skipped with a corrective result and the
            // model's next response completes the turn instead of aborting it.
            assert!(finish_turn(&mut app).await);
            let calls = provider.calls();
            assert_eq!(calls.len(), 2);
            assert!(
                calls[1]
                    .to_string()
                    .contains("unknown_unregistered_cost_tool` is not available"),
                "{}",
                calls[1]
            );
            assert!(!app.run.is_running);
            drop(app);
            provider.finish().await;
            continue;
        }
        assert_eq!(provider.calls().len(), 1);
        assert!(app.ui.session.spend_account.snapshot().tracking_error);
        assert!(!app.ui.session.spend_account.has_pending());
        assert_eq!(app.ui.session.total_cost, 0.0);
        let ledger =
            crate::spend::Ledger::new(&roots.directory.path().join("MINI_AGENT_STATE_DIR"));
        assert_eq!(
            ledger
                .summary_for_session(&app.ui.session.id)
                .unwrap()
                .session
                .records,
            0
        );
        assert!(app.ui.session.permission_allowlist.is_empty());
        assert!(app.chain.plan.pending.is_none());
        crate::session::storage::save_session(app.ui.session).unwrap();
        let saved = crate::session::storage::load_session_exact(&app.ui.session.id)
            .unwrap()
            .unwrap();
        assert!(saved.spend_account.snapshot().tracking_error);
        assert_eq!(saved.total_cost, 0.0);
        key(&mut app, KeyCode::Char('y')).await;
        assert_eq!(provider.calls().len(), 1);
        assert!(!app.run.is_running);
        drop(app);
        if response == "hold-open" {
            provider.cancel().await;
        } else {
            provider.finish().await;
        }
    }
}

#[tokio::test]
async fn actual_plan_edit_requires_reapproval_and_decline_preserves_planning_and_draft() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![PLAN]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    begin(&mut app).await;
    assert!(finish_turn(&mut app).await);
    let before = serde_json::to_vec(app.ui.session).unwrap();
    let display = app.renderer.feed().reasoning_display();
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
    )
    .await;
    assert_eq!(app.renderer.feed().reasoning_display(), display.next());
    assert!(app.chain.plan.pending.is_some());
    assert_eq!(provider.calls().len(), 1);
    app.input.load_text("follow-up preserved in kill ring");
    key(&mut app, KeyCode::Char('e')).await;
    assert_eq!(app.input.buffer, PLAN);
    app.input
        .load_text("Edited literal plan雪\n.plan !never execute");
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(provider.calls().len(), 1);
    assert!(!app.run.is_running);
    assert!(
        app.renderer
            .captured_output()
            .contains("Edited plan (awaiting approval)")
    );
    assert_eq!(
        app.chain.plan.pending.as_ref().unwrap().plan,
        "Edited literal plan雪\n.plan !never execute"
    );
    assert_eq!(serde_json::to_vec(app.ui.session).unwrap(), before);
    key(&mut app, KeyCode::Char('n')).await;
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
    assert_eq!(
        app.ui.permission.as_ref().unwrap().lock().unwrap().mode(),
        SecurityMode::PlanWrite
    );
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
    )
    .await;
    assert_eq!(app.input.buffer, "follow-up preserved in kill ring");
    assert!(app.chain.plan.pending.is_none());
    provider.finish().await;
}

#[tokio::test]
async fn actual_plan_error_empty_and_caller_cancel_never_offer_execution() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for response in ["error", " \n", "hold"] {
        let roots = Roots::new();
        let provider = Provider::new(vec![response]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        if response == "hold" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while provider.calls().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            app.input.load_text("typed follow-up");
            app.run.pending_inputs.push_back("must not continue".into());
            user(
                &mut app,
                UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            )
            .await;
            assert!(app.run.pending_inputs.is_empty());
            assert!(app.run.agent_rx.is_none());
            assert_eq!(app.input.buffer, "add the selected flag");
            provider.cancel().await;
        } else {
            assert_eq!(finish_turn(&mut app).await, response != "error");
            assert_eq!(provider.calls().len(), 1);
            provider.finish().await;
        }
        assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
        assert!(
            !app.renderer
                .captured_output()
                .contains(crate::ui::plan::QUESTION)
        );
    }
}

#[tokio::test]
async fn actual_plan_stale_revision_workspace_prompt_and_modified_keys_cannot_execute() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for stale in [
        "revision",
        "workspace",
        "prompt",
        "mode",
        "session",
        "cancel",
    ] {
        let roots = Roots::new();
        let provider = Provider::new(vec![PLAN]).await;
        let cfg = provider.config();
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            SecurityMode::Standard,
        ));
        begin(&mut app).await;
        assert!(finish_turn(&mut app).await);
        let grants = serde_json::to_value(&app.ui.session.permission_allowlist).unwrap();
        for mut key_event in [
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        ] {
            if key_event.modifiers.is_empty() {
                key_event.kind = crossterm::event::KeyEventKind::Release;
            }
            user(&mut app, UserEvent::Key(key_event)).await;
            assert!(app.chain.plan.pending.is_some());
            assert_eq!(provider.calls().len(), 1);
        }
        match stale {
            "revision" => {
                app.ui.session.messages.last_mut().unwrap().content =
                    "different same-session plan".into()
            }
            "workspace" => app.ui.workspace = roots.workspace(),
            "prompt" => {
                app.ui
                    .context
                    .prompts
                    .insert("code".into(), "changed code prompt".into());
            }
            "mode" => {
                user(
                    &mut app,
                    UserEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
                )
                .await
            }
            "session" => app.ui.session.id = "different-session".into(),
            "cancel" => {}
            _ => unreachable!(),
        }
        app.run
            .pending_inputs
            .push_back("stale queued instruction".into());
        if stale == "cancel" {
            user(
                &mut app,
                UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            )
            .await;
        } else {
            key(&mut app, KeyCode::Char('y')).await;
        }
        assert!(app.chain.plan.pending.is_none());
        assert!(!app.run.is_running && app.run.agent_rx.is_none());
        assert!(app.run.pending_inputs.is_empty());
        assert_eq!(provider.calls().len(), 1);
        assert_eq!(
            serde_json::to_value(&app.ui.session.permission_allowlist).unwrap(),
            grants
        );
        provider.finish().await;
    }
}

#[tokio::test]
async fn actual_approved_plan_permission_stop_settles_run_without_queued_followup_or_grants() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    std::fs::write(
        roots.directory.path().join("held.txt"),
        "PRIVATE_READ_MUST_NOT_COMPLETE",
    )
    .unwrap();
    let provider = Provider::new(vec![PLAN, "ask-read"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        tools: vec!["read".into()],
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    );
    let policy = PermissionConfigs::from(PermissionConfig {
        read: Some(crate::permission::ToolPerm::Simple(Action::Ask)),
        ..Default::default()
    });
    ui.permission = Some(Arc::new(std::sync::Mutex::new(
        crate::permission::checker::PermissionChecker::new(
            &policy,
            SecurityMode::Standard,
            Some(ui.workspace.root().to_owned()),
            None,
        )
        .unwrap(),
    )));
    let (ask, requests) = mpsc::channel(4);
    ui.ask_tx = Some(ask);
    let mut app = owned_app(ui);
    app.ask_rx = Some(requests);
    begin(&mut app).await;
    assert!(finish_turn(&mut app).await);
    key(&mut app, KeyCode::Char('y')).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        let app = &mut app.0;
        loop {
            tokio::select! {
                Some(request) = async {app.ask_rx.as_mut().unwrap().recv().await} => {
                    let request = request.into_permission();
                    assert_eq!(request.tool,"read");
                    app.begin_permission_request(request).unwrap();
                    break;
                }
                Some(event) = async {app.run.agent_rx.as_mut().unwrap().recv().await} => {
                    assert!(!matches!(event,AgentEvent::Done {..}|AgentEvent::Error {..}));
                    crate::startup::heap_future(||app.handle_agent_event(event)).await.unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(app.permission_prompt.is_some());
    assert_eq!(provider.calls().len(), 2);
    app.input.load_text("typed follow-up before stop");
    app.run
        .pending_inputs
        .push_back("must never be admitted".into());
    key(&mut app, KeyCode::Esc).await;
    assert!(app.permission_prompt.is_none());
    assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
    assert!(app.run.pending_inputs.is_empty());
    assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert!(
        !app.ui
            .session
            .messages
            .iter()
            .any(|message| message.role == MessageRole::ToolResult
                && message.content.contains("PRIVATE_READ_MUST_NOT_COMPLETE"))
    );
    assert_eq!(
        std::fs::read_to_string(roots.directory.path().join("held.txt")).unwrap(),
        "PRIVATE_READ_MUST_NOT_COMPLETE"
    );
    assert_eq!(provider.calls().len(), 2);
    provider.finish().await;
}

#[tokio::test]
async fn actual_plan_failed_local_admission_retains_literal_draft_and_replay_cannot_arm_approval() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![PLAN]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    begin(&mut app).await;
    assert!(finish_turn(&mut app).await);
    let saved = serde_json::to_vec(app.ui.session).unwrap();
    let count = app.ui.session.messages.len();
    app.ui.session.context_window = 32;
    key(&mut app, KeyCode::Char('y')).await;
    assert!(!app.run.is_running && app.run.agent_rx.is_none());
    assert!(app.input.buffer.starts_with("Execute this approved plan"));
    assert!(app.input.buffer.ends_with(PLAN));
    assert_eq!(app.ui.session.messages.len(), count);
    assert_eq!(provider.calls().len(), 1);
    assert!(
        app.renderer
            .captured_output()
            .contains("recoverable text is in the editor")
    );
    drop(app);
    session = serde_json::from_slice(&saved).unwrap();
    let workspace = roots.workspace();
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    let permission = app.ui.permission.clone();
    crate::ui::apply_prompt_mode("plan", app.ui.context, &permission);
    app.finalize_turn().await.unwrap();
    assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
    assert!(!app.run.is_running);
    assert_eq!(provider.calls().len(), 1);
    provider.finish().await;
}

#[cfg(feature = "goal")]
#[tokio::test]
async fn actual_plan_approval_does_not_take_over_unfinished_goal() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![PLAN]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    begin(&mut app).await;
    assert!(finish_turn(&mut app).await);
    let goal = crate::extras::goal::Goal::new(
        "unfinished unrelated goal",
        vec!["retained criteria".into()],
    )
    .unwrap();
    let id = goal.id.clone();
    app.ui.session.goal_store.set(goal, false).unwrap();
    key(&mut app, KeyCode::Char('y')).await;
    assert!(app.chain.plan.pending.is_some());
    assert!(!app.run.is_running);
    assert_eq!(provider.calls().len(), 1);
    assert!(
        app.renderer
            .captured_output()
            .contains("Finish or stop the active work")
    );
    assert_eq!(app.ui.session.goal_store.snapshot().unwrap().id, id);
    key(&mut app, KeyCode::Char('n')).await;
    assert_eq!(app.ui.session.goal_store.snapshot().unwrap().id, id);
    provider.finish().await;
}

#[tokio::test]
async fn actual_plan_unsettled_terminal_receiver_cannot_offer_execution() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![PLAN]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    begin(&mut app).await;
    let done = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
            if matches!(event, AgentEvent::Done { .. }) {
                break event;
            }
            crate::startup::heap_future(|| app.handle_agent_event(event))
                .await
                .unwrap();
        }
    })
    .await
    .unwrap();
    // Hold the retirement boundary open after the genuine provider's Done;
    // a terminal payload alone must not confer execution authority.
    let (retained, receiver) = mpsc::channel(1);
    app.run.agent_rx = Some(receiver);
    tokio::time::pause();
    let advance = async {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
    };
    let (result, ()) = tokio::join!(
        crate::startup::heap_future(|| app.handle_agent_event(done)),
        advance
    );
    tokio::time::resume();
    assert!(result.unwrap_err().to_string().contains("settling"));
    assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
    assert!(
        !app.renderer
            .captured_output()
            .contains(crate::ui::plan::QUESTION)
    );
    drop(retained);
    app.run.retire(Duration::from_secs(1)).await.unwrap();
    assert_eq!(provider.calls().len(), 1);
    provider.finish().await;
}

#[test]
fn actual_plan_confirmation_event_loop_runs_on_one_mib_stack() {
    std::thread::Builder::new()
        .name("plan-approval-one-mib".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(crate::startup::heap_future(|| async {
                    #[cfg(feature = "hooks")]
                    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
                    let roots = Roots::new();
                    let provider = Provider::new(vec![PLAN]).await;
                    let cfg = provider.config();
                    let cli = crate::cli::Cli {
                        no_session: true,
                        no_tools: true,
                        ..Default::default()
                    };
                    let workspace = roots.workspace();
                    let mut context =
                        crate::context::load(true).for_workspace_binding(true, &workspace);
                    let mut session = Session::new(
                        "plan80",
                        "owned-model",
                        128_000,
                        workspace.root().to_str().unwrap(),
                    );
                    let mut app = owned_app(ui(
                        &cli,
                        &cfg,
                        &mut session,
                        &mut context,
                        workspace,
                        SecurityMode::Standard,
                    ));
                    let capture = app.renderer.capture_bytes();
                    let tx = app.user_tx.clone();
                    let calls = provider.requests.clone();
                    let driver = async move {
                        tx.send(UserEvent::Paste(".plan add the selected flag".into()))
                            .await
                            .unwrap();
                        tx.send(UserEvent::Key(KeyEvent::new(
                            KeyCode::Enter,
                            KeyModifiers::NONE,
                        )))
                        .await
                        .unwrap();
                        tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                let shown =
                                    String::from_utf8_lossy(&capture.lock().unwrap()).into_owned();
                                if shown.contains(crate::ui::plan::QUESTION) {
                                    break;
                                }
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        assert_eq!(calls.lock().unwrap().len(), 1);
                        tx.send(UserEvent::Key(KeyEvent::new(
                            KeyCode::Char('n'),
                            KeyModifiers::NONE,
                        )))
                        .await
                        .unwrap();
                        tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                if String::from_utf8_lossy(&capture.lock().unwrap())
                                    .contains("Plan not executed; still planning")
                                {
                                    break;
                                }
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        tx.send(UserEvent::Quit).await.unwrap();
                    };
                    let ((), result) = tokio::time::timeout(Duration::from_secs(10), async {
                        tokio::join!(driver, crate::startup::heap_future(|| app.run_inner()))
                    })
                    .await
                    .unwrap();
                    result.unwrap();
                    assert!(app.chain.plan.pending.is_none() && app.chain.plan.run.is_none());
                    assert!(!app.run.is_running && app.run.agent_rx.is_none());
                    assert_eq!(app.ui.context.current_prompt_name.as_deref(), Some("plan"));
                    assert_eq!(
                        app.ui
                            .session
                            .messages
                            .iter()
                            .filter(|message| message.role == MessageRole::User)
                            .count(),
                        1
                    );
                    assert_eq!(provider.calls().len(), 1);
                    provider.finish().await;
                }));
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn next_question(app: &mut App<'_>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                request = app.ask_rx.as_mut().unwrap().recv() => {
                    app.begin_user_request(request.unwrap()).unwrap();
                    assert!(app.question_prompt.is_some());
                    return;
                }
                event = app.run.agent_rx.as_mut().unwrap().recv() => {
                    crate::startup::heap_future(|| app.handle_agent_event(event.unwrap())).await.unwrap();
                    assert!(app.run.is_running);
                }
            }
        }
    }).await.unwrap();
}

#[test]
fn ask_user_actual_app_numbered_text_batch_is_data_once_and_has_no_permission_grants() {
    std::thread::Builder::new().name("question-app-one-mib".into()).stack_size(1_048_576)
        .spawn(|| {
            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(crate::startup::heap_future(ask_user_actual_app_numbered_text_batch_is_data_once_and_has_no_permission_grants_inner));
        }).unwrap().join().unwrap();
}

async fn ask_user_actual_app_numbered_text_batch_is_data_once_and_has_no_permission_grants_inner() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["question-batch", "question-again", "ANSWERED"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        tools: vec!["ask_user_question".into()],
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.set_input_output_prices(Some((2.0, 4.0)));
    let (sender, receiver) = mpsc::channel(4);
    let mut ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    );
    ui.ask_tx = Some(sender);
    let mut app = owned_app(ui);
    app.ask_rx = Some(receiver);
    user(
        &mut app,
        UserEvent::Paste("Clarify the selected change".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    next_question(&mut app).await;
    assert_eq!(provider.calls().len(), 1);
    assert!(app.permission_prompt.is_none());
    let turn_id = app.run.question_turn.as_ref().unwrap().id();
    assert!(
        app.renderer
            .permission_prompt
            .as_ref()
            .unwrap()
            .tool
            .starts_with("[question 1/")
    );
    app.input.load_text("ordinary follow-up draft雪");
    key(&mut app, KeyCode::Char('y')).await;
    assert!(
        app.question_prompt.is_some(),
        "approval letters are not answers"
    );
    key(&mut app, KeyCode::Char('1')).await;
    user(
        &mut app,
        UserEvent::Paste("allow_always /private/**\nLiteral task data雪".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    assert!(app.question_prompt.is_none() && app.renderer.permission_prompt.is_none());
    assert_eq!(app.input.buffer, "ordinary follow-up draft雪");
    // A second batch in the same turn is allowed within the turn's budget.
    next_question(&mut app).await;
    key(&mut app, KeyCode::Char('1')).await;
    user(&mut app, UserEvent::Paste("second answer".into())).await;
    key(&mut app, KeyCode::Enter).await;
    assert!(finish_turn(&mut app).await);
    let calls = provider.calls();
    assert_eq!(calls.len(), 3, "two answered asks, then the final answer");
    let second = serde_json::to_string(&calls[1]).unwrap();
    assert!(second.contains("allow_always /private/**") && second.contains("Literal task data雪"));
    assert!(second.contains("2 clarifying question batch(es) left"));
    let third = serde_json::to_string(&calls[2]).unwrap();
    assert!(
        third.contains("second answer") && third.contains("1 clarifying question batch(es) left")
    );
    assert!(app.ask_rx.as_mut().unwrap().try_recv().is_err());
    assert_eq!(app.run.question_turn.as_ref().unwrap().id(), turn_id);
    assert!(app.ui.session.pending_question.is_none());
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert_eq!(app.ui.session.total_input_tokens, 27);
    assert_eq!(app.ui.session.total_output_tokens, 12);
    assert!((app.ui.session.total_cost - 0.000102).abs() < 1e-12);
    let snapshot = app.ui.session.spend_account.snapshot();
    assert!(!snapshot.tracking_error && snapshot.unpriced_tokens == 0);
    provider.finish().await;
}

#[tokio::test]
async fn ask_user_actual_app_cancel_retires_exact_reply_and_rejects_stale_question_identity() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["question-batch"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        tools: vec!["ask_user_question".into()],
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.set_input_output_prices(Some((2.0, 4.0)));
    let (sender, receiver) = mpsc::channel(4);
    let mut ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    );
    ui.ask_tx = Some(sender);
    let mut app = owned_app(ui);
    app.ask_rx = Some(receiver);
    user(&mut app, UserEvent::Paste("Question to cancel".into())).await;
    key(&mut app, KeyCode::Enter).await;
    next_question(&mut app).await;
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    )
    .await;
    assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
    assert!(app.question_prompt.is_none() && app.ui.session.pending_question.is_none());
    let (reply, answer) = tokio::sync::oneshot::channel();
    let request = crate::permission::ask::QuestionRequest {
        id: uuid::Uuid::new_v4().to_string(),
        questions: crate::question::QuestionBatch(vec![crate::question::Question {
            question: "STALE".into(),
            options: vec![],
            allow_free_text: true,
            multi_select: false,
        }]),
        #[cfg(feature = "acp")]
        tool_call_id: Some("stale-call".into()),
        reply,
    };
    app.begin_user_request(crate::permission::ask::UserRequest::Question(request))
        .unwrap();
    assert!(matches!(
        answer.await.unwrap(),
        crate::question::QuestionAnswer::Cancelled
    ));
    assert!(app.question_prompt.is_none());
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(app.ui.session.total_input_tokens, 9);
    assert_eq!(app.ui.session.total_output_tokens, 4);
    assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
    provider.finish().await;
}
#[tokio::test]
async fn ask_user_actual_app_fail_parks_queue_and_clear_retires_pending_data_without_grants() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["question-batch"]).await;
    let mut cfg = provider.config();
    cfg.headless_questions = Some(crate::config::HeadlessQuestions::Fail);
    let cli = crate::cli::Cli {
        tools: vec!["ask_user_question".into()],
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.set_input_output_prices(Some((2.0, 4.0)));
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    // A deliberately absent interactive transport exercises the same typed
    // terminal consumed when an ACP client asks to answer in a new message.
    assert!(app.ui.ask_tx.is_none());
    user(
        &mut app,
        UserEvent::Paste("Clarify before continuing".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    app.input.load_text("owner follow-up draft雪");
    app.run
        .pending_inputs
        .push_back("queued task must stay parked".into());
    assert!(!finish_turn(&mut app).await);
    assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
    assert!(app.question_prompt.is_none() && app.permission_prompt.is_none());
    assert_eq!(app.input.buffer, "owner follow-up draft雪");
    let pending = app.ui.session.pending_question.clone().unwrap();
    assert_eq!(pending.id, app.run.question_turn.as_ref().unwrap().id());
    assert_eq!(pending.questions.0.len(), 2);
    assert!(pending.answers.is_empty());
    assert!(
        app.renderer
            .captured_output()
            .contains("queued prompts remain parked")
    );
    app.finalize_turn().await.unwrap();
    assert_eq!(
        app.run.pending_inputs.front().map(String::as_str),
        Some("queued task must stay parked")
    );
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(
        (
            app.ui.session.total_input_tokens,
            app.ui.session.total_output_tokens
        ),
        (9, 4)
    );
    assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
    assert!(!app.ui.session.spend_account.has_pending());
    assert!(!app.ui.session.spend_account.snapshot().tracking_error);
    assert!(app.ui.session.permission_allowlist.is_empty());
    let decoded: Session =
        serde_json::from_slice(&serde_json::to_vec(app.ui.session).unwrap()).unwrap();
    assert_eq!(decoded.pending_question.unwrap().id, pending.id);
    // The owner removes parked queued input before clearing the transcript.
    app.run.pending_inputs.clear();
    app.run_slash_command("/clear").await.unwrap();
    assert!(app.ui.session.pending_question.is_none() && app.run.question_turn.is_none());
    assert!(app.ui.session.messages.is_empty());
    assert!((app.ui.session.total_cost - 0.000034).abs() < 1e-12);
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert_eq!(provider.calls().len(), 1);
    provider.finish().await;
}

#[cfg(feature = "subagents")]
#[tokio::test]
async fn ask_user_native_auxiliary_clears_even_an_explicit_main_slot_without_consuming_it() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["question-batch", "CHILD_DONE"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        tools: vec!["ask_user_question".into()],
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    app.ensure_agent().await;
    let turn = crate::question::QuestionTurn::default();
    let candidate = app
        .run
        .agent
        .as_ref()
        .unwrap()
        .clone()
        .with_question_turn(Some(turn.clone()));
    let usage = crate::agent::runner::SharedUsageLedger::default();
    let observed = usage.clone();
    crate::question::scope(Some(turn.clone()), async {
        let result = crate::startup::heap_future(|| {
            candidate.run_subagent("Child intent", 3, None, &cfg.retry, usage, None)
        })
        .await;
        assert_eq!(result.response.unwrap(), "CHILD_DONE");
        assert_eq!(
            crate::question::QuestionTurn::current().unwrap().id(),
            turn.id()
        );
    })
    .await;
    assert!(
        turn.claim().is_ok(),
        "auxiliary transport cannot consume the parent's once slot"
    );
    assert!(turn.required().is_none());
    assert_eq!(observed.total().input_tokens, 18);
    assert_eq!(observed.total().output_tokens, 8);
    let calls = provider.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        serde_json::to_string(&calls[1])
            .unwrap()
            .contains("available only in the main user turn")
    );
    assert!(
        app.ui.session.pending_question.is_none() && app.ui.session.permission_allowlist.is_empty()
    );
    provider.finish().await;
}

/// Drives a TUI turn until it ends, returning how many terminal errors the
/// event stream produced after relief had been spent (those are surfaced).
async fn drive_context_relief_turn(app: &mut App<'_>) -> (usize, bool) {
    let mut surfaced_errors = 0;
    let mut done = false;
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(rx) = app.run.agent_rx.as_mut() {
            let event = rx.recv().await.unwrap();
            let terminal_error = matches!(event, AgentEvent::Error { .. });
            let spent = app.run.reactive_relief_used;
            let compact = app.ui.cfg.resolve_compact_enabled();
            let is_done = matches!(event, AgentEvent::Done { .. });
            crate::startup::heap_future(|| app.handle_agent_event(event))
                .await
                .unwrap();
            if terminal_error && (spent || !compact || !app.run.is_running) {
                surfaced_errors += 1;
                break;
            }
            if is_done {
                done = true;
                break;
            }
        }
    })
    .await
    .unwrap();
    (surfaced_errors, done)
}

/// mini-agent-xorxl.6: an image-heavy session past `compact_size_fraction`
/// but far below the token budget reaches the summarizer through the same
/// handler the Done path uses; a second pass on the unchanged result must
/// not spend another summarizer request.
#[tokio::test]
async fn tui_byte_pressure_below_token_pressure_compacts_once() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["BYTE_SUMMARY"]).await;
    let mut cfg = provider.config();
    cfg.compact_enabled = Some(true);
    cfg.compact_size_fraction = Some(0.001);
    cfg.keep_recent_tokens = Some(1_000);
    let cli = crate::cli::Cli {
        no_tools: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        1_000_000,
        workspace.root().to_str().unwrap(),
    );
    session.add_message(MessageRole::User, &"x".repeat(40_000));
    session.add_message(MessageRole::Assistant, &"y".repeat(40_000));
    session.add_message(MessageRole::User, &"z".repeat(8_000));
    let ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace.clone(),
        SecurityMode::Standard,
    );
    let mut app = owned_app(ui);
    let inner = &mut app.0;
    assert!(
        inner
            .ui
            .session
            .actionable_size_pressure(inner.ui.cfg.resolve_compact_size_fraction())
    );
    crate::ui::slash::handle_compress(
        None,
        true,
        &mut inner.run,
        &mut inner.renderer,
        &mut inner.ui,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        provider.calls().len(),
        1,
        "byte pressure reached the summarizer"
    );
    assert!(
        inner
            .ui
            .session
            .messages
            .iter()
            .any(|m| m.content.contains("BYTE_SUMMARY"))
    );
    // Unchanged (futile) result: the floor stops a second paid request.
    crate::ui::slash::handle_compress(
        None,
        true,
        &mut inner.run,
        &mut inner.renderer,
        &mut inner.ui,
        true,
    )
    .await
    .unwrap();
    assert_eq!(provider.calls().len(), 1, "no summarizer re-spend");
    provider.finish().await;
}

/// mini-agent-p0h9g.3: a measured boundary must reach the summarizer
/// even below the independent between-turn window-minus-reserve budget.
#[tokio::test]
async fn real_token_boundary_below_local_budget_compacts_and_continues() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![
        "TOKEN_BOUNDARY_SUMMARY",
        "CONTINUED_AFTER_TOKEN_RELIEF",
    ])
    .await;
    let mut cfg = provider.config();
    cfg.compact_enabled = Some(true);
    cfg.compact_size_fraction = Some(0.0);
    cfg.keep_recent_tokens = Some(1_000);
    cfg.mid_turn_compact_threshold = Some(0.80);
    let cli = crate::cli::Cli {
        no_tools: true,
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        100_000,
        workspace.root().to_str().unwrap(),
    );
    session.add_message(MessageRole::User, &"x".repeat(160_000));
    session.add_message(MessageRole::Assistant, "old work complete");
    session.add_message(MessageRole::User, &"continue ".repeat(1_000));
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    let before = app.ui.session.messages.len();
    app.mid_turn_compact(0.81, &[]).await.unwrap();
    assert!(app.ui.session.messages.len() < before + 1);
    assert!(!app.run.mid_turn_compaction_disabled);
    assert!(
        app.ui
            .session
            .messages
            .iter()
            .any(|message| message.content.contains("TOKEN_BOUNDARY_SUMMARY"))
    );
    let (_, done) = drive_context_relief_turn(&mut app).await;
    assert!(done);
    assert_eq!(
        provider.calls().len(),
        2,
        "one summary and one continuing request"
    );
    provider.finish().await;
}

#[tokio::test]
async fn token_boundary_with_no_old_prefix_disables_repeated_noop_relief() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec!["CONTINUED_WITHOUT_SUMMARY"]).await;
    let cfg = provider.config();
    let cli = crate::cli::Cli {
        no_tools: true,
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        100_000,
        workspace.root().to_str().unwrap(),
    );
    session.add_message(MessageRole::User, "only a current prompt");
    let mut app = owned_app(ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace,
        SecurityMode::Standard,
    ));
    app.mid_turn_compact(0.95, &[]).await.unwrap();
    assert!(app.run.mid_turn_compaction_disabled);
    assert!(app.ui.session.compactions.is_empty());
    let (_, done) = drive_context_relief_turn(&mut app).await;
    assert!(done);
    assert_eq!(
        provider.calls().len(),
        1,
        "no summarizer request for an empty old prefix"
    );
    provider.finish().await;
}

/// mini-agent-cj9oh: a provider context-length 400 compacts once and resumes
/// the TUI turn; a second rejection (or `compact_enabled = false`) surfaces.
#[tokio::test]
async fn context_length_rejection_compacts_once_and_resumes_the_tui_turn() {
    for (script, compact, expect_done, expect_calls, summary_failed) in [
        (
            vec![
                "context-error",
                "COMPACTION_SUMMARY",
                "FINISHED_AFTER_RELIEF",
            ],
            true,
            true,
            3,
            false,
        ),
        (
            vec!["context-error", "COMPACTION_SUMMARY", "context-error"],
            true,
            false,
            3,
            false,
        ),
        // A failed summarizer ends this turn and leaves the TUI live.
        // The cache fork and its independent text fallback both fail.
        (
            vec!["context-error", "context-error", "context-error"],
            true,
            false,
            3,
            true,
        ),
        (vec!["context-error"], false, false, 1, false),
    ] {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let roots = Roots::new();
        let provider = Provider::new(script).await;
        let mut cfg = provider.config();
        cfg.compact_enabled = Some(compact);
        cfg.keep_recent_tokens = Some(1_000);
        let cli = crate::cli::Cli {
            no_tools: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = Session::new(
            "plan80",
            "owned-model",
            128_000,
            workspace.root().to_str().unwrap(),
        );
        session.add_message(MessageRole::User, &"x".repeat(40_000));
        session.add_message(MessageRole::Assistant, &"y".repeat(40_000));
        let ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace.clone(),
            SecurityMode::Standard,
        );
        let mut app = owned_app(ui);
        user(
            &mut app,
            UserEvent::Paste("Continue this ordinary turn".into()),
        )
        .await;
        key(&mut app, KeyCode::Enter).await;
        let (surfaced, done) = drive_context_relief_turn(&mut app).await;
        assert_eq!(done, expect_done, "script outcome (compact={compact})");
        assert_eq!(surfaced, usize::from(!expect_done), "surfaced errors");
        assert_eq!(provider.calls().len(), expect_calls);
        assert_eq!(app.run.reactive_relief_used, compact);
        if summary_failed {
            assert!(
                app.renderer
                    .captured_output()
                    .contains("context relief failed")
            );
            assert!(!app.run.awaiting_compaction_relief);
            assert!(app.run.pending_turn.is_none());
            assert!(app.ui.session.compactions.is_empty());
            user(&mut app, UserEvent::Paste("operator can still type".into())).await;
            assert!(app.input.buffer.contains("operator can still type"));
        }
        if expect_done {
            let last = serde_json::to_string(&provider.calls()[2]).unwrap();
            assert!(
                last.contains("COMPACTION_SUMMARY"),
                "summary reaches the retried request"
            );
            assert!(
                last.contains("Earlier context was compacted"),
                "continuation prompt is sent"
            );
            assert!(
                app.ui
                    .session
                    .messages
                    .iter()
                    .any(|m| m.content.contains("FINISHED_AFTER_RELIEF"))
            );
        }
        provider.finish().await;
    }
}

#[tokio::test]
async fn ask_user_actual_pressure_restart_preserves_the_preclaimed_main_slot_and_budget_profile() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = Provider::new(vec![
        "pressure-read",
        "PAID_CONTEXT_SUMMARY",
        "question-again",
        "FINISHED_AFTER_RESTART",
    ])
    .await;
    let mut cfg = provider.config();
    cfg.compact_enabled = Some(true);
    cfg.mid_turn_compact_threshold = Some(0.75);
    let cli = crate::cli::Cli {
        tools: vec!["read".into(), "ask_user_question".into()],
        ..Default::default()
    };
    let workspace = roots.workspace();
    let read = workspace.root().join("held.txt");
    std::fs::write(&read, "owned allowed read").unwrap();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new(
        "plan80",
        "owned-model",
        128_000,
        workspace.root().to_str().unwrap(),
    );
    session.add_message(MessageRole::User, &"x".repeat(80_000));
    session.add_message(MessageRole::Assistant, &"y".repeat(80_000));
    session.set_input_output_prices(Some((2.0, 4.0)));
    let id = session.id.clone();
    let mut ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace.clone(),
        SecurityMode::Standard,
    );
    ui.permission = Some(Arc::new(std::sync::Mutex::new(
        crate::permission::checker::PermissionChecker::new(
            &PermissionConfigs::from(PermissionConfig {
                default: Some(Action::Deny),
                read: Some(crate::permission::ToolPerm::Granular(
                    std::collections::HashMap::from([(
                        read.to_string_lossy().into_owned(),
                        Action::Allow,
                    )]),
                )),
                ..Default::default()
            }),
            SecurityMode::Standard,
            Some(workspace.root().to_path_buf()),
            None,
        )
        .unwrap(),
    )));
    let mut app = owned_app(ui);
    user(
        &mut app,
        UserEvent::Paste("Continue this ordinary turn".into()),
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    let captured = app.run.question_turn.as_ref().unwrap().clone();
    captured.claim().unwrap();
    let mut boundaries = 0;
    let mut rejected_again = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
            let done = matches!(event, AgentEvent::Done { .. });
            assert!(!matches!(
                event,
                AgentEvent::Error { .. } | AgentEvent::AwaitingUser { .. }
            ));
            if matches!(event, AgentEvent::CompactionBoundary { .. }) {
                boundaries += 1;
            }
            if let AgentEvent::ToolResult {
                name,
                output,
                is_error,
                ..
            } = &event
                && name.as_str() == "ask_user_question"
            {
                assert!(*is_error);
                assert!(output.contains("already outstanding"), "{output}");
                rejected_again = true;
            }
            crate::startup::heap_future(|| app.handle_agent_event(event))
                .await
                .unwrap();
            assert_eq!(app.run.question_turn.as_ref().unwrap().id(), captured.id());
            if done {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(boundaries, 1);
    assert!(rejected_again);
    assert_eq!(provider.calls().len(), 4);
    let persisted = crate::spend::Ledger::new(&roots.directory.path().join("MINI_AGENT_STATE_DIR"))
        .summary_for_session(&id)
        .unwrap()
        .session;
    assert_eq!(persisted.records, 4);
    assert_eq!(
        (persisted.input_tokens, persisted.output_tokens),
        (100027, 16)
    );
    assert!((persisted.known_cost - 0.200118).abs() < 1e-12);
    assert!(
        !app.ui.session.spend_account.has_pending()
            && !app.ui.session.spend_account.snapshot().tracking_error
    );
    assert!(
        app.ui.session.pending_question.is_none() && app.ui.session.permission_allowlist.is_empty()
    );
    provider.finish().await;
}
#[test]
fn model_listing48_actual_refresh_and_picker_keep_session_and_do_not_call_model_on_one_mib_stack() {
    std::thread::Builder::new().name("model48-app-one-mib".into()).stack_size(1024*1024).spawn(|| {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(crate::startup::heap_future(|| async {
            let roots=Roots::new(); let workspace=roots.workspace();
            let provider=Provider::new(Vec::new()).await; let cfg=provider.config();
            let cli=crate::cli::Cli { no_session:true,no_context_files:true,no_tools:true,api_key:Some("unused-local-key".into()),..Default::default() };
            let mut files=crate::context::load(true).for_workspace_binding(true,&workspace);
            let mut session=Session::new("anthropic","owned-model",128000,"model-listing48");
            session.set_input_output_prices(Some((2.0,3.0)));
            let mut app=owned_app(ui(&cli,&cfg,&mut session,&mut files,workspace,SecurityMode::Standard));
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            app.ui.client=crate::provider::AnyClient::Anthropic(rig::providers::anthropic::Client::builder()
                .api_key("CAPTURED_LISTING_KEY").base_url(format!("http://{}",listener.local_addr().unwrap()))
                .http_client(crate::provider_http::ProviderHttpClient::default()).build().unwrap());
            let before=serde_json::to_vec(app.ui.session).unwrap();
            let operator=async {
                app.input.buffer="/models refresh".into(); app.input.cursor=app.input.buffer.len();
                user(&mut app,UserEvent::Key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE))).await;
                app.input.buffer="/models".into(); app.input.cursor=app.input.buffer.len();
                user(&mut app,UserEvent::Key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE))).await;
            };
            let server=async {
                let (mut socket,_)=listener.accept().await.unwrap();
                let mut bytes=Vec::new();
                while !bytes.windows(4).any(|v| v==b"\r\n\r\n") { let mut chunk=[0;1024];let n=socket.read(&mut chunk).await.unwrap();assert!(n>0);bytes.extend_from_slice(&chunk[..n]);assert!(bytes.len()<16384); }
                assert!(String::from_utf8(bytes).unwrap().starts_with("GET /v1/models?limit=1000 "));
                let body=r#"{"data":[{"id":"claude-opus-5-5","display_name":"Opus 雪","max_input_tokens":200000},{"id":"new-unpriced-model","max_input_tokens":64000}],"has_more":false}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            };
            tokio::time::timeout(Duration::from_secs(5),async {tokio::join!(operator,server);}).await.unwrap();
            assert_eq!(serde_json::to_vec(app.ui.session).unwrap(),before);
            let captured=app.renderer.captured_output(); assert!(captured.contains("model list refreshed"));assert!(captured.contains("claude-opus-5-5"));assert!(captured.contains("200k ctx"));assert!(captured.contains("(no price known)"));
            let ids=crate::ui::slash::cached_model_ids("anthropic",false,&app.ui.client);assert!(ids.contains(&"claude-opus-5-5".into()));
            assert!(!app.run.is_running && app.run.agent_rx.is_none() && app.run.main_abort.is_none());
            assert!(provider.calls().is_empty());
            assert!(tokio::time::timeout(Duration::from_millis(20),listener.accept()).await.is_err());
            provider.finish().await;
        }));
    }).unwrap().join().unwrap();
}
#[tokio::test]
async fn model_listing48_actual_selection_uses_completed_metadata_and_retains_local_pin() {
    for pin in [None, Some(123456)] {
        let roots = Roots::new();
        let workspace = roots.workspace();
        let provider = Provider::new(Vec::new()).await;
        let mut cfg = provider.config();
        cfg.context_window = pin;
        let cli = crate::cli::Cli {
            no_session: true,
            no_context_files: true,
            no_tools: true,
            api_key: Some("unused-local-key".into()),
            ..Default::default()
        };
        let mut files = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session =
            Session::new("openrouter", "old-owned-model", 128000, "model-selection48");
        let mut app = owned_app(ui(
            &cli,
            &cfg,
            &mut session,
            &mut files,
            workspace,
            SecurityMode::Standard,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        app.ui.client = crate::provider::AnyClient::OpenRouter(
            rig::providers::openrouter::Client::builder()
                .api_key("CAPTURED_SELECTION_KEY")
                .base_url(format!("http://{}", listener.local_addr().unwrap()))
                .http_client(crate::provider_http::ProviderHttpClient::default())
                .build()
                .unwrap(),
        );
        let operator = async {
            app.input.buffer = "/model selection48-live".into();
            app.input.cursor = app.input.buffer.len();
            user(
                &mut app,
                UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            )
            .await;
        };
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                assert!(bytes.len() < 16384);
            }
            assert!(
                String::from_utf8(bytes)
                    .unwrap()
                    .starts_with("GET /models ")
            );
            let body = r#"{"data":[{"id":"selection48-live","context_length":240000,"pricing":{"prompt":"0.000007","completion":"0.000011"}}]}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(operator, server);
        })
        .await
        .unwrap();
        assert_eq!(app.ui.session.model.as_str(), "selection48-live");
        assert_eq!(app.ui.session.context_window, pin.unwrap_or(240000));
        assert!((app.ui.session.input_token_cost - 7.0).abs() < 1e-12);
        assert!((app.ui.session.output_token_cost - 11.0).abs() < 1e-12);
        assert!(app.ui.session.messages.is_empty());
        assert!(provider.calls().is_empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
        provider.finish().await;
    }
}

#[path = "compaction_hook_tests.rs"]
mod compaction_hook_tests;
