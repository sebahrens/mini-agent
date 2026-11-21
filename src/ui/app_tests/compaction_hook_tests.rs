//! mini-agent-sskdu.239: the compaction hooks fire the same way in every
//! front end. A recording command hook sees `PreCompact` once, then
//! `PostCompact` carrying the stored summary, then `SessionStart(compact)`.
use super::*;

/// A local OpenAI-compatible provider that stays up for the whole test: model
/// listings (`GET`) are answered with an empty list, and every completion
/// request streams `HOOK_SUMMARY`, or never answers when `hold` is set.
struct LocalProvider {
    address: std::net::SocketAddr,
    posts: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl LocalProvider {
    async fn new(hold: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let posts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = posts.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(Self::serve(stream, hold, captured.clone()));
            }
        });
        Self {
            address,
            posts,
            task,
        }
    }

    async fn serve(
        mut stream: tokio::net::TcpStream,
        hold: bool,
        posts: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (header, length) = loop {
            let count = stream.read(&mut chunk).await.unwrap_or(0);
            if count == 0 {
                return;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(header) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..header]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                break (header + 4, length);
            }
        };
        while bytes.len() < header + length {
            let count = stream.read(&mut chunk).await.unwrap_or(0);
            if count == 0 {
                return;
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        let (kind, body) = if bytes.starts_with(b"GET ") {
            (
                "application/json",
                "{\"object\":\"list\",\"data\":[]}".to_string(),
            )
        } else {
            if let Ok(request) = serde_json::from_slice(&bytes[header..header + length]) {
                posts.lock().unwrap().push(request);
            }
            if hold {
                std::future::pending::<()>().await;
            }
            let first = serde_json::json!({"id":"hook","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"HOOK_SUMMARY"},"finish_reason":null}]});
            let last = serde_json::json!({"id":"hook","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}});
            (
                "text/event-stream",
                format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
            )
        };
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(wire.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    fn config(&self) -> config::Config {
        serde_json::from_value(serde_json::json!({
            "session_title_model":"off", "compact":false,
            "custom_providers":{"plan80":{"provider_type":"openai","base_url":format!("http://{}/v1",self.address),"api_style":"completions"}}
        })).unwrap()
    }

    fn calls(&self) -> Vec<serde_json::Value> {
        self.posts.lock().unwrap().clone()
    }
}

impl Drop for LocalProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn long_session(workspace: &crate::paths::WorkspaceBinding) -> Session {
    let mut session = Session::new(
        "plan80",
        "owned-model",
        1_000_000,
        workspace.root().to_str().unwrap(),
    );
    session.add_message(MessageRole::User, &"x".repeat(40_000));
    session.add_message(MessageRole::Assistant, &"y".repeat(40_000));
    session.add_message(MessageRole::User, &"z".repeat(8_000));
    session
}

#[cfg(all(unix, feature = "hooks"))]
mod hooks {
    use super::*;

    /// Events a recording hook saw, in order, as parsed envelopes.
    fn recorded(log: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn names(events: &[serde_json::Value]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| event["hook_event_name"].as_str())
            .collect()
    }

    /// Installs a dispatcher whose three compaction events append their envelope
    /// to the returned log. Hold `dispatcher_guard::acquire()` first: the
    /// dispatcher is process-global and the guard hides it from other tests.
    fn install_recording_hooks(root: &std::path::Path) -> std::path::PathBuf {
        let log = root.join("compaction-hooks.log");
        let handler = serde_json::json!([{"hooks":[{
            "type":"command","command":"/bin/sh",
            "args":["-c", "{ cat; printf '\\n'; } >> \"$1\"", "record", log],
            "trust":"trusted","timeout":5
        }]}]);
        let settings = serde_json::json!({
            "PreCompact": handler.clone(),
            "PostCompact": handler.clone(),
            "SessionStart": handler,
        });
        let hooks = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
        crate::extras::hooks::init_dispatcher(
            crate::extras::hooks::dispatcher::HookDispatcher::from_config(&hooks).unwrap(),
        );
        crate::extras::hooks::set_active_workspace(root);
        log
    }

    fn assert_one_committed_compaction(
        events: &[serde_json::Value],
        session: &Session,
        trigger: &str,
    ) {
        assert_eq!(
            names(events),
            ["PreCompact", "PostCompact", "SessionStart"],
            "{events:?}"
        );
        assert_eq!(events[0]["trigger"], trigger);
        assert_eq!(events[1]["trigger"], trigger);
        let stored = session.compacted_context().0.expect("a summary was stored");
        assert!(stored.contains("HOOK_SUMMARY"), "{stored}");
        assert_eq!(
            events[1]["compact_summary"], stored,
            "PostCompact carries the stored summary"
        );
        assert_eq!(events[2]["source"], "compact");
    }

    #[tokio::test]
    async fn tui_manual_compress_fires_the_compaction_hooks_once() {
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let _workspace = crate::extras::hooks::retain_test_workspace_selection();
        let roots = Roots::new();
        let log = install_recording_hooks(roots.directory.path());
        let provider = LocalProvider::new(false).await;
        let mut cfg = provider.config();
        cfg.keep_recent_tokens = Some(1_000);
        let cli = crate::cli::Cli {
            no_tools: true,
            no_session: true,
            ..Default::default()
        };
        let workspace = roots.workspace();
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut session = long_session(&workspace);
        let ui = ui(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace.clone(),
            SecurityMode::Standard,
        );
        let mut app = owned_app(ui);
        app.run_slash_command("/compress").await.unwrap();
        assert_eq!(app.ui.session.compactions.len(), 1);
        assert_one_committed_compaction(&recorded(&log), app.ui.session, "manual");
    }

    #[tokio::test]
    async fn headless_compaction_fires_the_compaction_hooks_once() {
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        let _workspace = crate::extras::hooks::retain_test_workspace_selection();
        let roots = Roots::new();
        let log = install_recording_hooks(roots.directory.path());
        let provider = LocalProvider::new(false).await;
        let mut cfg = provider.config();
        cfg.compact_enabled = Some(true);
        cfg.keep_recent_tokens = Some(1_000);
        let client = crate::provider::create_client(
            "plan80",
            Some("unused-local-key"),
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap();
        let workspace = roots.workspace();
        let mut session = long_session(&workspace);
        let (_, happened) = crate::startup::compact_headless_session(
            &mut session,
            &client,
            &cfg,
            #[cfg(feature = "memory")]
            None,
            0,
            None,
            true,
            None,
        )
        .await
        .unwrap();
        assert!(happened);
        assert_one_committed_compaction(&recorded(&log), &session, "auto");
    }
}

#[test]
fn compaction_progress_line_reports_elapsed_time_and_the_request() {
    let line =
        crate::ui::slash::compaction_progress_line(std::time::Duration::from_secs(125), 2, 3);
    assert!(line.contains("request 2 of ~3"), "{line}");
    assert!(line.contains("2m05s elapsed"), "{line}");
    // The estimate is a lower bound: a request past it raises the total.
    let line = crate::ui::slash::compaction_progress_line(std::time::Duration::from_secs(5), 4, 3);
    assert!(line.contains("request 4 of ~4"), "{line}");
    let line = crate::ui::slash::compaction_progress_line(std::time::Duration::from_secs(0), 0, 0);
    assert!(line.contains("request 1 of ~1"), "{line}");
}

/// mini-agent-sskdu.232: a provider that never answers cannot freeze a
/// compaction. Esc abandons it, progress was reported while waiting, and the
/// history is untouched.
#[tokio::test]
async fn cancelling_a_compaction_that_never_gets_an_answer_leaves_history_intact() {
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = Roots::new();
    let provider = LocalProvider::new(true).await;
    let mut cfg = provider.config();
    cfg.keep_recent_tokens = Some(1_000);
    let cli = crate::cli::Cli {
        no_tools: true,
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = long_session(&workspace);
    let before = serde_json::to_value(&session.messages).unwrap();
    let ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace.clone(),
        SecurityMode::Standard,
    );
    let mut app = owned_app(ui);
    let escape = async {
        // Wait for the request to reach the provider, then let a few progress
        // ticks pass before pressing Esc.
        for _ in 0..500 {
            if !provider.calls().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(crate::ui::cancel_test_compaction());
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(app.run_slash_command("/compress"), escape)
    })
    .await
    .expect("the compaction must be abandoned, not wait for the provider");
    result.unwrap();
    let output = app.renderer.captured_output();
    assert!(output.contains("compaction cancelled"), "{output}");
    assert!(output.contains("summarising: request 1 of"), "{output}");
    assert!(output.contains("elapsed (Esc cancels)"), "{output}");
    assert!(app.ui.session.compactions.is_empty());
    assert_eq!(
        serde_json::to_value(&app.ui.session.messages).unwrap(),
        before
    );
    drop(app);
}
