//! One running language server process: spawn, `initialize` handshake,
//! full-document sync, and `publishDiagnostics` collection into the shared
//! [`DiagStore`]. Everything is fail-open — callers get `None`/no-op on any
//! error so a broken server never breaks an edit.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use process_wrap::tokio::ChildWrapper;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::rpc;
use crate::config::types::{LspNetwork, LspServerConfig};
use crate::process_creation::CommandWrapCreationExt;
use crate::sandbox::{Sandbox, owned_workspace_service_tree};

pub(crate) async fn read_stable_text(path: &Path) -> std::io::Result<String> {
    let mut file = crate::fs::open_stable_file(path).await?;
    let mut text = String::new();
    file.read_to_string(&mut text).await?;
    Ok(text)
}

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
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const STDERR_LIMIT: usize = 64 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC_FILES_PER_SERVER: usize = 128;
pub(crate) const MAX_DIAGNOSTICS_PER_FILE: usize = 50;
const MAX_DIAGNOSTIC_URI_BYTES: usize = 4 * 1024;
pub(crate) const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024;
const MAX_DIAGNOSTIC_METADATA_BYTES: usize = 256;
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
    /// File identity at publication time. Aggregate and explicit reads drop
    /// the entry if the path is later replaced or becomes a symlink.
    pub identity: Option<crate::fs::CheckedMetadata>,
    /// Conservative retained-memory accounting used by the global cache cap.
    pub cached_bytes: usize,
}

/// uri → latest diagnostics. Shared between the manager and every client's
/// reader task.
pub type DiagStore = Arc<Mutex<HashMap<String, FileDiags>>>;

/// Hard ceiling for distinct files retained in the diagnostic cache. Updates
/// to existing files remain allowed at the ceiling; new files are ignored.
pub(crate) const MAX_DIAGNOSTIC_FILES: usize = MAX_DIAGNOSTIC_FILES_PER_SERVER;
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
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    shutdown_tx: mpsc::UnboundedSender<()>,
    stopped: Arc<AtomicBool>,
    stopped_notify: Arc<Notify>,
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
        workspace_handle: Option<std::fs::File>,
        diags: DiagStore,
        diag_notify: Arc<Notify>,
    ) -> Option<Arc<Self>> {
        Self::spawn_with_timeout_and_workspace(
            name,
            cfg,
            root,
            workspace_handle,
            diags,
            diag_notify,
            INIT_TIMEOUT,
        )
        .await
    }

    pub(crate) async fn spawn_with_timeout(
        name: &str,
        cfg: &LspServerConfig,
        root: &Path,
        diags: DiagStore,
        diag_notify: Arc<Notify>,
        initialize_timeout: Duration,
    ) -> Option<Arc<Self>> {
        Self::spawn_with_timeout_and_workspace(
            name,
            cfg,
            root,
            None,
            diags,
            diag_notify,
            initialize_timeout,
        )
        .await
    }

    async fn spawn_with_timeout_and_workspace(
        name: &str,
        cfg: &LspServerConfig,
        root: &Path,
        workspace_handle: Option<std::fs::File>,
        diags: DiagStore,
        diag_notify: Arc<Notify>,
        initialize_timeout: Duration,
    ) -> Option<Arc<Self>> {
        let root = canonical_workspace_root(root)
            .map_err(|error| tracing::debug!("lsp[{name}]: invalid root: {error}"))
            .ok()?;
        let mut command = lsp_command(cfg, &root)
            .map_err(|error| {
                tracing::debug!("lsp[{name}]: launch denied: {error}");
            })
            .ok()?;
        let server_root = bind_workspace_handle(&mut command, workspace_handle, &root)
            .map_err(|error| tracing::debug!("lsp[{name}]: workspace bind failed: {error}"))
            .ok()?;
        let root_uri = file_uri(&server_root)
            .ok_or_else(|| {
                tracing::debug!("lsp[{name}]: root '{}' is not a valid uri", root.display());
            })
            .ok()?;
        let mut child = owned_workspace_service_tree(command)
            .spawn_guarded()
            .map_err(|error| {
                tracing::debug!("lsp[{name}]: spawn failed: {error}");
            })
            .ok()?;
        let process_group = child.id();
        let pipes = (
            take_pipe(child.stdin(), "stdin", name),
            take_pipe(child.stdout(), "stdout", name),
            take_pipe(child.stderr(), "stderr", name),
        );
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            terminate_and_reap(name, &mut child, process_group).await;
            return None;
        };

        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_notify = Arc::new(Notify::new());
        let open = Arc::new(Mutex::new(HashMap::new()));

        // Reader task: routes responses to pending requests, stores
        // diagnostics, and answers server→client requests with null so a
        // server never hangs waiting on us.
        let reader_task = {
            let pending = pending.clone();
            let stdin = stdin.clone();
            let open = open.clone();
            let server_name = name.to_string();
            let workspace_uri = root_uri.clone();
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let mut stdout = stdout;
                loop {
                    let frame = match rpc::read_frame(&mut stdout).await {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!("lsp[{server_name}]: read error: {e}");
                            break;
                        }
                    };
                    let Ok(msg) = serde_json::from_slice::<Value>(&frame) else {
                        tracing::debug!("lsp[{server_name}]: malformed JSON-RPC frame");
                        break;
                    };
                    let method = msg.get("method").and_then(Value::as_str);
                    let id = msg.get("id").and_then(Value::as_i64);
                    match (method, id) {
                        // Server→client request: reply null, we declare no
                        // capabilities that would legitimately trigger one.
                        (Some(_), Some(id)) => {
                            let reply = json!({"jsonrpc": "2.0", "id": id, "result": Value::Null});
                            let body = serde_json::to_vec(&reply).unwrap_or_default();
                            if !write_owned_frame(&stdin, &shutdown_tx, &body).await {
                                break;
                            }
                        }
                        // Server→client notification.
                        (Some(m), None) => {
                            if m == "textDocument/publishDiagnostics"
                                && let Some(params) = msg.get("params")
                            {
                                match validate_diagnostic_envelope(&workspace_uri, params) {
                                    DiagnosticStoreOutcome::Stored => {
                                        if store_diagnostics(
                                            &diags,
                                            &server_name,
                                            params,
                                            Some(&open),
                                        ) {
                                            diag_notify.notify_waiters();
                                        }
                                    }
                                    DiagnosticStoreOutcome::Ignored => {}
                                    DiagnosticStoreOutcome::LimitExceeded => {
                                        tracing::debug!(
                                            "lsp[{server_name}]: diagnostic storage limit exceeded"
                                        );
                                        let _ = shutdown_tx.send(());
                                        break;
                                    }
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
                let _ = shutdown_tx.send(());
            })
        };

        // Drain stderr in fixed chunks. Content is never retained or logged
        // because diagnostics may carry source or secrets. A cumulative cap
        // prevents a configured child from consuming unbounded pipe traffic.
        let stderr_task = {
            let server_name = name.to_string();
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let mut stderr = stderr;
                let mut observed = 0usize;
                let mut chunk = [0_u8; 4096];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            observed = observed.saturating_add(read);
                            if observed > STDERR_LIMIT {
                                tracing::debug!("lsp[{server_name}]: stderr byte limit exceeded");
                                let _ = shutdown_tx.send(());
                                break;
                            }
                        }
                    }
                }
            })
        };

        tokio::spawn(supervise_child(
            name.to_string(),
            child,
            process_group,
            shutdown_rx,
            reader_task,
            stderr_task,
            stopped.clone(),
            stopped_notify.clone(),
        ));

        let client = Arc::new(Self {
            name: name.to_string(),
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            shutdown_tx,
            stopped,
            stopped_notify,
            open,
        });

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
            "clientInfo": {
                "name": crate::product::PUBLIC_NAME,
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let initialized = client
            .request("initialize", init_params, initialize_timeout)
            .await;
        if initialized
            .as_ref()
            .and_then(|response| response.get("result"))
            .and_then(Value::as_object)
            .is_none()
        {
            client.shutdown().await;
            return None;
        }
        if !client.notify("initialized", json!({})).await {
            client.shutdown().await;
            return None;
        }
        tracing::info!("lsp[{name}]: initialized (root {})", root.display());
        Some(client)
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        // Removing the entry in Drop also covers cancellation of this future
        // while it is blocked on the writer lock or response channel.
        let _pending = PendingRequest::new(self.pending.clone(), id);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let body = match serde_json::to_vec(&msg) {
            Ok(body) => body,
            Err(_) => return None,
        };
        if !write_owned_frame(&self.stdin, &self.shutdown_tx, &body).await {
            return None;
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Some(resp),
            _ => {
                tracing::debug!("lsp[{}]: '{}' timed out", self.name, method);
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn request_for_test(&self, timeout: Duration) -> Option<Value> {
        self.request("mini-agent/test", json!({}), timeout).await
    }

    #[cfg(test)]
    pub(crate) fn pending_len_for_test(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    async fn notify(&self, method: &str, params: Value) -> bool {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Ok(body) = serde_json::to_vec(&msg) {
            return write_owned_frame(&self.stdin, &self.shutdown_tx, &body).await;
        }
        false
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(crate) async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
        if self.is_stopped() {
            return;
        }
        let notified = self.stopped_notify.notified();
        if self.is_stopped() {
            return;
        }
        notified.await;
    }

    /// Sends caller-verified content to the server: `didOpen` on first touch,
    /// full-content `didChange` afterwards. Disk access is deliberately owned
    /// by `LspManager`, which binds a stable file handle after authorization.
    pub async fn sync_file(&self, path: &Path) {
        let Ok(text) = read_stable_text(path).await else {
            return;
        };
        self.sync_text(path, text).await;
    }

    pub async fn sync_text(&self, path: &Path, text: String) {
        let Some(uri) = file_uri(path) else {
            return;
        };
        if text.len() as u64 > MAX_DOCUMENT_BYTES {
            tracing::debug!(
                "lsp[{}]: document exceeds synchronization byte limit",
                self.name
            );
            return;
        }
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
        let sent = match action {
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
                .await
            }
            Sync::Change(v) => {
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": v },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await
            }
        };
        if sent {
            return;
        }
        // A failed protocol write makes the cached client unusable. Complete
        // process-tree cleanup so the next matching edit can start fresh.
        self.shutdown().await;
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
    }
}

fn take_pipe<T>(pipe: &mut Option<T>, kind: &str, name: &str) -> Option<T> {
    pipe.take().or_else(|| {
        tracing::debug!("lsp[{name}]: child did not provide piped {kind}");
        None
    })
}

struct PendingRequest {
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    id: i64,
}

impl PendingRequest {
    fn new(pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>, id: i64) -> Self {
        Self { pending, id }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

struct TransportWriteGuard {
    shutdown_tx: mpsc::UnboundedSender<()>,
    complete: bool,
}

impl Drop for TransportWriteGuard {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.shutdown_tx.send(());
        }
    }
}

async fn write_owned_frame(
    stdin: &Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    shutdown_tx: &mpsc::UnboundedSender<()>,
    body: &[u8],
) -> bool {
    let mut write = TransportWriteGuard {
        shutdown_tx: shutdown_tx.clone(),
        complete: false,
    };
    let mut stdin = stdin.lock().await;
    let result = rpc::write_frame(&mut *stdin, body).await;
    write.complete = result.is_ok();
    result.is_ok()
}

async fn supervise_child(
    name: String,
    mut child: Box<dyn ChildWrapper>,
    process_group: Option<u32>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    mut reader_task: JoinHandle<()>,
    mut stderr_task: JoinHandle<()>,
    stopped: Arc<AtomicBool>,
    stopped_notify: Arc<Notify>,
) {
    // Poll the direct child as well as protocol tasks. A crashed server can
    // leave a descendant holding inherited stdio open; waiting only for EOF
    // or the whole process group would then hang forever.
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            _ = poll.tick() => {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::debug!("lsp[{name}]: child status failed: {error}");
                        break;
                    }
                }
            }
        }
    }
    terminate_and_reap(&name, &mut child, process_group).await;
    for task in [&mut reader_task, &mut stderr_task] {
        if tokio::time::timeout(TASK_DRAIN_TIMEOUT, &mut *task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
    stopped.store(true, Ordering::Release);
    stopped_notify.notify_waiters();
}

async fn terminate_and_reap(
    name: &str,
    child: &mut Box<dyn ChildWrapper>,
    process_group: Option<u32>,
) {
    if let Err(error) = child.start_kill() {
        // An already-exited direct child reports an error here; wait still
        // returns its cached status and reaps any remaining group members.
        tracing::debug!("lsp[{name}]: process-tree kill failed: {error}");
    }
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait())
        .await
        .is_ok()
    {
        return;
    }

    tracing::warn!("lsp[{name}]: process-tree shutdown required a second kill");
    #[cfg(unix)]
    if let Some(pid) = process_group {
        crate::sandbox::kill_process_group(pid);
    }
    #[cfg(not(unix))]
    let _ = process_group;
    let _ = child.start_kill();
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        // Keep the supervisor's ownership until process-wrap eventually
        // completes the reap; callers can observe that shutdown is not done.
        tracing::warn!("lsp[{name}]: process-tree reap remains pending");
        let _ = child.wait().await;
    }
}

fn canonical_workspace_root(root: &Path) -> anyhow::Result<PathBuf> {
    let root = root.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "LSP workspace root '{}' is unavailable: {error}",
            root.display()
        )
    })?;
    if !root.is_dir() {
        anyhow::bail!("LSP workspace root '{}' is not a directory", root.display());
    }
    Ok(root)
}

fn lsp_command(cfg: &LspServerConfig, root: &Path) -> anyhow::Result<tokio::process::Command> {
    let env = delegated_environment(&cfg.inherit_env, &cfg.env)?;
    // Resolve the executable against the launcher's PATH before clearing the
    // child's environment. Delegating PATH controls only subprocesses that
    // the language server may launch itself.
    let program = which::which(cfg.command.as_str()).map_err(|error| {
        anyhow::anyhow!("LSP executable '{}' was not found: {error}", cfg.command)
    })?;
    let args = cfg.args.iter().map(ToString::to_string).collect::<Vec<_>>();
    let mut command = if let Some(backend) = cfg.sandbox.as_deref() {
        Sandbox::new(true, backend)
            .wrap_workspace_service(
                &program,
                &args,
                &root,
                &env,
                cfg.network == LspNetwork::Deny,
            )
            .map_err(anyhow::Error::msg)?
    } else if cfg.network == LspNetwork::Deny {
        anyhow::bail!("LSP network denial requires an available workspace-service sandbox");
    } else {
        let mut command = tokio::process::Command::new(program);
        command.args(args).current_dir(root).env_clear().envs(env);
        command
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn delegated_environment(
    inherit_env: &[String],
    explicit: &HashMap<String, String>,
) -> anyhow::Result<Vec<(OsString, OsString)>> {
    let mut delegated = HashMap::<String, (OsString, OsString)>::new();
    for name in inherit_env {
        if name.is_empty() || name.contains('=') {
            anyhow::bail!("invalid inherited LSP environment name");
        }
        if let Some(value) = std::env::var_os(name) {
            delegated.insert(environment_identity(name), (OsString::from(name), value));
        }
    }
    for (name, value) in explicit {
        if name.is_empty() || name.contains('=') {
            anyhow::bail!("invalid explicit LSP environment name");
        }
        delegated.insert(
            environment_identity(name),
            (OsString::from(name), OsString::from(value)),
        );
    }
    Ok(delegated.into_values().collect())
}

fn environment_identity(name: &str) -> String {
    #[cfg(windows)]
    {
        name.to_ascii_uppercase()
    }
    #[cfg(not(windows))]
    {
        name.to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticStoreOutcome {
    Stored,
    Ignored,
    LimitExceeded,
}

fn validate_diagnostic_envelope(workspace_uri: &str, params: &Value) -> DiagnosticStoreOutcome {
    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
        return DiagnosticStoreOutcome::LimitExceeded;
    };
    if uri.len() > MAX_DIAGNOSTIC_URI_BYTES {
        return DiagnosticStoreOutcome::LimitExceeded;
    }
    if !uri_is_within_workspace(uri, workspace_uri) {
        return DiagnosticStoreOutcome::Ignored;
    }
    let Some(diagnostics) = params.get("diagnostics").and_then(Value::as_array) else {
        return DiagnosticStoreOutcome::LimitExceeded;
    };
    if diagnostics.len() > MAX_DIAGNOSTICS_PER_FILE {
        return DiagnosticStoreOutcome::LimitExceeded;
    }
    DiagnosticStoreOutcome::Stored
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
    let Ok(identity) = crate::fs::checked_path_metadata(&canonical) else {
        return false;
    };
    if !identity.is_file() || identity.file_type().is_symlink() {
        return false;
    }
    let Some(uri) = file_uri(&canonical) else {
        return false;
    };
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

    let stored = commit_diagnostics(diags, uri.clone(), server, diagnostics, Some(identity));
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
    identity: Option<crate::fs::CheckedMetadata>,
) -> bool {
    let diagnostics = sanitize_diagnostics(diagnostics);
    let cached_bytes = retained_diagnostic_bytes(&uri, server, &diagnostics);
    let mut store = diags.lock().unwrap();
    if !store.contains_key(&uri)
        && (store.len() >= MAX_DIAGNOSTIC_FILES
            || store
                .values()
                .filter(|entry| entry.server == server)
                .count()
                >= MAX_DIAGNOSTIC_FILES_PER_SERVER)
    {
        return false;
    }
    let old_bytes = store.get(&uri).map(|entry| entry.cached_bytes).unwrap_or(0);
    let current_bytes: usize = store.values().map(|entry| entry.cached_bytes).sum();
    let replacement_bytes = current_bytes
        .saturating_sub(old_bytes)
        .saturating_add(cached_bytes);
    if replacement_bytes > MAX_DIAGNOSTIC_CACHE_BYTES {
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
        entry.version = entry.version.saturating_add(1);
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
    entry.version = entry.version.saturating_add(1);
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

fn uri_is_within_workspace(uri: &str, workspace_uri: &str) -> bool {
    if uri == workspace_uri {
        return true;
    }
    let Some(suffix) = uri.strip_prefix(workspace_uri) else {
        return false;
    };
    workspace_uri.ends_with('/') || suffix.starts_with('/')
}

#[cfg(test)]
pub(crate) fn store_diagnostics_for_test(
    diags: &DiagStore,
    server: &str,
    workspace_uri: &str,
    params: &Value,
) -> Option<bool> {
    match validate_diagnostic_envelope(workspace_uri, params) {
        DiagnosticStoreOutcome::Stored => {
            let uri = params.get("uri")?.as_str()?.to_string();
            let diagnostics = serde_json::from_value::<Vec<lsp_types::Diagnostic>>(
                params.get("diagnostics")?.clone(),
            )
            .ok()?;
            {
                let store = diags.lock().unwrap();
                if !store.contains_key(&uri)
                    && (store.len() >= MAX_DIAGNOSTIC_FILES
                        || store
                            .values()
                            .filter(|entry| entry.server == server)
                            .count()
                            >= MAX_DIAGNOSTIC_FILES_PER_SERVER)
                {
                    return None;
                }
            }
            Some(commit_diagnostics(diags, uri, server, diagnostics, None))
        }
        DiagnosticStoreOutcome::Ignored => Some(false),
        DiagnosticStoreOutcome::LimitExceeded => None,
    }
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
