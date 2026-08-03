//! One running language server process: spawn, `initialize` handshake,
//! full-document sync, and `publishDiagnostics` collection into the shared
//! [`DiagStore`]. Everything is fail-open — callers get `None`/no-op on any
//! error so a broken server never breaks an edit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Notify, oneshot};

use super::rpc;
use crate::config::types::LspServerConfig;

/// Standards-compliant `file:` URI for an absolute path. Relative paths first
/// resolve against the process cwd. `url` handles platform-specific Windows
/// drive and UNC forms as well as UTF-8 and percent escaping.
pub(crate) fn file_uri(path: &Path) -> Option<String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    #[cfg(windows)]
    let abs = standard_windows_uri_path(&abs)?;
    url::Url::from_file_path(abs).ok().map(Into::into)
}

#[cfg(windows)]
fn standard_windows_uri_path(path: &Path) -> Option<PathBuf> {
    let path = path.to_str()?;
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        Some(PathBuf::from(format!(r"\\{rest}")))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        Some(PathBuf::from(rest))
    } else {
        Some(PathBuf::from(path))
    }
}

/// Decode a standards-compliant file URI using platform-aware drive/UNC path
/// handling. Query strings and fragments are rejected because they cannot be
/// part of a filesystem permission key.
pub(crate) fn file_path(uri: &str) -> Option<PathBuf> {
    if !valid_percent_escapes(uri.as_bytes()) {
        return None;
    }
    let uri = url::Url::parse(uri).ok()?;
    if uri.scheme() != "file" || uri.query().is_some() || uri.fragment().is_some() {
        return None;
    }
    let path = uri.to_file_path().ok()?;
    if path.to_str().is_none() {
        return None;
    }
    Some(path)
}

fn valid_percent_escapes(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if bytes
                .get(index + 1..index + 3)
                .is_none_or(|pair| !pair.iter().all(u8::is_ascii_hexdigit))
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

const INIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Diagnostics for one file, as last published by one server.
pub struct FileDiags {
    pub server: String,
    /// Bumped on every `publishDiagnostics` for this file — lets callers
    /// wait for the publish that follows their `didChange`.
    pub version: u64,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
    /// File identity at publication time. Aggregate and explicit reads drop
    /// the entry if the path is later replaced or becomes a symlink.
    pub identity: Option<std::fs::Metadata>,
    /// Conservative retained-memory accounting used by the global cache cap.
    pub cached_bytes: usize,
}

/// uri → latest diagnostics. Shared between the manager and every client's
/// reader task.
pub type DiagStore = Arc<Mutex<HashMap<String, FileDiags>>>;

/// Hard ceiling for distinct files retained in the diagnostic cache. Updates
/// to existing files remain allowed at the ceiling; new files are ignored.
pub(crate) const MAX_DIAGNOSTIC_FILES: usize = 256;
pub(crate) const MAX_DIAGNOSTICS_PER_FILE: usize = 256;
pub(crate) const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024;
pub(crate) const MAX_DIAGNOSTIC_CACHE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct SyncedDocument {
    pub(crate) version: i64,
    /// Versionless publishes are unambiguous only before the first change in
    /// an epoch, or after an exact versioned publish anchors that epoch.
    pub(crate) allow_versionless: bool,
}

pub struct LspClient {
    name: String,
    child: tokio::process::Child,
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// uri → last synced document version.
    open: Arc<Mutex<HashMap<String, SyncedDocument>>>,
}

impl LspClient {
    /// Spawns the server and runs the `initialize` handshake. Returns `None`
    /// (with a log) on any failure: missing binary, spawn error, init timeout.
    pub async fn spawn(
        name: &str,
        cfg: &LspServerConfig,
        root: &Path,
        diags: DiagStore,
        diag_notify: Arc<Notify>,
    ) -> Option<Arc<Self>> {
        let mut child = tokio::process::Command::new(cfg.command.as_str())
            .args(cfg.args.iter().map(|a| a.as_str()))
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                tracing::debug!("lsp[{name}]: cannot spawn '{}': {e}", cfg.command);
                e
            })
            .ok()?;

        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let stderr = child.stderr.take()?;

        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let open = Arc::new(Mutex::new(HashMap::new()));

        // Reader task: routes responses to pending requests, stores
        // diagnostics, and answers server→client requests with null so a
        // server never hangs waiting on us.
        {
            let pending = pending.clone();
            let stdin = stdin.clone();
            let open = open.clone();
            let server_name = name.to_string();
            tokio::spawn(async move {
                let mut stdout = stdout;
                loop {
                    let frame = match rpc::read_frame(&mut stdout).await {
                        Ok(Some(f)) => f,
                        Ok(None) => break, // clean EOF: server exited
                        Err(e) => {
                            tracing::debug!("lsp[{server_name}]: read error: {e}");
                            break;
                        }
                    };
                    let Ok(msg) = serde_json::from_slice::<Value>(&frame) else {
                        continue;
                    };
                    let method = msg.get("method").and_then(Value::as_str);
                    let id = msg.get("id").and_then(Value::as_i64);
                    match (method, id) {
                        // Server→client request: reply null, we declare no
                        // capabilities that would legitimately trigger one.
                        (Some(_), Some(id)) => {
                            let reply = json!({"jsonrpc": "2.0", "id": id, "result": Value::Null});
                            let body = serde_json::to_vec(&reply).unwrap_or_default();
                            let mut w = stdin.lock().await;
                            let _ = rpc::write_frame(&mut *w, &body).await;
                        }
                        // Server→client notification.
                        (Some(m), None) => {
                            if m == "textDocument/publishDiagnostics"
                                && let Some(params) = msg.get("params")
                            {
                                if store_diagnostics(&diags, &server_name, params, Some(&open)) {
                                    diag_notify.notify_waiters();
                                }
                            }
                        }
                        // Response to one of our requests.
                        (None, Some(id)) => {
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(msg);
                            }
                        }
                        _ => {}
                    }
                }
                // Server died: fail every outstanding request.
                pending.lock().unwrap().clear();
            });
        }

        // Drain stderr into the trace log so noisy servers don't fill pipes.
        {
            let server_name = name.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::trace!("lsp[{server_name}] stderr: {line}");
                }
            });
        }

        let client = Arc::new(Self {
            name: name.to_string(),
            child,
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            open,
        });

        let root_uri = file_uri(root)
            .ok_or_else(|| {
                tracing::debug!("lsp[{name}]: root '{}' is not a valid uri", root.display());
            })
            .ok()?;
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "didSave": false
                    },
                    "publishDiagnostics": { "versionSupport": true }
                }
            },
            "initializationOptions": cfg.initialization.clone().unwrap_or(Value::Null),
            "clientInfo": { "name": "zerostack", "version": env!("CARGO_PKG_VERSION") }
        });
        client
            .request("initialize", init_params, INIT_TIMEOUT)
            .await?;
        client.notify("initialized", json!({})).await;
        tracing::info!("lsp[{name}]: initialized (root {})", root.display());
        Some(client)
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let body = serde_json::to_vec(&msg).ok()?;
        {
            let mut w = self.stdin.lock().await;
            rpc::write_frame(&mut *w, &body).await.ok()?;
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Some(resp),
            _ => {
                self.pending.lock().unwrap().remove(&id);
                tracing::debug!("lsp[{}]: '{}' timed out", self.name, method);
                None
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Ok(body) = serde_json::to_vec(&msg) {
            let mut w = self.stdin.lock().await;
            let _ = rpc::write_frame(&mut *w, &body).await;
        }
    }

    /// Sends caller-verified content to the server: `didOpen` on first touch,
    /// full-content `didChange` afterwards. Disk access is deliberately owned
    /// by `LspManager`, which binds a stable file handle after authorization.
    pub async fn sync_file(&self, path: &Path, text: String) {
        let Some(uri) = file_uri(path) else {
            return;
        };
        let uri_str = uri.clone();
        enum Sync {
            Open,
            Change(i64),
        }
        let action = {
            let mut open = self.open.lock().unwrap();
            match open.get_mut(&uri_str) {
                Some(document) => {
                    document.version += 1;
                    document.allow_versionless = false;
                    Sync::Change(document.version)
                }
                None => {
                    open.insert(
                        uri_str,
                        SyncedDocument {
                            version: 1,
                            allow_versionless: true,
                        },
                    );
                    Sync::Open
                }
            }
        }; // lock released before any await
        match action {
            Sync::Open => {
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(path),
                            "version": 1,
                            "text": text
                        }
                    }),
                )
                .await;
            }
            Sync::Change(v) => {
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": v },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await;
            }
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub(crate) fn store_diagnostics(
    diags: &DiagStore,
    server: &str,
    params: &Value,
    synced_versions: Option<&Mutex<HashMap<String, SyncedDocument>>>,
) -> bool {
    let Some(raw_uri) = params.get("uri").and_then(Value::as_str) else {
        return false;
    };
    let Ok(diagnostics) = serde_json::from_value::<Vec<lsp_types::Diagnostic>>(
        params.get("diagnostics").cloned().unwrap_or(Value::Null),
    ) else {
        return false;
    };
    let Some(path) = file_path(raw_uri) else {
        return false;
    };
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(identity) = std::fs::symlink_metadata(&canonical) else {
        return false;
    };
    if !identity.is_file() || identity.file_type().is_symlink() {
        return false;
    }
    let Some(uri) = file_uri(&canonical) else {
        return false;
    };
    // Cache only the one canonical URI spelling emitted by this client.
    // This rejects symlink aliases, dot components, case/escape variants,
    // and stale file URIs that cannot be rebound to a verified path.
    if uri != raw_uri {
        return false;
    }
    let mut synced_guard = synced_versions.map(|versions| versions.lock().unwrap());
    let exact_version_anchor = if let Some(synced_versions) = synced_guard.as_mut() {
        let Some(synced) = synced_versions.get_mut(&uri) else {
            return false;
        };
        match params.get("version") {
            Some(version) if !version.is_null() => {
                let Some(published_version) = version.as_i64() else {
                    return false;
                };
                if published_version != synced.version {
                    return false;
                }
                true
            }
            None | Some(_) if synced.allow_versionless => false,
            None | Some(_) => return false,
        }
    } else {
        false
    };
    let stored = commit_diagnostics(&diags, uri.clone(), server, diagnostics, Some(identity));
    if stored
        && exact_version_anchor
        && let Some(synced_versions) = synced_guard.as_mut()
        && let Some(synced) = synced_versions.get_mut(&uri)
    {
        synced.allow_versionless = true;
    }
    stored
}

pub(crate) fn commit_diagnostics(
    diags: &DiagStore,
    uri: String,
    server: &str,
    diagnostics: Vec<lsp_types::Diagnostic>,
    identity: Option<std::fs::Metadata>,
) -> bool {
    let diagnostics = sanitize_diagnostics(diagnostics);
    let cached_bytes = retained_diagnostic_bytes(&uri, server, &diagnostics);
    let mut store = diags.lock().unwrap();
    if !store.contains_key(&uri) && store.len() >= MAX_DIAGNOSTIC_FILES {
        return false;
    }
    let old_bytes = store.get(&uri).map(|entry| entry.cached_bytes).unwrap_or(0);
    let current_bytes: usize = store.values().map(|entry| entry.cached_bytes).sum();
    let replacement_bytes = current_bytes
        .saturating_sub(old_bytes)
        .saturating_add(cached_bytes);
    if replacement_bytes > MAX_DIAGNOSTIC_CACHE_BYTES {
        // This is still a valid, newer publish. If it replaces an existing
        // URI, retain a tiny empty tombstone rather than the stale older
        // diagnostics. Its incremented version wakes post-edit waiters, while
        // empty diagnostics make explicit and aggregate queries disclose
        // nothing from the superseded publish.
        let Some(entry) = store.get_mut(&uri) else {
            return false;
        };
        let mut tombstone_bytes = retained_diagnostic_bytes(&uri, server, &[]);
        let retain_server = current_bytes
            .saturating_sub(old_bytes)
            .saturating_add(tombstone_bytes)
            <= MAX_DIAGNOSTIC_CACHE_BYTES;
        if !retain_server {
            tombstone_bytes = retained_diagnostic_bytes(&uri, "", &[]);
        }
        entry.server.clear();
        if retain_server {
            entry.server.push_str(server);
        }
        entry.version += 1;
        entry.diagnostics.clear();
        entry.identity = identity;
        entry.cached_bytes = tombstone_bytes;
        return true;
    }
    let entry = store.entry(uri).or_insert_with(|| FileDiags {
        server: server.to_string(),
        version: 0,
        diagnostics: Vec::new(),
        identity: None,
        cached_bytes: 0,
    });
    entry.server.clear();
    entry.server.push_str(server);
    entry.version += 1;
    entry.diagnostics = diagnostics;
    entry.identity = identity;
    entry.cached_bytes = cached_bytes;
    true
}

fn sanitize_diagnostics(diagnostics: Vec<lsp_types::Diagnostic>) -> Vec<lsp_types::Diagnostic> {
    diagnostics
        .into_iter()
        .take(MAX_DIAGNOSTICS_PER_FILE)
        .map(|diagnostic| lsp_types::Diagnostic {
            range: diagnostic.range,
            severity: diagnostic.severity,
            message: truncate_utf8_bytes(&diagnostic.message, MAX_DIAGNOSTIC_MESSAGE_BYTES),
            // Only range, severity, and message are rendered. Discard all
            // extension payloads (especially arbitrary JSON `data`) before
            // they can consume retained cache memory.
            ..Default::default()
        })
        .collect()
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn retained_diagnostic_bytes(
    uri: &str,
    server: &str,
    diagnostics: &[lsp_types::Diagnostic],
) -> usize {
    uri.len().saturating_add(server.len()).saturating_add(
        diagnostics
            .iter()
            .map(|diagnostic| {
                std::mem::size_of::<lsp_types::Diagnostic>()
                    .saturating_add(diagnostic.message.len())
            })
            .sum::<usize>(),
    )
}

/// LSP `languageId` for didOpen. Servers mostly infer from the extension,
/// but a correct id avoids ambiguity on shared extensions (`.h`, `.m`).
fn language_id(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "go" => "go",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "sh" | "bash" | "zsh" => "shellscript",
        "lua" => "lua",
        other => match other {
            "java" => "java",
            "rb" => "ruby",
            "php" => "php",
            "cs" => "csharp",
            "swift" => "swift",
            "kt" | "kts" => "kotlin",
            "nix" => "nix",
            "yaml" | "yml" => "yaml",
            "json" => "json",
            "toml" => "toml",
            "md" => "markdown",
            _ => "plaintext",
        },
    }
}
