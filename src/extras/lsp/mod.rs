//! LSP (Language Server Protocol) integration: spawns language servers for
//! files the agent edits and feeds diagnostics back into tool results.
//!
//! Available servers are detected automatically; explicit `[lsp] enabled = false`
//! disables integration. Server processes still start only on file interaction.
//! Everything is fail-open: a missing server binary, a hung handshake, or a
//! crashed server only means "no diagnostics", never a failed edit.

pub(crate) mod client;
pub(crate) mod registry;
pub mod rpc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use lsp_types::DiagnosticSeverity;
use tokio::sync::Notify;

use crate::config::types::LspConfig;
use client::{DiagStore, LspClient};

/// How long to wait for the `publishDiagnostics` that follows an edit before
/// falling back to whatever is already stored. Kept short so clean edits
/// stay fast; servers that don't republish identical diagnostics just time
/// out and reuse the previous (identical) set.
const DIAG_WAIT: Duration = Duration::from_millis(1000);
/// Wait used after a server missed its publish window: long enough to notice a
/// server that republishes promptly again, short enough that a server which
/// never republishes (indexing, identical-diagnostics dedupe) costs ~nothing.
const DIAG_WAIT_AFTER_MISS: Duration = Duration::from_millis(50);
const SERVER_RETRY_BASE: Duration = Duration::from_secs(1);
const SERVER_RETRY_MAX: Duration = Duration::from_secs(60);
const SERVER_FAILURE_COUNT_MAX: u32 = 7;

/// Max diagnostics lines appended to a tool result.
pub(crate) const MAX_DIAG_LINES: usize = 20;

#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Inner>,
}

const HEALTH_MAX_NAMES: usize = 32;
const HEALTH_MAX_NAME_CHARS: usize = 128;

/// Existing manager metadata only. Missing cache fields mean their lock was
/// busy or poisoned; they never mean an empty/healthy runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct LspHealthSnapshot {
    pub configured_count: usize,
    pub configured_names: Vec<String>,
    pub live_count: Option<usize>,
    pub live_names: Option<Vec<String>>,
    pub failure_count: Option<usize>,
    pub failures: Option<Vec<LspFailureSnapshot>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct LspFailureSnapshot {
    pub name: String,
    pub consecutive: u32,
    pub retry_in_ms: u64,
}

fn health_name(name: &str) -> String {
    name.chars()
        .take(HEALTH_MAX_NAME_CHARS)
        .map(|ch| {
            if ch.is_whitespace() || crate::ui::events::needs_terminal_sanitizing(ch) {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

pub(crate) enum LspWorkspace {
    Binding(Arc<crate::paths::WorkspaceBinding>),
    Path(PathBuf),
}

impl From<Arc<crate::paths::WorkspaceBinding>> for LspWorkspace {
    fn from(workspace: Arc<crate::paths::WorkspaceBinding>) -> Self {
        Self::Binding(workspace)
    }
}

impl From<PathBuf> for LspWorkspace {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

struct Inner {
    workspace: Arc<crate::paths::WorkspaceBinding>,
    servers: Vec<(String, crate::config::types::LspServerConfig)>,
    /// server name → currently live client. Failed or stopped clients are not
    /// retained so a later edit can restart a repaired server.
    clients: tokio::sync::Mutex<HashMap<String, Arc<LspClient>>>,
    /// Consecutive startup/crash failures. A broken server is not relaunched
    /// for every edit; successful initialization clears its entry.
    failures: Mutex<HashMap<String, ServerFailureState>>,
    diags: DiagStore,
    diag_notify: Arc<Notify>,
    /// server name → whether its last edit wait missed the publish.
    publish_waits: Mutex<HashMap<String, PublishWaitState>>,
    /// Seconds a client may sit unused before it is retired; 0 never retires.
    idle_timeout_secs: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    active_bindings: Arc<AtomicUsize>,
    #[cfg(test)]
    peak_bindings: Arc<AtomicUsize>,
    #[cfg(test)]
    test_synced_documents: Arc<std::sync::Mutex<HashMap<String, client::SyncedDocument>>>,
}

/// Adaptive state for the post-edit `publishDiagnostics` wait of one server.
#[derive(Debug, Default, Clone)]
struct PublishWaitState {
    /// The last edit wait timed out without a publish.
    missed: bool,
    /// Cache uri and publish counter at that miss; a later counter increase
    /// means the server published after we stopped waiting.
    missed_at: Option<(String, u64)>,
}

#[derive(Debug, Clone, Copy)]
struct ServerFailureState {
    consecutive: u32,
    retry_after: Instant,
}

impl ServerFailureState {
    fn next(previous: Option<Self>, now: Instant) -> Self {
        let consecutive = previous
            .map_or(1, |failure| failure.consecutive.saturating_add(1))
            .min(SERVER_FAILURE_COUNT_MAX);
        let multiplier = 1_u32 << consecutive.saturating_sub(1);
        let cooldown = SERVER_RETRY_BASE
            .saturating_mul(multiplier)
            .min(SERVER_RETRY_MAX);
        Self {
            consecutive,
            retry_after: now + cooldown,
        }
    }

    fn cooling_down(self, now: Instant) -> bool {
        now < self.retry_after
    }
}

static LIVE_MANAGERS: OnceLock<Mutex<Vec<Weak<Inner>>>> = OnceLock::new();

fn register_live_manager(inner: &Arc<Inner>) {
    let registry = LIVE_MANAGERS.get_or_init(|| Mutex::new(Vec::new()));
    let mut managers = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    managers.retain(|manager| manager.strong_count() > 0);
    managers.push(Arc::downgrade(inner));
}

/// Explicitly stop every language server owned by live agent tool sets.
/// Application teardown calls this before those opaque tool sets are dropped.
pub(crate) async fn shutdown_live_managers() {
    let managers = LIVE_MANAGERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain(..)
        .filter_map(|manager| manager.upgrade())
        .collect::<Vec<_>>();
    for inner in managers {
        LspManager { inner }.shutdown().await;
    }
}

#[cfg(any(feature = "acp", test))]
tokio::task_local! {
    static SESSION_LSP: Arc<SessionLsp>;
}

/// A session-owned language-server manager for frontends whose agent is
/// rebuilt every turn (ACP). Without it each turn would re-scan `PATH` and
/// restart, re-initialize and re-index every language server it uses.
///
/// Bind it around agent construction with [`SessionLsp::scope`]; the builder's
/// [`LspManager::prepare_for_session`] then reuses the manager built by an
/// earlier turn while the workspace and configuration are unchanged.
#[cfg(any(feature = "acp", test))]
#[derive(Default)]
pub(crate) struct SessionLsp {
    slot: tokio::sync::Mutex<Option<SessionLspEntry>>,
    #[cfg(test)]
    prepares: AtomicUsize,
}

#[cfg(any(feature = "acp", test))]
struct SessionLspEntry {
    workspace: usize,
    config: LspConfig,
    manager: Option<LspManager>,
}

#[cfg(any(feature = "acp", test))]
impl SessionLsp {
    pub(crate) async fn scope<F: std::future::Future>(self: &Arc<Self>, future: F) -> F::Output {
        SESSION_LSP.scope(Arc::clone(self), future).await
    }

    async fn get_or_prepare(
        &self,
        cfg: &LspConfig,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> Option<LspManager> {
        let workspace_id = Arc::as_ptr(&workspace) as usize;
        let mut slot = self.slot.lock().await;
        // Structural equality: no per-prompt serialization or allocation.
        if let Some(entry) = slot.as_ref()
            && entry.workspace == workspace_id
            && entry.config == *cfg
        {
            return entry.manager.clone();
        }
        #[cfg(test)]
        self.prepares.fetch_add(1, Ordering::Relaxed);
        let manager = LspManager::prepare(cfg, workspace).await;
        // A changed workspace or configuration replaces the entry; its old
        // servers are stopped rather than left running unowned.
        let previous = slot.replace(SessionLspEntry {
            workspace: workspace_id,
            config: cfg.clone(),
            manager: manager.clone(),
        });
        if let Some(previous) = previous.and_then(|entry| entry.manager) {
            previous.shutdown().await;
        }
        manager
    }

    #[cfg(test)]
    pub(crate) fn prepare_count(&self) -> usize {
        self.prepares.load(Ordering::Relaxed)
    }

    /// Stop the session's language servers (session close, connection end).
    pub(crate) async fn shutdown(&self) {
        let entry = self.slot.lock().await.take();
        if let Some(manager) = entry.and_then(|entry| entry.manager) {
            manager.shutdown().await;
        }
    }
}

/// A diagnostic-cache entry bound to the exact regular file that produced it.
/// The open handle keeps that object alive across an interactive permission
/// wait; formatting later compares the cache identity to this handle rather
/// than resolving the pathname again.
pub(crate) struct BoundDiagnosticPath {
    path: PathBuf,
    uri: String,
    identity: crate::fs::CheckedMetadata,
    _file: tokio::fs::File,
    #[cfg(test)]
    active_bindings: Arc<AtomicUsize>,
}

impl BoundDiagnosticPath {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
impl Drop for BoundDiagnosticPath {
    fn drop(&mut self) {
        self.active_bindings.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) struct DiagnosticSnapshot {
    uri: String,
    lines: Vec<String>,
    truncated: bool,
}

impl DiagnosticSnapshot {
    pub(crate) fn retained_line_count(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }
}

impl LspManager {
    /// Never opens a file, starts a server, requests diagnostics or waits for
    /// a runtime lock. Retained cache entries are not a new health probe.
    pub(crate) fn diagnostic_snapshot(&self) -> LspHealthSnapshot {
        let live = self.inner.clients.try_lock().ok();
        let failures = self.inner.failures.try_lock().ok();
        let now = Instant::now();
        LspHealthSnapshot {
            configured_count: self.inner.servers.len(),
            configured_names: self
                .inner
                .servers
                .iter()
                .take(HEALTH_MAX_NAMES)
                .map(|(name, _)| health_name(name))
                .collect(),
            live_count: live.as_ref().map(|clients| clients.len()),
            live_names: live.as_ref().map(|clients| {
                let mut names: Vec<_> = clients
                    .keys()
                    .take(HEALTH_MAX_NAMES)
                    .map(|name| health_name(name))
                    .collect();
                names.sort();
                names
            }),
            failure_count: failures.as_ref().map(|entries| entries.len()),
            failures: failures.as_ref().map(|entries| {
                let mut rows: Vec<_> = entries
                    .iter()
                    .take(HEALTH_MAX_NAMES)
                    .map(|(name, state)| LspFailureSnapshot {
                        name: health_name(name),
                        consecutive: state.consecutive,
                        retry_in_ms: state
                            .retry_after
                            .saturating_duration_since(now)
                            .as_millis()
                            .min(u64::MAX as u128) as u64,
                    })
                    .collect();
                rows.sort_by(|left, right| left.name.cmp(&right.name));
                rows
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn same_manager(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn new(cfg: &LspConfig, workspace: impl Into<LspWorkspace>) -> Self {
        let workspace = match workspace.into() {
            LspWorkspace::Binding(workspace) => workspace,
            LspWorkspace::Path(path) => Arc::new(
                crate::paths::WorkspaceBinding::capture(&path)
                    .expect("LSP workspace must be a stable directory"),
            ),
        };
        let servers = registry::resolve_servers(&cfg.servers);
        let manager = Self::with_servers(servers, workspace);
        manager.apply_idle_timeout(cfg);
        manager
    }

    /// Applies the configured idle timeout; affects servers started afterwards.
    fn apply_idle_timeout(&self, cfg: &LspConfig) {
        self.inner.idle_timeout_secs.store(
            cfg.idle_timeout().map_or(0, |idle| idle.as_secs()),
            Ordering::Relaxed,
        );
    }

    /// The idle timeout in force, `None` when servers are never retired.
    fn idle_timeout(&self) -> Option<Duration> {
        match self.inner.idle_timeout_secs.load(Ordering::Relaxed) {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        }
    }

    /// Own the filesystem metadata lookup before registering actual tool schema.
    /// Explicit on retains the existing configured lazy-server behavior.
    pub(crate) async fn prepare(
        cfg: &LspConfig,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> Option<Self> {
        use crate::config::types::LspActivation;
        match cfg.enabled {
            LspActivation::Off => None,
            LspActivation::On => Some(Self::new(cfg, workspace)),
            LspActivation::Auto => {
                let manager = Self::prepare_automatic(
                    cfg.servers.clone(),
                    std::env::var_os("PATH"),
                    workspace,
                    crate::agent::runner::AgentWorkScope::new(),
                )
                .await;
                if let Some(manager) = &manager {
                    manager.apply_idle_timeout(cfg);
                }
                manager
            }
        }
    }

    /// [`Self::prepare`], reusing the manager of the surrounding
    /// [`SessionLsp`] scope when there is one.
    pub(crate) async fn prepare_for_session(
        cfg: &LspConfig,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> Option<Self> {
        #[cfg(any(feature = "acp", test))]
        if let Ok(session) = SESSION_LSP.try_with(Arc::clone) {
            return session.get_or_prepare(cfg, workspace).await;
        }
        Self::prepare(cfg, workspace).await
    }

    async fn prepare_automatic(
        configured: HashMap<String, crate::config::types::LspServerConfig>,
        search: Option<std::ffi::OsString>,
        workspace: Arc<crate::paths::WorkspaceBinding>,
        scope: Arc<crate::agent::runner::AgentWorkScope>,
    ) -> Option<Self> {
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        let result = scope
            .run(async move {
                // Spawn only after run installs this exact owner, never while
                // constructing the argument to run.
                crate::agent::runner::spawn_blocking_scoped(move || {
                    registry::available_servers(&configured, search.as_deref())
                })
                .await
            })
            .await;
        let cancelled = scope.is_cancelled();
        cleanup.settle().await;
        let servers = result.ok()?;
        if cancelled || servers.is_empty() || workspace.validate().is_err() {
            return None;
        }
        Some(Self::with_servers(servers, workspace))
    }

    fn with_servers(
        servers: Vec<(String, crate::config::types::LspServerConfig)>,
        workspace: Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        let root = workspace.root();
        tracing::debug!(
            "lsp: {} server definitions resolved (root {})",
            servers.len(),
            root.display()
        );
        let inner = Arc::new(Inner {
            workspace,
            servers,
            clients: tokio::sync::Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            diags: DiagStore::default(),
            diag_notify: Arc::new(Notify::new()),
            publish_waits: Mutex::new(HashMap::new()),
            idle_timeout_secs: std::sync::atomic::AtomicU64::new(
                LspConfig::DEFAULT_IDLE_TIMEOUT_SECS,
            ),
            #[cfg(test)]
            active_bindings: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            peak_bindings: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            test_synced_documents: Arc::new(std::sync::Mutex::new(HashMap::new())),
        });
        register_live_manager(&inner);
        Self { inner }
    }

    pub fn resolve_path(&self, path: &Path) -> Result<PathBuf, String> {
        self.inner.workspace.validate()?;
        let requested = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.inner.workspace.root().join(path)
        };
        let resolved = std::fs::canonicalize(&requested).map_err(|error| {
            format!("lsp: failed to resolve '{}': {error}", requested.display())
        })?;
        if !path.is_absolute() && !resolved.starts_with(self.inner.workspace.root()) {
            return Err("lsp: file is outside the session workspace".to_string());
        }
        Ok(resolved)
    }

    /// Workspace scope whose cached diagnostics are exposed by the tool.
    pub fn root(&self) -> &Path {
        self.inner.workspace.root()
    }

    /// Whether any configured server claims this path's extension.
    pub fn handles(&self, path: &Path) -> bool {
        registry::server_for_path(&self.inner.servers, path).is_some()
    }

    /// Client for the server claiming `path`, spawning it on first use.
    /// `None` when no server matches or the current spawn attempt failed.
    async fn client_for(&self, path: &Path) -> Option<Arc<LspClient>> {
        if self.inner.workspace.validate().is_err()
            || !path.starts_with(self.inner.workspace.root())
        {
            return None;
        }
        let (name, cfg) = registry::server_for_path(&self.inner.servers, path)?;
        let mut clients = self.inner.clients.lock().await;
        if self.inner.workspace.validate().is_err()
            || !path.starts_with(self.inner.workspace.root())
        {
            return None;
        }
        if let Some(cached) = clients.get(name)
            && cached.is_usable()
        {
            // Touch under the clients lock: the idle watcher rechecks under
            // the same lock, so a client handed out here is never retired
            // out from under its caller.
            cached.touch();
            return Some(cached.clone());
        }
        if let Some(old) = clients.remove(name) {
            old.shutdown().await;
            self.record_server_failure(name);
            self.inner
                .diags
                .lock()
                .unwrap()
                .retain(|_, diagnostics| diagnostics.server != name.as_str());
        }
        if self.server_is_cooling_down(name) {
            tracing::debug!("lsp[{name}]: restart suppressed during failure cooldown");
            return None;
        }
        let spawned = LspClient::spawn(
            name,
            cfg,
            self.inner.workspace.root(),
            Some(self.inner.workspace.clone()),
            self.inner.diags.clone(),
            self.inner.diag_notify.clone(),
        )
        .await;
        let Some(spawned) = spawned else {
            self.record_server_failure(name);
            return None;
        };
        self.inner
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(name);
        clients.insert(name.clone(), spawned.clone());
        self.watch_idle(name, &spawned);
        Some(spawned)
    }

    /// Retires `client` once it has been unused for the idle timeout: removes
    /// it from the map, drops its cached diagnostics and stops the process. A
    /// later edit respawns it through [`Self::client_for`] without touching
    /// the failure cooldown, since retirement is not a failure. The watcher
    /// sleeps until the deadline and re-arms only if the client was used in
    /// the meantime; it never polls.
    fn watch_idle(&self, name: &str, client: &Arc<LspClient>) {
        let Some(idle) = self.idle_timeout() else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        let weak = Arc::downgrade(client);
        let name = name.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Some(deadline) = weak.upgrade().map(|client| client.idle_deadline(idle)) else {
                    return;
                };
                tokio::time::sleep_until(deadline).await;
                let (Some(inner), Some(client)) = (inner.upgrade(), weak.upgrade()) else {
                    return;
                };
                if !client.is_usable() {
                    return;
                }
                let mut clients = inner.clients.lock().await;
                if !clients
                    .get(&name)
                    .is_some_and(|current| Arc::ptr_eq(current, &client))
                {
                    return;
                }
                if client.idle_deadline(idle) > tokio::time::Instant::now() {
                    continue;
                }
                clients.remove(&name);
                inner
                    .diags
                    .lock()
                    .unwrap()
                    .retain(|_, diagnostics| diagnostics.server != name.as_str());
                tracing::debug!("lsp[{name}]: retiring after {idle:?} idle");
                client.detach_idle_task();
                client.shutdown().await;
                return;
            }
        });
        client.set_idle_task(task);
    }

    fn server_is_cooling_down(&self, name: &str) -> bool {
        self.inner
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .is_some_and(|failure| failure.cooling_down(Instant::now()))
    }

    fn record_server_failure(&self, name: &str) {
        let mut failures = self
            .inner
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = ServerFailureState::next(failures.get(name).copied(), Instant::now());
        tracing::debug!(
            "lsp[{name}]: failure {} recorded; retry after {:?}",
            next.consecutive,
            next.retry_after
        );
        failures.insert(name.to_string(), next);
    }

    /// Sync a canonical path already resolved by the caller's permission check.
    /// Reopening stays relative to the captured workspace: resolving again here
    /// would let a symlink replacement redirect an already-approved read.
    pub async fn notify_changed(&self, path: &Path) -> Option<u64> {
        let relative = path.strip_prefix(self.inner.workspace.root()).ok()?;
        self.notify_changed_relative(relative).await
    }

    pub async fn notify_changed_relative(&self, relative: &Path) -> Option<u64> {
        self.inner.workspace.validate().ok()?;
        let lookup = self.inner.workspace.root().join(relative);
        if !self.handles(&lookup) {
            return None;
        }
        let file = self.inner.workspace.open_relative(relative).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let document = client::read_document(tokio::fs::File::from_std(file))
            .await
            .ok()?;
        self.client_for(&lookup)
            .await?
            .sync_document(&lookup, document)
            .await
    }

    /// Stops and reaps every language-server process currently owned by this
    /// manager. A later edit may start fresh servers again.
    pub async fn shutdown(&self) {
        let clients = {
            let mut clients = self.inner.clients.lock().await;
            clients
                .drain()
                .map(|(_, client)| client)
                .collect::<Vec<_>>()
        };
        for client in clients {
            client.shutdown().await;
        }
    }

    /// Diagnostics block for one file, formatted for appending to a tool
    /// result. Waits up to `wait` for the publish following the last sync.
    /// `None` when the file is clean or has no server.
    #[cfg(test)]
    pub async fn diagnostics_block(&self, path: &Path, wait: Duration) -> Option<String> {
        self.diagnostics_block_since(path, wait, None).await
    }

    /// Wait from the counter captured by synchronization, including a publish
    /// that arrived before this future was polled. Without a completed sync,
    /// retain the existing bounded cache-refresh behavior.
    pub async fn diagnostics_block_since(
        &self,
        path: &Path,
        wait: Duration,
        baseline: Option<u64>,
    ) -> Option<String> {
        self.diagnostics_block_observed(path, wait, baseline)
            .await
            .0
    }

    /// Like [`Self::diagnostics_block_since`], also reporting whether the
    /// awaited publish arrived (`None` when no wait took place).
    async fn diagnostics_block_observed(
        &self,
        path: &Path,
        wait: Duration,
        baseline: Option<u64>,
    ) -> (Option<String>, Option<bool>) {
        if !self.handles(path) {
            return (None, None);
        }
        let Some(uri) = client::file_uri(path) else {
            return (None, None);
        };
        let (output, published) = self.diagnostics_block_wait(uri, wait, baseline).await;
        (output, Some(published))
    }

    async fn diagnostics_block_wait(
        &self,
        uri: String,
        wait: Duration,
        baseline: Option<u64>,
    ) -> (Option<String>, bool) {
        let mut published = false;
        // A production edit atomically replaces the file, so the previous
        // cache identity is expected to be stale here. Its version remains the
        // synchronization baseline while we wait for a publish tied to the new
        // identity; stale diagnostics themselves are never returned.
        let v0 = baseline.unwrap_or_else(|| {
            self.inner
                .diags
                .lock()
                .unwrap()
                .get(&uri)
                .map_or(0, |diagnostics| diagnostics.version)
        });
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Register before inspecting the version so a publish between the
            // check and await cannot be lost.
            let notified = self.inner.diag_notify.notified();
            tokio::pin!(notified);
            let current = self
                .inner
                .diags
                .lock()
                .unwrap()
                .get(&uri)
                .map(|d| d.version)
                .unwrap_or(0);
            if current > v0 {
                published = true;
                break;
            }
            if tokio::time::timeout_at(deadline, &mut notified)
                .await
                .is_err()
            {
                break; // timeout: use whatever is stored
            }
        }
        (self.format_stored_diagnostics(uri).await, published)
    }

    async fn format_stored_diagnostics(&self, uri: String) -> Option<String> {
        self.inner.workspace.validate().ok()?;
        let file = self.inner.diags.lock().unwrap().get(&uri)?.clone();
        let output = crate::agent::runner::spawn_blocking_scoped(move || {
            if !diagnostic_identity_is_current(&uri, &file) {
                return None;
            }
            format_file_diags(&file.server, &file.diagnostics)
        })
        .await
        .ok()
        .flatten();
        self.inner.workspace.validate().ok()?;
        output
    }

    /// Compact diagnostics block for one file. Errors and warnings only,
    /// capped at [`MAX_DIAG_LINES`]. `None` when the file is clean or has no
    /// server.
    pub async fn diagnostics_block_for_edit(
        &self,
        path: &Path,
        baseline: Option<u64>,
    ) -> Option<String> {
        self.diagnostics_block_adaptive(path, baseline).await
    }

    /// Edit-time wait that shortens itself for servers which did not publish
    /// within the last full window, and returns to the full window as soon as
    /// the server is seen publishing again.
    async fn diagnostics_block_adaptive(
        &self,
        path: &Path,
        baseline: Option<u64>,
    ) -> Option<String> {
        let server = registry::server_for_path(&self.inner.servers, path)?
            .0
            .clone();
        let uri = client::file_uri(path)?;
        let missed = {
            let mut waits = self
                .inner
                .publish_waits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let state = waits.entry(server.clone()).or_default();
            // A publish that landed after an earlier wait gave up proves the
            // server still republishes, only slowly.
            if let (Some((missed_uri, missed_version)), Some(baseline)) =
                (&state.missed_at, baseline)
                && *missed_uri == uri
                && baseline > *missed_version
            {
                *state = PublishWaitState::default();
            }
            state.missed
        };
        let wait = if missed {
            DIAG_WAIT_AFTER_MISS
        } else {
            DIAG_WAIT
        };
        let (output, published) = self
            .diagnostics_block_wait(uri.clone(), wait, baseline)
            .await;
        let mut waits = self
            .inner
            .publish_waits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = waits.entry(server).or_default();
        if published {
            *state = PublishWaitState::default();
        } else {
            state.missed = true;
            let version = self
                .inner
                .diags
                .lock()
                .unwrap()
                .get(&uri)
                .map_or(0, |diagnostics| diagnostics.version);
            state.missed_at = Some((uri, version));
        }
        output
    }

    pub async fn diagnostics_block_relative(
        &self,
        relative: &Path,
        wait: Duration,
        baseline: Option<u64>,
    ) -> Option<String> {
        let parent = self.resolve_relative(relative)?;
        self.diagnostics_block_since(&parent, wait, baseline).await
    }

    fn resolve_relative(&self, relative: &Path) -> Option<PathBuf> {
        self.inner.workspace.validate().ok()?;
        let parent = self
            .inner
            .workspace
            .root()
            .join(relative)
            .canonicalize()
            .ok()?;
        parent
            .starts_with(self.inner.workspace.root())
            .then_some(parent)
    }

    pub async fn diagnostics_block_for_relative_edit(
        &self,
        relative: &Path,
        baseline: Option<u64>,
    ) -> Option<String> {
        let parent = self.resolve_relative(relative)?;
        self.diagnostics_block_adaptive(&parent, baseline).await
    }

    /// Sorted cache candidates that contain at least one error or warning.
    /// The production cache is hard-capped at [`client::MAX_DIAGNOSTIC_FILES`],
    /// which also bounds this allocation and sort.
    /// Callers authorize the project scope before enumerating these opaque
    /// identifiers and bind them one at a time before inspecting a path.
    pub fn diagnostic_candidate_uris(&self) -> Vec<String> {
        let store = self.inner.diags.lock().unwrap();
        let mut uris: Vec<String> = store
            .iter()
            .filter(|(_, file)| {
                file.diagnostics
                    .iter()
                    .any(|diag| diag.severity <= Some(DiagnosticSeverity::WARNING))
            })
            .map(|(uri, _)| uri.clone())
            .collect();
        uris.sort();
        uris
    }

    /// Bind one cache candidate to its canonical regular-file identity. The
    /// canonicalization places Windows paths in the same extended-path form as
    /// the manager root; re-emitting the URI confirms it is still the exact
    /// cache identity rather than an alias.
    pub async fn bind_diagnostic_uri(&self, uri: &str) -> Option<BoundDiagnosticPath> {
        let decoded = client::file_path(uri)?;
        let path = tokio::fs::canonicalize(decoded).await.ok()?;
        if client::file_uri(&path).as_deref() != Some(uri) {
            return None;
        }
        let file = crate::fs::open_stable_file(&path).await.ok()?;
        let identity = crate::fs::checked_tokio_file_metadata(&file).await.ok()?;
        let matches_cache = self
            .inner
            .diags
            .lock()
            .unwrap()
            .get(uri)
            .and_then(|cached| cached.identity.as_ref())
            .is_some_and(|cached| crate::fs::ensure_same_file(&path, cached, &identity).is_ok());
        if !matches_cache {
            return None;
        }
        #[cfg(test)]
        {
            let active = self.inner.active_bindings.fetch_add(1, Ordering::SeqCst) + 1;
            self.inner.peak_bindings.fetch_max(active, Ordering::SeqCst);
        }
        Some(BoundDiagnosticPath {
            path,
            uri: uri.to_string(),
            identity,
            _file: file,
            #[cfg(test)]
            active_bindings: self.inner.active_bindings.clone(),
        })
    }

    /// Copy diagnostics only when the cache still names the exact object held
    /// by `binding`. The returned snapshot is independent of the file handle,
    /// so the caller can release it before authorizing the next candidate.
    pub async fn snapshot_bound_diagnostics(
        &self,
        binding: &BoundDiagnosticPath,
        remaining_lines: usize,
    ) -> Option<DiagnosticSnapshot> {
        let (version, content) = {
            let store = self.inner.diags.lock().unwrap();
            let cached = store.get(&binding.uri)?;
            if crate::fs::ensure_same_file(
                &binding.path,
                &binding.identity,
                cached.identity.as_ref()?,
            )
            .is_err()
            {
                return None;
            }
            (cached.version, cached.content)
        };
        if !client::content_matches(&binding.identity, content).await {
            return None;
        }
        let store = self.inner.diags.lock().unwrap();
        let cached = store.get(&binding.uri)?;
        if cached.version != version {
            return None;
        }
        let cache_identity = cached.identity.as_ref()?;
        if crate::fs::ensure_same_file(&binding.path, &binding.identity, cache_identity).is_err() {
            return None;
        }
        let display = binding
            .path
            .strip_prefix(self.inner.workspace.root())
            .map(|relative| relative.display().to_string())
            .unwrap_or_else(|_| binding.path.display().to_string());
        let mut interesting = cached
            .diagnostics
            .iter()
            .filter(|diag| diag.severity <= Some(DiagnosticSeverity::WARNING));
        let lines: Vec<String> = interesting
            .by_ref()
            .take(remaining_lines)
            .map(|diagnostic| format_diag_line(&display, diagnostic))
            .collect();
        let truncated = interesting.next().is_some();
        Some(DiagnosticSnapshot {
            uri: binding.uri.clone(),
            lines,
            truncated,
        })
    }

    pub fn all_diagnostics_block_for_snapshots(
        &self,
        snapshots: &[DiagnosticSnapshot],
    ) -> Option<String> {
        let mut entries: Vec<_> = snapshots.iter().collect();
        entries.sort_by(|a, b| a.uri.cmp(&b.uri));
        let mut out = String::new();
        for snapshot in entries {
            for line in &snapshot.lines {
                out.push_str(line);
            }
            if snapshot.truncated {
                out.push_str("\n  … (truncated)");
                return Some(out);
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }

    #[cfg(test)]
    pub(crate) async fn client_for_test(&self, path: &Path) -> Option<Arc<LspClient>> {
        self.client_for(path).await
    }

    #[cfg(test)]
    pub(crate) async fn cached_client_count(&self) -> usize {
        self.inner.clients.lock().await.len()
    }

    #[cfg(test)]
    pub(crate) fn peak_bound_diagnostic_count(&self) -> usize {
        self.inner.peak_bindings.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn diagnostic_cache_metrics(&self) -> (usize, usize, usize, usize, bool) {
        let store = self.inner.diags.lock().unwrap();
        let total_bytes = store.values().map(|entry| entry.cached_bytes).sum();
        let max_count = store
            .values()
            .map(|entry| entry.diagnostics.len())
            .max()
            .unwrap_or(0);
        let max_message = store
            .values()
            .flat_map(|entry| &entry.diagnostics)
            .map(|diagnostic| diagnostic.message.len())
            .max()
            .unwrap_or(0);
        let has_extension_payload =
            store
                .values()
                .flat_map(|entry| &entry.diagnostics)
                .any(|diagnostic| {
                    diagnostic.data.is_some()
                        || diagnostic.related_information.is_some()
                        || diagnostic.code_description.is_some()
                        || diagnostic.source.is_some()
                        || diagnostic.tags.is_some()
                        || diagnostic.code.is_some()
                });
        (
            store.len(),
            total_bytes,
            max_count,
            max_message,
            has_extension_payload,
        )
    }

    #[cfg(test)]
    pub(crate) fn diagnostic_cache_entry_metrics(&self, uri: &str) -> Option<(u64, usize)> {
        self.inner
            .diags
            .lock()
            .unwrap()
            .get(uri)
            .map(|entry| (entry.version, entry.diagnostics.len()))
    }

    #[cfg(test)]
    pub(crate) fn publish_diagnostics_for_test(
        &self,
        uri: &str,
        server: &str,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> bool {
        let stored = client::store_diagnostics(
            &self.inner.diags,
            server,
            &serde_json::json!({ "uri": uri, "diagnostics": diagnostics }),
            None,
        );
        if stored {
            self.inner.diag_notify.notify_waiters();
        }
        stored
    }

    #[cfg(test)]
    pub(crate) fn set_synced_document_for_test(
        &self,
        uri: &str,
        version: i64,
        allow_versionless: bool,
    ) {
        self.inner.test_synced_documents.lock().unwrap().insert(
            uri.to_string(),
            client::SyncedDocument {
                version,
                allow_versionless,
                identity: crate::fs::checked_file_metadata(
                    &std::fs::File::open(client::file_path(uri).unwrap()).unwrap(),
                )
                .unwrap(),
                content: crate::fs::ContentDigest::of(
                    &std::fs::read(client::file_path(uri).unwrap()).unwrap(),
                ),
                wire_uri: uri.to_string(),
                last_used: 0,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn publish_synced_diagnostics_for_test(
        &self,
        uri: &str,
        server: &str,
        published_version: Option<i64>,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> bool {
        let mut params = serde_json::json!({ "uri": uri, "diagnostics": diagnostics });
        if let Some(version) = published_version {
            params["version"] = serde_json::json!(version);
        }
        let stored = client::store_diagnostics(
            &self.inner.diags,
            server,
            &params,
            Some(&self.inner.test_synced_documents),
        );
        if stored {
            self.inner.diag_notify.notify_waiters();
        }
        stored
    }

    #[cfg(test)]
    pub(crate) fn publish_null_version_diagnostics_for_test(
        &self,
        uri: &str,
        server: &str,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> bool {
        let stored = client::store_diagnostics(
            &self.inner.diags,
            server,
            &serde_json::json!({
                "uri": uri,
                "version": serde_json::Value::Null,
                "diagnostics": diagnostics,
            }),
            Some(&self.inner.test_synced_documents),
        );
        if stored {
            self.inner.diag_notify.notify_waiters();
        }
        stored
    }

    /// Test hook: inject diagnostics as if a server had published them.
    #[cfg(test)]
    pub(crate) fn inject_diagnostics(
        &self,
        uri: &str,
        server: &str,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) {
        assert!(
            self.try_inject_diagnostics(uri, server, diagnostics),
            "test diagnostic injection exceeded the bounded cache",
        );
    }

    /// Test hook for cases that intentionally exercise bounded-cache rejection.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn try_inject_diagnostics(
        &self,
        uri: &str,
        server: &str,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> bool {
        let identity =
            client::file_path(uri).and_then(|path| crate::fs::checked_path_metadata(&path).ok());
        client::commit_diagnostics(
            &self.inner.diags,
            uri.to_string(),
            server,
            diagnostics,
            identity,
            None,
        )
    }
}

#[cfg(test)]
mod health_snapshot_tests {
    use super::*;

    fn manager(root: &crate::tests::OwnedTestDirectory) -> LspManager {
        LspManager::new(&LspConfig::default(), root.path().to_path_buf())
    }

    #[tokio::test]
    async fn lsp_health_cached_failures_and_busy_locks_are_read_only() {
        let root = crate::tests::OwnedTestDirectory::new("lsp-health-cached");
        let manager = manager(&root);
        let cold = manager.diagnostic_snapshot();
        assert!(cold.configured_count > 0);
        assert_eq!(cold.live_count, Some(0));
        assert_eq!(cold.failure_count, Some(0));
        manager.inner.failures.lock().unwrap().insert(
            "rust".into(),
            ServerFailureState {
                consecutive: 3,
                retry_after: Instant::now() + Duration::from_secs(30),
            },
        );
        manager.inner.failures.lock().unwrap().insert(
            "expired".into(),
            ServerFailureState {
                consecutive: 1,
                retry_after: Instant::now() - Duration::from_secs(1),
            },
        );
        let snapshot = manager.diagnostic_snapshot();
        let rows = snapshot.failures.unwrap();
        assert_eq!(snapshot.failure_count, Some(2));
        assert_eq!(rows[0].retry_in_ms, 0);
        assert_eq!(rows[1].consecutive, 3);
        assert!(rows[1].retry_in_ms > 0 && rows[1].retry_in_ms <= 30_000);
        let live_guard = manager.inner.clients.lock().await;
        let failure_guard = manager.inner.failures.lock().unwrap();
        let busy = manager.diagnostic_snapshot();
        assert_eq!(busy.configured_names, cold.configured_names);
        assert_eq!(busy.live_count, None);
        assert_eq!(busy.live_names, None);
        assert_eq!(busy.failure_count, None);
        assert_eq!(busy.failures, None);
        assert!(live_guard.is_empty());
        assert_eq!(failure_guard.len(), 2);
        drop(failure_guard);
        drop(live_guard);
        assert_eq!(manager.diagnostic_snapshot().failure_count, Some(2));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn lsp_health_bounds_untrusted_names_and_does_not_validate_workspace() {
        let root = crate::tests::OwnedTestDirectory::new("lsp-health-names");
        let mut cfg = LspConfig::default();
        for index in 0..64 {
            cfg.servers.insert(
                format!("{index:03}\n\u{1b}\u{202e}{}", "界".repeat(200)),
                crate::config::types::LspServerConfig {
                    command: "DO_NOT_RUN_SECRET_ARGV".into(),
                    env: [("KEY".into(), "DO_NOT_SHOW_SECRET_ENV".into())].into(),
                    ..Default::default()
                },
            );
        }
        let manager = LspManager::new(&cfg, root.path().to_path_buf());
        for (name, _) in &manager.inner.servers {
            manager.record_server_failure(name);
        }
        // A diagnostic snapshot needs no path authority revalidation or file reads.
        std::fs::remove_dir(root.path()).unwrap();
        let snapshot = manager.diagnostic_snapshot();
        assert!(snapshot.configured_count > HEALTH_MAX_NAMES);
        assert_eq!(snapshot.configured_names.len(), HEALTH_MAX_NAMES);
        assert_eq!(snapshot.failure_count, Some(snapshot.configured_count));
        assert_eq!(snapshot.failures.as_ref().unwrap().len(), HEALTH_MAX_NAMES);
        for name in snapshot.configured_names.iter().chain(
            snapshot
                .failures
                .as_ref()
                .unwrap()
                .iter()
                .map(|row| &row.name),
        ) {
            assert!(name.chars().count() <= HEALTH_MAX_NAME_CHARS);
            assert!(!name.chars().any(|ch| ch.is_control() || ch == '\u{202e}'));
        }
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("DO_NOT_RUN_SECRET_ARGV"));
        assert!(!encoded.contains("DO_NOT_SHOW_SECRET_ENV"));
        assert!(encoded.len() < 64 * 1024);
    }
}

fn diagnostic_identity_is_current(uri: &str, file: &client::FileDiags) -> bool {
    let Some(approved) = file.identity.as_ref() else {
        // Only raw unit-test fixtures omit identity; production insertion
        // always records one.
        return true;
    };
    let Some(path) = client::file_path(uri) else {
        return false;
    };
    let Ok(current) = crate::fs::checked_path_metadata(&path) else {
        return false;
    };
    if current.file_type().is_symlink() {
        return false;
    }
    crate::fs::ensure_same_file(&path, approved, &current).is_ok()
        && file
            .content
            .is_none_or(|content| content.matches_file(approved.handle()).unwrap_or(false))
}

/// "LSP diagnostics (server):" header + one line per error/warning, capped.
/// `None` when there is nothing worth reporting (clean edits stay silent).
fn format_file_diags(server: &str, diags: &[lsp_types::Diagnostic]) -> Option<String> {
    let mut sorted: Vec<_> = diags
        .iter()
        .filter(|d| d.severity <= Some(DiagnosticSeverity::WARNING))
        .collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by_key(|d| d.severity);
    let mut out = format!("\n\nLSP diagnostics ({server}):");
    for (i, d) in sorted.iter().enumerate() {
        if i >= MAX_DIAG_LINES {
            out.push_str("\n  … (truncated)");
            break;
        }
        out.push_str(&format_diag_line("", d));
    }
    Some(out)
}

fn format_diag_line(location_prefix: &str, d: &lsp_types::Diagnostic) -> String {
    let severity = match d.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        _ => "hint",
    };
    let line = d.range.start.line.saturating_add(1);
    let col = d.range.start.character.saturating_add(1);
    let message = d.message.lines().next().unwrap_or_default();
    let message = message.chars().take(200).collect::<String>();
    let where_ = if location_prefix.is_empty() {
        format!("{line}:{col}")
    } else {
        format!("{location_prefix}:{line}:{col}")
    };
    format!("\n  {where_} {severity}: {message}")
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn server_failure_cooldown_backs_off_and_caps() {
        let now = Instant::now();
        let first = ServerFailureState::next(None, now);
        let second = ServerFailureState::next(Some(first), now);
        let capped = (0..16).fold(second, |failure, _| {
            ServerFailureState::next(Some(failure), now)
        });

        assert_eq!(first.consecutive, 1);
        assert_eq!(
            first.retry_after.duration_since(now),
            Duration::from_secs(1)
        );
        assert_eq!(second.consecutive, 2);
        assert_eq!(
            second.retry_after.duration_since(now),
            Duration::from_secs(2)
        );
        assert_eq!(capped.consecutive, SERVER_FAILURE_COUNT_MAX);
        assert_eq!(capped.retry_after.duration_since(now), SERVER_RETRY_MAX);
        assert!(capped.cooling_down(now));
        assert!(!capped.cooling_down(capped.retry_after));
    }

    #[test]
    fn diagnostic_formatting_saturates_peer_positions() {
        let position = lsp_types::Position::new(u32::MAX, u32::MAX);
        let diagnostic = lsp_types::Diagnostic::new_simple(
            lsp_types::Range::new(position, position),
            "bounded".into(),
        );

        assert_eq!(
            format_diag_line("file.rs", &diagnostic),
            format!("\n  file.rs:{}:{} hint: bounded", u32::MAX, u32::MAX)
        );
    }
}

#[cfg(test)]
mod default44_ownership_tests {
    #[tokio::test]
    async fn lsp44_cancelled_automatic_preparation_retains_exact_blocking_owner_until_join() {
        use super::*;
        let root = crate::tests::OwnedTestDirectory::new("lsp44-held-lookup");
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let (scope, started, release) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        let outer = crate::agent::runner::AgentWorkScope::new();
        let mut preparation = Box::pin(outer.run(LspManager::prepare_automatic(
            HashMap::new(),
            None,
            workspace,
            scope.clone(),
        )));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    result = &mut preparation => panic!("lookup unexpectedly completed: {}", result.is_some()),
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                }
                if started.try_recv().is_ok() { break; }
            }
        }).await.unwrap();
        assert_eq!(scope.active_children(), 1);
        assert_eq!(
            outer.active_children(),
            0,
            "lookup must belong to the preparation owner"
        );
        drop(preparation);
        assert!(scope.is_cancelled());
        assert_eq!(
            scope.active_children(),
            1,
            "dropping caller cannot pretend the worker retired"
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
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
