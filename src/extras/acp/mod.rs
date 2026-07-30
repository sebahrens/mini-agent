pub mod config;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
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
use crate::permission::SecurityMode;
use crate::sandbox::Sandbox;

const AGENT_VERSION: &str = "1.0.5";
const DEFAULT_TCP_HOST: &str = "127.0.0.1";
const DEFAULT_TCP_PORT: u16 = 7243;
const MAX_PENDING_AUTHENTICATIONS: usize = 16;

struct SessionState {
    messages: Vec<(String, String)>,
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
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());

    tracing::info!(
        "ACP new session: {} (cwd: {})",
        session_id,
        req.cwd.display()
    );

    state.sessions.lock().await.insert(
        session_id.clone(),
        SessionState {
            messages: Vec::new(),
        },
    );

    let resp = NewSessionResponse::new(session_id);
    responder.respond(resp)
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
    {
        let mut sessions = state.sessions.lock().await;
        if let Some(sess) = sessions.get_mut(&session_id) {
            sess.messages
                .push(("user".to_string(), prompt_text.clone()));
        }
    }

    cx.spawn({
        let cx = cx.clone();
        async move { run_prompt(&state, &prompt_text, session_id, responder, cx).await }
    })
}

// --- Prompt Execution ---

async fn run_prompt(
    state: &AcpState,
    prompt_text: &str,
    session_id: SessionId,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
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

    let mode = resolve_acp_mode(&state.cli, &state.cfg);
    let (permission, ask_tx) =
        crate::permission::build_noninteractive_permission(&state.cli, &state.cfg, mode);
    let sandbox = Sandbox::new(
        state.cli.resolve_sandbox(&state.cfg),
        &state.cli.resolve_sandbox_backend(&state.cfg),
    )
    .with_shell(&state.cli.resolve_shell(&state.cfg));

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
    let agent = crate::provider::build_agent(
        model,
        &state.cli,
        &state.cfg,
        &state.context,
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

pub(crate) fn resolve_acp_mode(cli: &Cli, cfg: &Config) -> SecurityMode {
    if cli.dangerously_skip_permissions {
        SecurityMode::Standard
    } else if cli.yolo || cfg.yolo.unwrap_or(false) {
        SecurityMode::Yolo
    } else if cli.accept_all || cfg.accept_all.unwrap_or(false) {
        SecurityMode::Standard
    } else if cli.restrictive || cfg.restrictive.unwrap_or(false) {
        SecurityMode::Restrictive
    } else if let Some(m) = &cfg.default_permission_mode {
        match m.as_str() {
            "yolo" => SecurityMode::Yolo,
            "accept" | "standard" => SecurityMode::Standard,
            "guarded" => SecurityMode::Guarded,
            "readonly" => SecurityMode::ReadOnly,
            "restrictive" => SecurityMode::Restrictive,
            _ => SecurityMode::Standard,
        }
    } else {
        SecurityMode::Standard
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
