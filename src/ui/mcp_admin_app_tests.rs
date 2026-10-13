use super::*;
use crate::ui::app::test_support::fixture_app;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Every request, including an unintended provider request, goes to this
/// owned loopback server. The listener and all accepted sockets are joined.
struct Server {
    address: std::net::SocketAddr,
    requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    changed: Arc<tokio::sync::Notify>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    accepted: Arc<std::sync::atomic::AtomicUsize>,
    reject_initialization: Arc<std::sync::atomic::AtomicBool>,
    redirect_to: Arc<std::sync::Mutex<Option<String>>>,
    change_source_on_initialize: Arc<std::sync::Mutex<Option<(PathBuf, String)>>>,
}
impl Server {
    async fn new(hold_initialization: bool) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let changed = Arc::new(tokio::sync::Notify::new());
        let (stop, mut stopping) = tokio::sync::oneshot::channel();
        let seen = requests.clone();
        let signal = changed.clone();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepted_count = accepted.clone();
        let reject_initialization = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rejecting = reject_initialization.clone();
        let redirect_to = Arc::new(std::sync::Mutex::new(None::<String>));
        let redirecting = redirect_to.clone();
        let change_source_on_initialize =
            Arc::new(std::sync::Mutex::new(None::<(PathBuf, String)>));
        let changing = change_source_on_initialize.clone();
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopping => break,
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        accepted_count.fetch_add(1,std::sync::atomic::Ordering::AcqRel);
                        let seen = seen.clone();
                        let signal = signal.clone();
                        let rejecting = rejecting.clone();
                        let redirecting = redirecting.clone();
                        let changing = changing.clone();
                        clients.spawn(async move {
                            let mut bytes = Vec::new();
                            let mut chunk = [0u8; 2048];
                            let header_end = loop {
                                let count = stream.read(&mut chunk).await.unwrap();
                                if count == 0 { return; }
                                bytes.extend_from_slice(&chunk[..count]);
                                assert!(bytes.len() <= 128 * 1024);
                                if let Some(end) = bytes.windows(4).position(|row| row == b"\r\n\r\n") { break end + 4; }
                            };
                            let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                            let first = headers.lines().next().unwrap().to_owned();
                            let count: usize = headers.lines().find_map(|line| line.split_once(':').filter(|(name,_)| name.eq_ignore_ascii_case("content-length")).map(|(_,value)| value.trim().parse().unwrap())).unwrap_or(0);
                            assert!(count <= 64 * 1024);
                            while bytes.len() < header_end + count {
                                let read = stream.read(&mut chunk).await.unwrap();
                                if read == 0 { return; }
                                bytes.extend_from_slice(&chunk[..read]);
                                assert!(bytes.len() <= 128 * 1024);
                            }
                            let request = serde_json::from_slice::<serde_json::Value>(&bytes[header_end..header_end+count]).unwrap_or(serde_json::Value::Null);
                            seen.lock().unwrap().push((first.clone(), request.clone()));
                            signal.notify_waiters();
                            let method = request["method"].as_str().unwrap_or("");
                            if hold_initialization && method == "initialize" { std::future::pending::<()>().await; }
                            let redirect = if method == "initialize" { redirecting.lock().unwrap().clone() } else { None };
                            let (status, body) = if redirect.is_some() { ("307 Temporary Redirect", String::new()) }
                            else if !first.starts_with("POST /mcp ") || method == "initialize" && rejecting.swap(false, std::sync::atomic::Ordering::AcqRel) {
                                ("405 Method Not Allowed", String::new())
                            } else if request.get("id").is_none() {
                                ("202 Accepted", String::new())
                            } else {
                                let result = match method {
                                    "initialize" => serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"owned57","version":"1"}}),
                                    "tools/list" => serde_json::json!({"tools":[{"name":"owned_echo","description":"owned test echo","inputSchema":{"type":"object","properties":{}}}]}),
                                    "tools/call" => serde_json::json!({"content":[{"type":"text","text":"actual owned echo"}]}),
                                    _ => serde_json::json!({}),
                                };
                                ("200 OK", serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string())
                            };
                            if method == "initialize"
                                && let Some((path, content)) = changing.lock().unwrap().take()
                            {
                                std::fs::write(path, content).unwrap();
                            }
                            let location = redirect.map(|url| format!("Location: {url}\r\n")).unwrap_or_default();
                            let response = format!("HTTP/1.1 {status}\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                            // A cancelled client is an expected fixture teardown.
                            let _ = stream.write_all(response.as_bytes()).await;
                        });
                    }
                    result = clients.join_next(), if !clients.is_empty() => { result.unwrap().unwrap(); }
                }
            }
            clients.abort_all();
            while let Some(result) = clients.join_next().await {
                if let Err(error) = result {
                    assert!(error.is_cancelled(), "{error}");
                }
            }
        });
        Self {
            address,
            requests,
            changed,
            stop: Some(stop),
            task: Some(task),
            accepted,
            reject_initialization,
            redirect_to,
            change_source_on_initialize,
        }
    }
    fn url(&self) -> String {
        format!("http://{}/mcp", self.address)
    }
    fn method_count(&self, method: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, request)| request["method"] == method)
            .count()
    }
    async fn wait_method(&self, method: &str) {
        self.wait_method_count(method, 1).await;
    }
    async fn wait_method_count(&self, method: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.method_count(method) >= count {
                    break;
                }
                changed.await;
            }
        })
        .await
        .unwrap();
    }
    fn assert_no_provider_request(&self) {
        assert!(
            self.requests
                .lock()
                .unwrap()
                .iter()
                .all(|(first, _)| !first.contains("/v1/")),
            "operator MCP operations must not dispatch a provider request"
        );
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(tokio::net::TcpListener::bind(self.address).await.is_ok());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn config(root: &Path, server: &Server) -> config::Config {
    let global = root.join("config/config.json");
    let raw = serde_json::json!({"session_title_model":"off","update_check":false,"compact":false,"enable_exa_mcp":false,"enable_context7_mcp":false,"enable_grepapp_mcp":false,"mcp_servers":{},"future_unavailable_key":{"preserve":"雪"},"custom_providers":{"mcp57":{"provider_type":"openai","base_url":format!("http://{}/v1",server.address),"api_style":"completions"}}}).to_string();
    crate::fs::private_atomic_create_sync(&global, raw.as_bytes()).unwrap();
    config::parse_config_content(&global, &raw).unwrap()
}
fn environment(root: &Path) -> crate::tests::ScopedProcessEnv {
    environment_with(root, &[])
}
fn environment_with(
    root: &Path,
    additional: &[(&str, Option<std::ffi::OsString>)],
) -> crate::tests::ScopedProcessEnv {
    let mut bindings = vec![
        ("ZS_CONFIG_DIR", Some(root.join("config").into_os_string())),
        ("ZS_DATA_DIR", Some(root.join("data").into_os_string())),
        ("ZS_STATE_DIR", Some(root.join("state").into_os_string())),
        ("ZS_CACHE_DIR", Some(root.join("cache").into_os_string())),
    ];
    bindings.extend_from_slice(additional);
    crate::tests::ScopedProcessEnv::set(&bindings)
}
fn app<'a>(
    root: &Path,
    cli: &'a crate::cli::Cli,
    cfg: &'a config::Config,
    session: &'a mut Session,
    context: &'a mut crate::context::ContextFiles,
) -> App<'a> {
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root).unwrap());
    let client = crate::provider::create_client(
        "mcp57",
        Some("unused-owned-key"),
        &cfg.custom_providers_map(),
        None,
    )
    .unwrap();
    fixture_app(UiContext::new(
        cli,
        cfg,
        session,
        context,
        workspace.clone(),
        client,
        None,
        None,
        crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace),
        None,
    ))
}
async fn user(app: &mut App<'_>, event: UserEvent) {
    assert!(
        crate::startup::heap_future(|| app.handle_user_event(event))
            .await
            .unwrap()
            .is_continue()
    );
}
async fn command(app: &mut App<'_>, value: &str) {
    user(app, UserEvent::Paste(value.into())).await;
    user(
        app,
        UserEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    )
    .await;
}
async fn drain(app: &mut App<'_>) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while app
            .mcp_admin
            .as_ref()
            .is_some_and(|state| state.work.is_some())
        {
            let event = app.user_rx.recv().await.unwrap();
            user(app, event).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn actual_app_auth_unknown_server_is_nonfatal_and_status_is_bounded_without_rpc() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-status-bound");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let mut cfg = config(root.path(), &server);
    for index in 0..140 {
        cfg.mcp_origins.insert(
            format!("server-{index:03}"),
            vec![crate::extras::mcp::project::ServerOrigin {
                source: "captured config".into(),
                selected: false,
                inactive: Some("not trusted".into()),
                digest: None,
            }],
        );
    }
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(&mut app, "/mcp login missing").await;
    command(&mut app, "/mcp logout missing").await;
    command(&mut app, "/mcp").await;
    let rendered = app.renderer.captured_output();
    assert!(rendered.contains("not an OAuth URL server"));
    assert!(rendered.contains("server-000:") && rendered.contains("server-127:"));
    assert!(!rendered.contains("server-128:"));
    assert!(rendered.contains("omitted by the bounded catalogue"));
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(app.mcp_admin.is_none() && app.ui.session.permission_allowlist.is_empty());
    drop(app);
    server.finish().await;
}

#[tokio::test]
async fn actual_app_permission_prompt_refuses_config_mutation_without_answering_ask() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-permission-busy");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    let global = cfg.provenance.global_file.as_ref().unwrap();
    let before = std::fs::read(global).unwrap();
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    let (reply, mut decision) = tokio::sync::oneshot::channel();
    let request = crate::permission::ask::AskRequest {
        tool: "shell".into(),
        input: "literal held native request".into(),
        #[cfg(feature = "acp")]
        tool_call_id: Some("owned-native-request".into()),
        suggested_pattern: None,
        additional_allow_patterns: Vec::new(),
        reply,
    };
    let (asks, mut requests) = mpsc::channel(1);
    asks.send(request).await.unwrap();
    app.run.is_running = true;
    app.begin_permission_request(requests.recv().await.unwrap())
        .unwrap();
    assert!(app.permission_prompt.is_some());
    let add = format!("/mcp add owned {} --allow-local", server.url());
    crate::startup::heap_future(|| app.run_slash_command(&add))
        .await
        .unwrap();
    assert_eq!(std::fs::read(global).unwrap(), before);
    assert!(matches!(
        decision.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(app.permission_prompt.is_some() && app.mcp_admin.is_none());
    assert!(
        app.renderer
            .captured_output()
            .contains("Finish or stop active work")
    );
    app.cancel_pending_permissions();
    assert!(matches!(
        decision.await.unwrap(),
        crate::permission::ask::UserDecision::Deny
    ));
    app.run.is_running = false;
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert!(server.requests.lock().unwrap().is_empty());
    drop(app);
    server.finish().await;
}

/// Uses the production initial factory and its lazy UI caller. Cleanup occurs
/// before the assertion so the old unguarded behavior is a safe controlled red.
#[tokio::test]
async fn actual_initial_mcp_source_revocation_opens_zero_sockets_and_current_source_connects() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-initial-source-proof");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true}}})
        .to_string();
    let project = root.path().join(".mcp.json");
    crate::fs::private_atomic_create_sync(&project, raw.as_bytes()).unwrap();
    config::approve_project_mcp(
        &paths,
        &workspace,
        &crate::extras::mcp::project::digest(&raw),
    )
    .unwrap();
    let cfg = config::mcp_config_in_workspace_at(
        &workspace,
        base.provenance.global_file.as_ref().unwrap(),
    )
    .unwrap();
    assert!(cfg.mcp_servers.as_ref().unwrap().contains_key("owned"));
    // Loaded config must not retain launch authority after its raw source changes.
    std::fs::write(&project, "{\"mcpServers\":{}}").unwrap();
    let stale =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let stale_handles = stale.handles.len();
    stale.shutdown().await;
    let mut lazy = None;
    crate::ui::ensure_mcp_manager(&mut lazy, &cfg, &workspace).await;
    let lazy_handles = lazy.as_ref().unwrap().handles.len();
    lazy.take().unwrap().shutdown().await;
    let rebound_handles = {
        #[cfg(feature = "git-worktree")]
        {
            let mut rebound = None;
            crate::ui::rebind_mcp_manager(&mut rebound, &cfg, &workspace).await;
            let handles = rebound.as_ref().unwrap().handles.len();
            rebound.take().unwrap().shutdown().await;
            handles
        }
        #[cfg(not(feature = "git-worktree"))]
        {
            0
        }
    };
    let stale_sockets = server.accepted.load(std::sync::atomic::Ordering::Acquire);
    let stale_initializations = server.method_count("initialize");
    // Restoring precisely the approved bytes restores authority; a hash-only
    // rejection implementation must not make this positive case pass vacuously.
    std::fs::write(&project, &raw).unwrap();
    let current =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let current_handles = current.handles.len();
    current.shutdown().await;
    let current_initializations = server.method_count("initialize") - stale_initializations;
    server.assert_no_provider_request();
    server.finish().await;
    assert_eq!(
        stale_sockets, 0,
        "revoked initial/lazy config opened sockets"
    );
    assert_eq!(
        (
            stale_handles,
            lazy_handles,
            rebound_handles,
            stale_initializations
        ),
        (0, 0, 0, 0)
    );
    assert_eq!((current_handles, current_initializations), (1, 1));
}

#[tokio::test]
async fn actual_initial_mcp_source_revoked_during_handshake_retires_candidate() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-initial-handshake-source");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true}}})
        .to_string();
    let project = root.path().join(".mcp.json");
    crate::fs::private_atomic_create_sync(&project, raw.as_bytes()).unwrap();
    config::approve_project_mcp(
        &paths,
        &workspace,
        &crate::extras::mcp::project::digest(&raw),
    )
    .unwrap();
    let cfg = config::mcp_config_in_workspace_at(
        &workspace,
        base.provenance.global_file.as_ref().unwrap(),
    )
    .unwrap();
    *server.change_source_on_initialize.lock().unwrap() =
        Some((project, "{\"mcpServers\":{}}".into()));
    let current =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let handles = current.handles.len();
    current.shutdown().await;
    let initializations = server.method_count("initialize");
    server.assert_no_provider_request();
    server.finish().await;
    assert_eq!(initializations, 1, "must reach the real handshake");
    assert_eq!(handles, 0, "revoked candidate must not be adopted");
}

#[tokio::test]
async fn actual_connected_mcp_tool_refuses_revoked_source_before_rpc() {
    use rig::tool::ToolDyn;
    let root = crate::tests::OwnedTestDirectory::new("mcp-call-source-proof");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true}}})
        .to_string();
    let project = root.path().join(".mcp.json");
    crate::fs::private_atomic_create_sync(&project, raw.as_bytes()).unwrap();
    config::approve_project_mcp(
        &paths,
        &workspace,
        &crate::extras::mcp::project::digest(&raw),
    )
    .unwrap();
    let cfg = config::mcp_config_in_workspace_at(
        &workspace,
        base.provenance.global_file.as_ref().unwrap(),
    )
    .unwrap();
    let manager =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let tools = manager
        .collect_tools_with_timeouts(None, None, Default::default())
        .await;
    assert_eq!(tools.len(), 1);
    let positive = tools[0].call("{}".into()).await;
    let positive_calls = server.method_count("tools/call");
    std::fs::write(&project, "{\"mcpServers\":{}}").unwrap();
    let denied = tools[0].call("{}".into()).await;
    let final_calls = server.method_count("tools/call");
    drop(tools);
    manager.shutdown().await;
    server.assert_no_provider_request();
    server.finish().await;
    assert!(positive.unwrap().contains("actual owned echo"));
    assert_eq!(positive_calls, 1, "real current source must dispatch");
    assert!(
        denied
            .unwrap_err()
            .to_string()
            .contains("configuration source changed")
    );
    assert_eq!(
        final_calls, 1,
        "revoked source must dispatch zero additional RPCs"
    );
}

#[tokio::test]
async fn actual_connected_mcp_source_revocation_during_ordinary_ask_refuses_rpc() {
    use rig::tool::ToolDyn;
    let root = crate::tests::OwnedTestDirectory::new("mcp-call-source-ask");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true}}})
        .to_string();
    let project = root.path().join(".mcp.json");
    crate::fs::private_atomic_create_sync(&project, raw.as_bytes()).unwrap();
    config::approve_project_mcp(
        &paths,
        &workspace,
        &crate::extras::mcp::project::digest(&raw),
    )
    .unwrap();
    let cfg = config::mcp_config_in_workspace_at(
        &workspace,
        base.provenance.global_file.as_ref().unwrap(),
    )
    .unwrap();
    let manager =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let permission = Arc::new(std::sync::Mutex::new(
        crate::permission::checker::PermissionChecker::new(
            &crate::permission::PermissionConfigs {
                glob: crate::permission::PermissionConfig {
                    default: Some(crate::permission::Action::Ask),
                    ..Default::default()
                },
                ..Default::default()
            },
            crate::permission::SecurityMode::Standard,
            Some(root.path().to_path_buf()),
            None,
        )
        .unwrap(),
    ));
    let (asks, mut requests) = mpsc::channel(1);
    let tools = manager
        .collect_tools_with_timeouts(Some(permission.clone()), Some(asks), Default::default())
        .await;
    assert_eq!(tools.len(), 1);
    let mut results = Vec::new();
    for revoke in [false, true] {
        let mut call = tools[0].call("{}".into());
        let request = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                result = &mut call => panic!("ordinary Ask must occur before RPC: {result:?}"),
                request = requests.recv() => request.unwrap().into_permission(),
            }
        })
        .await
        .unwrap();
        assert_eq!(request.tool, "mcp_tool");
        assert_eq!(request.input, "mcp_tool:owned:owned_echo");
        assert_eq!(server.method_count("tools/call"), usize::from(revoke));
        if revoke {
            std::fs::write(&project, "{\"mcpServers\":{}}").unwrap();
        }
        request
            .reply
            .send(crate::permission::ask::UserDecision::AllowOnce)
            .unwrap();
        results.push(
            tokio::time::timeout(Duration::from_secs(3), call)
                .await
                .unwrap(),
        );
    }
    let calls = server.method_count("tools/call");
    let no_saved_grant = matches!(
        permission
            .lock()
            .unwrap()
            .check_mcp("mcp_tool:owned:owned_echo", None, "owned_echo"),
        crate::permission::checker::CheckResult::Ask
    );
    drop(tools);
    manager.shutdown().await;
    server.assert_no_provider_request();
    server.finish().await;
    assert!(results.remove(0).unwrap().contains("actual owned echo"));
    assert!(
        results
            .remove(0)
            .unwrap_err()
            .to_string()
            .contains("configuration source changed")
    );
    assert_eq!(
        calls, 1,
        "approval of a revoked invocation cannot send another RPC"
    );
    assert!(no_saved_grant);
}

#[tokio::test]
async fn actual_app_question_overlay_refuses_mcp_edit_without_answering_or_granting() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-question-busy");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    let global = cfg.provenance.global_file.as_ref().unwrap();
    let before = std::fs::read(global).unwrap();
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    let turn = crate::question::QuestionTurn::default();
    let (reply, mut answer) = tokio::sync::oneshot::channel();
    let request = crate::permission::ask::QuestionRequest {
        id: turn.id(),
        questions: crate::question::QuestionBatch(vec![crate::question::Question {
            question: "Which target should the held turn use?".into(),
            options: vec![],
            allow_free_text: true,
        }]),
        #[cfg(feature = "acp")]
        tool_call_id: Some("held-question".into()),
        reply,
    };
    let (sender, mut receiver) = mpsc::channel(1);
    sender
        .send(crate::permission::ask::UserRequest::Question(request))
        .await
        .unwrap();
    app.run.is_running = true;
    app.run.question_turn = Some(turn);
    app.begin_user_request(receiver.recv().await.unwrap())
        .unwrap();
    assert!(app.question_prompt.is_some() && app.permission_prompt.is_none());
    // The retained overlay still needs settlement after the main actor stops.
    // It must obstruct an edit independently of the main-running flag.
    app.run.is_running = false;
    let add = format!("/mcp add owned {} --allow-local", server.url());
    crate::startup::heap_future(|| app.run_slash_command(&add))
        .await
        .unwrap();
    assert_eq!(std::fs::read(global).unwrap(), before);
    assert!(app.question_prompt.is_some() && app.mcp_admin.is_none());
    assert!(matches!(
        answer.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(
        app.renderer
            .captured_output()
            .contains("Finish or stop active work")
    );
    app.cancel_pending_permissions();
    assert!(matches!(
        answer.await.unwrap(),
        crate::question::QuestionAnswer::Cancelled
    ));
    assert!(app.ui.session.permission_allowlist.is_empty());
    app.teardown().await;
    server.finish().await;
}

#[tokio::test]
async fn retained_source_revoked_after_endpoint_lookup_makes_zero_connections() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-source-after-lookup");
    let server = Server::new(false).await;
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let cfg = crate::extras::mcp::config::McpServerConfig::Url {
        url: server.url(),
        allow_local: true,
        headers: Default::default(),
        oauth: None,
    };
    let validations = std::sync::atomic::AtomicUsize::new(0);
    let result = crate::extras::mcp::client::McpClientHandle::connect_in_binding_guarded(
        "owned".into(),
        &cfg,
        &workspace,
        &|| validations.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(validations.load(std::sync::atomic::Ordering::Acquire), 2);
    assert_eq!(
        server.accepted.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert!(server.requests.lock().unwrap().is_empty());
    server.finish().await;
}

#[tokio::test]
async fn actual_app_global_add_catalogue_status_and_remove_are_owned_and_never_call_provider() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-global");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local", server.url()),
    )
    .await;
    drain(&mut app).await;
    assert_eq!(server.method_count("initialize"), 1);
    let manager = app.ui.mcp_manager.as_ref().unwrap();
    assert_eq!(manager.handles.len(), 1);
    let peer = manager.handles[0].peer();
    let tools = manager
        .collect_tools_with_timeouts(
            None,
            None,
            crate::extras::mcp::McpToolTimeouts {
                list: Duration::from_secs(1),
                call: Duration::from_secs(1),
            },
        )
        .await;
    assert_eq!(tools.len(), 1);
    command(&mut app, "/mcp").await;
    assert!(
        app.renderer
            .captured_output()
            .contains("owned: connected; origin")
    );
    assert!(
        app.renderer
            .captured_output()
            .contains("last catalogue attempt")
    );
    // The real Doctor consumer reports retained typed observations; it must
    // not ask either the server or provider for fresh health.
    let requests_before = server.requests.lock().unwrap().len();
    let global_before = std::fs::read(cfg.provenance.global_file.as_ref().unwrap()).unwrap();
    command(&mut app, "/doctor").await;
    let text = app.renderer.captured_output();
    assert!(text.contains("observed connection") && text.contains("owned: Succeeded"));
    assert!(text.contains("observed catalogue"));
    assert!(text.contains("captured MCP origin") && text.contains("owned: selected"));
    assert_eq!(server.requests.lock().unwrap().len(), requests_before);
    assert_eq!(
        std::fs::read(cfg.provenance.global_file.as_ref().unwrap()).unwrap(),
        global_before
    );
    // An actual failed initialization has a typed failed attempt even though
    // the previous successfully initialized handle remains present.
    command(
        &mut app,
        &format!("/mcp add failed {}-bad --allow-local", server.url()),
    )
    .await;
    drain(&mut app).await;
    assert_eq!(app.ui.mcp_manager.as_ref().unwrap().handles.len(), 1);
    let requests_before = server.requests.lock().unwrap().len();
    command(&mut app, "/doctor").await;
    assert!(app.renderer.captured_output().contains("failed: Failed"));
    assert_eq!(server.requests.lock().unwrap().len(), requests_before);
    command(&mut app, "/mcp remove failed").await;
    drain(&mut app).await;
    command(&mut app, "/mcp remove owned").await;
    drain(&mut app).await;
    let manager = app.ui.mcp_manager.as_ref().unwrap();
    assert!(manager.handles.is_empty());
    assert_eq!(
        manager.health_snapshot().servers["owned"]
            .connection
            .unwrap()
            .outcome,
        crate::extras::mcp::health::AttemptOutcome::Removed
    );
    assert!(
        crate::extras::mcp::client::list_all_tools_bounded(&peer, Duration::from_millis(200))
            .await
            .is_err()
    );
    let global = std::fs::read_to_string(cfg.provenance.global_file.as_ref().unwrap()).unwrap();
    assert!(!global.contains(&server.url()) && global.contains("future_unavailable_key"));
    assert!(serde_json::from_str::<serde_json::Value>(&global).is_ok());
    assert!(app.ui.session.permission_allowlist.is_empty());
    assert!(app.run.agent.is_none() && !app.run.is_running && app.run.agent_rx.is_none());
    server.assert_no_provider_request();
    app.retire_mcp_work().await.unwrap();
    drop(app);
    drop(tools);
    drop(peer);
    server.finish().await;
}

#[tokio::test]
async fn actual_explicit_loopback_transport_bypasses_proxy_and_refuses_redirects() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-local-egress");
    let server = Server::new(false).await;
    let trap = Server::new(false).await;
    let proxy = format!("http://{}", trap.address);
    let _environment = environment_with(
        root.path(),
        &[
            ("HTTP_PROXY", Some(proxy.clone().into())),
            ("HTTPS_PROXY", Some(proxy.clone().into())),
            ("ALL_PROXY", Some(proxy.clone().into())),
            ("NO_PROXY", Some("".into())),
            ("http_proxy", Some(proxy.clone().into())),
            ("https_proxy", Some(proxy.clone().into())),
            ("all_proxy", Some(proxy.into())),
            ("no_proxy", Some("".into())),
        ],
    );
    let cfg = config(root.path(), &server);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add direct {} --allow-local", server.url()),
    )
    .await;
    drain(&mut app).await;
    assert_eq!(server.method_count("initialize"), 1);
    assert_eq!(app.ui.mcp_manager.as_ref().unwrap().handles.len(), 1);
    assert_eq!(trap.accepted.load(std::sync::atomic::Ordering::Acquire), 0);
    *server.redirect_to.lock().unwrap() = Some(trap.url());
    command(
        &mut app,
        &format!("/mcp add redirected {} --allow-local", server.url()),
    )
    .await;
    drain(&mut app).await;
    let manager = app.ui.mcp_manager.as_ref().unwrap();
    assert_eq!(manager.handles.len(), 1);
    assert_eq!(
        manager.health_snapshot().servers["redirected"]
            .connection
            .unwrap()
            .outcome,
        crate::extras::mcp::health::AttemptOutcome::Failed
    );
    assert_eq!(trap.accepted.load(std::sync::atomic::Ordering::Acquire), 0);
    server.assert_no_provider_request();
    assert!(app.ui.session.permission_allowlist.is_empty());
    app.retire_mcp_work().await.unwrap();
    let mut handle = app.ui.mcp_manager.as_mut().unwrap().handles.remove(0);
    handle.running_service.close().await.unwrap();
    drop(app);
    server.finish().await;
    trap.finish().await;
}

#[tokio::test]
async fn actual_reconnect_failure_keeps_prior_usable_handle_and_changed_workspace_blocks_launch() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-reconnect");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local", server.url()),
    )
    .await;
    drain(&mut app).await;
    let peer = app.ui.mcp_manager.as_ref().unwrap().handles[0].peer();
    server
        .reject_initialization
        .store(true, std::sync::atomic::Ordering::Release);
    crate::startup::heap_future(|| app.start_mcp_reconnect("owned".into()))
        .await
        .unwrap();
    drain(&mut app).await;
    let manager = app.ui.mcp_manager.as_ref().unwrap();
    assert_eq!(manager.handles.len(), 1);
    assert_eq!(
        manager.health_snapshot().servers["owned"]
            .connection
            .unwrap()
            .outcome,
        crate::extras::mcp::health::AttemptOutcome::Failed
    );
    assert_eq!(
        crate::extras::mcp::client::list_all_tools_bounded(&peer, Duration::from_secs(1))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(server.method_count("initialize"), 2);
    // Preserve the captured file identity but retire this session/workspace
    // actor before its replacement can be installed.
    crate::startup::heap_future(|| app.start_mcp_reconnect("owned".into()))
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        server.wait_method_count("initialize", 3),
    )
    .await
    .unwrap();
    let new_binding = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    app.ui.workspace = new_binding;
    drain(&mut app).await;
    assert_eq!(app.ui.mcp_manager.as_ref().unwrap().handles.len(), 1);
    assert!(
        app.renderer
            .captured_output()
            .contains("no new authority adopted")
    );
    assert_eq!(
        crate::extras::mcp::client::list_all_tools_bounded(&peer, Duration::from_secs(1))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(app.ui.session.permission_allowlist.is_empty() && !app.run.is_running);
    server.assert_no_provider_request();
    app.retire_mcp_work().await.unwrap();
    let mut handle = app.ui.mcp_manager.as_mut().unwrap().handles.remove(0);
    handle.running_service.close().await.unwrap();
    drop(peer);
    drop(app);
    server.finish().await;
}

#[tokio::test]
async fn actual_app_project_decline_keeps_whole_file_inactive_and_stale_approval_cannot_launch() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-project-decline");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local --project", server.url()),
    )
    .await;
    drain(&mut app).await;
    assert!(app.mcp_admin.as_ref().unwrap().approval.is_some());
    assert!(app.renderer.chain_prompt.is_some());
    assert_eq!(server.method_count("initialize"), 0);
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
    )
    .await;
    assert!(app.mcp_admin.as_ref().unwrap().approval.is_none());
    assert_eq!(server.method_count("initialize"), 0);
    let text = std::fs::read_to_string(root.path().join(".mcp.json")).unwrap();
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    assert!(!config::project_mcp_is_trusted(
        &paths,
        &app.ui.workspace,
        &text
    ));
    command(&mut app, "/mcp").await;
    assert!(
        app.renderer
            .captured_output()
            .contains("inactive: project content is not trusted")
    );
    command(&mut app, "/mcp remove owned --project").await;
    drain(&mut app).await;
    assert!(app.mcp_admin.as_ref().unwrap().approval.is_some());
    std::fs::write(root.path().join(".mcp.json"), text).unwrap();
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
    )
    .await;
    assert!(
        app.renderer.captured_output().contains("approval is stale"),
        "{}",
        app.renderer.captured_output()
    );
    assert_eq!(server.method_count("initialize"), 0);
    assert!(app.ui.session.permission_allowlist.is_empty());
    server.assert_no_provider_request();
    app.retire_mcp_work().await.unwrap();
    drop(app);
    server.finish().await;
}

#[tokio::test]
async fn actual_app_project_whole_file_approval_connects_only_requested_server_and_rejects_wrong_remove_source()
 {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-project-approve");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    let existing=serde_json::json!({"outside":{"preserve":"unknown"},"mcpServers":{"other":{"url":format!("http://{}/other",server.address),"allow_local":true,"headers":{"Authorization":"REDACT_THIS_SECRET"}}}}).to_string();
    crate::fs::private_atomic_create_sync(&root.path().join(".mcp.json"), existing.as_bytes())
        .unwrap();
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local --project", server.url()),
    )
    .await;
    drain(&mut app).await;
    assert_eq!(server.method_count("initialize"), 0);
    assert!(app.renderer.captured_output().contains("other"));
    assert!(
        !app.renderer
            .captured_output()
            .contains("REDACT_THIS_SECRET")
    );
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
    )
    .await;
    drain(&mut app).await;
    assert_eq!(server.method_count("initialize"), 1);
    assert_eq!(app.ui.mcp_manager.as_ref().unwrap().handles.len(), 1);
    assert_eq!(
        app.ui.mcp_manager.as_ref().unwrap().handles[0].server_name,
        "owned"
    );
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(first, _)| !first.contains("/other")),
        "reviewed whole-file trust must not silently connect a second server during this add"
    );
    let text = std::fs::read_to_string(root.path().join(".mcp.json")).unwrap();
    assert!(
        text.contains("outside") && text.contains("REDACT_THIS_SECRET"),
        "raw placeholders/fields are preserved on disk; the operator review is redacted"
    );
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    assert!(config::project_mcp_is_trusted(
        &paths,
        &app.ui.workspace,
        &text
    ));
    command(&mut app, "/mcp remove owned").await;
    assert!(
        app.renderer
            .captured_output()
            .contains("removal source does not own")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join(".mcp.json")).unwrap(),
        text
    );
    assert_eq!(app.ui.mcp_manager.as_ref().unwrap().handles.len(), 1);
    command(&mut app, "/mcp remove owned --project").await;
    drain(&mut app).await;
    assert!(app.ui.mcp_manager.as_ref().unwrap().handles.is_empty());
    assert!(app.mcp_admin.as_ref().unwrap().approval.is_some());
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
    )
    .await;
    assert!(app.ui.session.permission_allowlist.is_empty());
    server.assert_no_provider_request();
    app.retire_mcp_work().await.unwrap();
    drop(app);
    server.finish().await;
}

#[tokio::test]
async fn actual_app_stale_initialized_candidate_is_closed_without_adoption() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-stale-result");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local", server.url()),
    )
    .await;
    // First notification publishes the structural edit and starts its reconnect.
    let prepared = tokio::time::timeout(Duration::from_secs(3), app.user_rx.recv())
        .await
        .unwrap()
        .unwrap();
    user(&mut app, prepared).await;
    let connected = tokio::time::timeout(Duration::from_secs(3), app.user_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.method_count("initialize"), 1);
    app.ui.session.id = "different-owned-session".into();
    user(&mut app, connected).await;
    drain(&mut app).await;
    assert!(app.ui.mcp_manager.as_ref().unwrap().handles.is_empty());
    assert!(
        app.renderer
            .captured_output()
            .contains("no new authority adopted")
    );
    assert_eq!(
        app.ui
            .mcp_manager
            .as_ref()
            .unwrap()
            .health_snapshot()
            .servers["owned"]
            .connection
            .unwrap()
            .outcome,
        crate::extras::mcp::health::AttemptOutcome::Cancelled
    );
    assert!(app.ui.session.permission_allowlist.is_empty());
    server.assert_no_provider_request();
    app.retire_mcp_work().await.unwrap();
    drop(app);
    server.finish().await;
}

#[tokio::test]
async fn actual_app_cancel_during_http_initialization_joins_work_and_preserves_saved_entry() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-cancel-init");
    let _environment = environment(root.path());
    let server = Server::new(true).await;
    let cfg = config(root.path(), &server);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local", server.url()),
    )
    .await;
    let prepared = tokio::time::timeout(Duration::from_secs(3), app.user_rx.recv())
        .await
        .unwrap()
        .unwrap();
    user(&mut app, prepared).await;
    server.wait_method("initialize").await;
    user(
        &mut app,
        UserEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    )
    .await;
    assert!(app.mcp_admin.as_ref().unwrap().work.is_none());
    assert!(app.ui.mcp_manager.as_ref().unwrap().handles.is_empty());
    assert!(
        std::fs::read_to_string(cfg.provenance.global_file.as_ref().unwrap())
            .unwrap()
            .contains(&server.url())
    );
    assert!(app.ui.session.permission_allowlist.is_empty());
    server.assert_no_provider_request();
    app.retire_mcp_work().await.unwrap();
    drop(app);
    server.finish().await;
}

#[tokio::test]
async fn actual_app_teardown_joins_held_mcp_initialization_before_service_shutdown() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-app-teardown-init");
    let _environment = environment(root.path());
    let server = Server::new(true).await;
    let cfg = config(root.path(), &server);
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local", server.url()),
    )
    .await;
    let prepared = tokio::time::timeout(Duration::from_secs(3), app.user_rx.recv())
        .await
        .unwrap()
        .unwrap();
    user(&mut app, prepared).await;
    server.wait_method("initialize").await;
    let work = app.mcp_admin.as_ref().unwrap().work.as_ref().unwrap();
    let scope = work.scope.clone();
    let task = work.task.as_ref().unwrap().abort_handle();
    assert!(
        !task.is_finished(),
        "actual initialization is held by the fixture"
    );
    assert!(app.ui.mcp_manager.as_ref().unwrap().handles.is_empty());
    let persisted = std::fs::read_to_string(cfg.provenance.global_file.as_ref().unwrap()).unwrap();
    assert!(persisted.contains(&server.url()));
    tokio::time::timeout(Duration::from_secs(6), app.teardown())
        .await
        .expect("teardown must settle its exact MCP actor");
    assert!(
        task.is_finished(),
        "teardown returned only after the captured task joined"
    );
    tokio::time::timeout(Duration::from_secs(1), scope.wait_idle())
        .await
        .expect("teardown also settles captured child work");
    assert_eq!(server.method_count("initialize"), 1);
    assert_eq!(server.method_count("tools/list"), 0);
    assert!(session.permission_allowlist.is_empty());
    assert_eq!(
        std::fs::read_to_string(cfg.provenance.global_file.as_ref().unwrap()).unwrap(),
        persisted
    );
    server.assert_no_provider_request();
    server.finish().await;
}

#[tokio::test]
async fn actual_doctor_tracks_live_add_remove_catalogue_without_reloading_or_rpc() {
    let root = crate::tests::OwnedTestDirectory::new("mcp-doctor-current-catalogue");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let cfg = config(root.path(), &server);
    assert!(
        cfg.mcp_servers
            .as_ref()
            .is_none_or(|servers| servers.is_empty())
    );
    let cli = crate::cli::Cli {
        no_session: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load(true);
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    command(
        &mut app,
        &format!("/mcp add owned {} --allow-local", server.url()),
    )
    .await;
    drain(&mut app).await;
    assert_eq!(
        app.ui.mcp_manager.as_ref().unwrap().configured_count(),
        Some(1)
    );
    assert!(
        app.ui
            .cfg
            .mcp_servers
            .as_ref()
            .is_none_or(|servers| servers.is_empty()),
        "startup provider/budget/config authority must not be replaced for diagnostics"
    );
    for (remove, expected) in [(false, 1), (true, 0)] {
        if remove {
            command(&mut app, "/mcp remove owned").await;
            drain(&mut app).await;
        }
        assert_eq!(
            app.ui.mcp_manager.as_ref().unwrap().configured_count(),
            Some(expected)
        );
        let global = cfg.provenance.global_file.as_ref().unwrap();
        let before = std::fs::read(global).unwrap();
        // A diagnostic must use captured state even when a later file revision
        // is invalid. Reloading would fail or invent a new effective catalogue.
        std::fs::write(global, b"not valid config JSON").unwrap();
        let diagnostic_source = std::fs::read(global).unwrap();
        let requests = server.requests.lock().unwrap().len();
        let prefix = app.renderer.captured_output().len();
        command(&mut app, "/doctor").await;
        let rendered = app.renderer.captured_output();
        let current = &rendered[prefix..];
        let count = current.rsplit("MCP configured servers:").next().unwrap();
        assert!(
            count.starts_with(&format!(" {expected}")),
            "actual Doctor must use the live catalogue: {count}"
        );
        assert!(current.contains("MCP connection metadata (no liveness RPC)"));
        if !remove {
            assert!(current.contains("captured MCP origin") && current.contains("owned: selected"));
        }
        assert_eq!(
            server.requests.lock().unwrap().len(),
            requests,
            "Doctor must not query the server or provider"
        );
        assert_eq!(
            std::fs::read(global).unwrap(),
            diagnostic_source,
            "Doctor must not publish or reload configuration"
        );
        std::fs::write(global, before).unwrap();
        assert!(
            app.ui
                .cfg
                .mcp_servers
                .as_ref()
                .is_none_or(|servers| servers.is_empty())
        );
    }
    assert_eq!(server.method_count("initialize"), 1);
    assert_eq!(server.method_count("tools/list"), 0);
    assert!(app.ui.session.permission_allowlist.is_empty());
    app.teardown().await;
    server.finish().await;
}
