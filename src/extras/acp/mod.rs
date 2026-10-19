mod accept;
mod commands;
pub mod config;
mod cron;
mod idle;
mod policy;
mod sessions;
mod tls;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
#[cfg(test)]
use std::time::Duration;

use agent_client_protocol::on_receive_request;
use agent_client_protocol::schema::ProtocolVersion;
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

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TCP_HOST: &str = "127.0.0.1";
const DEFAULT_TCP_PORT: u16 = 7243;
const MAX_PENDING_AUTHENTICATIONS: usize = 16;
const MAX_ACP_HISTORY_TURNS: usize = 128;
const MAX_ACP_HISTORY_BYTES: usize = 2 * 1024 * 1024;
const MAX_ACP_SESSIONS: usize = 64;
#[cfg(feature = "mcp")]
const MAX_ACP_CLIENT_MCP_SERVERS: usize = 8;

#[cfg(test)]
thread_local! {
    static LIVE_BYTES_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct SessionHistory {
    #[cfg(feature = "hooks")]
    hooks_started: bool,
    // One canonical durable graph. Runtime grants/leases are separate and are
    // never restored from this data owner.
    session: crate::session::Session,
    _lease: Option<Arc<SessionLease>>,
    policy: policy::SessionPolicy,
    pending_question: Option<crate::question::QuestionRequired>,
    manual_compact: bool,
    /// Servers and managers that outlive a single prompt.
    reuse: SessionReuse,
}

/// Prompt-spanning runtime owned by one ACP session. Each prompt builds a new
/// agent, but spawning and initializing MCP servers and language servers is
/// far costlier than the turn's first token, so they are connected lazily by
/// the first prompt that needs them and reused until the session closes, the
/// connection ends, their configuration changes, or a prompt fails or is
/// cancelled (which may leave a server mid-request).
#[derive(Default)]
struct SessionReuse {
    #[cfg(feature = "lsp")]
    lsp: Arc<crate::extras::lsp::SessionLsp>,
    #[cfg(feature = "mcp")]
    mcp: Option<PromptMcp>,
}

impl SessionReuse {
    async fn shutdown(&mut self) {
        #[cfg(feature = "mcp")]
        if let Some(mcp) = self.mcp.take() {
            mcp.close(false).await;
        }
        #[cfg(feature = "lsp")]
        self.lsp.shutdown().await;
    }
}

/// A connected MCP manager with the scope that owns its connections and the
/// identity of the configuration it was connected for.
#[cfg(feature = "mcp")]
struct PromptMcp {
    manager: crate::extras::mcp::McpClientManager,
    scope: Arc<crate::agent::runner::AgentWorkScope>,
    key: Option<McpKey>,
}

#[cfg(feature = "mcp")]
#[derive(PartialEq, Eq)]
struct McpKey {
    workspace: usize,
    servers: String,
}

#[cfg(feature = "mcp")]
impl McpKey {
    fn new(cfg: &Config, workspace: &Arc<crate::paths::WorkspaceBinding>) -> Self {
        let servers: std::collections::BTreeMap<_, _> = cfg.mcp_servers.iter().flatten().collect();
        Self {
            workspace: Arc::as_ptr(workspace) as usize,
            servers: serde_json::to_string(&servers).unwrap_or_default(),
        }
    }
}

#[cfg(feature = "mcp")]
impl PromptMcp {
    /// Still the right manager for `key`: same configuration and workspace,
    /// every configured server connected, and no server connection closed.
    fn is_reusable(&self, key: &McpKey) -> bool {
        self.key.as_ref() == Some(key)
            && self
                .manager
                .configured_count()
                .is_none_or(|configured| self.manager.handles.len() >= configured)
            && self.manager.all_connections_open()
    }

    async fn close(self, cancelled: bool) {
        if cancelled {
            self.scope.cancellation_handle().cancel();
        }
        self.manager.shutdown().await;
    }
}

impl Default for SessionHistory {
    fn default() -> Self {
        Self {
            #[cfg(feature = "hooks")]
            hooks_started: false,
            session: crate::session::Session::new("", "", 32_000, ""),
            _lease: None,
            policy: Default::default(),
            pending_question: None,
            manual_compact: false,
            reuse: Default::default(),
        }
    }
}

impl SessionHistory {
    const EMERGENCY_RECAP: &'static str = "Earlier ACP turns were evicted because automatic summarization was unavailable; retained recent turns remain authoritative.";

    #[cfg(test)]
    fn snapshot(&self) -> Vec<Message> {
        self.snapshot_with_tool_result_retention(crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS)
    }
    fn snapshot_with_tool_result_retention(&self, keep_recent: usize) -> Vec<Message> {
        crate::agent::runner::convert_canonical_history_with_tool_result_retention(
            &self.session,
            keep_recent,
        )
    }
    #[cfg(test)]
    fn commit_completed_turn(&mut self, prompt: &str, interactions: Vec<Message>) {
        let response = interactions
            .last()
            .and_then(|message| match message {
                Message::Assistant { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|item| match item {
                            rig::message::AssistantContent::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        crate::print::persist_headless_turn(&mut self.session, prompt, &response, &interactions);
    }
    fn live_start(&self) -> usize {
        self.session.compacted_context().1
    }
    fn user_starts(&self) -> Vec<usize> {
        self.session
            .messages
            .iter()
            .enumerate()
            .skip(self.live_start())
            .filter_map(|(index, message)| {
                (message.role == crate::session::MessageRole::User).then_some(index)
            })
            .collect()
    }
    fn live_bytes(&self) -> usize {
        #[cfg(test)]
        LIVE_BYTES_CALLS.with(|calls| calls.set(calls.get() + 1));
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > MAX_ACP_HISTORY_BYTES {
                    return Err(std::io::Error::other("ACP live history byte bound"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut count = Count(0);
        let _ = serde_json::to_writer(&mut count, &self.session.messages[self.live_start()..]);
        count.0
    }
    #[cfg(feature = "hooks")]
    fn record_hook_context(&mut self, notices: Vec<String>) {
        for notice in notices {
            self.session
                .add_message(crate::session::MessageRole::System, &notice);
        }
    }

    fn needs_compaction(&self) -> bool {
        (self.manual_compact && self.live_start() < self.session.messages.len())
            || self.user_starts().len() > MAX_ACP_HISTORY_TURNS
            || self.live_bytes() > MAX_ACP_HISTORY_BYTES
    }
    fn next_cut(&self) -> usize {
        if self.manual_compact {
            return self.session.messages.len();
        }
        let starts = self.user_starts();
        if starts.is_empty() {
            return self.session.messages.len();
        }
        let excess = starts.len().saturating_sub(MAX_ACP_HISTORY_TURNS).max(1);
        starts
            .get(excess)
            .copied()
            .unwrap_or(self.session.messages.len())
    }
    #[cfg(test)]
    async fn compact_with<F, Fut>(
        &mut self,
        control: &TurnControl,
        #[cfg(feature = "goal")] goal_store: Option<&crate::extras::goal::GoalStore>,
        summarize: F,
    ) -> Option<crate::event::UsageDelta>
    where
        F: FnMut(Vec<crate::session::SessionMessage>, Option<String>) -> Fut,
        Fut: Future<Output = anyhow::Result<crate::provider::CompactionOutput>>,
    {
        let needs = self.needs_compaction();
        self.compact_with_known(
            needs,
            control,
            #[cfg(feature = "goal")]
            goal_store,
            summarize,
        )
        .await
    }
    /// `needs` is the caller's current `needs_compaction()`: it serializes the
    /// whole live history, so it is evaluated once per decision rather than at
    /// every call site.
    async fn compact_with_known<F, Fut>(
        &mut self,
        mut needs: bool,
        control: &TurnControl,
        #[cfg(feature = "goal")] goal_store: Option<&crate::extras::goal::GoalStore>,
        mut summarize: F,
    ) -> Option<crate::event::UsageDelta>
    where
        F: FnMut(Vec<crate::session::SessionMessage>, Option<String>) -> Fut,
        Fut: Future<Output = anyhow::Result<crate::provider::CompactionOutput>>,
    {
        #[cfg(feature = "subagents")]
        let _background_pause = if needs {
            match &self.session.background_pool {
                Some(pool) => Some(pool.pause_for_history().await.ok()?),
                None => None,
            }
        } else {
            None
        };
        let mut usage = crate::event::UsageDelta::default();
        #[cfg(feature = "goal")]
        if let Some(goal) = goal_store {
            self.session.goal_store = goal.clone();
        }
        while needs {
            if control.is_cancelled() {
                return None;
            }
            let cut = self.next_cut();
            let previous = self.session.compacted_context().0.map(str::to_owned);
            let mut messages = self.session.messages[self.live_start()..cut].to_vec();
            // Text summaries can retain the fact an image was supplied, never
            // infer pixels or turn the opaque base64 into instruction text.
            for message in &mut messages {
                if let Some(user) = &message.user {
                    let image_count = user
                        .blocks()
                        .iter()
                        .filter(|block| {
                            matches!(block, crate::session::user::UserBlock::Image { .. })
                        })
                        .count();
                    if image_count > 0 {
                        message.content = format!("{}\n[{} image attachments supplied; pixels not represented in this text summary]", message.content, image_count).into();
                        message.user = None;
                    }
                }
            }
            let result = tokio::select! {
                biased;
                _ = control.cancelled() => return None,
                result = summarize(messages, previous) => result,
            };
            let output = match result {
                Ok(output) if output.messages_included > 0 => output,
                _ => {
                    self.emergency_compact();
                    return Some(usage);
                }
            };
            if let Some(run) = crate::spend::run::Run::current() {
                self.session.charge_usage_delta_at(
                    output.usage,
                    run.prices.anthropic_native,
                    run.prices.input.unwrap_or(0.0),
                    run.prices.output.unwrap_or(0.0),
                    crate::pricing::CachePrices {
                        read: run.prices.cache_read,
                        write: run.prices.cache_write,
                        write_1h: run.prices.cache_write_1h,
                        split_required: run.prices.cache_split_required,
                    },
                );
            }
            usage = usage.saturating_add(output.usage);
            let consumed = self
                .live_start()
                .saturating_add(output.messages_included)
                .min(cut);
            // A partial summarizer prefix must finish a complete User-started
            // interval; otherwise take the explicit bounded emergency recap.
            let completed_cut = self
                .user_starts()
                .into_iter()
                .filter(|start| *start > self.live_start() && *start <= consumed)
                .max()
                .or_else(|| (consumed == self.session.messages.len()).then_some(consumed));
            let Some(completed_cut) = completed_cut else {
                self.emergency_compact();
                return Some(usage);
            };
            self.session.compress(output.summary, completed_cut, 0);
            self.manual_compact = false;
            needs = self.needs_compaction();
        }
        Some(usage)
    }
    #[cfg(feature = "memory")]
    fn active_compaction_ref(&self) -> Option<String> {
        self.session.active_compaction_ref().map(str::to_owned)
    }
    fn emergency_compact(&mut self) {
        self.manual_compact = false;
        while self.needs_compaction() {
            let cut = self.next_cut();
            if cut == 0 {
                break;
            }
            self.session
                .compress(Self::EMERGENCY_RECAP.to_owned(), cut, 0);
        }
    }
}

fn acp_capabilities() -> AgentCapabilities {
    AgentCapabilities::new()
        .prompt_capabilities(PromptCapabilities::new().image(cfg!(feature = "multimodal")))
        .load_session(true)
        .session_capabilities(
            SessionCapabilities::new()
                .list(Some(SessionListCapabilities::new()))
                .close(Some(SessionCloseCapabilities::new())),
        )
}

fn acp_protocol_version() -> ProtocolVersion {
    ProtocolVersion::V1
}

struct SessionState {
    spend: crate::spend::run::Run,
    history: Arc<Mutex<SessionHistory>>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
    turns: Arc<StdMutex<SessionTurns>>,
    read_tracker: crate::agent::tools::ReadTracker,
    todo_store: crate::agent::tools::TodoStore,
    #[cfg(feature = "goal")]
    goal_store: crate::extras::goal::GoalStore,
    sandbox: crate::sandbox::Sandbox,
    #[cfg(feature = "js")]
    js_session_state: crate::extras::js::session::JsSessionStateOwner,
    #[cfg(feature = "skills")]
    skill_services: Arc<crate::extras::js::skills::session::SkillServiceOwner>,
    /// MCP servers the client supplied in `session/new`, connected for each
    /// tool-enabled prompt alongside the configured servers.
    #[cfg(feature = "mcp")]
    client_mcp_servers: Arc<ClientMcpServers>,
    /// This session's `/cron` schedules. Never persisted; dropping the
    /// session (close or replacement) clears them and releases their idle
    /// lease.
    cron: cron::CronOwner,
}

struct PromptSessionSnapshot {
    spend: crate::spend::run::Run,
    history: Arc<Mutex<SessionHistory>>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
    read_tracker: crate::agent::tools::ReadTracker,
    todo_store: crate::agent::tools::TodoStore,
    #[cfg(feature = "goal")]
    goal_store: crate::extras::goal::GoalStore,
    sandbox: crate::sandbox::Sandbox,
    #[cfg(feature = "js")]
    js_session_state: crate::extras::js::session::JsSessionStateOwner,
    #[cfg(feature = "skills")]
    skill_services: Arc<crate::extras::js::skills::session::SkillServiceOwner>,
    control: Arc<TurnControl>,
    registration: TurnRegistration,
    hook_lease: Option<HookRootLease>,
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
    sandbox: StdMutex<Option<crate::sandbox::Sandbox>>,
}

impl TurnControl {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(TURN_ACTIVE),
            cancelled: Notify::new(),
            runner_cancel: StdMutex::new(None),
            sandbox: StdMutex::new(None),
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
        if let Some(sandbox) = lock_unpoisoned(&self.sandbox).as_ref() {
            sandbox.kill_active();
        }
        self.cancelled.notify_waiters();
        true
    }

    fn attach_sandbox(&self, sandbox: crate::sandbox::Sandbox) -> bool {
        let mut slot = lock_unpoisoned(&self.sandbox);
        if self.state.load(Ordering::Acquire) != TURN_ACTIVE {
            drop(slot);
            sandbox.kill_active();
            return false;
        }
        *slot = Some(sandbox);
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

/// Workspace roots of the active ACP turns while hooks are configured.
///
/// Each turn dispatches hooks against its own session workspace through its
/// agent work scope (`AgentWorkScope::set_hook_execution_root`), so concurrent
/// turns in different workspaces run with hooks. The process-wide hook
/// execution root remains only as the fallback for a dispatch that happens
/// outside a workspace-bound turn: it names the single active workspace, and
/// is disabled (fail-closed) while turns in several workspaces are active so
/// such a dispatch can never run against another session's repository.
struct HookRootClaims {
    active: StdMutex<HashMap<std::path::PathBuf, usize>>,
    /// Publishes the shared fallback root: `Some` for a single active
    /// workspace, `None` for several.
    publish: HookRootPublisher,
}

type HookRootPublisher = Box<dyn Fn(Option<&Path>) + Send + Sync>;

impl Default for HookRootClaims {
    fn default() -> Self {
        Self::with_publisher(|root| {
            #[cfg(feature = "hooks")]
            crate::extras::hooks::bind_shared_execution_root(root);
            #[cfg(not(feature = "hooks"))]
            let _ = root;
        })
    }
}

/// One active turn's hold on its hook root; released on drop.
struct HookRootLease {
    claims: Arc<HookRootClaims>,
    root: std::path::PathBuf,
}

impl HookRootClaims {
    fn with_publisher(publish: impl Fn(Option<&Path>) + Send + Sync + 'static) -> Self {
        Self {
            active: StdMutex::new(HashMap::new()),
            publish: Box::new(publish),
        }
    }

    fn acquire(self: &Arc<Self>, root: &Path) -> HookRootLease {
        let mut active = lock_unpoisoned(&self.active);
        let count = active.entry(root.to_path_buf()).or_insert(0);
        *count += 1;
        if *count == 1 {
            self.publish_locked(&active);
        }
        HookRootLease {
            claims: Arc::clone(self),
            root: root.to_path_buf(),
        }
    }

    /// Called with the claims lock held whenever the set of active roots
    /// changed, so publications are ordered like the changes themselves.
    fn publish_locked(&self, active: &HashMap<std::path::PathBuf, usize>) {
        match active.len() {
            0 => {}
            1 => (self.publish)(active.keys().next().map(std::path::PathBuf::as_path)),
            _ => (self.publish)(None),
        }
    }
}

impl Drop for HookRootLease {
    fn drop(&mut self) {
        let mut active = lock_unpoisoned(&self.claims.active);
        if let Some(count) = active.get_mut(&self.root) {
            *count -= 1;
            if *count == 0 {
                active.remove(&self.root);
                self.claims.publish_locked(&active);
            }
        }
    }
}

fn acp_hook_root_claims() -> Option<Arc<HookRootClaims>> {
    #[cfg(feature = "hooks")]
    if crate::extras::hooks::get_dispatcher().is_some_and(|dispatcher| !dispatcher.is_empty()) {
        return Some(Arc::default());
    }
    None
}

#[derive(Default)]
struct SessionLeases {
    ids: StdMutex<HashSet<SessionId>>,
    #[cfg(test)]
    storage_scope_for_test: StdMutex<Option<Arc<crate::agent::runner::AgentWorkScope>>>,
}

struct SessionLease {
    id: SessionId,
    owner: Arc<SessionLeases>,
    writer: std::sync::atomic::AtomicBool,
}

impl SessionLeases {
    fn reserve(
        self: &Arc<Self>,
        id: SessionId,
    ) -> Result<Arc<SessionLease>, agent_client_protocol::Error> {
        crate::paths::validate_portable_component(&id.to_string()).map_err(|_| {
            agent_client_protocol::Error::new(-32602, "invalid ACP session identity")
        })?;
        let mut ids = lock_unpoisoned(&self.ids);
        if ids.contains(&id) {
            return Err(agent_client_protocol::Error::new(
                -32000,
                "ACP session identity is already owned",
            ));
        }
        if ids.len() >= MAX_ACP_SESSIONS {
            return Err(agent_client_protocol::Error::new(
                -32000,
                "ACP session capacity is full",
            ));
        }
        ids.insert(id.clone());
        Ok(Arc::new(SessionLease {
            id,
            owner: self.clone(),
            writer: Default::default(),
        }))
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if self.writer.load(Ordering::Acquire) {
            crate::session::lock::release_session(&self.id.to_string());
        }
        lock_unpoisoned(&self.owner.ids).remove(&self.id);
    }
}

struct AcpState {
    leases: Arc<SessionLeases>,
    /// Provider clients shared by every prompt. `cli` and `cfg` never change
    /// for the server's lifetime, so a provider name determines its client; a
    /// fresh `reqwest::Client` per prompt would discard the TLS connection pool.
    provider_clients: ProviderClients,
    idle: Arc<idle::Idle>,
    cli: Cli,
    cfg: Config,
    context: ContextFiles,
    sessions: Mutex<HashMap<SessionId, SessionState>>,
    cancel_routes: StdMutex<HashMap<SessionId, Arc<StdMutex<SessionTurns>>>>,
    shell_search_path: Option<std::ffi::OsString>,
    /// Present only when hooks are configured: tracks the active turns'
    /// workspaces to keep the shared fallback hook root safe.
    hook_roots: Option<Arc<HookRootClaims>>,
    /// How client-supplied stdio MCP servers are launched. They start local
    /// processes, so they are accepted only from the editor that spawned this
    /// server over stdio (never from a network peer) and run sandboxed unless
    /// the operator explicitly trusts them.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    client_mcp: ClientMcpPolicy,
    #[cfg(test)]
    prompt_fixture: Option<PromptFixture>,
    #[cfg(test)]
    runner_fixture: Option<RunnerFixture>,
    #[cfg(all(test, feature = "mcp"))]
    mcp_fixture: StdMutex<Option<crate::extras::mcp::McpClientManager>>,
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
    tls: Option<Arc<tls::AcpTlsConfig>>,
    idle: Arc<idle::Idle>,
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

        if let Some(tls) = self.tls {
            tracing::info!("ACP TCP listening on {} (TLS)", local_addr);
            let (stream, peer_addr) = tls::accept_tls_peer_with_idle(
                listener,
                self.api_key,
                tls,
                MAX_PENDING_AUTHENTICATIONS,
                self.idle,
            )
            .await
            .map_err(|e| {
                agent_client_protocol::util::internal_error(format!("TCP accept: {}", e))
            })?;
            tracing::info!("Authenticated ACP TLS client connected from {}", peer_addr);
            let (read_half, write_half) = tokio::io::split(stream);
            return ConnectTo::<Counterpart>::connect_to(
                ByteStreams::new(tls::FuturesIo(write_half), tls::FuturesIo(read_half)),
                client,
            )
            .await;
        }

        tracing::info!("ACP TCP listening on {}", local_addr);
        let accept_error =
            |e| agent_client_protocol::util::internal_error(format!("TCP accept: {}", e));
        let listener = accept::tokio_listener(listener).map_err(accept_error)?;
        let (stream, peer_addr) = accept_authenticated_peer_with_idle(
            listener,
            self.api_key,
            MAX_PENDING_AUTHENTICATIONS,
            self.idle,
        )
        .await
        .map_err(accept_error)?;
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

/// Accepts connections until one peer completes the challenge-response. Each
/// peer authenticates on the blocking pool with bounded reads, so a slow or
/// silent peer never blocks a valid one, and at most `max_pending` are in
/// flight at once. A failed accept of one connection is logged and skipped
/// (see [`accept`]); only a failure of the listener itself ends the loop.
#[cfg(test)]
async fn accept_authenticated_peer<S: accept::AcceptSource>(
    source: S,
    api_key: String,
    max_pending: usize,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    accept_authenticated_peer_with_idle(
        source,
        api_key,
        max_pending,
        idle::Idle::new(None).unwrap(),
    )
    .await
}

async fn accept_authenticated_peer_with_idle<S: accept::AcceptSource>(
    mut source: S,
    api_key: String,
    max_pending: usize,
    idle: Arc<idle::Idle>,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    let api_key: Arc<str> = api_key.into();
    let mut pending: tokio::task::JoinSet<(Option<TcpStream>, SocketAddr)> =
        tokio::task::JoinSet::new();
    let mut backoff_until: Option<tokio::time::Instant> = None;
    loop {
        let (tcp, peer_addr) = tokio::select! {
            accepted = source.accept(), if backoff_until.is_none() => match accepted {
                Ok(connection) => connection,
                Err(error) => {
                    backoff_until = accept::after_accept_error("ACP TCP", error)?;
                    continue;
                }
            },
            () = tokio::time::sleep_until(backoff_until.unwrap_or_else(tokio::time::Instant::now)),
                if backoff_until.is_some() =>
            {
                backoff_until = None;
                continue;
            }
            Some(joined) = pending.join_next(), if !pending.is_empty() => {
                match joined {
                    Ok((Some(stream), peer_addr)) => return Ok((stream, peer_addr)),
                    Ok((None, peer_addr)) => {
                        tracing::warn!("ACP TCP peer authentication rejected for {}", peer_addr);
                    }
                    Err(error) => {
                        tracing::warn!("ACP TCP peer authentication task failed: {error}");
                    }
                }
                continue;
            }
        };
        if pending.len() >= max_pending {
            tracing::warn!("ACP TCP peer rejected because authentication capacity is full");
            continue;
        }
        // The challenge-response uses bounded blocking reads on the blocking
        // pool, and the session afterwards is driven through a blocking
        // adapter, so the peer leaves the reactor in blocking mode.
        let stream = match tcp
            .into_std()
            .and_then(|stream| stream.set_nonblocking(false).map(|()| stream))
        {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(
                    "ACP TCP peer rejected because blocking mode could not be restored: {error}"
                );
                continue;
            }
        };
        let api_key = api_key.clone();
        let activity = idle
            .admit()
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::ConnectionAborted))?;
        struct AuthenticationJob {
            stream: TcpStream,
            _activity: idle::Activity,
        }
        impl AuthenticationJob {
            fn authenticate(self, api_key: &str) -> Option<TcpStream> {
                let _activity = self._activity;
                let mut stream = self.stream;
                let accepted = authenticate_peer(&mut stream, api_key).is_ok();
                accepted.then_some(stream)
            }
        }
        let job = AuthenticationJob {
            stream,
            _activity: activity,
        };
        pending.spawn_blocking(move || (job.authenticate(&api_key), peer_addr));
    }
}

struct TcpServerSettings {
    host: String,
    port: u16,
    api_key: String,
    tls: Option<Arc<tls::AcpTlsConfig>>,
}

/// Opt-in for a non-loopback plaintext ACP TCP bind. Without TLS the TCP
/// transport authenticates only the connection handshake: the session
/// afterwards is plaintext and the client never authenticates the server, so
/// anyone on the network path can read or inject into a tool-executing
/// session. A TLS listener (`MINI_AGENT_ACP_TLS_CERT`/`_KEY`) needs no opt-in.
const ACP_ALLOW_REMOTE_ENV: &str = "MINI_AGENT_ACP_ALLOW_INSECURE_REMOTE";

fn resolve_tcp_settings(cli: &Cli, cfg: &Config) -> anyhow::Result<Option<TcpServerSettings>> {
    let environment_key = std::env::var("MINI_AGENT_ACP_API_KEY").ok();
    let allow_remote = std::env::var(ACP_ALLOW_REMOTE_ENV).is_ok_and(|value| value == "1");
    let tls_paths = tls::tls_paths_from_env()?;
    let Some(mut settings) = resolve_tcp_settings_with_key(
        cli,
        cfg,
        environment_key,
        allow_remote,
        tls_paths.is_some(),
    )?
    else {
        if tls_paths.is_some() {
            anyhow::bail!(
                "{} is set but ACP is not listening on TCP; pass --acp-host/--acp-port",
                tls::ACP_TLS_CERT_ENV
            );
        }
        return Ok(None);
    };
    if let Some((certificate, key)) = tls_paths {
        settings.tls = Some(Arc::new(tls::AcpTlsConfig::load(&certificate, &key)?));
    }
    Ok(Some(settings))
}

fn resolve_tcp_settings_with_key(
    cli: &Cli,
    cfg: &Config,
    environment_key: Option<String>,
    allow_remote: bool,
    tls: bool,
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

    if !is_loopback_host(&host) && !tls {
        if !allow_remote {
            anyhow::bail!(
                "refusing ACP TCP bind on non-loopback host '{host}': the TCP transport \
                 authenticates only the handshake, then carries the session in plaintext \
                 without authenticating the server, so an on-path attacker could read or \
                 inject into a tool-executing session. Set {} and {} to serve TLS, bind to \
                 127.0.0.1 and tunnel (for example over SSH), or set {ACP_ALLOW_REMOTE_ENV}=1 \
                 to accept that risk",
                tls::ACP_TLS_CERT_ENV,
                tls::ACP_TLS_KEY_ENV
            );
        }
        let warning = format!(
            "WARNING: ACP TCP is listening on non-loopback host {host} because \
             {ACP_ALLOW_REMOTE_ENV}=1. The session is unencrypted and the server is not \
             authenticated to clients; use only on a trusted network."
        );
        tracing::warn!("{warning}");
        eprintln!("{warning}");
    }

    Ok(Some(TcpServerSettings {
        host,
        port,
        api_key,
        tls: None,
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

// --- Provider resolution ---

/// The provider/model an ACP turn runs with: the same `--provider`/`--model`
/// resolution and `--quick-model` override as interactive startup. A custom
/// provider's `model` is only a default, applied by `Cli::resolve_model` when
/// nothing else selects a model; it never overrides an explicit `model`.
fn acp_provider_and_model(
    cli: &Cli,
    cfg: &Config,
) -> (compact_str::CompactString, compact_str::CompactString) {
    cli.resolve_provider_and_model(cfg)
}

/// The ACP turn client, with the same credential precedence as startup
/// (`--api-key` first).
fn acp_client(
    cli: &Cli,
    cfg: &Config,
    provider: &str,
) -> anyhow::Result<crate::provider::AnyClient> {
    crate::provider::create_client(
        provider,
        cli.api_key.as_deref(),
        &cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
}

/// Per-provider client cache. `AnyClient` clones share one connection pool.
/// Failures are not cached, so a later prompt retries credential resolution.
#[derive(Default)]
struct ProviderClients {
    clients: StdMutex<HashMap<String, crate::provider::AnyClient>>,
    #[cfg(test)]
    builds: std::sync::atomic::AtomicUsize,
}

impl ProviderClients {
    fn get(
        &self,
        cli: &Cli,
        cfg: &Config,
        provider: &str,
    ) -> anyhow::Result<crate::provider::AnyClient> {
        let mut clients = lock_unpoisoned(&self.clients);
        if let Some(client) = clients.get(provider) {
            return Ok(client.clone());
        }
        let client = acp_client(cli, cfg, provider)?;
        #[cfg(test)]
        self.builds.fetch_add(1, Ordering::Relaxed);
        clients.insert(provider.to_owned(), client.clone());
        Ok(client)
    }
}

/// The subagent configuration an ACP server installs, or `None` when the
/// `task` tool is disabled or the main provider has no usable client.
#[cfg(feature = "subagents")]
fn acp_subagent_config(
    cli: &Cli,
    cfg: &Config,
) -> Option<crate::extras::subagents::SubagentConfig> {
    if !cfg.task_enabled.unwrap_or(true) {
        return None;
    }
    let (provider, model) = acp_provider_and_model(cli, cfg);
    match acp_client(cli, cfg, &provider) {
        Ok(client) => Some(crate::extras::subagents::resolve_config(
            cfg,
            cli.api_key.as_deref(),
            &provider,
            &model,
            &client,
        )),
        Err(error) => {
            tracing::warn!(
                "ACP: subagent provider '{}' unavailable ({}); the task tool will fail until it is configured",
                provider,
                error
            );
            None
        }
    }
}

#[cfg(feature = "subagents")]
fn init_acp_subagents(cli: &Cli, cfg: &Config) {
    if let Some(config) = acp_subagent_config(cli, cfg) {
        crate::extras::subagents::install(config);
    }
}

// --- Server Entry Point ---

pub async fn serve(cli: Cli, mut cfg: Config, context: ContextFiles) -> anyhow::Result<()> {
    cfg.max_budget_usd = cli.resolve_max_budget_usd(&cfg)?;
    // Keep direct callers fail-closed too. The normal binary path has already
    // validated in `Startup::init`, before provider/client construction.
    let (authority, _) = crate::permission::resolve_configured_execution_authority(&cli, &cfg)?;
    crate::permission::build_noninteractive_permission_with_cli(&cli, &cfg, authority)?;

    let tcp_settings = resolve_tcp_settings(&cli, &cfg)?;
    let is_tcp = tcp_settings.is_some();
    let transport_mode = if tcp_settings.is_some() {
        "tcp"
    } else {
        "stdio"
    };
    tracing::info!("ACP server starting: transport={}", transport_mode);

    // ACP returns before `Startup::init_features`, which is where the other
    // surfaces install the subagent configuration. The `task` tool is still
    // registered for every ACP turn, so install it here from the same resolver.
    #[cfg(feature = "subagents")]
    init_acp_subagents(&cli, &cfg);

    let state = Arc::new(AcpState {
        idle: idle::Idle::new(
            cli.idle_exit_secs
                .map(|seconds| std::time::Duration::from_secs(seconds.get())),
        )?,
        cli,
        cfg,
        context,
        leases: Arc::default(),
        provider_clients: Default::default(),
        sessions: Mutex::new(HashMap::new()),
        cancel_routes: StdMutex::new(HashMap::new()),
        shell_search_path: std::env::var_os("PATH"),
        hook_roots: acp_hook_root_claims(),
        client_mcp: client_mcp_policy(
            is_tcp,
            std::env::var(ACP_TRUST_CLIENT_MCP_ENV).is_ok_and(|value| value == "1"),
        ),
        #[cfg(test)]
        prompt_fixture: None,
        #[cfg(test)]
        runner_fixture: None,
        #[cfg(all(test, feature = "mcp"))]
        mcp_fixture: StdMutex::new(None),
        #[cfg(test)]
        prompt_exit_barrier: None,
    });

    // Choose transport: TCP if an endpoint is configured, otherwise stdio.
    if let Some(settings) = tcp_settings {
        let idle = state.idle.clone();
        connect_agent(
            state,
            TcpTransport {
                host: settings.host,
                port: settings.port,
                api_key: settings.api_key,
                tls: settings.tls,
                idle,
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
    let builder = Agent.builder().name(crate::product::PUBLIC_NAME);

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
                move |req: ListSessionsRequest, responder, _cx| {
                    let state = state.clone();
                    async move { sessions::handle_list(req, responder, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: LoadSessionRequest, responder, cx| {
                    let state = state.clone();
                    async move { sessions::handle_load(req, responder, cx, &state).await }
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
                move |req: SetSessionModeRequest, responder, cx| {
                    let state = state.clone();
                    async move { handle_set_session_mode(req, responder, cx, &state).await }
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                move |req: CloseSessionRequest, responder, _cx| {
                    let state = state.clone();
                    async move { handle_close_session(req, responder, &state).await }
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
                    let activity = state.idle.admit();
                    let generation = activity
                        .as_ref()
                        .ok()
                        .and_then(|_| receive_cancel(&state, &notification.session_id));
                    async move {
                        let _activity = activity?;
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
            {
                let state = state.clone();
                move |dispatch: Dispatch<AgentRequest, AgentNotification>,
                      _cx: ConnectionTo<Client>| {
                    let state = state.clone();
                    async move {
                        let _activity = state.idle.admit()?;
                        match dispatch {
                            Dispatch::Request(_, responder) => {
                                tracing::warn!("ACP received an unhandled request");
                                responder.respond_with_error(
                                    agent_client_protocol::Error::method_not_found(),
                                )
                            }
                            Dispatch::Notification(_) => {
                                tracing::warn!("ACP received an unhandled notification");
                                Ok(())
                            }
                            Dispatch::Response(response, router) => {
                                router.route_with_result(response)
                            }
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        );

    let result = if state.idle.enabled() {
        // Both connection futures are dropped before waiting for actual task
        // cleanup. Dropping a Tokio JoinHandle alone would detach that cleanup.
        tokio::select! {
            result = builder.connect_to(transport) => result,
            () = state.idle.expired() => {
                tracing::info!("ACP server exiting after idle");
                Ok(())
            }
        }
    } else {
        builder.connect_to(transport).await
    };
    // Connection teardown dropped every `/cron` driver; drop the schedules
    // and their idle leases too, so the settle wait below does not wait on a
    // schedule that can never fire again.
    for session in state.sessions.lock().await.values() {
        session.cron.clear();
    }
    #[cfg(feature = "subagents")]
    {
        let pools = state
            .sessions
            .lock()
            .await
            .values()
            .filter_map(|session| session.spend.background_pool())
            .collect::<Vec<_>>();
        for pool in pools {
            pool.close().await;
        }
    }
    // Servers kept for later prompts end with the connection. Teardown has
    // cancelled every turn, so each history lock is released promptly.
    let histories = state
        .sessions
        .lock()
        .await
        .values()
        .map(|session| session.history.clone())
        .collect::<Vec<_>>();
    for history in histories {
        history.lock().await.reuse.shutdown().await;
    }
    state.idle.wait_settled().await;
    result
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
    _req: InitializeRequest,
    responder: Responder<InitializeResponse>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _activity = state.idle.admit()?;
    let caps = acp_capabilities();

    let resp = InitializeResponse::new(acp_protocol_version())
        .agent_capabilities(caps)
        .agent_info(Implementation::new(
            crate::product::PUBLIC_NAME,
            AGENT_VERSION,
        ));

    responder.respond(resp)
}

async fn handle_new_session(
    req: NewSessionRequest,
    responder: Responder<NewSessionResponse>,
    cx: ConnectionTo<Client>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _activity = state.idle.admit()?;
    if !req.additional_directories.is_empty() {
        return Err(agent_client_protocol::Error::new(
            -32602,
            "additional ACP workspace directories are not supported",
        ));
    }

    let workspace = Arc::new(canonical_session_workspace(&req.cwd)?);
    #[cfg(feature = "mcp")]
    let client_mcp_servers = client_mcp_servers(state, &req.mcp_servers)?;
    #[cfg(not(feature = "mcp"))]
    client_mcp_servers(state, &req.mcp_servers)?;
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let lease = state.leases.reserve(session_id.clone())?;
    let (provider, model) = acp_provider_and_model(&state.cli, &state.cfg);
    let canonical = crate::session::Session::new(
        &provider,
        &model,
        state.cfg.resolve_context_window(
            &provider,
            &model,
            &crate::config::quick_models_map(&state.cfg),
        ),
        "",
    );
    let modes = install_session(
        state,
        session_id.clone(),
        workspace,
        canonical,
        lease,
        #[cfg(feature = "subagents")]
        BackgroundSessionClient::Protocol(cx.clone()),
        #[cfg(feature = "mcp")]
        client_mcp_servers,
    )
    .await?;

    commands::announce(&session_id, &cx)?;
    let resp = NewSessionResponse::new(session_id).modes(modes);
    responder.respond(resp)
}

#[cfg(feature = "subagents")]
fn install_background_ask_factory(
    pool: &Arc<crate::extras::subagents::background::Pool>,
    state: &AcpState,
    cx: ConnectionTo<Client>,
    session_id: &SessionId,
    policy: &policy::SessionPolicy,
) -> Result<(), agent_client_protocol::Error> {
    {
        let cx = cx.clone();
        let idle = state.idle.clone();
        let policy = policy.clone();
        let id = session_id.clone();
        pool.install_ask_factory(Arc::new(move |permission, scope| {
            let activity = idle
                .admit()
                .map_err(|_| "ACP client is retiring".to_string())?;
            let (sender, receiver) = tokio::sync::mpsc::channel(32);
            let control = Arc::new(TurnControl::new());
            let cancel = control.clone();
            let bridge = drive_permission_bridge(
                cx.clone(),
                idle::Activity::default(),
                id.clone(),
                receiver,
                control,
                Some(BridgePolicy {
                    policy: policy.clone(),
                    permission,
                }),
                #[cfg(feature = "goal")]
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
            );
            let future = activity.track(async move {
                tokio::pin!(bridge);
                tokio::select! {
                    _ = scope.cancelled() => { cancel.cancel(); bridge.await; }
                    () = &mut bridge => {}
                }
            });
            Ok(crate::extras::subagents::background::PreparedAsk {
                sender,
                future: Box::pin(future),
            })
        }))
        .map_err(|error| agent_client_protocol::Error::new(-32603, error))?;
    }
    Ok(())
}

#[cfg(feature = "subagents")]
enum BackgroundSessionClient {
    Protocol(ConnectionTo<Client>),
    #[cfg(all(test, feature = "goal"))]
    Unconnected,
}

/// Install one canonical data owner with entirely fresh runtime authority.
async fn install_session(
    state: &AcpState,
    session_id: SessionId,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    mut canonical: crate::session::Session,
    lease: Arc<SessionLease>,
    #[cfg(feature = "subagents")] client: BackgroundSessionClient,
    #[cfg(feature = "mcp")] client_mcp_servers: ClientMcpServers,
) -> Result<Option<SessionModeState>, agent_client_protocol::Error> {
    let sandbox = acp_session_sandbox(state, workspace.clone())?;
    let context = state
        .context
        .for_workspace_binding(state.cli.resolve_no_context_files(&state.cfg), &workspace);
    workspace.validate().map_err(|_| {
        agent_client_protocol::Error::new(
            -32602,
            "ACP workspace changed during context preparation",
        )
    })?;
    let (provider, model) = acp_provider_and_model(&state.cli, &state.cfg);
    let initial_spend = crate::spend::run::Run::for_new_session(
        &session_id.to_string(),
        &provider,
        &model,
        &state.cfg,
        Some(state.cli.resolve_model(&state.cfg).as_str()),
    )
    .map_err(|_| agent_client_protocol::Error::new(-32602, "invalid USD budget configuration"))?;
    canonical.id = session_id.to_string().into();
    canonical.working_dir = workspace.root().to_string_lossy().into_owned().into();
    canonical.provider = provider;
    canonical.model = model;
    canonical.input_token_cost = initial_spend.prices.input.unwrap_or(0.0);
    canonical.output_token_cost = initial_spend.prices.output.unwrap_or(0.0);
    canonical.input_output_prices_known =
        Some(initial_spend.prices.input.is_some() && initial_spend.prices.output.is_some());
    canonical.set_cache_prices(crate::pricing::CachePrices {
        read: initial_spend.prices.cache_read,
        write: initial_spend.prices.cache_write,
        write_1h: initial_spend.prices.cache_write_1h,
        split_required: initial_spend.prices.cache_split_required,
    });
    #[cfg(feature = "goal")]
    if let Some(saved) = canonical.goal_store.snapshot() {
        // Goal commands, judge endpoint and file-context authority come only
        // from this installation, never from an imported session record.
        let mut restored = crate::extras::goal::Goal::configured(
            saved.objective.clone(),
            saved.criteria.clone(),
            crate::extras::goal::GoalDefaults {
                cfg: &state.cfg,
                provider: &canonical.provider,
                model: &canonical.model,
            },
        )
        .map_err(|_| agent_client_protocol::Error::new(-32602, "saved ACP goal is invalid"))?;
        restored.id = saved.id;
        restored.status = saved.status;
        restored.paused_reason = saved.paused_reason;
        restored.progress = saved.progress;
        restored.reports = saved.reports;
        restored.history = saved.history;
        restored.created_at = saved.created_at;
        restored.updated_at = saved.updated_at;
        // A stored verdict remains display data, not a newly verified verdict.
        restored.last_verdict = None;
        canonical.goal_store = Default::default();
        canonical.goal_store.set(restored, true).map_err(|_| {
            agent_client_protocol::Error::new(-32602, "saved ACP goal could not be restored")
        })?;
    }
    // Imported totals seed the same account; existing uncertainty stays conservative.
    let spend = crate::spend::run::Run::for_session(
        &mut canonical,
        &state.cfg,
        crate::spend::RequestKind::Turn,
    );
    canonical.permission_allowlist.clear();
    canonical.rewind_undo = None;
    if !state.cli.no_session {
        claim_writer(lease.clone()).await?;
    }
    let authority =
        crate::permission::resolve_configured_execution_authority(&state.cli, &state.cfg)
            .map_err(|_| agent_client_protocol::Error::new(-32602, "invalid execution authority"))?
            .0;
    let policy = policy::SessionPolicy::new(
        authority.mode,
        authority.tools_enabled && authority.permission_checks_enabled,
    );
    #[cfg(feature = "subagents")]
    match client {
        BackgroundSessionClient::Protocol(cx) => {
            if let Some(pool) = spend.background_pool() {
                install_background_ask_factory(&pool, state, cx, &session_id, &policy)?;
            }
        }
        #[cfg(all(test, feature = "goal"))]
        BackgroundSessionClient::Unconnected => {}
    }
    let modes = policy.modes();
    let turns = Arc::new(StdMutex::new(SessionTurns::default()));
    let pending_question = canonical.pending_question.clone();
    let read_tracker = canonical.read_tracker.clone();
    let todo_store = canonical.todos.clone();
    #[cfg(feature = "goal")]
    let goal_store = canonical.goal_store.clone();
    let mut sessions = state.sessions.lock().await;
    sessions.insert(
        session_id.clone(),
        SessionState {
            spend,
            history: Arc::new(Mutex::new(SessionHistory {
                #[cfg(feature = "hooks")]
                hooks_started: false,
                session: canonical,
                _lease: Some(lease),
                policy,
                pending_question,
                manual_compact: false,
                reuse: Default::default(),
            })),
            workspace,
            context: Arc::new(context),
            turns: turns.clone(),
            read_tracker,
            todo_store,
            #[cfg(feature = "goal")]
            goal_store,
            sandbox,
            #[cfg(feature = "js")]
            js_session_state: Default::default(),
            #[cfg(feature = "skills")]
            skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            client_mcp_servers: Arc::new(client_mcp_servers),
            cron: cron::CronOwner::default(),
        },
    );
    lock_unpoisoned(&state.cancel_routes).insert(session_id, turns);
    state.idle.sessions_changed(sessions.len());
    Ok(modes)
}

async fn claim_writer(lease: Arc<SessionLease>) -> Result<(), agent_client_protocol::Error> {
    let scope = crate::agent::runner::AgentWorkScope::new();
    #[cfg(test)]
    let scope = lock_unpoisoned(&lease.owner.storage_scope_for_test)
        .clone()
        .unwrap_or(scope);
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let result = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                anyhow::ensure!(
                    !crate::paths::artifact_disabled("sessions"),
                    "session persistence is disabled"
                );
                anyhow::ensure!(
                    crate::session::lock::claim_session(&lease.id.to_string())?
                        == crate::session::lock::SessionClaim::Owned,
                    "session already has another writer"
                );
                lease.writer.store(true, Ordering::Release);
                Ok::<_, anyhow::Error>(())
            })
            .await
        })
        .await;
    cleanup.settle().await;
    result
        .map_err(|_| agent_client_protocol::Error::new(-32603, "session writer task failed"))?
        .map_err(|_| {
            agent_client_protocol::Error::new(
                -32000,
                "could not claim exclusive session persistence",
            )
        })
}

#[cfg(feature = "mcp")]
type ClientMcpServers = HashMap<String, crate::extras::mcp::config::McpServerConfig>;
#[cfg(not(feature = "mcp"))]
type ClientMcpServers = ();

/// Operator opt-in that launches client-supplied stdio MCP servers without the
/// workspace-service sandbox, like a configured command server.
const ACP_TRUST_CLIENT_MCP_ENV: &str = "MINI_AGENT_ACP_TRUST_CLIENT_MCP";

/// Launch policy for MCP servers an ACP client supplies in `session/new`.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
enum ClientMcpPolicy {
    /// A network peer must not launch local processes (ACP TCP).
    Refused,
    /// Default for stdio: run in the dedicated workspace-service sandbox of
    /// the resolved backend, refusing the servers when it is unavailable.
    Sandboxed,
    /// `MINI_AGENT_ACP_TRUST_CLIENT_MCP=1`: launch unsandboxed.
    Trusted,
}

fn client_mcp_policy(is_tcp: bool, trusted_by_operator: bool) -> ClientMcpPolicy {
    if is_tcp {
        ClientMcpPolicy::Refused
    } else if trusted_by_operator {
        tracing::warn!(
            "ACP: {ACP_TRUST_CLIENT_MCP_ENV}=1 — client-supplied MCP servers run unsandboxed \
             with host filesystem and network access"
        );
        ClientMcpPolicy::Trusted
    } else {
        ClientMcpPolicy::Sandboxed
    }
}

/// Validate the MCP servers a client supplied in `session/new`.
///
/// ACP requires every agent to accept stdio servers. The server list comes
/// from editor configuration that may be repository-controlled (for example a
/// workspace settings file), which is not the human-trusted `mcp_servers`
/// path, so by default they are launched in the dedicated workspace-service
/// sandbox of the resolved sandbox backend (working directory = the session
/// root, the client's `env` plus the server process's `PATH` and `HOME`, with
/// network access denied). A backend that cannot enforce network denial is
/// refused at session creation rather than after a prompt begins.
/// `MINI_AGENT_ACP_TRUST_CLIENT_MCP=1` launches them unsandboxed instead.
/// Anything this server cannot honour is refused explicitly rather than
/// silently ignored: HTTP/SSE servers (not advertised), servers from a TCP
/// peer (a network client must not launch local processes), servers when the
/// sandbox is unavailable and not explicitly bypassed, and servers when MCP
/// is disabled or not compiled in.
fn client_mcp_servers(
    state: &AcpState,
    servers: &[McpServer],
) -> Result<ClientMcpServers, agent_client_protocol::Error> {
    let refuse = |message: String| Err(agent_client_protocol::Error::new(-32602, message));
    if servers.is_empty() {
        #[cfg(feature = "mcp")]
        return Ok(ClientMcpServers::new());
        #[cfg(not(feature = "mcp"))]
        return Ok(());
    }
    #[cfg(not(feature = "mcp"))]
    {
        let _ = state;
        refuse(
            "client-supplied MCP servers are not supported: this build has no MCP support".into(),
        )
    }
    #[cfg(feature = "mcp")]
    {
        use crate::extras::mcp::config::{McpServerConfig, McpStdioNetwork};
        if servers.len() > MAX_ACP_CLIENT_MCP_SERVERS {
            return refuse(format!(
                "ACP session accepts at most {MAX_ACP_CLIENT_MCP_SERVERS} client-supplied MCP servers"
            ));
        }
        let (sandbox, network) = match state.client_mcp {
            ClientMcpPolicy::Refused => {
                return refuse(
                    "client-supplied MCP servers are refused over ACP TCP; configure them in \
                     `mcp_servers` on the agent host instead"
                        .into(),
                );
            }
            ClientMcpPolicy::Trusted => (None, McpStdioNetwork::Inherit),
            ClientMcpPolicy::Sandboxed => {
                let backend = state.cli.resolve_sandbox_backend(&state.cfg);
                if backend == "zerobox" {
                    return refuse(
                        "client-supplied MCP servers require network denial, which the 'zerobox' \
                         workspace-service sandbox cannot enforce"
                            .into(),
                    );
                }
                if !matches!(
                    crate::sandbox::Sandbox::new(true, &backend).policy(),
                    crate::sandbox::SandboxPolicy::RequiredAndAvailable
                ) {
                    return refuse(format!(
                        "client-supplied MCP servers run in the '{backend}' workspace-service \
                         sandbox, which is not available; configure them in `mcp_servers` or set \
                         {ACP_TRUST_CLIENT_MCP_ENV}=1 to launch them unsandboxed"
                    ));
                }
                (Some(backend), McpStdioNetwork::Deny)
            }
        };
        if !state.cli.mcp_is_eligible(&state.cfg) {
            return refuse(
                "client-supplied MCP servers were sent but MCP is disabled for this agent".into(),
            );
        }
        let mut accepted = ClientMcpServers::new();
        for server in servers {
            let McpServer::Stdio(stdio) = server else {
                return refuse(
                    "only stdio MCP servers are supported; HTTP and SSE servers are not \
                     advertised by this agent"
                        .into(),
                );
            };
            if !stdio.command.is_absolute() {
                return refuse(format!(
                    "MCP server '{}' command must be an absolute path",
                    stdio.name
                ));
            }
            if accepted.contains_key(&stdio.name) {
                return refuse(format!("duplicate MCP server name '{}'", stdio.name));
            }
            accepted.insert(
                stdio.name.clone(),
                McpServerConfig::Command {
                    command: stdio.command.to_string_lossy().into_owned(),
                    defer: Default::default(),
                    args: stdio.args.clone(),
                    cwd: None,
                    env: stdio
                        .env
                        .iter()
                        .map(|variable| (variable.name.clone(), variable.value.clone()))
                        .collect(),
                    inherit_env: vec!["PATH".to_string(), "HOME".to_string()],
                    sandbox: sandbox.clone(),
                    network,
                },
            );
        }
        Ok(accepted)
    }
}

/// The configuration a prompt connects MCP with: the configured servers plus
/// the session's client-supplied ones. A configured server keeps its name;
/// a client server with the same name is skipped.
#[cfg(feature = "mcp")]
fn with_client_mcp_servers<'a>(
    cfg: &'a Config,
    client: &ClientMcpServers,
) -> std::borrow::Cow<'a, Config> {
    if client.is_empty() {
        return std::borrow::Cow::Borrowed(cfg);
    }
    let mut merged = cfg.clone();
    let servers = merged.mcp_servers.get_or_insert_with(HashMap::new);
    for (name, server) in client {
        if servers.contains_key(name) {
            tracing::warn!(
                "ACP: client MCP server '{}' shadows a configured server; using the configured one",
                name
            );
            continue;
        }
        servers.insert(name.clone(), server.clone());
    }
    std::borrow::Cow::Owned(merged)
}

async fn handle_set_session_mode(
    req: SetSessionModeRequest,
    responder: Responder<SetSessionModeResponse>,
    cx: ConnectionTo<Client>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _activity = state.idle.admit()?;
    let sessions = state.sessions.lock().await;
    let session = sessions
        .get(&req.session_id)
        .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown ACP session"))?;
    let turns = lock_unpoisoned(&session.turns);
    if !turns.queue.is_empty() {
        return Err(agent_client_protocol::Error::new(
            -32000,
            "ACP session already has an active prompt",
        ));
    }
    session
        .workspace
        .validate()
        .map_err(|error| agent_client_protocol::Error::new(-32602, error.to_string()))?;
    #[cfg(feature = "subagents")]
    if session
        .spend
        .background_pool()
        .is_some_and(|pool| pool.active_count() > 0)
    {
        return Err(agent_client_protocol::Error::new(
            -32000,
            "cancel or finish background tasks before changing permission mode",
        ));
    }
    let history = session
        .history
        .try_lock()
        .map_err(|_| agent_client_protocol::Error::new(-32000, "ACP session history is busy"))?;
    history
        .policy
        .set_mode(req.mode_id.0.as_ref())
        .map_err(|error| agent_client_protocol::Error::new(-32602, error))?;
    drop(history);
    drop(turns);
    drop(sessions);
    cx.send_notification(SessionNotification::new(
        req.session_id,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(req.mode_id)),
    ))?;
    responder.respond(SetSessionModeResponse::new())
}

async fn handle_close_session(
    req: CloseSessionRequest,
    responder: Responder<CloseSessionResponse>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _activity = state.idle.admit()?;
    let session_id = &req.session_id;
    tracing::info!("ACP close session: {}", session_id);

    // Cancel any active turn first, then remove the session.
    // Unknown or already-closed sessions are silently accepted (idempotent).
    {
        let turns = lock_unpoisoned(&state.cancel_routes).remove(session_id);
        if let Some(turns) = turns {
            let turns = lock_unpoisoned(&turns);
            if let Some(front) = turns.queue.front() {
                front.control.cancel();
            }
        }
    }

    let removed = {
        let mut sessions = state.sessions.lock().await;
        let removed = sessions.remove(session_id);
        state.idle.sessions_changed(sessions.len());
        removed
    };
    #[cfg(feature = "subagents")]
    if let Some(pool) = removed
        .as_ref()
        .and_then(|session| session.spend.background_pool())
    {
        pool.close().await;
    }
    // Stop the session's MCP and language servers. An idle session is torn
    // down before the response; a session whose cancelled turn is still
    // settling is torn down by a task that waits for that turn (the turn
    // itself closes whatever it was holding).
    if let Some(removed) = removed {
        let history = removed.history.clone();
        drop(removed);
        match history.clone().try_lock_owned() {
            Ok(mut idle_history) => idle_history.reuse.shutdown().await,
            Err(_) => {
                let activity = state.idle.admit()?;
                tokio::spawn(activity.track(async move {
                    history.lock().await.reuse.shutdown().await;
                }));
            }
        }
    }

    responder.respond(CloseSessionResponse::new())
}

fn canonical_session_workspace(
    path: &Path,
) -> Result<crate::paths::WorkspaceBinding, agent_client_protocol::Error> {
    if !path.is_absolute() {
        return Err(agent_client_protocol::Error::new(
            -32602,
            format!(
                "invalid ACP session cwd '{}': path must be absolute",
                path.display()
            ),
        ));
    }
    crate::paths::WorkspaceBinding::capture(path).map_err(|error| {
        agent_client_protocol::Error::new(
            -32602,
            format!("invalid ACP session cwd '{}': {error}", path.display()),
        )
    })
}

fn acp_session_sandbox(
    state: &AcpState,
    workspace: Arc<crate::paths::WorkspaceBinding>,
) -> Result<crate::sandbox::Sandbox, agent_client_protocol::Error> {
    let (authority, sandbox) =
        crate::permission::resolve_configured_execution_authority(&state.cli, &state.cfg)
            .map_err(|error| agent_client_protocol::Error::new(-32602, error.to_string()))?;
    Ok(crate::permission::bind_configured_shell(
        &state.cli,
        &state.cfg,
        authority,
        &workspace,
        state.shell_search_path.as_deref(),
        sandbox,
    )
    .with_workspace_binding(workspace))
}

/// Apply a `_meta.goal` object from a prompt request.
///
/// Shape: `{"goal": {"objective": "...", "criteria": ["..."], "replace": false, "clear": false}}`.
/// Replacing an unfinished goal is refused unless `replace` is set, for the same
/// reason `/goal` refuses it: the rounds and verdicts on it are the record of
/// the work done so far.
#[cfg(feature = "goal")]
fn apply_meta_goal(
    state: &AcpState,
    store: &crate::extras::goal::GoalStore,
    meta: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let Some(spec) = meta.get("goal") else {
        return Ok(());
    };
    let (provider, model) = acp_provider_and_model(&state.cli, &state.cfg);

    let MetaGoal {
        objective,
        criteria,
        replace,
    } = match parse_meta_goal(spec)? {
        None => {
            store.clear();
            return Ok(());
        }
        Some(parsed) => parsed,
    };

    // The same factory every other surface uses, so the same objective is
    // bounded, checked and judged the same way in an editor as in a terminal.
    let goal = crate::extras::goal::Goal::configured(
        objective,
        criteria,
        crate::extras::goal::GoalDefaults {
            cfg: &state.cfg,
            provider: &provider,
            model: &model,
        },
    )
    .map_err(|e| e.to_string())?;
    store.set(goal, replace).map_err(|e| e.to_string())
}

/// A goal a client asked for.
#[cfg(feature = "goal")]
#[derive(Debug)]
struct MetaGoal {
    objective: String,
    criteria: Vec<String>,
    replace: bool,
}

/// Read a `_meta.goal` object. `None` means the client asked to clear.
///
/// A closed schema. An editor that sends `checks` or `judge` is asking for
/// something this channel deliberately cannot grant — those carry the
/// authority of whoever configured the harness — and silently ignoring the
/// field would leave the client believing its goal was gated when it is not.
/// The same goes for a criterion that is not a string: dropping it quietly
/// would weaken the objective without saying so.
#[cfg(feature = "goal")]
fn parse_meta_goal(spec: &serde_json::Value) -> Result<Option<MetaGoal>, String> {
    let spec = spec
        .as_object()
        .ok_or_else(|| "_meta.goal must be an object".to_string())?;
    if let Some(unknown) = spec
        .keys()
        .find(|key| !matches!(key.as_str(), "objective" | "criteria" | "replace" | "clear"))
    {
        return Err(format!(
            "_meta.goal does not accept `{unknown}`; goal checks and the judge come from this \
             installation's configuration, not from the client"
        ));
    }

    if spec.get("clear").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let Some(objective) = spec.get("objective").and_then(serde_json::Value::as_str) else {
        return Err("_meta.goal needs an objective".to_string());
    };
    let criteria = match spec.get("criteria") {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "_meta.goal criteria must be strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("_meta.goal criteria must be an array of strings".to_string()),
    };
    Ok(Some(MetaGoal {
        objective: objective.to_string(),
        criteria,
        replace: spec.get("replace").and_then(serde_json::Value::as_bool) == Some(true),
    }))
}

async fn handle_prompt(
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    state: Arc<AcpState>,
) -> Result<(), agent_client_protocol::Error> {
    let activity = state.idle.admit()?;
    let session_id = req.session_id.clone();

    tracing::info!("ACP prompt for session {}", session_id);

    // `/cron` only edits the session's schedule store and never touches a
    // running turn, so it is answered before (and without) turn admission.
    if let Some(arguments) = commands::cron_arguments(&req.prompt) {
        return cron::handle_command(&arguments, &session_id, responder, cx, &state).await;
    }

    let command = commands::classify(&req.prompt)?;
    let prompt_input = parse_prompt_blocks(&req.prompt)?;

    let snapshot = {
        let sessions = state.sessions.lock().await;
        let sess = sessions
            .get(&session_id)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown ACP session"))?;
        admit_turn(&state, sess)?
    };

    // Opt-in `@agent:<name>` routing (`headless_agent_mentions`). Resolved
    // after admission so a refusal settles this generation like any other
    // invalid prompt, and before `_meta.goal` so a refused prompt changes
    // nothing. Commands are never parsed.
    #[cfg(feature = "subagents")]
    let prompt_input = if command.is_none() && state.cfg.resolve_headless_agent_mentions() {
        route_prompt_agent_mentions(&req.prompt, &snapshot.workspace, prompt_input)?
    } else {
        prompt_input
    };

    // A client sets or clears the session's goal through the request's `_meta`
    // rather than through prompt text: the objective is a long-lived
    // instruction channel, and a text directive would let anything that reaches
    // the prompt install one. It is applied only once this prompt has been
    // admitted, so a prompt rejected because another turn is active cannot
    // clear or replace the goal that turn is being judged against. A refused
    // goal drops `registration`, which settles the just-admitted generation.
    #[cfg(feature = "goal")]
    if let Some(meta) = req.meta.as_ref()
        && let Err(error) = apply_meta_goal(&state, &snapshot.goal_store, meta)
    {
        return Err(agent_client_protocol::Error::new(-32602, error));
    }

    let request_cancellation = responder.cancellation();
    if request_cancellation.is_cancelled() {
        snapshot.control.cancel();
    }
    let request_cancellation_bridge =
        RequestCancellationBridge::new(request_cancellation, snapshot.control.clone());
    spawn_turn(
        state,
        cx,
        snapshot,
        prompt_input,
        command,
        session_id,
        PromptReply::Client(responder),
        Some(request_cancellation_bridge),
        activity,
    )
}

/// Register the next turn generation of `sess`, or refuse while another turn
/// (a client prompt or a `/cron` fire) is active: one session never runs two
/// turns at once. Called under the session catalogue lock.
fn admit_turn(
    state: &AcpState,
    sess: &SessionState,
) -> Result<PromptSessionSnapshot, agent_client_protocol::Error> {
    let control = Arc::new(TurnControl::new());
    let hook_lease;
    let generation = {
        let mut turns = lock_unpoisoned(&sess.turns);
        if !turns.queue.is_empty() {
            return Err(agent_client_protocol::Error::new(
                -32000,
                "ACP session already has an active prompt",
            ));
        }
        hook_lease = state
            .hook_roots
            .as_ref()
            .map(|claims| claims.acquire(sess.workspace.root()));
        let generation = turns.next_generation;
        turns.next_generation = turns.next_generation.wrapping_add(1);
        turns.queue.push_back(RegisteredTurn {
            generation,
            control: control.clone(),
        });
        generation
    };
    Ok(PromptSessionSnapshot {
        spend: sess.spend.clone(),
        history: sess.history.clone(),
        workspace: sess.workspace.clone(),
        context: sess.context.clone(),
        read_tracker: sess.read_tracker.clone(),
        todo_store: sess.todo_store.clone(),
        #[cfg(feature = "goal")]
        goal_store: sess.goal_store.clone(),
        sandbox: sess.sandbox.clone(),
        #[cfg(feature = "js")]
        js_session_state: sess.js_session_state.clone(),
        #[cfg(feature = "skills")]
        skill_services: sess.skill_services.clone(),
        control: control.clone(),
        registration: TurnRegistration {
            generation,
            turns: sess.turns.clone(),
            control,
        },
        hook_lease,
    })
}

/// Run an admitted turn on its own task, owned by `activity` until the turn,
/// its hooks and its tracked work settle. The connection owns only a waiter
/// that cancels the turn on teardown.
#[allow(clippy::too_many_arguments)]
fn spawn_turn(
    state: Arc<AcpState>,
    cx: ConnectionTo<Client>,
    snapshot: PromptSessionSnapshot,
    prompt_input: crate::agent::input::PromptInput,
    command: Option<commands::Command>,
    session_id: SessionId,
    reply: PromptReply,
    request_cancellation_bridge: Option<RequestCancellationBridge>,
    activity: idle::Activity,
) -> Result<(), agent_client_protocol::Error> {
    let PromptSessionSnapshot {
        spend,
        history,
        workspace,
        context,
        read_tracker,
        todo_store,
        #[cfg(feature = "goal")]
        goal_store,
        sandbox,
        #[cfg(feature = "js")]
        js_session_state,
        #[cfg(feature = "skills")]
        skill_services,
        control,
        registration,
        hook_lease,
    } = snapshot;

    control.attach_sandbox(sandbox.clone());

    cx.spawn({
        let cx = cx.clone();
        let teardown_control = control.clone();
        async move {
            // The connection owns this waiter, but the Tokio task owns the
            // actual turn. On connection teardown the waiter signals
            // cancellation while the turn remains alive just long enough to
            // kill/reap hooks and finish tracked blocking work.
            let _cancel_on_connection_drop = CancelTurnOnDrop(teardown_control);
            tokio::spawn(activity.track(async move {
                // Held until the turn, its hooks and its tracked work settle.
                let _hook_lease = hook_lease;
                let registration = registration;
                let _request_cancellation_bridge = request_cancellation_bridge;
                if control.is_cancelled() {
                    let _ = respond_terminal(&registration, reply, StopReason::Cancelled);
                    return Ok(());
                }
                let history = history.lock_owned().await;
                let result = crate::spend::run::scope(
                    Some(spend),
                    crate::startup::heap_future(|| {
                        run_prompt(
                            &state,
                            &prompt_input,
                            command,
                            session_id,
                            history,
                            workspace,
                            context,
                            read_tracker,
                            todo_store,
                            #[cfg(feature = "goal")]
                            goal_store,
                            sandbox,
                            #[cfg(feature = "js")]
                            js_session_state,
                            #[cfg(feature = "skills")]
                            skill_services,
                            reply,
                            cx,
                            control,
                            &registration,
                        )
                    }),
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
            }))
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

fn parse_prompt_blocks(
    blocks: &[ContentBlock],
) -> Result<crate::agent::input::PromptInput, agent_client_protocol::Error> {
    use crate::session::user::{MAX_USER_BLOCKS, MAX_USER_TEXT_BYTES};
    if blocks.len() > MAX_USER_BLOCKS {
        return Err(agent_client_protocol::Error::new(
            -32602,
            "ACP prompt has too many content blocks",
        ));
    }
    if !blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
    {
        let text = render_prompt_blocks(blocks)?;
        if text.len() > MAX_USER_TEXT_BYTES {
            return Err(agent_client_protocol::Error::new(
                -32602,
                "ACP prompt text exceeds the byte limit",
            ));
        }
        return Ok(crate::agent::input::PromptInput::text(text));
    }
    #[cfg(not(feature = "multimodal"))]
    return Err(agent_client_protocol::Error::new(
        -32602,
        "image ACP prompts require the multimodal feature",
    ));
    #[cfg(feature = "multimodal")]
    {
        use crate::session::user::{ImageMime, PersistedUserMessage, UserBlock};
        let mut content = Vec::with_capacity(blocks.len());
        for block in blocks {
            match block {
                ContentBlock::Image(image) => {
                    let mime_type = ImageMime::parse(&image.mime_type).ok_or_else(|| {
                        agent_client_protocol::Error::new(-32602, "unsupported ACP image MIME type")
                    })?;
                    // The inline data is authoritative. URI/annotations/meta are
                    // display metadata, never a fetch or file-read request.
                    content.push(UserBlock::Image {
                        data: image.data.clone(),
                        mime_type,
                    });
                }
                _ => content.push(UserBlock::Text {
                    text: render_prompt_blocks(std::slice::from_ref(block))?,
                }),
            }
        }
        let user = PersistedUserMessage::new(content)
            .map_err(|error| agent_client_protocol::Error::new(-32602, error))?;
        Ok(crate::agent::input::PromptInput::user(user))
    }
}

/// Resolve `@agent:<name>` mentions in the client's own text blocks.
///
/// Only `Text` blocks are parsed: embedded resources and links are reference
/// material and can neither route nor fail a prompt. Resolved aliases are
/// normalised in place (the normalised text is what the session records) and
/// the routing note rides on this request only. An unknown canonical name is
/// an invalid-params error naming the valid personas.
#[cfg(feature = "subagents")]
fn route_prompt_agent_mentions(
    blocks: &[ContentBlock],
    workspace: &crate::paths::WorkspaceBinding,
    parsed: crate::agent::input::PromptInput,
) -> Result<crate::agent::input::PromptInput, agent_client_protocol::Error> {
    let mut names: Vec<String> = Vec::new();
    let mut normalised_blocks: Option<Vec<ContentBlock>> = None;
    for (index, block) in blocks.iter().enumerate() {
        let ContentBlock::Text(text) = block else {
            continue;
        };
        let mentions = crate::context::mentions::parse_agent_mentions(&text.text, Some(workspace))
            .map_err(|error| agent_client_protocol::Error::new(-32602, error.to_string()))?;
        let Some(mentions) = mentions else {
            continue;
        };
        for name in mentions.names {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        if mentions.text != text.text {
            let mut normalised = text.clone();
            normalised.text = mentions.text;
            normalised_blocks.get_or_insert_with(|| blocks.to_vec())[index] =
                ContentBlock::Text(normalised);
        }
    }
    if names.is_empty() {
        return Ok(parsed);
    }
    let prompt = match normalised_blocks {
        Some(blocks) => parse_prompt_blocks(&blocks)?,
        None => parsed,
    };
    Ok(prompt.with_turn_directive(Some(crate::context::mentions::routing_directive(&names))))
}

#[cfg(test)]
mod typed_prompt_tests {
    use super::*;

    /// An untrusted temporary workspace: compiled-in personas such as
    /// `rust-maintainer` resolve, project personas never do.
    #[cfg(feature = "subagents")]
    fn mention_workspace() -> (std::path::PathBuf, crate::paths::WorkspaceBinding) {
        let root =
            std::env::temp_dir().join(format!("mini-agent-acp-mentions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&root).unwrap();
        (root, binding)
    }

    #[cfg(feature = "subagents")]
    fn route(
        blocks: &[ContentBlock],
        workspace: &crate::paths::WorkspaceBinding,
    ) -> Result<crate::agent::input::PromptInput, agent_client_protocol::Error> {
        route_prompt_agent_mentions(blocks, workspace, parse_prompt_blocks(blocks).unwrap())
    }

    #[cfg(feature = "subagents")]
    #[test]
    fn acp_agent_mention_routes_with_a_non_persisted_directive() {
        let (root, workspace) = mention_workspace();
        let blocks = [ContentBlock::Text(TextContent::new(
            "ask @agent-rust-maintainer, then @agent:rust-maintainer again",
        ))];
        let prompt = route(&blocks, &workspace).unwrap();
        assert_eq!(
            prompt.literal_text(),
            "ask @agent:rust-maintainer, then @agent:rust-maintainer again"
        );
        let directive = prompt.turn_directive().expect("routing directive");
        assert!(directive.contains("\"rust-maintainer\""), "{directive}");
        assert_eq!(
            directive.matches("rust-maintainer").count(),
            1,
            "{directive}"
        );
        let Message::User { content } = prompt.to_message() else {
            panic!("a prompt renders as a user message");
        };
        let texts: Vec<_> = content
            .iter()
            .filter_map(|item| match item {
                rig::message::UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert_eq!(texts[0], prompt.literal_text());
        assert!(texts[1].contains(directive), "{texts:?}");
        drop(workspace);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "subagents")]
    #[test]
    fn acp_unknown_agent_mention_is_invalid_params_naming_valid_agents() {
        let (root, workspace) = mention_workspace();
        let error = route(
            &[ContentBlock::Text(TextContent::new(
                "run @agent:no-such-persona now",
            ))],
            &workspace,
        )
        .unwrap_err();
        assert_eq!(i32::from(error.code), -32602);
        assert!(
            error
                .message
                .contains("unknown agent '@agent:no-such-persona'")
                && error.message.contains("rust-maintainer"),
            "{}",
            error.message
        );
        drop(workspace);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "subagents")]
    #[test]
    fn acp_agent_mentions_ignore_reference_blocks_and_plain_prompts() {
        let (root, workspace) = mention_workspace();
        // Resource text is reference material: it can neither fail nor route.
        let blocks = [
            ContentBlock::Text(TextContent::new("summarise the report")),
            ContentBlock::ResourceLink(
                ResourceLink::new("report", "file:///workspace/report.md")
                    .description(Some("mentions @agent:no-such-persona".to_string())),
            ),
        ];
        let prompt = route(&blocks, &workspace).unwrap();
        assert_eq!(prompt.turn_directive(), None);
        assert_eq!(
            prompt.to_message(),
            parse_prompt_blocks(&blocks).unwrap().to_message()
        );
        let plain = [ContentBlock::Text(TextContent::new("mail me@agent:x.org"))];
        let prompt = route(&plain, &workspace).unwrap();
        assert_eq!(prompt.turn_directive(), None);
        assert_eq!(prompt.to_message(), Message::user("mail me@agent:x.org"));
        drop(workspace);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn acp53_image_prompt_feature_policy_is_explicit_and_nonfetching() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("before 雪")),
            ContentBlock::Image(
                ImageContent::new("aW5lcnQ=", "image/png")
                    .uri("file:///private/inert-never-opened.png"),
            ),
            ContentBlock::Text(TextContent::new("after $HOME /undo")),
        ];
        #[cfg(not(feature = "multimodal"))]
        assert!(parse_prompt_blocks(&blocks).is_err());
        #[cfg(feature = "multimodal")]
        {
            let prompt = parse_prompt_blocks(&blocks).unwrap();
            assert_eq!(prompt.literal_text(), "before 雪\nafter $HOME /undo");
            let expected = crate::session::user::PersistedUserMessage::new(vec![
                crate::session::user::UserBlock::Text {
                    text: "before 雪".into(),
                },
                crate::session::user::UserBlock::Image {
                    data: "aW5lcnQ=".into(),
                    mime_type: crate::session::user::ImageMime::Png,
                },
                crate::session::user::UserBlock::Text {
                    text: "after $HOME /undo".into(),
                },
            ])
            .unwrap();
            assert_eq!(prompt.to_message(), expected.to_message());
        }
    }
    #[test]
    fn acp53_prompt_text_and_block_limits_precede_runner_admission() {
        assert!(
            parse_prompt_blocks(&vec![
                ContentBlock::Text(TextContent::new("x"));
                crate::session::user::MAX_USER_BLOCKS + 1
            ])
            .is_err()
        );
        assert!(
            parse_prompt_blocks(&[ContentBlock::Text(TextContent::new(
                "x".repeat(crate::session::user::MAX_USER_TEXT_BYTES + 1)
            ))])
            .is_err()
        );
        let text = "literal $HOME @file /undo 雪";
        assert_eq!(
            parse_prompt_blocks(&[ContentBlock::Text(TextContent::new(text))])
                .unwrap()
                .to_message(),
            Message::user(text)
        );
    }
}

fn render_prompt_blocks(blocks: &[ContentBlock]) -> Result<String, agent_client_protocol::Error> {
    let mut rendered = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Text(text) => rendered.push(text.text.clone()),
            ContentBlock::ResourceLink(resource) => {
                let mut link = format!("[resource: {}]({})", resource.name, resource.uri);
                if let Some(description) = resource.description.as_deref()
                    && !description.trim().is_empty()
                {
                    link.push('\n');
                    link.push_str(description);
                }
                rendered.push(link);
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    rendered.push(format!("[embedded resource: {}]\n{}", text.uri, text.text))
                }
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    return Err(agent_client_protocol::Error::new(
                        -32602,
                        "binary embedded resources are not supported in ACP prompts",
                    ));
                }
                _ => {
                    return Err(agent_client_protocol::Error::new(
                        -32602,
                        "unsupported embedded resource in ACP prompt",
                    ));
                }
            },
            ContentBlock::Image(_) | ContentBlock::Audio(_) => {
                return Err(agent_client_protocol::Error::new(
                    -32602,
                    "image and audio ACP prompts are not supported",
                ));
            }
            _ => {
                return Err(agent_client_protocol::Error::new(
                    -32602,
                    "unsupported ACP prompt content block",
                ));
            }
        }
    }
    let rendered = rendered.join("\n");
    if rendered.trim().is_empty() {
        return Err(agent_client_protocol::Error::new(
            -32602,
            "ACP prompt must contain non-empty text or a resource link",
        ));
    }
    Ok(rendered)
}

// --- Permission Bridge ---

/// The session AllowAlways grant one permission prompt installs, plus the
/// `_meta` that tells the client exactly what it covers.
///
/// A caller-supplied suggestion wins. Without one, a path tool grants exactly
/// the prompted path as a literal scope, so a model-chosen `/home/me/**`
/// approved "for this session" never becomes a glob over the whole tree; any
/// other tool grants its literal permission key. `_meta.suggestedPattern` is
/// the human-readable rule (every installed scope, decoded) and
/// `_meta.allowPatterns` lists the exact patterns handed to
/// `add_session_allowlist`, primary first.
fn allow_always_scope(
    ask: &crate::permission::ask::AskRequest,
) -> (String, serde_json::Map<String, serde_json::Value>) {
    let pattern = match &ask.suggested_pattern {
        Some(pattern) => pattern.clone(),
        None => crate::permission::checker::exact_allow_scope_for_input(&ask.tool, &ask.input)
            .unwrap_or_else(|| ask.input.clone()),
    };
    let installed: Vec<&String> = std::iter::once(&pattern)
        .chain(&ask.additional_allow_patterns)
        .collect();
    let readable = installed
        .iter()
        .map(|pattern| crate::permission::pattern::describe_allow_pattern(pattern))
        .collect::<Vec<_>>()
        .join(" and ");
    let mut meta = serde_json::Map::new();
    meta.insert(
        "suggestedPattern".to_owned(),
        serde_json::Value::String(readable),
    );
    meta.insert("allowPatterns".to_owned(), serde_json::json!(installed));
    meta.insert(
        "scope".to_owned(),
        serde_json::Value::String("session".to_owned()),
    );
    (pattern, meta)
}

#[derive(Debug, PartialEq, Eq)]
enum QuestionSelection {
    Choice(usize),
    NewMessage,
}

/// Decode only an exact advertised identity. This does not construct a
/// permission decision, even though ACP renders the choices with that RPC.
fn question_selection(
    id: &str,
    index: usize,
    item: &crate::question::Question,
    selected: &str,
) -> Option<QuestionSelection> {
    let prefix = format!("{id}:{index}");
    if item.allow_free_text && selected == format!("{prefix}:new-message") {
        return Some(QuestionSelection::NewMessage);
    }
    (0..item.options.len())
        .find(|choice| selected == format!("{prefix}:choice:{choice}"))
        .map(QuestionSelection::Choice)
}

#[cfg(test)]
mod question_selection_tests {
    use super::*;
    use crate::question::{Question, QuestionOption};

    #[test]
    fn ask_user_acp_answers_require_exact_current_advertised_option_ids() {
        let id = uuid::Uuid::new_v4().to_string();
        let mut item = Question {
            question: "Which branch?".into(),
            options: vec![QuestionOption {
                label: "main".into(),
                description: String::new(),
            }],
            allow_free_text: true,
        };
        assert_eq!(
            question_selection(&id, 0, &item, &format!("{id}:0:choice:0")),
            Some(QuestionSelection::Choice(0))
        );
        assert_eq!(
            question_selection(&id, 0, &item, &format!("{id}:0:new-message")),
            Some(QuestionSelection::NewMessage)
        );
        for stale in [
            format!("{id}:1:choice:0"),
            format!("{}:0:choice:0", uuid::Uuid::new_v4()),
            format!("{id}:0:choice:1"),
            format!("{id}:0:choice:00"),
            format!("{id}:0:cancel"),
            "allow_once".into(),
            "allow_always".into(),
            "main".into(),
            "0".into(),
        ] {
            assert_eq!(question_selection(&id, 0, &item, &stale), None);
        }
        item.allow_free_text = false;
        assert_eq!(
            question_selection(&id, 0, &item, &format!("{id}:0:new-message")),
            None
        );
        assert_eq!(
            question_selection(&id, 0, &item, &format!("{id}:0:choice:0")),
            Some(QuestionSelection::Choice(0))
        );
    }
}

/// Clarification uses advertised option IDs as task data only. This path
/// never calls permission grant installation or constructs UserDecision.
async fn drive_question_request(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    mut question: crate::permission::ask::QuestionRequest,
    control: &Arc<TurnControl>,
    #[cfg(feature = "goal")] blocked_ms: &Arc<std::sync::atomic::AtomicU64>,
) {
    use crate::question::{Answer, QuestionAnswer};
    if question.questions.validate().is_err()
        || question.id.len() != 36
        || uuid::Uuid::parse_str(&question.id).is_err()
        || question.reply.is_closed()
        || control.is_cancelled()
    {
        let _ = question.reply.send(QuestionAnswer::Cancelled);
        return;
    }
    let synthetic = question.tool_call_id.is_none();
    let call_id = ToolCallId::new(
        question
            .tool_call_id
            .clone()
            .unwrap_or_else(|| format!("question-{}", question.id)),
    );
    if synthetic {
        let announced = ToolCall::new(
            call_id.clone(),
            "Clarification (task data, no permission grant)",
        );
        if cx
            .send_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCall(announced),
            ))
            .is_err()
        {
            let _ = question.reply.send(QuestionAnswer::Cancelled);
            return;
        }
    }
    let mut answers = Vec::new();
    let mut waiting = false;
    for (index, item) in question.questions.0.iter().enumerate() {
        let prefix = format!("{}:{index}", question.id);
        let mut options = item
            .options
            .iter()
            .enumerate()
            .map(|(choice, option)| {
                PermissionOption::new(
                    format!("{prefix}:choice:{choice}"),
                    crate::ui::events::sanitize_output(&option.label),
                    PermissionOptionKind::AllowOnce,
                )
            })
            .collect::<Vec<_>>();
        let other_id = format!("{prefix}:new-message");
        if item.allow_free_text {
            options.push(PermissionOption::new(
                other_id.clone(),
                "Answer in a new user message (park this turn)",
                PermissionOptionKind::AllowOnce,
            ));
        }
        options.push(PermissionOption::new(
            format!("{prefix}:cancel"),
            "Cancel clarification",
            PermissionOptionKind::RejectOnce,
        ));
        let content = item
            .options
            .iter()
            .enumerate()
            .map(|(choice, option)| {
                format!("{}. {} — {}", choice + 1, option.label, option.description)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let fields = ToolCallUpdateFields::new()
            .title(format!(
                "Clarification {}/{}: {}",
                index + 1,
                question.questions.0.len(),
                crate::ui::events::sanitize_output(&item.question)
            ))
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new(crate::ui::events::sanitize_output(&content)),
            ))]);
        let req = RequestPermissionRequest::new(
            session_id.clone(),
            ToolCallUpdate::new(call_id.clone(), fields),
            options,
        );
        #[cfg(feature = "goal")]
        let started = std::time::Instant::now();
        let response = tokio::select! {
            biased;
            _=control.cancelled()=>None,
            _=question.reply.closed()=>None,
            response=cx.send_request(req).block_task()=>Some(response),
        };
        #[cfg(feature = "goal")]
        blocked_ms.fetch_add(
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
        let selected = match response {
            Some(Ok(response)) => match response.outcome {
                RequestPermissionOutcome::Selected(selected) => {
                    Some(selected.option_id.0.to_string())
                }
                _ => None,
            },
            _ => None,
        };
        match selected
            .as_deref()
            .and_then(|selected| question_selection(&question.id, index, item, selected))
        {
            Some(QuestionSelection::NewMessage) => {
                waiting = true;
                break;
            }
            Some(QuestionSelection::Choice(choice)) => answers.push(Answer::Option(choice)),
            None => {
                let _ = question.reply.send(QuestionAnswer::Cancelled);
                return;
            }
        }
    }
    if synthetic {
        let text = if waiting {
            "Waiting for the next ordinary user message; no free-text reply protocol or permission grant was used"
        } else {
            "Clarification answered; no permission was granted"
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new(text),
            ))]);
        let _ = cx.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(call_id, fields)),
        ));
    }
    let answer = if waiting {
        QuestionAnswer::AwaitingUser(answers)
    } else {
        QuestionAnswer::Answered(answers)
    };
    let _ = question.reply.send(answer);
}

struct BridgePolicy {
    policy: policy::SessionPolicy,
    permission: Option<crate::permission::checker::PermCheck>,
}

async fn drive_permission_bridge(
    cx: ConnectionTo<Client>,
    _activity: idle::Activity,
    session_id: SessionId,
    mut ask_rx: crate::permission::ask::AskReceiver,
    control: Arc<TurnControl>,
    runtime_policy: Option<BridgePolicy>,
    // Milliseconds this turn spent waiting on the user. A goal's time bound
    // measures how long the agent worked, not how long the person took to
    // answer, so the round subtracts this from its wall clock.
    #[cfg(feature = "goal")] blocked_ms: Arc<std::sync::atomic::AtomicU64>,
) {
    use crate::permission::ask::UserDecision;

    const OPT_ALLOW_ONCE: &str = "allow_once";
    const OPT_ALLOW_ALWAYS: &str = "allow_always";
    const OPT_DENY: &str = "deny";
    const OPT_DENY_ALWAYS: &str = "deny_always";

    loop {
        let ask = tokio::select! {
            biased;
            _ = control.cancelled() => break,
            ask = ask_rx.recv() => match ask {
                Some(ask) => ask,
                None => return,
            },
        };

        let mut ask = match ask {
            #[cfg(feature = "mcp")]
            crate::permission::ask::UserRequest::McpPermission(request) => {
                let _ = request.request.reply.send(UserDecision::Deny);
                continue;
            }
            #[cfg(feature = "mcp")]
            crate::permission::ask::UserRequest::McpElicitation(request) => {
                // ACP has no form-response contract. Refuse instead of routing
                // a server question through the permission approval protocol.
                let _ = request
                    .question
                    .reply
                    .send(crate::question::QuestionAnswer::Cancelled);
                continue;
            }
            crate::permission::ask::UserRequest::Permission(ask) => ask,
            crate::permission::ask::UserRequest::Question(question) => {
                drive_question_request(
                    &cx,
                    &session_id,
                    question,
                    &control,
                    #[cfg(feature = "goal")]
                    &blocked_ms,
                )
                .await;
                continue;
            }
        };
        let (suggested, allow_always_meta) = allow_always_scope(&ask);
        let synthetic_tool_call_id = ask.tool_call_id.is_none();
        let title = if synthetic_tool_call_id {
            format!("Permission: {}", ask.tool)
        } else {
            ask.tool.to_string()
        };
        let title = match &ask.agent_name {
            Some(name) => format!("[subagent: {name}] {title}"),
            None => title,
        };
        let title = match ask.diagnostic {
            Some(diagnostic) => format!("{title} [{diagnostic}]"),
            None => title,
        };
        let tool_call_id = ToolCallId::new(
            ask.tool_call_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        );
        let content = vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
            ask.input.clone(),
        )))];
        if synthetic_tool_call_id {
            let announced = ToolCall::new(tool_call_id.clone(), title.clone())
                .content(content.clone())
                .raw_input(Some(serde_json::Value::String(ask.input.clone())));
            if let Err(error) = cx.send_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCall(announced),
            )) {
                tracing::warn!("ACP failed to announce synthetic permission tool call: {error}");
                let _ = ask.reply.send(UserDecision::Deny);
                continue;
            }
        }
        let tool_call = ToolCallUpdate::new(
            tool_call_id.clone(),
            ToolCallUpdateFields::new().title(title).content(content),
        );
        let mut options = vec![
            PermissionOption::new(
                OPT_ALLOW_ONCE,
                "Allow once",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                OPT_ALLOW_ALWAYS,
                "Allow for this session",
                PermissionOptionKind::AllowAlways,
            )
            .meta(allow_always_meta),
            PermissionOption::new(OPT_DENY, "Deny", PermissionOptionKind::RejectOnce),
        ];
        if runtime_policy.is_some() {
            options.push(PermissionOption::new(
                OPT_DENY_ALWAYS,
                "Deny this exact request for this session",
                PermissionOptionKind::RejectAlways,
            ));
        }
        let req = RequestPermissionRequest::new(session_id.clone(), tool_call, options);

        #[cfg(feature = "goal")]
        let waiting_since = std::time::Instant::now();
        let response = tokio::select! {
            biased;
            _ = control.cancelled() => None,
            _ = ask.reply.closed() => None,
            res = cx.send_request(req).block_task() => Some(res),
        };
        #[cfg(feature = "goal")]
        blocked_ms.fetch_add(
            u64::try_from(waiting_since.elapsed().as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );

        let mut choice = None;
        let (mut decision, mut notice) = match response {
            None => (UserDecision::Deny, "Permission request cancelled"),
            Some(Ok(resp)) => match resp.outcome {
                RequestPermissionOutcome::Cancelled => {
                    (UserDecision::Deny, "Permission request cancelled")
                }
                RequestPermissionOutcome::Selected(sel) => {
                    let id = sel.option_id.0.as_ref();
                    if id == OPT_ALLOW_ALWAYS {
                        let entries = std::iter::once(suggested.clone())
                            .chain(ask.additional_allow_patterns.iter().cloned())
                            .map(|pattern| (ask.tool.to_string(), pattern))
                            .collect();
                        choice = Some(policy::Choice::Allow(entries));
                        (UserDecision::AllowAlways(suggested), "Permission approved")
                    } else if id == OPT_ALLOW_ONCE {
                        (UserDecision::AllowOnce, "Permission approved")
                    } else if id == OPT_DENY_ALWAYS && runtime_policy.is_some() {
                        choice = Some(policy::Choice::Deny(
                            ask.tool.to_string(),
                            ask.input.clone(),
                        ));
                        (UserDecision::Deny, "Exact request denied for this session")
                    } else {
                        (UserDecision::Deny, "Permission denied")
                    }
                }
                _ => (UserDecision::Deny, "Permission denied"),
            },
            Some(Err(e)) => {
                tracing::warn!("ACP permission request failed: {e}");
                (UserDecision::Deny, "Permission request failed")
            }
        };
        if control.is_cancelled() || ask.reply.is_closed() {
            choice = None;
            decision = UserDecision::Deny;
            notice = "Permission request cancelled";
        } else if let (Some(runtime), Some(selected)) = (&runtime_policy, &choice)
            && !runtime.policy.can_record(selected)
        {
            choice = None;
            decision = UserDecision::Deny;
            notice = "Session permission choice capacity exceeded";
        }
        if let (Some(runtime), Some(policy::Choice::Deny(tool, input))) = (&runtime_policy, &choice)
            && let Some(permission) = &runtime.permission
        {
            permission
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .add_session_exact_deny(tool, input);
        }
        // This lifecycle represents the approval request, not the underlying
        // effect. Ordinary tool calls remain owned by the runner's result event.
        if synthetic_tool_call_id {
            let status = match &decision {
                UserDecision::AllowOnce | UserDecision::AllowAlways(_) => ToolCallStatus::Completed,
                UserDecision::Deny => ToolCallStatus::Failed,
            };
            let fields =
                ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(notice),
                    ))]);
            let update = ToolCallUpdate::new(tool_call_id, fields);
            if let Err(error) = cx.send_notification(SessionNotification::new(
                session_id.clone(),
                SessionUpdate::ToolCallUpdate(update),
            )) {
                tracing::warn!("ACP failed to settle synthetic permission tool call: {error}");
            }
        }
        // Notification serialization and checker locking are synchronous, but
        // another owned task may still cancel or abandon this reply meanwhile.
        if control.is_cancelled() || ask.reply.is_closed() {
            decision = UserDecision::Deny;
            choice = None;
        }
        // Successful delivery is the accepted selection boundary. Later turn
        // cancellation does not revoke an already-delivered human choice.
        if ask.reply.send(decision).is_ok()
            && let (Some(runtime), Some(choice)) = (&runtime_policy, choice)
            && let Err(error) = runtime.policy.record(choice)
        {
            tracing::error!("ACP session choice could not be retained: {error}");
        }
    }
    // Drain remaining buffered asks as Deny after control cancelled.
    while let Ok(ask) = ask_rx.try_recv() {
        ask.cancel();
    }
}

// --- Prompt Execution ---

/// Where a turn's terminal outcome goes: the client's `session/prompt`
/// response, or, for a `/cron` fire that no client request started, a
/// host-labelled `session/update` in that session.
enum PromptReply {
    Client(Responder<PromptResponse>),
    Scheduled(cron::ScheduledReply),
}

impl PromptReply {
    fn respond(self, response: PromptResponse) -> Result<(), agent_client_protocol::Error> {
        match self {
            Self::Client(responder) => responder.respond(response),
            Self::Scheduled(reply) => reply.finished(response),
        }
    }

    fn respond_with_error(
        self,
        error: agent_client_protocol::Error,
    ) -> Result<(), agent_client_protocol::Error> {
        match self {
            Self::Client(responder) => responder.respond_with_error(error),
            Self::Scheduled(reply) => reply.failed(error),
        }
    }
}

fn respond_terminal(
    registration: &TurnRegistration,
    responder: PromptReply,
    reason: StopReason,
) -> Result<(), agent_client_protocol::Error> {
    let _ = registration.complete_and_settle();
    responder.respond(PromptResponse::new(reason))
}

fn respond_terminal_error(
    registration: &TurnRegistration,
    responder: PromptReply,
    error: String,
) -> Result<(), agent_client_protocol::Error> {
    registration.settle();
    responder.respond_with_error(agent_client_protocol::Error::new(-32603, error))
}

#[allow(clippy::too_many_arguments)]
async fn run_prompt(
    state: &AcpState,
    prompt_input: &crate::agent::input::PromptInput,
    command: Option<commands::Command>,
    session_id: SessionId,
    mut history: tokio::sync::OwnedMutexGuard<SessionHistory>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
    read_tracker: crate::agent::tools::ReadTracker,
    todo_store: crate::agent::tools::TodoStore,
    #[cfg(feature = "goal")] goal_store: crate::extras::goal::GoalStore,
    sandbox: crate::sandbox::Sandbox,
    #[cfg(feature = "js")] js_session_state: crate::extras::js::session::JsSessionStateOwner,
    #[cfg(feature = "skills")] skill_services: Arc<
        crate::extras::js::skills::session::SkillServiceOwner,
    >,
    responder: PromptReply,
    cx: ConnectionTo<Client>,
    control: Arc<TurnControl>,
    registration: &TurnRegistration,
) -> Result<(), agent_client_protocol::Error> {
    if let Some(command) = command.as_ref() {
        let mutates_native_context = match command {
            commands::Command::Undo | commands::Command::Compact | commands::Command::Rename(_) => {
                true
            }
            #[cfg(feature = "goal")]
            commands::Command::Goal(arguments) => !arguments.is_empty() && arguments != "status",
        };
        if crate::session::native_publication::is_native_bearing(&history.session)
            && mutates_native_context
        {
            return respond_terminal_error(
                registration,
                responder,
                "native session changes require a new ACP session".into(),
            );
        }
        let line = match command {
            commands::Command::Undo => {
                let removed = crate::ui::slash::undo_last(&mut history.session);
                Some(format!(
                    "Removed {removed} conversation records; files and billed usage remain unchanged."
                ))
            }
            commands::Command::Rename(name) => {
                history.session.name = name.as_str().into();
                Some(format!("Session renamed to {name}."))
            }
            #[cfg(feature = "goal")]
            commands::Command::Goal(arguments) => {
                if arguments.is_empty() || arguments == "status" {
                    Some(goal_store.snapshot().map_or_else(
                        || "No goal is set.".into(),
                        |goal| format!("Goal ({}): {}", goal.status.label(), goal.objective),
                    ))
                } else if arguments == "clear" {
                    goal_store.clear();
                    Some("Goal cleared.".into())
                } else {
                    let goal = match crate::extras::goal::Goal::configured(
                        arguments.clone(),
                        Vec::new(),
                        crate::extras::goal::GoalDefaults {
                            cfg: &state.cfg,
                            provider: &history.session.provider,
                            model: &history.session.model,
                        },
                    ) {
                        Ok(goal) => goal,
                        Err(_) => {
                            return respond_terminal_error(
                                registration,
                                responder,
                                "invalid ACP goal".into(),
                            );
                        }
                    };
                    if goal_store.set(goal, false).is_err() {
                        return respond_terminal_error(
                            registration,
                            responder,
                            "an unfinished ACP goal already exists".into(),
                        );
                    }
                    Some("Goal created.".into())
                }
            }
            commands::Command::Compact => {
                history.manual_compact = true;
                None
            }
        };
        if let Some(line) = line {
            if !registration.control.complete() {
                return respond_terminal(registration, responder, StopReason::Cancelled);
            }
            if !state.cli.no_session && sessions::checkpoint(&history).await.is_err() {
                return respond_terminal_error(
                    registration,
                    responder,
                    "ACP command checkpoint failed; observed progress remains in memory".into(),
                );
            }
            cx.send_notification(SessionNotification::new(
                session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(line),
                ))),
            ))?;
            return respond_terminal(registration, responder, StopReason::EndTurn);
        }
    }
    #[cfg(all(not(test), feature = "mcp"))]
    let mut mcp_manager: Option<PromptMcp> = None;
    #[cfg(all(test, feature = "mcp"))]
    let mut mcp_manager: Option<PromptMcp> =
        lock_unpoisoned(&state.mcp_fixture)
            .take()
            .map(|manager| PromptMcp {
                manager,
                scope: crate::agent::runner::AgentWorkScope::new(),
                key: None,
            });
    #[cfg(feature = "goal")]
    let goal_store_for_gate = goal_store.clone();
    #[cfg(feature = "goal")]
    let sandbox_for_gate = sandbox.clone();
    #[cfg(feature = "goal")]
    let todo_snapshot = todo_store.snapshot();
    // A goal's time bound measures how long the agent worked, so the turn's
    // wall clock is taken here and whatever the user spent on a permission
    // prompt is subtracted from it.
    #[cfg(feature = "goal")]
    let blocked_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    #[cfg(feature = "goal")]
    let round_started = std::time::Instant::now();
    let run = crate::spend::run::Run::current();
    let usage_before = run
        .as_ref()
        .map_or_else(Default::default, |run| run.accepted_usage());
    let pricing = crate::print::HeadlessPricing::from_session(&history.session, &state.cfg);
    history.pending_question = None;
    let mut native_owner = None;
    let result = crate::startup::heap_future(|| {
        execute_prompt(
            state,
            prompt_input,
            matches!(command, Some(commands::Command::Compact)),
            session_id.clone(),
            &mut history,
            &mut native_owner,
            workspace,
            context,
            read_tracker,
            todo_store.clone(),
            #[cfg(feature = "goal")]
            goal_store,
            sandbox,
            #[cfg(feature = "js")]
            js_session_state,
            #[cfg(feature = "skills")]
            skill_services,
            cx.clone(),
            control,
            #[cfg(feature = "mcp")]
            &mut mcp_manager,
            #[cfg(feature = "goal")]
            blocked_ms.clone(),
        )
    })
    .await;
    // Keep the history lock and generation registered until every server this
    // prompt does not hand back to the session is closed, including
    // cancellation during agent/runner preparation. Only a prompt that ended
    // cleanly leaves its servers connected for the next one.
    #[cfg(feature = "mcp")]
    if let Some(mcp) = mcp_manager {
        let cancelled = registration.control.is_cancelled();
        let clean = !cancelled
            && matches!(
                &result,
                Ok(outcome) if outcome.reason == StopReason::EndTurn && outcome.error.is_none()
            );
        if clean {
            history.reuse.mcp = Some(mcp);
        } else {
            mcp.close(cancelled).await;
        }
    }
    let mut outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => return respond_terminal_error(registration, responder, error.to_string()),
    };
    if !registration.control.complete() {
        outcome.reason = StopReason::Cancelled;
        outcome.error = None;
        outcome.question = None;
    }
    // Settle the goal round this prompt turn was, before the transcript is
    // committed, so the gate sees the round's own record.
    #[cfg(feature = "goal")]
    let goal_line = if outcome.question.is_some() {
        goal_store_for_gate
            .snapshot()
            .filter(|goal| goal.status.is_running())
            .and_then(|goal| {
                let open_todos = todo_snapshot
                    .iter()
                    .filter(|todo| !matches!(todo.status.as_str(), "completed" | "cancelled"))
                    .count();
                let cost = crate::extras::goal::driver::RoundCost {
                    usage: outcome.usage,
                    active: round_started.elapsed().saturating_sub(
                        std::time::Duration::from_millis(
                            blocked_ms.load(std::sync::atomic::Ordering::Relaxed),
                        ),
                    ),
                };
                let summary = crate::extras::goal::driver::summary_from_headless_turn(
                    &goal,
                    outcome.progress.as_deref().unwrap_or(&[]),
                    &cost,
                    None,
                    open_todos,
                    false,
                );
                match crate::extras::goal::driver::park_for_question(&goal_store_for_gate, summary)
                {
                    crate::extras::goal::driver::RoundOutcome::Stopped { line, .. } => {
                        goal_store_for_gate.snapshot().map(|goal| (line, goal))
                    }
                    _ => None,
                }
            })
    } else {
        settle_acp_goal_round(
            &goal_store_for_gate,
            outcome.reason,
            outcome.error.as_deref(),
            outcome.progress.as_deref().unwrap_or(&[]),
            &todo_snapshot,
            state,
            &sandbox_for_gate,
            &crate::extras::goal::driver::RoundCost {
                usage: outcome.usage,
                // What the agent worked for, excluding whatever the user spent
                // deciding on a permission prompt.
                active: round_started
                    .elapsed()
                    .saturating_sub(std::time::Duration::from_millis(
                        blocked_ms.load(std::sync::atomic::Ordering::Relaxed),
                    )),
            },
        )
        .await
    };

    history.pending_question = outcome.question.clone();
    history.session.pending_question = outcome.question.clone();
    history.session.todos = todo_store;
    #[cfg(feature = "goal")]
    {
        history.session.goal_store = goal_store_for_gate;
    }
    let usage = run.as_ref().map_or_else(Default::default, |run| {
        run.accepted_usage().nonnegative_difference(usage_before)
    });
    let response = outcome
        .progress
        .as_ref()
        .and_then(|progress| progress.last())
        .and_then(|message| match message {
            Message::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|item| match item {
                        rig::message::AssistantContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    if let Some(owner) = native_owner.as_mut() {
        use crate::session::native_replay::RetiredNativeOutcome;
        owner.outcome = if outcome.reason == StopReason::EndTurn
            && outcome.error.is_none()
            && outcome.question.is_none()
        {
            RetiredNativeOutcome::Completed
        } else if outcome.question.is_some() {
            RetiredNativeOutcome::LiveToolReply
        } else {
            RetiredNativeOutcome::Incomplete
        };
        if let Some(binding) = &owner.binding
            && let Err(reason) = binding.commit_canonical_input_into_with_outcome(
                &mut history.session,
                prompt_input,
                &response,
                outcome.progress.as_deref().unwrap_or(&[]),
                usage,
                pricing,
                owner.outcome,
            )
        {
            outcome.error = Some(reason.to_string());
        }
    } else {
        if let Some(progress) = outcome.progress.as_ref()
            && (outcome.reason == StopReason::EndTurn || !progress.is_empty())
            && crate::print::persist_headless_turn_input(
                &mut history.session,
                prompt_input,
                &response,
                progress,
            )
            .is_err()
        {
            outcome.error = Some("canonical ACP user content could not be retained".into());
        }
        history.session.charge_usage_delta_at(
            usage,
            pricing.anthropic_native,
            pricing.input_token_cost,
            pricing.output_token_cost,
            pricing.cache_prices,
        );
    }
    // Auxiliary requests belong to the same canonical financial checkpoint;
    // their separately captured receipt prices are never re-estimated here.
    history.session.total_cost = history
        .session
        .total_cost
        .max(history.session.spend_account.snapshot().recorded_spend());
    if let Some(mut owner) = native_owner {
        if let Err(error) = owner.finish(&mut history.session).await {
            outcome.error = Some(error.to_string());
        }
    } else if !state.cli.no_session
        && let Err(error) = sessions::checkpoint(&history).await
    {
        outcome.error = Some(error.to_string());
    }

    // Report the decision twice: as a thought chunk a person can read, and as
    // `_meta` a client can act on without parsing prose. Clients routinely hide
    // thoughts, and an agent that kept working with no visible reason is the
    // complaint this exists to prevent.
    #[cfg(feature = "goal")]
    if let Some((line, goal)) = goal_line {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "goal".to_string(),
            serde_json::json!({
                "id": goal.id.as_str(),
                "status": goal.status.label(),
                "round": goal.progress.rounds,
                "max_rounds": goal.bounds.max_rounds,
                "reason": line,
                // What the verdict actually rests on. A client that shows a
                // completion has to be able to tell one a command proved from
                // one the model asserted, without reading the prose.
                "verified": goal
                    .last_verdict
                    .as_ref()
                    .is_some_and(crate::extras::goal::Verdict::externally_verified),
                "evidence": goal
                    .last_verdict
                    .as_ref()
                    .map(|verdict| verdict.evidence_labels())
                    .unwrap_or_default(),
            }),
        );
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(line.clone())));
        let mut notification =
            SessionNotification::new(session_id.clone(), SessionUpdate::AgentThoughtChunk(chunk));
        notification.meta = Some(meta);
        let _ = cx.send_notification(notification);
    }
    // An internal failure (provider, runner, workspace or configuration) is
    // not a model refusal: it is answered as a JSON-RPC error so a client can
    // tell the two apart. Progress the turn made is still committed above.
    if let Some(error) = outcome.error {
        let _ = respond_terminal_error(registration, responder, error);
    } else if let Some(question) = outcome.question {
        // ACP options carry answers as data. A free-text answer comes as the
        // next ordinary prompt, after this exact reply/turn is retired.
        let mut response = PromptResponse::new(StopReason::EndTurn);
        response.meta = Some(serde_json::Map::from_iter([
            ("stop_reason".to_owned(), serde_json::json!("awaiting_user")),
            (
                "pending_question".to_owned(),
                serde_json::to_value(question).map_err(|_| {
                    agent_client_protocol::Error::new(-32603, "Could not encode bounded question")
                })?,
            ),
        ]));
        registration.settle();
        let _ = responder.respond(response);
    } else {
        let _ = respond_terminal(registration, responder, outcome.reason);
    }
    Ok(())
}

/// Judge the goal round this ACP prompt turn was.
///
/// ACP runs one round per `session/prompt`: the client drives the next one by
/// sending another prompt, which is how an editor already works. Relaunching
/// inside a single turn would mean re-entering the runner while the turn's
/// cancellation ownership and partial-transcript retention are live, and those
/// invariants are load-bearing.
///
/// Returns the line to report and the goal it describes.
#[cfg(feature = "goal")]
#[allow(clippy::too_many_arguments)]
async fn settle_acp_goal_round(
    store: &crate::extras::goal::GoalStore,
    reason: StopReason,
    error: Option<&str>,
    interactions: &[Message],
    todos: &[crate::agent::tools::todo::TodoItem],
    state: &AcpState,
    sandbox: &crate::sandbox::Sandbox,
    cost: &crate::extras::goal::driver::RoundCost,
) -> Option<(String, crate::extras::goal::Goal)> {
    let cfg = &state.cfg;
    use crate::extras::goal::driver::{self, RoundOutcome};
    use crate::extras::goal::gate::RoundEnd;

    // Only a running goal is having rounds run for it; an ordinary prompt
    // while one is parked for the user is not a round of it.
    let goal = store.snapshot().filter(|g| g.status.is_running())?;
    let open_todos = todos
        .iter()
        .filter(|item| !matches!(item.status.as_str(), "completed" | "cancelled"))
        .count();
    let mut summary = driver::summary_from_headless_turn(
        &goal,
        interactions,
        cost,
        None,
        open_todos,
        cfg.verify_command
            .as_deref()
            .is_some_and(|command| !command.trim().is_empty()),
    );
    summary.end = match (error, reason) {
        (Some(error), _) => RoundEnd::Failed(error.to_string()),
        (None, StopReason::EndTurn) => RoundEnd::Done,
        (None, StopReason::Cancelled) => RoundEnd::Cancelled,
        (None, other) => RoundEnd::Failed(format!("{other:?}")),
    };

    // The same objective is gated the same way in an editor as in a terminal:
    // both tiers run here. A claim no command proved is labelled as such in
    // the line and in `_meta`, wherever it is settled.
    let (main_provider, main_model) = acp_provider_and_model(&state.cli, cfg);
    let judge = crate::extras::goal::judge::resolve(&goal.judge, cfg, &main_provider, &main_model);
    let transcript = crate::extras::goal::judge::transcript_from_interactions(interactions);
    let outcome = driver::settle_round(store, summary, |request| async move {
        let checks = crate::extras::goal::checks::run(&goal, &request, sandbox, cfg).await;
        // A failing check already decides the claim, so the judge is not asked
        // for an answer the gate will not read.
        let checks_rejected = checks.as_ref().is_some_and(|o| !o.all_passed);
        let judged = match judge {
            Some(resolved) if request.run_judge && !checks_rejected => {
                match state
                    .provider_clients
                    .get(&state.cli, cfg, &resolved.provider)
                {
                    Ok(client) => Some(
                        crate::extras::goal::judge::ask_with_transcript_observed(
                            &goal,
                            &request,
                            &resolved,
                            &client,
                            &transcript,
                            cfg,
                            checks.as_ref(),
                            crate::spend::run::Run::current().map_or_else(
                                Default::default,
                                |run| {
                                    crate::spend::run::UsageObserver::with_run(
                                        run.for_judge(
                                            &resolved,
                                            cfg,
                                            resolved
                                                .same_provider_as_session
                                                .then(|| {
                                                    client.listing_key(&resolved.provider).ok()
                                                })
                                                .flatten()
                                                .as_ref(),
                                        ),
                                    )
                                },
                            ),
                        )
                        .await,
                    ),
                    // Fail open, like every other judge failure: an editor
                    // without a reachable judge still gets its checks.
                    Err(error) => Some(crate::extras::goal::judge::JudgeCall::unmetered(
                        crate::extras::goal::gate::JudgeOutcome::Unavailable {
                            reason: error.to_string(),
                        },
                    )),
                }
            }
            _ => None,
        };
        driver::Verification {
            checks,
            judge_tokens: judged.as_ref().map_or(0, |call| call.tokens),
            judge: judged.map(|call| call.outcome),
            ..driver::Verification::default()
        }
    })
    .await;
    let line = match outcome {
        RoundOutcome::Inactive => return None,
        RoundOutcome::Relaunch { line, .. } | RoundOutcome::Stopped { line, .. } => line,
    };
    store.snapshot().map(|goal| (line, goal))
}

struct PromptOutcome {
    /// Ignored when `error` is set: a failed turn is answered with a JSON-RPC
    /// error, never with a stop reason.
    reason: StopReason,
    question: Option<crate::question::QuestionRequired>,
    progress: Option<Vec<Message>>,
    /// An internal failure; the prompt request fails with this message.
    error: Option<String>,
    /// Tokens this turn spent. A goal's token bound is enforced from here, so
    /// leaving it at zero would make the bound configurable and inert.
    #[cfg(feature = "goal")]
    usage: rig::completion::Usage,
}

/// A summarizer result for `SessionHistory::compact_with` tests.
#[cfg(test)]
fn compaction_output(
    summary: impl Into<String>,
    messages_included: usize,
) -> crate::provider::CompactionOutput {
    crate::provider::CompactionOutput {
        summary_byte_budget: 16 * 1024,
        summary: summary.into(),
        messages_included,
        usage: crate::event::UsageDelta::default(),
    }
}

impl PromptOutcome {
    fn cancelled(progress: Option<Vec<Message>>) -> Self {
        Self {
            reason: StopReason::Cancelled,
            question: None,
            progress,
            error: None,
            #[cfg(feature = "goal")]
            usage: rig::completion::Usage::default(),
        }
    }

    fn failed(error: String) -> Self {
        Self {
            reason: StopReason::Refusal,
            question: None,
            progress: None,
            error: Some(error),
            #[cfg(feature = "goal")]
            usage: rig::completion::Usage::default(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_prompt(
    state: &AcpState,
    prompt_input: &crate::agent::input::PromptInput,
    compact_only: bool,
    session_id: SessionId,
    history: &mut SessionHistory,
    native_owner: &mut Option<crate::provider::NativeRunOwner>,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    context: Arc<ContextFiles>,
    read_tracker: crate::agent::tools::ReadTracker,
    todo_store: crate::agent::tools::TodoStore,
    #[cfg(feature = "goal")] goal_store: crate::extras::goal::GoalStore,
    sandbox: crate::sandbox::Sandbox,
    #[cfg(feature = "js")] js_session_state: crate::extras::js::session::JsSessionStateOwner,
    #[cfg(feature = "skills")] skill_services: Arc<
        crate::extras::js::skills::session::SkillServiceOwner,
    >,
    cx: ConnectionTo<Client>,
    control: Arc<TurnControl>,
    #[cfg(feature = "mcp")] mcp_manager: &mut Option<PromptMcp>,
    // Filled by the permission bridge with the time the user held this turn.
    #[cfg(feature = "goal")] blocked_ms: Arc<std::sync::atomic::AtomicU64>,
) -> Result<PromptOutcome, agent_client_protocol::Error> {
    let prompt_text = prompt_input.literal_text();
    if control.is_cancelled() {
        return Ok(PromptOutcome::cancelled(None));
    }
    if let Err(error) = workspace.validate() {
        return Ok(PromptOutcome::failed(error.to_string()));
    }
    if let Some(run) = crate::spend::run::Run::current()
        && run.admit().is_err()
    {
        return Ok(PromptOutcome::failed(crate::spend::run::BUDGET_STOP.into()));
    }
    // The relay reads the same session-local store the actual tool mutates.
    // Tool output and arguments are display data, never a second TODO source.
    let plan_store = todo_store.clone();
    // The provider this turn's usage is reported by, so a goal's token total
    // is normalised the same way as the TUI's and headless rounds'.
    #[cfg(feature = "goal")]
    let usage_provider = acp_provider_and_model(&state.cli, &state.cfg).0;
    #[cfg(test)]
    if let Some(fixture) = &state.runner_fixture {
        let prior_history = history
            .snapshot_with_tool_result_retention(state.cfg.resolve_keep_recent_tool_results());
        let paused_runner = tokio::select! {
            biased;
            _ = control.cancelled() => {
                return Ok(PromptOutcome::cancelled(None));
            }
            runner = fixture(prompt_text.to_owned(), prior_history.clone()) => runner,
        };
        return Ok(relay_paused_runner(
            session_id,
            cx,
            control,
            paused_runner,
            plan_store,
            #[cfg(feature = "goal")]
            (&state.cfg, &usage_provider),
        )
        .await);
    }

    #[cfg(test)]
    if let Some(fixture) = &state.prompt_fixture {
        let prior_history = history
            .snapshot_with_tool_result_retention(state.cfg.resolve_keep_recent_tool_results());
        let fixture_result = tokio::select! {
            biased;
            _ = control.cancelled() => {
                return Ok(PromptOutcome::cancelled(None));
            }
            result = fixture(prompt_text.to_owned(), prior_history) => result,
        };
        let events = match fixture_result {
            Ok(events) => events,
            Err(error) => {
                return Ok(PromptOutcome::failed(error));
            }
        };
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(events.len().max(1));
        for event in events {
            event_tx.send(event).await.map_err(|_| {
                agent_client_protocol::Error::new(-32603, "ACP fixture event channel closed")
            })?;
        }
        drop(event_tx);
        return Ok(relay_prompt_events(
            session_id,
            cx,
            control,
            event_rx,
            plan_store,
            #[cfg(feature = "goal")]
            (&state.cfg, &usage_provider),
        )
        .await);
    }

    let workspace_root = workspace.root();
    let mut authority =
        match crate::permission::resolve_configured_execution_authority(&state.cli, &state.cfg) {
            Ok(resolved) => resolved,
            Err(error) => {
                return Ok(PromptOutcome::failed(error.to_string()));
            }
        }
        .0;
    if let Some(mode) = history.policy.mode() {
        authority.mode = mode;
    }
    let (permission, ask_tx, ask_rx) = crate::permission::build_interactive_permission_with_cli_at(
        &state.cli,
        &state.cfg,
        authority,
        Some(workspace_root.to_path_buf()),
    )
    .map_err(|error| agent_client_protocol::Error::new(-32602, error.to_string()))?;
    history.policy.apply(&permission);
    if let Some(rx) = ask_rx {
        tokio::spawn(state.idle.admit()?.track(drive_permission_bridge(
            cx.clone(),
            idle::Activity::default(),
            session_id.clone(),
            rx,
            control.clone(),
            Some(BridgePolicy {
                policy: history.policy.clone(),
                permission: permission.clone(),
            }),
            #[cfg(feature = "goal")]
            blocked_ms,
        )));
    }
    let (provider_str, model_str) = acp_provider_and_model(&state.cli, &state.cfg);

    tracing::debug!(
        "ACP run_prompt: provider={}, model={}, prompt_len={}",
        provider_str,
        model_str,
        prompt_text.len(),
    );

    let client = match state
        .provider_clients
        .get(&state.cli, &state.cfg, &provider_str)
    {
        Ok(client) => client,
        Err(error) => {
            return Ok(PromptOutcome::failed(error.to_string()));
        }
    };

    let native = matches!(&client, crate::provider::AnyClient::Anthropic(_))
        || crate::session::native_publication::is_native_bearing(&history.session);
    let needs_compaction = history.needs_compaction();
    if native && needs_compaction {
        return Ok(PromptOutcome::failed(
            "native history cannot be compacted in place; start a new ACP session".into(),
        ));
    }
    let work_scope = crate::agent::runner::AgentWorkScope::new();
    #[cfg(feature = "hooks")]
    work_scope.set_hook_execution_root(workspace.root());
    if native {
        let ticket = if crate::session::native_publication::startup_requires_publication(
            &history.session,
            matches!(&client, crate::provider::AnyClient::Anthropic(_)),
            state.cli.no_session,
        )
        .map_err(|error| agent_client_protocol::Error::new(-32603, error.to_string()))?
        {
            Some(
                crate::session::native_publication::begin_owned(&mut history.session, &work_scope)
                    .await
                    .map_err(|error| {
                        agent_client_protocol::Error::new(-32603, error.to_string())
                    })?,
            )
        } else {
            None
        };
        *native_owner = Some(crate::provider::NativeRunOwner::with_scope(
            ticket,
            work_scope.clone(),
        ));
    }
    #[cfg(feature = "hooks")]
    let mut owned_startup_context = None;

    // Goal calibration uses the compaction's usage too. Financial receipts
    // belong to this ACP logical session and survive prompt cancellation.
    #[cfg(feature = "goal")]
    let mut compaction_usage = crate::event::UsageDelta::default();
    if needs_compaction {
        #[cfg(feature = "hooks")]
        crate::startup::heap_future(|| crate::extras::hooks::dispatch_pre_compact(true, None))
            .await;
        let quick_models = crate::config::quick_models_map(&state.cfg);
        let context_window =
            state
                .cfg
                .resolve_context_window(&provider_str, &model_str, &quick_models);
        let reserve = state
            .cfg
            .resolve_reserve_tokens(&model_str, &quick_models, context_window);
        let compaction_client = &client;
        #[cfg(feature = "goal")]
        let goal_store_for_compaction = goal_store.clone();
        let compaction_model = &model_str;
        let compaction_retry = &state.cfg.retry;
        let observed = crate::spend::run::Run::current().map_or_else(Default::default, |run| {
            crate::spend::run::UsageObserver::with_run(run.with_captured_profile(
                &provider_str,
                &model_str,
                run.prices,
                &state.cfg,
                crate::spend::RequestKind::Compaction,
            ))
        });
        let compacted = history
            .compact_with_known(
                needs_compaction,
                &control,
                #[cfg(feature = "goal")]
                Some(&goal_store_for_compaction),
                move |messages, previous_summary| {
                    let observed = observed.clone();
                    let provider_str = provider_str.clone();
                    async move {
                        let mut output = compaction_client
                            .compress_messages_observed(
                                compaction_model,
                                &messages,
                                previous_summary.as_deref(),
                                None,
                                context_window.saturating_sub(reserve),
                                reserve,
                                compaction_retry,
                                observed,
                            )
                            .await?;
                        output.usage = state.cfg.normalize_usage(&provider_str, output.usage);
                        Ok(output)
                    }
                },
            )
            .await;
        let Some(usage) = compacted else {
            return Ok(PromptOutcome::cancelled(None));
        };
        #[cfg(feature = "hooks")]
        {
            let notices = crate::startup::heap_future(|| {
                crate::extras::hooks::dispatch_session_start("compact")
            })
            .await;
            history.record_hook_context(notices.into_notices());
        }
        #[cfg(feature = "goal")]
        {
            compaction_usage = usage;
        }
        #[cfg(not(feature = "goal"))]
        let _ = usage;
    }
    if compact_only {
        return Ok(PromptOutcome {
            reason: StopReason::EndTurn,
            question: None,
            progress: None,
            error: None,
            #[cfg(feature = "goal")]
            usage: compaction_usage.into(),
        });
    }
    // Refreshed after compaction, so the recap this turn replays is the one
    // whose daily-memory copy is left out of the block.
    #[cfg(feature = "memory")]
    let context = {
        let mut refreshed = (*context).clone();
        refreshed
            .refresh_memory_if_changed(history.active_compaction_ref())
            .await;
        Arc::new(refreshed)
    };
    #[cfg(not(feature = "hooks"))]
    let prior_history =
        history.snapshot_with_tool_result_retention(state.cfg.resolve_keep_recent_tool_results());

    let model = client.completion_model_with_cache_ttl(
        model_str.to_string(),
        state.cfg.cache_ttl.unwrap_or_default(),
    );

    let temperature = crate::config::resolve_temperature(&state.cli, &state.cfg, &model_str);
    let extra_body = crate::config::resolve_extra_body(&state.cfg, &model_str);
    #[cfg(feature = "hooks")]
    let prior_history = {
        if !history.hooks_started {
            if let Some(dispatcher) = crate::extras::hooks::get_dispatcher() {
                let hook_ctx = crate::extras::hooks::HookCtx {
                    session_id: session_id.to_string(),
                    session_path: String::new(),
                    cwd: workspace.root().to_string_lossy().into_owned(),
                    permission_mode: permission
                        .as_ref()
                        .map(|permission| {
                            permission
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .mode()
                                .to_string()
                        })
                        .unwrap_or_else(|| "standard".into()),
                };
                let Some(notices) = run_owned_pre_run(
                    &control,
                    &work_scope,
                    crate::extras::hooks::dispatch_session_start_with(
                        &dispatcher,
                        &hook_ctx,
                        "startup",
                    ),
                )
                .await
                else {
                    return Ok(PromptOutcome::cancelled(None));
                };
                if native {
                    owned_startup_context = Some(notices);
                } else {
                    history.record_hook_context(notices.into_notices());
                }
            }
            history.hooks_started = true;
        }
        history.snapshot_with_tool_result_retention(state.cfg.resolve_keep_recent_tool_results())
    };
    #[cfg(feature = "mcp")]
    if state.cli.mcp_is_eligible(&state.cfg) {
        let client_servers = state
            .sessions
            .lock()
            .await
            .get(&session_id)
            .map(|session| session.client_mcp_servers.clone())
            .unwrap_or_default();
        let mcp_cfg = with_client_mcp_servers(&state.cfg, &client_servers);
        let key = McpKey::new(&mcp_cfg, &workspace);
        // A manager this prompt did not connect stays at the session's
        // disposal until the prompt finishes (`run_prompt` hands it back).
        match history.reuse.mcp.take() {
            Some(previous) if previous.is_reusable(&key) => *mcp_manager = Some(previous),
            stale => {
                if let Some(stale) = stale {
                    stale.close(false).await;
                }
                // The servers belong to the session, not to this prompt: they
                // connect under their own scope so a later prompt's cancellation
                // cannot reach connections it did not start.
                let scope = crate::agent::runner::AgentWorkScope::new();
                #[cfg(feature = "hooks")]
                scope.set_hook_execution_root(workspace.root());
                let Some(connected) =
                    connect_prompt_mcp(&mcp_cfg, &workspace, &scope, control.cancelled()).await
                else {
                    return Ok(PromptOutcome::cancelled(None));
                };
                *mcp_manager = connected.map(|manager| PromptMcp {
                    manager,
                    scope,
                    key: Some(key),
                });
            }
        }
    }
    let native_js_context = history.session.native_js_context();
    let tool_output_session_id = session_id.to_string();
    let build = crate::provider::build_agent_in_workspace(
        model,
        &state.cli,
        &state.cfg,
        &context,
        workspace,
        permission,
        ask_tx,
        sandbox,
        read_tracker,
        todo_store,
        #[cfg(feature = "goal")]
        goal_store,
        &tool_output_session_id,
        None,
        native_js_context,
        false,
        temperature,
        extra_body,
        #[cfg(feature = "js")]
        js_session_state,
        #[cfg(feature = "skills")]
        skill_services,
        #[cfg(feature = "mcp")]
        mcp_manager.as_ref().map(|mcp| &mcp.manager),
    );
    // Language servers outlive the turn: the builder reuses the session's
    // manager instead of re-scanning PATH and restarting every server.
    #[cfg(feature = "lsp")]
    let build = {
        let lsp_session = history.reuse.lsp.clone();
        async move { lsp_session.scope(build).await }
    };
    let Some(agent) = run_owned_pre_run(&control, &work_scope, build).await else {
        return Ok(PromptOutcome::cancelled(None));
    };

    let file_checkpoints = history.session.file_checkpoints.recorder(
        &history.session.id,
        history.session.messages.len(),
        state.cli.no_session,
    );
    let agent = agent
        .with_main_session(&state.cfg, &history.session)
        .with_file_checkpoints(file_checkpoints.clone())
        .with_spend(crate::spend::run::Run::current())
        .with_question_turn(cron::question_origin().capture(None));
    let (agent, prior_history) = if let Some(owner) = native_owner.as_mut() {
        use crate::session::native_replay::ReplayBoundary;
        let boundary = match history.session.native_history_snapshot() {
            Some(saved) => saved
                .saved_boundary()
                .map_err(|reason| agent_client_protocol::Error::new(-32603, reason.to_string()))?,
            None => ReplayBoundary::CompletedTurn,
        };
        #[cfg_attr(not(feature = "hooks"), allow(unused_mut))]
        let (agent, mut binding) = agent
            .bind_native_turn(
                &history.session,
                prior_history.into(),
                boundary,
                crate::provider::RecordedToolIds::Canonical,
            )
            .map_err(|reason| agent_client_protocol::Error::new(-32603, reason.to_string()))?;
        #[cfg(feature = "hooks")]
        if let Some(context) = owned_startup_context.take() {
            binding
                .append_owned_startup_context(&mut history.session, context)
                .map_err(|reason| agent_client_protocol::Error::new(-32603, reason.to_string()))?;
            file_checkpoints.rebase_turn(history.session.messages.len());
        }
        owner
            .progress(&mut history.session)
            .await
            .map_err(|error| agent_client_protocol::Error::new(-32603, error.to_string()))?;
        if let Some(ticket) = &mut owner.ticket {
            binding
                .pair_publication(&history.session, ticket)
                .map_err(|error| agent_client_protocol::Error::new(-32603, error.to_string()))?;
        }
        let prior_history = binding.history();
        owner.binding = Some(binding);
        (agent, prior_history)
    } else {
        (agent, prior_history.into())
    };
    let Some(paused_runner) = run_owned_pre_run(
        &control,
        &work_scope,
        agent.spawn_runner_paused_input_in_scope(
            prompt_input.clone(),
            prior_history,
            state.cfg.retry.clone(),
            #[cfg(feature = "hooks")]
            None, // ACP is not loop mode; global lifecycle hooks remain active.
            Arc::clone(&work_scope),
        ),
    )
    .await
    else {
        return Ok(PromptOutcome::cancelled(None));
    };
    #[cfg_attr(not(feature = "goal"), allow(unused_mut))]
    let mut outcome = relay_paused_runner(
        session_id,
        cx,
        control,
        paused_runner,
        plan_store,
        #[cfg(feature = "goal")]
        (&state.cfg, &usage_provider),
    )
    .await;
    // The compaction ran on this round's behalf, so its tokens count toward
    // the goal's total and `max_tokens` bound like the agent's own
    // (mini-agent-i6q98).
    #[cfg(feature = "goal")]
    add_goal_usage(
        &mut outcome.usage,
        &state.cfg,
        &usage_provider,
        compaction_usage,
    );
    Ok(outcome)
}

#[cfg(feature = "mcp")]
pub(crate) async fn connect_prompt_mcp(
    cfg: &Config,
    workspace: &Arc<crate::paths::WorkspaceBinding>,
    work_scope: &Arc<crate::agent::runner::AgentWorkScope>,
    cancelled: impl Future<Output = ()>,
) -> Option<Option<crate::extras::mcp::McpClientManager>> {
    let connecting = work_scope.run(crate::startup::connect_headless_mcp(cfg, workspace));
    tokio::pin!(connecting);
    tokio::select! {
        biased;
        _ = cancelled => {
            work_scope.cancellation_handle().cancel();
            // Initializing clients observe the scope and await their process
            // cleanup. Successfully connected siblings still need explicit close.
            if let Some(manager) = connecting.await { manager.shutdown().await; }
            work_scope.wait_idle().await;
            None
        }
        manager = &mut connecting => Some(manager),
    }
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
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    control: Arc<TurnControl>,
    paused_runner: crate::agent::runner::PausedAgentRunner,
    plan_store: crate::agent::tools::TodoStore,
    #[cfg(feature = "goal")] usage_provider: (&Config, &str),
) -> PromptOutcome {
    #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
    let mut paused_runner = paused_runner;
    #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
    let notices = paused_runner.take_hook_context();
    let attached = control.attach_runner(paused_runner.cancellation_handle());
    let mut runner = paused_runner.start();
    if !attached {
        let mut last_plan = Vec::new();
        return cancelled_after_runner_with_plan(
            &mut runner.event_rx,
            &session_id,
            &cx,
            &plan_store,
            &mut last_plan,
        )
        .await;
    }
    let outcome = relay_prompt_events(
        session_id,
        cx,
        control,
        runner.event_rx,
        plan_store,
        #[cfg(feature = "goal")]
        usage_provider,
    )
    .await;
    #[cfg(any(feature = "hooks", feature = "memory", feature = "subagents"))]
    let outcome = {
        let mut outcome = outcome;
        if !notices.is_empty() {
            let progress = outcome.progress.get_or_insert_with(Vec::new);
            progress.splice(0..0, notices.into_iter().map(Message::system));
        }
        outcome
    };
    outcome
}

/// Add one usage report to an ACP goal round's token total, normalised for
/// `provider` exactly once (Gemini reports thinking tokens beside output), so
/// the goal's `max_tokens` bound counts what the TUI and headless rounds count
/// (mini-agent-da4my).
#[cfg(feature = "goal")]
fn add_goal_usage(
    total: &mut rig::completion::Usage,
    cfg: &Config,
    provider: &str,
    delta: crate::event::UsageDelta,
) {
    let delta = cfg.normalize_usage(provider, delta);
    total.input_tokens = total.input_tokens.saturating_add(delta.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(delta.output_tokens);
    total.total_tokens = total
        .total_tokens
        .saturating_add(delta.input_tokens.saturating_add(delta.output_tokens));
}

/// Publish only the actual session store. A denied or failed `todo_write`
/// leaves it unchanged, and a successful empty replacement clears the plan.
fn relay_plan_if_changed(
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
    store: &crate::agent::tools::TodoStore,
    previous: &mut Vec<crate::agent::tools::todo::TodoItem>,
) {
    let current = store.snapshot();
    if current == *previous {
        return;
    }
    let entries = current
        .iter()
        .take(crate::agent::tools::todo::MAX_TODO_ITEMS)
        .map(|item| {
            let priority = match item.priority.as_str() {
                "high" => PlanEntryPriority::High,
                "low" => PlanEntryPriority::Low,
                _ => PlanEntryPriority::Medium,
            };
            let status = match item.status.as_str() {
                "in_progress" => PlanEntryStatus::InProgress,
                "completed" => PlanEntryStatus::Completed,
                _ => PlanEntryStatus::Pending,
            };
            let mut entry = PlanEntry::new(
                item.content
                    .chars()
                    .take(crate::agent::tools::todo::MAX_TODO_CONTENT_CHARS)
                    .collect::<String>(),
                priority,
                status,
            );
            // ACP v1 has no cancelled status. Preserve the actual status as
            // metadata rather than presenting abandoned work as completed.
            if item.status == "cancelled" {
                entry.meta = Some(serde_json::Map::from_iter([(
                    "mini-agent-status".to_owned(),
                    serde_json::json!("cancelled"),
                )]));
            }
            entry
        })
        .collect();
    let notification =
        SessionNotification::new(session_id.clone(), SessionUpdate::Plan(Plan::new(entries)));
    match cx.send_notification(notification) {
        Ok(()) => *previous = current,
        Err(error) => tracing::warn!(%error, "ACP failed to send plan notification"),
    }
}

/// Streamed deltas arrive a few characters at a time; one `session/update`
/// per delta costs a JSON-RPC frame and a pipe write each.
const TEXT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(32);
const TEXT_FLUSH_BYTES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Message,
    Thought,
}

/// Consecutive same-kind text deltas, flushed as one chunk after
/// [`TEXT_FLUSH_INTERVAL`], once [`TEXT_FLUSH_BYTES`] accumulate, when the
/// kind changes, or before any other update (so ordering is unchanged).
#[derive(Default)]
struct PendingText {
    buffer: Option<(TextKind, String, tokio::time::Instant)>,
}

impl PendingText {
    fn is_pending(&self) -> bool {
        self.buffer.is_some()
    }

    async fn deadline_elapsed(&self) {
        if let Some((_, _, started)) = &self.buffer {
            tokio::time::sleep_until(*started + TEXT_FLUSH_INTERVAL).await;
        }
    }

    fn push(
        &mut self,
        kind: TextKind,
        text: &str,
        session_id: &SessionId,
        cx: &ConnectionTo<Client>,
    ) {
        if self
            .buffer
            .as_ref()
            .is_some_and(|(pending, _, _)| *pending != kind)
        {
            self.flush(session_id, cx);
        }
        let (_, buffer, _) = self
            .buffer
            .get_or_insert_with(|| (kind, String::new(), tokio::time::Instant::now()));
        buffer.push_str(text);
        if buffer.len() >= TEXT_FLUSH_BYTES {
            self.flush(session_id, cx);
        }
    }

    fn flush(&mut self, session_id: &SessionId, cx: &ConnectionTo<Client>) {
        let Some((kind, text, _)) = self.buffer.take() else {
            return;
        };
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
        let update = match kind {
            TextKind::Message => SessionUpdate::AgentMessageChunk(chunk),
            TextKind::Thought => SessionUpdate::AgentThoughtChunk(chunk),
        };
        if let Err(error) =
            cx.send_notification(SessionNotification::new(session_id.clone(), update))
        {
            tracing::warn!("ACP failed to send text notification: {}", error);
        }
    }
}

async fn relay_prompt_events(
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    control: Arc<TurnControl>,
    mut rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    plan_store: crate::agent::tools::TodoStore,
    #[cfg(feature = "goal")] usage_provider: (&Config, &str),
) -> PromptOutcome {
    let mut last_plan = Vec::new();
    relay_plan_if_changed(&session_id, &cx, &plan_store, &mut last_plan);
    #[cfg(feature = "goal")]
    let mut usage = rig::completion::Usage::default();
    let mut pending_text = PendingText::default();
    loop {
        let event = tokio::select! {
            biased;
            _ = control.cancelled() => {
                pending_text.flush(&session_id, &cx);
                return cancelled_after_runner_with_plan(
                    &mut rx, &session_id, &cx, &plan_store, &mut last_plan,
                ).await;
            }
            () = pending_text.deadline_elapsed(), if pending_text.is_pending() => {
                pending_text.flush(&session_id, &cx);
                continue;
            }
            event = rx.recv() => event,
        };
        // Streamed text is coalesced; anything else (tool call, result,
        // terminal event, ...) is ordered after every delta that preceded it.
        if !matches!(event, Some(AgentEvent::Token(_) | AgentEvent::Reasoning(_))) {
            pending_text.flush(&session_id, &cx);
        }
        let Some(event) = event else {
            return PromptOutcome {
                reason: StopReason::Refusal,
                question: None,
                progress: None,
                #[cfg(feature = "goal")]
                usage: rig::completion::Usage::default(),
                error: Some("agent runner stopped without a terminal event".to_string()),
            };
        };
        match event {
            AgentEvent::Token(text) => {
                pending_text.push(TextKind::Message, &text, &session_id, &cx);
            }
            AgentEvent::Reasoning(text) => {
                pending_text.push(TextKind::Thought, &text, &session_id, &cx);
            }
            AgentEvent::ToolCall { id, name, args } => {
                let id = ToolCallId::new(id.to_string());
                let tool_call = ToolCall::new(id.clone(), name.to_string()).raw_input(Some(args));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCall(tool_call),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send tool call notification: {}", e);
                }
            }
            #[cfg(feature = "subagents")]
            AgentEvent::SubagentToolCall { .. } => {
                // This is a display-only event from inside a task tool. The outer
                // provider tool call/result is canonical and will arrive with a
                // stable ID; advertising a nested call here would create an ACP
                // call that can never receive a matching result.
            }
            #[cfg(feature = "subagents")]
            AgentEvent::SubagentStarted { .. } => {
                // Display-only provenance; the outer task call remains the
                // canonical ACP tool lifecycle.
            }
            AgentEvent::ToolProgress { .. } => {
                // Display-only live output preview for the TUI. The tool
                // result update below remains the canonical ACP content.
            }
            AgentEvent::FileMutation { id, name, diff } => {
                // A distinct historical preview survives the subsequent tool
                // result update; it never asserts current filesystem contents.
                if diff.bounded() {
                    let text = crate::ui::events::sanitize_for_review(&format!(
                        "Recorded {name} mutation preview (call {id}):\n{}",
                        diff.review_text()
                    ));
                    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                    let notification = SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentThoughtChunk(chunk),
                    );
                    if let Err(error) = cx.send_notification(notification) {
                        tracing::warn!(%error,"ACP failed to send mutation preview");
                    }
                }
            }
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
                ..
            } => {
                let id = id.to_string();
                let mut fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(output.to_string()),
                    ))]);
                if is_error {
                    fields.status = Some(ToolCallStatus::Failed);
                }
                let update = ToolCallUpdate::new(ToolCallId::new(id), fields);
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::ToolCallUpdate(update),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send tool result notification: {}", e);
                }
                relay_plan_if_changed(&session_id, &cx, &plan_store, &mut last_plan);
            }
            AgentEvent::ToolLoop { name, message } => {
                let text = format!("loop guard ({name}): {message}");
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(chunk),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send tool-loop notification: {}", e);
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
            AgentEvent::Verification {
                attempt,
                max,
                passed,
                output,
            } => {
                let status = if passed { "passed" } else { "failed" };
                let text = if passed {
                    format!("verification {status} ({attempt}/{max})")
                } else {
                    format!("verification {status} ({attempt}/{max})\n{output}")
                };
                let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
                let notif = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentThoughtChunk(chunk),
                );
                if let Err(e) = cx.send_notification(notif) {
                    tracing::warn!("ACP failed to send verification notification: {}", e);
                }
            }
            AgentEvent::UsageDelta { usage: delta, .. } => {
                // ACP has no status bar to update, but a goal's token bound is
                // enforced from this total, so it is accumulated rather than
                // dropped.
                #[cfg(feature = "goal")]
                add_goal_usage(&mut usage, usage_provider.0, usage_provider.1, delta);
                #[cfg(not(feature = "goal"))]
                let _ = delta;
            }
            AgentEvent::SteerConsumed { .. }
            | AgentEvent::SteerRejected { .. }
            | AgentEvent::CompactionBoundary { .. } => {
                tracing::error!("interactive-only compaction boundary reached ACP");
                control.cancel();
                return cancelled_after_runner_with_plan(
                    &mut rx,
                    &session_id,
                    &cx,
                    &plan_store,
                    &mut last_plan,
                )
                .await;
            }
            AgentEvent::AwaitingUser {
                question,
                interactions,
            } => {
                // Closing this receiver waits for the exact run and its owned
                // effects. A future ordinary user prompt gets a fresh slot.
                while rx.recv().await.is_some() {}
                relay_plan_if_changed(&session_id, &cx, &plan_store, &mut last_plan);
                return PromptOutcome {
                    reason: StopReason::EndTurn,
                    question: Some(*question),
                    progress: Some(interactions),
                    error: None,
                    #[cfg(feature = "goal")]
                    usage,
                };
            }
            AgentEvent::Done { interactions, .. } => {
                while rx.recv().await.is_some() {}
                relay_plan_if_changed(&session_id, &cx, &plan_store, &mut last_plan);
                return PromptOutcome {
                    reason: StopReason::EndTurn,
                    question: None,
                    progress: Some(interactions),
                    error: None,
                    #[cfg(feature = "goal")]
                    usage,
                };
            }
            AgentEvent::Error {
                message,
                interactions,
            } => {
                while rx.recv().await.is_some() {}
                relay_plan_if_changed(&session_id, &cx, &plan_store, &mut last_plan);
                return PromptOutcome {
                    reason: StopReason::Refusal,
                    question: None,
                    progress: Some(interactions),
                    error: Some(message.to_string()),
                    #[cfg(feature = "goal")]
                    usage,
                };
            }
        }
    }
}

/// Cancellation can win after a tool mutates its store but before the relay
/// consumes its result event. Publish that actual final state after draining.
async fn cancelled_after_runner_with_plan(
    rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
    store: &crate::agent::tools::TodoStore,
    previous: &mut Vec<crate::agent::tools::todo::TodoItem>,
) -> PromptOutcome {
    let outcome = cancelled_after_runner(rx).await;
    relay_plan_if_changed(session_id, cx, store, previous);
    outcome
}

async fn cancelled_after_runner(rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>) -> PromptOutcome {
    // Channel closure follows the model/tool future and all owned work. Retain
    // any terminal progress that beat cancellation so effects aren't repeated.
    let mut progress = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Done { interactions, .. }
            | AgentEvent::Error { interactions, .. }
            | AgentEvent::AwaitingUser { interactions, .. }
                if !interactions.is_empty() =>
            {
                progress = Some(interactions);
            }
            _ => {}
        }
    }
    PromptOutcome::cancelled(progress)
}

#[cfg(test)]
mod history_tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use rig::agent::AgentBuilder;
    use rig::completion::message::{ToolResultContent, UserContent};
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
            None,
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut events = runner.event_rx;
        while let Some(event) = events.recv().await {
            match event {
                AgentEvent::Done { interactions, .. } => return interactions,
                AgentEvent::Error { message: error, .. } => panic!("fake ACP turn failed: {error}"),
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
    fn acp_snapshot_clears_old_results_without_dropping_calls() {
        let mut history = SessionHistory::default();
        for index in 0..3 {
            let id = format!("call-{index}");
            history.commit_completed_turn(
                &format!("prompt-{index}"),
                vec![
                    Message::Assistant {
                        id: None,
                        content: rig::OneOrMany::one(AssistantContent::tool_call(
                            id.clone(),
                            "read",
                            serde_json::json!({"path": format!("file-{index}")}),
                        )),
                    },
                    Message::tool_result(id, format!("output-{index}")),
                ],
            );
        }

        let snapshot = history.snapshot_with_tool_result_retention(1);
        let calls = snapshot
            .iter()
            .map(|message| match message {
                Message::Assistant { content, .. } => content
                    .iter()
                    .filter(|item| matches!(item, AssistantContent::ToolCall(_)))
                    .count(),
                Message::User { .. } | Message::System { .. } => 0,
            })
            .sum::<usize>();
        let outputs = snapshot
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => content.iter().find_map(|item| match item {
                    UserContent::ToolResult(result) => match result.content.first() {
                        ToolResultContent::Text(text) => Some(text.text),
                        ToolResultContent::Image(_) => None,
                    },
                    _ => None,
                }),
                Message::Assistant { .. } | Message::System { .. } => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(calls, 3);
        assert_eq!(
            outputs,
            vec![
                crate::session::CLEARED_TOOL_RESULT_NOTICE.to_string(),
                crate::session::CLEARED_TOOL_RESULT_NOTICE.to_string(),
                "output-2".to_string(),
            ]
        );
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

    #[tokio::test]
    async fn history_retention_summarizes_whole_oldest_turns_at_both_bounds() {
        let mut by_count = SessionHistory::default();
        for index in 0..=MAX_ACP_HISTORY_TURNS {
            by_count.commit_completed_turn(
                &format!("user-{index}"),
                vec![Message::assistant(format!("assistant-{index}"))],
            );
        }
        assert!(by_count.needs_compaction());
        by_count
            .compact_with(
                &TurnControl::new(),
                #[cfg(feature = "goal")]
                None,
                |messages, _| async move {
                    let count = messages.len();
                    Ok(compaction_output("origin user-0 assistant-0", count))
                },
            )
            .await
            .unwrap();
        let count_snapshot = by_count.snapshot();
        assert_eq!(count_snapshot.len(), MAX_ACP_HISTORY_TURNS * 2 + 1);
        assert!(!count_snapshot.contains(&Message::user("user-0")));
        assert!(
            count_snapshot[0]
                == Message::assistant(
                    "[Recap of my prior work in this conversation]\norigin user-0 assistant-0"
                )
        );
        assert!(count_snapshot.contains(&Message::user(format!("user-{MAX_ACP_HISTORY_TURNS}"))));

        let mut by_bytes = SessionHistory::default();
        by_bytes.commit_completed_turn(
            &"u".repeat(MAX_ACP_HISTORY_BYTES),
            vec![Message::assistant("oversized")],
        );
        by_bytes
            .compact_with(
                &TurnControl::new(),
                #[cfg(feature = "goal")]
                None,
                |messages, _| async move {
                    let count = messages.len();
                    Ok(compaction_output("oversized origin retained", count))
                },
            )
            .await
            .unwrap();
        assert!(
            by_bytes.snapshot()[0]
                == Message::assistant(
                    "[Recap of my prior work in this conversation]\noversized origin retained"
                ),
            "a single oversized turn must be replaced by its bounded recap"
        );
    }

    #[tokio::test]
    async fn compaction_decision_serializes_live_history_once() {
        let mut history = SessionHistory::default();
        history.commit_completed_turn("hello", vec![Message::assistant("hi")]);
        LIVE_BYTES_CALLS.with(|calls| calls.set(0));
        // `compact_with` decides once and the loop does not re-ask when
        // nothing needs compacting; a known decision costs nothing.
        history
            .compact_with(
                &TurnControl::new(),
                #[cfg(feature = "goal")]
                None,
                |_, _| async { unreachable!("history is within bounds") },
            )
            .await
            .unwrap();
        assert_eq!(LIVE_BYTES_CALLS.with(std::cell::Cell::get), 1);
        history
            .compact_with_known(
                false,
                &TurnControl::new(),
                #[cfg(feature = "goal")]
                None,
                |_, _| async { unreachable!("history is within bounds") },
            )
            .await
            .unwrap();
        assert_eq!(LIVE_BYTES_CALLS.with(std::cell::Cell::get), 1);
    }

    #[tokio::test]
    async fn partial_summaries_finish_compaction_and_carry_forward_prior_evidence() {
        for byte_limited in [false, true] {
            let mut history = SessionHistory::default();
            let turn_count = if byte_limited {
                4
            } else {
                MAX_ACP_HISTORY_TURNS + 3
            };
            for index in 0..turn_count {
                let prompt = if byte_limited {
                    format!("user-{index} {}", "x".repeat(MAX_ACP_HISTORY_BYTES / 2))
                } else {
                    format!("user-{index}")
                };
                history.commit_completed_turn(
                    &prompt,
                    vec![Message::assistant(format!("assistant-{index}"))],
                );
            }
            let retained = history.snapshot_with_tool_result_retention(0)[6..].to_vec();
            let mut prior_summaries = Vec::new();
            history
                .compact_with(
                    &TurnControl::new(),
                    #[cfg(feature = "goal")]
                    None,
                    |messages, previous| {
                        let index = prior_summaries.len();
                        assert!(messages[0].content.contains(&format!("user-{index}")));
                        prior_summaries.push(previous);
                        async move {
                            Ok(compaction_output(
                                format!("covered through turn {index}"),
                                2,
                            ))
                        }
                    },
                )
                .await
                .expect("compaction is not cancelled");
            assert!(!history.needs_compaction());
            assert_eq!(
                prior_summaries,
                vec![
                    None,
                    Some("covered through turn 0".into()),
                    Some("covered through turn 1".into())
                ]
            );
            let snapshot = history.snapshot_with_tool_result_retention(0);
            assert_eq!(
                snapshot[0],
                Message::assistant(
                    "[Recap of my prior work in this conversation]\ncovered through turn 2"
                )
            );
            assert_eq!(snapshot[1..], retained);
        }
    }

    /// mini-agent-i6q98: every compaction pass is real spend, so the history
    /// reports the sum of all passes for the turn's goal round to count.
    #[tokio::test]
    async fn compaction_reports_the_summed_usage_of_every_pass() {
        let mut history = SessionHistory::default();
        for index in 0..(MAX_ACP_HISTORY_TURNS + 3) {
            history.commit_completed_turn(
                &format!("user-{index}"),
                vec![Message::assistant(format!("assistant-{index}"))],
            );
        }
        let mut passes = 0u64;
        let usage = history
            .compact_with(
                &TurnControl::new(),
                #[cfg(feature = "goal")]
                None,
                |_, _| {
                    passes += 1;
                    let pass = passes;
                    async move {
                        Ok(crate::provider::CompactionOutput {
                            usage: crate::event::UsageDelta {
                                input_tokens: 1_000 * pass,
                                output_tokens: 100 * pass,
                                ..crate::event::UsageDelta::default()
                            },
                            ..compaction_output(format!("pass {pass}"), 2)
                        })
                    }
                },
            )
            .await
            .expect("compaction is not cancelled");
        assert_eq!(passes, 3);
        assert_eq!(usage.input_tokens, 6_000);
        assert_eq!(usage.output_tokens, 600);
    }

    #[tokio::test]
    async fn cancelling_a_pending_summary_preserves_history_and_releases_its_lock() {
        struct PendingSummary(Arc<AtomicBool>);
        impl Drop for PendingSummary {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        for completed_first in [false, true] {
            let history = Arc::new(Mutex::new(SessionHistory::default()));
            {
                let mut history = history.lock().await;
                for index in 0..=MAX_ACP_HISTORY_TURNS + usize::from(completed_first) {
                    history.commit_completed_turn(
                        &format!("user-{index}"),
                        vec![Message::assistant("answer")],
                    );
                }
            }
            let mut expected = history.lock().await.snapshot_with_tool_result_retention(0);
            if completed_first {
                expected.drain(..2);
                expected.insert(
                    0,
                    Message::assistant(
                        "[Recap of my prior work in this conversation]\nfirst pass recap",
                    ),
                );
            }
            let control = TurnControl::new();
            let entered = Notify::new();
            let dropped = Arc::new(AtomicBool::new(false));
            let compact = async {
                let mut history = history.lock().await;
                let mut first = true;
                history
                    .compact_with(
                        &control,
                        #[cfg(feature = "goal")]
                        None,
                        |_, _| {
                            let complete_this_pass = completed_first && std::mem::take(&mut first);
                            let dropped = dropped.clone();
                            let entered = &entered;
                            async move {
                                if complete_this_pass {
                                    return Ok(compaction_output("first pass recap", 2));
                                }
                                let _pending = PendingSummary(dropped);
                                entered.notify_one();
                                std::future::pending::<
                                    anyhow::Result<crate::provider::CompactionOutput>,
                                >()
                                .await
                            }
                        },
                    )
                    .await
            };
            let cancel = async {
                entered.notified().await;
                assert!(control.cancel());
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(compact, cancel)
            })
            .await
            .expect("cancellation must finish without waiting for the summarizer");
            assert!(result.is_none());
            assert!(
                dropped.load(Ordering::Acquire),
                "the pending provider future must be dropped"
            );
            let mut history = history
                .try_lock()
                .expect("cancelled compaction releases the session lock");
            assert_eq!(history.snapshot_with_tool_result_retention(0), expected);
            assert!(
                history
                    .compact_with(
                        &control,
                        #[cfg(feature = "goal")]
                        None,
                        |_,
                         _|
                         -> std::future::Ready<
                            anyhow::Result<crate::provider::CompactionOutput>,
                        > {
                            panic!("an already-cancelled turn must not start a summary")
                        }
                    )
                    .await
                    .is_none()
            );
            history
                .compact_with(
                    &TurnControl::new(),
                    #[cfg(feature = "goal")]
                    None,
                    |messages, _| async move {
                        Ok(compaction_output("recovered recap", messages.len()))
                    },
                )
                .await
                .expect("a later turn can compact the unchanged history");
            assert!(!history.needs_compaction());
        }
    }

    #[tokio::test]
    async fn failed_or_incomplete_summary_cannot_wedge_acp_history() {
        for incomplete in [false, true] {
            let mut history = SessionHistory::default();
            history.commit_completed_turn(
                &"oversized".repeat(MAX_ACP_HISTORY_BYTES),
                vec![Message::assistant("old answer")],
            );
            assert!(history.needs_compaction());
            let result = history
                .compact_with(
                    &TurnControl::new(),
                    #[cfg(feature = "goal")]
                    None,
                    |_, _| async move {
                        if incomplete {
                            Ok(compaction_output("did not cover a turn", 0))
                        } else {
                            anyhow::bail!("summarizer unavailable")
                        }
                    },
                )
                .await;
            assert!(result.is_some());
            assert!(!history.needs_compaction());
            assert_eq!(
                history.snapshot(),
                vec![Message::assistant(format!(
                    "[Recap of my prior work in this conversation]\n{}",
                    SessionHistory::EMERGENCY_RECAP
                ))]
            );
        }
    }

    /// mini-agent-p9oyv: the turn's memory block leaves out a daily-log copy
    /// of the recap the ACP history already replays, like the terminal does.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn acp_memory_block_excludes_the_recap_history_replays() {
        let summary = "ACP recap: renamed the parser module";
        let mut history = SessionHistory::default();
        assert_eq!(history.active_compaction_ref(), None);
        history.commit_completed_turn(
            &"u".repeat(MAX_ACP_HISTORY_BYTES),
            vec![Message::assistant("old answer")],
        );
        history
            .compact_with(
                &TurnControl::new(),
                #[cfg(feature = "goal")]
                None,
                |messages, _| async move { Ok(compaction_output(summary, messages.len())) },
            )
            .await
            .unwrap();
        let reference = history.active_compaction_ref().expect("recap reference");
        assert_eq!(reference, crate::session::compaction_ref(summary));

        let root = std::env::temp_dir().join(format!(
            "mini-agent-acp-p9oyv-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mem = crate::extras::memory::Mem {
            root: root.clone(),
            project: "proj".into(),
            today: "2026-05-25".into(),
        };
        crate::extras::memory::flush_compaction_summary(&mem, summary, Some(2));
        crate::extras::memory::flush_compaction_summary(&mem, "an unrelated summary", None);
        let block = mem.context_block_excluding(Some(&reference)).unwrap();
        assert!(!block.contains(summary), "{block}");
        assert!(block.contains("an unrelated summary"), "{block}");

        // Canonical Session compaction gives every replayed recap an identity,
        // including the emergency recap. It has no daily-log copy to exclude;
        // the prior recap is no longer replayed and must become visible again.
        history.commit_completed_turn(
            &"v".repeat(MAX_ACP_HISTORY_BYTES),
            vec![Message::assistant("newer answer")],
        );
        history
            .compact_with(
                &TurnControl::new(),
                #[cfg(feature = "goal")]
                None,
                |_, _| async move { anyhow::bail!("summarizer unavailable") },
            )
            .await
            .unwrap();
        let emergency_reference = history
            .active_compaction_ref()
            .expect("emergency recap reference");
        assert_eq!(
            emergency_reference,
            crate::session::compaction_ref(SessionHistory::EMERGENCY_RECAP)
        );
        assert_ne!(emergency_reference, reference);
        let block = mem
            .context_block_excluding(Some(&emergency_reference))
            .unwrap();
        assert!(block.contains(summary), "{block}");
        assert!(block.contains("an unrelated summary"), "{block}");
        let _ = std::fs::remove_dir_all(&root);
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
            #[cfg(feature = "hooks")]
            if let Ok(workspace) = std::env::current_dir() {
                crate::extras::hooks::set_active_workspace(&workspace);
            }
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

    async fn acp53_poll_storage_until_held<F: std::future::Future>(
        work: &mut std::pin::Pin<Box<F>>,
        started: &mut std::sync::mpsc::Receiver<()>,
    ) {
        tokio::time::timeout(Duration::from_secs(2), async move {
            loop {
                tokio::select! {
                    _ = work.as_mut() => panic!("storage returned before held worker"),
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {},
                }
                if started.try_recv().is_ok() {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn acp53_checkpoint_drop_keeps_lease_and_blocking_job_until_actual_retirement() {
        let leases = Arc::new(SessionLeases::default());
        let id = SessionId::new(format!("checkpoint-{}", uuid::Uuid::new_v4()));
        let lease = leases.reserve(id.clone()).unwrap();
        lease.writer.store(true, Ordering::Release);
        let (scope, mut started, release) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        *lock_unpoisoned(&leases.storage_scope_for_test) = Some(scope.clone());
        let mut history = SessionHistory {
            _lease: Some(lease),
            ..Default::default()
        };
        // Validate fails before any artifact/path/claim. No global test roots
        // or production credential/storage namespace is opened by this control.
        history.session.add_user_message(
            crate::session::user::PersistedUserMessage::new(vec![
                crate::session::user::UserBlock::Text {
                    text: "authored".into(),
                },
            ])
            .unwrap(),
        );
        history.session.messages.last_mut().unwrap().content = "mismatched".into();
        let mut work = Box::pin(sessions::checkpoint(&history));
        acp53_poll_storage_until_held(&mut work, &mut started).await;
        assert_eq!(scope.active_children(), 1);
        drop(work);
        drop(history);
        assert!(scope.is_cancelled());
        assert!(
            lock_unpoisoned(&leases.ids).contains(&id),
            "writer lease retired before blocked checkpoint"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), scope.wait_idle())
                .await
                .is_err()
        );
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
            .await
            .unwrap();
        assert_eq!(scope.active_children(), 0);
        assert!(!lock_unpoisoned(&leases.ids).contains(&id));
    }

    #[tokio::test]
    async fn acp53_checkpoint_settlement_waits_for_the_exact_admitted_blocking_job() {
        let leases = Arc::new(SessionLeases::default());
        let lease = leases
            .reserve(SessionId::new(format!(
                "checkpoint-{}",
                uuid::Uuid::new_v4()
            )))
            .unwrap();
        lease.writer.store(true, Ordering::Release);
        let (scope, mut started, release) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        *lock_unpoisoned(&leases.storage_scope_for_test) = Some(scope.clone());
        let mut history = SessionHistory {
            _lease: Some(lease),
            ..Default::default()
        };
        history.session.add_user_message(
            crate::session::user::PersistedUserMessage::new(vec![
                crate::session::user::UserBlock::Text {
                    text: "authored".into(),
                },
            ])
            .unwrap(),
        );
        history.session.messages.last_mut().unwrap().content = "mismatched".into();
        let mut work = Box::pin(sessions::checkpoint(&history));
        acp53_poll_storage_until_held(&mut work, &mut started).await;
        assert_eq!(scope.active_children(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), work.as_mut())
                .await
                .is_err()
        );
        drop(release);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), work)
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(scope.active_children(), 0);
        assert!(
            scope.is_cancelled(),
            "normal settlement closes scope admission"
        );
    }

    #[tokio::test]
    async fn acp53_actual_list_handler_drop_keeps_discovery_owned_until_gate_release() {
        let root = crate::tests::OwnedTestDirectory::new("acp53-held-list");
        let requested = root.path().join("missing-workspace");
        let fixture: PromptFixture =
            Arc::new(|_, _| Box::pin(async { panic!("storage test must not invoke a provider") }));
        let state = fixture_state(fixture);
        let (scope, started, release) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        *lock_unpoisoned(&state.leases.storage_scope_for_test) = Some(scope.clone());
        let gate = Arc::new(StdMutex::new(Some((started, release))));
        let server = Agent.builder().on_receive_request(
            {
                let state = state.clone();
                let scope = scope.clone();
                let gate = gate.clone();
                move |req: ListSessionsRequest, responder, _cx| {
                    let state = state.clone();
                    let scope = scope.clone();
                    let (mut started, release) = lock_unpoisoned(&gate).take().unwrap();
                    async move {
                        let mut work = Box::pin(sessions::handle_list(req, responder, &state));
                        acp53_poll_storage_until_held(&mut work, &mut started).await;
                        assert_eq!(scope.active_children(), 1);
                        drop(work);
                        assert!(scope.is_cancelled());
                        assert!(
                            tokio::time::timeout(Duration::from_millis(20), scope.wait_idle())
                                .await
                                .is_err()
                        );
                        drop(release);
                        tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
                            .await
                            .unwrap();
                        assert_eq!(scope.active_children(), 0);
                        Ok(())
                    }
                }
            },
            on_receive_request!(),
        );
        Client
            .builder()
            .connect_with(server, async move |cx| {
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        cx.send_request(ListSessionsRequest::new().cwd(requested))
                            .block_task()
                    )
                    .await
                    .is_err(),
                    "dropped actual responder cannot report a catalogue"
                );
                // Drive the server through its owned assertions after response drop.
                tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
                    .await
                    .unwrap();
                Ok(())
            })
            .await
            .unwrap();
        assert!(state.sessions.lock().await.is_empty());
        assert!(lock_unpoisoned(&state.leases.ids).is_empty());
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn protocol_workspace_fixture_restores_the_hook_workspace_before_cleanup() {
        let expected = std::env::current_dir().unwrap().canonicalize().unwrap();
        {
            let workspace = ProtocolTempDir::new();
            crate::extras::hooks::set_active_workspace(workspace.path());
            assert_eq!(crate::extras::hooks::active_workspace(), workspace.path());
        }
        assert_eq!(crate::extras::hooks::active_workspace(), expected);
    }

    #[test]
    fn acp53_session_identity_stays_reserved_until_last_history_owner_retires() {
        let leases = Arc::new(SessionLeases::default());
        let id = SessionId::new("same-record");
        for invalid in ["", "../record", "CON", &"x".repeat(256)] {
            assert!(leases.reserve(SessionId::new(invalid)).is_err());
        }
        assert!(lock_unpoisoned(&leases.ids).is_empty());
        let pending = leases.reserve(id.clone()).unwrap();
        assert!(
            leases.reserve(id.clone()).is_err(),
            "parallel imports cannot share one writer"
        );
        let history = Arc::new(Mutex::new(SessionHistory {
            _lease: Some(pending),
            ..Default::default()
        }));
        let retiring_turn = history.clone();
        drop(history); // Simulate removal from the loaded session map.
        assert!(
            leases.reserve(id.clone()).is_err(),
            "close cannot redirect the retiring writer"
        );
        drop(retiring_turn);
        let replacement = leases.reserve(id.clone()).unwrap();
        assert_eq!(lock_unpoisoned(&leases.ids).len(), 1);
        drop(replacement);
        assert!(lock_unpoisoned(&leases.ids).is_empty());
    }

    #[test]
    fn acp53_pending_and_retiring_session_owners_share_the_loaded_capacity() {
        let leases = Arc::new(SessionLeases::default());
        let mut reservations = (0..MAX_ACP_SESSIONS)
            .map(|n| {
                leases
                    .reserve(SessionId::new(format!("record-{n}")))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let extra = SessionId::new("extra");
        assert!(leases.reserve(extra.clone()).is_err());
        let retiring = reservations[0].clone();
        drop(reservations.remove(0));
        assert!(
            leases.reserve(extra.clone()).is_err(),
            "a retiring owner still occupies its slot"
        );
        drop(retiring);
        let admitted = leases.reserve(extra.clone()).unwrap();
        assert_eq!(lock_unpoisoned(&leases.ids).len(), MAX_ACP_SESSIONS);
        drop(admitted);
        drop(reservations);
        assert!(lock_unpoisoned(&leases.ids).is_empty());
        // An import that fails before insertion releases both identity and slot.
        drop(leases.reserve(extra.clone()).unwrap());
        assert!(leases.reserve(extra).is_ok());
    }

    #[test]
    fn prompt_blocks_render_required_resource_links_and_reject_empty_input() {
        let rendered = render_prompt_blocks(&[
            ContentBlock::Text(TextContent::new("inspect this")),
            ContentBlock::ResourceLink(
                ResourceLink::new("report", "file:///workspace/report.md")
                    .description(Some("the generated report".to_string())),
            ),
        ])
        .unwrap();
        assert_eq!(
            rendered,
            "inspect this\n[resource: report](file:///workspace/report.md)\nthe generated report"
        );
        assert!(render_prompt_blocks(&[]).is_err());
        assert!(render_prompt_blocks(&[ContentBlock::Text(TextContent::new("  "))]).is_err());
    }

    #[tokio::test]
    async fn acp53_actual_local_commands_mutate_the_canonical_session_without_a_runner() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let state = fixture_state(Arc::new(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(vec![AgentEvent::Done {
                    response: "answer".into(),
                    interactions: vec![Message::assistant("answer")],
                }])
            })
        }));
        let inspect = state.clone();
        let updates = Arc::new(StdMutex::new(Vec::<SessionNotification>::new()));
        let sink = updates.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        Client.builder().on_receive_notification(async move |notification: SessionNotification, _cx| {
            sink.lock().unwrap().push(notification); Ok(())
        }, agent_client_protocol::on_receive_notification!()).connect_with(InMemoryAgent(state), async move |cx| {
            let id = cx.send_request(NewSessionRequest::new(cwd)).block_task().await?.session_id;
            cx.send_request(prompt(id.clone(), "ordinary")).block_task().await?;
            let before = inspect.sessions.lock().await[&id].history.lock().await.session.messages.len();
            assert_eq!(before, 2);
            cx.send_request(prompt(id.clone(), "/rename literal $HOME @file")).block_task().await?;
            {
                let sessions = inspect.sessions.lock().await;
                let history = sessions[&id].history.lock().await;
                assert_eq!(history.session.name, "literal $HOME @file");
                assert_eq!(history.session.messages.len(), before);
            }
            cx.send_request(prompt(id.clone(), "/undo")).block_task().await?;
            assert!(inspect.sessions.lock().await[&id].history.lock().await.session.messages.is_empty());
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            assert!(updates.lock().unwrap().iter().any(|notice| matches!(&notice.update, SessionUpdate::AvailableCommandsUpdate(commands) if commands.available_commands.iter().any(|command| command.name == "rename"))));
            assert_eq!(calls.load(Ordering::SeqCst), 1, "local commands cannot launch a model runner");
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn acp53_local_checkpoint_failure_replies_without_closing_the_connection() {
        let mut state = fixture_state(Arc::new(|_, _| {
            Box::pin(async { panic!("local commands must not invoke a model") })
        }));
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let id = Client
            .builder()
            .connect_with(InMemoryAgent(state.clone()), async move |cx| {
                Ok(cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id)
            })
            .await
            .unwrap();
        // The same admitted Session has no claimed disk writer in --no-session
        // mode. Requiring persistence now provides a deterministic real
        // checkpoint failure without touching private/user filesystem roots.
        Arc::get_mut(&mut state)
            .expect("connection retired its state owner")
            .cli
            .no_session = false;
        let inspect = state.clone();
        Client
            .builder()
            .connect_with(InMemoryAgent(state), async move |cx| {
                let error = cx
                    .send_request(prompt(id.clone(), "/rename retained local name"))
                    .block_task()
                    .await
                    .unwrap_err();
                assert!(
                    error.to_string().contains("ACP command checkpoint failed"),
                    "{error}"
                );
                #[cfg(feature = "goal")]
                {
                    for (text, expected) in [
                        (
                            "/goal first bounded goal".to_owned(),
                            "ACP command checkpoint failed",
                        ),
                        (
                            "/goal replacement goal".to_owned(),
                            "an unfinished ACP goal already exists",
                        ),
                        (
                            format!(
                                "/goal {}",
                                "x".repeat(crate::extras::goal::MAX_OBJECTIVE_CHARS + 1)
                            ),
                            "invalid ACP goal",
                        ),
                    ] {
                        let error = cx
                            .send_request(prompt(id.clone(), &text))
                            .block_task()
                            .await
                            .unwrap_err();
                        assert!(error.to_string().contains(expected), "{error}");
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                    }
                }
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert_eq!(
                    inspect.sessions.lock().await[&id]
                        .history
                        .lock()
                        .await
                        .session
                        .name,
                    "retained local name"
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[cfg(feature = "goal")]
    #[tokio::test]
    async fn acp53_loaded_goal_rebuilds_execution_authority_from_current_configuration() {
        let state = fixture_state(Arc::new(|_, _| {
            Box::pin(async { Err("unused runner".into()) })
        }));
        let workspace = ProtocolTempDir::new();
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(workspace.path()).unwrap());
        let id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let lease = state.leases.reserve(id.clone()).unwrap();
        let saved = crate::session::Session::new("untrusted-label", "untrusted-model", 128_000, "");
        let mut goal = crate::extras::goal::Goal::configured(
            "retained objective",
            Vec::new(),
            crate::extras::goal::GoalDefaults {
                cfg: &state.cfg,
                provider: "openai",
                model: "fixture",
            },
        )
        .unwrap();
        goal.context_file = Some(workspace.path().join("must-not-be-read"));
        goal.progress.rounds = 7;
        saved.goal_store.set(goal, false).unwrap();
        install_session(
            &state,
            id.clone(),
            binding,
            saved,
            lease,
            #[cfg(feature = "subagents")]
            BackgroundSessionClient::Unconnected,
            #[cfg(feature = "mcp")]
            Default::default(),
        )
        .await
        .unwrap();
        let sessions = state.sessions.lock().await;
        let stored = sessions[&id].goal_store.snapshot().unwrap();
        assert_eq!(stored.objective, "retained objective");
        assert_eq!(stored.progress.rounds, 7);
        assert!(
            stored.context_file.is_none(),
            "saved path cannot become read authority"
        );
        assert!(stored.checks.is_empty());
        assert!(stored.last_verdict.is_none());
        assert!(
            sessions[&id]
                .history
                .lock()
                .await
                .session
                .goal_store
                .snapshot()
                .unwrap()
                .context_file
                .is_none()
        );
    }

    fn fixture_state(prompt_fixture: PromptFixture) -> Arc<AcpState> {
        Arc::new(AcpState {
            idle: idle::Idle::new(None).unwrap(),
            cli: Cli {
                no_session: true,
                ..Cli::default()
            },
            cfg: Config::default(),
            context: crate::context::load(true),
            leases: Arc::default(),
            provider_clients: Default::default(),
            sessions: Mutex::new(HashMap::new()),
            cancel_routes: StdMutex::new(HashMap::new()),
            shell_search_path: std::env::var_os("PATH"),
            hook_roots: None,
            client_mcp: ClientMcpPolicy::Sandboxed,
            prompt_fixture: Some(prompt_fixture),
            runner_fixture: None,
            #[cfg(feature = "mcp")]
            mcp_fixture: StdMutex::new(None),
            prompt_exit_barrier: None,
        })
    }

    fn runner_fixture_state(runner_fixture: RunnerFixture) -> Arc<AcpState> {
        Arc::new(AcpState {
            idle: idle::Idle::new(None).unwrap(),
            cli: Cli {
                no_session: true,
                ..Cli::default()
            },
            cfg: Config::default(),
            context: crate::context::load(true),
            leases: Arc::default(),
            provider_clients: Default::default(),
            sessions: Mutex::new(HashMap::new()),
            cancel_routes: StdMutex::new(HashMap::new()),
            shell_search_path: std::env::var_os("PATH"),
            hook_roots: None,
            client_mcp: ClientMcpPolicy::Sandboxed,
            prompt_fixture: None,
            runner_fixture: Some(runner_fixture),
            #[cfg(feature = "mcp")]
            mcp_fixture: StdMutex::new(None),
            prompt_exit_barrier: None,
        })
    }

    fn fixture_state_with_exit_barrier(
        prompt_fixture: PromptFixture,
        prompt_exit_barrier: Arc<PromptExitBarrier>,
    ) -> Arc<AcpState> {
        Arc::new(AcpState {
            idle: idle::Idle::new(None).unwrap(),
            cli: Cli {
                no_session: true,
                ..Cli::default()
            },
            cfg: Config::default(),
            context: crate::context::load(true),
            leases: Arc::default(),
            provider_clients: Default::default(),
            sessions: Mutex::new(HashMap::new()),
            cancel_routes: StdMutex::new(HashMap::new()),
            shell_search_path: std::env::var_os("PATH"),
            hook_roots: None,
            client_mcp: ClientMcpPolicy::Sandboxed,
            prompt_fixture: Some(prompt_fixture),
            runner_fixture: None,
            #[cfg(feature = "mcp")]
            mcp_fixture: StdMutex::new(None),
            prompt_exit_barrier: Some(prompt_exit_barrier),
        })
    }

    #[test]
    fn acp_workspace_shell_capability_is_captured_once_for_rebuilds() {
        let temp = ProtocolTempDir::new();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join(if cfg!(windows) { "bash.exe" } else { "bash" });
        #[cfg(windows)]
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        #[cfg(not(windows))]
        std::fs::write(&executable, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(temp.path()).unwrap());
        let state = AcpState {
            idle: idle::Idle::new(None).unwrap(),
            cli: Cli {
                no_sandbox: true,
                shell: Some("bash".to_string()),
                ..Cli::default()
            },
            cfg: Config::default(),
            context: crate::context::load(true),
            leases: Arc::default(),
            provider_clients: Default::default(),
            sessions: Mutex::new(HashMap::new()),
            cancel_routes: StdMutex::new(HashMap::new()),
            shell_search_path: Some(std::ffi::OsString::from("bin")),
            hook_roots: None,
            client_mcp: ClientMcpPolicy::Sandboxed,
            prompt_fixture: None,
            runner_fixture: None,
            #[cfg(feature = "mcp")]
            mcp_fixture: StdMutex::new(None),
            prompt_exit_barrier: None,
        };

        let sandbox = acp_session_sandbox(&state, workspace.clone()).unwrap();
        let rebuilt = sandbox.clone();
        let capability = sandbox.shell_capability().unwrap();
        assert_eq!(capability.executable(), executable.canonicalize().unwrap());
        assert_eq!(capability.command_arg(), "-c");
        assert_eq!(
            rebuilt.shell_capability().unwrap().executable(),
            capability.executable()
        );

        drop((rebuilt, sandbox, workspace, state));
        drop(temp);
    }

    fn done(response: &str, interactions: Vec<Message>) -> AgentEvent {
        AgentEvent::Done {
            response: response.into(),
            interactions,
        }
    }

    #[tokio::test]
    async fn streamed_text_is_coalesced_and_ordered_before_other_updates() {
        let fixture: PromptFixture = Arc::new(|_, _| {
            Box::pin(async move {
                let mut events = Vec::new();
                for index in 0..200 {
                    events.push(AgentEvent::Token(format!("m{index} ").into()));
                }
                events.push(AgentEvent::Reasoning("think-1 ".into()));
                events.push(AgentEvent::Reasoning("think-2".into()));
                events.push(AgentEvent::Token("before-tool ".into()));
                events.push(AgentEvent::ToolCall {
                    id: "coalesce-a".into(),
                    name: "read".into(),
                    args: serde_json::json!({"path": "a"}),
                });
                events.push(AgentEvent::Token("after-tool".into()));
                events.push(done("end", vec![Message::assistant("end")]));
                Ok(events)
            })
        });
        let state = fixture_state(fixture);
        let notifications = Arc::new(StdMutex::new(Vec::<SessionNotification>::new()));
        let sink = notifications.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    sink.lock().unwrap().push(notification);
                    Ok(())
                },
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
                let response = cx
                    .send_request(prompt(session, "stream"))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                Ok(())
            })
            .await
            .unwrap();
        // (kind, text) in arrival order; kind: 'm' message, 't' thought, 'c' tool call.
        let mut order = Vec::<(char, String)>::new();
        for notification in notifications.lock().unwrap().iter() {
            let (kind, text) = match &notification.update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => ('m', text.text.clone()),
                    _ => continue,
                },
                SessionUpdate::AgentThoughtChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => ('t', text.text.clone()),
                    _ => continue,
                },
                SessionUpdate::ToolCall(_) => ('c', String::new()),
                _ => continue,
            };
            order.push((kind, text));
        }
        let kinds = order.iter().map(|(kind, _)| *kind).collect::<String>();
        let tool_at = kinds.find('c').expect("tool call notification");
        assert!(
            kinds[..tool_at]
                .chars()
                .all(|kind| matches!(kind, 'm' | 't')),
            "{kinds}"
        );
        let joined = |kind: char, range: std::ops::Range<usize>| {
            order[range]
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, text)| text.as_str())
                .collect::<String>()
        };
        let expected_messages = (0..200).map(|i| format!("m{i} ")).collect::<String>();
        assert_eq!(
            joined('m', 0..tool_at),
            format!("{expected_messages}before-tool ")
        );
        assert_eq!(joined('t', 0..tool_at), "think-1 think-2");
        assert_eq!(joined('m', tool_at + 1..order.len()), "after-tool");
        // Reasoning stays before the later message text it preceded.
        let first_thought = kinds.find('t').unwrap();
        assert!(kinds[..first_thought].chars().all(|kind| kind == 'm'));
        assert!(
            kinds[..tool_at].len() < 100,
            "{} chunks for 203 deltas",
            kinds[..tool_at].len()
        );
    }

    fn prompt(session_id: SessionId, text: &str) -> PromptRequest {
        PromptRequest::new(session_id, vec![ContentBlock::Text(TextContent::new(text))])
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
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
    async fn usd_budget_actual_acp_protocol_preserves_session_accounts_and_denies_without_model_effects()
     {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let fixture: PromptFixture = Arc::new(move |_, _| {
            counter.fetch_add(1, Ordering::AcqRel);
            Box::pin(async { Ok(vec![done("NEVER", vec![Message::assistant("NEVER")])]) })
        });
        let mut state = fixture_state(fixture);
        Arc::get_mut(&mut state).unwrap().cfg.max_budget_usd = Some(0.0);
        let owner = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        Client
            .builder()
            .connect_with(InMemoryAgent(state), async move |cx| {
                let first = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                let second = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let failed = cx
                    .send_request(prompt(first.clone(), "never admitted"))
                    .block_task()
                    .await;
                assert!(failed.is_err(), "budget refusal is a JSON-RPC error");
                assert!(
                    failed
                        .unwrap_err()
                        .to_string()
                        .contains(crate::spend::run::BUDGET_STOP)
                );
                let sessions = owner.sessions.lock().await;
                let first = sessions.get(&first).unwrap();
                let second = sessions.get(&second).unwrap();
                assert!(first.history.lock().await.snapshot().is_empty());
                assert!(second.history.lock().await.snapshot().is_empty());
                assert!(first.spend.admit_effect().is_err());
                assert!(second.spend.admit_effect().is_err());
                assert_eq!(first.spend.account.snapshot().recorded_spend(), 0.0);
                assert_eq!(second.spend.account.snapshot().recorded_spend(), 0.0);
                assert_eq!(calls.load(Ordering::Acquire), 0);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn acp53_actual_mode_rpc_is_session_local_and_unknown_modes_leave_it_unchanged() {
        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { unreachable!() }));
        let state = fixture_state(fixture);
        let retained = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let updates = Arc::new(StdMutex::new(Vec::new()));
        let sink = updates.clone();
        let connection = Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    if let SessionUpdate::CurrentModeUpdate(update) = notification.update {
                        sink.lock()
                            .unwrap()
                            .push(update.current_mode_id.to_string());
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                let first = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                let second = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                assert_eq!(first.modes.as_ref().unwrap().available_modes.len(), 6);
                let initial = second.modes.unwrap().current_mode_id.to_string();
                cx.send_request(SetSessionModeRequest::new(
                    first.session_id.clone(),
                    SessionModeId::new("readonly"),
                ))
                .block_task()
                .await?;
                assert!(
                    cx.send_request(SetSessionModeRequest::new(
                        first.session_id.clone(),
                        SessionModeId::new("none")
                    ))
                    .block_task()
                    .await
                    .is_err()
                );
                let sessions = retained.sessions.lock().await;
                assert_eq!(
                    sessions[&first.session_id]
                        .history
                        .lock()
                        .await
                        .policy
                        .mode(),
                    Some(crate::permission::SecurityMode::ReadOnly)
                );
                assert_eq!(
                    sessions[&second.session_id]
                        .history
                        .lock()
                        .await
                        .policy
                        .mode()
                        .unwrap()
                        .to_string(),
                    initial
                );
                drop(sessions);
                cx.send_request(CloseSessionRequest::new(first.session_id))
                    .block_task()
                    .await?;
                cx.send_request(CloseSessionRequest::new(second.session_id))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(5), connection)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*updates.lock().unwrap(), vec!["readonly"]);
    }

    #[tokio::test]
    async fn acp53_actual_mode_rpc_refuses_an_admitted_pending_turn() {
        struct Release(Arc<Notify>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let fixture: PromptFixture = {
            let entered = entered.clone();
            let release = release.clone();
            Arc::new(move |_, _| {
                let entered = entered.clone();
                let release = release.clone();
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(vec![done("settled", vec![Message::assistant("settled")])])
                })
            })
        };
        let state = fixture_state(fixture);
        let retained = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let connection = Client.builder().connect_with(InMemoryAgent(state), async move |cx| {
            let _release_on_exit = Release(release.clone());
            let created = cx.send_request(NewSessionRequest::new(cwd)).block_task().await?;
            let initial = created.modes.unwrap().current_mode_id.to_string();
            let pending = cx.send_request(prompt(created.session_id.clone(), "pending")).block_task();
            tokio::pin!(pending);
            tokio::select! {
                _ = entered.notified() => {},
                result = &mut pending => panic!("prompt ended before the owned pending gate: {result:?}"),
            }
            assert!(cx.send_request(SetSessionModeRequest::new(created.session_id.clone(), SessionModeId::new("readonly")))
                .block_task().await.is_err());
            {
                let sessions = retained.sessions.lock().await;
                assert!(sessions[&created.session_id].history.try_lock().is_err());
            }
            release.notify_one();
            assert_eq!(pending.await?.stop_reason, StopReason::EndTurn);
            {
                let sessions = retained.sessions.lock().await;
                assert_eq!(sessions[&created.session_id].history.lock().await.policy.mode().unwrap().to_string(), initial);
            }
            cx.send_request(SetSessionModeRequest::new(created.session_id.clone(), SessionModeId::new("readonly")))
                .block_task().await?;
            cx.send_request(CloseSessionRequest::new(created.session_id)).block_task().await?;
            Ok(())
        });
        tokio::time::timeout(Duration::from_secs(5), connection)
            .await
            .unwrap()
            .unwrap();
    }

    #[cfg(feature = "subagents")]
    #[tokio::test]
    async fn background56_actual_acp_permission_after_parent_done_and_close_joins_child() {
        use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode};
        let root = crate::tests::OwnedTestDirectory::new("background56-acp-permission");
        let bindings = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _environment = crate::tests::ScopedProcessEnv::set(&bindings);
        for selected in ["allow_once", "deny_once", "cancelled"] {
            let state = fixture_state(Arc::new(|_, _| {
                Box::pin(async { Ok(vec![done("parent complete", Vec::new())]) })
            }));
            let retained = state.clone();
            let captured = state.clone();
            let cwd = root.path().to_path_buf();
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed_calls = calls.clone();
            let work = Client
                .builder()
                .on_receive_notification(
                    async |_: SessionNotification, _cx| Ok(()),
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    move |request: RequestPermissionRequest,
                          responder: Responder<RequestPermissionResponse>,
                          _cx| {
                        observed_calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        async move {
                            assert!(
                                request
                                    .options
                                    .iter()
                                    .any(|o| o.option_id == PermissionOptionId::new("allow_once"))
                            );
                            let outcome = if selected == "cancelled" {
                                RequestPermissionOutcome::Cancelled
                            } else {
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                    selected,
                                ))
                            };
                            responder.respond(RequestPermissionResponse::new(outcome))
                        }
                    },
                    on_receive_request!(),
                )
                .connect_with(InMemoryAgent(state), async move |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let sid = cx
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?
                        .session_id;
                    let response = cx
                        .send_request(prompt(sid.clone(), "parent boundary"))
                        .block_task()
                        .await?;
                    assert_eq!(response.stop_reason, StopReason::EndTurn);
                    let (run, pool, workspace, recorder) = {
                        let sessions = captured.sessions.lock().await;
                        let session = &sessions[&sid];
                        let history = session.history.lock().await;
                        assert!(lock_unpoisoned(&session.turns).queue.is_empty());
                        (
                            session.spend.clone(),
                            session.spend.background_pool().unwrap(),
                            session.workspace.clone(),
                            history
                                .session
                                .file_checkpoints
                                .recorder(&history.session.id, 0, true),
                        )
                    };
                    let permission = Some(Arc::new(StdMutex::new(
                        crate::permission::checker::PermissionChecker::new(
                            &PermissionConfigs::from(PermissionConfig {
                                default: Some(Action::Ask),
                                ..Default::default()
                            }),
                            SecurityMode::Standard,
                            Some(workspace.root().into()),
                            None,
                        )
                        .unwrap(),
                    )));
                    let mut admission = pool.reserve(&workspace, &run, &recorder).unwrap();
                    let ask = admission.prepare_ask(permission.clone(), None).unwrap();
                    let (observed, decision) = tokio::sync::oneshot::channel();
                    let id = admission
                        .start(
                            async move {
                                let result = crate::agent::tools::check_perm(
                                    &permission,
                                    &ask,
                                    "read",
                                    "owned.txt",
                                )
                                .await;
                                observed.send(result.is_ok()).unwrap();
                                // This is real session lifetime work, not a turn waiter.
                                std::future::pending::<Result<String, String>>().await
                            },
                            run,
                        )
                        .unwrap();
                    assert_eq!(
                        tokio::time::timeout(Duration::from_secs(3), decision)
                            .await
                            .unwrap()
                            .unwrap(),
                        selected == "allow_once"
                    );
                    assert_eq!(pool.active_count(), 1);
                    cx.send_request(CloseSessionRequest::new(sid.clone()))
                        .block_task()
                        .await?;
                    assert_eq!(pool.active_count(), 0);
                    assert_eq!(
                        pool.result(&id).unwrap().status,
                        crate::extras::subagents::background::Status::Cancelled
                    );
                    assert!(!captured.sessions.lock().await.contains_key(&sid));
                    Ok(())
                });
            let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(work)).await;
            let pools = retained
                .sessions
                .lock()
                .await
                .values()
                .filter_map(|s| s.spend.background_pool())
                .collect::<Vec<_>>();
            for pool in pools {
                pool.close().await;
            }
            retained_wait_for_fixture_cleanup(&retained).await;
            match result {
                Ok(result) => result.unwrap(),
                Err(panic) => std::panic::resume_unwind(panic),
            }
            assert_eq!(calls.load(std::sync::atomic::Ordering::Acquire), 1);
        }
    }

    #[tokio::test]
    async fn acp53_actual_permission_selections_rebuild_only_the_selected_session_choice() {
        use crate::permission::{
            PermissionConfigs, SecurityMode,
            ask::{AskRequest, UserDecision},
            checker::{CheckResult, PermissionChecker},
        };
        let root = crate::tests::OwnedTestDirectory::new("acp53-rpc-choice");
        let target = root
            .path()
            .join("exact*.txt")
            .to_string_lossy()
            .into_owned();
        for selected in ["allow_always", "deny_always", "unadvertised"] {
            let policy = policy::SessionPolicy::new(SecurityMode::Guarded, true);
            let retained = policy.clone();
            let permission = Some(Arc::new(StdMutex::new(
                PermissionChecker::new(
                    &PermissionConfigs::default(),
                    SecurityMode::Guarded,
                    Some(root.path().to_path_buf()),
                    None,
                )
                .unwrap(),
            )));
            let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
            let agent = Agent
                .builder()
                .on_receive_request(
                    async |_: InitializeRequest, responder, _cx| {
                        responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                    },
                    on_receive_request!(),
                )
                .with_spawned(move |cx| async move {
                    drive_permission_bridge(
                        cx,
                        idle::Activity::default(),
                        SessionId::new("choice"),
                        ask_rx,
                        Arc::new(TurnControl::new()),
                        Some(BridgePolicy { policy, permission }),
                        #[cfg(feature = "goal")]
                        Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    )
                    .await;
                    Ok(())
                });
            let (requests, mut incoming) = tokio::sync::mpsc::unbounded_channel();
            let input = target.clone();
            let connection = Client
                .builder()
                .on_receive_request(
                    move |request: RequestPermissionRequest, responder, _cx| {
                        requests.send((request, responder)).unwrap();
                        async { Ok(()) }
                    },
                    on_receive_request!(),
                )
                .connect_with(agent, async move |_cx| {
                    let (reply, receiver) = tokio::sync::oneshot::channel();
                    ask_tx
                        .send(
                            AskRequest {
                                diagnostic: Some(crate::permission::ask::AskDiagnostic {
                                    effective_mode: SecurityMode::Guarded,
                                    selected_mode: SecurityMode::Yolo,
                                    cause: crate::permission::ask::AskCause::ConfiguredAskRule,
                                }),
                                #[cfg(feature = "subagents")]
                                background: None,
                                tool: "write".into(),
                                input,
                                tool_call_id: Some("current".into()),
                                agent_name: None,
                                suggested_pattern: None,
                                additional_allow_patterns: Vec::new(),
                                reply,
                            }
                            .into(),
                        )
                        .await
                        .unwrap();
                    let (request, responder) = incoming.recv().await.unwrap();
                    let wire = serde_json::to_value(&request).unwrap();
                    assert_eq!(wire.pointer("/toolCall/title").and_then(serde_json::Value::as_str),
                        Some("write [effective mode: guarded; selected mode: yolo; cause: configured Ask rule; prompt mode narrowed the selected mode]"));
                    assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "current");
                    assert!(
                        request
                            .options
                            .iter()
                            .any(|option| option.kind == PermissionOptionKind::RejectAlways)
                    );
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            selected,
                        )),
                    ))?;
                    Ok(receiver.await.unwrap())
                });
            let decision = tokio::time::timeout(Duration::from_secs(5), connection)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                matches!(decision, UserDecision::AllowAlways(_)),
                selected == "allow_always"
            );
            let next = Some(Arc::new(StdMutex::new(
                PermissionChecker::new(
                    &PermissionConfigs::default(),
                    SecurityMode::Guarded,
                    Some(root.path().to_path_buf()),
                    None,
                )
                .unwrap(),
            )));
            retained.apply(&next);
            let result = next
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .check_bound_path("write", &target);
            assert_eq!(
                matches!(result, CheckResult::Allowed),
                selected == "allow_always"
            );
            assert_eq!(
                matches!(result, CheckResult::Denied(_)),
                selected == "deny_always"
            );
        }
    }

    #[tokio::test]
    async fn allow_always_meta_names_the_exact_session_rule_it_installs() {
        use crate::permission::ask::{AskRequest, UserDecision};

        let requested = if cfg!(windows) {
            r"C:\work\**"
        } else {
            "/work/**"
        };
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
        let control = Arc::new(TurnControl::new());
        let agent = Agent
            .builder()
            .on_receive_request(
                async |_: InitializeRequest, responder, _cx| {
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                on_receive_request!(),
            )
            .with_spawned(move |cx| async move {
                drive_permission_bridge(
                    cx,
                    idle::Activity::default(),
                    SessionId::new("allow-always-meta"),
                    ask_rx,
                    control,
                    None,
                    #[cfg(feature = "goal")]
                    Arc::new(std::sync::atomic::AtomicU64::new(0)),
                )
                .await;
                Ok(())
            });
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let connection = Client
            .builder()
            .on_receive_request(
                move |request: RequestPermissionRequest, responder, _cx| {
                    let _ = request_tx.send((request, responder));
                    async { Ok(()) }
                },
                on_receive_request!(),
            )
            .connect_with(agent, async move |_cx| {
                let (reply, receiver) = tokio::sync::oneshot::channel();
                ask_tx
                    .send(
                        AskRequest {
                            diagnostic: None,
                            #[cfg(feature = "subagents")]
                            background: None,
                            agent_name: None,
                            tool: "write".into(),
                            input: requested.to_owned(),
                            tool_call_id: Some("write-call".to_owned()),
                            suggested_pattern: None,
                            additional_allow_patterns: Vec::new(),
                            reply,
                        }
                        .into(),
                    )
                    .await
                    .unwrap();
                let (request, responder) = request_rx.recv().await.unwrap();
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        "allow_always",
                    )),
                ))?;
                Ok((request, receiver.await.unwrap()))
            });
        let (request, decision) =
            tokio::time::timeout(std::time::Duration::from_secs(5), connection)
                .await
                .expect("the bridge must answer")
                .unwrap();

        let UserDecision::AllowAlways(installed) = decision else {
            panic!("allow_always must install a session rule: {decision:?}");
        };
        let option = request
            .options
            .iter()
            .find(|option| option.kind == PermissionOptionKind::AllowAlways)
            .expect("an allow-always option is offered");
        assert_eq!(option.name, "Allow for this session");
        let meta = option.meta.as_ref().expect("the option carries _meta");
        assert_eq!(meta["allowPatterns"], serde_json::json!([installed]));
        assert_eq!(meta["scope"], "session");
        let readable = format!("exactly {}", requested.replace('\\', "/"));
        assert_eq!(meta["suggestedPattern"], readable.as_str());

        // What the checker receives is a literal path, not a glob over the
        // tree the model named.
        let matcher =
            crate::permission::pattern::Pattern::new_generated_path_scope(&installed).unwrap();
        assert!(matcher.matches_path(requested));
        let sibling = if cfg!(windows) {
            r"C:\work\sibling.txt"
        } else {
            "/work/sibling.txt"
        };
        assert!(!matcher.matches_path(sibling));
    }

    #[test]
    fn allow_always_meta_lists_a_suggestion_and_its_additional_scopes() {
        let (reply, _receiver) = tokio::sync::oneshot::channel();
        let (descendants, exact) = crate::permission::pattern::search_root_allow_scope("/repo");
        let ask = crate::permission::ask::AskRequest {
            diagnostic: None,
            #[cfg(feature = "subagents")]
            background: None,
            agent_name: None,
            tool: "grep".into(),
            input: "/repo".to_owned(),
            tool_call_id: None,
            suggested_pattern: Some(descendants.clone()),
            additional_allow_patterns: vec![exact.clone()],
            reply,
        };
        let (pattern, meta) = allow_always_scope(&ask);
        assert_eq!(pattern, descendants);
        assert_eq!(
            meta["allowPatterns"],
            serde_json::json!([descendants, exact])
        );
        assert_eq!(
            meta["suggestedPattern"],
            "anything under /repo/ and exactly /repo"
        );

        let (reply, _receiver) = tokio::sync::oneshot::channel();
        let shell = crate::permission::ask::AskRequest {
            diagnostic: None,
            #[cfg(feature = "subagents")]
            background: None,
            agent_name: None,
            tool: "shell".into(),
            input: "rm -rf *".to_owned(),
            tool_call_id: None,
            suggested_pattern: None,
            additional_allow_patterns: Vec::new(),
            reply,
        };
        let (pattern, meta) = allow_always_scope(&shell);
        assert_eq!(pattern, "rm -rf *");
        assert_eq!(meta["suggestedPattern"], "rm -rf *");
    }

    #[tokio::test]
    async fn permission_bridge_settles_synthetic_calls_and_releases_abandoned_requests() {
        use crate::permission::ask::{AskRequest, UserDecision};

        let mut failures = Vec::new();
        for synthetic in [true, false] {
            for case in [
                "once",
                "always",
                "deny",
                "cancelled",
                "invalid",
                "error",
                "control",
                "abandoned",
            ] {
                let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(4);
                let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                let control = Arc::new(TurnControl::new());
                let bridge_control = control.clone();
                let agent = Agent
                    .builder()
                    .on_receive_request(
                        async |_: InitializeRequest, responder, _cx| {
                            responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                        },
                        on_receive_request!(),
                    )
                    .with_spawned(move |cx| async move {
                        drive_permission_bridge(
                            cx,
                            idle::Activity::default(),
                            SessionId::new("permission-test"),
                            ask_rx,
                            bridge_control,
                            None,
                            #[cfg(feature = "goal")]
                            Arc::new(std::sync::atomic::AtomicU64::new(0)),
                        )
                        .await;
                        let _ = done_tx.send(());
                        Ok(())
                    });
                let (notification_tx, mut notification_rx) = tokio::sync::mpsc::unbounded_channel();
                let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
                let connection = Client
                    .builder()
                    .on_receive_notification(
                        move |notification: SessionNotification, _cx| {
                            let _ = notification_tx.send(notification);
                            async { Ok(()) }
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    .on_receive_request(
                        move |request: RequestPermissionRequest, responder, _cx| {
                            let _ = request_tx.send((request, responder));
                            async { Ok(()) }
                        },
                        on_receive_request!(),
                    )
                    .connect_with(agent, async move |cx| {
                        let send_ask = |name: &str| {
                            let (reply, receiver) = tokio::sync::oneshot::channel();
                            let ask = AskRequest {
                                diagnostic: None,
                                #[cfg(feature = "subagents")]
                                background: None,
                                agent_name: None,
                                tool: "shell".into(),
                                input: name.to_owned(),
                                tool_call_id: (!synthetic).then(|| name.to_owned()),
                                suggested_pattern: Some("approved/**".into()),
                                additional_allow_patterns: Vec::new(),
                                reply,
                            };
                            (ask, receiver)
                        };
                        let (ask, reply) = send_ask("first");
                        ask_tx.send(ask.into()).await.unwrap();
                        let (request, responder) = request_rx.recv().await.unwrap();
                        let mut ids = vec![request.tool_call.tool_call_id.clone()];
                        let mut decisions = Vec::new();
                        if case == "abandoned" {
                            drop(reply);
                            let (next, next_reply) = send_ask("next");
                            ask_tx.send(next.into()).await.unwrap();
                            // Keep the first client response pending. Only caller
                            // abandonment can let the bridge reach this request.
                            let (next_request, next_responder) = request_rx.recv().await.unwrap();
                            ids.push(next_request.tool_call.tool_call_id);
                            next_responder.respond(RequestPermissionResponse::new(
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                    "allow_once",
                                )),
                            ))?;
                            decisions.push(next_reply.await.unwrap());
                            drop(responder);
                        } else if case == "control" {
                            let (buffered, buffered_reply) = send_ask("buffered");
                            ask_tx.send(buffered.into()).await.unwrap();
                            control.cancel();
                            decisions.push(reply.await.unwrap());
                            decisions.push(buffered_reply.await.unwrap());
                            drop(responder);
                        } else {
                            if case == "error" {
                                responder.respond_with_error(agent_client_protocol::Error::new(
                                    -32603,
                                    "client refusal",
                                ))?;
                            } else {
                                let outcome = match case {
                                    "cancelled" => RequestPermissionOutcome::Cancelled,
                                    _ => RequestPermissionOutcome::Selected(
                                        SelectedPermissionOutcome::new(match case {
                                            "once" => "allow_once",
                                            "always" => "allow_always",
                                            "deny" => "deny",
                                            _ => "unknown-option",
                                        }),
                                    ),
                                };
                                responder.respond(RequestPermissionResponse::new(outcome))?;
                            }
                            decisions.push(reply.await.unwrap());
                        }
                        drop(ask_tx);
                        done_rx.await.unwrap();
                        // An RPC round trip fences the already-enqueued terminal
                        // notifications before inspecting the client-side stream.
                        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                            .block_task()
                            .await?;
                        let mut updates = Vec::new();
                        while let Ok(notification) = notification_rx.try_recv() {
                            updates.push(notification.update);
                        }
                        Ok((ids, decisions, updates))
                    });
                let observed =
                    tokio::time::timeout(std::time::Duration::from_secs(5), connection).await;
                let Ok(Ok((ids, decisions, updates))) = observed else {
                    failures.push(format!(
                        "{case} synthetic={synthetic}: connection did not settle: {observed:?}"
                    ));
                    continue;
                };
                let decision_kinds: Vec<_> = decisions
                    .iter()
                    .map(|decision| match decision {
                        UserDecision::AllowOnce => "once".to_owned(),
                        UserDecision::AllowAlways(pattern) => format!("always:{pattern}"),
                        UserDecision::Deny => "deny".to_owned(),
                    })
                    .collect();
                let expected_decisions = match case {
                    "once" | "abandoned" => vec!["once"],
                    "always" => vec!["always:approved/**"],
                    "control" => vec!["deny", "deny"],
                    _ => vec!["deny"],
                };
                if decision_kinds != expected_decisions {
                    failures.push(format!(
                        "{case} synthetic={synthetic}: decisions {decision_kinds:?}"
                    ));
                }
                if !synthetic {
                    if !updates.is_empty() || ids[0].0.as_ref() != "first" {
                        failures.push(format!(
                            "{case}: bridge changed an ordinary tool lifecycle: {updates:?}"
                        ));
                    }
                    continue;
                }
                let announced: Vec<_> = updates
                    .iter()
                    .filter_map(|update| match update {
                        SessionUpdate::ToolCall(call) => Some(call),
                        _ => None,
                    })
                    .collect();
                let terminal: Vec<_> = updates
                    .iter()
                    .filter_map(|update| match update {
                        SessionUpdate::ToolCallUpdate(update) => Some(update),
                        _ => None,
                    })
                    .collect();
                if announced.len() != ids.len() || terminal.len() != ids.len() {
                    failures.push(format!(
                        "{case}: expected {} announcements and terminal updates, got {}/{}",
                        ids.len(),
                        announced.len(),
                        terminal.len()
                    ));
                    continue;
                }
                for (index, id) in ids.iter().enumerate() {
                    let approved =
                        matches!(case, "once" | "always") || (case == "abandoned" && index == 1);
                    let expected = if approved {
                        ToolCallStatus::Completed
                    } else {
                        ToolCallStatus::Failed
                    };
                    if announced[index].tool_call_id != *id
                        || terminal[index].tool_call_id != *id
                        || terminal[index].fields.status != Some(expected)
                    {
                        failures.push(format!(
                            "{case}: mismatched synthetic lifecycle {updates:?}"
                        ));
                    }
                    let notice = if approved {
                        "Permission approved"
                    } else if matches!(case, "control" | "cancelled" | "abandoned") {
                        "Permission request cancelled"
                    } else if case == "error" {
                        "Permission request failed"
                    } else {
                        "Permission denied"
                    };
                    let content = vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(notice),
                    ))];
                    if announced[index].title != "Permission: shell"
                        || terminal[index].fields.content.as_ref() != Some(&content)
                    {
                        failures.push(format!("{case}: synthetic result must describe an approval, not completed tool work"));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[tokio::test]
    async fn acp53_cancelled_relay_publishes_actual_store_after_owned_drain() {
        use rig::tool::Tool;

        struct CancelledRelay {
            store: crate::agent::tools::TodoStore,
            rx: tokio::sync::mpsc::Receiver<AgentEvent>,
            entered: Arc<Notify>,
        }
        impl ConnectTo<Client> for CancelledRelay {
            async fn connect_to(
                self,
                client: impl ConnectTo<Agent>,
            ) -> Result<(), agent_client_protocol::Error> {
                let rx = Arc::new(StdMutex::new(Some(self.rx)));
                Agent
                    .builder()
                    .on_receive_request(
                        move |req: PromptRequest, responder: Responder<PromptResponse>, cx| {
                            let rx = rx.lock().unwrap().take().unwrap();
                            let store = self.store.clone();
                            let entered = self.entered.clone();
                            async move {
                                let control = Arc::new(TurnControl::new());
                                control.cancel();
                                entered.notify_one();
                                let outcome = relay_prompt_events(
                                    req.session_id,
                                    cx,
                                    control,
                                    rx,
                                    store,
                                    #[cfg(feature = "goal")]
                                    (&Config::default(), "openai"),
                                )
                                .await;
                                responder.respond(PromptResponse::new(outcome.reason))
                            }
                        },
                        on_receive_request!(),
                    )
                    .connect_to(client)
                    .await
            }
        }
        let store = crate::agent::tools::TodoStore::default();
        let entered = Arc::new(Notify::new());
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let updates = Arc::new(StdMutex::new(Vec::<SessionNotification>::new()));
        let sink = updates.clone();
        let connection = Client.builder().on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                sink.lock().unwrap().push(notification);
                Ok(())
            }, agent_client_protocol::on_receive_notification!(),
        ).connect_with(CancelledRelay { store: store.clone(), rx, entered: entered.clone() }, async move |cx| {
            let session = SessionId::new("cancelled-plan");
            let response = cx.send_request(prompt(session.clone(), "cancelled")).block_task();
            tokio::pin!(response);
            tokio::select! {
                _ = entered.notified() => {}
                outcome = &mut response => panic!("cancelled relay must still drain: {outcome:?}"),
            }
            assert!(updates.lock().unwrap().is_empty());
            let output = crate::agent::tools::todo::WriteTodoList::new_with_store(None, None, store.clone())
                .call(crate::agent::tools::todo::TodoWriteArgs { todos: vec![crate::agent::tools::todo::TodoItem {
                    content: "actual write before drained result".to_owned(),
                    status: "in_progress".into(), priority: "high".into(),
                }] }).await.unwrap();
            tx.send(AgentEvent::ToolResult { id: "cancelled-todo".into(), name: "todo_write".into(),
                output: output.into(), is_error: false, images: Vec::new() }).await.unwrap();
            tx.send(done("settled", vec![Message::assistant("settled")])).await.unwrap();
            drop(tx);
            assert_eq!(response.await?.stop_reason, StopReason::Cancelled);
            // The terminal response fences its prior queued final Plan update.
            let updates = updates.lock().unwrap();
            assert_eq!(updates.len(), 1, "{updates:?}");
            assert_eq!(updates[0].session_id, session);
            let SessionUpdate::Plan(plan) = &updates[0].update else { panic!("expected final plan") };
            assert_eq!(plan.entries.len(), 1);
            assert_eq!(plan.entries[0].content, store.snapshot()[0].content);
            assert_eq!(plan.entries[0].status, PlanEntryStatus::InProgress);
            Ok(())
        });
        tokio::time::timeout(Duration::from_secs(5), connection)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn acp53_actual_todo_tool_relays_store_changes_clears_and_session_isolation() {
        #[cfg(feature = "hooks")]
        let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
        let stores = Arc::new(StdMutex::new(HashMap::<
            String,
            crate::agent::tools::TodoStore,
        >::new()));
        let fixture: RunnerFixture = {
            let stores = stores.clone();
            Arc::new(move |prompt, history| {
                let store = stores.lock().unwrap().get(&prompt).unwrap().clone();
                Box::pin(async move {
                    let todos = match prompt.as_str() {
                        "clear" => serde_json::json!([]),
                        "denied" => serde_json::json!([{
                            "content": "FORGED_DENIED_PLAN", "status": "pending", "priority": "high"
                        }]),
                        "invalid" => serde_json::json!([{
                            "content": "FORGED_INVALID_PLAN", "status": "invalid", "priority": "high"
                        }]),
                        "isolated" => serde_json::json!([{
                            "content": "SECOND_SESSION_ONLY", "status": "pending", "priority": "medium"
                        }]),
                        _ => serde_json::json!([
                            {"content":"inspect", "status":"pending", "priority":"high"},
                            {"content":"implement", "status":"in_progress", "priority":"medium"},
                            {"content":"verify", "status":"completed", "priority":"low"},
                            {"content":"abandoned", "status":"cancelled", "priority":"low"}
                        ]),
                    };
                    let permission = (prompt == "denied").then(|| {
                        let mut checker = crate::permission::checker::PermissionChecker::new(
                            &crate::permission::PermissionConfigs::default(),
                            crate::permission::SecurityMode::Yolo,
                            None,
                            None,
                        )
                        .unwrap();
                        checker.add_session_exact_deny("todo_write", "");
                        Arc::new(StdMutex::new(checker))
                    });
                    let model = rig::test_utils::MockCompletionModel::from_stream_turns(vec![
                        vec![
                            rig::test_utils::MockStreamEvent::tool_call(
                                "todo-call",
                                "todo_write",
                                serde_json::json!({"todos": todos}),
                            ),
                            rig::test_utils::MockStreamEvent::final_response_with_default_usage(),
                        ],
                        vec![
                            rig::test_utils::MockStreamEvent::text("settled"),
                            rig::test_utils::MockStreamEvent::final_response_with_default_usage(),
                        ],
                    ]);
                    let agent = rig::agent::AgentBuilder::new(model)
                        .tool(crate::agent::tools::todo::WriteTodoList::new_with_store(
                            permission, None, store,
                        ))
                        .add_hook(crate::agent::runner::ToolLoopGuard)
                        .default_max_turns(2)
                        .build();
                    crate::agent::runner::spawn_agent_paused(
                        agent,
                        prompt,
                        history,
                        crate::retry::RetryConfig::default(),
                        None,
                        #[cfg(feature = "skills")]
                        None,
                        #[cfg(feature = "hooks")]
                        None,
                    )
                })
            })
        };
        let state = runner_fixture_state(fixture);
        let inspect_state = state.clone();
        let notifications = Arc::new(StdMutex::new(Vec::<SessionNotification>::new()));
        let sink = notifications.clone();
        let workspace = ProtocolTempDir::new();
        let ledger_root = crate::tests::OwnedTestDirectory::new("acp53-plan-owned-ledger");
        let ledger = Arc::new(crate::spend::Ledger::new(ledger_root.path()));
        let cwd = workspace.path().to_path_buf();
        let connection = Client.builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    sink.lock().unwrap().push(notification);
                    Ok(())
                }, agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                let first = cx.send_request(NewSessionRequest::new(cwd.clone())).block_task().await?.session_id;
                let second = cx.send_request(NewSessionRequest::new(cwd)).block_task().await?.session_id;
                let (first_store, second_store) = {
                    let mut sessions = inspect_state.sessions.lock().await;
                    // Inject a real owned ledger and known zero prices for the
                    // mock provider's actual SDK request account. No ambient
                    // path override or production admission bypass is needed.
                    for id in [&first, &second] {
                        sessions.get_mut(id).unwrap().spend = crate::spend::run::Run::fixture(
                            crate::spend::Account::new(&id.to_string(), None, 0.0).unwrap(),
                            ledger.clone(),
                            &id.to_string(),
                            crate::spend::PriceSnapshot::capture(Some((0.0, 0.0)), Default::default(), false),
                        );
                    }
                    (sessions[&first].todo_store.clone(), sessions[&second].todo_store.clone())
                };
                for case in ["update", "denied", "invalid", "clear"] {
                    stores.lock().unwrap().insert(case.to_owned(), first_store.clone());
                }
                stores.lock().unwrap().insert("isolated".to_owned(), second_store.clone());
                for case in ["update", "denied", "invalid", "clear", "isolated"] {
                    let session = if case == "isolated" { &second } else { &first };
                    let outcome = cx.send_request(prompt(session.clone(), case)).block_task().await?;
                    assert_eq!(outcome.stop_reason, StopReason::EndTurn, "{case}");
                    // Fence queued updates through the actual connection before
                    // draining. All effects and the runner have settled above.
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                    let updates = std::mem::take(&mut *notifications.lock().unwrap());
                    let plans = updates.iter().filter_map(|notification| {
                        if let SessionUpdate::Plan(plan) = &notification.update {
                            assert_eq!(&notification.session_id, session);
                            Some(plan)
                        } else { None }
                    }).collect::<Vec<_>>();
                    match case {
                        "clear" => {
                            assert_eq!(plans.len(), 2);
                            assert_eq!(plans[0].entries.len(), 4);
                            assert!(plans[1].entries.is_empty());
                            assert!(first_store.is_empty());
                        }
                        "isolated" => {
                            assert_eq!(plans.len(), 1);
                            assert_eq!(plans[0].entries[0].content, "SECOND_SESSION_ONLY");
                            assert!(first_store.is_empty());
                            assert_eq!(second_store.snapshot().len(), 1);
                        }
                        _ => {
                            assert_eq!(plans.len(), 1, "{case}: {plans:?}; actual updates: {updates:?}; actual store: {:?}", first_store.snapshot());
                            let entries = &plans[0].entries;
                            assert_eq!(entries.iter().map(|entry| entry.content.as_str()).collect::<Vec<_>>(),
                                ["inspect", "implement", "verify", "abandoned"]);
                            assert_eq!(entries[0].priority, PlanEntryPriority::High);
                            assert_eq!(entries[1].priority, PlanEntryPriority::Medium);
                            assert_eq!(entries[2].priority, PlanEntryPriority::Low);
                            assert_eq!(entries[0].status, PlanEntryStatus::Pending);
                            assert_eq!(entries[1].status, PlanEntryStatus::InProgress);
                            assert_eq!(entries[2].status, PlanEntryStatus::Completed);
                            assert_eq!(entries[3].status, PlanEntryStatus::Pending);
                            assert_eq!(entries[3].meta.as_ref().unwrap()["mini-agent-status"], "cancelled");
                            assert_eq!(first_store.snapshot().len(), 4);
                            assert!(second_store.is_empty());
                            if case != "update" {
                                assert!(updates.iter().any(|notification| matches!(&notification.update,
                                    SessionUpdate::ToolCallUpdate(update) if update.fields.status == Some(ToolCallStatus::Failed))),
                                    "actual typed tool failure must remain failed: {updates:?}");
                                let expected = if case == "denied" {
                                    "Permission denied"
                                } else {
                                    "todo_write rejected"
                                };
                                assert!(updates.iter().any(|notification| matches!(&notification.update,
                                    SessionUpdate::ToolCallUpdate(update) if serde_json::to_string(&update.fields.content).unwrap().contains(expected))),
                                    "{case} must reach the actual tool refusal: {updates:?}");
                            }
                        }
                    }
                }
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(10), connection)
            .await
            .unwrap()
            .unwrap();
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
                                id: "lifecycle-a".into(),
                                name: "read".into(),
                                args: serde_json::json!({"path": "a"}),
                            },
                            AgentEvent::ToolCall {
                                id: "lifecycle-b".into(),
                                name: "read".into(),
                                args: serde_json::json!({"path": "b"}),
                            },
                            #[cfg(feature = "subagents")]
                            AgentEvent::SubagentToolCall {
                                name: "nested-display-only".into(),
                                args: serde_json::json!({"not": "canonical"}),
                            },
                            AgentEvent::ToolResult {
                                id: "lifecycle-b".into(),
                                name: "read".into(),
                                output: "b-result".into(),
                                images: Vec::new(),
                                is_error: true,
                            },
                            AgentEvent::ToolResult {
                                id: "lifecycle-a".into(),
                                name: "read".into(),
                                output: "a-result".into(),
                                images: Vec::new(),
                                is_error: false,
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
        #[cfg(feature = "skills")]
        let state_for_skill_assertion = state.clone();
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
                assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
                assert!(initialized.agent_capabilities.load_session);
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .list
                        .is_some()
                );
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .close
                        .is_some(),
                    "clients must be told that session/close is supported"
                );

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
                #[cfg(feature = "skills")]
                {
                    let sessions = state_for_skill_assertion.sessions.lock().await;
                    let first_services = &sessions.get(&first).unwrap().skill_services;
                    let second_services = &sessions.get(&second).unwrap().skill_services;
                    assert!(
                        !Arc::ptr_eq(first_services, second_services),
                        "ACP sessions must not share mutable learned-skill turn state"
                    );
                    assert_eq!(first_services.initialization_attempts(), 0);
                    assert_eq!(second_services.initialization_attempts(), 0);
                }

                cx.send_request(prompt(first.clone(), "tools"))
                    .block_task()
                    .await?;
                cx.send_request(prompt(first.clone(), "second"))
                    .block_task()
                    .await?;
                let failed = cx
                    .send_request(prompt(first.clone(), "fail"))
                    .block_task()
                    .await;
                assert!(
                    failed.is_err(),
                    "an internal failure is a JSON-RPC error, not a refusal: {failed:?}"
                );
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
                    "load must not replace an identity whose existing runtime lease is still owned"
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
        let statuses = notifications
            .iter()
            .filter_map(|notification| match &notification.update {
                SessionUpdate::ToolCallUpdate(update) => {
                    Some((update.tool_call_id.to_string(), update.fields.status))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                ("lifecycle-b".to_string(), Some(ToolCallStatus::Failed)),
                ("lifecycle-a".to_string(), Some(ToolCallStatus::Completed)),
            ],
            "a failed tool result must be reported as failed"
        );
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

    #[cfg(feature = "goal")]
    #[tokio::test]
    async fn rejected_prompt_does_not_touch_the_active_turns_goal() {
        let blocked_started = Arc::new(tokio::sync::Notify::new());
        let fixture: PromptFixture = {
            let blocked_started = blocked_started.clone();
            Arc::new(move |prompt, _history| {
                let blocked_started = blocked_started.clone();
                Box::pin(async move {
                    if prompt == "blocked" {
                        blocked_started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant(prompt.clone())],
                    )])
                })
            })
        };
        let state = fixture_state(fixture);
        let observed = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let with_goal = |session: SessionId, text: &str, goal: serde_json::Value| {
            let mut request = prompt(session, text);
            let mut meta = serde_json::Map::new();
            meta.insert("goal".to_string(), goal);
            request.meta = Some(meta);
            request
        };

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
                let first = with_goal(
                    session.clone(),
                    "blocked",
                    serde_json::json!({"objective": "keep this objective"}),
                );
                let blocked =
                    tokio::spawn(async move { first_cx.send_request(first).block_task().await });
                tokio::time::timeout(Duration::from_secs(1), blocked_started.notified())
                    .await
                    .expect("the first turn should start");

                for goal in [
                    serde_json::json!({"clear": true}),
                    serde_json::json!({"objective": "hijack", "replace": true}),
                ] {
                    assert!(
                        cx.send_request(with_goal(session.clone(), "concurrent", goal))
                            .block_task()
                            .await
                            .is_err(),
                        "a concurrent prompt must be rejected"
                    );
                    let objective = observed
                        .sessions
                        .lock()
                        .await
                        .get(&session)
                        .and_then(|s| s.goal_store.snapshot())
                        .map(|goal| goal.objective.clone());
                    assert_eq!(
                        objective.as_deref(),
                        Some("keep this objective"),
                        "a rejected prompt must not change the active turn's goal"
                    );
                }

                cx.send_notification(CancelNotification::new(session))?;
                let blocked = tokio::time::timeout(Duration::from_secs(5), blocked)
                    .await
                    .expect("cancellation must release the active turn")
                    .unwrap()?;
                assert_eq!(blocked.stop_reason, StopReason::Cancelled);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hooked_server_runs_concurrent_turns_in_different_workspaces() {
        let blocked_started = Arc::new(tokio::sync::Notify::new());
        let fixture: PromptFixture = {
            let blocked_started = blocked_started.clone();
            Arc::new(move |prompt, _history| {
                let blocked_started = blocked_started.clone();
                Box::pin(async move {
                    if prompt == "blocked" {
                        blocked_started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant(prompt.clone())],
                    )])
                })
            })
        };
        let mut state = fixture_state(fixture);
        let published = Arc::new(StdMutex::new(Vec::<Option<std::path::PathBuf>>::new()));
        let recorder = published.clone();
        Arc::get_mut(&mut state).unwrap().hook_roots =
            Some(Arc::new(HookRootClaims::with_publisher(move |root| {
                lock_unpoisoned(&recorder).push(root.map(Path::to_path_buf));
            })));
        let repo_a = ProtocolTempDir::new();
        let repo_b = ProtocolTempDir::new();
        let (cwd_a, cwd_b) = (repo_a.path().to_path_buf(), repo_b.path().to_path_buf());

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
                let a = cx
                    .send_request(NewSessionRequest::new(cwd_a.clone()))
                    .block_task()
                    .await?
                    .session_id;
                let a_again = cx
                    .send_request(NewSessionRequest::new(cwd_a))
                    .block_task()
                    .await?
                    .session_id;
                let b = cx
                    .send_request(NewSessionRequest::new(cwd_b))
                    .block_task()
                    .await?
                    .session_id;

                let blocked_cx = cx.clone();
                let blocked_id = a.clone();
                let blocked = tokio::spawn(async move {
                    blocked_cx
                        .send_request(prompt(blocked_id, "blocked"))
                        .block_task()
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), blocked_started.notified())
                    .await
                    .expect("the first turn should start");

                // Each turn dispatches hooks against its own workspace, so a
                // prompt in another repository runs concurrently (mini-agent-su92b).
                let other_repo = cx
                    .send_request(prompt(b.clone(), "other-repo"))
                    .block_task()
                    .await?;
                assert_eq!(other_repo.stop_reason, StopReason::EndTurn);
                let same_repo = cx
                    .send_request(prompt(a_again, "same-repo"))
                    .block_task()
                    .await?;
                assert_eq!(same_repo.stop_reason, StopReason::EndTurn);

                cx.send_notification(CancelNotification::new(a))?;
                tokio::time::timeout(Duration::from_secs(5), blocked)
                    .await
                    .expect("cancellation must release the active turn")
                    .unwrap()?;
                let after = cx
                    .send_request(prompt(b, "other-repo"))
                    .block_task()
                    .await?;
                assert_eq!(after.stop_reason, StopReason::EndTurn);
                Ok(())
            })
            .await
            .unwrap();
        // While both workspaces were active the shared fallback root was
        // disabled rather than pointing at either repository.
        assert!(
            lock_unpoisoned(&published).contains(&None),
            "{:?}",
            lock_unpoisoned(&published)
        );
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn client_stdio_mcp_servers_become_session_command_servers() {
        use crate::extras::mcp::config::{McpServerConfig, McpStdioNetwork};
        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        let mut state = fixture_state(fixture);
        Arc::get_mut(&mut state).unwrap().client_mcp = ClientMcpPolicy::Trusted;
        let command = std::env::temp_dir().join("client-mcp-server");
        let servers = vec![McpServer::Stdio(
            McpServerStdio::new("editor-tools", command.clone())
                .args(vec!["--stdio".to_string()])
                .env(vec![EnvVariable::new("TOKEN", "from-editor")]),
        )];

        let accepted = client_mcp_servers(&state, &servers).unwrap();
        let Some(McpServerConfig::Command {
            command: launched,
            args,
            env,
            inherit_env,
            sandbox,
            network,
            cwd,
            ..
        }) = accepted.get("editor-tools")
        else {
            panic!("a client stdio server must become a command server: {accepted:?}");
        };
        assert_eq!(launched, &command.display().to_string());
        assert_eq!(args, &vec!["--stdio".to_string()]);
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("from-editor"));
        assert_eq!(inherit_env, &vec!["PATH".to_string(), "HOME".to_string()]);
        assert!(sandbox.is_none() && cwd.is_none());
        assert_eq!(*network, McpStdioNetwork::Inherit);

        let configured = Config {
            mcp_servers: Some(HashMap::from([(
                "editor-tools".to_string(),
                McpServerConfig::Url {
                    allow_local: false,
                    url: "https://configured.example/mcp".to_string(),
                    defer: Default::default(),
                    headers: HashMap::new(),
                    oauth: None,
                },
            )])),
            ..Config::default()
        };
        let merged = with_client_mcp_servers(&configured, &accepted);
        assert!(
            matches!(
                merged.mcp_servers.as_ref().unwrap().get("editor-tools"),
                Some(McpServerConfig::Url { .. })
            ),
            "a configured server keeps its name"
        );
        let unconfigured = Config::default();
        let merged = with_client_mcp_servers(&unconfigured, &accepted);
        assert!(matches!(
            merged.mcp_servers.as_ref().unwrap().get("editor-tools"),
            Some(McpServerConfig::Command { .. })
        ));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn client_stdio_mcp_server_count_is_bounded_before_prompt_connection() {
        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        let mut state = fixture_state(fixture);
        Arc::get_mut(&mut state).unwrap().client_mcp = ClientMcpPolicy::Trusted;
        let servers = (0..=MAX_ACP_CLIENT_MCP_SERVERS)
            .map(|index| {
                McpServer::Stdio(McpServerStdio::new(
                    format!("editor-{index}"),
                    std::env::temp_dir().join("client-mcp-server"),
                ))
            })
            .collect::<Vec<_>>();

        let accepted = client_mcp_servers(&state, &servers[..MAX_ACP_CLIENT_MCP_SERVERS])
            .expect("eight client servers fit the admission bound");
        assert_eq!(accepted.len(), MAX_ACP_CLIENT_MCP_SERVERS);
        let error = client_mcp_servers(&state, &servers)
            .expect_err("the ninth editor-supplied process must be refused");
        assert!(error.to_string().contains("at most 8"), "{error}");
    }

    /// Client-supplied servers may come from repository-controlled editor
    /// settings, so by default they run in the workspace-service sandbox and
    /// are refused when it is unavailable (mini-agent-e6rne).
    #[cfg(feature = "mcp")]
    #[test]
    fn client_stdio_mcp_servers_are_sandboxed_by_default() {
        use crate::extras::mcp::config::{McpServerConfig, McpStdioNetwork};
        assert!(matches!(
            client_mcp_policy(false, false),
            ClientMcpPolicy::Sandboxed
        ));
        assert!(matches!(
            client_mcp_policy(false, true),
            ClientMcpPolicy::Trusted
        ));
        assert!(matches!(
            client_mcp_policy(true, true),
            ClientMcpPolicy::Refused
        ));

        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        let state = fixture_state(fixture);
        let servers = vec![McpServer::Stdio(McpServerStdio::new(
            "editor-tools",
            std::env::temp_dir().join("client-mcp-server"),
        ))];
        let backend = state.cli.resolve_sandbox_backend(&state.cfg);
        let available = matches!(
            crate::sandbox::Sandbox::new(true, &backend).policy(),
            crate::sandbox::SandboxPolicy::RequiredAndAvailable
        );
        match client_mcp_servers(&state, &servers) {
            Ok(accepted) => {
                assert!(available, "accepted without an available sandbox");
                let Some(McpServerConfig::Command {
                    sandbox, network, ..
                }) = accepted.get("editor-tools")
                else {
                    panic!("expected a command server: {accepted:?}");
                };
                assert_eq!(sandbox.as_deref(), Some(backend.as_str()));
                assert_eq!(*network, McpStdioNetwork::Deny);
            }
            Err(error) => {
                assert!(!available, "refused although the sandbox is available");
                let message = error.to_string();
                assert!(message.contains(ACP_TRUST_CLIENT_MCP_ENV), "{message}");
            }
        }

        // An unknown backend is never available, so the servers are refused
        // rather than launched unsandboxed.
        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        let mut missing = fixture_state(fixture);
        Arc::get_mut(&mut missing).unwrap().cfg.sandbox_backend =
            Some("no-such-backend".to_string());
        let error = client_mcp_servers(&missing, &servers)
            .expect_err("an unavailable sandbox must refuse client servers");
        assert!(
            error.to_string().contains(ACP_TRUST_CLIENT_MCP_ENV),
            "{error}"
        );

        // A selected backend with no workspace-service network-denial arm is
        // refused during session/new, before a tool-enabled prompt can start.
        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        let mut no_network_gate = fixture_state(fixture);
        Arc::get_mut(&mut no_network_gate)
            .unwrap()
            .cfg
            .sandbox_backend = Some("zerobox".to_string());
        let error = client_mcp_servers(&no_network_gate, &servers)
            .expect_err("zerobox cannot enforce client MCP network denial");
        assert!(error.to_string().contains("cannot enforce"), "{error}");
    }

    /// Exercise the complete editor server configuration -> MCP launch path.
    /// The same helper connects to an owned loopback listener when explicitly
    /// trusted and is blocked by the default workspace-service sandbox.
    #[cfg(all(feature = "mcp", target_os = "macos"))]
    #[tokio::test]
    async fn client_stdio_mcp_network_denial_survives_native_launch() {
        const PORT: &str = "MINI_AGENT_ACP_MCP_NETWORK_PORT";
        const MARKER: &str = "MINI_AGENT_ACP_MCP_NETWORK_MARKER";
        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        let state = fixture_state(fixture.clone());
        let backend = state.cli.resolve_sandbox_backend(&state.cfg);
        if !matches!(
            crate::sandbox::Sandbox::new(true, &backend).policy(),
            crate::sandbox::SandboxPolicy::RequiredAndAvailable
        ) || backend == "zerobox"
        {
            return; // Platform has no enforceable workspace-service backend.
        }

        let workspace = ProtocolTempDir::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = "if /usr/bin/nc -z -w 1 127.0.0.1 \"$MINI_AGENT_ACP_MCP_NETWORK_PORT\"; \
            then printf connected > \"$MINI_AGENT_ACP_MCP_NETWORK_MARKER\"; \
            else printf blocked > \"$MINI_AGENT_ACP_MCP_NETWORK_MARKER\"; fi";
        for (policy, expected) in [
            (ClientMcpPolicy::Trusted, "connected"),
            (ClientMcpPolicy::Sandboxed, "blocked"),
        ] {
            let marker = workspace.path().join(format!("{expected}.txt"));
            let mut policy_state = fixture_state(fixture.clone());
            Arc::get_mut(&mut policy_state).unwrap().client_mcp = policy;
            let server = McpServer::Stdio(
                McpServerStdio::new("network-probe", std::path::PathBuf::from("/bin/sh"))
                    .args(vec!["-c".to_string(), script.to_string()])
                    .env(vec![
                        EnvVariable::new(PORT, port.to_string()),
                        EnvVariable::new(MARKER, marker.display().to_string()),
                    ]),
            );
            let accepted = client_mcp_servers(&policy_state, &[server]).unwrap();
            let config = accepted.get("network-probe").unwrap();
            let launch = crate::extras::mcp::client::McpClientHandle::connect_with_timeout_in(
                compact_str::CompactString::new("network-probe"),
                config,
                Duration::from_secs(5),
                workspace.path(),
            )
            .await;
            let launch_error = launch.err().map(|error| error.to_string());
            assert_eq!(
                std::fs::read_to_string(&marker).unwrap_or_default(),
                expected,
                "ACP {policy:?} helper must report {expected} through {backend}; launch: {launch_error:?}"
            );
        }
    }

    #[test]
    fn unsupported_client_mcp_servers_are_refused_not_ignored() {
        let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
        let state = fixture_state(fixture);
        let absolute = std::env::temp_dir().join("client-mcp-server");
        let refused = |state: &AcpState, server: McpServer| {
            client_mcp_servers(state, &[server])
                .err()
                .map(|e| e.to_string())
        };

        assert!(
            refused(
                &state,
                McpServer::Http(McpServerHttp::new("remote", "https://example.com/mcp"))
            )
            .is_some(),
            "HTTP servers are not advertised"
        );
        #[cfg(feature = "mcp")]
        {
            assert!(
                refused(
                    &state,
                    McpServer::Stdio(McpServerStdio::new("relative", "bin/server"))
                )
                .is_some()
            );
            let duplicate = McpServer::Stdio(McpServerStdio::new("same", absolute.clone()));
            assert!(client_mcp_servers(&state, &[duplicate.clone(), duplicate]).is_err());

            let fixture: PromptFixture = Arc::new(|_, _| Box::pin(async { Ok(Vec::new()) }));
            let mut tcp = fixture_state(fixture);
            Arc::get_mut(&mut tcp).unwrap().client_mcp = ClientMcpPolicy::Refused;
            let error = refused(
                &tcp,
                McpServer::Stdio(McpServerStdio::new("local", absolute.clone())),
            )
            .expect("a TCP peer must not launch local processes");
            assert!(error.contains("TCP"), "{error}");
        }
        #[cfg(not(feature = "mcp"))]
        assert!(
            refused(
                &state,
                McpServer::Stdio(McpServerStdio::new("local", absolute))
            )
            .is_some()
        );
        assert!(client_mcp_servers(&state, &[]).is_ok());
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
                                                id: "blocking-tool".into(),
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
                        crate::agent::runner::AgentRunner::without_compaction(
                            event_rx,
                            join.abort_handle(),
                        ),
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

    /// Budget for each readiness stage of the production Bash cancellation
    /// fixture. Each stage is timed from the end of the previous one, so load
    /// that delays one stage is never charged against the next.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const PROCESS_TREE_STAGE_BUDGET: Duration = Duration::from_secs(30);

    /// How one readiness stage of the Bash cancellation fixture ended.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    enum ProcessTreeStage<T, E> {
        Ready(T),
        PromptEnded(E),
        Stalled,
    }

    /// Waits for `ready` within [`PROCESS_TREE_STAGE_BUDGET`], unless the
    /// prompt ends first. A stage that is ready when the prompt also ends wins.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn process_tree_stage<T, E>(
        ready: impl Future<Output = T>,
        prompt: &mut tokio::task::JoinHandle<E>,
    ) -> ProcessTreeStage<T, Result<E, tokio::task::JoinError>> {
        tokio::select! {
            biased;
            value = ready => ProcessTreeStage::Ready(value),
            ended = prompt => ProcessTreeStage::PromptEnded(ended),
            _ = tokio::time::sleep(PROCESS_TREE_STAGE_BUDGET) => ProcessTreeStage::Stalled,
        }
    }

    /// Polls `path` until it holds a complete, non-zero pid. A file can exist
    /// before its contents are flushed, so this waits for a parsable pid
    /// rather than for the path to appear.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn published_process_tree_pid(path: &Path) -> u32 {
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
                && let Ok(pid) = text.trim().parse::<u32>()
                && pid != 0
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn exercise_cancellation_process_tree(fail_after_readiness: bool) {
        use crate::tests::process_gate::ProcessGate;
        use crate::tests::process_state::{ProcessIdentity, ProcessState};
        use futures::FutureExt;
        // The production runner consults the process-global Stop dispatcher
        // after a model turn. Another test must not install a blocking hook
        // while this fixture is assessing Bash process-tree cancellation.
        #[cfg(feature = "hooks")]
        let _dispatcher_guard = crate::tests::fake_model::dispatcher_guard::acquire();
        let files = ProtocolTempDir::new();
        let shell_pid_file = files.path().join("shell.pid");
        let descendant_pid_file = files.path().join("descendant.pid");
        let release_path = files.path().join("release");
        let mut gate = ProcessGate::new(&release_path).unwrap();
        let scope = Arc::new(StdMutex::new(
            None::<crate::agent::runner::AgentWorkCancellation>,
        ));
        let fixture_scope = scope.clone();
        let command = format!(
            "printf '%s\\n' \"$$\" > {}; sh -c 'printf \"%s\\n\" \"$$\" > \"$1\"; IFS= read -r release < \"$2\"' sh {} {} & wait",
            shell_quote(&shell_pid_file),
            shell_quote(&descendant_pid_file),
            shell_quote(&release_path),
        );
        let fixture: RunnerFixture = Arc::new(move |prompt, history| {
            let command = command.clone();
            let fixture_scope = fixture_scope.clone();
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
                let paused = crate::agent::runner::spawn_agent_paused(
                    agent,
                    prompt,
                    history,
                    crate::retry::RetryConfig::default(),
                    None,
                    #[cfg(feature = "skills")]
                    None,
                    #[cfg(feature = "hooks")]
                    None,
                );
                *fixture_scope.lock().unwrap() = Some(paused.cancellation_handle());
                paused
            })
        });
        let state = runner_fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        // The agent announces the Bash call before the tool runs. Readiness is
        // staged on that announcement so model and agent latency under a loaded
        // parallel runner is not counted against the shell's own budget.
        let (announced_tx, announced_rx) = tokio::sync::watch::channel(false);
        let tool_updates = Arc::new(StdMutex::new(Vec::<String>::new()));
        let reported_updates = Arc::clone(&tool_updates);

        Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    if let SessionUpdate::ToolCall(call) = &notification.update
                        && call.title
                            == <crate::agent::tools::bash::BashTool as rig::tool::Tool>::NAME
                    {
                        announced_tx.send_replace(true);
                    }
                    if let SessionUpdate::ToolCallUpdate(update) = &notification.update {
                        reported_updates.lock().unwrap().push(format!("{update:?}"));
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                let mut announced_rx = announced_rx;
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
                let mut blocked = Some(tokio::spawn(async move {
                    prompt_cx
                        .send_request(prompt(prompt_session, "run production bash"))
                        .block_task()
                        .await
                }));
                let mut observed_tree = None;
                let result = std::panic::AssertUnwindSafe(async {
                    // Readiness has three stages, each with its own budget and
                    // its own failure: the agent announces the Bash call, the
                    // spawned shell publishes its pid, and the shell's child
                    // publishes its pid. A prompt that ends early reports its
                    // own response instead of an anonymous timeout.
                    macro_rules! stage {
                        ($name:expr, $ready:expr) => {
                            match process_tree_stage($ready, blocked.as_mut().unwrap()).await {
                                ProcessTreeStage::Ready(value) => value,
                                ProcessTreeStage::PromptEnded(ended) => {
                                    // The handle has completed; drop it so
                                    // cleanup does not poll it again.
                                    blocked.take();
                                    panic!(
                                        "the prompt ended before {}: {ended:?}; tool updates: {:?}",
                                        $name,
                                        tool_updates.lock().unwrap()
                                    );
                                }
                                ProcessTreeStage::Stalled => panic!(
                                    "production BashTool stalled: {} did not happen within \
                                     {:?} (shell pid file present: {}, descendant pid file \
                                     present: {})",
                                    $name,
                                    PROCESS_TREE_STAGE_BUDGET,
                                    shell_pid_file.exists(),
                                    descendant_pid_file.exists(),
                                ),
                            }
                        };
                    }
                    stage!(
                        "the agent announced the Bash tool call",
                        announced_rx.wait_for(|announced| *announced)
                    )
                    .expect("the notification sender outlives the client");
                    let shell_pid = stage!(
                        "the Bash shell published its pid",
                        published_process_tree_pid(&shell_pid_file)
                    );
                    let descendant_pid = stage!(
                        "the Bash descendant published its pid",
                        published_process_tree_pid(&descendant_pid_file)
                    );

                    let shell = ProcessIdentity::capture(shell_pid).unwrap();
                    let descendant = ProcessIdentity::capture(descendant_pid).unwrap();
                    // Record the topology while the tree is still whole. A
                    // survivor is only interpretable against the process group
                    // it belonged to when the kill was sent: inside the shell's
                    // group means killpg should have reached it, and a
                    // different group means the descendant escaped the group
                    // the killer targets. The states alone cannot say which.
                    let shell_at_readiness = shell.state().unwrap();
                    let descendant_at_readiness = descendant.state().unwrap();
                    observed_tree = Some((shell, descendant));
                    if fail_after_readiness {
                        panic!("injected after Bash process-tree readiness");
                    }
                    cx.send_notification(CancelNotification::new(session.clone()))?;
                    let joined =
                        tokio::time::timeout(Duration::from_secs(15), blocked.as_mut().unwrap())
                            .await
                            .expect("Bash cancellation should settle the process tree");
                    blocked.take();
                    let response = joined.unwrap()?;
                    assert_eq!(response.stop_reason, StopReason::Cancelled);
                    let shell_state = shell.state().unwrap();
                    let descendant_state = descendant.state().unwrap();
                    assert!(
                        matches!(shell_state, ProcessState::Gone | ProcessState::Replaced),
                        "Cancelled before direct Bash child was reaped: {shell_state:?}"
                    );
                    assert!(
                        matches!(
                            descendant_state,
                            ProcessState::Exited | ProcessState::Gone | ProcessState::Replaced
                        ),
                        "Cancelled while Bash descendant remained live: {descendant_state:?}; \
                         at readiness shell {shell_pid} was {shell_at_readiness:?} and \
                         descendant {descendant_pid} was {descendant_at_readiness:?}"
                    );
                    Ok::<(), agent_client_protocol::Error>(())
                })
                .catch_unwind()
                .await;
                // Cleanup follows the observation, including when it failed.
                // The FIFO also lets a surviving fixture descendant exit without
                // signalling a PID that may have been recycled after cancellation.
                let released = gate.release();
                let _ = cx.send_notification(CancelNotification::new(session));
                let cancellation = scope.lock().unwrap().clone();
                if let Some(cancellation) = &cancellation {
                    cancellation.cancel();
                }
                if let Some(task) = blocked.take() {
                    task.abort();
                    let _ = task.await;
                }
                if let Some(cancellation) = cancellation {
                    tokio::time::timeout(
                        Duration::from_secs(15),
                        cancellation.wait_idle_for_test(),
                    )
                    .await
                    .expect("Bash fixture work did not settle");
                }
                released.unwrap();
                if let Some((shell, descendant)) = observed_tree {
                    // This wait is only fixture cleanup. The acceptance snapshot
                    // above was taken before release and is never retried.
                    tokio::time::timeout(Duration::from_secs(15), async {
                        while matches!(shell.state().unwrap(), ProcessState::Live { .. })
                            || matches!(descendant.state().unwrap(), ProcessState::Live { .. })
                        {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .expect("fixture process remained live after release");
                }
                match result {
                    Ok(result) => result,
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            })
            .await
            .unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn cancellation_reaps_production_bash_process_tree_before_responding() {
        exercise_cancellation_process_tree(false).await;
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn cancellation_process_tree_fixture_settles_after_readiness_failure() {
        use futures::FutureExt;
        let panic = std::panic::AssertUnwindSafe(exercise_cancellation_process_tree(true))
            .catch_unwind()
            .await
            .expect_err("injected fixture failure must propagate after cleanup");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"injected after Bash process-tree readiness")
        );
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
                            turns.lock().unwrap().queue.is_empty()
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
        // The production telemetry worker deliberately holds no scope guard while
        // it runs: `TelemetryDispatcher::drop` scopes the join instead, with a
        // bounded flush budget. So the builder's result owns a worker without
        // making the scope non-idle, and only the settlement assertions below
        // describe the contract. Asserting a live child here would re-assert the
        // test-only work guard that was removed with `current_work_guard`.
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

    #[cfg(all(feature = "hooks", any(target_os = "linux", target_os = "macos")))]
    #[tokio::test]
    async fn cancellation_reaps_configured_async_user_prompt_hook() {
        use crate::extras::hooks::dispatcher::HookDispatcher;
        use crate::extras::hooks::settings::{HookGroup, HookHandler, HookTrust, HooksConfig};
        use crate::tests::process_state::{ProcessIdentity, ProcessState};

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
                        "printf '%s\\n' \"$$\" > \"$1\"; while :; do sleep 1; done".to_owned(),
                        "configured-user-prompt-hook".to_owned(),
                        pid_file.display().to_string(),
                    ]),
                    timeout: Some(60),
                    is_async: true,
                    condition: None,
                    once: false,
                    trust: HookTrust::Trusted,
                    env: Default::default(),
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
        // These bounds are hang guards. The assertions below inspect live work
        // before cancellation and settled work afterward, not elapsed latency.
        let gate = tokio::time::timeout(
            Duration::from_secs(15),
            work_scope.run(crate::extras::hooks::gate_user_prompt(
                &dispatcher,
                &ctx,
                "configured hook prompt".to_owned(),
            )),
        )
        .await;
        let observed = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&pid_file)
                    && contents.ends_with('\n')
                    && let Ok(pid) = contents.trim().parse::<u32>()
                    && pid > 0
                {
                    break ProcessIdentity::capture(pid);
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let had_active_work = work_scope.active_children() > 0;

        // Settle the fixture before reporting dispatch/readiness failures.
        work_scope.cancellation_handle().cancel();
        let settled = tokio::time::timeout(Duration::from_secs(15), work_scope.wait_idle()).await;
        settled.expect("hook cancellation must kill and reap the configured subprocess");
        let gate = gate.expect("async prompt dispatch waited for its still-running hook");
        let identity = observed
            .expect("the configured UserPromptSubmit hook should start")
            .expect("capture live hook identity");
        assert!(matches!(gate, crate::extras::hooks::PromptGate::Proceed(_)));
        assert!(
            had_active_work,
            "dispatch must return while hook work is owned"
        );
        assert_eq!(work_scope.active_children(), 0);
        assert!(
            matches!(
                identity
                    .state()
                    .expect("observe configured hook after cleanup"),
                ProcessState::Gone | ProcessState::Replaced
            ),
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

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn mcp_stdio_teardown_precedes_terminal_response_and_session_settlement() {
        use crate::extras::mcp::{McpClientManager, client::McpClientHandle};
        use crate::tests::mcp_stdio_tests::{FixtureBuild, process_is_alive, wait_for_pid};

        let server = FixtureBuild::compile();
        for case in [
            "done",
            "error",
            "closed",
            "prepare-error",
            "compaction",
            "prepare-cancel",
            "cleanup-cancel",
        ] {
            let lease = server.lease(case);
            let handle = McpClientHandle::connect(
                case.into(),
                &server.config(
                    server.executable.display().to_string(),
                    Vec::new(),
                    "gated-eof",
                    &lease,
                ),
            )
            .await
            .unwrap();
            let pid = wait_for_pid(&lease).await;
            let preparing = Arc::new(Notify::new());
            let fixture: PromptFixture = {
                let preparing = preparing.clone();
                Arc::new(move |prompt, _| {
                    let preparing = preparing.clone();
                    Box::pin(async move {
                        if prompt == "next" {
                            return Ok(vec![done("next", vec![Message::assistant("next")])]);
                        }
                        match case {
                            "prepare-cancel" => {
                                preparing.notify_one();
                                std::future::pending().await
                            }
                            "prepare-error" => Err("preparation failed".to_string()),
                            // A clean turn keeps the session's servers connected, so
                            // teardown-time cancellation needs a turn that tears
                            // them down: one that ended in an error.
                            "error" | "cleanup-cancel" => Ok(vec![AgentEvent::Error {
                                message: "tool failed".into(),
                                interactions: vec![Message::assistant("effect recorded")],
                            }]),
                            "closed" => Ok(Vec::new()),
                            "compaction" => Ok(vec![AgentEvent::CompactionBoundary {
                                interactions: Vec::new(),
                            }]),
                            _ => Ok(vec![done(
                                "done",
                                vec![Message::assistant("effect recorded")],
                            )]),
                        }
                    })
                })
            };
            let state = fixture_state(fixture);
            *lock_unpoisoned(&state.mcp_fixture) =
                Some(McpClientManager::from_handles(vec![handle]));
            let workspace = ProtocolTempDir::new();
            let cwd = workspace.path().to_path_buf();
            let inspect_state = state.clone();
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
                    let request_cx = cx.clone();
                    let request_session = session.clone();
                    let response = tokio::spawn(async move {
                        request_cx
                            .send_request(prompt(request_session, case))
                            .block_task()
                            .await
                    });
                    if case == "done" {
                        // A clean turn leaves the session's servers connected
                        // for the next prompt; closing the session ends them.
                        let done = tokio::time::timeout(Duration::from_secs(2), response)
                            .await
                            .unwrap()
                            .unwrap()?;
                        assert_eq!(done.stop_reason, StopReason::EndTurn);
                        assert!(
                            process_is_alive(pid),
                            "a clean prompt must not tear the session's server down"
                        );
                        let closing = tokio::spawn({
                            let close_cx = cx.clone();
                            let session = session.clone();
                            async move {
                                close_cx
                                    .send_request(CloseSessionRequest::new(session))
                                    .block_task()
                                    .await
                            }
                        });
                        assert_eq!(wait_for_pid(&lease.with_extension("eof")).await, pid);
                        assert!(
                            process_is_alive(pid),
                            "close must wait for the server to exit before responding"
                        );
                        assert!(!closing.is_finished());
                        std::fs::write(lease.with_extension("release"), b"exit").unwrap();
                        tokio::time::timeout(Duration::from_secs(2), closing)
                            .await
                            .unwrap()
                            .unwrap()?;
                        assert!(!process_is_alive(pid), "session close ends the server");
                        return Ok(());
                    }
                    if case == "prepare-cancel" {
                        tokio::time::timeout(Duration::from_secs(2), preparing.notified())
                            .await
                            .unwrap();
                        assert!(receive_cancel(&inspect_state, &session).is_some());
                    }
                    // EOF proves that the real transport has entered graceful
                    // shutdown. The child cannot exit until the release file.
                    assert_eq!(wait_for_pid(&lease.with_extension("eof")).await, pid);
                    assert!(
                        process_is_alive(pid),
                        "{case}: fixture must still hold cleanup open"
                    );
                    let history = inspect_state
                        .sessions
                        .lock()
                        .await
                        .get(&session)
                        .unwrap()
                        .history
                        .clone();
                    assert!(
                        history.try_lock().is_err(),
                        "{case}: history released during shutdown"
                    );
                    assert!(
                        !response.is_finished(),
                        "{case}: response preceded server exit"
                    );
                    assert!(
                        cx.send_request(prompt(session.clone(), "overlapping"))
                            .block_task()
                            .await
                            .is_err(),
                        "{case}: generation settled during shutdown"
                    );
                    if case == "cleanup-cancel" {
                        assert!(
                            receive_cancel(&inspect_state, &session).is_some(),
                            "cancellation during teardown must still find the active generation"
                        );
                    }
                    std::fs::write(lease.with_extension("release"), b"exit").unwrap();
                    let response = tokio::time::timeout(Duration::from_secs(2), response)
                        .await
                        .unwrap()
                        .unwrap();
                    match case {
                        "prepare-cancel" | "cleanup-cancel" | "compaction" => {
                            assert_eq!(response?.stop_reason, StopReason::Cancelled, "{case}")
                        }
                        "done" => assert_eq!(response?.stop_reason, StopReason::EndTurn, "{case}"),
                        _ => assert!(
                            response.is_err(),
                            "{case}: internal failures are JSON-RPC errors"
                        ),
                    }
                    assert!(
                        !process_is_alive(pid),
                        "{case}: child outlived terminal response"
                    );
                    let expected_history = match case {
                        "done" | "error" | "cleanup-cancel" => {
                            vec![Message::user(case), Message::assistant("effect recorded")]
                        }
                        _ => Vec::new(),
                    };
                    assert_eq!(history.lock().await.snapshot(), expected_history, "{case}");
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
        server.cleanup();
    }

    #[cfg(all(feature = "mcp", unix))]
    #[tokio::test]
    async fn session_mcp_manager_is_reused_only_for_its_config_while_connected() {
        use crate::extras::mcp::{McpClientManager, client::McpClientHandle};
        use crate::tests::mcp_stdio_tests::{FixtureBuild, process_is_alive, wait_for_pid};

        let server = FixtureBuild::compile();
        let lease = server.lease("reuse");
        let server_config = server.config(
            server.executable.display().to_string(),
            Vec::new(),
            "gated-eof",
            &lease,
        );
        let handle = McpClientHandle::connect("reuse".into(), &server_config)
            .await
            .unwrap();
        let pid = wait_for_pid(&lease).await;
        let workspace = ProtocolTempDir::new();
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(workspace.path()).unwrap());
        let mut cfg = Config {
            mcp_servers: Some(HashMap::from([("reuse".to_owned(), server_config)])),
            ..Config::default()
        };
        let key = McpKey::new(&cfg, &binding);
        let mcp = PromptMcp {
            manager: McpClientManager::from_handles(vec![handle]),
            scope: crate::agent::runner::AgentWorkScope::new(),
            key: Some(McpKey::new(&cfg, &binding)),
        };
        assert!(mcp.is_reusable(&key));
        cfg.mcp_servers = None;
        assert!(
            !mcp.is_reusable(&McpKey::new(&cfg, &binding)),
            "a changed server set must reconnect"
        );
        let other = Arc::new(crate::paths::WorkspaceBinding::capture(workspace.path()).unwrap());
        assert!(
            !mcp.is_reusable(&McpKey::new(&Config::default(), &other)),
            "a different workspace binding must reconnect"
        );
        std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap();
        for _ in 0..200 {
            if !mcp.manager.all_connections_open() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!mcp.is_reusable(&key), "a dead server must reconnect");
        std::fs::write(lease.with_extension("release"), b"exit").unwrap();
        mcp.close(false).await;
        assert!(!process_is_alive(pid));
        server.cleanup();
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
    async fn ask_user_acp_parked_terminal_commits_progress_retires_generation_and_next_message_is_ordinary()
     {
        let turn = crate::question::QuestionTurn::default();
        turn.claim().unwrap();
        turn.require(crate::question::QuestionBatch(vec![
            crate::question::Question {
                question: "Which branch?".into(),
                options: Vec::new(),
                allow_free_text: true,
            },
        ]));
        let pending = turn.required().unwrap();
        let captured = pending.clone();
        let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counts = invocations.clone();
        let fixture: PromptFixture = Arc::new(move |prompt, history| {
            let pending = captured.clone();
            let counts = counts.clone();
            Box::pin(async move {
                let index = counts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if index == 0 {
                    assert!(history.is_empty());
                    Ok(vec![AgentEvent::AwaitingUser {
                        question: Box::new(pending),
                        interactions: vec![Message::assistant("Prior completed progress")],
                    }])
                } else {
                    assert!(history.iter().any(|item| {
                        serde_json::to_string(item)
                            .unwrap()
                            .contains("Prior completed progress")
                    }));
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant("ordinary next user turn")],
                    )])
                }
            })
        });
        let state = fixture_state(fixture);
        let owner = state.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        Client
            .builder()
            .on_receive_notification(
                async |_: SessionNotification, _cx| Ok(()),
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
                let response = cx
                    .send_request(prompt(session.clone(), "first"))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                let meta = response.meta.unwrap();
                assert_eq!(meta["stop_reason"], "awaiting_user");
                assert_eq!(
                    meta["pending_question"],
                    serde_json::to_value(&pending).unwrap()
                );
                {
                    let sessions = owner.sessions.lock().await;
                    let session = sessions.get(&session).unwrap();
                    assert!(
                        lock_unpoisoned(&session.turns).queue.is_empty(),
                        "exact generation retires before terminal RPC reply"
                    );
                    assert_eq!(
                        session
                            .history
                            .lock()
                            .await
                            .pending_question
                            .as_ref()
                            .unwrap()
                            .id,
                        pending.id
                    );
                }
                let next = cx
                    .send_request(prompt(session.clone(), "main; literal answer data"))
                    .block_task()
                    .await?;
                assert_eq!(next.stop_reason, StopReason::EndTurn);
                assert!(next.meta.is_none());
                assert!(
                    owner
                        .sessions
                        .lock()
                        .await
                        .get(&session)
                        .unwrap()
                        .history
                        .lock()
                        .await
                        .pending_question
                        .is_none()
                );
                assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 2);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn turn_cancellation_stops_session_background_jobs() {
        let sandbox = Sandbox::new(false, "bwrap");
        let id = sandbox
            .start_background_command(
                "while :; do sleep 1; done".to_string(),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        let control = TurnControl::new();
        assert!(control.attach_sandbox(sandbox.clone()));
        assert!(control.cancel());

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = sandbox.background_job_snapshot(&id).unwrap();
                if snapshot.status.as_str() == "cancelled" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ACP cancellation must settle every session background job");
        assert_eq!(sandbox.running_background_job_count(), 0);
        assert_eq!(sandbox.active_group_count(), 0);
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
                            !turns.lock().unwrap().queue.is_empty()
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

    #[tokio::test(start_paused = true)]
    async fn idle_exit_empty_protocol_server_closes_without_model_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fixture: PromptFixture = {
            let calls = calls.clone();
            Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::AcqRel);
                Box::pin(async { panic!("idle server must not invoke a model") })
            })
        };
        let mut state = fixture_state(fixture);
        Arc::get_mut(&mut state).unwrap().idle =
            idle::Idle::new(Some(Duration::from_secs(3))).unwrap();
        let retained = state.clone();
        let connection = Client
            .builder()
            .connect_with(InMemoryAgent(state), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                // A successful server close does not cancel the SDK callback.
                // Observe its explicit EOF signal and let the callback finish.
                cx.incoming_closed().await;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(10), connection)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.sessions.lock().await.is_empty());
        assert!(
            retained.idle.admit().is_err(),
            "server completed by idle selection"
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_exit_connection_eof_drains_actual_prompt_cleanup() {
        use futures::FutureExt;
        let fixture: PromptFixture = Arc::new(|p, _| {
            Box::pin(async move { Ok(vec![done(&p, vec![Message::assistant(p.clone())])]) })
        });
        let barrier = Arc::new(PromptExitBarrier {
            generation: 0,
            entered: Notify::new(),
            release: Notify::new(),
        });
        let mut state = fixture_state_with_exit_barrier(fixture, barrier.clone());
        Arc::get_mut(&mut state).unwrap().idle =
            idle::Idle::new(Some(Duration::from_secs(3))).unwrap();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let retained = state.clone();
        let callback_barrier = barrier.clone();
        // The server is independently owned, like the actual process. A
        // connect_with foreground must not own/drop this server future.
        let (server_io, client_io) = tokio::io::duplex(65536);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);
        let server = connect_agent(
            state,
            ByteStreams::new(tls::FuturesIo(server_write), tls::FuturesIo(server_read)),
        );
        let client = Client
            .builder()
            .on_receive_notification(
                async |_: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                ByteStreams::new(tls::FuturesIo(client_write), tls::FuturesIo(client_read)),
                async move |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let session = cx
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?
                        .session_id;
                    cx.send_request(prompt(session.clone(), "finish response before task"))
                        .block_task()
                        .await?;
                    callback_barrier.entered.notified().await;
                    cx.send_request(CloseSessionRequest::new(session))
                        .block_task()
                        .await?;
                    Ok(())
                },
            );
        tokio::pin!(server, client);
        let mut server_done = false;
        let mut client_done = false;
        let assertions = std::panic::AssertUnwindSafe(async {
            tokio::select! {
                result = &mut client => { client_done = true; result.unwrap(); },
                result = &mut server => { server_done = true; panic!("server completed before client EOF/cleanup: {result:?}"); },
            }
            // Drive the actual server after physical byte-transport EOF for
            // ten idle intervals; only the retained prompt task keeps it live.
            tokio::select! {
                result = &mut server => { server_done = true; panic!("server completed before actual prompt cleanup: {result:?}"); },
                () = tokio::time::sleep(Duration::from_secs(30)) => {},
            }
            assert!(retained.sessions.lock().await.is_empty());
            assert!(retained.idle.wait_settled().now_or_never().is_none());
        }).catch_unwind().await;
        // Clear completed futures through explicit flags and unconditionally
        // settle both endpoints and the actual task before directory removal.
        barrier.release.notify_one();
        if !client_done {
            tokio::time::timeout(Duration::from_secs(5), &mut client)
                .await
                .unwrap()
                .unwrap();
        }
        if !server_done {
            tokio::time::timeout(Duration::from_secs(5), &mut server)
                .await
                .unwrap()
                .unwrap();
        }
        retained_wait_for_fixture_cleanup(&retained).await;
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_exit_preserves_dormant_session_until_explicit_close() {
        use futures::FutureExt;
        let fixture: PromptFixture = Arc::new(|p, _| {
            Box::pin(async move { Ok(vec![done(&p, vec![Message::assistant(p.clone())])]) })
        });
        let mut state = fixture_state(fixture);
        Arc::get_mut(&mut state).unwrap().idle =
            idle::Idle::new(Some(Duration::from_secs(3))).unwrap();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let retained = state.clone();
        let callback_state = retained.clone();
        Client
            .builder()
            .on_receive_notification(
                async |_: SessionNotification, _cx| Ok(()),
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
                tokio::time::advance(Duration::from_secs(30)).await;
                assert!(callback_state.idle.expired().now_or_never().is_none());
                let response = cx
                    .send_request(prompt(session.clone(), "still loaded"))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                cx.send_request(CloseSessionRequest::new(session))
                    .block_task()
                    .await?;
                assert!(callback_state.sessions.lock().await.is_empty());
                assert!(callback_state.idle.expired().now_or_never().is_none());
                Ok(())
            })
            .await
            .unwrap();
        retained_wait_for_fixture_cleanup(&retained).await;
    }

    fn cron_notices(
        updates: &StdMutex<Vec<SessionNotification>>,
    ) -> Vec<(String, Option<serde_json::Value>)> {
        updates
            .lock()
            .unwrap()
            .iter()
            .filter_map(|notice| match &notice.update {
                SessionUpdate::AgentMessageChunk(ContentChunk {
                    content: ContentBlock::Text(text),
                    ..
                }) if text.text.starts_with("cron") => Some((
                    text.text.clone(),
                    notice
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("cron"))
                        .cloned(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn acp_cron_fire_runs_a_scheduled_turn_and_refuses_a_concurrent_prompt() {
        let kinds = Arc::new(StdMutex::new(Vec::new()));
        let release = Arc::new(Notify::new());
        let fixture: PromptFixture = {
            let kinds = kinds.clone();
            let release = release.clone();
            Arc::new(move |prompt, _| {
                kinds
                    .lock()
                    .unwrap()
                    .push(crate::spend::run::Run::current().map(|run| run.kind()));
                let release = release.clone();
                Box::pin(async move {
                    if prompt == "scheduled ping" {
                        release.notified().await;
                    }
                    Ok(vec![done(
                        &prompt,
                        vec![Message::assistant(prompt.clone())],
                    )])
                })
            })
        };
        let mut state = fixture_state(fixture);
        Arc::get_mut(&mut state).unwrap().idle =
            idle::Idle::new(Some(Duration::from_secs(3))).unwrap();
        let retained = state.clone();
        let inspect = state.clone();
        let updates = Arc::new(StdMutex::new(Vec::<SessionNotification>::new()));
        let sink = updates.clone();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let callback_kinds = kinds.clone();
        Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    sink.lock().unwrap().push(notification);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                for refused in [
                    "/cron run c1",
                    "/cron resume",
                    "/cron check it",
                    "/cron 5m /cost",
                    "/cron 5m !ls",
                ] {
                    let error = cx
                        .send_request(prompt(session.clone(), refused))
                        .block_task()
                        .await
                        .expect_err(refused);
                    assert!(
                        error.message.contains("ACP"),
                        "{refused}: {}",
                        error.message
                    );
                }
                let armed = cx
                    .send_request(prompt(session.clone(), "/cron 1m scheduled ping"))
                    .block_task()
                    .await?;
                assert_eq!(armed.stop_reason, StopReason::EndTurn);
                assert!(
                    callback_kinds.lock().unwrap().is_empty(),
                    "arming starts no turn"
                );
                tokio::time::sleep(Duration::from_secs(61)).await;
                for _ in 0..200 {
                    if !callback_kinds.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    *callback_kinds.lock().unwrap(),
                    [Some(crate::spend::RequestKind::Scheduled)],
                    "the fire runs one turn with the scheduled origin"
                );
                assert!(matches!(
                    cron::question_origin(),
                    crate::question::QuestionOrigin::NewUserTurn
                ));
                // The fire holds the session: a client prompt is refused like
                // any concurrent prompt, never interleaved.
                let busy = cx
                    .send_request(prompt(session.clone(), "client prompt"))
                    .block_task()
                    .await
                    .expect_err("a fire is in flight");
                assert!(busy.message.contains("already has an active prompt"));
                // `/cron` itself never touches the running turn.
                cx.send_request(prompt(session.clone(), "/cron list"))
                    .block_task()
                    .await?;
                release.notify_one();
                for _ in 0..200 {
                    if lock_unpoisoned(&inspect.sessions.lock().await[&session].turns)
                        .queue
                        .is_empty()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let response = cx
                    .send_request(prompt(session.clone(), "client prompt"))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                assert_eq!(
                    callback_kinds.lock().unwrap().last(),
                    Some(&Some(crate::spend::RequestKind::Turn))
                );
                {
                    let sessions = inspect.sessions.lock().await;
                    let history = sessions[&session].history.lock().await;
                    assert_eq!(
                        history.session.messages.len(),
                        4,
                        "the scheduled turn is recorded like a client prompt"
                    );
                }
                cx.send_request(prompt(session.clone(), "/cron stop all"))
                    .block_task()
                    .await?;
                assert!(
                    !inspect.sessions.lock().await[&session]
                        .cron
                        .shared()
                        .holds_lease()
                );
                cx.send_request(CloseSessionRequest::new(session))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        let notices = cron_notices(&updates);
        let started = notices
            .iter()
            .find(|(text, _)| {
                text.starts_with("cron c1: scheduled turn (fire 1/24, every 1m): scheduled ping")
            })
            .expect("the client is told a scheduled turn started");
        let meta = started.1.as_ref().expect("_meta.cron");
        assert_eq!(meta["origin"], "scheduled");
        assert_eq!(meta["event"], "started");
        assert_eq!(meta["id"], "c1");
        assert!(
            notices
                .iter()
                .any(|(text, _)| text.starts_with("cron c1: every 1m"))
        );
        assert!(
            notices
                .iter()
                .any(|(text, _)| text == "cron: stopped 1 schedule(s)")
        );
        assert!(
            updates.lock().unwrap().iter().any(|notice| matches!(
                &notice.update,
                SessionUpdate::AvailableCommandsUpdate(commands)
                    if commands.available_commands.iter().any(|command| command.name == "cron")
            )),
            "/cron is advertised"
        );
        retained_wait_for_fixture_cleanup(&retained).await;
        assert_eq!(kinds.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_exit_armed_cron_is_released_by_close_and_by_connection_teardown() {
        use futures::FutureExt;
        let fixture: PromptFixture =
            Arc::new(|_, _| Box::pin(async { panic!("no fire is due in this test") }));
        let mut state = fixture_state(fixture);
        Arc::get_mut(&mut state).unwrap().idle =
            idle::Idle::new(Some(Duration::from_secs(3))).unwrap();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let retained = state.clone();
        let callback_state = retained.clone();
        Client
            .builder()
            .on_receive_notification(
                async |_: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                let closed = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                cx.send_request(prompt(closed.clone(), "/cron 1h hourly"))
                    .block_task()
                    .await?;
                let cron = callback_state.sessions.lock().await[&closed].cron.shared();
                assert!(cron.holds_lease());
                cx.send_request(CloseSessionRequest::new(closed))
                    .block_task()
                    .await?;
                assert!(
                    !cron.holds_lease(),
                    "close drops the schedules and their lease"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
                assert!(callback_state.idle.wait_settled().now_or_never().is_some());
                // A schedule still armed when the connection ends.
                let open = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                cx.send_request(prompt(open, "/cron 1h hourly"))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        // Teardown cleared the dormant session's schedule, so actual cleanup
        // settles instead of waiting on a schedule that can never fire again.
        retained_wait_for_fixture_cleanup(&retained).await;
        for session in retained.sessions.lock().await.values() {
            assert!(!session.cron.shared().holds_lease());
        }
    }

    // Session deletion and response delivery happen before the actual prompt
    // task reaches this retained barrier. This is an actual protocol control,
    // not an inference from the registered-turn queue.
    #[tokio::test(start_paused = true)]
    async fn idle_exit_close_waits_for_whole_prompt_terminal_cleanup() {
        use futures::FutureExt;
        struct ReleaseBarrier(Arc<PromptExitBarrier>);
        impl Drop for ReleaseBarrier {
            fn drop(&mut self) {
                self.0.release.notify_one();
            }
        }
        let fixture: PromptFixture = Arc::new(|p, _| {
            Box::pin(async move { Ok(vec![done(&p, vec![Message::assistant(p.clone())])]) })
        });
        let barrier = Arc::new(PromptExitBarrier {
            generation: 0,
            entered: Notify::new(),
            release: Notify::new(),
        });
        let mut state = fixture_state_with_exit_barrier(fixture, barrier.clone());
        Arc::get_mut(&mut state).unwrap().idle =
            idle::Idle::new(Some(Duration::from_secs(3))).unwrap();
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();
        let retained = state.clone();
        let callback_state = retained.clone();
        let callback_barrier = barrier.clone();
        let result = Client
            .builder()
            .on_receive_notification(
                async |_: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state), async move |cx| {
                let _release_on_any_exit = ReleaseBarrier(callback_barrier.clone());
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let response = cx
                    .send_request(prompt(session.clone(), "complete before cleanup"))
                    .block_task()
                    .await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                callback_barrier.entered.notified().await;
                cx.send_request(CloseSessionRequest::new(session))
                    .block_task()
                    .await?;
                assert!(callback_state.sessions.lock().await.is_empty());
                assert!(lock_unpoisoned(&callback_state.cancel_routes).is_empty());
                tokio::time::advance(Duration::from_secs(30)).await;
                assert!(
                    callback_state.idle.expired().now_or_never().is_none(),
                    "terminal cleanup was detached from idle ownership"
                );
                assert!(callback_state.idle.wait_settled().now_or_never().is_none());
                // Still accept an ordinary request while the actual task owns
                // cleanup, despite an empty session catalogue and queue.
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                callback_barrier.release.notify_one();
                callback_state.idle.wait_settled().await;
                assert!(callback_state.idle.expired().now_or_never().is_none());
                Ok(())
            })
            .await;
        // Every assertion exit releases the retained cleanup owner before the
        // protocol directory is removed, including a failed control.
        barrier.release.notify_one();
        retained_wait_for_fixture_cleanup(&retained).await;
        result.unwrap();
    }

    async fn retained_wait_for_fixture_cleanup(state: &Arc<AcpState>) {
        tokio::time::timeout(Duration::from_secs(5), state.idle.wait_settled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn close_session_removes_it_and_is_idempotent() {
        let fixture: PromptFixture = Arc::new(|p, _| {
            Box::pin(async move { Ok(vec![done(&p, vec![Message::assistant(p.clone())])]) })
        });
        let state = fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(InMemoryAgent(state.clone()), async move |cx| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;

                assert!(
                    state.sessions.lock().await.contains_key(&session),
                    "session must exist before close"
                );

                cx.send_request(CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                assert!(
                    !state.sessions.lock().await.contains_key(&session),
                    "session must be removed after close"
                );

                // Second close must also succeed (idempotent).
                cx.send_request(CloseSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;

                // After close, prompt must fail.
                assert!(
                    cx.send_request(prompt(session, "after-close"))
                        .block_task()
                        .await
                        .is_err(),
                    "prompt on closed session must return error"
                );

                // New session in same workspace still works.
                let fresh = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let resp = cx
                    .send_request(prompt(fresh.clone(), "hello"))
                    .block_task()
                    .await?;
                assert_eq!(resp.stop_reason, StopReason::EndTurn);
                cx.send_request(CloseSessionRequest::new(fresh))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn close_session_cancels_active_turn() {
        let turn_started = Arc::new(tokio::sync::Notify::new());
        let fixture: PromptFixture = {
            let turn_started = turn_started.clone();
            Arc::new(move |_p, _| {
                let turn_started = turn_started.clone();
                Box::pin(async move {
                    turn_started.notify_one();
                    std::future::pending::<Result<Vec<AgentEvent>, String>>().await
                })
            })
        };
        let state = fixture_state(fixture);
        let workspace = ProtocolTempDir::new();
        let cwd = workspace.path().to_path_buf();

        Client
            .builder()
            .on_receive_notification(
                async |_: SessionNotification, _cx| Ok(()),
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

                let cx2 = cx.clone();
                let sid = session.clone();
                let blocked = tokio::spawn(async move {
                    cx2.send_request(prompt(sid, "blocked")).block_task().await
                });
                tokio::time::timeout(Duration::from_secs(1), turn_started.notified())
                    .await
                    .expect("turn should start");

                cx.send_request(CloseSessionRequest::new(session))
                    .block_task()
                    .await?;

                let resp = tokio::time::timeout(Duration::from_secs(1), blocked)
                    .await
                    .expect("cancelled turn must complete")
                    .unwrap()?;
                assert_eq!(resp.stop_reason, StopReason::Cancelled);
                Ok(())
            })
            .await
            .unwrap();
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
        assert!(canonical_session_workspace(Path::new("relative/workspace")).is_err());
    }

    #[tokio::test]
    async fn concurrent_workspace_context_and_core_tools_remain_isolated() {
        let (_container, first, second) = roots();
        let base_context = crate::context::load(true);
        let first_binding = crate::paths::WorkspaceBinding::capture(&first).unwrap();
        let second_binding = crate::paths::WorkspaceBinding::capture(&second).unwrap();
        let first_context = base_context.for_workspace_binding(false, &first_binding);
        let second_context = base_context.for_workspace_binding(false, &second_binding);
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
            false,
            None,
        );
        assert!(first_preamble.contains(&first.display().to_string()));
        assert!(!first_preamble.contains(&second.display().to_string()));

        let first_read = ReadTool::new(None, None, None, 100).with_workspace_root(first.clone());
        let second_read = ReadTool::new(None, None, None, 100).with_workspace_root(second.clone());
        let (first_value, second_value) = tokio::join!(
            first_read.call(ReadArgs {
                raw_notebook: false,
                path: "sentinel.txt".into(),
                offset: None,
                limit: None,
            }),
            second_read.call(ReadArgs {
                raw_notebook: false,
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
                overwrite: false,
            }),
            second_write.call(WriteArgs {
                path: "created.txt".into(),
                content: "created-second".into(),
                overwrite: false,
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
                max_results: None,
                multiline: false,
                pattern: "first-only".into(),
                path: Some(".".into()),
                include: None,
                context_lines: None,
                case_insensitive: false,
                files_only: false,
                count: false,
            }),
            second_grep_tool.call(GrepArgs {
                max_results: None,
                multiline: false,
                pattern: "second-only".into(),
                path: Some(".".into()),
                include: None,
                context_lines: None,
                case_insensitive: false,
                files_only: false,
                count: false,
            })
        );
        assert!(first_grep.unwrap().contains("first-only"));
        assert!(second_grep.unwrap().contains("second-only"));

        let first_find = FindFilesTool::new(None, None, 100)
            .with_workspace_binding(first_workspace.clone())
            .call(FindFilesArgs {
                sort: crate::agent::tools::FindFilesSort::Name,
                mode: crate::agent::tools::FindFilesMode::Regex,
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
                fuzzy: false,
                path: "created.txt".into(),
                replace_all: false,
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
                background: false,
            }),
            second_bash.call(BashArgs {
                command: "pwd; cat sentinel.txt".into(),
                timeout: None,
                background: false,
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
        )
        .unwrap();
        let tool = ReadTool::new(permission, ask_tx, None, 100).with_workspace_root(first.clone());
        let relative_escape = PathBuf::from("..")
            .join(second.file_name().unwrap())
            .join("sentinel.txt");
        let error = tool
            .call(ReadArgs {
                raw_notebook: false,
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
                raw_notebook: false,
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
                    overwrite: false,
                })
                .await
                .is_err()
        );
        assert!(
            bash.call(BashArgs {
                command: "cat sentinel.txt".into(),
                timeout: None,
                background: false,
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
                raw_notebook: false,
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
                overwrite: false,
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
            assert!(
                value.starts_with("JS ") && value.contains("(stage:"),
                "the JS read must fail: {value}"
            );
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
            assert!(
                value.starts_with("JS ") && value.contains("(stage:"),
                "the JS write must fail: {value}"
            );
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
                Sandbox::new(false, "bwrap")
                    .with_workspace_binding(workspace.clone())
                    .with_complete_process_tree_for_test(),
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
mod provider_resolution_tests {
    use super::*;

    fn config(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("test config parses")
    }

    const CUSTOM: &str = r#"
provider = "acme"

[custom_providers.acme]
provider_type = "openai"
base_url = "http://127.0.0.1:9/v1"
model = "acme-default"

[quick_models.fast]
provider = "acme"
model = "acme-fast"
"#;

    #[test]
    fn acp_honours_quick_model_like_startup() {
        let cli = Cli {
            quick_model: Some("fast".into()),
            ..Default::default()
        };
        let (provider, model) = acp_provider_and_model(&cli, &config(CUSTOM));
        assert_eq!(provider, "acme");
        assert_eq!(model, "acme-fast");
    }

    #[test]
    fn custom_provider_model_is_only_a_default_under_acp() {
        let cfg = config(CUSTOM);
        let (_, model) = acp_provider_and_model(&Cli::default(), &cfg);
        assert_eq!(model, "acme-default");

        let explicit = config(&format!("model = \"explicit-model\"\n{CUSTOM}"));
        let (provider, model) = acp_provider_and_model(&Cli::default(), &explicit);
        assert_eq!(provider, "acme");
        assert_eq!(
            model, "explicit-model",
            "cfg.model must win over the custom default"
        );
    }

    #[test]
    fn acp_client_uses_the_cli_api_key() {
        let cfg = config(CUSTOM);
        assert!(
            acp_client(&Cli::default(), &cfg, "acme").is_err(),
            "a custom provider without any key must not resolve"
        );
        let cli = Cli {
            api_key: Some("cli-key".into()),
            ..Default::default()
        };
        assert!(acp_client(&cli, &cfg, "acme").is_ok());
    }

    #[test]
    fn acp_provider_client_is_built_once_and_failures_are_not_cached() {
        let cfg = config(CUSTOM);
        let clients = ProviderClients::default();
        assert!(clients.get(&Cli::default(), &cfg, "acme").is_err());
        let cli = Cli {
            api_key: Some("cli-key".into()),
            ..Default::default()
        };
        for _ in 0..3 {
            assert!(clients.get(&cli, &cfg, "acme").is_ok());
        }
        assert_eq!(clients.builds.load(Ordering::Relaxed), 1);
    }

    #[cfg(feature = "subagents")]
    #[test]
    fn acp_installs_a_subagent_config_for_the_task_tool() {
        let cli = Cli {
            api_key: Some("cli-key".into()),
            quick_model: Some("fast".into()),
            ..Default::default()
        };
        let resolved = acp_subagent_config(&cli, &config(CUSTOM))
            .expect("ACP must initialise the task tool's subagent config");
        assert_eq!(resolved.provider_name, "acme");
        assert_eq!(resolved.model_name, "acme-fast");
        assert_eq!(resolved.api_key.as_deref(), Some("cli-key"));

        let disabled = config(&format!("task_enabled = false\n{CUSTOM}"));
        assert!(acp_subagent_config(&cli, &disabled).is_none());
    }
}

#[cfg(test)]
mod hook_root_tests {
    use super::*;

    #[test]
    fn concurrent_turns_in_different_workspaces_disable_the_shared_root() {
        let published = Arc::new(StdMutex::new(Vec::<Option<std::path::PathBuf>>::new()));
        let recorder = published.clone();
        let claims = Arc::new(HookRootClaims::with_publisher(move |root| {
            lock_unpoisoned(&recorder).push(root.map(Path::to_path_buf));
        }));
        let repo_a = Path::new("/workspace/a");
        let repo_b = Path::new("/workspace/b");
        let take = || std::mem::take(&mut *lock_unpoisoned(&published));

        let first = claims.acquire(repo_a);
        assert_eq!(take(), vec![Some(repo_a.to_path_buf())]);
        let same = claims.acquire(repo_a);
        assert!(take().is_empty(), "the same root is not republished");
        let other = claims.acquire(repo_b);
        assert_eq!(
            take(),
            vec![None],
            "several workspaces leave no correct shared root"
        );

        drop(first);
        assert!(take().is_empty(), "repo a is still held by another turn");
        drop(same);
        assert_eq!(take(), vec![Some(repo_b.to_path_buf())]);
        drop(other);
        assert!(take().is_empty());
        assert!(lock_unpoisoned(&claims.active).is_empty());
    }
}

#[cfg(test)]
mod tcp_authentication_tests {
    #[tokio::test(start_paused = true)]
    async fn idle_exit_plaintext_auth_remains_active_until_native_job_settles() {
        use futures::FutureExt;
        use tokio::io::AsyncReadExt;
        let idle = idle::Idle::new(Some(Duration::from_secs(1))).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let source = accept::tokio_listener(listener).unwrap();
        let server = tokio::spawn(accept_authenticated_peer_with_idle(
            source,
            "inert-key".into(),
            2,
            idle.clone(),
        ));
        let mut peer = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut byte = [0; 1];
        tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut byte))
            .await
            .unwrap()
            .unwrap();
        // The actual challenge byte proves the blocking task owns the peer.
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(idle.expired().now_or_never().is_none());
        tokio::time::resume();
        drop(peer);
        server.abort();
        let _ = server.await;
        tokio::time::timeout(Duration::from_secs(5), idle.wait_settled())
            .await
            .unwrap();
        assert!(
            idle.expired().now_or_never().is_none(),
            "quiet interval must restart after native auth settles"
        );
    }

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
            resolve_tcp_settings_with_key(&Cli::default(), &Config::default(), None, false, false)
                .unwrap();
        assert!(settings.is_none());
    }

    #[test]
    fn port_only_tcp_configuration_defaults_to_loopback() {
        let cli = Cli {
            acp_port: Some(8123),
            ..Default::default()
        };
        let cfg = tcp_config(DEFAULT_TCP_HOST, 8123, Some("configured-key"));

        let settings = resolve_tcp_settings_with_key(&cli, &cfg, None, false, false)
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

        let error = resolve_tcp_settings_with_key(&cli, &Config::default(), None, false, false)
            .err()
            .expect("TCP without authentication must fail");
        assert!(error.to_string().contains("requires authentication"));
    }

    #[test]
    fn non_loopback_tcp_bind_requires_an_explicit_insecure_opt_in() {
        let cli = Cli {
            acp_host: Some("0.0.0.0".to_owned()),
            ..Default::default()
        };
        let key = Some("secret".to_owned());
        let error =
            resolve_tcp_settings_with_key(&cli, &Config::default(), key.clone(), false, false)
                .err()
                .expect("a remote bind must be refused by default");
        assert!(error.to_string().contains(ACP_ALLOW_REMOTE_ENV), "{error}");
        assert!(!error.to_string().contains("secret"));

        let settings =
            resolve_tcp_settings_with_key(&cli, &Config::default(), key.clone(), true, false)
                .unwrap()
                .unwrap();
        assert_eq!(settings.host, "0.0.0.0");

        // A TLS listener authenticates the server and protects the session,
        // so a remote bind needs no insecure opt-in (mini-agent-bky6w).
        assert!(
            resolve_tcp_settings_with_key(&cli, &Config::default(), key.clone(), false, true)
                .unwrap()
                .is_some()
        );

        for loopback in ["127.0.0.1", "::1", "localhost"] {
            let cli = Cli {
                acp_host: Some(loopback.to_owned()),
                ..Default::default()
            };
            assert!(
                resolve_tcp_settings_with_key(&cli, &Config::default(), key.clone(), false, false)
                    .unwrap()
                    .is_some(),
                "{loopback} needs no opt-in"
            );
        }
    }

    /// Runs the plaintext accept loop over `source` and, on the blocking
    /// pool, one partial peer followed by one valid peer; returns the address
    /// the loop authenticated and the valid peer's own address.
    async fn authenticate_after_partial_peer<S: accept::AcceptSource + 'static>(
        source: S,
        address: SocketAddr,
    ) -> (SocketAddr, SocketAddr) {
        let server = tokio::spawn(accept_authenticated_peer(
            source,
            "configured-key".to_owned(),
            MAX_PENDING_AUTHENTICATIONS,
        ));
        let (partial, valid_address) = tokio::task::spawn_blocking(move || {
            let mut partial = TcpStream::connect(address).unwrap();
            let _ = read_challenge(&mut partial).unwrap();

            let mut valid = TcpStream::connect(address).unwrap();
            let nonce = read_challenge(&mut valid).unwrap();
            send_response(&mut valid, &nonce, "configured-key").unwrap();
            (partial, valid.local_addr().unwrap())
        })
        .await
        .unwrap();

        let (_, authenticated_address) = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("a valid peer must not be blocked by a partial peer")
            .unwrap()
            .unwrap();
        drop(partial);
        (authenticated_address, valid_address)
    }

    #[tokio::test]
    async fn racing_valid_peer_is_not_blocked_by_partial_peer() {
        let listener =
            accept::tokio_listener(TcpListener::bind((DEFAULT_TCP_HOST, 0)).unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        let (authenticated, valid) = authenticate_after_partial_peer(listener, address).await;
        assert_eq!(authenticated, valid);
    }

    #[tokio::test]
    async fn aborted_accepts_do_not_stop_a_valid_peer_from_authenticating() {
        let (source, address) = accept::test_support::FlakyListener::bind(
            accept::test_support::per_connection_failures(),
        );
        let (authenticated, valid) = authenticate_after_partial_peer(source, address).await;
        assert_eq!(authenticated, valid);
    }

    #[tokio::test]
    async fn a_listener_failure_still_ends_the_plaintext_accept_loop() {
        let (source, _) = accept::test_support::FlakyListener::bind(vec![std::io::Error::from(
            std::io::ErrorKind::InvalidInput,
        )]);
        let error = accept_authenticated_peer(source, "configured-key".to_owned(), 4)
            .await
            .map(|_| ())
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[cfg(test)]
mod partial_turn_tests {
    use super::*;
    use rig::agent::AgentBuilder;
    use rig::completion::Message;
    use rig::test_utils::{MockCompletionModel, MockError, MockStreamEvent};
    use rig::tool::ToolDyn;

    /// Run a scripted turn that writes a real file and then fails, returning
    /// the runner's terminal event.
    async fn failing_turn_after_a_write(root: &std::path::Path) -> AgentEvent {
        let write = Box::new(
            crate::agent::tools::WriteTool::new(None, None, None)
                .with_workspace(root.to_path_buf()),
        ) as Box<dyn ToolDyn>;
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "acp-write",
                    "write",
                    serde_json::json!({ "path": "effect.txt", "content": "written\n" }),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![MockStreamEvent::Error(MockError::provider(
                "invalid_request_error: bad request",
            ))],
        ]);
        let agent = AgentBuilder::new(model)
            .tools(vec![write])
            .default_max_turns(4)
            .build();
        let runner = crate::agent::runner::spawn_agent(
            agent,
            "write the file".to_string(),
            Vec::<Message>::new(),
            crate::retry::RetryConfig::default(),
            None,
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut events = runner.event_rx;
        let mut terminal = None;
        while let Some(event) = events.recv().await {
            if matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. }) {
                terminal = Some(event);
            }
        }
        terminal.expect("the runner must reach a terminal event")
    }

    fn temp_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-acp-partial-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(&root).unwrap()
    }

    #[tokio::test]
    async fn a_failed_turn_carries_the_completed_tool_messages() {
        let root = temp_root();
        let terminal = failing_turn_after_a_write(&root).await;
        let AgentEvent::Error {
            message,
            interactions,
        } = terminal
        else {
            panic!("expected a terminal failure");
        };
        assert!(message.contains("invalid_request_error"), "{message}");
        assert!(
            root.join("effect.txt").is_file(),
            "the scripted write must have reached the workspace"
        );
        assert!(
            !interactions.is_empty(),
            "a failure after a completed tool call must carry its messages"
        );

        // Committing that progress makes the next prompt see the same history
        // the workspace already reflects.
        let mut history = SessionHistory::default();
        history.commit_completed_turn("write the file", interactions);
        let next = history.snapshot();
        assert!(
            !next.is_empty(),
            "the next prompt must not receive an empty history after a refusal"
        );
        assert!(
            next.iter().any(|message| matches!(
                message,
                Message::User { content }
                    if content
                        .iter()
                        .any(|item| matches!(item, rig::message::UserContent::ToolResult(_)))
            )),
            "the completed tool result must be retained: {next:?}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_turn_without_progress_commits_nothing() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![MockStreamEvent::Error(
            MockError::provider("invalid_request_error: refused before any work"),
        )]]);
        let agent = AgentBuilder::new(model).default_max_turns(2).build();
        let runner = crate::agent::runner::spawn_agent(
            agent,
            "do nothing".to_string(),
            Vec::<Message>::new(),
            crate::retry::RetryConfig::default(),
            None,
            #[cfg(feature = "skills")]
            None,
            #[cfg(feature = "hooks")]
            None,
        );
        let mut events = runner.event_rx;
        let mut terminal = None;
        while let Some(event) = events.recv().await {
            if matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. }) {
                terminal = Some(event);
            }
        }
        let AgentEvent::Error { interactions, .. } =
            terminal.expect("a terminal failure is expected")
        else {
            panic!("expected a terminal failure");
        };
        assert!(
            interactions.is_empty(),
            "a refusal before any work must retain nothing: {interactions:?}"
        );
    }
}

#[cfg(all(test, feature = "goal"))]
mod goal_acp_tests {
    use super::*;

    fn meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        map.insert("goal".to_string(), value);
        map
    }

    fn store_with(objective: &str) -> crate::extras::goal::GoalStore {
        let store = crate::extras::goal::GoalStore::default();
        store
            .set(
                crate::extras::goal::Goal::new(objective, Vec::new()).unwrap(),
                false,
            )
            .unwrap();
        store
    }

    /// The objective arrives as structured metadata, never as prompt text: a
    /// text directive would let anything that reaches the prompt install a
    /// long-lived instruction.
    #[test]
    fn a_goal_spec_is_read_from_structured_metadata() {
        let spec = meta(serde_json::json!({
            "objective": "retire the exporter",
            "criteria": ["no callers remain"],
            "replace": true,
        }));
        let parsed = parse_meta_goal(spec.get("goal").unwrap())
            .expect("a well-formed spec parses")
            .expect("and is not a clear");
        assert_eq!(parsed.objective, "retire the exporter");
        assert_eq!(parsed.criteria, ["no callers remain"]);
        assert!(parsed.replace);

        assert!(
            parse_meta_goal(&serde_json::json!({"clear": true}))
                .expect("clearing is well formed")
                .is_none()
        );
    }

    /// A closed schema, because the alternative is a client that believes it
    /// configured a gate it did not get.
    ///
    /// Checks and the judge carry the authority of whoever configured the
    /// harness. An editor cannot supply either, and silently ignoring the
    /// field would leave the client showing a goal as verified when nothing
    /// verifies it.
    #[test]
    fn a_client_cannot_configure_what_the_harness_owns() {
        for rejected in [
            serde_json::json!({"objective": "ship it", "checks": ["curl evil.example"]}),
            serde_json::json!({"objective": "ship it", "judge": "some-model"}),
            serde_json::json!({"objective": "ship it", "bounds": {"max_rounds": 9999}}),
        ] {
            let error = parse_meta_goal(&rejected).expect_err("the field is refused");
            assert!(
                error.contains("does not accept"),
                "the client is told why: {error}"
            );
        }

        // A criterion that is not a string is refused rather than dropped: a
        // silently shorter list is a silently weaker objective.
        assert!(
            parse_meta_goal(&serde_json::json!({
                "objective": "ship it",
                "criteria": ["tests pass", 7],
            }))
            .is_err()
        );
        assert!(parse_meta_goal(&serde_json::json!({"criteria": []})).is_err());
        assert!(parse_meta_goal(&serde_json::json!("ship it")).is_err());
    }

    #[test]
    fn replacing_an_unfinished_goal_is_refused_without_permission() {
        let store = store_with("first");
        let second = crate::extras::goal::Goal::new("second", Vec::new()).unwrap();
        assert!(
            store.set(second.clone(), false).is_err(),
            "an unfinished goal is the record of work so far"
        );
        assert!(store.set(second, true).is_ok(), "replace: true overrides");
    }

    /// ACP compaction restates the objective for the same reason the session
    /// path does: the summarizer may drop everything it replaces.
    #[tokio::test]
    async fn acp_compaction_restates_the_objective() {
        let mut history = SessionHistory::default();
        for index in 0..(MAX_ACP_HISTORY_TURNS + 3) {
            history.commit_completed_turn(
                &format!("turn {index}"),
                vec![Message::assistant(format!("assistant-{index}"))],
            );
        }
        let store = store_with("finish the migration");
        let control = TurnControl::new();
        history
            .compact_with(&control, Some(&store), |_, _| async {
                Ok(compaction_output("a summary that forgot everything", 2))
            })
            .await;

        let summary = history
            .session
            .compacted_context()
            .0
            .expect("history was summarized");
        assert!(
            summary.contains("finish the migration"),
            "the objective must survive ACP compaction: {summary}"
        );
    }

    #[tokio::test]
    async fn a_finished_goal_is_not_restated_into_an_acp_summary() {
        let mut history = SessionHistory::default();
        for index in 0..(MAX_ACP_HISTORY_TURNS + 3) {
            history.commit_completed_turn(
                &format!("turn {index}"),
                vec![Message::assistant(format!("assistant-{index}"))],
            );
        }
        let store = store_with("already done");
        store.with_mut(|goal| goal.set_status(crate::extras::goal::GoalStatus::Met, None));
        let control = TurnControl::new();
        history
            .compact_with(&control, Some(&store), |_, _| async {
                Ok(compaction_output("summary", 2))
            })
            .await;
        assert!(
            !history
                .session
                .compacted_context()
                .0
                .unwrap_or_default()
                .contains("already done")
        );
    }
}

#[cfg(all(test, feature = "goal"))]
mod goal_usage_tests {
    use super::*;

    fn report(output_tokens: u64, reasoning_tokens: u64) -> crate::event::UsageDelta {
        crate::event::UsageDelta {
            input_tokens: 100,
            output_tokens,
            total_tokens: 100 + output_tokens + reasoning_tokens,
            reasoning_tokens,
            ..crate::event::UsageDelta::default()
        }
    }

    #[test]
    fn acp_goal_usage_folds_gemini_thinking_tokens_into_output() {
        let cfg = Config::default();
        let mut total = rig::completion::Usage::default();
        add_goal_usage(&mut total, &cfg, "gemini", report(20, 30));
        add_goal_usage(&mut total, &cfg, "gemini", report(5, 0));
        assert_eq!(total.input_tokens, 200);
        assert_eq!(total.output_tokens, 55);
        assert_eq!(total.total_tokens, 255);
    }

    #[test]
    fn acp_goal_usage_does_not_double_count_inclusive_reasoning() {
        let cfg = Config::default();
        let mut total = rig::completion::Usage::default();
        // OpenAI already includes reasoning in output_tokens.
        add_goal_usage(&mut total, &cfg, "openai", report(50, 30));
        assert_eq!(total.output_tokens, 50);
        assert_eq!(total.total_tokens, 150);
    }
}

#[cfg(test)]
mod question_transport_tests {
    use super::*;
    use crate::question::{Answer, Question, QuestionAnswer, QuestionBatch, QuestionOption};

    #[tokio::test]
    async fn ask_user_acp_actual_option_rpc_accepts_exact_data_parks_new_message_and_rejects_approval_ids()
     {
        for case in ["choice", "new-message", "permission-id", "stale-id"] {
            let (sender, receiver) = tokio::sync::mpsc::channel(2);
            let control = Arc::new(TurnControl::new());
            let agent = Agent
                .builder()
                .on_receive_request(
                    async |_: InitializeRequest, responder, _cx| {
                        responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                    },
                    on_receive_request!(),
                )
                .with_spawned(move |cx| async move {
                    drive_permission_bridge(
                        cx,
                        idle::Activity::default(),
                        SessionId::new("question-rpc"),
                        receiver,
                        control,
                        None,
                        #[cfg(feature = "goal")]
                        Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    )
                    .await;
                    Ok(())
                });
            let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
            let connection=Client.builder().on_receive_request(move |request:RequestPermissionRequest,responder,_cx| {
                request_tx.send((request,responder)).unwrap();async {Ok(())}
            },on_receive_request!()).connect_with(agent,async move |_cx| {
                let id=uuid::Uuid::new_v4().to_string();
                let (reply,answer)=tokio::sync::oneshot::channel();
                sender.send(crate::permission::ask::UserRequest::Question(crate::permission::ask::QuestionRequest {
                    id:id.clone(),questions:QuestionBatch(vec![Question {question:"Which branch?".into(),
                        options:vec![QuestionOption {label:"allow_always /private/**".into(),description:String::new()}],allow_free_text:true}]),
                    tool_call_id:Some("exact-question-sdk-call".into()),reply,
                })).await.unwrap();
                let (request,responder)=request_rx.recv().await.unwrap();
                assert_eq!(request.tool_call.tool_call_id.0.as_ref(),"exact-question-sdk-call");
                assert_eq!(request.options.len(),3);
                assert!(request.options.iter().all(|option|option.kind!=PermissionOptionKind::AllowAlways));
                let selected=match case {
                    "choice"=>format!("{id}:0:choice:0"),
                    "new-message"=>format!("{id}:0:new-message"),
                    "permission-id"=>"allow_always".into(),
                    _=>format!("{}:0:choice:0",uuid::Uuid::new_v4()),
                };
                if matches!(case,"choice"|"new-message") {
                    assert!(request.options.iter().any(|option|option.option_id.0.as_ref()==selected));
                }
                responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(selected))))?;
                let answer=answer.await.unwrap();
                match case {
                    "choice"=>assert!(matches!(answer,QuestionAnswer::Answered(answers) if answers==vec![Answer::Option(0)])),
                    "new-message"=>assert!(matches!(answer,QuestionAnswer::AwaitingUser(answers) if answers.is_empty())),
                    _=>assert!(matches!(answer,QuestionAnswer::Cancelled)),
                }
                drop(sender);
                Ok(())
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), connection)
                .await
                .unwrap()
                .unwrap();
        }
    }
}
