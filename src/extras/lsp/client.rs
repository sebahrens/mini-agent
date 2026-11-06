//! One running language server process: spawn, `initialize` handshake,
//! full-document sync, and `publishDiagnostics` collection into the shared
//! [`DiagStore`]. Everything is fail-open — callers get `None`/no-op on any
//! error so a broken server never breaks an edit.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
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
    /// Wall-clock time just before identity capture and the read; the
    /// reference for deciding whether file timestamps are trustworthy.
    started: std::time::SystemTime,
}

/// A file whose last change is within this window of our read could be changed
/// again inside the same timestamp tick, so its timestamps prove nothing.
const RACY_WINDOW: Duration = Duration::from_millis(50);

#[cfg(test)]
thread_local! {
    static REHASH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether the retained handle still reports the exact size and change times
/// captured at read, with the last change old enough that a same-tick rewrite
/// is impossible. Only then may the second full-file hash be skipped.
fn stat_proves_unchanged(document: &Document) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let before: &std::fs::Metadata = &document.identity;
        let Ok(now) = document.identity.handle().metadata() else {
            return false;
        };
        if now.len() != before.len()
            || now.mtime() != before.mtime()
            || now.mtime_nsec() != before.mtime_nsec()
            || now.ctime() != before.ctime()
            || now.ctime_nsec() != before.ctime_nsec()
        {
            return false;
        }
        let last = (before.ctime(), before.ctime_nsec()).max((before.mtime(), before.mtime_nsec()));
        let (Ok(secs), Ok(nanos)) = (u64::try_from(last.0), u32::try_from(last.1)) else {
            return false;
        };
        let changed = std::time::UNIX_EPOCH + Duration::new(secs, nanos);
        document
            .started
            .duration_since(changed)
            .is_ok_and(|age| age > RACY_WINDOW)
    }
    #[cfg(not(unix))]
    {
        let _ = document;
        false
    }
}

/// Confirms the bytes read still match the file, hashing again only when
/// timestamps cannot prove it.
async fn document_content_current(document: &Document) -> bool {
    if stat_proves_unchanged(document) {
        return true;
    }
    #[cfg(test)]
    REHASH_COUNT.with(|count| count.set(count.get() + 1));
    content_matches(&document.identity, Some(document.content)).await
}

/// Observe the kernel pipe state without writing a probe frame. A fixture's
/// close marker does not exclude a reader inherited by another fork before exec.
#[cfg(all(test, unix))]
#[allow(unsafe_code)]
pub(crate) fn pipe_reader_closed_for_test(
    writer: &impl std::os::fd::AsRawFd,
) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: writer.as_raw_fd(),
        // Request write-side observation; Darwin's pipe poll reports its
        // final-reader hangup on this direction, without performing a write.
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: the caller retains the writer for this nonblocking syscall;
    // descriptor is one initialized, writable pollfd and no fd is transferred.
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(std::io::Error::other("fixture pipe descriptor is invalid"));
    }
    // Linux reports POLLERR; Darwin reports POLLHUP for this write-end state.
    Ok(descriptor.revents & (libc::POLLERR | libc::POLLHUP) != 0)
}

/// Capture identity from the same authorized handle that supplies the text.
pub(crate) async fn read_document(file: tokio::fs::File) -> std::io::Result<Document> {
    let started = std::time::SystemTime::now();
    let identity = crate::fs::checked_tokio_file_metadata(&file).await?;
    let text = read_document_text(file).await?;
    let content = crate::fs::ContentDigest::of(text.as_bytes());
    Ok(Document {
        text,
        identity,
        content,
        started,
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
/// Abuse ceiling for one publish. Larger lists are still data to truncate to
/// [`MAX_DIAGNOSTICS_PER_FILE`], but a frame claiming more than this many
/// entries is treated as a protocol violation and closes the transport.
const MAX_DIAGNOSTICS_PER_PUBLISH: usize = 10_000;
const MAX_DIAGNOSTIC_URI_BYTES: usize = 4 * 1024;
pub(crate) const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024;
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
    /// Diagnostics the server published beyond [`MAX_DIAGNOSTICS_PER_FILE`]
    /// (lowest severity dropped first). Surfaced as "N more not shown".
    pub omitted: usize,
    /// Identity and text digest from synchronization. Reads reject replaced
    /// files, symlinks and same-inode content changes.
    pub identity: Option<crate::fs::CheckedMetadata>,
    pub content: Option<crate::fs::ContentDigest>,
    /// Conservative retained-memory accounting used by the global cache cap.
    pub cached_bytes: usize,
    /// Store-clock stamp of the last publication; the eviction recency.
    pub last_touched: u64,
}

/// URIs whose non-empty diagnostics were evicted (or refused) because the
/// cache was full. Bounded: past [`MAX_TRACKED_DROPPED`] only a count is kept.
const MAX_TRACKED_DROPPED: usize = 1024;

#[derive(Default)]
struct DroppedFiles {
    uris: std::collections::HashSet<String>,
    overflow: usize,
}

/// The diagnostic cache: uri → latest diagnostics plus the bookkeeping that
/// makes eviction visible. `lock()` exposes the map exactly as before.
#[derive(Default)]
pub struct DiagStoreState {
    map: Mutex<HashMap<String, FileDiags>>,
    /// Monotonic recency clock for [`FileDiags::last_touched`].
    clock: AtomicU64,
    /// Highest version of any evicted entry. A re-created entry starts above
    /// it so a waiter holding a pre-eviction baseline still sees progress.
    version_floor: AtomicU64,
    dropped: Mutex<DroppedFiles>,
}

impl DiagStoreState {
    pub fn lock(
        &self,
    ) -> std::sync::LockResult<std::sync::MutexGuard<'_, HashMap<String, FileDiags>>> {
        self.map.lock()
    }

    /// Files whose diagnostics were lost to a full cache and not republished.
    pub fn dropped_files(&self) -> usize {
        let dropped = self.dropped.lock().unwrap();
        dropped.uris.len().saturating_add(dropped.overflow)
    }

    fn note_dropped(&self, uri: &str) {
        let mut dropped = self.dropped.lock().unwrap();
        if dropped.uris.contains(uri) {
            return;
        }
        if dropped.uris.len() < MAX_TRACKED_DROPPED {
            dropped.uris.insert(uri.to_string());
        } else {
            dropped.overflow = dropped.overflow.saturating_add(1);
        }
    }

    fn note_published(&self, uri: &str) {
        self.dropped.lock().unwrap().uris.remove(uri);
    }
}

/// uri → latest diagnostics. Shared between the manager and every client's
/// reader task.
pub type DiagStore = Arc<DiagStoreState>;

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
    /// URI the server knows this document by, for `didClose` on eviction.
    pub(crate) wire_uri: String,
    /// Recency stamp; the least recently synced document is evicted first.
    pub(crate) last_used: u64,
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

/// Most distinct server progress tokens tracked at once; further tokens are
/// ignored so a hostile server cannot grow the set.
const MAX_PROGRESS_TOKENS: usize = 64;
/// Delay before the single retry of a request the server cancelled.
const RETRY_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_millis(250)
};
/// LSP `ContentModified` and `ServerCancelled` error codes.
const CONTENT_MODIFIED: i64 = -32801;
const SERVER_CANCELLED: i64 = -32802;

/// Work-done progress the server has begun and not ended (indexing, loading
/// the workspace). While non-empty, an empty navigation answer is
/// indistinguishable from "not ready yet".
#[derive(Default)]
struct ProgressState {
    active: Mutex<std::collections::HashSet<String>>,
}

impl ProgressState {
    fn apply(&self, params: &Value) {
        let token = match params.get("token") {
            Some(Value::String(token)) => token.clone(),
            Some(Value::Number(token)) => token.to_string(),
            _ => return,
        };
        let mut active = self.active.lock().unwrap();
        match params
            .get("value")
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
        {
            Some("begin") if active.len() < MAX_PROGRESS_TOKENS => {
                active.insert(token);
            }
            Some("end") => {
                active.remove(&token);
            }
            _ => {}
        }
    }

    fn busy(&self) -> bool {
        !self.active.lock().unwrap().is_empty()
    }
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
    /// Reference point for `last_used_ms`.
    epoch: tokio::time::Instant,
    /// Milliseconds since `epoch` of the last spawn, request or document sync,
    /// read by the manager's idle watcher.
    last_used_ms: std::sync::atomic::AtomicU64,
    /// The manager's idle watcher, aborted when the client is shut down.
    idle_task: Mutex<Option<JoinHandle<()>>>,
    /// Whether the server registered for `textDocument/didSave` in its
    /// `initialize` result: [`SAVE_NONE`], [`SAVE_NOTIFY`] or
    /// [`SAVE_WITH_TEXT`]. Set once after the handshake.
    save_mode: AtomicU8,
    /// `capabilities` from the server's `initialize` result.
    capabilities: std::sync::OnceLock<Value>,
    /// Active `$/progress` work-done tokens.
    progress: Arc<ProgressState>,
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

    #[cfg(test)]
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
        let progress = Arc::new(ProgressState::default());
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
            let progress = progress.clone();
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
                    let Ok(mut msg) = serde_json::from_slice::<Value>(&frame) else {
                        tracing::debug!("lsp[{server_name}]: malformed JSON-RPC frame");
                        break;
                    };
                    let method = msg.get("method").and_then(Value::as_str);
                    let id = msg.get("id").and_then(Value::as_i64);
                    if let Some(reply) = server_request_reply(&msg) {
                        // Server→client request (numeric or string id).
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
                        continue;
                    }
                    match (method, id) {
                        // Requests were answered above.
                        (Some(_), Some(_)) => {}
                        // Server→client notification.
                        (Some(m), None)
                            if m == "textDocument/publishDiagnostics"
                                && msg.get("params").is_some() =>
                        {
                            let envelope = {
                                let params = &msg["params"];
                                match validate_diagnostic_envelope(&workspace_uri, params) {
                                    DiagnosticStoreOutcome::Ignored => {
                                        validate_diagnostic_envelope(&parent_workspace_uri, params)
                                    }
                                    outcome => outcome,
                                }
                            };
                            match envelope {
                                DiagnosticStoreOutcome::Stored => {
                                    let Some(uri) = msg["params"]
                                        .get("uri")
                                        .and_then(Value::as_str)
                                        .and_then(|uri| {
                                            parent_diagnostic_uri(&workspace, &server_root, uri)
                                        })
                                    else {
                                        continue;
                                    };
                                    // The frame is owned and not read again:
                                    // move the params out instead of cloning
                                    // a possibly large diagnostics array.
                                    let mut params = msg["params"].take();
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
                        (Some("$/progress"), None) => {
                            if let Some(params) = msg.get("params") {
                                progress.apply(params);
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
                progress.active.lock().unwrap().clear();
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
            epoch: tokio::time::Instant::now(),
            last_used_ms: std::sync::atomic::AtomicU64::new(0),
            idle_task: Mutex::new(None),
            save_mode: AtomicU8::new(SAVE_NONE),
            capabilities: std::sync::OnceLock::new(),
            progress,
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
                        "didSave": true
                    },
                    "publishDiagnostics": { "versionSupport": true },
                    "callHierarchy": { "dynamicRegistration": false },
                    "rename": { "dynamicRegistration": false, "prepareSupport": false },
                    "codeAction": {
                        "dynamicRegistration": false,
                        "isPreferredSupport": true,
                        "disabledSupport": true,
                        "codeActionLiteralSupport": {
                            "codeActionKind": { "valueSet": [
                                "quickfix", "refactor", "refactor.extract",
                                "refactor.inline", "refactor.rewrite", "source",
                                "source.organizeImports"
                            ] }
                        }
                    }
                },
                "window": { "workDoneProgress": true },
                "workspace": {
                    "workspaceEdit": {
                        "documentChanges": true,
                        "resourceOperations": ["create", "rename", "delete"]
                    }
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
        let Some(result) = initialized
            .as_ref()
            .and_then(|response| response.get("result"))
            .and_then(Value::as_object)
        else {
            client.shutdown().await;
            return None;
        };
        client.save_mode.store(
            save_mode_from_capabilities(result.get("capabilities")),
            Ordering::Release,
        );
        let _ = client.capabilities.set(
            result
                .get("capabilities")
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
        if !client.notify("initialized", json!({})).await {
            client.shutdown().await;
            return None;
        }
        tracing::info!("lsp[{name}]: initialized (root {})", root.display());
        Some(client)
    }

    /// Records use of the server so the idle watcher defers retirement.
    pub(crate) fn touch(&self) {
        let elapsed = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_used_ms
            .fetch_max(elapsed, std::sync::atomic::Ordering::Relaxed);
    }

    /// When the client becomes idle for `idle`, counted from its last use.
    pub(crate) fn idle_deadline(&self, idle: Duration) -> tokio::time::Instant {
        self.epoch
            + Duration::from_millis(self.last_used_ms.load(std::sync::atomic::Ordering::Relaxed))
            + idle
    }

    /// Installs the idle watcher; it is aborted by [`Self::shutdown`].
    pub(crate) fn set_idle_task(&self, task: JoinHandle<()>) {
        *self.idle_task.lock().unwrap() = Some(task);
    }

    /// Releases the idle watcher without aborting it, for the watcher itself
    /// before it shuts the client down.
    pub(crate) fn detach_idle_task(&self) {
        drop(self.idle_task.lock().unwrap().take());
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Option<Value> {
        self.touch();
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
        let response = tokio::time::timeout(remaining, rx).await;
        self.touch();
        match response {
            Ok(Ok(resp)) => Some(resp),
            _ => {
                tracing::debug!("lsp[{}]: '{}' timed out", self.name, method);
                None
            }
        }
    }

    /// Whether the server's `initialize` result advertises `provider`
    /// (e.g. `definitionProvider`). Absent, `null` and `false` mean no.
    pub(crate) fn supports(&self, provider: &str) -> bool {
        self.capabilities
            .get()
            .and_then(|capabilities| capabilities.get(provider))
            .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)))
    }

    /// Whether the server has work-done progress (indexing, workspace load)
    /// in flight.
    pub(crate) fn is_indexing(&self) -> bool {
        self.progress.busy()
    }

    /// Waits until no progress is active, at most `timeout`. Returns whether
    /// the server went idle.
    pub(crate) async fn wait_until_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.progress.busy() {
            if !self.is_usable() || tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        true
    }

    /// Sends a navigation request and returns its `result` (`Value::Null`
    /// for an empty answer). Errors on timeout, transport failure or an
    /// error response. A `ContentModified` (-32801) or `ServerCancelled`
    /// (-32802) reply is retried once, as the LSP specification directs.
    pub(crate) async fn query(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let started = tokio::time::Instant::now();
        let mut retried = false;
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            match self.query_once(method, params.clone(), remaining).await {
                Err((Some(CONTENT_MODIFIED | SERVER_CANCELLED), _))
                    if !retried && started.elapsed() + RETRY_DELAY < timeout =>
                {
                    retried = true;
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Err((code, message)) => {
                    return Err(match code {
                        Some(CONTENT_MODIFIED | SERVER_CANCELLED) => format!(
                            "{message} (the server reported the document changed or cancelled the request, again after one retry; try again shortly)"
                        ),
                        _ => message,
                    });
                }
                Ok(value) => return Ok(value),
            }
        }
    }

    async fn query_once(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, (Option<i64>, String)> {
        if !self.is_usable() {
            return Err((None, "language server is not running".to_string()));
        }
        let response = self.request(method, params, timeout).await.ok_or_else(|| {
            (
                None,
                format!("'{method}' timed out after {} ms", timeout.as_millis()),
            )
        })?;
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err((
                error.get("code").and_then(Value::as_i64),
                format!(
                    "'{method}' failed: {}",
                    truncate_utf8_bytes(message, MAX_DIAGNOSTIC_MESSAGE_BYTES)
                ),
            ));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The URI this server uses for a canonical workspace file.
    pub(crate) fn wire_uri(&self, parent_path: &Path) -> Option<String> {
        self.workspace.validate().ok()?;
        let relative = parent_path.strip_prefix(self.workspace.root()).ok()?;
        file_uri(&self.server_root.join(relative))
    }

    /// Maps a URI reported by the server to a canonical workspace path.
    /// Outside paths, traversal and symlink aliases yield `None`.
    pub(crate) fn parent_path(&self, wire_uri: &str) -> Option<PathBuf> {
        file_path(&parent_diagnostic_uri(
            &self.workspace,
            &self.server_root,
            wire_uri,
        )?)
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
        if let Some(task) = self.idle_task.lock().unwrap().take() {
            task.abort();
        }
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

    #[cfg(all(test, unix))]
    pub(crate) async fn stdin_reader_closed_for_test(&self) -> std::io::Result<bool> {
        let stdin = self.stdin.lock().await;
        pipe_reader_closed_for_test(&*stdin)
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
        self.touch();
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
                ..
            } = document;
            enum Sync {
                Open,
                Change(i64),
            }
            let (action, baseline, evicted) = {
                let mut open = self.open.lock().unwrap();
                // Match the diagnostic cache ceiling and bound retained source
                // handles. At capacity a new document evicts the least
                // recently synced one, which is closed on the server, so
                // syncing never silently stops.
                let evicted =
                    if !open.contains_key(&uri) && open.len() >= MAX_DIAGNOSTIC_FILES_PER_SERVER {
                        open.iter()
                            .min_by_key(|(_, document)| document.last_used)
                            .map(|(evicted, _)| evicted.clone())
                            .and_then(|evicted| open.remove_entry(&evicted))
                    } else {
                        None
                    };
                let last_used = open
                    .values()
                    .map(|document| document.last_used)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                let action = match open.get_mut(&uri) {
                    Some(document) => {
                        document.version += 1;
                        document.allow_versionless = false;
                        document.identity = identity;
                        document.content = content;
                        document.last_used = last_used;
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
                                wire_uri: wire_uri.clone(),
                                last_used,
                            },
                        );
                        Sync::Open
                    }
                };
                // Match the reader's open-then-diags lock order; a publication
                // from the previous epoch cannot satisfy this sync's wait.
                let mut diags = self.diags.lock().unwrap();
                // A closed document's diagnostics are no longer maintained by
                // the server; drop them so they cannot be reported as current.
                if let Some((evicted_uri, _)) = &evicted
                    && diags
                        .get(evicted_uri)
                        .is_some_and(|entry| entry.server == self.name)
                {
                    diags.remove(evicted_uri);
                }
                let baseline = diags.get(&uri).map_or(0, |diagnostics| diagnostics.version);
                (
                    action,
                    baseline,
                    evicted.map(|(_, document)| document.wire_uri),
                )
            }; // State locks are released before writing; stdin stays locked.
            #[cfg(test)]
            if let Some((advanced, release)) = probe {
                let _ = advanced.send(());
                let _ = release.await;
            }
            let save = match self.save_mode.load(Ordering::Acquire) {
                SAVE_NONE => None,
                mode => {
                    let mut params = json!({ "textDocument": { "uri": wire_uri } });
                    if mode == SAVE_WITH_TEXT {
                        params["text"] = Value::String(text.clone());
                    }
                    Some(
                        serde_json::to_vec(&json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/didSave",
                            "params": params,
                        }))
                        .map_err(std::io::Error::other)?,
                    )
                }
            };
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
            if let Some(evicted) = evicted {
                let close = serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didClose",
                    "params": { "textDocument": { "uri": evicted } },
                }))
                .map_err(std::io::Error::other)?;
                rpc::write_frame(write.writer(), &close).await?;
            }
            rpc::write_frame(write.writer(), &body).await?;
            // Servers such as rust-analyzer run their full checker (flycheck)
            // only on save. The file is already on disk, so announcing the
            // save after each synchronized edit is truthful.
            if let Some(save) = save {
                rpc::write_frame(write.writer(), &save).await?;
            }
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
        if !document_content_current(document).await {
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

/// The server did not register for `textDocument/didSave`.
const SAVE_NONE: u8 = 0;
/// Send `didSave` without the document text.
const SAVE_NOTIFY: u8 = 1;
/// Send `didSave` with the full document text (`includeText: true`).
const SAVE_WITH_TEXT: u8 = 2;

/// Decode `capabilities.textDocumentSync.save` from an `initialize` result.
/// The legacy numeric `TextDocumentSyncKind` form carries no save options,
/// so such servers never receive `didSave`.
fn save_mode_from_capabilities(capabilities: Option<&Value>) -> u8 {
    match capabilities
        .and_then(|capabilities| capabilities.get("textDocumentSync"))
        .and_then(|sync| sync.get("save"))
    {
        Some(Value::Bool(true)) => SAVE_NOTIFY,
        Some(Value::Object(options)) => {
            if options.get("includeText").and_then(Value::as_bool) == Some(true) {
                SAVE_WITH_TEXT
            } else {
                SAVE_NOTIFY
            }
        }
        _ => SAVE_NONE,
    }
}

/// Most `workspace/configuration` items answered in one reply.
const MAX_CONFIGURATION_ITEMS: usize = 1024;

/// The reply to a server→client request, or `None` when `msg` is not one.
///
/// The id is echoed exactly as sent (JSON-RPC allows numbers and strings).
/// `workspace/configuration` must answer with one entry per requested item;
/// every other request gets `null`, since this client declares no
/// capabilities that would legitimately trigger one.
fn server_request_reply(msg: &Value) -> Option<Value> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id")?;
    if !(id.is_number() || id.is_string()) {
        return None;
    }
    let result = if method == "workspace/configuration" {
        let items = msg
            .get("params")
            .and_then(|params| params.get("items"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
            .min(MAX_CONFIGURATION_ITEMS);
        Value::Array(vec![Value::Null; items])
    } else {
        Value::Null
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
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

/// The raw child under any wrapper layers: a wrapper's `inner` is a distinct
/// object, while the raw tokio child returns itself.
fn innermost_child(child: &mut dyn ChildWrapper) -> &mut dyn ChildWrapper {
    let is_raw = std::ptr::addr_eq(
        child.inner() as *const dyn ChildWrapper,
        child as *const dyn ChildWrapper,
    );
    if is_raw {
        child
    } else {
        innermost_child(child.inner_mut())
    }
}

async fn wait_direct_child(
    child: &mut dyn ChildWrapper,
) -> std::io::Result<std::process::ExitStatus> {
    innermost_child(child).wait().await
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
    // Await the direct child as well as protocol tasks. A crashed server can
    // leave a descendant holding inherited stdio open; waiting only for EOF
    // or the whole process group would then hang forever. The wrapper's own
    // `wait` also blocks reaping the group after the leader exits, so wait on
    // the raw child, which completes as soon as the direct child does.
    //
    // Tokio caches the status, so the wrapper's later `wait`/`try_wait` in
    // `terminate_and_reap` still observe it and reap the remaining group
    // members as before.
    tokio::select! {
        _ = shutdown_rx.recv() => {}
        waited = wait_direct_child(child.as_mut()) => {
            if let Err(error) = waited {
                tracing::debug!("lsp[{name}]: child status failed: {error}");
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
    lsp_command_with_search_path(cfg, root, std::env::var_os("PATH").as_deref())
}

fn lsp_command_with_search_path(
    cfg: &LspServerConfig,
    root: &Path,
    search_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<tokio::process::Command> {
    let env = delegated_environment(&cfg.inherit_env, &cfg.env)?;
    // Resolve the executable against the absolute entries of the launcher's
    // PATH before clearing the child's environment. Relative/empty entries
    // would resolve against the workspace, and LSP configuration has no
    // working-directory anchor for `./server`-style commands. Delegating PATH
    // controls only subprocesses that the language server may launch itself.
    let program = crate::extras::executable_search::resolve_service_executable(
        cfg.command.as_str(),
        search_path,
        None,
    )
    .map_err(|error| anyhow::anyhow!("LSP {error}"))?;
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

#[cfg(test)]
pub(crate) fn delegated_environment_for_test(
    inherit_env: &[String],
    explicit: &HashMap<String, String>,
) -> anyhow::Result<Vec<(OsString, OsString)>> {
    delegated_environment(inherit_env, explicit)
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
    if diagnostics.len() > MAX_DIAGNOSTICS_PER_PUBLISH {
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

/// Evicts one entry other than `keep` (restricted to `server` when given):
/// clean (empty) entries first, then the least recently published. Returns
/// the freed bytes, or `None` when nothing is evictable. An evicted entry that
/// still held diagnostics is recorded as dropped.
fn evict_one(
    state: &DiagStoreState,
    store: &mut HashMap<String, FileDiags>,
    server: Option<&str>,
    keep: &str,
    clean_only: bool,
) -> Option<usize> {
    let victim = store
        .iter()
        .filter(|(uri, entry)| {
            uri.as_str() != keep
                && server.is_none_or(|server| entry.server == server)
                && (!clean_only || (entry.diagnostics.is_empty() && entry.omitted == 0))
        })
        .min_by_key(|(_, entry)| {
            (
                !(entry.diagnostics.is_empty() && entry.omitted == 0),
                entry.last_touched,
            )
        })
        .map(|(uri, _)| uri.clone())?;
    let entry = store.remove(&victim)?;
    state
        .version_floor
        .fetch_max(entry.version, Ordering::AcqRel);
    if !entry.diagnostics.is_empty() || entry.omitted > 0 {
        state.note_dropped(&victim);
    }
    Some(entry.cached_bytes)
}

pub(crate) fn commit_diagnostics(
    diags: &DiagStore,
    uri: String,
    server: &str,
    diagnostics: Vec<lsp_types::Diagnostic>,
    identity: Option<crate::fs::CheckedMetadata>,
    content: Option<crate::fs::ContentDigest>,
) -> bool {
    let (diagnostics, omitted) = sanitize_diagnostics(diagnostics);
    let cached_bytes = retained_diagnostic_bytes(&uri, server, &diagnostics);
    // An incoming clean publication may only displace clean entries: it must
    // not push out a file that still has errors.
    let incoming_clean = diagnostics.is_empty() && omitted == 0;
    let mut store = diags.lock().unwrap();
    if !store.contains_key(&uri) {
        while store.len() >= MAX_DIAGNOSTIC_FILES {
            if evict_one(diags, &mut store, None, &uri, incoming_clean).is_none() {
                if !incoming_clean {
                    diags.note_dropped(&uri);
                }
                return false;
            }
        }
        while store
            .values()
            .filter(|entry| entry.server == server)
            .count()
            >= MAX_DIAGNOSTIC_FILES_PER_SERVER
        {
            if evict_one(diags, &mut store, Some(server), &uri, incoming_clean).is_none() {
                if !incoming_clean {
                    diags.note_dropped(&uri);
                }
                return false;
            }
        }
    }
    let old_bytes = store.get(&uri).map(|entry| entry.cached_bytes).unwrap_or(0);
    let mut current_bytes: usize = store.values().map(|entry| entry.cached_bytes).sum();
    while current_bytes
        .saturating_sub(old_bytes)
        .saturating_add(cached_bytes)
        > MAX_DIAGNOSTIC_CACHE_BYTES
    {
        match evict_one(diags, &mut store, None, &uri, incoming_clean) {
            Some(freed) => current_bytes = current_bytes.saturating_sub(freed),
            None => break,
        }
    }
    let replacement_bytes = current_bytes
        .saturating_sub(old_bytes)
        .saturating_add(cached_bytes);
    let touched = diags.clock.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    if replacement_bytes > MAX_DIAGNOSTIC_CACHE_BYTES {
        let Some(entry) = store.get_mut(&uri) else {
            if !incoming_clean {
                diags.note_dropped(&uri);
            }
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
        entry.omitted = 0;
        entry.identity = identity;
        entry.content = content;
        entry.cached_bytes = tombstone_bytes;
        entry.last_touched = touched;
        diags.note_dropped(&uri);
        return true;
    }
    let floor = diags.version_floor.load(Ordering::Acquire);
    let entry = store.entry(uri.clone()).or_insert_with(|| FileDiags {
        server: server.to_string(),
        version: floor,
        diagnostics: Vec::new(),
        omitted: 0,
        identity: None,
        content: None,
        cached_bytes: 0,
        last_touched: touched,
    });
    entry.server.clear();
    entry.server.push_str(server);
    entry.version = entry.version.saturating_add(1);
    entry.diagnostics = diagnostics;
    entry.omitted = omitted;
    entry.identity = identity;
    entry.content = content;
    entry.cached_bytes = cached_bytes;
    entry.last_touched = touched;
    drop(store);
    diags.note_published(&uri);
    true
}

/// Keep the [`MAX_DIAGNOSTICS_PER_FILE`] most severe diagnostics (errors
/// first, stable within a severity) and report how many were dropped.
fn sanitize_diagnostics(
    mut diagnostics: Vec<lsp_types::Diagnostic>,
) -> (Vec<lsp_types::Diagnostic>, usize) {
    let omitted = diagnostics.len().saturating_sub(MAX_DIAGNOSTICS_PER_FILE);
    if omitted > 0 {
        diagnostics.sort_by_key(|diagnostic| diagnostic.severity);
    }
    let kept = diagnostics
        .into_iter()
        .take(MAX_DIAGNOSTICS_PER_FILE)
        .map(|diagnostic| lsp_types::Diagnostic {
            range: diagnostic.range,
            severity: diagnostic.severity,
            message: truncate_utf8_bytes(&diagnostic.message, MAX_DIAGNOSTIC_MESSAGE_BYTES),
            ..Default::default()
        })
        .collect();
    (kept, omitted)
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
        "rake" | "gemspec" => "ruby",
        "zig" => "zig",
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

    #[cfg(unix)]
    #[test]
    fn lsp_command_ignores_relative_and_empty_path_entries() {
        use crate::extras::executable_search::PlantedExecutables;
        let fixture = PlantedExecutables::new("lsp");
        let cfg = LspServerConfig {
            command: compact_str::CompactString::new("rust-analyzer"),
            ..Default::default()
        };
        PlantedExecutables::plant(&fixture.workspace, "rust-analyzer");
        for search in fixture.hostile_search_paths() {
            let error = lsp_command_with_search_path(&cfg, &fixture.workspace, Some(&search))
                .unwrap_err()
                .to_string();
            assert!(error.contains("not found"), "{error}");
        }
        let trusted = PlantedExecutables::plant(&fixture.bin, "rust-analyzer");
        for search in fixture.hostile_search_paths() {
            let command =
                lsp_command_with_search_path(&cfg, &fixture.workspace, Some(&search)).unwrap();
            assert_eq!(Path::new(command.as_std().get_program()), trusted);
        }
        let relative = LspServerConfig {
            command: compact_str::CompactString::new("./rust-analyzer"),
            ..Default::default()
        };
        let error = lsp_command_with_search_path(&relative, &fixture.workspace, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("directory component"), "{error}");
    }

    #[test]
    fn did_save_is_sent_only_for_servers_that_register_for_it() {
        let mode = |capabilities: Value| save_mode_from_capabilities(Some(&capabilities));
        assert_eq!(save_mode_from_capabilities(None), SAVE_NONE);
        assert_eq!(mode(json!({})), SAVE_NONE);
        // Legacy numeric sync kind carries no save registration.
        assert_eq!(mode(json!({"textDocumentSync": 1})), SAVE_NONE);
        assert_eq!(mode(json!({"textDocumentSync": {"change": 1}})), SAVE_NONE);
        assert_eq!(
            mode(json!({"textDocumentSync": {"save": false}})),
            SAVE_NONE
        );
        assert_eq!(
            mode(json!({"textDocumentSync": {"save": true}})),
            SAVE_NOTIFY
        );
        assert_eq!(mode(json!({"textDocumentSync": {"save": {}}})), SAVE_NOTIFY);
        assert_eq!(
            mode(json!({"textDocumentSync": {"save": {"includeText": true}}})),
            SAVE_WITH_TEXT
        );
    }

    #[test]
    fn server_requests_are_answered_with_their_exact_id() {
        let string_id = server_request_reply(&json!({
            "jsonrpc": "2.0", "id": "req-7", "method": "window/workDoneProgress/create",
            "params": {"token": "t"}
        }))
        .expect("a string-id request must be answered");
        assert_eq!(string_id["id"], "req-7");
        assert_eq!(string_id["result"], Value::Null);

        let configuration = server_request_reply(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "workspace/configuration",
            "params": {"items": [{"section": "a"}, {"section": "b"}, {}]}
        }))
        .unwrap();
        assert_eq!(configuration["id"], 3);
        assert_eq!(configuration["result"], json!([null, null, null]));

        // Notifications and responses are not requests.
        assert!(server_request_reply(&json!({"method": "window/logMessage"})).is_none());
        assert!(server_request_reply(&json!({"id": 1, "result": {}})).is_none());
        assert!(server_request_reply(&json!({"id": null, "method": "x"})).is_none());
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn settled_unchanged_document_is_hashed_once_and_racy_or_rewritten_files_rehash() {
        let dir = std::env::temp_dir().join(format!("lsp-rehash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, b"fn a() {}\n").unwrap();
        let rehashes = || REHASH_COUNT.with(std::cell::Cell::get);

        // Fresh write: timestamps are inside the racy window, so rehash.
        let racy = read_document(crate::fs::open_stable_file(&path).await.unwrap())
            .await
            .unwrap();
        let start = rehashes();
        assert!(document_content_current(&racy).await);
        assert_eq!(rehashes(), start + 1);

        // Settled and unchanged: the read-time hash is the only hash.
        tokio::time::sleep(RACY_WINDOW * 2).await;
        let settled = read_document(crate::fs::open_stable_file(&path).await.unwrap())
            .await
            .unwrap();
        let start = rehashes();
        assert!(document_content_current(&settled).await);
        assert_eq!(rehashes(), start, "no second hash for a settled file");

        // Same-length in-place rewrite changes ctime: detected by rehash.
        std::fs::write(&path, b"fn b() {}\n").unwrap();
        assert!(!document_content_current(&settled).await);
        assert_eq!(rehashes(), start + 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
