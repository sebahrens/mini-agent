pub mod config;

use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::future::Future;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use agent_client_protocol::on_receive_request;
use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectTo, ConnectionTo, Dispatch, RequestCancellation, Responder,
    Role, Stdio,
};
use rig::completion::Message;
use tokio::sync::{Mutex, Notify};

use crate::acp_auth::authenticate_peer;
use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::event::AgentEvent;
#[cfg(test)]
use crate::sandbox::Sandbox;

const AGENT_VERSION: &str = "1.0.5";
const DEFAULT_TCP_HOST: &str = "127.0.0.1";
const DEFAULT_TCP_PORT: u16 = 7243;
const MAX_PENDING_AUTHENTICATIONS: usize = 16;
const MAX_ACP_HISTORY_TURNS: usize = 128;
const MAX_ACP_HISTORY_BYTES: usize = 2 * 1024 * 1024;
const MAX_ACP_SESSIONS: usize = 64;

struct CommittedTurn {
    messages: Vec<Message>,
    serialized_bytes: usize,
}

#[derive(Default)]
struct SessionHistory {
    turns: VecDeque<CommittedTurn>,
    serialized_bytes: usize,
}

impl SessionHistory {
    fn snapshot(&self) -> Vec<Message> {
        self.turns
            .iter()
            .flat_map(|turn| turn.messages.iter().cloned())
            .collect()
    }

    fn commit_completed_turn(&mut self, prompt: &str, interactions: Vec<Message>) {
        let mut messages = Vec::with_capacity(interactions.len() + 1);
        messages.push(Message::user(prompt));
        messages.extend(interactions);

        // Rig messages are serializable. Treat an unexpected serialization failure
        // as oversized so history remains bounded instead of retaining it forever.
        let serialized_bytes = serde_json::to_vec(&messages)
            .map(|encoded| encoded.len())
            .unwrap_or(MAX_ACP_HISTORY_BYTES.saturating_add(1));
        self.serialized_bytes = self.serialized_bytes.saturating_add(serialized_bytes);
        self.turns.push_back(CommittedTurn {
            messages,
            serialized_bytes,
        });

        while self.turns.len() > MAX_ACP_HISTORY_TURNS
            || self.serialized_bytes > MAX_ACP_HISTORY_BYTES
        {
            let Some(evicted) = self.turns.pop_front() else {
                break;
            };
            self.serialized_bytes = self
                .serialized_bytes
                .saturating_sub(evicted.serialized_bytes);
        }
    }
}

fn acp_capabilities() -> AgentCapabilities {
    // Session history is intentionally process-local; session/load is unsupported.
    AgentCapabilities::new()
}

struct SessionState {
    history: Arc<Mutex<SessionHistory>>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
    turns: Arc<StdMutex<SessionTurns>>,
}

const TURN_ACTIVE: u8 = 0;
const TURN_CANCELLED: u8 = 1;
const TURN_COMPLETED: u8 = 2;

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TurnControl {
    state: AtomicU8,
    cancelled: Notify,
    runner_cancel: StdMutex<Option<crate::agent::runner::AgentWorkCancellation>>,
}

impl TurnControl {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(TURN_ACTIVE),
            cancelled: Notify::new(),
            runner_cancel: StdMutex::new(None),
        }
    }

    fn cancel(&self) -> bool {
        if self
            .state
            .compare_exchange(
                TURN_ACTIVE,
                TURN_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        if let Some(cancel) = lock_unpoisoned(&self.runner_cancel).as_ref() {
            cancel.cancel();
        }
        self.cancelled.notify_waiters();
        true
    }

    fn attach_runner(&self, cancel: crate::agent::runner::AgentWorkCancellation) -> bool {
        let mut slot = lock_unpoisoned(&self.runner_cancel);
        if self.state.load(Ordering::Acquire) != TURN_ACTIVE {
            cancel.cancel();
            return false;
        }
        *slot = Some(cancel);
        true
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == TURN_CANCELLED
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.cancelled.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    fn complete(&self) -> bool {
        self.state
            .compare_exchange(
                TURN_ACTIVE,
                TURN_COMPLETED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

struct RegisteredTurn {
    generation: u64,
    control: Arc<TurnControl>,
}

#[derive(Default)]
struct SessionTurns {
    next_generation: u64,
    queue: VecDeque<RegisteredTurn>,
}

struct TurnRegistration {
    generation: u64,
    turns: Arc<StdMutex<SessionTurns>>,
    control: Arc<TurnControl>,
}

impl TurnRegistration {
    fn complete_and_settle(&self) -> bool {
        let mut turns = lock_unpoisoned(&self.turns);
        let completed = self.control.complete();
        if let Some(index) = turns
            .queue
            .iter()
            .position(|turn| turn.generation == self.generation)
        {
            turns.queue.remove(index);
        }
        completed
    }

    fn settle(&self) {
        let mut turns = lock_unpoisoned(&self.turns);
        if let Some(index) = turns
            .queue
            .iter()
            .position(|turn| turn.generation == self.generation)
        {
            turns.queue.remove(index);
        }
    }
}

struct RequestCancellationBridge {
    task: tokio::task::AbortHandle,
}

struct CancelTurnOnDrop(Arc<TurnControl>);

impl Drop for CancelTurnOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl RequestCancellationBridge {
    fn new(cancellation: RequestCancellation, control: Arc<TurnControl>) -> Self {
        let task = tokio::spawn(async move {
            cancellation.cancelled().await;
            control.cancel();
        });
        Self {
            task: task.abort_handle(),
        }
    }
}

impl Drop for RequestCancellationBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Drop for TurnRegistration {
    fn drop(&mut self) {
        // Connection/task teardown must not detach a live provider or tool task.
        // Completed and already-cancelled generations make this a no-op.
        self.control.cancel();
        self.settle();
    }
}

struct AcpState {
    cli: Cli,
    cfg: Config,
    context: ContextFiles,
    sessions: Mutex<HashMap<SessionId, SessionState>>,
    cancel_routes: StdMutex<HashMap<SessionId, Arc<StdMutex<SessionTurns>>>>,
    #[cfg(test)]
    prompt_fixture: Option<PromptFixture>,
    #[cfg(test)]
    runner_fixture: Option<RunnerFixture>,
    #[cfg(test)]
    prompt_exit_barrier: Option<Arc<PromptExitBarrier>>,
}

#[cfg(test)]
struct PromptExitBarrier {
    generation: u64,
    entered: Notify,
    release: Notify,
}

#[cfg(test)]
type PromptFixture = Arc<
    dyn Fn(
            String,
            Vec<Message>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<AgentEvent>, String>> + Send>>
        + Send
        + Sync,
>;

#[cfg(test)]
type RunnerFixture = Arc<
    dyn Fn(
            String,
            Vec<Message>,
        ) -> Pin<Box<dyn Future<Output = crate::agent::runner::PausedAgentRunner> + Send>>
        + Send
        + Sync,
>;

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
        cancel_routes: StdMutex::new(HashMap::new()),
        #[cfg(test)]
        prompt_fixture: None,
        #[cfg(test)]
        runner_fixture: None,
        #[cfg(test)]
        prompt_exit_barrier: None,
    });

    // Choose transport: TCP if an endpoint is configured, otherwise stdio.
    if let Some(settings) = tcp_settings {
        connect_agent(
            state,
            TcpTransport {
                host: settings.host,
                port: settings.port,
                api_key: settings.api_key,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("ACP TCP server error: {}", e))?;
    } else {
        connect_agent(state, Stdio::new())
            .await
            .map_err(|e| anyhow::anyhow!("ACP stdio server error: {}", e))?;
    }

    Ok(())
}

async fn connect_agent(
    state: Arc<AcpState>,
    transport: impl ConnectTo<Agent> + 'static,
) -> Result<(), agent_client_protocol::Error> {
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
        .on_receive_notification(
            {
                let state = state.clone();
                move |notification: CancelNotification, _cx| {
                    // Both capture and state transition are synchronous at
                    // notification receipt. The returned future may be polled
                    // after this generation settles, so it performs no
                    // cancellation work and cannot lose a race to `Done`.
                    let generation = receive_cancel(&state, &notification.session_id);
                    async move {
                        if let Some(generation) = generation {
                            tracing::debug!(generation, "ACP cancellation dispatch completed");
                        }
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_notification!(),
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

    builder.connect_to(transport).await
}

fn receive_cancel(state: &AcpState, session_id: &SessionId) -> Option<u64> {
    let turns = lock_unpoisoned(&state.cancel_routes)
        .get(session_id)
        .cloned();
    let generation = turns.and_then(|turns| {
        let turns = lock_unpoisoned(&turns);
        turns.queue.front().map(|turn| {
            // Keep the queue lock across the state transition. Successful
            // completion uses the same lock, so cancel-vs-Done has one
            // generation-specific linearization order.
            turn.control.cancel();
            turn.generation
        })
    });
    if generation.is_none() {
        tracing::debug!(session_id = %session_id, "ACP ignored cancellation without an active generation");
    } else if let Some(generation) = generation {
        tracing::debug!(generation, "ACP cancelling captured generation");
    }
    generation
}

// --- Request Handlers ---

async fn handle_initialize(
    req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    _state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let caps = acp_capabilities();

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

    let mut sessions = state.sessions.lock().await;
    if sessions.len() >= MAX_ACP_SESSIONS {
        return Err(agent_client_protocol::Error::new(
            -32000,
            "ACP session capacity is full",
        ));
    }

    let turns = Arc::new(StdMutex::new(SessionTurns::default()));
    sessions.insert(
        session_id.clone(),
        SessionState {
            history: Arc::new(Mutex::new(SessionHistory::default())),
            workspace,
            context: Arc::new(context),
            turns: turns.clone(),
        },
    );
    lock_unpoisoned(&state.cancel_routes).insert(session_id.clone(), turns);
    drop(sessions);

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

    let (history, workspace, context, control, registration) = {
        let sessions = state.sessions.lock().await;
        let sess = sessions
            .get(&session_id)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown ACP session"))?;
        let control = Arc::new(TurnControl::new());
        let generation = {
            let mut turns = lock_unpoisoned(&sess.turns);
            if !turns.queue.is_empty() {
                return Err(agent_client_protocol::Error::new(
                    -32000,
                    "ACP session already has an active prompt",
                ));
            }
            let generation = turns.next_generation;
            turns.next_generation = turns.next_generation.wrapping_add(1);
            turns.queue.push_back(RegisteredTurn {
                generation,
                control: control.clone(),
            });
            generation
        };
        (
            sess.history.clone(),
            sess.workspace.clone(),
            sess.context.clone(),
            control.clone(),
            TurnRegistration {
                generation,
                turns: sess.turns.clone(),
                control: control.clone(),
            },
        )
    };

    let request_cancellation = responder.cancellation();
    if request_cancellation.is_cancelled() {
        control.cancel();
    }
    let request_cancellation_bridge =
        RequestCancellationBridge::new(request_cancellation, control.clone());

    cx.spawn({
        let cx = cx.clone();
        let teardown_control = control.clone();
        async move {
            // The connection owns this waiter, but the Tokio task owns the
            // actual turn. On connection teardown the waiter signals
            // cancellation while the turn remains alive just long enough to
            // kill/reap hooks and finish tracked blocking work.
            let _cancel_on_connection_drop = CancelTurnOnDrop(teardown_control);
            tokio::spawn(async move {
                let registration = registration;
                let _request_cancellation_bridge = request_cancellation_bridge;
                if control.is_cancelled() {
                    let _ = respond_terminal(&registration, responder, StopReason::Cancelled);
                    return Ok(());
                }
                let history = history.lock_owned().await;
                let result = run_prompt(
                    &state,
                    &prompt_text,
                    session_id,
                    history,
                    workspace,
                    context,
                    responder,
                    cx,
                    control,
                    &registration,
                )
                .await;
                #[cfg(test)]
                if let Some(barrier) = &state.prompt_exit_barrier
                    && barrier.generation == registration.generation
                {
                    barrier.entered.notify_one();
                    barrier.release.notified().await;
                }
                result
            })
            .await
            .map_err(|error| {
                agent_client_protocol::Error::new(
                    -32603,
                    format!("ACP prompt task failed: {error}"),
                )
            })?
        }
    })
}

// --- Prompt Execution ---

fn respond_terminal(
    registration: &TurnRegistration,
    responder: Responder<PromptResponse>,
    reason: StopReason,
) -> Result<(), agent_client_protocol::Error> {
    registration.settle();
    responder.respond(PromptResponse::new(reason))
}

async fn run_prompt(
    state: &AcpState,
    prompt_text: &str,
    session_id: SessionId,
    history: tokio::sync::OwnedMutexGuard<SessionHistory>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    control: Arc<TurnControl>,
    registration: &TurnRegistration,
) -> Result<(), agent_client_protocol::Error> {
    if control.is_cancelled() {
        let _ = respond_terminal(registration, responder, StopReason::Cancelled);
        return Ok(());
    }
    if let Err(error) = workspace.validate() {
        return respond_prompt_failure(session_id, responder, cx, registration, error.to_string());
    }
    let prior_history = history.snapshot();

    #[cfg(test)]
    if let Some(fixture) = &state.runner_fixture {
        let paused_runner = tokio::select! {
            biased;
            _ = control.cancelled() => {
                let _ = respond_terminal(registration, responder, StopReason::Cancelled);
                return Ok(());
            }
            runner = fixture(prompt_text.to_owned(), prior_history.clone()) => runner,
        };
        return relay_paused_runner(
            prompt_text,
            session_id,
            history,
            responder,
            cx,
            control,
            paused_runner,
            registration,
        )
        .await;
    }

    #[cfg(test)]
    if let Some(fixture) = &state.prompt_fixture {
        let fixture_result = tokio::select! {
            biased;
            _ = control.cancelled() => {
                let _ = respond_terminal(registration, responder, StopReason::Cancelled);
                return Ok(());
            }
            result = fixture(prompt_text.to_owned(), prior_history) => result,
        };
        let events = match fixture_result {
            Ok(events) => events,
            Err(error) => {
                return respond_prompt_failure(session_id, responder, cx, registration, error);
            }
        };
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(events.len().max(1));
        for event in events {
            event_tx.send(event).await.map_err(|_| {
                agent_client_protocol::Error::new(-32603, "ACP fixture event channel closed")
            })?;
        }
        drop(event_tx);
        return relay_prompt_events(
            prompt_text,
            session_id,
            history,
            responder,
            cx,
            control,
            event_rx,
            registration,
        )
        .await;
    }

    let workspace_root = workspace.root();
    let (authority, sandbox) =
        match crate::permission::resolve_configured_execution_authority(&state.cli, &state.cfg) {
            Ok(resolved) => resolved,
            Err(error) => {
                return respond_prompt_failure(
                    session_id,
                    responder,
                    cx,
                    registration,
                    error.to_string(),
                );
            }
        };
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

    let client = match crate::provider::create_client(
        &provider_str,
        None,
        &state.cfg.custom_providers_map(),
        state.cfg.api_keys.as_ref(),
    ) {
        Ok(client) => client,
        Err(error) => {
            return respond_prompt_failure(
                session_id,
                responder,
                cx,
                registration,
                error.to_string(),
            );
        }
    };

    let model = client.completion_model(model_str.to_string());

    let temperature = crate::config::resolve_temperature(&state.cli, &state.cfg, &model_str);
    let extra_body = crate::config::resolve_extra_body(&state.cfg, &model_str);
    let work_scope = crate::agent::runner::AgentWorkScope::new();
    let Some(agent) = run_owned_pre_run(
        &control,
        &work_scope,
        crate::provider::build_agent_in_workspace(
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
        ),
    )
    .await
    else {
        let _ = respond_terminal(registration, responder, StopReason::Cancelled);
        return Ok(());
    };

    let Some(paused_runner) = run_owned_pre_run(
        &control,
        &work_scope,
        agent.spawn_runner_paused_in_scope(
            prompt_text.to_string(),
            prior_history,
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "hooks")]
            None,
            Arc::clone(&work_scope),
        ),
    )
    .await
    else {
        let _ = respond_terminal(registration, responder, StopReason::Cancelled);
        return Ok(());
    };
    relay_paused_runner(
        prompt_text,
        session_id,
        history,
        responder,
        cx,
        control,
        paused_runner,
        registration,
    )
    .await
}

async fn run_owned_pre_run<T>(
    control: &TurnControl,
    work_scope: &Arc<crate::agent::runner::AgentWorkScope>,
    future: impl Future<Output = T>,
) -> Option<T> {
    let scoped = work_scope.run(future);
    tokio::pin!(scoped);
    tokio::select! {
        biased;
        _ = control.cancelled() => {
            work_scope.cancellation_handle().cancel();
            let result = scoped.await;
            // Builder results can own admission/telemetry threads whose guards
            // keep this scope non-idle. Tear the result down before waiting for
            // those workers to join.
            drop(result);
            work_scope.wait_idle().await;
            None
        }
        result = &mut scoped => Some(result),
    }
}

async fn relay_paused_runner(
    prompt_text: &str,
    session_id: SessionId,
    history: tokio::sync::OwnedMutexGuard<SessionHistory>,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    control: Arc<TurnControl>,
    paused_runner: crate::agent::runner::PausedAgentRunner,
    registration: &TurnRegistration,
) -> Result<(), agent_client_protocol::Error> {
    let attached = control.attach_runner(paused_runner.cancellation_handle());
    let mut runner = paused_runner.start();
    if !attached {
        while runner.event_rx.recv().await.is_some() {}
        let _ = respond_terminal(registration, responder, StopReason::Cancelled);
        return Ok(());
    }
    relay_prompt_events(
        prompt_text,
        session_id,
        history,
        responder,
        cx,
        control,
        runner.event_rx,
        registration,
    )
    .await
}

fn respond_prompt_failure(
    session_id: SessionId,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    registration: &TurnRegistration,
    error: String,
) -> Result<(), agent_client_protocol::Error> {
    if !registration.complete_and_settle() {
        let _ = respond_terminal(registration, responder, StopReason::Cancelled);
        return Ok(());
    }
    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(format!(
        "[error: {error}]"
    ))));
    let _ = cx.send_notification(SessionNotification::new(
        session_id,
        SessionUpdate::AgentMessageChunk(chunk),
    ));
    let _ = respond_terminal(registration, responder, StopReason::Refusal);
    Ok(())
}

async fn relay_prompt_events(
    prompt_text: &str,
    session_id: SessionId,
    mut history: tokio::sync::OwnedMutexGuard<SessionHistory>,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    control: Arc<TurnControl>,
    mut rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    registration: &TurnRegistration,
) -> Result<(), agent_client_protocol::Error> {
    let completed_interactions = loop {
        let event = tokio::select! {
            biased;
            _ = control.cancelled() => {
                return respond_cancelled_after_runner(responder, &mut rx, registration).await;
            }
            event = rx.recv() => event,
        };
        let Some(event) = event else {
            if control.is_cancelled() {
                let _ = respond_terminal(registration, responder, StopReason::Cancelled);
            } else if registration.complete_and_settle() {
                let _ = respond_terminal(registration, responder, StopReason::Refusal);
            } else {
                let _ = respond_terminal(registration, responder, StopReason::Cancelled);
            }
            return Ok(());
        };
        match event {
            AgentEvent::Token(text) => {
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
            AgentEvent::ToolCall { id, name, args } => {
                let id = ToolCallId::new(id);
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
            AgentEvent::SubagentToolCall { .. } => {
                // This is a display-only event from inside a task tool. The outer
                // provider tool call/result is canonical and will arrive with a
                // stable ID; advertising a nested call here would create an ACP
                // call that can never receive a matching result.
            }
            AgentEvent::ToolResult { id, output, .. } => {
                let fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(output.to_string()),
                    ))]);
                let update = ToolCallUpdate::new(ToolCallId::new(id), fields);
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
            AgentEvent::Done { interactions, .. } => {
                while rx.recv().await.is_some() {}
                break interactions;
            }
            AgentEvent::Error(err) => {
                while rx.recv().await.is_some() {}
                if !registration.complete_and_settle() {
                    return respond_cancelled_after_runner(responder, &mut rx, registration).await;
                }
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
                let _ = respond_terminal(registration, responder, StopReason::Refusal);
                return Ok(());
            }
        }
    };

    if !registration.complete_and_settle() {
        return respond_cancelled_after_runner(responder, &mut rx, registration).await;
    }

    history.commit_completed_turn(prompt_text, completed_interactions);

    let _ = respond_terminal(registration, responder, StopReason::EndTurn);
    Ok(())
}

async fn respond_cancelled_after_runner(
    responder: Responder<PromptResponse>,
    rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    registration: &TurnRegistration,
) -> Result<(), agent_client_protocol::Error> {
    // The abort handle closes this channel only after the model/tool future and
    // every sender it owns have been dropped. Waiting here makes cancellation a
    // true completion boundary before the ACP request and session are released.
    while rx.recv().await.is_some() {}
    let _ = respond_terminal(registration, responder, StopReason::Cancelled);
    Ok(())
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use rig::agent::AgentBuilder;
    use rig::completion::message::UserContent;
    use rig::completion::{AssistantContent, Message};
    use rig::test_utils::{MockCompletionModel, MockStreamEvent};

    async fn complete_prompt(
        model: MockCompletionModel,
        prompt: &str,
        history: Vec<Message>,
    ) -> Vec<Message> {
        let agent = AgentBuilder::new(model).build();
        let runner = crate::agent::runner::spawn_agent(
            agent,
            prompt.to_string(),
            history,
            crate::retry::RetryConfig::default(),
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut events = runner.event_rx;
        while let Some(event) = events.recv().await {
            match event {
                AgentEvent::Done { interactions, .. } => return interactions,
                AgentEvent::Error(error) => panic!("fake ACP turn failed: {error}"),
                _ => {}
            }
        }
        panic!("fake ACP turn ended without a terminal event")
    }

    #[tokio::test]
    async fn second_prompt_receives_the_first_committed_turn_exactly_once() {
        let mut history = SessionHistory::default();
        let first_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("The project code is red-fox."),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let first_observer = first_model.clone();
        let first_interactions = complete_prompt(
            first_model,
            "Remember that the project code is red-fox.",
            history.snapshot(),
        )
        .await;
        assert!(
            history.snapshot().is_empty(),
            "running a turn must not insert its user message before an explicit commit"
        );
        history.commit_completed_turn(
            "Remember that the project code is red-fox.",
            first_interactions,
        );

        let first_request = first_observer.requests();
        assert_eq!(first_request.len(), 1);
        assert_eq!(
            first_request[0]
                .chat_history
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![Message::user("Remember that the project code is red-fox.")],
            "Rig should append the current prompt exactly once to an empty prior snapshot"
        );

        let second_model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("red-fox"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let second_observer = second_model.clone();
        let _ = complete_prompt(
            second_model,
            "What project code did I ask you to remember?",
            history.snapshot(),
        )
        .await;

        let requests = second_observer.requests();
        let received = requests[0].chat_history.iter().cloned().collect::<Vec<_>>();
        assert_eq!(
            received,
            vec![
                Message::user("Remember that the project code is red-fox."),
                Message::assistant("The project code is red-fox."),
                Message::user("What project code did I ask you to remember?"),
            ]
        );
    }

    #[tokio::test]
    async fn concurrent_acp_sessions_keep_history_isolated() {
        let first = Arc::new(Mutex::new(SessionHistory::default()));
        let second = Arc::new(Mutex::new(SessionHistory::default()));
        let first_task = {
            let first = first.clone();
            tokio::spawn(async move {
                first.lock().await.commit_completed_turn(
                    "first-user",
                    vec![Message::assistant("first-assistant")],
                );
            })
        };
        let second_task = {
            let second = second.clone();
            tokio::spawn(async move {
                second.lock().await.commit_completed_turn(
                    "second-user",
                    vec![Message::assistant("second-assistant")],
                );
            })
        };
        tokio::try_join!(first_task, second_task).unwrap();

        assert_eq!(
            first.lock().await.snapshot(),
            vec![
                Message::user("first-user"),
                Message::assistant("first-assistant")
            ]
        );
        assert_eq!(
            second.lock().await.snapshot(),
            vec![
                Message::user("second-user"),
                Message::assistant("second-assistant")
            ]
        );
    }

    #[test]
    fn structured_tool_history_keeps_call_and_result_correlated() {
        let call = Message::Assistant {
            id: None,
            content: rig::OneOrMany::one(AssistantContent::tool_call(
                "call-17",
                "read",
                serde_json::json!({"path": "sentinel.txt"}),
            )),
        };
        let result = Message::tool_result("call-17", "sentinel contents");
        let mut history = SessionHistory::default();
        history.commit_completed_turn(
            "read the sentinel",
            vec![call, result, Message::assistant("The sentinel is present.")],
        );
        let snapshot = history.snapshot();

        let Message::Assistant { content, .. } = &snapshot[1] else {
            panic!("tool call must remain an assistant message")
        };
        let AssistantContent::ToolCall(call) = content.first() else {
            panic!("assistant history must retain a structured tool call")
        };
        assert_eq!(call.id, "call-17");
        assert_eq!(call.function.name, "read");

        let Message::User { content } = &snapshot[2] else {
            panic!("tool result must remain a user message")
        };
        let UserContent::ToolResult(result) = content.first() else {
            panic!("user history must retain a structured tool result")
        };
        assert_eq!(result.id, call.id);
    }

    #[test]
    fn continuation_bridge_is_committed_as_the_exact_model_visible_transcript() {
        let grouped_assistant = Message::Assistant {
            id: None,
            content: rig::OneOrMany::many(vec![
                AssistantContent::text("checking"),
                AssistantContent::tool_call(
                    "causal-tool",
                    "read",
                    serde_json::json!({"path": "sentinel.txt"}),
                ),
            ])
            .expect("grouped assistant content is non-empty"),
        };
        let model_visible_transcript = vec![
            Message::user("start"),
            grouped_assistant,
            Message::tool_result("causal-tool", "sentinel contents"),
            Message::assistant(""),
            Message::user("Please continue."),
            Message::assistant("answer"),
        ];

        let mut history = SessionHistory::default();
        history.commit_completed_turn("start", model_visible_transcript[1..].to_vec());

        assert_eq!(history.snapshot(), model_visible_transcript);
    }

    #[test]
    fn history_retention_evicts_whole_oldest_turns_at_both_bounds() {
        let mut by_count = SessionHistory::default();
        for index in 0..=MAX_ACP_HISTORY_TURNS {
            by_count.commit_completed_turn(
                &format!("user-{index}"),
                vec![Message::assistant(format!("assistant-{index}"))],
            );
        }
        let count_snapshot = by_count.snapshot();
        assert_eq!(count_snapshot.len(), MAX_ACP_HISTORY_TURNS * 2);
        assert!(!count_snapshot.contains(&Message::user("user-0")));
        assert!(count_snapshot.contains(&Message::user(format!("user-{MAX_ACP_HISTORY_TURNS}"))));

        let mut by_bytes = SessionHistory::default();
        by_bytes.commit_completed_turn(
            &"u".repeat(MAX_ACP_HISTORY_BYTES),
            vec![Message::assistant("oversized")],
        );
        assert!(
            by_bytes.snapshot().is_empty(),
            "a single oversized turn must not leave history above its byte bound"
        );
    }

    #[test]
    fn initialize_truthfully_does_not_advertise_load_session() {
        assert!(!acp_capabilities().load_session);
    }
}

#[cfg(test)]
mod protocol_tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use agent_client_protocol::schema::ProtocolVersion;
    use rig::completion::AssistantContent;

    use super::*;

    struct InMemoryAgent(Arc<AcpState>);

    struct ProtocolTempDir(PathBuf);

    struct NotifyOnDrop(Arc<tokio::sync::Notify>);

    struct MarkOnDrop {
        dropped: Arc<AtomicBool>,
        notify: Arc<tokio::sync::Notify>,
    }

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    impl Drop for MarkOnDrop {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
            self.notify.notify_one();
        }
    }

    impl ProtocolTempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "mini-agent-acp-protocol-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ProtocolTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl ConnectTo<Client> for InMemoryAgent {
        async fn connect_to(
            self,
            client: impl ConnectTo<Agent>,
        ) -> Result<(), agent_client_protocol::Error> {
            connect_agent(self.0, client).await
        }
    }

    fn fixture_state(prompt_fixture: PromptFixture) -> Arc<AcpState> {
        Arc::new(AcpState {
            cli: Cli::default(),
            cfg: Config::default(),
            context: crate::context::load(true),
            sessions: Mutex::new(HashMap::new()),
            cancel_routes: StdMutex::new(HashMap::new()),
            prompt_fixture: Some(prompt_fixture),
            runner_fixture: None,
            prompt_exit_barrier: None,
        })
    }

    fn runner_fixture_state(runner_fixture: RunnerFixture) -> Arc<AcpState> {
        Arc::new(AcpState {
            cli: Cli::default(),
            cfg: Config::default(),
            context: crate::context::load(true),
            sessions: Mutex::new(HashMap::new()),
            cancel_routes: StdMutex::new(HashMap::new()),
            prompt_fixture: None,
            runner_fixture: Some(runner_fixture),
            prompt_exit_barrier: None,
        })
    }

    fn fixture_state_with_exit_barrier(
        prompt_fixture: PromptFixture,
        prompt_exit_barrier: Arc<PromptExitBarrier>,
    ) -> Arc<AcpState> {
        Arc::new(AcpState {
            cli: Cli::default(),
            cfg: Config::default(),
            context: crate::context::load(true),
            sessions: Mutex::new(HashMap::new()),
            cancel_routes: StdMutex::new(HashMap::new()),
            prompt_fixture: Some(prompt_fixture),
            runner_fixture: None,
            prompt_exit_barrier: Some(prompt_exit_barrier),
        })
    }

    fn done(response: &str, interactions: Vec<Message>) -> AgentEvent {
        AgentEvent::Done {
            response: response.into(),
            interactions,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }
    }

    fn prompt(session_id: SessionId, text: &str) -> PromptRequest {
        PromptRequest::new(session_id, vec![ContentBlock::Text(TextContent::new(text))])
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn canonical_tool_turn() -> Vec<Message> {
        vec![
            Message::Assistant {
                id: None,
                content: rig::OneOrMany::many([
                    AssistantContent::tool_call(
                        "provider-a",
                        "read",
                        serde_json::json!({"path": "a"}),
                    ),
                    AssistantContent::tool_call(
                        "provider-b",
                        "read",
                        serde_json::json!({"path": "b"}),
                    ),
                ])
                .unwrap(),
            },
            Message::User {
                content: rig::OneOrMany::many([
                    rig::message::UserContent::tool_result(
                        "provider-a",
                        rig::OneOrMany::one(rig::message::ToolResultContent::text("a-result")),
                    ),
                    rig::message::UserContent::tool_result(
                        "provider-b",
                        rig::OneOrMany::one(rig::message::ToolResultContent::text("b-result")),
                    ),
                ])
                .unwrap(),
            },
            Message::assistant("tools-complete"),
        ]
    }

    #[tokio::test]
    async fn in_memory_protocol_preserves_history_isolation_rollback_and_tool_ids() {
        let observed = Arc::new(StdMutex::new(Vec::<(String, Vec<Message>)>::new()));
        let answered_from_context = Arc::new(AtomicBool::new(false));
        let fixture: PromptFixture = {
            let observed = observed.clone();
            let answered_from_context = answered_from_context.clone();
            Arc::new(move |prompt, history| {
                let observed = observed.clone();
                let answered_from_context = answered_from_context.clone();
                Box::pin(async move {
                    observed
                        .lock()
                        .unwrap()
                        .push((prompt.clone(), history.clone()));
                    if prompt == "fail" {
                        return Err("fixture failure".to_owned());
                    }
                    if prompt == "tools" {
                        return Ok(vec![
                            AgentEvent::ToolCall {
                                id: "lifecycle-a".to_owned(),
                                name: "read".into(),
                                args: serde_json::json!({"path": "a"}),
                            },
                            AgentEvent::ToolCall {
                                id: "lifecycle-b".to_owned(),
                                name: "read".into(),
                                args: serde_json::json!({"path": "b"}),
                            },
                            AgentEvent::SubagentToolCall {
                                name: "nested-display-only".into(),
                                args: serde_json::json!({"not": "canonical"}),
                            },
                            AgentEvent::ToolResult {
                                id: "lifecycle-b".to_owned(),
                                name: "read".into(),
                                output: "b-result".into(),
                            },
                            AgentEvent::ToolResult {
                                id: "lifecycle-a".to_owned(),
                                name: "read".into(),
                                output: "a-result".into(),
                            },
                            done("tools-complete", canonical_tool_turn()),
                        ]);
                    }
                    if prompt == "second" {
                        let mut expected = vec![Message::user("tools")];
                        expected.extend(canonical_tool_turn());
                        if history != expected {
                            return Err("first-turn sentinel was absent from history".to_owned());
                        }
                        answered_from_context.store(true, Ordering::Release);
                        return Ok(vec![done(
                            "sentinel-from-tools",
                            vec![Message::assistant("sentinel-from-tools")],
                        )]);
                    }
                    let response = format!("answer-{prompt}");
                    Ok(vec![done(
                        &response,
                        vec![Message::assistant(response.clone())],
                    )])
                })
            })
        };
        let state = fixture_state(fixture);
        let notifications = Arc::new(StdMutex::new(Vec::<SessionNotification>::new()));
        let notification_sink = notifications.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    notification_sink.lock().unwrap().push(notification);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                let initialized = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(!initialized.agent_capabilities.load_session);

                let first = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                let second = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;

                cx.send_request(prompt(first.clone(), "tools"))
                    .block_task()
                    .await?;
                cx.send_request(prompt(first.clone(), "second"))
                    .block_task()
                    .await?;
                let failed = cx
                    .send_request(prompt(first.clone(), "fail"))
                    .block_task()
                    .await?;
                assert_eq!(failed.stop_reason, StopReason::Refusal);
                cx.send_request(prompt(first.clone(), "after-failure"))
                    .block_task()
                    .await?;
                cx.send_request(prompt(second, "isolated"))
                    .block_task()
                    .await?;

                assert!(
                    cx.send_request(LoadSessionRequest::new(first, cwd))
                        .block_task()
                        .await
                        .is_err(),
                    "unadvertised load_session must also be rejected on the wire"
                );
                Ok(())
            })
            .await
            .unwrap();

        let observed = observed.lock().unwrap();
        assert!(observed[0].1.is_empty());
        let mut exact_first_turn = vec![Message::user("tools")];
        exact_first_turn.extend(canonical_tool_turn());
        assert_eq!(observed[1].1, exact_first_turn);
        assert!(answered_from_context.load(Ordering::Acquire));
        let failed_history = &observed[2].1;
        assert_eq!(
            &observed[3].1, failed_history,
            "failed turns must roll back"
        );
        assert!(observed[4].1.is_empty(), "live sessions must stay isolated");

        let notifications = notifications.lock().unwrap();
        let call_ids = notifications
            .iter()
            .filter_map(|notification| match &notification.update {
                SessionUpdate::ToolCall(call) => Some(call.tool_call_id.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let result_ids = notifications
            .iter()
            .filter_map(|notification| match &notification.update {
                SessionUpdate::ToolCallUpdate(update) => Some(update.tool_call_id.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(call_ids, vec!["lifecycle-a", "lifecycle-b"]);
        assert_eq!(result_ids, vec!["lifecycle-b", "lifecycle-a"]);
    }

    #[tokio::test]
    async fn in_memory_protocol_rejects_concurrent_session_turns_without_blocking_dispatch() {
        let blocked_started = Arc::new(tokio::sync::Notify::new());
        let next_started = Arc::new(AtomicUsize::new(0));
        let fixture: PromptFixture = {
            let blocked_started = blocked_started.clone();
            let next_started = next_started.clone();
            Arc::new(move |prompt, _history| {
                let blocked_started = blocked_started.clone();
                let next_started = next_started.clone();
                Box::pin(async move {
                    if prompt == "blocked" {
                        blocked_started.notify_one();
                        std::future::pending::<()>().await;
                    } else if prompt == "next" {
                        next_started.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant(prompt.clone())],
                    )])
                })
            })
        };
        let state = fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let first = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;

                let first_cx = cx.clone();
                let first_id = first.clone();
                let blocked = tokio::spawn(async move {
                    first_cx
                        .send_request(prompt(first_id, "blocked"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), blocked_started.notified())
                    .await
                    .expect("the first turn should start");

                assert!(
                    cx.send_request(prompt(first.clone(), "concurrent"))
                        .block_task()
                        .await
                        .is_err(),
                    "an untagged cancellation API cannot safely support queued generations"
                );
                cx.send_notification(CancelNotification::new(first.clone()))?;
                cx.send_notification(CancelNotification::new(first.clone()))?;

                let second = tokio::time::timeout(
                    Duration::from_secs(1),
                    cx.send_request(NewSessionRequest::new(cwd)).block_task(),
                )
                .await
                .expect("new-session dispatch must not wait behind the active prompt")?
                .session_id;
                tokio::time::timeout(
                    Duration::from_secs(1),
                    cx.send_request(prompt(second, "other-session"))
                        .block_task(),
                )
                .await
                .expect("another session must run while the first is blocked")?;

                let blocked = tokio::time::timeout(Duration::from_secs(1), blocked)
                    .await
                    .expect("cancellation must release the active turn")
                    .unwrap()?;
                assert_eq!(blocked.stop_reason, StopReason::Cancelled);
                let next = tokio::time::timeout(
                    Duration::from_secs(1),
                    cx.send_request(prompt(first, "next")).block_task(),
                )
                .await
                .expect("a new generation must start after cancellation")?;
                assert_eq!(next.stop_reason, StopReason::EndTurn);
                assert_eq!(next_started.load(Ordering::Acquire), 1);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_waits_for_blocking_runner_before_responding() {
        let tool_started = Arc::new(tokio::sync::Notify::new());
        let tool_dropped = Arc::new(tokio::sync::Notify::new());
        let tool_was_dropped = Arc::new(AtomicBool::new(false));
        let fixture: RunnerFixture = {
            let tool_started = tool_started.clone();
            let tool_dropped = tool_dropped.clone();
            let tool_was_dropped = tool_was_dropped.clone();
            Arc::new(move |prompt, _history| {
                let tool_started = tool_started.clone();
                let tool_dropped = tool_dropped.clone();
                let tool_was_dropped = tool_was_dropped.clone();
                Box::pin(async move {
                    let (event_tx, event_rx) = tokio::sync::mpsc::channel(2);
                    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
                    let work_scope = crate::agent::runner::AgentWorkScope::new();
                    let task_scope = Arc::clone(&work_scope);
                    let join = tokio::spawn(async move {
                        task_scope
                            .run(async move {
                                let work = async move {
                                    if start_rx.await.is_err() {
                                        return;
                                    }
                                    if prompt == "blocked-tool" {
                                        let _drop_signal = MarkOnDrop {
                                            dropped: tool_was_dropped,
                                            notify: tool_dropped,
                                        };
                                        event_tx
                                            .send(AgentEvent::ToolCall {
                                                id: "blocking-tool".to_owned(),
                                                name: "blocking".into(),
                                                args: serde_json::json!({}),
                                            })
                                            .await
                                            .unwrap();
                                        tool_started.notify_one();
                                        std::future::pending::<()>().await;
                                    } else {
                                        event_tx
                                            .send(done(
                                                &prompt,
                                                vec![Message::assistant(prompt.clone())],
                                            ))
                                            .await
                                            .unwrap();
                                    }
                                };
                                tokio::select! {
                                    biased;
                                    _ = crate::agent::runner::current_work_scope_cancelled() => {}
                                    _ = work => {}
                                }
                            })
                            .await;
                    });
                    crate::agent::runner::PausedAgentRunner::new(
                        crate::agent::runner::AgentRunner {
                            event_rx,
                            abort_handle: join.abort_handle(),
                        },
                        start_tx,
                        work_scope,
                    )
                })
            })
        };
        let state = runner_fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let prompt_cx = cx.clone();
                let prompt_session = session.clone();
                let blocked = tokio::spawn(async move {
                    prompt_cx
                        .send_request(prompt(prompt_session, "blocked-tool"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), tool_started.notified())
                    .await
                    .expect("the blocking runner should enter tool work");

                cx.send_notification(CancelNotification::new(session.clone()))?;
                let response = tokio::time::timeout(Duration::from_secs(1), blocked)
                    .await
                    .expect("the cancelled runner should terminate")
                    .unwrap()?;
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                assert!(
                    tool_was_dropped.load(Ordering::Acquire),
                    "Cancelled must not be returned before tool work is dropped"
                );
                tokio::time::timeout(Duration::from_secs(1), tool_dropped.notified())
                    .await
                    .expect("the tool drop signal remains observable");

                let next = cx
                    .send_request(prompt(session, "next"))
                    .block_task()
                    .await?;
                assert_eq!(next.stop_reason, StopReason::EndTurn);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_reaps_production_bash_process_tree_before_responding() {
        let files = ProtocolTempDir::new();
        let shell_pid_file = files.path().join("shell.pid");
        let descendant_pid_file = files.path().join("descendant.pid");
        let command = format!(
            "printf '%s' \"$$\" > {}; sh -c 'printf \"%s\" \"$$\" > {}; while :; do sleep 1; done' & wait",
            shell_quote(&shell_pid_file),
            shell_quote(&descendant_pid_file),
        );
        let fixture: RunnerFixture = Arc::new(move |prompt, history| {
            let command = command.clone();
            Box::pin(async move {
                let model = rig::test_utils::MockCompletionModel::from_stream_turns(vec![vec![
                    rig::test_utils::MockStreamEvent::tool_call(
                        "bash-process-tree",
                        <crate::agent::tools::bash::BashTool as rig::tool::Tool>::NAME,
                        serde_json::json!({"command": command, "timeout": 30_000}),
                    ),
                    rig::test_utils::MockStreamEvent::final_response_with_default_usage(),
                ]]);
                let agent = rig::agent::AgentBuilder::new(model)
                    .tool(crate::agent::tools::bash::BashTool::new(
                        None,
                        None,
                        crate::sandbox::Sandbox::new(false, "bwrap"),
                        None,
                    ))
                    .default_max_turns(2)
                    .build();
                crate::agent::runner::spawn_agent_paused(
                    agent,
                    prompt,
                    history,
                    crate::retry::RetryConfig::default(),
                    #[cfg(feature = "skills")]
                    None,
                    #[cfg(feature = "hooks")]
                    None,
                )
            })
        });
        let state = runner_fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let prompt_cx = cx.clone();
                let prompt_session = session.clone();
                let blocked = tokio::spawn(async move {
                    prompt_cx
                        .send_request(prompt(prompt_session, "run production bash"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !shell_pid_file.exists() || !descendant_pid_file.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("production BashTool should start its process tree");
                let shell_pid = std::fs::read_to_string(&shell_pid_file)
                    .unwrap()
                    .parse::<u32>()
                    .unwrap();
                let descendant_pid = std::fs::read_to_string(&descendant_pid_file)
                    .unwrap()
                    .parse::<u32>()
                    .unwrap();

                cx.send_notification(CancelNotification::new(session))?;
                let response = tokio::time::timeout(Duration::from_secs(2), blocked)
                    .await
                    .expect("Bash cancellation should terminate and reap the process tree")
                    .unwrap()?;
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                assert!(
                    !process_exists(shell_pid),
                    "Cancelled was returned before the Bash shell was reaped"
                );
                assert!(
                    !process_exists(descendant_pid),
                    "Cancelled was returned before the Bash descendant exited"
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dropping_prompt_request_cancels_work_and_reaps_the_generation() {
        let work_started = Arc::new(tokio::sync::Notify::new());
        let work_dropped = Arc::new(tokio::sync::Notify::new());
        let observed = Arc::new(StdMutex::new(Vec::<(String, Vec<Message>)>::new()));
        let fixture: PromptFixture = {
            let work_started = work_started.clone();
            let work_dropped = work_dropped.clone();
            let observed = observed.clone();
            Arc::new(move |prompt, history| {
                let work_started = work_started.clone();
                let work_dropped = work_dropped.clone();
                let observed = observed.clone();
                Box::pin(async move {
                    observed.lock().unwrap().push((prompt.clone(), history));
                    if prompt == "dropped-request" {
                        let _drop_signal = NotifyOnDrop(work_dropped);
                        work_started.notify_one();
                        std::future::pending::<Result<Vec<AgentEvent>, String>>().await
                    } else {
                        Ok(vec![done(
                            &prompt,
                            vec![Message::assistant(prompt.clone())],
                        )])
                    }
                })
            })
        };
        let state = fixture_state(fixture);
        let state_for_control = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let sent = cx.send_request(prompt(session.clone(), "dropped-request"));
                tokio::time::timeout(Duration::from_secs(1), work_started.notified())
                    .await
                    .expect("provider work should start");
                drop(sent);
                tokio::time::timeout(Duration::from_secs(1), work_dropped.notified())
                    .await
                    .expect("dropping SentRequest must cancel provider work");
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        let empty = {
                            let sessions = state_for_control.sessions.lock().await;
                            let turns = sessions.get(&session).unwrap().turns.clone();
                            let empty = turns.lock().unwrap().queue.is_empty();
                            empty
                        };
                        if empty {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("the dropped request generation must be reaped");

                let next = cx
                    .send_request(prompt(session, "next"))
                    .block_task()
                    .await?;
                assert_eq!(next.stop_reason, StopReason::EndTurn);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            observed.lock().unwrap().as_slice(),
            &[
                ("dropped-request".to_owned(), Vec::new()),
                ("next".to_owned(), Vec::new()),
            ],
            "a request-handle cancellation must not leak partial history"
        );
    }

    #[tokio::test]
    async fn connection_teardown_aborts_detached_prompt_work() {
        let work_started = Arc::new(tokio::sync::Notify::new());
        let work_dropped = Arc::new(tokio::sync::Notify::new());
        let fixture: PromptFixture = {
            let work_started = work_started.clone();
            let work_dropped = work_dropped.clone();
            Arc::new(move |_prompt, _history| {
                let work_started = work_started.clone();
                let work_dropped = work_dropped.clone();
                Box::pin(async move {
                    let _drop_signal = NotifyOnDrop(work_dropped);
                    work_started.notify_one();
                    std::future::pending::<Result<Vec<AgentEvent>, String>>().await
                })
            })
        };
        let state = fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        tokio::time::timeout(
            Duration::from_secs(1),
            Client
                .builder()
                .on_receive_notification(
                    async |_notification: SessionNotification, _cx| Ok(()),
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(InMemoryAgent(state), async move |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let session = cx
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?
                        .session_id;
                    cx.send_request(prompt(session, "detached")).detach();
                    tokio::time::timeout(Duration::from_secs(1), work_started.notified())
                        .await
                        .expect("detached provider work should start");
                    Ok(())
                }),
        )
        .await
        .expect("connection teardown must not wait forever for detached prompt work")
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), work_dropped.notified())
            .await
            .expect("connection teardown must drop provider work");
    }

    #[tokio::test]
    async fn cancellation_drops_blocking_pre_run_work() {
        let control = Arc::new(TurnControl::new());
        let work_started = Arc::new(tokio::sync::Notify::new());
        let work_dropped = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let control = control.clone();
            let work_started = work_started.clone();
            let work_dropped = work_dropped.clone();
            async move {
                tokio::select! {
                    biased;
                    _ = control.cancelled() => false,
                    _ = async move {
                        let _drop_signal = NotifyOnDrop(work_dropped);
                        work_started.notify_one();
                        std::future::pending::<()>().await;
                    } => true,
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(1), work_started.notified())
            .await
            .expect("pre-run hook/build work should start");
        assert!(control.cancel());
        assert!(!task.await.unwrap());
        tokio::time::timeout(Duration::from_secs(1), work_dropped.notified())
            .await
            .expect("cancellation must drop blocking hook/build work");
    }

    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn cancelled_pre_run_drops_production_worker_result_before_waiting_idle() {
        let root = ProtocolTempDir::new();
        let environment = crate::paths::PathEnvironment {
            platform: if cfg!(target_os = "macos") {
                crate::paths::PathPlatform::MacOs
            } else if cfg!(target_os = "windows") {
                crate::paths::PathPlatform::Windows
            } else {
                crate::paths::PathPlatform::Linux
            },
            home_dir: None,
            config_base: Some(root.path().join("config")),
            data_base: Some(root.path().join("data")),
            local_data_base: Some(root.path().join("local")),
            state_base: Some(root.path().join("state")),
            cache_base: Some(root.path().join("cache")),
            workspace_root: None,
            overrides: Default::default(),
        };
        let paths = crate::paths::AppPaths::resolve(&environment).expect("worker paths");
        let control = Arc::new(TurnControl::new());
        let work_scope = crate::agent::runner::AgentWorkScope::new();
        let worker_built = Arc::new(Notify::new());
        let release_builder = Arc::new(Notify::new());
        let mut task = tokio::spawn({
            let control = control.clone();
            let work_scope = work_scope.clone();
            let worker_built = worker_built.clone();
            let release_builder = release_builder.clone();
            async move {
                run_owned_pre_run(&control, &work_scope, async move {
                    let dispatcher =
                        crate::extras::js::skills::telemetry::TelemetryDispatcher::spawn(&paths)
                            .expect("production telemetry worker");
                    worker_built.notify_one();
                    release_builder.notified().await;
                    dispatcher
                })
                .await
            }
        });

        tokio::time::timeout(Duration::from_secs(1), worker_built.notified())
            .await
            .expect("builder should return an owner of a production worker");
        assert_eq!(work_scope.active_children(), 1);
        assert!(control.cancel());
        release_builder.notify_one();
        let result = match tokio::time::timeout(Duration::from_secs(1), &mut task).await {
            Ok(result) => result.unwrap(),
            Err(_) => {
                task.abort();
                let _ = task.await;
                panic!("cancelled build must tear down its result before scope settlement");
            }
        };
        assert!(result.is_none());
        assert_eq!(work_scope.active_children(), 0);
    }

    #[cfg(all(feature = "hooks", unix))]
    #[tokio::test]
    async fn cancellation_reaps_configured_async_user_prompt_hook() {
        use crate::extras::hooks::dispatcher::HookDispatcher;
        use crate::extras::hooks::settings::{HookGroup, HookHandler, HooksConfig};

        let workspace = ProtocolTempDir::new();
        let pid_file = workspace.path().join("hook.pid");
        let mut config = HooksConfig::new();
        config.insert(
            "UserPromptSubmit".to_owned(),
            vec![HookGroup {
                matcher: None,
                hooks: vec![HookHandler {
                    kind: "command".to_owned(),
                    command: Some("/bin/sh".to_owned()),
                    args: Some(vec![
                        "-c".to_owned(),
                        "printf '%s' \"$$\" > \"$1\"; while :; do sleep 1; done".to_owned(),
                        "configured-user-prompt-hook".to_owned(),
                        pid_file.display().to_string(),
                    ]),
                    timeout: Some(60),
                    is_async: true,
                    condition: None,
                    once: false,
                }],
            }],
        );
        let dispatcher = HookDispatcher::from_config(&config).unwrap();
        let ctx = crate::extras::hooks::HookCtx {
            session_id: "acp-cancel-test".to_owned(),
            session_path: String::new(),
            cwd: workspace.path().display().to_string(),
            permission_mode: "deny".to_owned(),
        };
        let work_scope = crate::agent::runner::AgentWorkScope::new();
        let started = std::time::Instant::now();
        let gate = work_scope
            .run(crate::extras::hooks::gate_user_prompt(
                &dispatcher,
                &ctx,
                "configured hook prompt".to_owned(),
            ))
            .await;
        assert!(matches!(gate, crate::extras::hooks::PromptGate::Proceed(_)));
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "async:true prompt hooks must not delay prompt dispatch"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while !pid_file.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the configured UserPromptSubmit hook should start");
        let pid = std::fs::read_to_string(&pid_file).unwrap();

        work_scope.cancellation_handle().cancel();
        tokio::time::timeout(Duration::from_secs(1), work_scope.wait_idle())
            .await
            .expect("hook cancellation must kill and reap the configured subprocess");
        assert!(
            !std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            "Cancelled must not return while the configured hook process is live"
        );
    }

    #[tokio::test]
    async fn in_memory_protocol_cancel_aborts_work_rolls_back_and_is_generation_safe() {
        let blocked_started = Arc::new(tokio::sync::Notify::new());
        let release_blocked = Arc::new(tokio::sync::Notify::new());
        let observed = Arc::new(StdMutex::new(Vec::<(String, Vec<Message>)>::new()));
        let fixture: PromptFixture = {
            let blocked_started = blocked_started.clone();
            let release_blocked = release_blocked.clone();
            let observed = observed.clone();
            Arc::new(move |prompt, history| {
                let blocked_started = blocked_started.clone();
                let release_blocked = release_blocked.clone();
                let observed = observed.clone();
                Box::pin(async move {
                    observed.lock().unwrap().push((prompt.clone(), history));
                    if prompt == "blocked" {
                        blocked_started.notify_one();
                        release_blocked.notified().await;
                    }
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant(prompt.clone())],
                    )])
                })
            })
        };
        let state = fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;

                let prompt_cx = cx.clone();
                let prompt_session = session.clone();
                let mut blocked = tokio::spawn(async move {
                    prompt_cx
                        .send_request(prompt(prompt_session, "blocked"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), blocked_started.notified())
                    .await
                    .expect("the blocked turn should start");

                cx.send_notification(CancelNotification::new(session.clone()))?;
                cx.send_notification(CancelNotification::new(session.clone()))?;
                let cancelled = tokio::time::timeout(Duration::from_secs(1), &mut blocked).await;
                if cancelled.is_err() {
                    release_blocked.notify_one();
                    let _ = blocked.await;
                }
                let response = cancelled
                    .expect("session/cancel must stop active prompt work")
                    .expect("prompt task should not panic")?;
                assert_eq!(response.stop_reason, StopReason::Cancelled);

                let next = cx
                    .send_request(prompt(session.clone(), "next"))
                    .block_task()
                    .await?;
                assert_eq!(next.stop_reason, StopReason::EndTurn);

                // This notification is stale: the completed generation must absorb
                // it rather than leaving an abort handle that can kill the next one.
                cx.send_notification(CancelNotification::new(session.clone()))?;
                let after_stale = cx
                    .send_request(prompt(session, "after-stale"))
                    .block_task()
                    .await?;
                assert_eq!(after_stale.stop_reason, StopReason::EndTurn);
                Ok(())
            })
            .await
            .unwrap();

        let observed = observed.lock().unwrap();
        assert_eq!(observed[0], ("blocked".to_owned(), Vec::new()));
        assert_eq!(
            observed[1],
            ("next".to_owned(), Vec::new()),
            "a cancelled turn must not commit user or assistant history"
        );
        assert_eq!(
            observed[2],
            (
                "after-stale".to_owned(),
                vec![Message::user("next"), Message::assistant("next")],
            ),
            "stale cancellation must not affect the next generation"
        );
    }

    #[tokio::test]
    async fn cancel_receipt_linearizes_before_returned_future_is_polled() {
        let first_started = Arc::new(tokio::sync::Notify::new());
        let release_first = Arc::new(tokio::sync::Notify::new());
        let second_started = Arc::new(tokio::sync::Notify::new());
        let fixture: PromptFixture = {
            let first_started = first_started.clone();
            let release_first = release_first.clone();
            let second_started = second_started.clone();
            Arc::new(move |prompt, _history| {
                let first_started = first_started.clone();
                let release_first = release_first.clone();
                let second_started = second_started.clone();
                Box::pin(async move {
                    match prompt.as_str() {
                        "generation-a" => {
                            first_started.notify_one();
                            release_first.notified().await;
                        }
                        "generation-b" => {
                            second_started.notify_one();
                            std::future::pending::<()>().await;
                        }
                        _ => {}
                    }
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant(prompt.clone())],
                    )])
                })
            })
        };
        let state = fixture_state(fixture);
        let state_for_intent = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let first_cx = cx.clone();
                let first_session = session.clone();
                let first = tokio::spawn(async move {
                    first_cx
                        .send_request(prompt(first_session, "generation-a"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), first_started.notified())
                    .await
                    .expect("generation A should start");
                // This is the synchronous portion of the notification callback.
                // Keep its returned future completely unpolled across A's
                // response and B's start to make the ordering deterministic.
                let captured_generation = receive_cancel(&state_for_intent, &session)
                    .expect("generation A cancellation should be applied at receipt");
                let returned_future = async move { captured_generation };
                let (poll_future, wait_to_poll) = tokio::sync::oneshot::channel();
                let delayed_handler = tokio::spawn(async move {
                    let _ = wait_to_poll.await;
                    returned_future.await
                });
                release_first.notify_one();
                assert_eq!(first.await.unwrap()?.stop_reason, StopReason::Cancelled);

                let second_cx = cx.clone();
                let second_session = session.clone();
                let mut second = tokio::spawn(async move {
                    second_cx
                        .send_request(prompt(second_session, "generation-b"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), second_started.notified())
                    .await
                    .expect("generation B should start");

                poll_future.send(()).unwrap();
                assert_eq!(delayed_handler.await.unwrap(), 0);
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), &mut second)
                        .await
                        .is_err(),
                    "polling A's delayed notification future must not cancel blocking B"
                );

                receive_cancel(&state_for_intent, &session);
                let response = tokio::time::timeout(Duration::from_secs(1), second)
                    .await
                    .expect("B's own cancellation should resolve it")
                    .unwrap()?;
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn terminal_response_removes_generation_before_prompt_task_exits() {
        let exit_barrier = Arc::new(PromptExitBarrier {
            generation: 0,
            entered: Notify::new(),
            release: Notify::new(),
        });
        let fixture: PromptFixture = Arc::new(|prompt, _history| {
            Box::pin(async move {
                Ok(vec![done(
                    &prompt,
                    vec![Message::assistant(prompt.clone())],
                )])
            })
        });
        let state = fixture_state_with_exit_barrier(fixture, exit_barrier.clone());
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;

                let first = cx
                    .send_request(prompt(session.clone(), "first"))
                    .block_task()
                    .await?;
                assert_eq!(first.stop_reason, StopReason::EndTurn);
                tokio::time::timeout(Duration::from_secs(1), exit_barrier.entered.notified())
                    .await
                    .expect("first prompt task should remain alive after publishing its response");

                let second = cx
                    .send_request(prompt(session, "second"))
                    .block_task()
                    .await;
                exit_barrier.release.notify_one();
                let second = second?;
                assert_eq!(second.stop_reason, StopReason::EndTurn);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_before_runner_start_rejects_late_abort_attachment() {
        let control = Arc::new(TurnControl::new());
        assert!(control.cancel());
        assert!(
            !control.cancel(),
            "duplicate cancellation must be idempotent"
        );

        let work_scope = crate::agent::runner::AgentWorkScope::new();
        let cancellation = work_scope.cancellation_handle();
        assert!(!control.attach_runner(cancellation.clone()));
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn dropping_registered_generation_aborts_work_without_removing_the_next_turn() {
        let first = Arc::new(TurnControl::new());
        let second = Arc::new(TurnControl::new());
        let turns = Arc::new(StdMutex::new(SessionTurns {
            next_generation: 2,
            queue: VecDeque::from([
                RegisteredTurn {
                    generation: 0,
                    control: first.clone(),
                },
                RegisteredTurn {
                    generation: 1,
                    control: second.clone(),
                },
            ]),
        }));
        let work_scope = crate::agent::runner::AgentWorkScope::new();
        let cancellation = work_scope.cancellation_handle();
        assert!(first.attach_runner(cancellation.clone()));

        drop(TurnRegistration {
            generation: 0,
            turns: turns.clone(),
            control: first,
        });

        assert!(cancellation.is_cancelled());
        let turns = lock_unpoisoned(&turns);
        assert_eq!(turns.queue.len(), 1);
        assert_eq!(turns.queue.front().unwrap().generation, 1);
        assert!(!second.is_cancelled());
    }

    #[tokio::test]
    async fn in_memory_protocol_cancel_before_start_never_enters_provider_work() {
        let fixture_calls = Arc::new(AtomicUsize::new(0));
        let fixture: PromptFixture = {
            let fixture_calls = fixture_calls.clone();
            Arc::new(move |prompt, _history| {
                fixture_calls.fetch_add(1, Ordering::AcqRel);
                Box::pin(async move {
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant(prompt.clone())],
                    )])
                })
            })
        };
        let state = fixture_state(fixture);
        let state_for_control = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let history = {
                    let sessions = state_for_control.sessions.lock().await;
                    sessions.get(&session).unwrap().history.clone()
                };
                let history_guard = history.lock_owned().await;

                let prompt_cx = cx.clone();
                let prompt_session = session.clone();
                let pending_prompt = tokio::spawn(async move {
                    prompt_cx
                        .send_request(prompt(prompt_session, "cancel-before-start"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        let registered = {
                            let sessions = state_for_control.sessions.lock().await;
                            let turns = sessions.get(&session).unwrap().turns.clone();
                            let registered = !turns.lock().unwrap().queue.is_empty();
                            registered
                        };
                        if registered {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("the prompt must register its generation before starting");

                cx.send_notification(CancelNotification::new(session.clone()))?;
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        let cancelled = {
                            let sessions = state_for_control.sessions.lock().await;
                            let turns = sessions.get(&session).unwrap().turns.clone();
                            let control =
                                turns.lock().unwrap().queue.front().unwrap().control.clone();
                            control.is_cancelled()
                        };
                        if cancelled {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("session/cancel must mark the registered generation");
                drop(history_guard);
                let response = tokio::time::timeout(Duration::from_secs(1), pending_prompt)
                    .await
                    .expect("cancel-before-start must resolve the prompt")
                    .unwrap()?;
                assert_eq!(response.stop_reason, StopReason::Cancelled);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            fixture_calls.load(Ordering::Acquire),
            0,
            "provider work must not start after an earlier cancellation"
        );
    }

    #[tokio::test]
    async fn in_memory_protocol_rejects_capacity_without_evicting_live_sessions() {
        let fixture: PromptFixture = Arc::new(|prompt, _history| {
            Box::pin(async move {
                Ok(vec![done(
                    &prompt,
                    vec![Message::assistant(prompt.clone())],
                )])
            })
        });
        let state = fixture_state(fixture);
        let state_for_assertion = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let mut ids = Vec::new();
                for _ in 0..MAX_ACP_SESSIONS {
                    ids.push(
                        cx.send_request(NewSessionRequest::new(cwd.clone()))
                            .block_task()
                            .await?
                            .session_id,
                    );
                }
                assert!(
                    cx.send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await
                        .is_err(),
                    "the 65th session must be rejected"
                );
                cx.send_request(prompt(ids[0].clone(), "still-live"))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            state_for_assertion.sessions.lock().await.len(),
            MAX_ACP_SESSIONS
        );
    }
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
        #[cfg(feature = "archmd")]
        assert!(
            first_context
                .architecture
                .as_deref()
                .unwrap_or_default()
                .contains("FIRST_ARCHITECTURE_SENTINEL")
        );
        #[cfg(feature = "archmd")]
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
