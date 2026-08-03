pub mod config;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use agent_client_protocol::on_receive_request;
use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectTo, ConnectionTo, Dispatch, Responder, Role, Stdio,
};
use tokio::sync::Mutex;

use crate::acp_auth::authenticate_peer;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::event::AgentEvent;
use crate::sandbox::Sandbox;

const AGENT_VERSION: &str = "1.0.5";
const DEFAULT_TCP_HOST: &str = "127.0.0.1";
const DEFAULT_TCP_PORT: u16 = 7243;
const MAX_PENDING_AUTHENTICATIONS: usize = 16;

struct SessionState {
    messages: Vec<(String, String)>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
}

struct AcpState {
    cli: Cli,
    cfg: Config,
    context: ContextFiles,
    sessions: Mutex<HashMap<SessionId, SessionState>>,
}

// --- TCP Transport ---

struct TcpTransport {
    host: String,
    port: u16,
    api_key: String,
}

impl<Counterpart: Role> ConnectTo<Counterpart> for TcpTransport {
    async fn connect_to(
        self,
        client: impl ConnectTo<Counterpart::Counterpart>,
    ) -> Result<(), agent_client_protocol::Error> {
        let listener = TcpListener::bind((self.host.as_str(), self.port)).map_err(|e| {
            agent_client_protocol::util::internal_error(format!(
                "TCP bind {}:{}: {}",
                self.host, self.port, e
            ))
        })?;
        let local_addr = listener.local_addr().map_err(|e| {
            agent_client_protocol::util::internal_error(format!("TCP local address: {}", e))
        })?;

        tracing::info!("ACP TCP listening on {}", local_addr);
        let (stream, peer_addr) =
            accept_authenticated_peer(listener, self.api_key).map_err(|e| {
                agent_client_protocol::util::internal_error(format!("TCP accept: {}", e))
            })?;
        tracing::info!("Authenticated ACP client connected from {}", peer_addr);

        let read_half = stream.try_clone().map_err(|e| {
            agent_client_protocol::util::internal_error(format!("TCP clone: {}", e))
        })?;
        let write_half = stream;

        let read_unblock = blocking::Unblock::new(read_half);
        let write_unblock = blocking::Unblock::new(write_half);

        ConnectTo::<Counterpart>::connect_to(ByteStreams::new(write_unblock, read_unblock), client)
            .await
    }
}

enum AuthenticationAttempt {
    Accepted(TcpStream, SocketAddr),
    Rejected(SocketAddr),
}

fn accept_authenticated_peer(
    listener: TcpListener,
    api_key: String,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    listener.set_nonblocking(true)?;
    let api_key: Arc<str> = api_key.into();
    let pending = Arc::new(AtomicUsize::new(0));
    let (result_tx, result_rx) = mpsc::channel();

    loop {
        while let Ok(attempt) = result_rx.try_recv() {
            match attempt {
                AuthenticationAttempt::Accepted(stream, peer_addr) => {
                    return Ok((stream, peer_addr));
                }
                AuthenticationAttempt::Rejected(peer_addr) => {
                    tracing::warn!("ACP TCP peer authentication rejected for {}", peer_addr);
                }
            }
        }

        match listener.accept() {
            Ok((mut stream, peer_addr)) => {
                if pending.fetch_add(1, Ordering::AcqRel) >= MAX_PENDING_AUTHENTICATIONS {
                    pending.fetch_sub(1, Ordering::AcqRel);
                    tracing::warn!("ACP TCP peer rejected because authentication capacity is full");
                    continue;
                }

                let api_key = api_key.clone();
                let worker_pending = pending.clone();
                let result_tx = result_tx.clone();
                let spawn_result = std::thread::Builder::new()
                    .name("acp-peer-auth".to_owned())
                    .spawn(move || {
                        let attempt = if authenticate_peer(&mut stream, &api_key).is_ok() {
                            AuthenticationAttempt::Accepted(stream, peer_addr)
                        } else {
                            AuthenticationAttempt::Rejected(peer_addr)
                        };
                        worker_pending.fetch_sub(1, Ordering::AcqRel);
                        let _ = result_tx.send(attempt);
                    });

                if spawn_result.is_err() {
                    pending.fetch_sub(1, Ordering::AcqRel);
                    tracing::warn!("ACP TCP peer authentication worker could not start");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

struct TcpServerSettings {
    host: String,
    port: u16,
    api_key: String,
}

fn resolve_tcp_settings(cli: &Cli, cfg: &Config) -> anyhow::Result<Option<TcpServerSettings>> {
    let environment_key = std::env::var("MINI_AGENT_ACP_API_KEY").ok();
    resolve_tcp_settings_with_key(cli, cfg, environment_key)
}

fn resolve_tcp_settings_with_key(
    cli: &Cli,
    cfg: &Config,
    environment_key: Option<String>,
) -> anyhow::Result<Option<TcpServerSettings>> {
    let configured_host = cli.acp_host.clone().or_else(|| cfg.acp_host.clone());
    let configured_port = cli.acp_port.or(cfg.acp_port);
    if configured_host.is_none() && configured_port.is_none() {
        return Ok(None);
    }

    let host = configured_host.unwrap_or_else(|| DEFAULT_TCP_HOST.to_owned());
    let port = configured_port.unwrap_or(DEFAULT_TCP_PORT);
    let api_key = environment_key
        .filter(|key| !key.is_empty())
        .or_else(|| configured_tcp_api_key(cfg, &host, port))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "ACP TCP requires authentication; set MINI_AGENT_ACP_API_KEY or configure a matching TCP acp_servers api_key"
            )
        })?;

    if !is_loopback_host(&host) {
        tracing::warn!(
            "ACP TCP remote bind explicitly enabled for {}; authentication is required",
            host
        );
    }

    Ok(Some(TcpServerSettings {
        host,
        port,
        api_key,
    }))
}

fn configured_tcp_api_key(cfg: &Config, host: &str, port: u16) -> Option<String> {
    let mut matching_keys = cfg
        .acp_servers
        .as_ref()?
        .values()
        .filter_map(|server| server.tcp_endpoint())
        .filter(|(configured_host, configured_port, _)| {
            *configured_host == host && *configured_port == port
        })
        .filter_map(|(_, _, api_key)| api_key)
        .filter(|api_key| !api_key.is_empty());

    let first = matching_keys.next()?.to_owned();
    if matching_keys.all(|key| key == first.as_str()) {
        Some(first)
    } else {
        None
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

// --- Server Entry Point ---

pub async fn serve(cli: Cli, cfg: Config, context: ContextFiles) -> anyhow::Result<()> {
    let tcp_settings = resolve_tcp_settings(&cli, &cfg)?;
    let transport_mode = if tcp_settings.is_some() {
        "tcp"
    } else {
        "stdio"
    };
    tracing::info!("ACP server starting: transport={}", transport_mode);

    let state = Arc::new(AcpState {
        cli,
        cfg,
        context,
        sessions: Mutex::new(HashMap::new()),
    });

    let builder = Agent.builder().name("zerostack");

    let builder = builder
        .on_receive_request(
            {
                let state = state.clone();
                move |req: InitializeRequest, responder, _cx| {
                    let state = state.clone();
                    async move { handle_initialize(req, responder, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: NewSessionRequest, responder, cx| {
                    let state = state.clone();
                    async move { handle_new_session(req, responder, cx, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: PromptRequest, responder, cx| {
                    let state = state.clone();
                    async move { handle_prompt(req, responder, cx, state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_dispatch(
            |dispatch: Dispatch<AgentRequest, AgentNotification>, cx: ConnectionTo<Client>| {
                async move {
                    tracing::warn!("ACP unhandled dispatch message");
                    dispatch.respond_with_error(
                        agent_client_protocol::util::internal_error("Unhandled ACP message"),
                        cx,
                    )
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        );

    // Choose transport: TCP if an endpoint is configured, otherwise stdio.
    if let Some(settings) = tcp_settings {
        builder
            .connect_to(TcpTransport {
                host: settings.host,
                port: settings.port,
                api_key: settings.api_key,
            })
            .await
            .map_err(|e| anyhow::anyhow!("ACP TCP server error: {}", e))?;
    } else {
        builder
            .connect_to(Stdio::new())
            .await
            .map_err(|e| anyhow::anyhow!("ACP stdio server error: {}", e))?;
    }

    Ok(())
}

// --- Request Handlers ---

async fn handle_initialize(
    req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    _state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let caps = AgentCapabilities::new();

    let resp = InitializeResponse::new(req.protocol_version)
        .agent_capabilities(caps)
        .agent_info(Implementation::new("zerostack", AGENT_VERSION));

    responder.respond(resp)
}

async fn handle_new_session(
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    _cx: ConnectionTo<Client>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let workspace = Arc::new(canonical_session_workspace(&req.cwd)?);
    let workspace_root = workspace.root();
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());

    tracing::info!(
        "ACP new session: {} (cwd: {})",
        session_id,
        workspace_root.display()
    );

    let context = state
        .context
        .for_workspace_binding(state.cli.resolve_no_context_files(&state.cfg), &workspace);
    workspace.validate().map_err(|error| {
        agent_client_protocol::Error::new(
            -32602,
            format!("ACP session cwd changed while loading context: {error}"),
        )
    })?;

    state.sessions.lock().await.insert(
        session_id.clone(),
        SessionState {
            messages: Vec::new(),
            workspace,
            context: Arc::new(context),
        },
    );

    let resp = NewSessionResponse::new(session_id);
    responder.respond(resp)
}

fn canonical_session_workspace(
    path: &Path,
) -> Result<crate::paths::WorkspaceBinding, agent_client_protocol::Error> {
    crate::paths::WorkspaceBinding::capture(path).map_err(|error| {
        agent_client_protocol::Error::new(
            -32602,
            format!("invalid ACP session cwd '{}': {error}", path.display()),
        )
    })
}

async fn handle_prompt(
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    state: Arc<AcpState>,
) -> Result<(), agent_client_protocol::Error> {
    let session_id = req.session_id.clone();

    tracing::info!("ACP prompt for session {}", session_id);

    let prompt_text = req
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Append user message to session history
    let (workspace, context) = {
        let mut sessions = state.sessions.lock().await;
        let sess = sessions
            .get_mut(&session_id)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown ACP session"))?;
        sess.messages
            .push(("user".to_string(), prompt_text.clone()));
        (sess.workspace.clone(), sess.context.clone())
    };

    cx.spawn({
        let cx = cx.clone();
        async move {
            run_prompt(
                &state,
                &prompt_text,
                session_id,
                workspace,
                context,
                responder,
                cx,
            )
            .await
        }
    })
}

// --- Prompt Execution ---

async fn run_prompt(
    state: &AcpState,
    prompt_text: &str,
    session_id: SessionId,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
    workspace
        .validate()
        .map_err(|error| agent_client_protocol::Error::new(-32603, error))?;
    let workspace_root = workspace.root();
    let (authority, sandbox) =
        crate::permission::resolve_configured_execution_authority(&state.cli, &state.cfg)
            .map_err(|error| agent_client_protocol::Error::new(-32603, error.to_string()))?;
    let (permission, ask_tx) = crate::permission::build_noninteractive_permission_at(
        &state.cfg,
        authority,
        Some(workspace_root.to_path_buf()),
    );
    let sandbox = sandbox.with_workspace_binding(workspace.clone());

    let provider_str = state.cli.resolve_provider(&state.cfg);
    let mut model_str = state.cli.resolve_model(&state.cfg);

    tracing::debug!(
        "ACP run_prompt: provider={}, model={}, prompt_len={}",
        provider_str,
        model_str,
        prompt_text.len(),
    );

    // Custom provider model override (if no explicit model set)
    if (model_str.as_str() == "deepseek/deepseek-v4-pro" || state.cli.model.is_none())
        && let Some(custom) = state.cfg.custom_providers_map().get(provider_str.as_str())
        && let Some(ref custom_model) = custom.model
    {
        model_str = custom_model.clone();
    }

    let client = crate::provider::create_client(
        &provider_str,
        None,
        &state.cfg.custom_providers_map(),
        state.cfg.api_keys.as_ref(),
    )
    .map_err(|e| agent_client_protocol::Error::new(-32603, e.to_string()))?;

    let model = client.completion_model(model_str.to_string());

    // Track session history for future context persistence
    let _extra_messages = {
        let sessions = state.sessions.lock().await;
        sessions
            .get(&session_id)
            .map(|s| s.messages.clone())
            .unwrap_or_default()
    };

    let temperature = crate::config::resolve_temperature(&state.cli, &state.cfg, &model_str);
    let extra_body = crate::config::resolve_extra_body(&state.cfg, &model_str);
    let agent = crate::provider::build_agent_in_workspace(
        model,
        &state.cli,
        &state.cfg,
        &context,
        workspace,
        permission,
        ask_tx,
        sandbox,
        false,
        temperature,
        extra_body,
        #[cfg(feature = "mcp")]
        None::<&crate::extras::mcp::McpClientManager>,
    )
    .await;

    let runner = agent
        .spawn_runner(
            prompt_text.to_string(),
            vec![],
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    let mut rx = runner.event_rx;

    let mut tool_call_id: Option<ToolCallId> = None;
    let mut final_response = String::new();

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Token(text) => {
                final_response.push_str(&text);
                let chunk =
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(chunk),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send token notification: {}", e);
                }
            }
            AgentEvent::Reasoning(text) => {
                let chunk =
                    ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(chunk),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send reasoning notification: {}", e);
                }
            }
            AgentEvent::ToolCall { name, args } => {
                let id = ToolCallId::new(uuid::Uuid::new_v4().to_string());
                tool_call_id = Some(id.clone());
                let args_str = args.to_string();
                let tool_call = ToolCall::new(id.clone(), name.to_string())
                    .raw_input(serde_json::from_str(&args_str).ok());
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCall(tool_call),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send tool call notification: {}", e);
                }
            }
            AgentEvent::SubagentToolCall { name, args } => {
                let id = ToolCallId::new(uuid::Uuid::new_v4().to_string());
                tool_call_id = Some(id.clone());
                let args_str = args.to_string();
                let tool_call = ToolCall::new(id.clone(), format!("[subagent] {}", name))
                    .raw_input(serde_json::from_str(&args_str).ok());
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCall(tool_call),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send subagent tool call notification: {}", e);
                }
            }
            AgentEvent::ToolResult { output, .. } => {
                let id = tool_call_id
                    .take()
                    .unwrap_or_else(|| ToolCallId::new(uuid::Uuid::new_v4().to_string()));
                let fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(output.to_string()),
                    ))]);
                let update = ToolCallUpdate::new(id, fields);
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCallUpdate(update),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send tool result notification: {}", e);
                }
            }
            AgentEvent::Retrying { attempt, max } => {
                // ACP has no status bar, so surface the retry as an agent
                // thought. This keeps the client from going silent during the
                // backoff delay and mirrors how `Reasoning` is forwarded.
                let text = format!("retrying... ({}/{})", attempt, max);
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(chunk),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send retry notification: {}", e);
                }
            }
            AgentEvent::CompletionCall { .. } => {
                // Mid-stream provider usage; ACP has no status bar to update, so
                // there is nothing to surface for this event.
            }
            AgentEvent::Done { .. } => {
                break;
            }
            AgentEvent::Error(err) => {
                // Surface the error to the client instead of silently
                // reporting EndTurn.
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(format!(
                    "[error: {}]",
                    err
                ))));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(chunk),
                );
                let _ = cx.send_notification(notif);
                let _ = responder.respond(PromptResponse::new(StopReason::Refusal));
                return Ok(());
            }
        }
    }

    // Store assistant response in session history
    if !final_response.is_empty() {
        let mut sessions = state.sessions.lock().await;
        if let Some(sess) = sessions.get_mut(&session_id) {
            sess.messages
                .push(("assistant".to_string(), final_response));
        }
    }

    let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
    Ok(())
}

#[cfg(test)]
mod workspace_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use rig::tool::Tool;

    use super::*;
    use crate::agent::tools::{
        BashArgs, BashTool, EditArgs, EditTool, FindFilesArgs, FindFilesTool, GrepArgs, GrepTool,
        ListDirArgs, ListDirTool, ReadArgs, ReadTool, WriteArgs, WriteTool,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mini-agent-acp-workspace-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn roots() -> (TempDir, PathBuf, PathBuf) {
        let container = TempDir::new();
        let first = container.path().join("first");
        let second = container.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("sentinel.txt"), "first-only").unwrap();
        std::fs::write(second.join("sentinel.txt"), "second-only").unwrap();
        std::fs::write(first.join("AGENTS.md"), "FIRST_CONTEXT_SENTINEL").unwrap();
        std::fs::write(second.join("AGENTS.md"), "SECOND_CONTEXT_SENTINEL").unwrap();
        std::fs::write(first.join("ARCHITECTURE.md"), "FIRST_ARCHITECTURE_SENTINEL").unwrap();
        std::fs::write(
            second.join("ARCHITECTURE.md"),
            "SECOND_ARCHITECTURE_SENTINEL",
        )
        .unwrap();
        std::fs::create_dir_all(first.join(".zerostack/prompts")).unwrap();
        std::fs::create_dir_all(second.join(".zerostack/prompts")).unwrap();
        std::fs::write(
            first.join(".zerostack/prompts/acp-root.md"),
            "FIRST_PROMPT_SENTINEL",
        )
        .unwrap();
        std::fs::write(
            second.join(".zerostack/prompts/acp-root.md"),
            "SECOND_PROMPT_SENTINEL",
        )
        .unwrap();
        (
            container,
            first.canonicalize().unwrap(),
            second.canonicalize().unwrap(),
        )
    }

    #[test]
    fn session_workspace_is_canonical_and_must_be_a_directory() {
        let (_container, first, _) = roots();
        assert_eq!(
            canonical_session_workspace(&first).unwrap().root(),
            first.canonicalize().unwrap()
        );

        let file = first.join("sentinel.txt");
        assert!(canonical_session_workspace(&file).is_err());
        assert!(canonical_session_workspace(&first.join("missing")).is_err());
    }

    #[tokio::test]
    async fn concurrent_workspace_context_and_core_tools_remain_isolated() {
        let (_container, first, second) = roots();
        let base_context = crate::context::load(true);
        let first_context = base_context.for_workspace(false, &first);
        let second_context = base_context.for_workspace(false, &second);
        let first_agents = first_context.agents.as_deref().unwrap_or_default();
        let second_agents = second_context.agents.as_deref().unwrap_or_default();
        assert!(first_agents.contains("FIRST_CONTEXT_SENTINEL"));
        assert!(!first_agents.contains("SECOND_CONTEXT_SENTINEL"));
        assert!(second_agents.contains("SECOND_CONTEXT_SENTINEL"));
        assert!(!second_agents.contains("FIRST_CONTEXT_SENTINEL"));
        assert!(
            first_context
                .architecture
                .as_deref()
                .unwrap_or_default()
                .contains("FIRST_ARCHITECTURE_SENTINEL")
        );
        assert!(
            second_context
                .architecture
                .as_deref()
                .unwrap_or_default()
                .contains("SECOND_ARCHITECTURE_SENTINEL")
        );
        assert_eq!(first_context.prompts["acp-root"], "FIRST_PROMPT_SENTINEL");
        assert_eq!(second_context.prompts["acp-root"], "SECOND_PROMPT_SENTINEL");
        let first_preamble = crate::agent::builder::build_preamble_for_workspace(
            &first_context,
            false,
            Some(&first),
        );
        assert!(first_preamble.contains(&first.display().to_string()));
        assert!(!first_preamble.contains(&second.display().to_string()));

        let first_read = ReadTool::new(None, None, None, 100).with_workspace_root(first.clone());
        let second_read = ReadTool::new(None, None, None, 100).with_workspace_root(second.clone());
        let (first_value, second_value) = tokio::join!(
            first_read.call(ReadArgs {
                path: "sentinel.txt".into(),
                offset: None,
                limit: None,
            }),
            second_read.call(ReadArgs {
                path: "sentinel.txt".into(),
                offset: None,
                limit: None,
            })
        );
        assert!(first_value.unwrap().contains("first-only"));
        assert!(second_value.unwrap().contains("second-only"));

        let first_write = WriteTool::new(None, None, None).with_workspace_root(first.clone());
        let second_write = WriteTool::new(None, None, None).with_workspace_root(second.clone());
        let (first_result, second_result) = tokio::join!(
            first_write.call(WriteArgs {
                path: "created.txt".into(),
                content: "created-first".into(),
            }),
            second_write.call(WriteArgs {
                path: "created.txt".into(),
                content: "created-second".into(),
            })
        );
        first_result.unwrap();
        second_result.unwrap();
        assert_eq!(
            std::fs::read_to_string(first.join("created.txt")).unwrap(),
            "created-first"
        );
        assert_eq!(
            std::fs::read_to_string(second.join("created.txt")).unwrap(),
            "created-second"
        );

        let first_workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&first).unwrap());
        let second_workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&second).unwrap());
        let first_grep_tool =
            GrepTool::new(None, None, 100).with_workspace_binding(first_workspace.clone());
        let second_grep_tool =
            GrepTool::new(None, None, 100).with_workspace_binding(second_workspace.clone());
        let (first_grep, second_grep) = tokio::join!(
            first_grep_tool.call(GrepArgs {
                pattern: "first-only".into(),
                path: Some(".".into()),
                include: None,
                context_lines: None,
            }),
            second_grep_tool.call(GrepArgs {
                pattern: "second-only".into(),
                path: Some(".".into()),
                include: None,
                context_lines: None,
            })
        );
        assert!(first_grep.unwrap().contains("first-only"));
        assert!(second_grep.unwrap().contains("second-only"));

        let first_find = FindFilesTool::new(None, None, 100)
            .with_workspace_binding(first_workspace.clone())
            .call(FindFilesArgs {
                pattern: "sentinel".into(),
                path: Some(".".into()),
            })
            .await
            .unwrap();
        let second_list = ListDirTool::new(None, None, Some(100))
            .with_workspace_binding(second_workspace.clone())
            .call(ListDirArgs {
                path: Some(".".into()),
            })
            .await
            .unwrap();
        assert!(first_find.contains("sentinel.txt"));
        assert!(second_list.contains("sentinel.txt"));

        EditTool::new(None, None)
            .with_workspace_binding(first_workspace)
            .call(EditArgs {
                path: "created.txt".into(),
                block: Some(
                    "<<<<<<< SEARCH\ncreated-first\n=======\nedited-first\n>>>>>>> REPLACE".into(),
                ),
                file_crc: None,
                edits: None,
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(first.join("created.txt")).unwrap(),
            "edited-first"
        );
        assert_eq!(
            std::fs::read_to_string(second.join("created.txt")).unwrap(),
            "created-second"
        );

        let first_bash = BashTool::new(
            None,
            None,
            Sandbox::new(false, "bwrap").with_workspace_binding(Arc::new(
                crate::paths::WorkspaceBinding::capture(&first).unwrap(),
            )),
            None,
        );
        let second_bash = BashTool::new(
            None,
            None,
            Sandbox::new(false, "bwrap").with_workspace_binding(Arc::new(
                crate::paths::WorkspaceBinding::capture(&second).unwrap(),
            )),
            None,
        );
        let (first_pwd, second_pwd) = tokio::join!(
            first_bash.call(BashArgs {
                command: "pwd; cat sentinel.txt".into(),
                timeout: None,
            }),
            second_bash.call(BashArgs {
                command: "pwd; cat sentinel.txt".into(),
                timeout: None,
            })
        );
        let first_pwd = first_pwd.unwrap();
        let second_pwd = second_pwd.unwrap();
        assert!(first_pwd.contains(&first.display().to_string()));
        assert!(first_pwd.contains("first-only"));
        assert!(second_pwd.contains(&second.display().to_string()));
        assert!(second_pwd.contains("second-only"));
    }

    #[tokio::test]
    async fn relative_parent_escape_is_denied_by_session_permission_root() {
        let (_container, first, second) = roots();
        let cli = Cli::default();
        let cfg = Config::default();
        let (authority, _) =
            crate::permission::resolve_configured_execution_authority(&cli, &cfg).unwrap();
        let (permission, ask_tx) = crate::permission::build_noninteractive_permission_at(
            &cfg,
            authority,
            Some(first.clone()),
        );
        let tool = ReadTool::new(permission, ask_tx, None, 100).with_workspace_root(first.clone());
        let relative_escape = PathBuf::from("..")
            .join(second.file_name().unwrap())
            .join("sentinel.txt");
        let error = tool
            .call(ReadArgs {
                path: relative_escape.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap_err();
        let error = error.to_string();
        assert!(
            error.contains("Permission denied")
                || error.contains("workspace capability requires a contained relative path")
        );
        assert!(!error.contains("second-only"));
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp_relative_paths_resolve_under_each_session_root() {
        let (_container, first, second) = roots();
        let cfg = crate::config::types::LspConfig::default();
        let first_lsp = crate::extras::lsp::LspManager::new(
            &cfg,
            Arc::new(crate::paths::WorkspaceBinding::capture(&first).unwrap()),
        );
        let second_lsp = crate::extras::lsp::LspManager::new(
            &cfg,
            Arc::new(crate::paths::WorkspaceBinding::capture(&second).unwrap()),
        );
        assert_eq!(
            first_lsp.resolve_path(Path::new("sentinel.txt")),
            Ok(first.join("sentinel.txt"))
        );
        assert_eq!(
            second_lsp.resolve_path(Path::new("sentinel.txt")),
            Ok(second.join("sentinel.txt"))
        );
        assert!(
            first_lsp
                .resolve_path(&second.join("sentinel.txt"))
                .is_err()
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                second.join("sentinel.txt"),
                first.join("linked-sentinel.txt"),
            )
            .unwrap();
            assert!(
                first_lsp
                    .resolve_path(Path::new("linked-sentinel.txt"))
                    .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[cfg(feature = "js")]
    #[tokio::test]
    async fn replacing_a_session_root_fails_every_bound_effect_closed() {
        use crate::extras::js::host::AllowConfig;
        use crate::extras::js::tool::{JsArgs, JsTool};

        let (_container, first, second) = roots();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&first).unwrap());
        let read = ReadTool::new(None, None, None, 100).with_workspace_binding(workspace.clone());
        let write = WriteTool::new(None, None, None).with_workspace_binding(workspace.clone());
        let sandbox = Sandbox::new(false, "bwrap").with_workspace_binding(workspace.clone());
        let bash = BashTool::new(None, None, sandbox.clone(), None);
        let roots = vec![".".to_string()];
        let js = JsTool::new(
            sandbox,
            None,
            None,
            AllowConfig::from_settings(&first, None, Some(&roots), Some(&roots), false, false)
                .with_workspace_binding(workspace.clone()),
        );
        #[cfg(feature = "lsp")]
        let lsp = crate::extras::lsp::LspManager::new(
            &crate::config::types::LspConfig::default(),
            workspace.clone(),
        );

        let original = first.with_file_name("first-original");
        std::fs::rename(&first, &original).unwrap();
        std::os::unix::fs::symlink(&second, &first).unwrap();

        assert!(workspace.validate().is_err());
        assert!(
            read.call(ReadArgs {
                path: "sentinel.txt".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("workspace binding")
        );
        assert!(
            write
                .call(WriteArgs {
                    path: "rebound.txt".into(),
                    content: "must-not-write".into(),
                })
                .await
                .is_err()
        );
        assert!(
            bash.call(BashArgs {
                command: "cat sentinel.txt".into(),
                timeout: None,
            })
            .await
            .is_err()
        );
        let js_result = js
            .call(JsArgs {
                code: "read_file('sentinel.txt')".into(),
            })
            .await;
        if let Ok(value) = js_result {
            assert!(!value.contains("second-only"));
        }
        #[cfg(feature = "lsp")]
        assert!(lsp.resolve_path(Path::new("sentinel.txt")).is_err());
        assert!(!second.join("rebound.txt").exists());

        std::fs::remove_file(&first).unwrap();
        std::fs::rename(original, first).unwrap();
    }

    #[cfg(all(unix, feature = "js"))]
    #[tokio::test]
    async fn core_and_javascript_effects_reject_in_workspace_symlink_traversal() {
        use crate::extras::js::host::AllowConfig;
        use crate::extras::js::tool::{JsArgs, JsTool};

        let (_container, first, _) = roots();
        std::fs::create_dir_all(first.join("safe")).unwrap();
        std::fs::create_dir_all(first.join("secret")).unwrap();
        std::fs::write(first.join("secret/value.txt"), "secret-value").unwrap();
        std::os::unix::fs::symlink("../secret/value.txt", first.join("safe/link.txt")).unwrap();
        std::os::unix::fs::symlink("../secret", first.join("safe/link-dir")).unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&first).unwrap());

        let read = ReadTool::new(None, None, None, 100)
            .with_workspace_binding(workspace.clone())
            .call(ReadArgs {
                path: "safe/link.txt".into(),
                offset: None,
                limit: None,
            })
            .await;
        assert!(read.is_err());
        let write = WriteTool::new(None, None, None)
            .with_workspace_binding(workspace.clone())
            .call(WriteArgs {
                path: "safe/link-dir/core.txt".into(),
                content: "must-not-write".into(),
            })
            .await;
        assert!(write.is_err());

        let roots = vec!["safe".to_string()];
        let js = JsTool::new(
            Sandbox::new(false, "bwrap").with_workspace_binding(workspace.clone()),
            None,
            None,
            AllowConfig::from_settings(&first, None, Some(&roots), Some(&roots), false, false)
                .with_workspace_binding(workspace),
        );
        let js_read = js
            .call(JsArgs {
                code: "read_file('safe/link.txt')".into(),
            })
            .await;
        if let Ok(value) = &js_read {
            assert!(value.starts_with("JS error:"), "the JS read must fail");
            assert!(
                !value.contains("secret-value"),
                "a JS read must not reveal a symlink target"
            );
        }
        let js_write = js
            .call(JsArgs {
                code: "write_file('safe/link-dir/js.txt', 'must-not-write')".into(),
            })
            .await;
        if let Ok(value) = &js_write {
            assert!(value.starts_with("JS error:"), "the JS write must fail");
        }
        assert!(!first.join("secret/core.txt").exists());
        assert!(!first.join("secret/js.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capability_handles_defeat_root_aba_between_check_and_effect() {
        use std::io::Read as _;

        let (_container, first, second) = roots();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&first).unwrap());
        let sandbox = Sandbox::new(false, "bwrap").with_workspace_binding(workspace.clone());
        let mut command = sandbox.wrap_command("pwd; cat sentinel.txt").unwrap();
        workspace.validate().unwrap();

        let original = first.with_file_name("first-original");
        let replacement = second.with_file_name("second-original");
        std::fs::rename(&first, &original).unwrap();
        std::fs::rename(&second, &replacement).unwrap();
        std::fs::rename(&replacement, &first).unwrap();

        let mut content = String::new();
        workspace
            .open_relative(Path::new("sentinel.txt"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "first-only");
        workspace
            .create_relative_atomic(Path::new("handle-created.txt"), b"first-handle")
            .unwrap();
        assert!(!first.join("handle-created.txt").exists());
        assert_eq!(
            std::fs::read_to_string(original.join("handle-created.txt")).unwrap(),
            "first-handle"
        );
        let context = crate::context::load(true).for_workspace_binding(false, &workspace);
        assert!(
            context
                .agents
                .as_deref()
                .unwrap_or_default()
                .contains("FIRST_CONTEXT_SENTINEL")
        );
        assert!(
            !context
                .agents
                .as_deref()
                .unwrap_or_default()
                .contains("SECOND_CONTEXT_SENTINEL")
        );

        let output = command.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("first-only"));
        assert!(!stdout.contains("second-only"));

        std::fs::rename(&first, &second).unwrap();
        std::fs::rename(&original, &first).unwrap();
    }

    #[cfg(feature = "js")]
    #[tokio::test]
    async fn javascript_relative_files_and_spawn_use_the_session_root() {
        use crate::extras::js::host::AllowConfig;
        use crate::extras::js::tool::{JsArgs, JsTool};

        let (_container, first, second) = roots();
        let make_tool = |root: &Path| {
            let roots = vec![".".to_string()];
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root).unwrap());
            JsTool::new(
                Sandbox::new(false, "bwrap").with_workspace_binding(workspace.clone()),
                None,
                None,
                AllowConfig::from_settings(root, None, Some(&roots), Some(&roots), false, false)
                    .with_workspace_binding(workspace),
            )
        };
        let first_tool = make_tool(&first);
        let second_tool = make_tool(&second);
        assert_eq!(
            first_tool
                .call(JsArgs {
                    code: "read_file('sentinel.txt')".into(),
                })
                .await
                .unwrap(),
            "first-only"
        );
        let write_probe = first_tool
            .call(JsArgs {
                code: "write_file('probe.txt', 'probe'); 'ok'".into(),
            })
            .await
            .unwrap();
        assert_eq!(write_probe, "ok");
        assert_eq!(
            std::fs::read_to_string(first.join("probe.txt")).unwrap(),
            "probe"
        );
        let code =
            "write_file('js-created.txt', read_file('sentinel.txt')); read_file('js-created.txt')";
        let (first_value, second_value) = tokio::join!(
            first_tool.call(JsArgs { code: code.into() }),
            second_tool.call(JsArgs { code: code.into() })
        );
        assert_eq!(first_value.unwrap(), "first-only");
        assert_eq!(second_value.unwrap(), "second-only");
        assert_eq!(
            std::fs::read_to_string(first.join("js-created.txt")).unwrap(),
            "first-only"
        );
        assert_eq!(
            std::fs::read_to_string(second.join("js-created.txt")).unwrap(),
            "second-only"
        );

        let first_spawn = first_tool
            .call(JsArgs {
                code: "spawn('sh', ['-c', 'pwd']).stdout.trim()".into(),
            })
            .await
            .unwrap();
        let second_spawn = second_tool
            .call(JsArgs {
                code: "spawn('sh', ['-c', 'pwd']).stdout.trim()".into(),
            })
            .await
            .unwrap();
        assert_eq!(first_spawn, first.display().to_string());
        assert_eq!(second_spawn, second.display().to_string());
    }
}

#[cfg(test)]
mod tcp_authentication_tests {
    use super::*;
    use crate::acp_auth::{read_challenge, send_response};
    use crate::extras::acp::config::AcpServerConfig;

    fn tcp_config(host: &str, port: u16, api_key: Option<&str>) -> Config {
        Config {
            acp_servers: Some(HashMap::from([(
                "test".to_owned(),
                AcpServerConfig::Tcp {
                    host: host.to_owned(),
                    port,
                    api_key: api_key.map(str::to_owned),
                },
            )])),
            ..Default::default()
        }
    }

    #[test]
    fn stdio_remains_default_without_tcp_endpoint() {
        let settings =
            resolve_tcp_settings_with_key(&Cli::default(), &Config::default(), None).unwrap();
        assert!(settings.is_none());
    }

    #[test]
    fn port_only_tcp_configuration_defaults_to_loopback() {
        let cli = Cli {
            acp_port: Some(8123),
            ..Default::default()
        };
        let cfg = tcp_config(DEFAULT_TCP_HOST, 8123, Some("configured-key"));

        let settings = resolve_tcp_settings_with_key(&cli, &cfg, None)
            .unwrap()
            .unwrap();
        assert_eq!(settings.host, DEFAULT_TCP_HOST);
        assert_eq!(settings.port, 8123);
        assert_eq!(settings.api_key, "configured-key");
    }

    #[test]
    fn tcp_configuration_without_authentication_fails_closed() {
        let cli = Cli {
            acp_host: Some(DEFAULT_TCP_HOST.to_owned()),
            ..Default::default()
        };

        let error = resolve_tcp_settings_with_key(&cli, &Config::default(), None)
            .err()
            .expect("TCP without authentication must fail");
        assert!(error.to_string().contains("requires authentication"));
    }

    #[test]
    fn racing_valid_peer_is_not_blocked_by_partial_peer() {
        let listener = TcpListener::bind((DEFAULT_TCP_HOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            accept_authenticated_peer(listener, "configured-key".to_owned()).unwrap()
        });

        let mut partial = TcpStream::connect(address).unwrap();
        let _ = read_challenge(&mut partial).unwrap();

        let mut valid = TcpStream::connect(address).unwrap();
        let nonce = read_challenge(&mut valid).unwrap();
        send_response(&mut valid, &nonce, "configured-key").unwrap();
        let valid_address = valid.local_addr().unwrap();

        let (_, authenticated_address) = server.join().unwrap();
        assert_eq!(authenticated_address, valid_address);
    }
}
