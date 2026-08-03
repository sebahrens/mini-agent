//! One running language server process: spawn, `initialize` handshake,
//! full-document sync, and `publishDiagnostics` collection into the shared
//! [`DiagStore`]. Everything is fail-open — callers get `None`/no-op on any
//! error so a broken server never breaks an edit.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::{Notify, oneshot};

use super::rpc;
use crate::config::types::LspServerConfig;

pub(crate) async fn read_stable_text(path: &Path) -> std::io::Result<String> {
    let mut file = crate::fs::open_stable_file(path).await?;
    let mut text = String::new();
    file.read_to_string(&mut text).await?;
    Ok(text)
}

/// `file://` URI for a path, with minimal percent-encoding (anything outside
/// RFC 3986 unreserved + `/` is hex-escaped). Relative paths resolve against
/// the caller-provided workspace root.
pub(crate) fn file_uri(path: &Path) -> Option<String> {
    let abs = path.is_absolute().then(|| path.to_path_buf())?;
    let s = abs.to_str()?;
    let mut out = String::with_capacity(s.len() + 7);
    out.push_str("file://");
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(out)
}

const INIT_TIMEOUT: Duration = Duration::from_secs(15);
const LSP_WORKSPACE_FD: i32 = 198;

pub(crate) fn workspace_service_root(_fallback: &Path) -> std::path::PathBuf {
    #[cfg(all(unix, target_os = "linux"))]
    return std::path::PathBuf::from(format!("/proc/self/fd/{LSP_WORKSPACE_FD}"));
    #[cfg(all(unix, not(target_os = "linux")))]
    return std::path::PathBuf::from(format!("/dev/fd/{LSP_WORKSPACE_FD}"));
    #[cfg(not(unix))]
    return _fallback.to_path_buf();
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn bind_workspace_handle(
    command: &mut tokio::process::Command,
    workspace: Option<std::fs::File>,
    fallback: &Path,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let Some(workspace) = workspace else {
        return Ok(fallback.to_path_buf());
    };
    let source = workspace.as_raw_fd();
    unsafe {
        command.as_std_mut().pre_exec(move || {
            let _keep_workspace_alive = &workspace;
            if libc::dup2(source, LSP_WORKSPACE_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // dup2 does not clear CLOEXEC when source == destination.
            let descriptor_flags = libc::fcntl(LSP_WORKSPACE_FD, libc::F_GETFD);
            if descriptor_flags == -1
                || libc::fcntl(
                    LSP_WORKSPACE_FD,
                    libc::F_SETFD,
                    descriptor_flags & !libc::FD_CLOEXEC,
                ) == -1
                || libc::fchdir(LSP_WORKSPACE_FD) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(workspace_service_root(fallback))
}

#[cfg(not(unix))]
fn bind_workspace_handle(
    _command: &mut tokio::process::Command,
    _workspace: Option<std::fs::File>,
    fallback: &Path,
) -> std::io::Result<std::path::PathBuf> {
    Ok(fallback.to_path_buf())
}

/// Diagnostics for one file, as last published by one server.
pub struct FileDiags {
    pub server: String,
    /// Bumped on every `publishDiagnostics` for this file — lets callers
    /// wait for the publish that follows their `didChange`.
    pub version: u64,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
}

/// uri → latest diagnostics. Shared between the manager and every client's
/// reader task.
pub type DiagStore = Arc<Mutex<HashMap<String, FileDiags>>>;

pub struct LspClient {
    name: String,
    child: tokio::process::Child,
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// uri → last synced document version.
    open: Mutex<HashMap<String, i64>>,
}

impl LspClient {
    /// Spawns the server and runs the `initialize` handshake. Returns `None`
    /// (with a log) on any failure: missing binary, spawn error, init timeout.
    pub async fn spawn(
        name: &str,
        cfg: &LspServerConfig,
        root: &Path,
        workspace_handle: Option<std::fs::File>,
        diags: DiagStore,
        diag_notify: Arc<Notify>,
    ) -> Option<Arc<Self>> {
        let mut command = tokio::process::Command::new(cfg.command.as_str());
        command
            .args(cfg.args.iter().map(|a| a.as_str()))
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(root)
            .kill_on_drop(true);
        let server_root = bind_workspace_handle(&mut command, workspace_handle, root).ok()?;
        let mut child = command
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

        // Reader task: routes responses to pending requests, stores
        // diagnostics, and answers server→client requests with null so a
        // server never hangs waiting on us.
        {
            let pending = pending.clone();
            let stdin = stdin.clone();
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
                                store_diagnostics(&diags, &server_name, params);
                                diag_notify.notify_waiters();
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
            open: Mutex::new(HashMap::new()),
        });

        let root_uri = file_uri(&server_root)
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
                    "publishDiagnostics": {}
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

    /// Syncs a file's current disk content with the server: `didOpen` on
    /// first touch, full-content `didChange` afterwards.
    pub async fn sync_file(&self, path: &Path) {
        let Ok(text) = read_stable_text(path).await else {
            return; // unreadable/binary file — skip silently
        };
        self.sync_text(path, text).await;
    }

    pub async fn sync_text(&self, path: &Path, text: String) {
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
                Some(version) => {
                    *version += 1;
                    Sync::Change(*version)
                }
                None => {
                    open.insert(uri_str, 1);
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

fn store_diagnostics(diags: &DiagStore, server: &str, params: &Value) {
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return;
    };
    let diagnostics: Vec<lsp_types::Diagnostic> =
        serde_json::from_value(params.get("diagnostics").cloned().unwrap_or(Value::Null))
            .unwrap_or_default();
    let mut store = diags.lock().unwrap();
    let entry = store.entry(uri.to_string()).or_insert_with(|| FileDiags {
        server: server.to_string(),
        version: 0,
        diagnostics: Vec::new(),
    });
    entry.version += 1;
    entry.diagnostics = diagnostics;
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
