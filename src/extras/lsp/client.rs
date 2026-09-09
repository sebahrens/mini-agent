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

pub(crate) struct Document {
    text: String,
    identity: crate::fs::CheckedMetadata,
    content: crate::fs::ContentDigest,
}

pub(crate) async fn read_stable_document(path: &Path) -> std::io::Result<Document> {
    read_document(crate::fs::open_stable_file(path).await?).await
}

/// Capture identity from the same authorized handle that supplies the text.
pub(crate) async fn read_document(file: tokio::fs::File) -> std::io::Result<Document> {
    let identity = crate::fs::checked_tokio_file_metadata(&file).await?;
    let text = read_document_text(file).await?;
    let content = crate::fs::ContentDigest::of(text.as_bytes());
    Ok(Document {
        text,
        identity,
        content,
    })
}

/// Hash only the retained readable object, off the async executor and without
/// holding the synchronized-document or diagnostic-cache mutex.
pub(crate) async fn content_matches(
    identity: &crate::fs::CheckedMetadata,
    content: Option<crate::fs::ContentDigest>,
) -> bool {
    let Some(content) = content else {
        return true;
    }; // Raw test fixtures only.
    let identity = identity.clone();
    crate::agent::runner::spawn_blocking_scoped(move || {
        content.matches_file(identity.handle()).unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// Bound reads on the authorized handle itself, including a file that grows
/// after metadata inspection. Oversized documents never reach synchronization.
pub(crate) async fn read_document_text(
    reader: impl tokio::io::AsyncRead + Unpin,
) -> std::io::Result<String> {
    let mut text = String::new();
    reader
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_string(&mut text)
        .await?;
    if text.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LSP document exceeds synchronization byte limit",
        ));
    }
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
    path.to_str()?;
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
/// Deadline covering writer-lock acquisition plus the frame write and flush.
///
/// A server that initializes successfully and then stops reading fills its
/// stdin pipe. Without this bound, synchronizing an ordinary document blocks
/// the calling file tool — and its shared or exclusive tool lane — forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
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
#[derive(Clone)]
pub struct FileDiags {
    pub server: String,
    /// Bumped on every `publishDiagnostics` for this file — lets callers
    /// wait for the publish that follows their `didChange`.
    pub version: u64,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
    /// Identity and text digest from synchronization. Reads reject replaced
    /// files, symlinks and same-inode content changes.
    pub identity: Option<crate::fs::CheckedMetadata>,
    pub content: Option<crate::fs::ContentDigest>,
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

#[derive(Clone)]
pub(crate) struct SyncedDocument {
    pub(crate) version: i64,
    /// Versionless publishes are unambiguous only before the first change in
    /// an epoch, or after an exact versioned publish anchors that epoch.
    pub(crate) allow_versionless: bool,
    pub(crate) identity: crate::fs::CheckedMetadata,
    pub(crate) content: crate::fs::ContentDigest,
}

impl SyncedDocument {
    /// Whether this reply anchors the epoch; None rejects its version.
    fn publication_anchor(&self, version: Option<&Value>) -> Option<bool> {
        match version {
            Some(version) if !version.is_null() => {
                (version.as_i64()? == self.version).then_some(true)
            }
            _ => self.allow_versionless.then_some(false),
        }
    }
}

#[cfg(test)]
struct SyncProbe {
    queued: oneshot::Sender<()>,
    advanced: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
type ShutdownProbe = Arc<Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>>;

#[cfg(test)]
struct DiagnosticProbe {
    version: i64,
    entered: oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

struct TransportState {
    closing: AtomicBool,
    shutdown_tx: mpsc::UnboundedSender<()>,
    #[cfg(test)]
    timeout_ms: std::sync::atomic::AtomicU64,
}

impl TransportState {
    fn new(shutdown_tx: mpsc::UnboundedSender<()>) -> Self {
        Self {
            closing: AtomicBool::new(false),
            shutdown_tx,
            #[cfg(test)]
            timeout_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn close(&self) {
        if !self.closing.swap(true, Ordering::AcqRel) {
            let _ = self.shutdown_tx.send(());
        }
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    fn write_timeout(&self) -> Duration {
        #[cfg(test)]
        {
            let millis = self.timeout_ms.load(Ordering::Relaxed);
            if millis > 0 {
                return Duration::from_millis(millis);
            }
        }
        WRITE_TIMEOUT
    }
}

pub struct LspClient {
    name: String,
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    transport: Arc<TransportState>,
    stopped: Arc<AtomicBool>,
    stopped_notify: Arc<Notify>,
    diags: DiagStore,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    server_root: PathBuf,
    /// Parent-canonical uri → last synced document version.
    open: Arc<Mutex<HashMap<String, SyncedDocument>>>,
    #[cfg(test)]
    sync_probes: Mutex<std::collections::VecDeque<SyncProbe>>,
    #[cfg(test)]
    shutdown_probe: ShutdownProbe,
    #[cfg(test)]
    diagnostic_probe: Arc<Mutex<Option<DiagnosticProbe>>>,
}

impl LspClient {
    /// Spawns the server and runs the `initialize` handshake. Returns `None`
    /// (with a log) on any failure: missing binary, spawn error, init timeout.
    pub async fn spawn(
        name: &str,
        cfg: &LspServerConfig,
        root: &Path,
        workspace: Option<Arc<crate::paths::WorkspaceBinding>>,
        diags: DiagStore,
        diag_notify: Arc<Notify>,
    ) -> Option<Arc<Self>> {
        Self::spawn_with_timeout_and_workspace(
            name,
            cfg,
            root,
            workspace,
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
        workspace: Option<Arc<crate::paths::WorkspaceBinding>>,
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
        let use_descriptor_root = workspace.is_some();
        let workspace = match workspace {
            Some(workspace) => workspace,
            None => Arc::new(crate::paths::WorkspaceBinding::capture(&root).ok()?),
        };
        workspace.validate().ok()?;
        let workspace_handle = if use_descriptor_root {
            Some(workspace.try_clone_directory_file().ok()?)
        } else {
            None
        };
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
        let transport = Arc::new(TransportState::new(shutdown_tx));
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_notify = Arc::new(Notify::new());
        let open = Arc::new(Mutex::new(HashMap::new()));
        // The reader is sequential, but its blocking publication can survive
        // reader cancellation. The supervisor retains this barrier until all
        // cache access and captured file handles have been released.
        let diagnostic_work = Arc::new(tokio::sync::Mutex::new(()));
        #[cfg(test)]
        let shutdown_probe = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let diagnostic_probe: Arc<Mutex<Option<DiagnosticProbe>>> = Arc::new(Mutex::new(None));

        // Reader task: routes responses to pending requests, stores
        // diagnostics, and answers server→client requests with null so a
        // server never hangs waiting on us.
        let reader_task = {
            let pending = pending.clone();
            let stdin = stdin.clone();
            let open = open.clone();
            let diags = diags.clone();
            let server_name = name.to_string();
            let workspace_uri = root_uri.clone();
            let parent_workspace_uri = file_uri(workspace.root())?;
            let workspace = workspace.clone();
            let server_root = server_root.clone();
            let transport = transport.clone();
            let diagnostic_work = diagnostic_work.clone();
            #[cfg(test)]
            let diagnostic_probe = diagnostic_probe.clone();
            tokio::spawn(async move {
                let mut stdout = rpc::FrameReader::new(stdout);
                loop {
                    let frame = match stdout.read_frame().await {
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
                            if !write_owned_frame_with_deadline(
                                &stdin,
                                &transport,
                                &body,
                                transport.write_timeout(),
                            )
                            .await
                            {
                                break;
                            }
                        }
                        // Server→client notification.
                        (Some(m), None) => {
                            if m == "textDocument/publishDiagnostics"
                                && let Some(params) = msg.get("params")
                            {
                                let envelope =
                                    match validate_diagnostic_envelope(&workspace_uri, params) {
                                        DiagnosticStoreOutcome::Ignored => {
                                            validate_diagnostic_envelope(
                                                &parent_workspace_uri,
                                                params,
                                            )
                                        }
                                        outcome => outcome,
                                    };
                                match envelope {
                                    DiagnosticStoreOutcome::Stored => {
                                        let Some(uri) = params
                                            .get("uri")
                                            .and_then(Value::as_str)
                                            .and_then(|uri| {
                                                parent_diagnostic_uri(&workspace, &server_root, uri)
                                            })
                                        else {
                                            continue;
                                        };
                                        let mut params = params.clone();
                                        params["uri"] = Value::String(uri);
                                        let diags = diags.clone();
                                        let open = open.clone();
                                        let server = server_name.clone();
                                        let work = diagnostic_work.clone().lock_owned().await;
                                        #[cfg(test)]
                                        let probe = {
                                            let mut probe = diagnostic_probe.lock().unwrap();
                                            if probe.as_ref().is_some_and(|probe| {
                                                params.get("version").and_then(Value::as_i64)
                                                    == Some(probe.version)
                                            }) {
                                                probe.take()
                                            } else {
                                                None
                                            }
                                        };
                                        let publish = move || {
                                            #[cfg(test)]
                                            if let Some(probe) = probe {
                                                let _ = probe.entered.send(());
                                                let _ = probe.release.recv();
                                            }
                                            store_diagnostics(&diags, &server, &params, Some(&open))
                                        };
                                        if tokio::task::spawn_blocking(move || {
                                            let _work = work;
                                            publish()
                                        })
                                        .await
                                        .unwrap_or(false)
                                        {
                                            diag_notify.notify_waiters();
                                        }
                                    }
                                    DiagnosticStoreOutcome::Ignored => {}
                                    DiagnosticStoreOutcome::LimitExceeded => {
                                        tracing::debug!(
                                            "lsp[{server_name}]: diagnostic storage limit exceeded"
                                        );
                                        transport.close();
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
                // Mark the transport unavailable before waking failed requests.
                transport.close();
                pending.lock().unwrap().clear();
            })
        };

        // Drain stderr in fixed chunks. Content is never retained or logged
        // because diagnostics may carry source or secrets. A cumulative cap
        // prevents a configured child from consuming unbounded pipe traffic.
        let stderr_task = {
            let server_name = name.to_string();
            let transport = transport.clone();
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
                                transport.close();
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
            SupervisedProtocol {
                reader_task,
                stderr_task,
                diagnostic_work,
                transport: transport.clone(),
                stopped: stopped.clone(),
                stopped_notify: stopped_notify.clone(),
                #[cfg(test)]
                shutdown_probe: shutdown_probe.clone(),
            },
        ));

        let client = Arc::new(Self {
            name: name.to_string(),
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            transport: transport.clone(),
            stopped,
            stopped_notify,
            diags,
            workspace,
            server_root,
            open,
            #[cfg(test)]
            sync_probes: Mutex::new(std::collections::VecDeque::new()),
            #[cfg(test)]
            shutdown_probe,
            #[cfg(test)]
            diagnostic_probe,
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
        // The deadline covers the write and the response wait together, so a
        // server that stops reading cannot consume the whole budget before the
        // response timer even starts.
        let started = tokio::time::Instant::now();
        if !write_owned_frame_with_deadline(
            &self.stdin,
            &self.transport,
            &body,
            timeout.min(self.transport.write_timeout()),
        )
        .await
        {
            return None;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, rx).await {
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
            return write_owned_frame_with_deadline(
                &self.stdin,
                &self.transport,
                &body,
                self.transport.write_timeout(),
            )
            .await;
        }
        false
    }

    pub(crate) fn is_usable(&self) -> bool {
        !self.transport.is_closing()
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(crate) async fn shutdown(&self) {
        self.transport.close();
        if self.is_stopped() {
            return;
        }
        let notified = self.stopped_notify.notified();
        if self.is_stopped() {
            return;
        }
        notified.await;
    }

    /// Synchronize a file through the same authorized read path as the manager.
    pub async fn sync_file(&self, path: &Path) {
        let Ok(document) = read_stable_document(path).await else {
            return;
        };
        let _ = self.sync_document(path, document).await;
    }

    #[cfg(test)]
    pub(crate) fn pause_shutdown_for_test(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *self.shutdown_probe.lock().unwrap() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }

    #[cfg(test)]
    pub(crate) fn pause_diagnostic_for_test(
        &self,
        version: i64,
    ) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, received) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        *self.diagnostic_probe.lock().unwrap() = Some(DiagnosticProbe {
            version,
            entered,
            release: gate,
        });
        (received, release)
    }

    #[cfg(test)]
    pub(crate) fn set_write_timeout_for_test(&self, value: Option<Duration>) {
        self.transport.timeout_ms.store(
            value.map(|value| value.as_millis() as u64).unwrap_or(0),
            Ordering::Relaxed,
        );
    }

    #[cfg(test)]
    pub(crate) fn pause_next_sync_for_test(
        &self,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
    ) {
        let (queued_tx, queued_rx) = oneshot::channel();
        let (advanced_tx, advanced_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        self.sync_probes.lock().unwrap().push_back(SyncProbe {
            queued: queued_tx,
            advanced: advanced_tx,
            release: release_rx,
        });
        (queued_rx, advanced_rx, release_tx)
    }

    /// Return the diagnostic publish counter captured for this sync, before
    /// the server can answer it. Callers must carry it into their wait.
    pub async fn sync_document(&self, path: &Path, document: Document) -> Option<u64> {
        #[cfg(test)]
        let probe = self.sync_probes.lock().unwrap().pop_front();
        #[cfg(test)]
        let probe = probe.map(|probe| {
            let _ = probe.queued.send(());
            (probe.advanced, probe.release)
        });
        let mut write = TransportWriteGuard::new(&self.transport);
        // One deadline includes the queue, validation and frame publication.
        // The stdin lock keeps document versions in the order sent on the wire.
        let outcome = tokio::time::timeout(self.transport.write_timeout(), async {
            write.stdin = Some(self.stdin.lock().await);
            if self.transport.is_closing() {
                return Ok(None);
            }
            let Some((uri, wire_uri)) = self.document_uris(path, &document).await else {
                return Ok(None);
            };
            if self.transport.is_closing() {
                return Ok(None);
            }
            let Document {
                text,
                identity,
                content,
            } = document;
            enum Sync {
                Open,
                Change(i64),
            }
            let (action, baseline) = {
                let mut open = self.open.lock().unwrap();
                // Match the diagnostic cache ceiling and bound retained source
                // handles. Existing documents can still advance at capacity.
                if !open.contains_key(&uri) && open.len() >= MAX_DIAGNOSTIC_FILES_PER_SERVER {
                    return Ok(None);
                }
                let action = match open.get_mut(&uri) {
                    Some(document) => {
                        document.version += 1;
                        document.allow_versionless = false;
                        document.identity = identity;
                        document.content = content;
                        Sync::Change(document.version)
                    }
                    None => {
                        open.insert(
                            uri.clone(),
                            SyncedDocument {
                                version: 1,
                                allow_versionless: true,
                                identity,
                                content,
                            },
                        );
                        Sync::Open
                    }
                };
                // Match the reader's open-then-diags lock order; a publication
                // from the previous epoch cannot satisfy this sync's wait.
                let baseline = self
                    .diags
                    .lock()
                    .unwrap()
                    .get(&uri)
                    .map_or(0, |diagnostics| diagnostics.version);
                (action, baseline)
            }; // State locks are released before writing; stdin stays locked.
            #[cfg(test)]
            if let Some((advanced, release)) = probe {
                let _ = advanced.send(());
                let _ = release.await;
            }
            let (method, params) = match action {
                Sync::Open => (
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": wire_uri, "languageId": language_id(path),
                            "version": 1, "text": text
                        }
                    }),
                ),
                Sync::Change(version) => (
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": wire_uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    }),
                ),
            };
            let body = serde_json::to_vec(&json!({
                "jsonrpc": "2.0", "method": method, "params": params,
            }))
            .map_err(std::io::Error::other)?;
            if self.transport.is_closing() {
                return Ok(None);
            }
            rpc::write_frame(write.writer(), &body).await?;
            Ok::<_, std::io::Error>(Some(baseline))
        })
        .await;
        let completed = matches!(outcome, Ok(Ok(_)));
        write.complete = completed;
        // Drop latches failure before the owned writer lock is released.
        drop(write);
        if !completed {
            // Cancellation also arms this cleanup through the write guard.
            // Never retain a client whose advanced version lacks a full frame.
            self.shutdown().await;
        }
        outcome.ok().and_then(Result::ok).flatten()
    }

    /// Validate after acquiring the writer: queued callers may have read bytes
    /// that changed while another notification was being published.
    async fn document_uris(&self, path: &Path, document: &Document) -> Option<(String, String)> {
        self.workspace.validate().ok()?;
        let parent_path = std::fs::canonicalize(path).ok()?;
        let relative = parent_path.strip_prefix(self.workspace.root()).ok()?;
        let wire_uri = file_uri(&self.server_root.join(relative))?;
        let uri = file_uri(&parent_path)?;
        if !content_matches(&document.identity, Some(document.content)).await {
            return None;
        }
        self.workspace.validate().ok()?;
        let current = crate::fs::checked_path_metadata(&parent_path).ok()?;
        crate::fs::ensure_same_file(&parent_path, &document.identity, &current).ok()?;
        Some((uri, wire_uri))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.transport.close();
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

struct TransportWriteGuard<'a> {
    transport: &'a TransportState,
    stdin: Option<tokio::sync::MutexGuard<'a, tokio::process::ChildStdin>>,
    complete: bool,
}

impl<'a> TransportWriteGuard<'a> {
    fn new(transport: &'a TransportState) -> Self {
        Self {
            transport,
            stdin: None,
            complete: false,
        }
    }

    fn writer(&mut self) -> &mut tokio::process::ChildStdin {
        self.stdin
            .as_deref_mut()
            .expect("transport writer acquired")
    }
}

impl Drop for TransportWriteGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.transport.close();
        }
        // Field destruction releases stdin only after the latch is visible.
    }
}

/// One deadline covers waiting for stdin and writing/flushing a frame. The
/// guard owns stdin, so failure or cancellation closes the transport before
/// any queued writer can acquire it, even while process cleanup is pending.
async fn write_owned_frame_with_deadline(
    stdin: &Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    transport: &TransportState,
    body: &[u8],
    deadline: Duration,
) -> bool {
    let mut write = TransportWriteGuard::new(transport);
    let written = tokio::time::timeout(deadline, async {
        write.stdin = Some(stdin.lock().await);
        if transport.is_closing() {
            return Ok(false);
        }
        rpc::write_frame(write.writer(), body).await?;
        Ok::<_, std::io::Error>(true)
    })
    .await;
    write.complete = matches!(written, Ok(Ok(_)));
    matches!(written, Ok(Ok(true)))
}

/// Protocol tasks drained, and stop signals raised, once the server exits.
struct SupervisedProtocol {
    reader_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    diagnostic_work: Arc<tokio::sync::Mutex<()>>,
    transport: Arc<TransportState>,
    stopped: Arc<AtomicBool>,
    stopped_notify: Arc<Notify>,
    #[cfg(test)]
    shutdown_probe: ShutdownProbe,
}

async fn supervise_child(
    name: String,
    mut child: Box<dyn ChildWrapper>,
    process_group: Option<u32>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    protocol: SupervisedProtocol,
) {
    let SupervisedProtocol {
        mut reader_task,
        mut stderr_task,
        diagnostic_work,
        transport,
        stopped,
        stopped_notify,
        #[cfg(test)]
        shutdown_probe,
    } = protocol;
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
    #[cfg(test)]
    let probe = shutdown_probe.lock().unwrap().take();
    #[cfg(test)]
    if let Some((entered, release)) = probe {
        let _ = entered.send(());
        let _ = release.await;
    }
    transport.close();
    terminate_and_reap(&name, &mut child, process_group).await;
    for task in [&mut reader_task, &mut stderr_task] {
        if tokio::time::timeout(TASK_DRAIN_TIMEOUT, &mut *task)
            .await
            .is_err()
        {
            task.abort();
            // An abort request alone does not establish that the reader has
            // stopped submitting blocking work or released its handles.
            let _ = task.await;
        }
    }
    // Native disk work cannot be aborted with its async reader. Keep shutdown
    // and manager replacement pending until the final publication is done.
    let _diagnostic_work = diagnostic_work.lock().await;
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
            .wrap_workspace_service(&program, &args, root, &env, cfg.network == LspNetwork::Deny)
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

/// Translate the server's descriptor namespace without ever resolving that
/// descriptor in the parent. Canonical server replies are also accepted, but
/// every spelling must name a file inside the retained workspace authority.
fn parent_diagnostic_uri(
    workspace: &crate::paths::WorkspaceBinding,
    server_root: &Path,
    raw_uri: &str,
) -> Option<String> {
    workspace.validate().ok()?;
    let path = file_path(raw_uri)?;
    if file_uri(&path).as_deref() != Some(raw_uri) {
        return None;
    }
    // File URIs use ordinary drive/UNC paths on Windows, while captured
    // filesystem roots may use verbatim prefixes. Compare in URI path form.
    let wire_root = file_path(&file_uri(server_root)?)?;
    let parent_root = file_path(&file_uri(workspace.root())?)?;
    let relative = path
        .strip_prefix(&wire_root)
        .or_else(|_| path.strip_prefix(&parent_root))
        .ok()?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let parent = workspace.root().join(relative);
    // Reject symlink aliases, traversal and non-canonical spellings before
    // attaching a server message to the parent's diagnostic cache identity.
    if std::fs::canonicalize(&parent).ok()? != parent {
        return None;
    }
    workspace.validate().ok()?;
    file_uri(&parent)
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
    let Ok(mut identity) = crate::fs::checked_path_metadata(&canonical) else {
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

    // Release the version lock during bounded disk I/O. Recheck the epoch
    // under the lock before commit so a concurrent sync cannot be overwritten.
    let checked_sync = if let Some(versions) = synced_versions {
        let Some(synced) = versions.lock().unwrap().get(&uri).cloned() else {
            return false;
        };
        if synced.publication_anchor(params.get("version")).is_none()
            || crate::fs::ensure_same_file(&canonical, &synced.identity, &identity).is_err()
            || !synced
                .content
                .matches_file(synced.identity.handle())
                .unwrap_or(false)
        {
            return false;
        }
        Some(synced)
    } else {
        None
    };
    let content = checked_sync.as_ref().map(|synced| synced.content);
    let mut synced_guard = synced_versions.map(|versions| versions.lock().unwrap());
    let exact_version_anchor = if let Some(synced_versions) = synced_guard.as_mut() {
        let Some(synced) = synced_versions.get_mut(&uri) else {
            return false;
        };
        if checked_sync
            .as_ref()
            .is_none_or(|checked| checked.version != synced.version)
        {
            return false;
        }
        // Retain the synchronized identity, sharing its handle with the cache.
        // A later pathname replacement cannot relabel the server's old text.
        identity = synced.identity.clone();
        let Some(anchor) = synced.publication_anchor(params.get("version")) else {
            return false;
        };
        anchor
    } else {
        false
    };

    let stored = commit_diagnostics(
        diags,
        uri.clone(),
        server,
        diagnostics,
        Some(identity),
        content,
    );
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
    content: Option<crate::fs::ContentDigest>,
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
        entry.content = content;
        entry.cached_bytes = tombstone_bytes;
        return true;
    }
    let entry = store.entry(uri).or_insert_with(|| FileDiags {
        server: server.to_string(),
        version: 0,
        diagnostics: Vec::new(),
        identity: None,
        content: None,
        cached_bytes: 0,
    });
    entry.server.clear();
    entry.server.push_str(server);
    entry.version = entry.version.saturating_add(1);
    entry.diagnostics = diagnostics;
    entry.identity = identity;
    entry.content = content;
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
            Some(commit_diagnostics(
                diags,
                uri,
                server,
                diagnostics,
                None,
                None,
            ))
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

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[tokio::test]
    async fn document_reads_stop_at_the_sync_limit_before_allocating_the_full_input() {
        for size in [
            MAX_DOCUMENT_BYTES - 1,
            MAX_DOCUMENT_BYTES,
            MAX_DOCUMENT_BYTES + 1,
            MAX_DOCUMENT_BYTES * 2,
        ] {
            let mut reader = std::io::Cursor::new(vec![b'x'; size as usize]);
            let result = read_document_text(&mut reader).await;
            assert_eq!(reader.position(), size.min(MAX_DOCUMENT_BYTES + 1));
            if size <= MAX_DOCUMENT_BYTES {
                assert_eq!(result.unwrap().len() as u64, size);
            } else {
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
            }
        }
        let unicode = "é".repeat(MAX_DOCUMENT_BYTES as usize / 2);
        assert_eq!(
            read_document_text(unicode.as_bytes()).await.unwrap(),
            unicode
        );
    }

    #[tokio::test]
    async fn diagnostic_uri_mapping_rejects_escapes_aliases_and_replaced_workspaces() {
        let temp = std::env::temp_dir().join(format!("lsp-uri-mapping-{}", uuid::Uuid::new_v4()));
        let root = temp.join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let source = root.join("document.rs");
        std::fs::write(&source, "document").unwrap();
        let outside = root.parent().unwrap().join("outside.rs");
        std::fs::write(&outside, "outside").unwrap();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&root).unwrap());
        let manager = super::super::LspManager::new(
            &crate::config::types::LspConfig::default(),
            workspace.clone(),
        );
        let server_root = workspace_service_root(&root);
        let wire_uri = file_uri(&server_root.join("document.rs")).unwrap();
        let parent_uri = file_uri(&source).unwrap();
        for uri in [&wire_uri, &parent_uri] {
            assert_eq!(
                parent_diagnostic_uri(&workspace, &server_root, uri),
                Some(parent_uri.clone())
            );
        }
        let wire_root_uri = file_uri(&server_root).unwrap();
        for uri in [
            file_uri(&outside).unwrap(),
            format!("{wire_root_uri}/%2e%2e/outside.rs"),
            format!("{wire_root_uri}/child%2F..%2F..%2Foutside.rs"),
            format!("{wire_root_uri}/%64ocument.rs"),
            format!("{wire_uri}?query=1"),
            format!("{wire_uri}#fragment"),
            "file:///invalid%escape".into(),
            "https://example.test/document.rs".into(),
        ] {
            assert_eq!(
                parent_diagnostic_uri(&workspace, &server_root, &uri),
                None,
                "{uri}"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, root.join("alias.rs")).unwrap();
            let alias = file_uri(&server_root.join("alias.rs")).unwrap();
            assert_eq!(
                parent_diagnostic_uri(&workspace, &server_root, &alias),
                None
            );
            let moved = root.with_file_name("moved");
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("document.rs"), "replacement").unwrap();
            assert_eq!(
                parent_diagnostic_uri(&workspace, &server_root, &wire_uri),
                None
            );
            assert_eq!(
                parent_diagnostic_uri(&workspace, &server_root, &parent_uri),
                None
            );
            // Model a publication racing the root replacement after translation.
            // Result formatting must revalidate authority as well as the inode.
            manager.inject_diagnostics(
                &parent_uri,
                "rust",
                vec![lsp_types::Diagnostic {
                    severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                    message: "replacement workspace diagnostic".into(),
                    ..Default::default()
                }],
            );
            assert!(
                manager
                    .diagnostics_block_since(&source, Duration::ZERO, Some(0))
                    .await
                    .is_none()
            );
        }
        drop(manager);
        drop(workspace);
        std::fs::remove_dir_all(temp).unwrap();
    }
}
