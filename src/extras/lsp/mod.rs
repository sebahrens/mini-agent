//! LSP (Language Server Protocol) integration: spawns language servers for
//! files the agent edits and feeds diagnostics back into tool results.
//!
//! Enabled via `[lsp] enabled = true` (requires the `lsp` cargo feature).
//! Everything is fail-open: a missing server binary, a hung handshake, or a
//! crashed server only means "no diagnostics", never a failed edit.

pub(crate) mod client;
pub(crate) mod registry;
pub mod rpc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use lsp_types::DiagnosticSeverity;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use crate::config::types::LspConfig;
use client::{DiagStore, LspClient};

/// How long to wait for the `publishDiagnostics` that follows an edit before
/// falling back to whatever is already stored. Kept short so clean edits
/// stay fast; servers that don't republish identical diagnostics just time
/// out and reuse the previous (identical) set.
const DIAG_WAIT: Duration = Duration::from_millis(1000);

/// Max diagnostics lines appended to a tool result.
pub(crate) const MAX_DIAG_LINES: usize = 20;

#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Inner>,
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
    diags: DiagStore,
    diag_notify: Arc<Notify>,
    #[cfg(test)]
    active_bindings: Arc<AtomicUsize>,
    #[cfg(test)]
    peak_bindings: Arc<AtomicUsize>,
    #[cfg(test)]
    test_synced_documents: Arc<std::sync::Mutex<HashMap<String, client::SyncedDocument>>>,
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
    pub(crate) fn new(cfg: &LspConfig, workspace: impl Into<LspWorkspace>) -> Self {
        let workspace = match workspace.into() {
            LspWorkspace::Binding(workspace) => workspace,
            LspWorkspace::Path(path) => Arc::new(
                crate::paths::WorkspaceBinding::capture(&path)
                    .expect("LSP workspace must be a stable directory"),
            ),
        };
        let root = workspace.root();
        let servers = registry::resolve_servers(&cfg.servers);
        tracing::debug!(
            "lsp: {} server definitions resolved (root {})",
            servers.len(),
            root.display()
        );
        let inner = Arc::new(Inner {
            workspace,
            servers,
            clients: tokio::sync::Mutex::new(HashMap::new()),
            diags: DiagStore::default(),
            diag_notify: Arc::new(Notify::new()),
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
            && !cached.is_stopped()
        {
            return Some(cached.clone());
        }
        if let Some(old) = clients.remove(name) {
            old.shutdown().await;
            self.inner
                .diags
                .lock()
                .unwrap()
                .retain(|_, diagnostics| diagnostics.server != name.as_str());
        }
        let spawned = LspClient::spawn(
            name,
            cfg,
            self.inner.workspace.root(),
            self.inner.workspace.try_clone_directory_file().ok(),
            self.inner.diags.clone(),
            self.inner.diag_notify.clone(),
        )
        .await?;
        clients.insert(name.clone(), spawned.clone());
        Some(spawned)
    }

    /// Syncs a file's disk content with its language server (no-op when no
    /// server handles the extension or the server failed to start).
    pub async fn notify_changed(&self, path: &Path) {
        let Ok(path) = std::fs::canonicalize(path) else {
            return;
        };
        if !self.handles(&path) {
            return;
        }
        // Open and bind the file identity before a server is selected or
        // launched. If a path approved by the caller is replaced by a symlink
        // while permission is pending, the replacement content is never read
        // and never reaches an LSP process.
        let Ok(mut file) = crate::fs::open_stable_file(&path).await else {
            return;
        };
        let mut text = String::new();
        if file.read_to_string(&mut text).await.is_err() {
            return;
        }
        if let Some(client) = self.client_for(&path).await {
            client.sync_text(&path, text).await;
        }
    }

    pub async fn notify_changed_relative(&self, relative: &Path) {
        if self.inner.workspace.validate().is_err() {
            return;
        }
        let Ok(mut file) = self.inner.workspace.open_relative(relative) else {
            return;
        };
        let Ok(metadata) = file.metadata() else {
            return;
        };
        if !metadata.is_file() {
            return;
        }
        let mut text = String::new();
        use std::io::Read as _;
        if file.read_to_string(&mut text).is_err() {
            return;
        }
        let logical = client::workspace_service_root(self.inner.workspace.root()).join(relative);
        let lookup = self.inner.workspace.root().join(relative);
        if let Some(client) = self.client_for(&lookup).await {
            client.sync_text(&logical, text).await;
        }
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
    pub async fn diagnostics_block(&self, path: &Path, wait: Duration) -> Option<String> {
        if !self.handles(path) {
            return None;
        }
        let uri = client::file_uri(path)?;
        // A production edit atomically replaces the file, so the previous
        // cache identity is expected to be stale here. Its version remains the
        // synchronization baseline while we wait for a publish tied to the new
        // identity; stale diagnostics themselves are never returned.
        let v0 = self
            .inner
            .diags
            .lock()
            .unwrap()
            .get(&uri)
            .map(|d| d.version)
            .unwrap_or(0);
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
                break;
            }
            if tokio::time::timeout_at(deadline, &mut notified)
                .await
                .is_err()
            {
                break; // timeout: use whatever is stored
            }
        }
        let store = self.inner.diags.lock().unwrap();
        let file = store.get(&uri)?;
        if !diagnostic_identity_is_current(&uri, file) {
            return None;
        }
        format_file_diags(&file.server, &file.diagnostics)
    }

    /// Compact diagnostics block for one file. Errors and warnings only,
    /// capped at [`MAX_DIAG_LINES`]. `None` when the file is clean or has no
    /// server.
    pub async fn diagnostics_block_for_edit(&self, path: &Path) -> Option<String> {
        self.diagnostics_block(path, DIAG_WAIT).await
    }

    pub async fn diagnostics_block_relative(
        &self,
        relative: &Path,
        wait: Duration,
    ) -> Option<String> {
        let service = client::workspace_service_root(self.inner.workspace.root()).join(relative);
        self.diagnostics_block(&service, wait).await
    }

    pub async fn diagnostics_block_for_relative_edit(&self, relative: &Path) -> Option<String> {
        self.diagnostics_block_relative(relative, DIAG_WAIT).await
    }

    /// All files that currently have diagnostics, formatted for the
    /// `lsp_diagnostics` tool. `None` when everything is clean.
    pub fn all_diagnostics_block(&self) -> Option<String> {
        self.all_diagnostics_block_inner()
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
    pub fn snapshot_bound_diagnostics(
        &self,
        binding: &BoundDiagnosticPath,
        remaining_lines: usize,
    ) -> Option<DiagnosticSnapshot> {
        let store = self.inner.diags.lock().unwrap();
        let cached = store.get(&binding.uri)?;
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

    fn all_diagnostics_block_inner(&self) -> Option<String> {
        let store = self.inner.diags.lock().unwrap();
        let mut out = String::new();
        let mut lines = 0usize;
        let mut entries: Vec<_> = store.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (uri, file) in entries {
            if !diagnostic_identity_is_current(uri, file) {
                continue;
            }
            let interesting: Vec<_> = file
                .diagnostics
                .iter()
                .filter(|d| d.severity <= Some(DiagnosticSeverity::WARNING))
                .collect();
            if interesting.is_empty() {
                continue;
            }
            let display = client::file_path(uri)
                .map(|path| {
                    path.strip_prefix(self.inner.workspace.root())
                        .map(|relative| relative.display().to_string())
                        .unwrap_or_else(|_| path.display().to_string())
                })
                .unwrap_or_else(|| uri.clone());
            for d in interesting {
                if lines >= MAX_DIAG_LINES {
                    out.push_str("  … (truncated)\n");
                    return Some(out);
                }
                out.push_str(&format_diag_line(&display, d));
                lines += 1;
            }
        }
        if out.is_empty() { None } else { Some(out) }
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
        let identity =
            client::file_path(uri).and_then(|path| crate::fs::checked_path_metadata(&path).ok());
        let _ = client::commit_diagnostics(
            &self.inner.diags,
            uri.to_string(),
            server,
            diagnostics,
            identity,
        );
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
    let line = d.range.start.line + 1;
    let col = d.range.start.character + 1;
    let message = d.message.lines().next().unwrap_or_default();
    let message = message.chars().take(200).collect::<String>();
    let where_ = if location_prefix.is_empty() {
        format!("{line}:{col}")
    } else {
        format!("{location_prefix}:{line}:{col}")
    };
    format!("\n  {where_} {severity}: {message}")
}
