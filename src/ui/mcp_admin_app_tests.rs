use super::*;
use crate::ui::app::test_support::fixture_app;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// True when descriptor `fd` is still a socket bound to `address`. A
/// closed descriptor, or one a parallel test reused for something else,
/// is not: unlike re-binding the released ephemeral port, this cannot be
/// defeated by another test taking the port.
#[cfg(unix)]
#[allow(unsafe_code)]
fn fd_is_socket_bound_to(fd: std::os::fd::RawFd, address: std::net::SocketAddr) -> bool {
    // SAFETY: getsockname only fills a local buffer for the integer fd; it
    // never owns, closes or mutates the descriptor.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockname(
            fd,
            std::ptr::addr_of_mut!(storage).cast::<libc::sockaddr>(),
            &mut length,
        )
    };
    if status != 0 || libc::c_int::from(storage.ss_family) != libc::AF_INET {
        return false;
    }
    // SAFETY: AF_INET storage is a sockaddr_in.
    let inet = unsafe { &*std::ptr::addr_of!(storage).cast::<libc::sockaddr_in>() };
    u16::from_be(inet.sin_port) == address.port()
        && std::net::Ipv4Addr::from(u32::from_be(inet.sin_addr.s_addr)) == address.ip()
}

/// Every request, including an unintended provider request, goes to this
/// owned loopback server. The listener and all accepted sockets are joined.
struct Server {
    address: std::net::SocketAddr,
    provider_streams: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    catalogue_count: Arc<std::sync::atomic::AtomicUsize>,
    anthropic_prefix: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    native_raw_prefixes: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    changed: Arc<tokio::sync::Notify>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    #[cfg(unix)]
    listener_fd: std::os::fd::RawFd,
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
        #[cfg(unix)]
        let listener_fd = std::os::fd::AsRawFd::as_raw_fd(&listener);
        let catalogue_count = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let counted_catalogue = catalogue_count.clone();
        let provider_streams = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::<String>::new(),
        ));
        let scripted_streams = provider_streams.clone();
        let anthropic_prefix = Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
        let expected_prefix = anthropic_prefix.clone();
        let native_raw_prefixes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_raw_prefixes = native_raw_prefixes.clone();
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
                        let scripted_streams = scripted_streams.clone();
                        let expected_prefix = expected_prefix.clone();
                        let captured_raw_prefixes = captured_raw_prefixes.clone();
                        let counted_catalogue = counted_catalogue.clone();
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
                            let anthropic_request = first.starts_with("POST /v1/messages ");
                            if anthropic_request {
                                #[derive(serde::Deserialize)]
                                struct RawPrefix<'a> {
                                    #[serde(borrow)] system: &'a serde_json::value::RawValue,
                                    #[serde(borrow)] tools: &'a serde_json::value::RawValue,
                                }
                                let prefix: RawPrefix<'_> = serde_json::from_slice(&bytes[header_end..header_end+count]).unwrap();
                                captured_raw_prefixes.lock().unwrap().push((prefix.system.get().into(),prefix.tools.get().into()));
                            }
                            let provider_request = first.starts_with("POST /v1/chat/completions ") || anthropic_request;
                            let (status, body) = if anthropic_request {
                                let mut prefix = expected_prefix.lock().unwrap();
                                let current = serde_json::json!({"system":request["system"],"tools":request["tools"]});
                                if prefix.is_none() {
                                    *prefix = Some(current);
                                    ("200 OK",scripted_streams.lock().unwrap().pop_front().expect("owned Anthropic first response"))
                                } else if prefix.as_ref() != Some(&current) {
                                    // Deterministic documented-prefix witness, not
                                    // cryptographic verification or a paid server.
                                    ("400 Bad Request",serde_json::json!({"type":"error","error":{"type":"invalid_request_error","message":"owned-native-prefix-change before replayed thinking"}}).to_string())
                                } else {
                                    ("200 OK",scripted_streams.lock().unwrap().pop_front().expect("bounded unchanged-prefix native response"))
                                }
                            } else if provider_request {
                                ("200 OK", scripted_streams.lock().unwrap().pop_front().expect("every owned provider request must have a bounded scripted response"))
                            } else if redirect.is_some() { ("307 Temporary Redirect", String::new()) }
                            else if !first.starts_with("POST /mcp ") || method == "initialize" && rejecting.swap(false, std::sync::atomic::Ordering::AcqRel) {
                                ("405 Method Not Allowed", String::new())
                            } else if request.get("id").is_none() {
                                ("202 Accepted", String::new())
                            } else {
                                let result = match method {
                                    "initialize" => serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"owned57","version":"1"}}),
                                    "tools/list" => {
                                        let count = counted_catalogue.load(std::sync::atomic::Ordering::Acquire);
                                        assert!(count <= 128);
                                        if count == 1 { serde_json::json!({"tools":[{"name":"owned_echo","description":"owned test echo","inputSchema":{"type":"object","properties":{}}}]}) }
                                        else { serde_json::json!({"tools": (0..count).map(|index| serde_json::json!({"name":format!("owned_{index:02}"),"description":"captured description ".repeat(64),"inputSchema":{"type":"object","properties":{"value":{"type":"string","description":"captured schema ".repeat(64)}}}})).collect::<Vec<_>>()}) }
                                    },
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
                            let content_type = if provider_request { "text/event-stream" } else { "application/json" };
                            let response = format!("HTTP/1.1 {status}\r\n{location}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
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
            #[cfg(unix)]
            listener_fd,
            provider_streams,
            catalogue_count,
            anthropic_prefix,
            native_raw_prefixes,
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
        #[cfg(unix)]
        assert!(
            !fd_is_socket_bound_to(self.listener_fd, self.address),
            "server listener must be released"
        );
        #[cfg(not(unix))]
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
        cfg.custom_providers_map(),
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
                authority: None,
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
        diagnostic: None,
        #[cfg(feature = "subagents")]
        background: None,
        agent_name: None,
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

#[tokio::test]
async fn config69_actual_cli_mcp_keeps_original_file_gate_before_connect_and_rpc() {
    use rig::tool::ToolDyn;
    let root = crate::tests::OwnedTestDirectory::new("config69-cli-mcp-wire");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let global = base.provenance.global_file.as_ref().unwrap();
    let mut raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(global).unwrap()).unwrap();
    raw["mcp_servers"] = serde_json::json!({"owned":{"url":server.url(),"allow_local":true,"headers":{"Authorization":"ORIGINAL_SECRET"}}});
    crate::config::atomic_config_write(global, &raw.to_string()).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut cfg = config::mcp_config_in_workspace_at(&workspace, global).unwrap();
    let overrides = config::layers::parse_overrides(&[
        "mcp_servers.owned.headers.Authorization='CLI_SECRET'".into(),
    ])
    .unwrap();
    config::layers::apply_overrides(&mut cfg, &overrides).unwrap();
    let manager =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let tools = manager
        .collect_tools_with_timeouts(None, None, Default::default())
        .await;
    assert_eq!(tools.len(), 1);
    let positive = tools[0].call("{}".into()).await;
    let first_calls = server.method_count("tools/call");
    // Valid JSON with identical effective settings but changed original bytes
    // must revoke both the retained tool and a newly attempted connection.
    std::fs::write(global, format!("{}\n", raw)).unwrap();
    let denied = tools[0].call("{}".into()).await;
    let final_calls = server.method_count("tools/call");
    let before_sockets = server.accepted.load(std::sync::atomic::Ordering::Acquire);
    let stale =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let stale_handles = stale.handles.len();
    stale.shutdown().await;
    let after_sockets = server.accepted.load(std::sync::atomic::Ordering::Acquire);
    drop(tools);
    manager.shutdown().await;
    server.assert_no_provider_request();
    server.finish().await;
    assert!(positive.unwrap().contains("actual owned echo"));
    assert_eq!((first_calls, final_calls), (1, 1));
    assert!(
        denied
            .unwrap_err()
            .to_string()
            .contains("configuration source changed")
    );
    assert_eq!((stale_handles, after_sockets - before_sockets), (0, 0));
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
async fn actual_retired_mcp_connection_cannot_dispatch_or_prompt_again() {
    use rig::tool::ToolDyn;
    let root = crate::tests::OwnedTestDirectory::new("mcp-retired-generation");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true}}})
        .to_string();
    crate::fs::private_atomic_create_sync(&root.path().join(".mcp.json"), raw.as_bytes()).unwrap();
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
    let (ask, mut questions) = tokio::sync::mpsc::channel(4);
    let tools = manager
        .collect_tools_with_timeouts(Some(permission.clone()), Some(ask), Default::default())
        .await;
    assert_eq!(tools.len(), 1);
    let mut call = tools[0].call("{}".into());
    let request = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut call => panic!("current connection must request ordinary Ask before RPC: {result:?}"),
            request = questions.recv() => request.unwrap().into_permission(),
        }
    }).await.unwrap();
    assert_eq!(request.tool, "mcp_tool");
    assert_eq!(request.input, "mcp_tool:owned:owned_echo");
    request
        .reply
        .send(crate::permission::ask::UserDecision::AllowOnce)
        .unwrap();
    let positive = tokio::time::timeout(Duration::from_secs(3), call)
        .await
        .unwrap();
    let calls = server.method_count("tools/call");
    manager.shutdown().await;
    let stale = tokio::time::timeout(Duration::from_secs(3), tools[0].call("{}".into())).await;
    let final_calls = server.method_count("tools/call");
    let no_prompt = questions.try_recv().is_err();
    let no_grant = matches!(
        permission
            .lock()
            .unwrap()
            .check_mcp("mcp_tool:owned:owned_echo", None, "owned_echo"),
        crate::permission::checker::CheckResult::Ask
    );
    drop(tools);
    server.assert_no_provider_request();
    server.finish().await;
    assert!(positive.unwrap().contains("actual owned echo"));
    assert_eq!(calls, 1);
    assert!(
        stale
            .expect("retired call must resolve without waiting for an answer")
            .unwrap_err()
            .to_string()
            .contains("connection retired")
    );
    assert_eq!(
        final_calls, calls,
        "retired exact connection must make zero extra RPCs"
    );
    assert!(no_prompt);
    assert!(no_grant);
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
        defer: Default::default(),
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
        // /doctor paints its report as one frame and the renderer repaints
        // only changed rows, so read the transcript the report was written to.
        let prefix = app.renderer.transcript_text().len();
        command(&mut app, "/doctor").await;
        let rendered = app.renderer.transcript_text();
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

/// Exercise production builder wrappers and the SDK's captured per-request
/// allowed-name set. All MCP effects reach the owned real transport; the model
/// carrier captures complete actual CompletionRequests without external I/O.
#[tokio::test]
async fn actual_deferred_builder_next_request_advertises_schema_but_same_response_cannot_call_hidden_tool()
 {
    use crate::tests::fake_model::{MockCompletionModel, MockStreamEvent};
    use futures::FutureExt;
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    for same_response in [false, true] {
        let root = crate::tests::OwnedTestDirectory::new("mcp-deferred-requests");
        let _environment = environment(root.path());
        let server = Server::new(false).await;
        let base = config(root.path(), &server);
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
        let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true,"defer":true}}}).to_string();
        crate::fs::private_atomic_create_sync(&root.path().join(".mcp.json"), raw.as_bytes())
            .unwrap();
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
        let (ask, mut questions) = tokio::sync::mpsc::channel(4);
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["mcp_search".into(), "owned_echo".into()],
            ..Default::default()
        };
        let context = crate::context::load_for_workspace(true, Some(root.path()));
        let mut initial = vec![MockStreamEvent::tool_call(
            "search-provider-id",
            "mcp_search",
            serde_json::json!({"query":"owned_echo"}),
        )];
        if same_response {
            initial.push(MockStreamEvent::tool_call(
                "hidden-provider-id",
                "owned_echo",
                serde_json::json!({}),
            ));
        }
        initial.push(MockStreamEvent::final_response_with_default_usage());
        let mut turns = vec![initial];
        if !same_response {
            turns.push(vec![
                MockStreamEvent::tool_call(
                    "effect-provider-id",
                    "owned_echo",
                    serde_json::json!({}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ]);
        }
        turns.push(vec![
            MockStreamEvent::text("owned terminal"),
            MockStreamEvent::final_response_with_default_usage(),
        ]);
        let model = MockCompletionModel::from_stream_turns(turns);
        let agent = crate::agent::builder::build_agent_inner(
            model.clone(),
            &cli,
            &cfg,
            &context,
            workspace.clone(),
            Some(permission.clone()),
            Some(ask),
            crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace),
            crate::agent::tools::ReadTracker::new(true),
            crate::agent::tools::TodoStore::default(),
            #[cfg(feature = "goal")]
            crate::extras::goal::GoalStore::default(),
            "owned-deferred-session",
            None,
            false,
            None,
            None,
            #[cfg(feature = "js")]
            crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
                backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
                assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
                reason: "MCP-only fixture does not start a JS worker".into(),
            },
            #[cfg(feature = "js")]
            crate::extras::js::session::ScratchStore::default(),
            #[cfg(feature = "skills")]
            None,
            Some(&manager),
        )
        .await;
        let scope = crate::agent::runner::AgentWorkScope::new();
        let assertions = std::panic::AssertUnwindSafe(async {
            let retry = crate::retry::RetryConfig::default();
            let mut turn = Box::pin(scope.run(crate::agent::runner::run_print(&agent, "discover then call", false, &retry, None, Vec::new(), #[cfg(feature = "hooks")] None)));
            if !same_response {
                let request = tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::select! {
                        result = &mut turn => panic!("activated real tool must still require ordinary Ask: {result:?}"),
                        request = questions.recv() => request.unwrap().into_permission(),
                    }
                }).await.unwrap();
                assert_eq!(request.tool, "mcp_tool");
                assert_eq!(request.input, "mcp_tool:owned:owned_echo");
                assert_eq!(server.method_count("tools/call"), 0);
                let requests = model.requests();
                assert_eq!(requests.len(), 2, "the effect must occur only after the next actual model request");
                assert_eq!(requests[0].tools.iter().map(|definition| definition.name.as_str()).collect::<Vec<_>>(), ["mcp_search"]);
                assert_eq!(requests[1].tools.iter().map(|definition| definition.name.as_str()).collect::<std::collections::BTreeSet<_>>(), std::collections::BTreeSet::from(["mcp_search", "owned_echo"]));
                let schema = requests[1].tools.iter().find(|definition| definition.name == "owned_echo").unwrap();
                assert_eq!(schema.parameters, serde_json::json!({"type":"object","properties":{}}));
                request.reply.send(crate::permission::ask::UserDecision::AllowOnce).unwrap();
            }
            let result = tokio::time::timeout(Duration::from_secs(3), &mut turn).await.unwrap();
            if !same_response { assert_eq!(result.unwrap().0, "owned terminal"); }
            let requests = model.requests();
            assert_eq!(requests[0].tools.iter().map(|definition| definition.name.as_str()).collect::<Vec<_>>(), ["mcp_search"]);
            assert_eq!(server.method_count("tools/call"), usize::from(!same_response));
            assert!(questions.try_recv().is_err(), "same-response hidden tool must neither prompt nor send RPC");
            assert!(matches!(permission.lock().unwrap().check_mcp("mcp_tool:owned:owned_echo", None, "owned_echo"), crate::permission::checker::CheckResult::Ask));
            if same_response {
                assert!(agent.tool_server_handle.get_tool_defs(None).await.unwrap().iter().all(|definition| definition.name == "mcp_search"), "an abandoned response must not execute its search sibling");
            }
        }).catch_unwind().await;
        scope.cancellation_handle().cancel();
        tokio::time::timeout(Duration::from_secs(3), scope.wait_idle())
            .await
            .expect("owned test work must settle even after an assertion failure");
        drop(agent);
        manager.shutdown().await;
        server.assert_no_provider_request();
        server.finish().await;
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
    }
}

/// Native OpenAI converter and the production MCP wrappers share the same
/// serialized request proof. The only network peer is this owned fixture.
#[tokio::test]
async fn actual_native_provider_wire_defers_schema_until_next_request_and_rejects_same_response_hidden_call()
 {
    for same_response in [false, true] {
        native_deferred_wire_case(same_response, None, false).await;
    }
}

#[cfg(all(feature = "hooks", unix))]
#[tokio::test]
async fn actual_activated_mcp_native_wire_preserves_hook_rewrite_redaction_and_denial() {
    for hook in ["rewrite", "deny"] {
        native_deferred_wire_case(false, Some(hook), false).await;
    }
}

#[tokio::test]
async fn actual_activated_mcp_native_wire_preserves_canonical_cost_cap_before_effect() {
    native_deferred_wire_case(false, None, true).await;
}

async fn native_deferred_wire_case(same_response: bool, hook: Option<&str>, cost_cap: bool) {
    use futures::FutureExt;
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    fn completion(tool_calls: Vec<serde_json::Value>, text: Option<&str>) -> String {
        let delta = match text {
            Some(text) => serde_json::json!({"role":"assistant","content":text}),
            None => serde_json::json!({"role":"assistant","tool_calls":tool_calls}),
        };
        let first = serde_json::json!({"id":"owned-model-response","object":"chat.completion.chunk","created":0,"model":"owned-model","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
        let end = serde_json::json!({"id":"owned-model-response","object":"chat.completion.chunk","created":0,"model":"owned-model","choices":[{"index":0,"delta":{},"finish_reason":if text.is_some(){"stop"}else{"tool_calls"}}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
        format!("data: {first}\n\ndata: {end}\n\ndata: [DONE]\n\n")
    }
    fn call(index: usize, id: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
    }
    {
        let root = crate::tests::OwnedTestDirectory::new("mcp-deferred-native-wire");
        let _environment = environment(root.path());
        #[cfg(all(feature = "hooks", unix))]
        if let Some(mode) = hook {
            use crate::extras::hooks::settings::{HookGroup, HookHandler, HookTrust};
            let handler = |text: &str| HookHandler {
                kind: "command".into(),
                command: Some("/bin/sh".into()),
                args: Some(vec!["-c".into(), text.into()]),
                timeout: Some(3),
                is_async: false,
                condition: None,
                once: false,
                trust: HookTrust::Trusted,
                env: Default::default(),
            };
            let pre = if mode == "deny" {
                r#"printf '%s\n' '{"permissionDecision":"deny","reason":"owned deferred hook denial"}'"#
            } else {
                r#"printf '%s\n' '{"updatedInput":{"owned_hook_argument":"actual"}}'"#
            };
            let hooks = std::collections::HashMap::from([
                (
                    "PreToolUse".into(),
                    vec![HookGroup {
                        matcher: Some("mcp__owned__owned_echo".into()),
                        hooks: vec![handler(pre)],
                    }],
                ),
                (
                    "PostToolUse".into(),
                    vec![HookGroup {
                        matcher: Some("mcp__owned__owned_echo".into()),
                        hooks: vec![handler(
                            r#"printf '%s\n' '{"redactions":["actual owned echo"]}'"#,
                        )],
                    }],
                ),
            ]);
            crate::extras::hooks::init_dispatcher(
                crate::extras::hooks::dispatcher::HookDispatcher::from_config_with_backend_and_root(&hooks,"none",root.path()).unwrap()
            );
        }
        let denied_by_hook = hook == Some("deny");
        let server = Server::new(false).await;
        let base = config(root.path(), &server);
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
        let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true,"defer":true}}}).to_string();
        crate::fs::private_atomic_create_sync(&root.path().join(".mcp.json"), raw.as_bytes())
            .unwrap();
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
        let (ask, mut questions) = tokio::sync::mpsc::channel(4);
        let mut initial = vec![call(
            0,
            "wire-search",
            "mcp_search",
            serde_json::json!({"query":"owned_echo"}),
        )];
        if same_response {
            initial.push(call(1, "wire-hidden", "owned_echo", serde_json::json!({})));
        }
        server
            .provider_streams
            .lock()
            .unwrap()
            .push_back(completion(initial, None));
        if !same_response {
            server
                .provider_streams
                .lock()
                .unwrap()
                .push_back(completion(
                    vec![call(0, "wire-effect", "owned_echo", serde_json::json!({}))],
                    None,
                ));
            if !cost_cap {
                server
                    .provider_streams
                    .lock()
                    .unwrap()
                    .push_back(completion(Vec::new(), Some("native owned terminal")));
            }
        }
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["mcp_search".into(), "owned_echo".into()],
            ..Default::default()
        };
        let context = crate::context::load_for_workspace(true, Some(root.path()));
        let client = crate::provider::create_client(
            "mcp57",
            Some("owned-inert-key"),
            cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        let crate::provider::AnyModel::OpenAI(crate::provider::OpenAiModel::Completions(model)) =
            client.completion_model("owned-model")
        else {
            panic!("owned fixture must use the native OpenAI completions converter");
        };
        let agent = crate::agent::builder::build_agent_inner(
            model,
            &cli,
            &cfg,
            &context,
            workspace.clone(),
            Some(permission.clone()),
            Some(ask),
            crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace),
            crate::agent::tools::ReadTracker::new(true),
            crate::agent::tools::TodoStore::default(),
            #[cfg(feature = "goal")]
            crate::extras::goal::GoalStore::default(),
            "owned-native-deferred-session",
            None,
            false,
            None,
            None,
            #[cfg(feature = "js")]
            crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
                backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
                assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
                reason: "MCP-only fixture does not start a JS worker".into(),
            },
            #[cfg(feature = "js")]
            crate::extras::js::session::ScratchStore::default(),
            #[cfg(feature = "skills")]
            None,
            Some(&manager),
        )
        .await;
        let mut diagnostic_context = context.clone();
        let mut diagnostic_session = Session::new(
            "mcp57",
            "owned-model",
            128_000,
            root.path().to_str().unwrap(),
        );
        let mut diagnostic_app = app(
            root.path(),
            &cli,
            &cfg,
            &mut diagnostic_session,
            &mut diagnostic_context,
        );
        diagnostic_app.ui.mcp_manager = Some(manager);
        let scope = crate::agent::runner::AgentWorkScope::new();
        let ledger = Arc::new(crate::spend::Ledger::new(&root.path().join("state")));
        let spend = cost_cap.then(|| {
            crate::spend::run::Run::fixture(
                crate::spend::Account::new("owned-discovery-cap", Some(14.0), 0.0).unwrap(),
                ledger.clone(),
                "owned-discovery-cap",
                crate::spend::PriceSnapshot::capture(
                    Some((1_000_000.0, 0.0)),
                    Default::default(),
                    false,
                ),
            )
        });
        #[cfg(feature = "hooks")]
        scope.set_hook_execution_root(root.path());
        let assertions = std::panic::AssertUnwindSafe(async {
            let retry = crate::retry::RetryConfig::default();
            let mut turn = Box::pin(crate::spend::run::scope(spend.clone(),scope.run(crate::agent::runner::run_print(&agent, "discover then call", false, &retry, None, Vec::new(), #[cfg(feature = "hooks")] None))));
            if !same_response && !denied_by_hook && !cost_cap {
                let request = tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::select! {
                        result = &mut turn => panic!("native activated tool still needs ordinary Ask: {result:?}"),
                        request = questions.recv() => request.unwrap().into_permission(),
                    }
                }).await.unwrap();
                assert_eq!(request.tool, "mcp_tool");
                assert_eq!(request.input, "mcp_tool:owned:owned_echo");
                assert_eq!(server.method_count("tools/call"), 0);
                let wire = server.requests.lock().unwrap().iter().filter(|(route,_)| route.starts_with("POST /v1/chat/completions ")).map(|(_,body)|body.clone()).collect::<Vec<_>>();
                assert_eq!(wire.len(),2);
                assert_eq!(wire[0]["tools"].as_array().unwrap().iter().map(|tool|tool["function"]["name"].as_str().unwrap()).collect::<Vec<_>>(),["mcp_search"]);
                let names = wire[1]["tools"].as_array().unwrap().iter().map(|tool|tool["function"]["name"].as_str().unwrap()).collect::<std::collections::BTreeSet<_>>();
                assert_eq!(names,std::collections::BTreeSet::from(["mcp_search","owned_echo"]));
                let echo = wire[1]["tools"].as_array().unwrap().iter().find(|tool|tool["function"]["name"]=="owned_echo").unwrap();
                assert_eq!(echo["function"]["parameters"],serde_json::json!({"type":"object","properties":{}}));
                request.reply.send(crate::permission::ask::UserDecision::AllowOnce).unwrap();
            }
            let result = tokio::time::timeout(Duration::from_secs(3), &mut turn).await.unwrap();
            if same_response {
                assert!(result.expect_err("unadvertised hidden wire call must be rejected").to_string().contains("owned_echo"));
            } else if cost_cap {
                assert!(result.expect_err("canonical second response exhausts the cap before MCP effect").to_string().contains(crate::spend::run::BUDGET_STOP));
                let run = spend.as_ref().unwrap();
                assert_eq!((run.accepted_usage().input_tokens,run.accepted_usage().output_tokens),(14,6));
                assert_eq!(run.account.snapshot().recorded_spend(),14.0);
                assert!(!run.account.snapshot().tracking_error && !run.account.has_pending());
                let totals = ledger.summary().unwrap().all_time;
                assert_eq!((totals.records,totals.input_tokens,totals.output_tokens),(2,14,6));
                assert_eq!(totals.known_cost,14.0);
            } else {
                let (answer,usage,_) = result.unwrap();
                assert_eq!(answer,"native owned terminal");
                assert_eq!((usage.input_tokens,usage.output_tokens),(21,9));
            }
            let cached_calls = server.method_count("tools/list");
            assert_eq!(diagnostic_app.ui.mcp_manager.as_ref().unwrap().deferred_status("owned"), Some((1, usize::from(!same_response))));
            assert_eq!(server.method_count("tools/list"), cached_calls, "cached deferred status must not perform an RPC");
            command(&mut diagnostic_app, "/mcp").await;
            let rendered = diagnostic_app.renderer.captured_output();
            assert!(rendered.contains("schema loading: Deferred"));
            assert!(rendered.contains(&format!("1 deferred schemas, {} enabled by search",usize::from(!same_response))));
            assert_eq!(server.method_count("tools/list"),cached_calls,"actual cached status command must not query the server");
            assert!(questions.try_recv().is_err());
            assert_eq!(server.method_count("tools/call"),usize::from(!same_response && !denied_by_hook && !cost_cap));
            assert!(server.provider_streams.lock().unwrap().is_empty());
            assert!(matches!(permission.lock().unwrap().check_mcp("mcp_tool:owned:owned_echo", None, "owned_echo"), crate::permission::checker::CheckResult::Ask));
            let wire = server.requests.lock().unwrap().iter().filter(|(route,_)|route.starts_with("POST /v1/chat/completions ")).map(|(_,body)|body.clone()).collect::<Vec<_>>();
            assert_eq!(wire.len(),if same_response{1}else if cost_cap{2}else{3});
            if let Some(mode) = hook {
                let calls = server.requests.lock().unwrap().iter().filter(|(_,body)|body["method"]=="tools/call").map(|(_,body)|body.clone()).collect::<Vec<_>>();
                let model_tool_results = serde_json::to_string(&wire[2]["messages"]).unwrap();
                if mode == "deny" {
                    assert!(calls.is_empty());
                    assert!(model_tool_results.contains("owned deferred hook denial"));
                } else {
                    assert_eq!(calls.len(),1);
                    assert_eq!(calls[0]["params"]["arguments"],serde_json::json!({"owned_hook_argument":"actual"}));
                    assert!(model_tool_results.contains("[REDACTED]"));
                    assert!(!model_tool_results.contains("actual owned echo"));
                }
            }
            let prefix = serde_json::to_string(&wire[0]["messages"]).unwrap();
            assert!(prefix.contains("<mcp_tool_index>") && prefix.contains("owned_echo"), "compact index must reach the actual native request system prefix");
            assert_eq!(wire[0]["tools"].as_array().unwrap().iter().map(|tool|tool["function"]["name"].as_str().unwrap()).collect::<Vec<_>>(),["mcp_search"]);
        }).catch_unwind().await;
        scope.cancellation_handle().cancel();
        tokio::time::timeout(Duration::from_secs(3), scope.wait_idle())
            .await
            .expect("wire fixture work must settle");
        drop(agent);
        assert_eq!(
            diagnostic_app
                .ui
                .mcp_manager
                .as_ref()
                .unwrap()
                .deferred_status("owned"),
            None,
            "manager status must not retain a strong discovery/SDK owner"
        );
        let manager = diagnostic_app.ui.mcp_manager.take().unwrap();
        diagnostic_app.retire_mcp_work().await.unwrap();
        drop(diagnostic_app);
        manager.shutdown().await;
        server.finish().await;
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
    }
}

/// Complete actual builder/SDK requests, not hand-calculated threshold input.
/// The deterministic estimator proves this fixture's reduced schema footprint;
/// it does not claim a provider tokenizer or signed-cache-prefix hit rate.
#[tokio::test]
async fn actual_builder_auto_threshold_cli_ceiling_and_deferred_token_estimate() {
    use crate::tests::fake_model::{MockCompletionModel, MockStreamEvent};
    use futures::FutureExt;
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let mut deferred_estimate = None;
    let mut eager_estimate = None;
    for (defer, count, selected, search, expected_count) in [
        (serde_json::json!("auto"), 32, false, true, 32),
        (serde_json::json!("auto"), 33, false, true, 1),
        (serde_json::json!(false), 33, false, true, 33),
        (serde_json::json!(true), 33, false, true, 1),
        (serde_json::json!("auto"), 33, true, true, 1),
        (serde_json::json!(true), 33, true, false, 1),
        (serde_json::json!(true), 33, false, false, 33),
    ] {
        let root = crate::tests::OwnedTestDirectory::new("mcp-builder-threshold");
        let _environment = environment(root.path());
        let server = Server::new(false).await;
        server
            .catalogue_count
            .store(count, std::sync::atomic::Ordering::Release);
        let base = config(root.path(), &server);
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
        let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true,"defer":defer}}}).to_string();
        crate::fs::private_atomic_create_sync(&root.path().join(".mcp.json"), raw.as_bytes())
            .unwrap();
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
        let mut tools = if selected {
            vec!["owned_00".into()]
        } else {
            (0..count)
                .map(|index| format!("owned_{index:02}"))
                .collect()
        };
        if search {
            tools.push("mcp_search".into());
        }
        let cli = crate::cli::Cli {
            no_session: true,
            tools,
            ..Default::default()
        };
        let context = crate::context::load_for_workspace(true, Some(root.path()));
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("counted terminal"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = deferred_mock_agent(
            model.clone(),
            &cli,
            &cfg,
            &context,
            &workspace,
            &manager,
            "owned-threshold-session",
        )
        .await;
        let scope = crate::agent::runner::AgentWorkScope::new();
        let assertions = std::panic::AssertUnwindSafe(async {
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                scope.run(crate::agent::runner::run_print(
                    &agent,
                    "measure definitions",
                    false,
                    &crate::retry::RetryConfig::default(),
                    None,
                    Vec::new(),
                    #[cfg(feature = "hooks")]
                    None,
                )),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(result.0, "counted terminal");
            let requests = model.requests();
            assert_eq!(requests.len(), 1);
            let definitions = &requests[0].tools;
            // Rig prepends the preamble as a System message and deliberately
            // clears the separate preamble field before invoking the carrier.
            let actual_prefix = serde_json::to_string(&requests[0].chat_history).unwrap();
            assert_eq!(
                definitions.len(),
                expected_count,
                "defer={defer} count={count} selected={selected} search={search}"
            );
            let deferred = search
                && !selected
                && (defer == serde_json::json!(true)
                    || defer == serde_json::json!("auto") && count > 32);
            if deferred {
                assert_eq!(definitions[0].name, "mcp_search");
                let preamble = &actual_prefix;
                assert!(preamble.contains("owned_00") && preamble.contains("owned_32"));
                assert!(
                    !preamble.contains("captured description")
                        && !preamble.contains("captured schema")
                );
            }
            if selected && defer == serde_json::json!("auto") {
                assert_eq!(
                    definitions[0].name, "owned_00",
                    "CLI ceiling is applied before the automatic threshold"
                );
            }
            if !search {
                let expected: Vec<_> = if selected {
                    vec!["owned_00".to_owned()]
                } else {
                    (0..count)
                        .map(|index| format!("owned_{index:02}"))
                        .collect()
                };
                assert_eq!(
                    definitions
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect::<Vec<_>>(),
                    expected,
                    "excluding discovery must retain exactly the CLI-admitted MCP schemas eagerly"
                );
                assert!(!actual_prefix.contains("<mcp_tool_index>"));
            }
            if count == 33 && !selected {
                let serialized = serde_json::to_string(definitions).unwrap();
                let estimate =
                    Session::estimate_tokens(&format!("{}\n{}", actual_prefix, serialized));
                if defer == serde_json::json!("auto") {
                    deferred_estimate = Some(estimate);
                }
                if defer == serde_json::json!(false) {
                    eager_estimate = Some(estimate);
                }
            }
            assert_eq!(server.method_count("tools/call"), 0);
            assert!(server.provider_streams.lock().unwrap().is_empty());
            server.assert_no_provider_request();
        })
        .catch_unwind()
        .await;
        scope.cancellation_handle().cancel();
        tokio::time::timeout(Duration::from_secs(3), scope.wait_idle())
            .await
            .unwrap();
        drop(agent);
        manager.shutdown().await;
        server.finish().await;
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
    }
    assert!(
        deferred_estimate.unwrap() * 4 < eager_estimate.unwrap(),
        "this bounded schema fixture must reduce estimated context by more than75%"
    );
}

async fn deferred_mock_agent(
    model: crate::tests::fake_model::MockCompletionModel,
    cli: &crate::cli::Cli,
    cfg: &config::Config,
    context: &crate::context::ContextFiles,
    workspace: &Arc<crate::paths::WorkspaceBinding>,
    manager: &crate::extras::mcp::McpClientManager,
    session_id: &str,
) -> rig::agent::Agent<crate::tests::fake_model::MockCompletionModel> {
    crate::agent::builder::build_agent_inner(
        model.clone(),
        cli,
        cfg,
        context,
        workspace.clone(),
        None,
        None,
        crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace.clone()),
        crate::agent::tools::ReadTracker::new(true),
        crate::agent::tools::TodoStore::default(),
        #[cfg(feature = "goal")]
        crate::extras::goal::GoalStore::default(),
        session_id,
        None,
        false,
        None,
        None,
        #[cfg(feature = "js")]
        crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
            backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
            assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
            reason: "MCP-only fixture does not start a JS worker".into(),
        },
        #[cfg(feature = "js")]
        crate::extras::js::session::ScratchStore::default(),
        #[cfg(feature = "skills")]
        None,
        Some(manager),
    )
    .await
}

#[tokio::test]
async fn actual_deferred_builder_rebuild_retains_only_same_catalogue_session_and_workspace() {
    use crate::tests::fake_model::{MockCompletionModel, MockStreamEvent};
    use futures::FutureExt;
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("mcp-discovery-rebuild");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    let raw = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true,"defer":true}}}).to_string();
    crate::fs::private_atomic_create_sync(&root.path().join(".mcp.json"), raw.as_bytes()).unwrap();
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
    let cli = crate::cli::Cli {
        no_session: true,
        tools: vec!["mcp_search".into(), "owned_echo".into()],
        ..Default::default()
    };
    let context = crate::context::load_for_workspace(true, Some(root.path()));
    let scope = crate::agent::runner::AgentWorkScope::new();
    let assertions = std::panic::AssertUnwindSafe(async {
        for (index, session_id, new_binding, hide_search, discover, expected) in [
            (0, "rebuild-owned", false, false, true, 1),
            (1, "rebuild-owned", false, false, false, 2),
            (2, "rebuild-foreign", false, false, false, 1),
            (3, "rebuild-owned", false, true, false, 1),
            (4, "rebuild-owned", false, false, false, 1),
            (5, "rebuild-owned", false, false, true, 1),
            (6, "rebuild-owned", true, false, false, 1),
            (7, "rebuild-owned", false, false, true, 1),
            (8, "rebuild-owned", false, false, false, 0),
            (9, "rebuild-owned", false, false, false, 1),
            (10, "rebuild-owned", false, false, true, 1),
            (11, "rebuild-owned", false, false, false, 1),
            (12, "rebuild-owned", false, false, false, 1),
        ] {
            let selected = if new_binding {
                Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap())
            } else {
                workspace.clone()
            };
            let mut selected_cli = cli.clone();
            selected_cli.no_tools = index == 8;
            let mut selected_cfg = cfg.clone();
            if index == 11 {
                let crate::extras::mcp::config::McpServerConfig::Url { defer, .. } = selected_cfg
                    .mcp_servers
                    .as_mut()
                    .unwrap()
                    .get_mut("owned")
                    .unwrap()
                else {
                    panic!("owned URL fixture");
                };
                *defer = crate::extras::mcp::config::McpDefer::Eager;
            }
            if hide_search {
                selected_cli.tools = vec!["owned_echo".into()];
            }
            let mut turns = Vec::new();
            if discover {
                turns.push(vec![
                    MockStreamEvent::tool_call(
                        "owned-rebuild-search",
                        "mcp_search",
                        serde_json::json!({"query":"owned_echo"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ]);
            }
            turns.push(vec![
                MockStreamEvent::text("rebuild terminal"),
                MockStreamEvent::final_response_with_default_usage(),
            ]);
            let model = MockCompletionModel::from_stream_turns(turns);
            // Clone the actual retained ContextFiles, as production rebuilds do.
            let rebuilt_context = context.clone();
            let agent = deferred_mock_agent(
                model.clone(),
                &selected_cli,
                &selected_cfg,
                &rebuilt_context,
                &selected,
                &manager,
                session_id,
            )
            .await;
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                scope.run(crate::agent::runner::run_print(
                    &agent,
                    "rebuild",
                    false,
                    &crate::retry::RetryConfig::default(),
                    None,
                    Vec::new(),
                    #[cfg(feature = "hooks")]
                    None,
                )),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(result.0, "rebuild terminal");
            let requests = model.requests();
            assert_eq!(requests.len(), if discover { 2 } else { 1 }, "case {index}");
            assert_eq!(
                requests[0].tools.len(),
                expected,
                "case {index} must bind exactly current session/catalogue/workspace"
            );
            if hide_search {
                assert_eq!(requests[0].tools[0].name, "owned_echo");
            }
            if discover {
                assert_eq!(requests[1].tools.len(), 2);
            }
            assert_eq!(
                server.method_count("tools/call"),
                0,
                "schema activation never executes candidates"
            );
            drop(agent);
        }
        server.assert_no_provider_request();
    })
    .catch_unwind()
    .await;
    scope.cancellation_handle().cancel();
    tokio::time::timeout(Duration::from_secs(3), scope.wait_idle())
        .await
        .expect("owned rebuild work must settle");
    manager.shutdown().await;
    server.finish().await;
    if let Err(panic) = assertions {
        std::panic::resume_unwind(panic);
    }
}

/// A deterministic reproduction of the documented native prefix restriction.
/// The endpoint compares concrete SDK request bodies; canary signatures are
/// deliberately inert and no cryptographic/server acceptance is claimed.
async fn native_deferred_case(case: &str) {
    let deny = case == "deny";
    let eager = case == "eager";
    let requested_model = if eager {
        "claude-opus-4-1"
    } else {
        "claude-opus-5-5"
    };
    let no_effect = matches!(case, "invalid" | "same-response" | "redact" | "terminate");
    use futures::FutureExt;
    #[cfg(feature = "hooks")]
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("mcp-anthropic-prefix-witness");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let base = config(root.path(), &server);
    let global = base.provenance.global_file.as_ref().unwrap();
    let mut raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(global).unwrap()).unwrap();
    raw["custom_providers"]["mcp57"] = serde_json::json!({"provider_type":"anthropic","base_url":format!("http://{}",server.address)});
    crate::fs::private_atomic_write_sync(global, raw.to_string().as_bytes()).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let paths = crate::paths::AppPaths::from_process(Some(root.path().to_path_buf())).unwrap();
    let project = serde_json::json!({"mcpServers":{"owned":{"url":server.url(),"allow_local":true,"defer":true}}}).to_string();
    crate::fs::private_atomic_create_sync(&root.path().join(".mcp.json"), project.as_bytes())
        .unwrap();
    config::approve_project_mcp(
        &paths,
        &workspace,
        &crate::extras::mcp::project::digest(&project),
    )
    .unwrap();
    let cfg = config::mcp_config_in_workspace_at(&workspace, global).unwrap();
    let mut manager =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(&cfg, &workspace).await;
    let cli = crate::cli::Cli {
        no_session: true,
        tools: vec!["mcp_search".into(), "owned_echo".into()],
        ..Default::default()
    };
    let context = crate::context::load_for_workspace(true, Some(root.path()));
    let client = crate::provider::create_client(
        "mcp57",
        Some("owned-inert-key"),
        cfg.custom_providers_map(),
        None,
    )
    .unwrap();
    let crate::provider::AnyModel::Anthropic(model) = client.completion_model(requested_model)
    else {
        panic!("owning exact native Anthropic converter required");
    };
    let model = if case == "cached-1h" {
        model.with_automatic_caching_1h()
    } else {
        model
    };
    let mut events = vec![
        serde_json::json!({"type":"message_start","message":{"id":"owned-anthropic-search","role":"assistant","content":[],"model":"claude-opus-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"owned canonical thought"}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"owned-inert-signature"}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"owned-search-call","name":"mcp_search","input":{}}}),
        serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"owned_echo\"}"}}),
        serde_json::json!({"type":"content_block_stop","index":1}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":3}}),
        serde_json::json!({"type":"message_stop"}),
    ];
    if eager {
        events[0]["message"]["model"] = serde_json::json!(requested_model);
        events[5]["content_block"]["name"] = serde_json::json!("owned_echo");
        events[6]["delta"]["partial_json"] = serde_json::json!("{}");
    }
    if case == "invalid" {
        events[6]["delta"]["partial_json"] = serde_json::json!("{\"query\":\"\"}");
    }
    if case == "same-response" {
        events.splice(8..8, [
                serde_json::json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"owned-hidden-call","name":"owned_echo","input":{}}}),
                serde_json::json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
                serde_json::json!({"type":"content_block_stop","index":2}),
            ]);
    }
    let stream = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    fn sse(events: &[serde_json::Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }
    server.provider_streams.lock().unwrap().extend([
        stream,
        sse(&[
            serde_json::json!({"type":"message_start","message":{"id":"owned-anthropic-echo","role":"assistant","content":[],"model":"claude-opus-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"owned-echo-call","name":"owned_echo","input":{}}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":3}}),
            serde_json::json!({"type":"message_stop"}),
        ]),
        sse(&[
            serde_json::json!({"type":"message_start","message":{"id":"owned-anthropic-done","role":"assistant","content":[],"model":"claude-opus-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"owned native terminal"}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}),
            serde_json::json!({"type":"message_stop"}),
        ]),
    ]);
    if eager || no_effect && case != "same-response" {
        server.provider_streams.lock().unwrap().remove(1);
    }
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
    let (ask, mut questions) = tokio::sync::mpsc::channel(4);
    let mut agent = crate::agent::builder::build_agent_inner(
        model,
        &cli,
        &cfg,
        &context,
        workspace.clone(),
        Some(permission.clone()),
        Some(ask),
        crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace),
        crate::agent::tools::ReadTracker::new(true),
        crate::agent::tools::TodoStore::default(),
        #[cfg(feature = "goal")]
        crate::extras::goal::GoalStore::default(),
        "owned-anthropic-deferred",
        None,
        true,
        None,
        Some(if eager {
            serde_json::json!({"thinking":{"type":"enabled","budget_tokens":1024}})
        } else {
            serde_json::json!({"thinking":{"type":"adaptive"}})
        }),
        #[cfg(feature = "js")]
        crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
            backend: crate::sandbox::worker::WorkerBackend::for_current_platform(),
            assurance: crate::sandbox::worker::WorkerContainmentAssurance::Enforced,
            reason: "MCP-only native witness".into(),
        },
        #[cfg(feature = "js")]
        crate::extras::js::session::ScratchStore::default(),
        #[cfg(feature = "skills")]
        None,
        Some(&manager),
    )
    .await;
    struct ReferencePolicy(bool);
    impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for ReferencePolicy {
        async fn on_event(
            &self,
            _: &rig::agent::HookContext,
            event: rig::agent::StepEvent<'_, M>,
        ) -> rig::agent::Flow {
            if matches!(
                event,
                rig::agent::StepEvent::ToolResult {
                    tool_name: "mcp_search",
                    ..
                }
            ) {
                if self.0 {
                    rig::agent::Flow::terminate("owned discovery termination")
                } else {
                    rig::agent::Flow::rewrite_result("owned redacted discovery")
                }
            } else {
                rig::agent::Flow::Continue
            }
        }
    }
    if matches!(case, "redact" | "terminate") {
        agent.hooks.push(ReferencePolicy(case == "terminate"));
    }
    let scope = crate::agent::runner::AgentWorkScope::new();
    let assertions = std::panic::AssertUnwindSafe(async {
        let retry = crate::retry::RetryConfig::default();
        let turn = scope.run(crate::agent::runner::run_print(&agent,"discover",false,&retry,None,Vec::new(),#[cfg(feature = "hooks")] None));
        tokio::pin!(turn);
        if !no_effect {
            let request = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::select! {
                    result = &mut turn => panic!("native activated tool requires actual Ask: {result:?}"),
                    request = questions.recv() => request.unwrap().into_permission(),
                }
            }).await.unwrap();
            assert_eq!(request.tool,"mcp_tool");
            assert_eq!(request.input,"mcp_tool:owned:owned_echo");
            assert_eq!(server.method_count("tools/call"),0);
            request.reply.send(if deny { crate::permission::ask::UserDecision::Deny } else { crate::permission::ask::UserDecision::AllowOnce }).unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(3), &mut turn).await.unwrap();
        if matches!(case, "same-response" | "terminate") {
            let error = result.unwrap_err().to_string();
            assert!(error.contains(if case == "terminate" {"owned discovery termination"} else {"UnknownToolCall"}), "{error}");
        } else { assert_eq!(result.unwrap().0,"owned native terminal"); }
        let wire = server.requests.lock().unwrap().iter().filter(|(route,_)|route.starts_with("POST /v1/messages ")).map(|(_,body)|body.clone()).collect::<Vec<_>>();
        assert_eq!(wire.len(),if matches!(case,"same-response" | "terminate") {1} else if no_effect || eager {2} else {3}, "{case}");
        let raw_prefixes=server.native_raw_prefixes.lock().unwrap();
        assert_eq!(raw_prefixes.len(),wire.len());
        assert!(raw_prefixes.iter().all(|prefix|prefix==&raw_prefixes[0]),"native system/tools bytes changed: {case}");
        assert!(server.anthropic_prefix.lock().unwrap().is_some());
        if eager {
            assert_eq!(wire[0]["tools"].as_array().unwrap().iter().map(|tool|tool["name"].as_str().unwrap()).collect::<Vec<_>>(),["owned_echo"]);
            assert!(wire[0]["tools"][0].get("defer_loading").is_none());
            assert!(manager.take_notices().iter().any(|notice| notice.contains("remain eager")));
        } else {
            assert_eq!(wire[0]["tools"].as_array().unwrap().iter().map(|tool|tool["name"].as_str().unwrap()).collect::<Vec<_>>(),["mcp_search","owned_echo"]);
            assert_eq!(wire[0]["tools"][1]["defer_loading"],true);
            assert!(wire[0]["tools"][0].get("defer_loading").is_none());
        }
        let expected_marker = if case == "cached-1h" {serde_json::json!({"type":"ephemeral","ttl":"1h"})} else {serde_json::json!({"type":"ephemeral"})};
        let native_tools = wire[0]["tools"].as_array().unwrap();
        let cacheable = native_tools.iter().filter(|tool|tool["defer_loading"]!=true).collect::<Vec<_>>();
        assert_eq!(cacheable.last().unwrap()["cache_control"],expected_marker);
        assert_eq!(native_tools.iter().filter(|tool|tool.get("cache_control").is_some()).count(),1);
        assert!(native_tools.iter().filter(|tool|tool["defer_loading"]==true).all(|tool|tool.get("cache_control").is_none()));
        assert_eq!(wire[0]["system"].as_array().unwrap().last().unwrap()["cache_control"],expected_marker);
        if case == "cached-1h" {
            assert_eq!(wire[0]["cache_control"],expected_marker);
        }
        for request in wire.iter().skip(1) {
            assert_eq!(wire[0]["cache_control"],request["cache_control"],"{case}");
            assert_eq!(wire[0]["tools"],request["tools"],"{case}");
            assert_eq!(wire[0]["system"],request["system"],"{case}");
        }
        if wire.len() > 1 {
            let results = wire[1]["messages"].as_array().unwrap().iter().flat_map(|message| message["content"].as_array().unwrap()).filter(|block|block["type"]=="tool_result").collect::<Vec<_>>();
            assert_eq!(results.len(),1);
            assert_eq!(results[0]["tool_use_id"],"owned-search-call");
            let references = results[0]["content"].as_array().unwrap().iter().filter(|block|block["type"]=="tool_reference").collect::<Vec<_>>();
            if no_effect || eager { assert!(references.is_empty(),"{case}"); }
            else { assert_eq!(references,vec![&serde_json::json!({"type":"tool_reference","tool_name":"owned_echo"})]); }
            if case == "redact" {
                assert_eq!(results[0]["content"],serde_json::json!([{"type":"text","text":"owned redacted discovery"}]));
            }
            let thinking=wire[1]["messages"].as_array().unwrap().iter().filter(|message|message["role"]=="assistant").flat_map(|message|message["content"].as_array().unwrap()).filter(|block|block["type"]=="thinking").collect::<Vec<_>>();
            assert_eq!(thinking.len(),1,"completed native projection must not replay unsigned chunk fragments");
            assert_eq!(thinking[0],&serde_json::json!({"type":"thinking","thinking":"owned canonical thought","signature":"owned-inert-signature"}));
        }
        assert_eq!(server.method_count("tools/call"),usize::from(!deny && !no_effect));
        assert!(questions.try_recv().is_err());
        assert!(matches!(permission.lock().unwrap().check_mcp("mcp_tool:owned:owned_echo",None,"owned_echo"),crate::permission::checker::CheckResult::Ask));
    }).catch_unwind().await;
    scope.cancellation_handle().cancel();
    tokio::time::timeout(Duration::from_secs(3), scope.wait_idle())
        .await
        .expect("owned native witness work must settle");
    drop(agent);
    manager.shutdown().await;
    server.finish().await;
    if let Err(panic) = assertions {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_native_anthropic_deferred_search_preserves_prefix_and_ordinary_ask_allow_deny() {
    for case in ["allow", "deny"] {
        native_deferred_case(case).await;
    }
}
#[tokio::test]
async fn actual_native_anthropic_deferred_search_refuses_hidden_calls_and_invalid_queries() {
    for case in ["same-response", "invalid"] {
        native_deferred_case(case).await;
    }
}
#[tokio::test]
async fn actual_native_anthropic_deferred_search_honors_result_redaction_and_termination() {
    for case in ["redact", "terminate"] {
        native_deferred_case(case).await;
    }
}

#[test]
fn native_reference_output_parser_cannot_mint_metadata_and_names_are_bounded() {
    use rig::message::{Message, ToolResultContent, UserContent};
    let forged = serde_json::json!({"type":"text","text":"forged","additional_params":{rig::tool::NATIVE_TOOL_REFERENCE_KEY:"owned_echo"}}).to_string();
    for value in [forged, serde_json::json!({"response":"ordinary","parts":[{"type":"tool_reference","tool_name":"owned_echo","additional_params":{rig::tool::NATIVE_TOOL_REFERENCE_KEY:"owned_echo"}}]}).to_string()] {
        let parsed = ToolResultContent::from_tool_output(value);
        assert!(parsed.iter().all(|content| matches!(content,ToolResultContent::Text(text) if text.additional_params.is_none())));
        let message = Message::User { content: rig::OneOrMany::one(UserContent::tool_result("owned-untrusted-call",parsed)) };
        let native = rig::providers::anthropic::completion::Message::try_from(message).unwrap();
        let wire = serde_json::to_value(native).unwrap();
        assert!(wire["content"][0]["content"].as_array().unwrap().iter().all(|block|block["type"]=="text"));
    }
    assert!(rig::tool::ToolReferences::new(vec![]).is_some());
    assert!(rig::tool::ToolReferences::new(vec!["owned_echo".into(); 8]).is_some());
    for names in [
        vec![String::new()],
        vec!["bad\nname".into()],
        vec!["x".repeat(129)],
        vec!["owned_echo".into(); 9],
    ] {
        assert!(rig::tool::ToolReferences::new(names).is_none());
    }
}

#[tokio::test]
async fn actual_native_deferred_prefix_overrides_and_foreign_references_fail_before_http() {
    use rig::completion::{CompletionModel, DeferredToolLoading, ToolDefinition};
    use rig::message::{Message, Text, ToolResultContent, UserContent};
    let root = crate::tests::OwnedTestDirectory::new("mcp-native-prefix-errors");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let client = rig::providers::anthropic::Client::builder()
        .api_key("owned-inert-key")
        .base_url(format!("http://{}", server.address))
        .http_client(crate::provider_http::ProviderHttpClient::default())
        .build()
        .unwrap();
    use rig::client::CompletionClient;
    let model = client
        .completion_model("claude-opus-5-5")
        .with_deferred_tools(vec![ToolDefinition {
            name: "owned_echo".into(),
            description: "owned".into(),
            parameters: serde_json::json!({"type":"object"}),
        }]);
    assert_eq!(
        model.deferred_tool_loading(),
        DeferredToolLoading::NativeReferences
    );
    for name in [
        "claude-opus-4-1",
        "claude-opus-5-unknown",
        "claude-opus-4-6-custom",
        "display-alias",
    ] {
        assert_eq!(
            client.completion_model(name).deferred_tool_loading(),
            DeferredToolLoading::Eager
        );
    }
    let mut requests = Vec::new();
    for extra in [
        serde_json::json!({"model":"other"}),
        serde_json::json!({"system":"other"}),
        serde_json::json!({"messages":[]}),
        serde_json::json!({"tools":[{"name":"owned_echo"}]}),
        serde_json::json!({"tools":{}}),
        serde_json::json!(false),
    ] {
        requests.push(
            model
                .completion_request("owned")
                .max_tokens(64)
                .additional_params(extra)
                .build(),
        );
    }
    requests.push(
        model
            .completion_request("owned")
            .max_tokens(64)
            .model("claude-opus-4-1")
            .build(),
    );
    for marker in [serde_json::json!("foreign_tool"), serde_json::json!({})] {
        let result = UserContent::tool_result(
            "owned-imported-id",
            rig::OneOrMany::one(ToolResultContent::Text(Text {
                text: String::new(),
                additional_params: Some(
                    serde_json::json!({rig::tool::NATIVE_TOOL_REFERENCE_KEY:marker}),
                ),
            })),
        );
        requests.push(
            model
                .completion_request("owned")
                .max_tokens(64)
                .message(Message::User {
                    content: rig::OneOrMany::one(result),
                })
                .build(),
        );
    }
    let assertions = std::panic::AssertUnwindSafe(async {
        let invalid_model = client
            .completion_model("claude-opus-5-5")
            .with_deferred_tools(vec![ToolDefinition {
                name: "x".repeat(129),
                description: "owned".into(),
                parameters: serde_json::json!({}),
            }]);
        let invalid_request = invalid_model
            .completion_request("owned")
            .max_tokens(64)
            .build();
        let error = match tokio::time::timeout(
            Duration::from_secs(3),
            invalid_model.stream(invalid_request),
        )
        .await
        .unwrap()
        {
            Ok(_) => panic!("invalid captured declaration must fail before HTTP"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("Invalid captured native tool-reference name")
        );
        for request in requests {
            let error = match tokio::time::timeout(Duration::from_secs(3), model.stream(request))
                .await
                .unwrap()
            {
                Ok(_) => panic!("conflicting native prefix must fail before HTTP"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("captured")
                    || error.to_string().contains("Deferred native model changed")
                    || error.to_string().contains("Native deferred request extras")
                    || error.to_string().contains("Invalid native tools override"),
                "{error}"
            );
        }
        assert_eq!(
            server.accepted.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert!(server.requests.lock().unwrap().is_empty());
    });
    use futures::FutureExt;
    let assertions = assertions.catch_unwind().await;
    server.finish().await;
    if let Err(panic) = assertions {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn actual_native_anthropic_unsupported_model_keeps_eager_schema_and_ordinary_ask() {
    native_deferred_case("eager").await;
}

#[tokio::test]
async fn actual_native_anthropic_deferred_cache_markers_keep_selected_one_hour_prefix() {
    native_deferred_case("cached-1h").await;
}

fn auxiliary_completion() -> String {
    let first = serde_json::json!({"id":"mcp58-owned","object":"chat.completion.chunk","created":0,"model":"owned-model","choices":[{"index":0,"delta":{"role":"assistant","content":"MCP58 terminal"},"finish_reason":null}]});
    let end = serde_json::json!({"id":"mcp58-owned","object":"chat.completion.chunk","created":0,"model":"owned-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
    format!("data: {first}\n\ndata: {end}\n\ndata: [DONE]\n\n")
}
async fn auxiliary_terminal(app: &mut App<'_>) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while app.run.is_running {
            let event = app.run.agent_rx.as_mut().unwrap().recv().await.unwrap();
            crate::startup::heap_future(|| app.handle_agent_event(event))
                .await
                .unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn actual_mcp_auxiliary_actor_asks_once_and_preserves_literal_context_and_prompt_admission() {
    use futures::FutureExt;
    let root = crate::tests::OwnedTestDirectory::new("mcp58-actual-app");
    let _environment = environment(root.path());
    let server = Server::new(false).await;
    let mut cfg = config(root.path(), &server);
    let fixture = crate::tests::mcp_stdio_tests::FixtureBuild::compile();
    let lease = fixture.lease("actual-auxiliary");
    let mut mcp = fixture.config(
        fixture.executable.to_string_lossy().into_owned(),
        Vec::new(),
        "aux",
        &lease,
    );
    if let crate::extras::mcp::config::McpServerConfig::Command { env, .. } = &mut mcp {
        env.insert("MCP_FIXTURE_TOOL_NAME".into(), "resources/read".into());
    }
    cfg.mcp_servers = Some(std::collections::HashMap::from([(
        "owned".into(),
        mcp.clone(),
    )]));
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        ..Default::default()
    };
    let mut session = Session::new(
        "mcp57",
        "owned-model",
        128_000,
        root.path().to_str().unwrap(),
    );
    let mut context = crate::context::load_for_workspace(true, Some(root.path()));
    let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
    let handle =
        crate::extras::mcp::client::McpClientHandle::connect_in("owned".into(), &mcp, root.path())
            .await
            .unwrap();
    handle.discover_auxiliary().await;
    app.ui.mcp_manager = Some(McpClientManager::from_handles(vec![handle]));
    let (ask, mut asks) = tokio::sync::mpsc::channel(4);
    app.ui.ask_tx = Some(ask);
    let policy = crate::permission::PermissionConfig {
        default: Some(crate::permission::Action::Deny),
        mcp_tool: Some(crate::permission::ToolPerm::Granular(
            std::collections::HashMap::from([
                (
                    "mcp_tool:owned:resources/read".into(),
                    crate::permission::Action::Ask,
                ),
                (
                    "mcp_tool:owned:prompts/get".into(),
                    crate::permission::Action::Allow,
                ),
            ]),
        )),
        ..Default::default()
    };
    app.ui.permission = Some(Arc::new(std::sync::Mutex::new(
        crate::permission::checker::PermissionChecker::new(
            &crate::permission::PermissionConfigs::from(policy),
            crate::permission::SecurityMode::Standard,
            None,
            Some(vec!["standard".into()]),
        )
        .unwrap(),
    )));
    let assertions = std::panic::AssertUnwindSafe(async {
        app.refresh_custom_commands();
        let rpc_before = std::fs::read_to_string(lease.with_extension("rpc")).unwrap();
        command(&mut app, "/add mcp:owned:urn%3Aowned%3Anotes").await;
        let request = tokio::time::timeout(Duration::from_secs(2), asks.recv())
            .await
            .unwrap()
            .unwrap();
        let crate::permission::ask::UserRequest::McpPermission(owned) = &request else {
            panic!("operator permission requires its exact actor binding");
        };
        assert_eq!(
            owned.request.input,
            "mcp_auxiliary:owned:resources/read:urn%3Aowned%3Anotes"
        );
        assert!(
            !std::fs::read_to_string(lease.with_extension("rpc"))
                .unwrap()
                .contains("resources/read")
        );
        app.begin_user_request(request).unwrap();
        app.refresh().unwrap();
        assert!(
            app.renderer
                .captured_output()
                .contains("mcp_auxiliary:owned:resources/read:urn%3Aowned%3Anotes")
        );
        user(
            &mut app,
            UserEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        )
        .await;
        drain(&mut app).await;
        assert_eq!(app.mcp_admin.as_ref().unwrap().resources.len(), 1);
        assert_eq!(app.mcp_current_resources()[0].text, "Resource BODY 雪");
        let native = app.ui.mcp_manager.as_ref().unwrap().collect_tools_with_timeouts(
            app.ui.permission.clone(), app.ui.ask_tx.clone(), crate::extras::mcp::McpToolTimeouts {call:Duration::from_secs(2),..Default::default()}
        ).await;
        assert_eq!(native.len(),1);
        let mut native_call=Box::pin(rig::tool::ToolDyn::call(&native[0], "{}".into()));
        let native_request=tokio::time::timeout(Duration::from_secs(2),async {
            tokio::select! {
                request=asks.recv()=>request.unwrap(),
                result=&mut native_call=>panic!("auxiliary session approval widened a same-name actual native tool: {result:?}"),
            }
        }).await.unwrap();
        let crate::permission::ask::UserRequest::Permission(native_request)=native_request else {panic!("native invocation must retain its separate ordinary scope");};
        assert_eq!(native_request.input,"mcp_tool:owned:resources/read");
        assert!(!std::fs::read_to_string(lease.with_extension("rpc")).unwrap().contains("tools/call"));
        native_request.reply.send(crate::permission::ask::UserDecision::Deny).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2),&mut native_call).await.unwrap().is_err());
        drop(native_call);
        assert_eq!(app.ui.session.permission_allowlist.len(),1);

        server.assert_no_provider_request();
        let rpc_added = std::fs::read_to_string(lease.with_extension("rpc")).unwrap();
        assert!(rpc_added.len() > rpc_before.len());
        app.mcp_status().unwrap();
        app.refresh_custom_commands();
        assert_eq!(
            std::fs::read_to_string(lease.with_extension("rpc")).unwrap(),
            rpc_added,
            "status/pickers never query the peer"
        );
        server
            .provider_streams
            .lock()
            .unwrap()
            .push_back(auxiliary_completion());
        command(&mut app, "Explain the added resource").await;
        auxiliary_terminal(&mut app).await;
        let rows = server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(route, _)| route.starts_with("POST /v1/chat/completions "))
            .map(|(_, body)| body.clone())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        let text = rows[0].to_string();
        assert!(text.contains("Resource BODY 雪") && text.contains("User request:"));
        assert!(
            !rows[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "system")
                .any(|message| message.to_string().contains("Resource BODY 雪")),
            "resource data must not mutate native system prefix"
        );
        command(&mut app, "/drop mcp:owned:urn%3Aowned%3Anotes").await;
        assert!(app.mcp_current_resources().is_empty());
        server
            .provider_streams
            .lock()
            .unwrap()
            .push_back(auxiliary_completion());
        command(&mut app, "/mcp:owned:review {\"topic\":\"public\"}").await;
        drain(&mut app).await;
        auxiliary_terminal(&mut app).await;
        assert!(!app.input.buffer.contains("/commit"));
        assert!(
            app.ui.session.permission_allowlist.len() == 1,
            "only the explicit selected auxiliary approval can persist; server prompt data cannot mint grants"
        );
        let rows = server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(route, _)| route.starts_with("POST /v1/chat/completions "))
            .map(|(_, body)| body.clone())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert!(rows[1].to_string().contains("Prompt BODY 雪"));
        assert_eq!(app.ui.session.total_input_tokens, 14);
    })
    .catch_unwind()
    .await;
    app.retire_mcp_work().await.unwrap();
    app.abort_main_run().await.unwrap();
    if let Some(manager) = app.ui.mcp_manager.take() {
        manager.shutdown().await;
    }
    drop(app);
    server.finish().await;
    fixture.cleanup();
    if let Err(panic) = assertions {
        std::panic::resume_unwind(panic);
    }
}

fn auxiliary_tool_completion() -> String {
    let first = serde_json::json!({"id":"mcp58-tool","object":"chat.completion.chunk","created":0,"model":"owned-model","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"mcp58-canonical","type":"function","function":{"name":"probe","arguments":"{}"}}]},"finish_reason":null}]});
    let end = serde_json::json!({"id":"mcp58-tool","object":"chat.completion.chunk","created":0,"model":"owned-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
    format!("data: {first}\n\ndata: {end}\n\ndata: [DONE]\n\n")
}
#[tokio::test]
async fn actual_mcp_elicitation_channels_render_btw_before_answer_and_cancel_without_next_request()
{
    use futures::FutureExt;
    for outcome in ["accept", "cancel", "retired-turn"] {
        let cancel = outcome != "accept";
        let root = crate::tests::OwnedTestDirectory::new("mcp58-elicitation-app");
        let _environment = environment(root.path());
        let server = Server::new(false).await;
        let mut cfg = config(root.path(), &server);
        let fixture = crate::tests::mcp_stdio_tests::FixtureBuild::compile();
        let lease = fixture.lease("actual-form");
        let mcp = fixture.config(
            fixture.executable.to_string_lossy().into_owned(),
            Vec::new(),
            "aux-elicit",
            &lease,
        );
        cfg.mcp_servers = Some(std::collections::HashMap::from([(
            "owned".into(),
            mcp.clone(),
        )]));
        let cli = crate::cli::Cli {
            no_session: true,
            tools: vec!["probe".into()],
            ..Default::default()
        };
        let mut session = Session::new(
            "mcp57",
            "owned-model",
            128_000,
            root.path().to_str().unwrap(),
        );
        let mut context = crate::context::load_for_workspace(true, Some(root.path()));
        let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
        let handle = crate::extras::mcp::client::McpClientHandle::connect_in(
            "owned".into(),
            &mcp,
            root.path(),
        )
        .await
        .unwrap();
        app.ui.mcp_manager = Some(McpClientManager::from_handles(vec![handle]));
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        app.ui.ask_tx = Some(sender);
        app.ask_rx = Some(receiver);
        let policy = crate::permission::PermissionConfig {
            default: Some(crate::permission::Action::Deny),
            mcp_tool: Some(crate::permission::ToolPerm::Granular(
                std::collections::HashMap::from([(
                    "mcp_tool:owned:probe".into(),
                    crate::permission::Action::Allow,
                )]),
            )),
            ..Default::default()
        };
        app.ui.permission = Some(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfigs::from(policy),
                crate::permission::SecurityMode::Standard,
                None,
                Some(vec!["standard".into()]),
            )
            .unwrap(),
        )));
        let assertions=std::panic::AssertUnwindSafe(async {
            if !cancel {
                server.provider_streams.lock().unwrap().push_back(auxiliary_completion());
                command(&mut app,"/btw Public side question").await;
                tokio::time::timeout(Duration::from_secs(2),async {loop {if server.requests.lock().unwrap().iter().any(|(route,_)|route.starts_with("POST /v1/")){break;}tokio::task::yield_now().await;}}).await.unwrap();
            }
            server.provider_streams.lock().unwrap().push_back(auxiliary_tool_completion());
            if !cancel {server.provider_streams.lock().unwrap().push_back(auxiliary_completion());}
            command(&mut app,"Ask the owned MCP server for its public label").await;
            tokio::time::timeout(Duration::from_secs(3),async {
                while app.question_prompt.is_none() {
                    tokio::select! {biased;
                        request=app.ask_rx.as_mut().unwrap().recv()=>app.begin_user_request(request.unwrap()).unwrap(),
                        event=app.run.agent_rx.as_mut().unwrap().recv()=>crate::startup::heap_future(||app.handle_agent_event(event.unwrap())).await.unwrap(),
                    }
                }
            }).await.unwrap();
            let before=app.renderer.captured_output();assert!(before.contains("Choose a public label"));
            assert!(app.ui.session.permission_allowlist.is_empty());assert!(app.ui.session.pending_question.is_none());
            if cancel {
                if outcome == "retired-turn" {
                    app.run.question_turn = None;
                    app.expire_abandoned_permission().unwrap();
                    assert!(app.question_prompt.is_none(), "an old Question turn cannot retain the form overlay");
                }
                app.run.pending_inputs.push_back("STALE never launch".into());
                let stop = if outcome == "retired-turn" { KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL) } else { KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE) };
                user(&mut app,UserEvent::Key(stop)).await;
                assert!(!app.run.is_running&&app.run.agent_rx.is_none());
                assert!(app.run.pending_inputs.is_empty());
                assert!(app.ui.session.permission_allowlist.is_empty());
                assert_eq!(server.requests.lock().unwrap().iter().filter(|(route,_)|route.starts_with("POST /v1/")).count(),1);
            } else {
                tokio::time::timeout(Duration::from_secs(3),async {while app.btw_inflight>0 {let event=app.btw_rx.recv().await.unwrap();app.handle_btw_event(event).unwrap();}}).await.unwrap();
                assert!(app.renderer.captured_output().contains("MCP58 terminal"),"real side-channel result rendered before answering");
                assert!(app.question_prompt.is_some(),"side completion must not auto-answer the form");
                user(&mut app,UserEvent::Key(KeyEvent::new(KeyCode::Char('1'),KeyModifiers::NONE))).await;
                auxiliary_terminal(&mut app).await;
                let rows=server.requests.lock().unwrap().iter().filter(|(route,_)|route.starts_with("POST /v1/")).map(|(_,body)|body.clone()).collect::<Vec<_>>();
                assert_eq!(rows.len(),3);assert!(rows[2].to_string().contains("accept"));
                assert!(app.ui.session.pending_question.is_none()&&app.ui.session.permission_allowlist.is_empty());
            }
        }).catch_unwind().await;
        app.cancel_pending_permissions();
        app.abort_main_run().await.unwrap();
        app.retire_mcp_work().await.unwrap();
        if let Some(manager) = app.ui.mcp_manager.take() {
            manager.shutdown().await;
        }
        drop(app);
        server.finish().await;
        fixture.cleanup();
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
    }
}

#[tokio::test]
async fn actual_mcp_auxiliary_held_ask_cannot_survive_drop_session_or_connection_retirement() {
    use futures::FutureExt;
    for retirement in ["drop", "session", "connection", "late-reply"] {
        let root = crate::tests::OwnedTestDirectory::new("mcp58-retired-auxiliary");
        let _environment = environment(root.path());
        let server = Server::new(false).await;
        let mut cfg = config(root.path(), &server);
        let fixture = crate::tests::mcp_stdio_tests::FixtureBuild::compile();
        let lease = fixture.lease("retired-auxiliary");
        let mcp = fixture.config(
            fixture.executable.to_string_lossy().into_owned(),
            Vec::new(),
            "aux",
            &lease,
        );
        cfg.mcp_servers = Some(std::collections::HashMap::from([(
            "owned".into(),
            mcp.clone(),
        )]));
        let cli = crate::cli::Cli {
            no_session: true,
            no_tools: true,
            ..Default::default()
        };
        let mut session = Session::new(
            "mcp57",
            "owned-model",
            128_000,
            root.path().to_str().unwrap(),
        );
        let mut context = crate::context::load_for_workspace(true, Some(root.path()));
        let mut app = app(root.path(), &cli, &cfg, &mut session, &mut context);
        let handle = crate::extras::mcp::client::McpClientHandle::connect_in(
            "owned".into(),
            &mcp,
            root.path(),
        )
        .await
        .unwrap();
        handle.discover_auxiliary().await;
        app.ui.mcp_manager = Some(McpClientManager::from_handles(vec![handle]));
        let (sender, mut requests) = tokio::sync::mpsc::channel(4);
        app.ui.ask_tx = Some(sender);
        app.ui.permission = Some(Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfigs::from(crate::permission::PermissionConfig {
                    default: Some(crate::permission::Action::Ask),
                    ..Default::default()
                }),
                crate::permission::SecurityMode::Standard,
                None,
                Some(vec!["standard".into()]),
            )
            .unwrap(),
        )));
        let assertions = std::panic::AssertUnwindSafe(async {
            command(&mut app, "/add mcp:owned:urn%3Aowned%3Anotes").await;
            let request = tokio::time::timeout(Duration::from_secs(2), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                &request,
                crate::permission::ask::UserRequest::McpPermission(_)
            ));
            let mut request = Some(request);
            if retirement != "drop" && retirement != "late-reply" {
                app.begin_user_request(request.take().unwrap()).unwrap();
                assert!(app.permission_prompt.is_some());
            }
            match retirement {
                "drop" => {
                    command(&mut app, "/drop mcp:owned:urn%3Aowned%3Anotes").await;
                    assert!(app.mcp_admin.as_ref().unwrap().work.is_none());
                    app.begin_user_request(request.take().unwrap()).unwrap();
                    assert!(
                        app.permission_prompt.is_none(),
                        "retired channel backlog cannot become an overlay"
                    );
                }
                "session" => app.ui.session.id = "fresh-session".into(),
                "late-reply" => {
                    let manager = app.ui.mcp_manager.take().unwrap();
                    manager.shutdown().await;
                    let crate::permission::ask::UserRequest::McpPermission(late) =
                        request.take().unwrap()
                    else {
                        unreachable!()
                    };
                    let pattern = late.request.input.clone();
                    late.request
                        .reply
                        .send(crate::permission::ask::UserDecision::AllowAlways(pattern))
                        .unwrap();
                }
                "connection" => {
                    let manager = app.ui.mcp_manager.take().unwrap();
                    manager.shutdown().await;
                }
                _ => unreachable!(),
            }
            app.expire_abandoned_permission().unwrap();
            assert!(
                app.permission_prompt.is_none(),
                "retired operator cannot retain a permission overlay"
            );
            user(
                &mut app,
                UserEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            )
            .await;
            if app
                .mcp_admin
                .as_ref()
                .is_some_and(|state| state.work.is_some())
            {
                drain(&mut app).await;
            }
            assert!(app.mcp_current_resources().is_empty());
            assert!(
                !std::fs::read_to_string(lease.with_extension("rpc"))
                    .unwrap()
                    .contains("resources/read"),
                "held authorization cannot dispatch after exact retirement"
            );
            assert!(app.ui.session.permission_allowlist.is_empty());
            assert!(matches!(
                app.ui
                    .permission
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .check_mcp("mcp_tool:owned:resources/read", None, "resources/read"),
                crate::permission::checker::CheckResult::Ask
            ));
            server.assert_no_provider_request();
        })
        .catch_unwind()
        .await;
        app.cancel_pending_permissions();
        app.retire_mcp_work().await.unwrap();
        if let Some(manager) = app.ui.mcp_manager.take() {
            manager.shutdown().await;
        }
        drop(app);
        server.finish().await;
        fixture.cleanup();
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
    }
}
