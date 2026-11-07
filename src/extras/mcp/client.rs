use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use compact_str::CompactString;
use process_wrap::tokio::ChildWrapper;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ListToolsRequest,
    PaginatedRequestParams, ServerResult, Tool,
};
use rmcp::service::{
    Peer, PeerRequestOptions, RoleClient, RunningService, RxJsonRpcMessage, ServiceError,
    TxJsonRpcMessage, serve_client,
};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

use super::bounded_http::BoundedHttpClient;
use super::config::{McpServerConfig, McpStdioNetwork, OAuthConfig, TrustedMcpServer};
use super::egress::ValidatedEndpoint;
use crate::process_creation::CommandWrapCreationExt;
use crate::sandbox::{Sandbox, owned_workspace_service_tree};

/// Bound on the MCP initialization handshake for every transport. For HTTP
/// servers this also covers OAuth client construction (stored-token restore
/// and refresh) and the TCP/TLS connect.
pub(crate) const MCP_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on a complete `tools/list` enumeration for one server, including
/// every paginated page.
pub(crate) const MCP_LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on `tools/list` pages so a server that keeps returning a cursor cannot
/// spin the enumeration loop forever within the time budget.
pub(crate) const MCP_LIST_TOOLS_MAX_PAGES: usize = 64;
/// Maximum number of model-visible tools accepted from one server whose
/// schemas are loaded eagerly (`defer = false`).
pub(crate) const MCP_LIST_TOOLS_MAX_TOOLS: usize = 128;
/// Ceiling for a server whose schemas may be deferred (`defer = true`/`auto`):
/// tool search loads a schema only on demand, so a large catalogue stays
/// usable. Memory stays bounded (tools x 16 KiB schema).
pub(crate) const MCP_LIST_TOOLS_MAX_DEFERRED_TOOLS: usize = 1024;
/// Names of cap-omitted tools quoted in the notice.
pub(crate) const MCP_OMITTED_TOOL_NAMES: usize = 16;

/// Retries of a first HTTP connection after a transient failure (5xx,
/// connection refused or reset, timeout), so up to four attempts in all.
pub(crate) const MCP_CONNECT_RETRIES: usize = 3;
/// Delay before the first retry; each later retry doubles it.
const MCP_CONNECT_RETRY_BASE: Duration = Duration::from_millis(500);

/// Whether a failed first connection is worth retrying: a timeout, a refused
/// or reset connection, or an HTTP 5xx. Authentication problems, other 4xx
/// responses, refused redirects and cancellation are final.
pub(crate) fn is_transient_connect_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    let lower = text.to_ascii_lowercase();
    const FINAL: [&str; 7] = [
        "cancelled",
        "auth required",
        "auth error",
        "insufficient scope",
        "unauthorized",
        "forbidden",
        "disabled",
    ];
    if FINAL.iter().any(|marker| lower.contains(marker)) {
        return false;
    }
    if let Some(code) = text.match_indices("HTTP ").find_map(|(at, _)| {
        text.get(at + 5..at + 8)
            .and_then(|digits| digits.parse::<u16>().ok())
    }) {
        // 408 and 429 are retried too (a rate-limited first connect is
        // common); other 4xx responses are final.
        return (500..600).contains(&code) || matches!(code, 408 | 429);
    }
    // A hung handshake already consumed its whole attempt budget: retrying it
    // would multiply the startup timeout (mini-agent-0yrql).
    if lower.contains("initialization timed out") {
        return false;
    }
    const TRANSIENT: [&str; 6] = [
        "timed out",
        "connection refused",
        "connection reset",
        "error sending request",
        "tcp connect",
        "unexpected end of stream",
    ];
    TRANSIENT.iter().any(|marker| lower.contains(marker))
}

/// Server-requested wait (`Retry-After` seconds) recorded in an HTTP error
/// message as `[retry-after: N]`.
fn retry_after_hint(error: &anyhow::Error) -> Option<Duration> {
    let text = format!("{error:#}");
    let rest = text.split_once("[retry-after: ")?.1;
    let seconds = rest.split_once(']')?.0.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Runs `attempt`, retrying a transient failure up to `retries` times with a
/// doubling backoff from `base` (or the server's `Retry-After` when longer).
/// The whole sequence, attempts and waits included, is bounded by `budget`
/// (the configured startup timeout): an attempt is cut off when it runs out
/// and no retry is started whose wait would not fit. Cancellation of the work
/// scope ends it.
pub(crate) async fn retry_transient_connect<T, F, Fut>(
    mut attempt: F,
    retries: usize,
    base: Duration,
    budget: Duration,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut tried = 0usize;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let result = match tokio::time::timeout_at(deadline, attempt()).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "MCP HTTP connection failed: initialization timed out after {} ms (startup budget)",
                budget.as_millis()
            )),
        };
        match result {
            Ok(value) => return Ok(value),
            Err(error) if tried < retries && is_transient_connect_error(&error) => {
                let mut delay = base.saturating_mul(1u32 << tried.min(16));
                if let Some(hint) = retry_after_hint(&error) {
                    delay = delay.max(hint);
                }
                if tokio::time::Instant::now() + delay >= deadline {
                    return Err(error);
                }
                tracing::debug!(
                    "MCP first connection failed transiently; retry {} of {retries}",
                    tried + 1
                );
                tokio::select! {
                    biased;
                    _ = crate::agent::runner::current_work_scope_cancelled() => return Err(error),
                    _ = tokio::time::sleep(delay) => {}
                }
                tried += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Test hook: the retrying HTTP first connect with a short backoff.
#[cfg(test)]
pub(crate) async fn connect_http_retrying_for_test(
    url: &str,
    base: Duration,
) -> anyhow::Result<McpClientHandle> {
    let headers = HashMap::new();
    retry_transient_connect(
        || {
            McpClientHandle::connect_http_with_timeout(
                CompactString::new("t"),
                url,
                &headers,
                None,
                None,
                Duration::from_secs(5),
            )
        },
        MCP_CONNECT_RETRIES,
        base,
        Duration::from_secs(5),
    )
    .await
}

/// Tool-count ceiling for a server's `defer` setting.
pub(crate) const fn tool_cap_for(defer: super::config::McpDefer) -> usize {
    match defer {
        super::config::McpDefer::Eager => MCP_LIST_TOOLS_MAX_TOOLS,
        _ => MCP_LIST_TOOLS_MAX_DEFERRED_TOOLS,
    }
}

/// Tools a listing could not keep because of the tool-count cap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OverCap {
    /// Tools counted beyond the cap (a lower bound if `exact` is false).
    pub(crate) count: usize,
    /// The first few omitted names.
    pub(crate) names: Vec<String>,
    /// Pagination finished, so `count` is the whole overflow.
    pub(crate) exact: bool,
}
const MCP_STDERR_LIMIT: usize = 8 * 1024;
const MCP_STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// One listed tool plus its model-facing metadata, bounded once when the
/// listing is cached. `bounded` is `None` when the input schema was rejected.
pub struct ListedTool {
    pub definition: Tool,
    pub(crate) bounded: Option<BoundedModelMetadata>,
}

#[derive(Clone)]
pub(crate) struct BoundedModelMetadata {
    pub(crate) description: Arc<str>,
    pub(crate) parameters: Arc<serde_json::Value>,
    pub(crate) description_truncated: bool,
    /// How the input schema was fitted to the model-facing bound.
    pub(crate) schema_fit: super::tool::SchemaFit,
}

/// A successful `tools/list` of one connection generation with every tool's
/// bounded model metadata. It depends only on the listed definitions, so it
/// is valid exactly as long as the cell holding it (a reconnect is a new
/// handle and a new cell).
pub struct ListedTools {
    pub entries: Vec<ListedTool>,
    pub(crate) reached_tool_cap: bool,
    pub(crate) over_cap: OverCap,
    /// Tools the server listed that the entry's `enabled_tools` /
    /// `disabled_tools` selection excluded (they do not count toward the cap).
    pub(crate) filtered_by_config: usize,
}

pub(crate) async fn list_bounded_tools(
    peer: &Peer<RoleClient>,
    timeout: Duration,
    options: &super::config::McpServerOptions,
    defer: super::config::McpDefer,
) -> Result<Arc<ListedTools>, ServiceError> {
    let cap = tool_cap_for(defer);
    let found =
        list_all_tools_selected(peer, timeout, &|name| options.admits_tool(name), cap).await?;
    let mut listed = bound_listed_tools(found.tools);
    listed.filtered_by_config = found.filtered;
    listed.over_cap = found.over_cap;
    // A catalogue of exactly the cap that was listed to the end lost nothing;
    // report a cut only when tools were counted past the cap or the listing
    // stopped before the server's last page.
    listed.reached_tool_cap =
        listed.over_cap.count > 0 || (listed.entries.len() >= cap && !listed.over_cap.exact);
    Ok(Arc::new(listed))
}

pub(crate) fn bound_listed_tools(tools: Vec<Tool>) -> ListedTools {
    let entries = tools
        .into_iter()
        .map(|definition| {
            let bounded = super::tool::McpTool::bounded_model_metadata(&definition)
                .ok()
                .map(|fitted| BoundedModelMetadata {
                    description: Arc::from(fitted.description),
                    parameters: Arc::new(fitted.parameters),
                    description_truncated: fitted.description_truncated,
                    schema_fit: fitted.schema_fit,
                });
            ListedTool {
                definition,
                bounded,
            }
        })
        .collect();
    ListedTools {
        entries,
        reached_tool_cap: false,
        over_cap: OverCap::default(),
        filtered_by_config: 0,
    }
}

/// Per-connection `tools/list` cache invalidated by
/// `notifications/tools/list_changed`. The flag is shared with the
/// connection's client handler, which sets it when the notification arrives.
#[derive(Default)]
pub(crate) struct ToolListCache {
    cell: Mutex<Arc<tokio::sync::OnceCell<Arc<ListedTools>>>>,
    changed: Arc<std::sync::atomic::AtomicBool>,
    /// `tools/list_changed` flag of a reconnect replacement connection; read
    /// and consumed together with `changed`.
    adopted: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
}

/// Outcome of a cached listing; `kept_previous` is set when a relist after
/// `list_changed` failed and the previous successful set was retained.
pub(crate) struct CachedListing {
    pub(crate) listed: Arc<ListedTools>,
    pub(crate) kept_previous: Option<ServiceError>,
}

impl ToolListCache {
    pub(crate) fn with_flag(changed: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            cell: Mutex::default(),
            changed,
            adopted: Mutex::default(),
        }
    }

    /// After a reconnect: marks the cached listing stale (the next collection
    /// relists from the new connection, keeping the previous set if that
    /// fails) and follows the replacement connection's `list_changed` flag.
    pub(crate) fn adopt_replacement(&self, flag: Arc<std::sync::atomic::AtomicBool>) {
        *self.adopted.lock().unwrap_or_else(|e| e.into_inner()) = Some(flag);
        self.changed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn current(&self) -> Arc<tokio::sync::OnceCell<Arc<ListedTools>>> {
        self.cell.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether the server announced a changed tool list since the last listing.
    pub(crate) fn is_stale(&self) -> bool {
        self.changed.load(std::sync::atomic::Ordering::Acquire)
            || self
                .adopted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
    }

    #[cfg(test)]
    pub(crate) fn initialized(&self) -> bool {
        self.current().initialized()
    }

    #[cfg(test)]
    pub(crate) fn get(&self) -> Option<Arc<ListedTools>> {
        self.current().get().cloned()
    }

    /// The cached listing, listing with `list` on first use or after a
    /// `list_changed` notification. A failed relist keeps (and re-caches) the
    /// previous successful set and reports the error; a failed first listing
    /// is returned and not cached.
    pub(crate) async fn get_or_list<F, Fut>(&self, list: F) -> Result<CachedListing, ServiceError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Arc<ListedTools>, ServiceError>>,
    {
        let mut previous = None;
        let adopted = self
            .adopted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|flag| flag.swap(false, std::sync::atomic::Ordering::AcqRel));
        if self
            .changed
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            || adopted
        {
            let mut cell = self.cell.lock().unwrap_or_else(|e| e.into_inner());
            previous = cell.get().cloned();
            *cell = Arc::new(tokio::sync::OnceCell::new());
        }
        let cell = self.current();
        match cell.get_or_try_init(list).await {
            Ok(listed) => Ok(CachedListing {
                listed: listed.clone(),
                kept_previous: None,
            }),
            Err(error) => match previous {
                Some(previous) => {
                    let _ = cell.set(previous.clone());
                    Ok(CachedListing {
                        listed: previous,
                        kept_previous: Some(error),
                    })
                }
                None => Err(error),
            },
        }
    }
}

pub struct McpClientHandle {
    pub server_name: CompactString,
    pub trusted_identity: Option<TrustedMcpServer>,
    pub running_service: RunningService<RoleClient, super::elicitation::Handler>,
    pub(crate) elicitation: super::elicitation::Handler,
    pub(crate) source_proof: Option<std::sync::Arc<super::project::ServerSourceProof>>,
    pub(crate) liveness: std::sync::Arc<()>,
    pub(crate) auxiliary: Arc<super::catalogue::AuxiliaryCache>,
    /// Successful `tools/list` of this connection generation. A reconnect is a
    /// new handle (and cache); failures are not cached. A server's
    /// `notifications/tools/list_changed` marks it stale, so the next
    /// collection (the next agent build) lists again.
    pub(crate) tools: Arc<ToolListCache>,
    /// Per-server options captured from the configuration that connected.
    pub(crate) options: super::config::McpServerOptions,
    /// Schema-loading mode of the connecting configuration; it sets the
    /// tool-count ceiling of this server's listing.
    pub(crate) defer: super::config::McpDefer,
    /// Auto-reconnect of a dropped remote server (URL servers only); shared
    /// with that server's tools. Stdio servers stay manual.
    pub(crate) link: Option<Arc<super::reconnect::ServerLink>>,
}

/// Longest stdout line (one JSON-RPC message) accepted from a stdio MCP
/// server. RMCP buffers each line until its newline with no bound of its own,
/// so a server writing a large newline-free blob would otherwise grow the
/// agent's memory without limit, independent of any call timeout.
pub(crate) const MCP_STDIO_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Server stdout that fails the read once a line grows past `max_line`
/// bytes. The owning transport then kills the server.
struct LineCappedStdout<R> {
    inner: R,
    max_line: usize,
    current_line: usize,
    raw_line: super::forms::LineObserver,
    forms: super::forms::RawForms,
    overflowed: Arc<std::sync::atomic::AtomicBool>,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for LineCappedStdout<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.overflowed.load(std::sync::atomic::Ordering::Acquire) {
            return std::task::Poll::Ready(Err(line_overflow_error(this.max_line)));
        }
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &poll {
            let mut rest = &buf.filled()[before..];
            while !rest.is_empty() {
                let newline = super::forms::find_newline(rest);
                let segment = &rest[..newline.unwrap_or(rest.len())];
                this.current_line = this.current_line.saturating_add(segment.len());
                if this.current_line > this.max_line {
                    break;
                }
                this.raw_line.feed(segment);
                let Some(newline) = newline else {
                    break;
                };
                this.raw_line.end(&this.forms);
                this.current_line = 0;
                rest = &rest[newline + 1..];
            }
            if this.current_line > this.max_line {
                this.overflowed
                    .store(true, std::sync::atomic::Ordering::Release);
                // A failed read must not report filled bytes.
                buf.set_filled(before);
                return std::task::Poll::Ready(Err(line_overflow_error(this.max_line)));
            }
        }
        poll
    }
}

fn line_overflow_error(max_line: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("MCP server wrote a stdout line longer than {max_line} bytes"),
    )
}

type StdioProtocol = AsyncRwTransport<RoleClient, LineCappedStdout<ChildStdout>, ChildStdin>;

struct OwnedStdioTransport {
    child: Option<Box<dyn ChildWrapper>>,
    protocol: Option<StdioProtocol>,
    overflowed: Arc<std::sync::atomic::AtomicBool>,
    cleanup_complete: Option<tokio::sync::oneshot::Sender<()>>,
}

impl OwnedStdioTransport {
    fn new(
        child: Box<dyn ChildWrapper>,
        stdout: ChildStdout,
        stdin: ChildStdin,
        max_line: usize,
        forms: super::forms::RawForms,
    ) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (cleanup_complete, completion) = tokio::sync::oneshot::channel();
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stdout = LineCappedStdout {
            inner: stdout,
            max_line,
            current_line: 0,
            raw_line: Default::default(),
            forms,
            overflowed: Arc::clone(&overflowed),
        };
        (
            Self {
                child: Some(child),
                protocol: Some(AsyncRwTransport::new_client(stdout, stdin)),
                overflowed,
                cleanup_complete: Some(cleanup_complete),
            },
            completion,
        )
    }

    fn start_cleanup(&mut self, graceful: bool) -> Option<JoinHandle<std::io::Result<()>>> {
        let child = self.child.take()?;
        let process_group = child.id();
        // Dropping the protocol closes the server's stdin/stdout pipes. Keep
        // child ownership in one cleanup task so cancellation cannot detach an
        // untracked wait.
        drop(self.protocol.take());
        let child = Some(child);
        let completion = self.cleanup_complete.take();
        if !graceful {
            Self::terminate_tree(process_group);
        }
        Some(tokio::spawn(async move {
            let result = if let Some(mut child) = child {
                let exited = if graceful {
                    tokio::time::timeout(Duration::from_secs(3), child.wait())
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .is_some()
                } else {
                    false
                };
                if exited {
                    Ok(())
                } else {
                    Self::terminate_tree(process_group);
                    Box::into_pin(child.kill()).await
                }
            } else {
                Ok(())
            };
            Self::terminate_tree(process_group);
            if let Some(completion) = completion {
                let _ = completion.send(());
            }
            result
        }))
    }

    fn terminate_tree(process_group: Option<u32>) {
        if let Some(pid) = process_group {
            crate::sandbox::kill_process_group_if_live(pid);
        }
    }
}

impl Drop for OwnedStdioTransport {
    fn drop(&mut self) {
        // The task retains child ownership through kill/reap. Initialization
        // cancellation waits on cleanup_complete before returning to ACP.
        drop(self.start_cleanup(false));
    }
}

impl Transport<RoleClient> for OwnedStdioTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let send = self.protocol.as_mut().map(|inner| inner.send(item));
        async move {
            match send {
                Some(send) => send.await,
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "MCP transport is closed",
                )),
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let received = match self.protocol.as_mut() {
            Some(inner) => inner.receive().await,
            None => None,
        };
        if received.is_none() && self.overflowed.load(std::sync::atomic::Ordering::Acquire) {
            tracing::warn!(
                "MCP stdio server exceeded the {} byte line limit; terminating it",
                MCP_STDIO_MAX_LINE_BYTES
            );
            drop(self.start_cleanup(false));
        }
        received
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        match self.start_cleanup(true) {
            Some(cleanup) => cleanup.await.map_err(std::io::Error::other)?,
            None => Ok(()),
        }
    }
}

type SpawnedStdioServer = (
    OwnedStdioTransport,
    tokio::sync::oneshot::Receiver<()>,
    Option<tokio::process::ChildStderr>,
);

/// Launch a stdio MCP server in its owned process tree with piped stdio and a
/// line-capped protocol reader.
fn spawn_stdio_server(
    cmd: Command,
    max_line: usize,
    forms: super::forms::RawForms,
) -> std::io::Result<SpawnedStdioServer> {
    let mut command = owned_workspace_service_tree(cmd);
    command
        .command_mut()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn_guarded()?;
    let stdin = child.inner_mut().stdin().take();
    let stdout = child.inner_mut().stdout().take();
    let stderr = child.inner_mut().stderr().take();
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        let process_group = child.id();
        OwnedStdioTransport::terminate_tree(process_group);
        return Err(std::io::Error::other(
            "MCP server stdio pipes were not created",
        ));
    };
    let (transport, completion) = OwnedStdioTransport::new(child, stdout, stdin, max_line, forms);
    Ok((transport, completion, stderr))
}

impl Drop for McpClientHandle {
    fn drop(&mut self) {
        // RunningService's own DropGuard is asynchronous. Signal cancellation
        // immediately. The service task's OwnedStdioTransport then closes and
        // reaps its Unix process group or Windows Job Object.
        self.running_service.cancellation_token().cancel();
        if let Some(link) = &self.link {
            link.retire();
        }
    }
}

impl McpClientHandle {
    #[cfg(test)]
    pub async fn connect(
        server_name: CompactString,
        config: &McpServerConfig,
    ) -> anyhow::Result<Self> {
        let workspace = std::env::current_dir().unwrap_or_default();
        Self::connect_in(server_name, config, &workspace).await
    }

    pub(crate) async fn connect_in(
        server_name: CompactString,
        config: &McpServerConfig,
        workspace: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let timeout = config.options().startup_timeout(MCP_INITIALIZE_TIMEOUT);
        Self::connect_with_timeout_in(server_name, config, timeout, workspace).await
    }

    #[cfg(test)]
    pub(crate) async fn connect_with_timeout(
        server_name: CompactString,
        config: &McpServerConfig,
        initialize_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let workspace = std::env::current_dir().unwrap_or_default();
        Self::connect_with_timeout_in(server_name, config, initialize_timeout, &workspace).await
    }

    pub(crate) async fn connect_with_timeout_in(
        server_name: CompactString,
        config: &McpServerConfig,
        initialize_timeout: Duration,
        workspace: &std::path::Path,
    ) -> anyhow::Result<Self> {
        Self::connect_bound_with_timeout_in(
            server_name,
            config,
            initialize_timeout,
            workspace,
            None,
            None,
            true,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn connect_in_binding(
        server_name: CompactString,
        config: &McpServerConfig,
        workspace: &Arc<crate::paths::WorkspaceBinding>,
    ) -> anyhow::Result<Self> {
        Self::connect_bound_with_timeout_in(
            server_name,
            config,
            MCP_INITIALIZE_TIMEOUT,
            workspace.root(),
            Some(workspace),
            None,
            true,
        )
        .await
    }

    pub(crate) async fn connect_in_binding_guarded(
        server_name: CompactString,
        config: &McpServerConfig,
        workspace: &Arc<crate::paths::WorkspaceBinding>,
        admission: &(dyn Fn() -> bool + Send + Sync),
    ) -> anyhow::Result<Self> {
        let timeout = config.options().startup_timeout(MCP_INITIALIZE_TIMEOUT);
        Self::connect_bound_with_timeout_in(
            server_name,
            config,
            timeout,
            workspace.root(),
            Some(workspace),
            Some(admission),
            true,
        )
        .await
    }

    async fn connect_bound_with_timeout_in(
        server_name: CompactString,
        config: &McpServerConfig,
        initialize_timeout: Duration,
        workspace: &std::path::Path,
        binding: Option<&Arc<crate::paths::WorkspaceBinding>>,
        admission: Option<&(dyn Fn() -> bool + Send + Sync)>,
        retry_http: bool,
    ) -> anyhow::Result<Self> {
        let validate = || -> anyhow::Result<()> {
            if let Some(binding) = binding {
                binding.validate().map_err(anyhow::Error::msg)?;
            }
            anyhow::ensure!(
                admission.is_none_or(|check| check()),
                "MCP launch source changed"
            );
            Ok(())
        };
        validate()?;
        if crate::agent::runner::current_work_scope_is_cancelled() {
            anyhow::bail!("MCP connection cancelled");
        }
        crate::config::managed::check_mcp_server(&server_name, config)
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            config.options().is_enabled(),
            "MCP server '{server_name}' is disabled by configuration (enabled = false)"
        );
        let result: anyhow::Result<Self> = match config {
            McpServerConfig::Command {
                command,
                args,
                cwd,
                env,
                inherit_env,
                sandbox,
                network,
                options,
                ..
            } => {
                anyhow::ensure!(
                    options.headers_helper.is_none(),
                    "headersHelper applies only to URL servers"
                );
                tracing::debug!(
                    "MCP command transport configured ({} args, {} env vars, {} inherited env vars)",
                    args.len(),
                    env.len(),
                    inherit_env.len(),
                );
                let cmd = stdio_command(
                    command,
                    args,
                    workspace,
                    cwd.as_deref(),
                    env,
                    inherit_env,
                    sandbox.as_deref(),
                    *network,
                    std::env::var_os("PATH").as_deref(),
                )
                .map_err(|error| {
                    bounded_stdio_error(
                        &server_name,
                        &format!("command resolution failed: {error}"),
                        "",
                    )
                })?;
                validate()?;
                // A stdio server already runs in the captured workspace; it
                // may learn that root through `roots/list`.
                let elicitation = super::elicitation::Handler::with_root(workspace);
                let tools_changed = elicitation.tools_changed.clone();
                let auxiliary_changed = elicitation.auxiliary_changed.clone();
                let (transport, cleanup_complete, stderr) =
                    spawn_stdio_server(cmd, MCP_STDIO_MAX_LINE_BYTES, elicitation.forms.clone())
                        .map_err(|error| {
                            bounded_stdio_error(
                                &server_name,
                                &format!("command spawn failed: {error}"),
                                "",
                            )
                        })?;
                let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
                let stderr_task =
                    stderr.map(|stderr| capture_stderr(stderr, Arc::downgrade(&stderr_buffer)));
                let initialized = tokio::select! {
                    biased;
                    _ = crate::agent::runner::current_work_scope_cancelled() => None,
                    result = tokio::time::timeout(initialize_timeout, serve_client(elicitation.clone(), transport)) => Some(result),
                };
                let running_service = match initialized {
                    None => {
                        let _ = cleanup_complete.await;
                        return Err(stdio_connect_error(
                            &server_name,
                            "initialization cancelled".into(),
                            stderr_task,
                            stderr_buffer,
                        )
                        .await);
                    }
                    Some(Ok(Ok(service))) => service,
                    Some(Ok(Err(_))) => {
                        let _ = cleanup_complete.await;
                        return Err(stdio_connect_error(
                            &server_name,
                            "initialization failed".to_string(),
                            stderr_task,
                            stderr_buffer,
                        )
                        .await);
                    }
                    Some(Err(_)) => {
                        let _ = cleanup_complete.await;
                        return Err(stdio_connect_error(
                            &server_name,
                            format!(
                                "initialization timed out after {} ms",
                                initialize_timeout.as_millis()
                            ),
                            stderr_task,
                            stderr_buffer,
                        )
                        .await);
                    }
                };
                // Connected: nothing can read the diagnostic any more, so the
                // capture task (which holds only a `Weak`) now just drains.
                drop(stderr_buffer);
                Ok(Self {
                    server_name,
                    trusted_identity: config.trusted_identity(),
                    running_service,
                    elicitation,
                    source_proof: None,
                    liveness: std::sync::Arc::new(()),
                    auxiliary: Arc::new(super::catalogue::AuxiliaryCache::with_flag(
                        auxiliary_changed,
                    )),
                    tools: Arc::new(ToolListCache::with_flag(tools_changed)),
                    options: Default::default(),
                    defer: Default::default(),
                    link: None,
                })
            }
            McpServerConfig::Url {
                url,
                allow_local,
                headers,
                oauth,
                options,
                ..
            } => {
                let endpoint = tokio::select! {
                    biased;
                    _ = crate::agent::runner::current_work_scope_cancelled() => anyhow::bail!("MCP connection cancelled"),
                    result = validate_mcp_server_url_policy_cidrs(url, *allow_local, &options.allowed_cidrs) => result?,
                };
                validate()?;
                // A configured headers helper runs after URL policy passes and
                // only under the explicit `mcp_headers_helper` opt-in.
                let merged;
                let headers = if let Some(helper) = &options.headers_helper {
                    merged = tokio::select! {
                        biased;
                        _ = crate::agent::runner::current_work_scope_cancelled() => anyhow::bail!("MCP connection cancelled"),
                        result = super::headers_helper::merged_headers(
                            &server_name,
                            helper,
                            headers,
                            workspace,
                            super::headers_helper::is_enabled(),
                            super::headers_helper::HELPER_TIMEOUT,
                        ) => result?,
                    };
                    validate()?;
                    &merged
                } else {
                    headers
                };
                let retries = if retry_http { MCP_CONNECT_RETRIES } else { 0 };
                let mut handle = retry_transient_connect(
                    || async {
                        validate()?;
                        Self::connect_http_with_timeout(
                            server_name.clone(),
                            url,
                            headers,
                            oauth.as_ref(),
                            Some(&endpoint),
                            initialize_timeout,
                        )
                        .await
                    },
                    retries,
                    MCP_CONNECT_RETRY_BASE,
                    initialize_timeout,
                )
                .await?;
                handle.trusted_identity = config.trusted_identity();
                Ok(handle)
            }
            McpServerConfig::BuiltIn { identity, headers } => {
                validate()?;
                let retries = if retry_http { MCP_CONNECT_RETRIES } else { 0 };
                let mut handle = retry_transient_connect(
                    || async {
                        validate()?;
                        Self::connect_http_with_timeout(
                            server_name.clone(),
                            identity.endpoint(),
                            headers,
                            None,
                            None,
                            initialize_timeout,
                        )
                        .await
                    },
                    retries,
                    MCP_CONNECT_RETRY_BASE,
                    initialize_timeout,
                )
                .await?;
                handle.trusted_identity = config.trusted_identity();
                Ok(handle)
            }
        };
        let mut handle = result?;
        handle.options = config.options();
        handle.defer = config.defer();
        if validate().is_err() || crate::agent::runner::current_work_scope_is_cancelled() {
            // A completed handshake cannot confer authority after its retained
            // workspace changed. Join this exact candidate before rejecting it.
            handle.running_service.close().await?;
            anyhow::bail!("MCP workspace changed or connection was cancelled");
        }
        Ok(handle)
    }

    /// Connect a streamable-HTTP server with every network step bounded by
    /// `initialize_timeout`: OAuth client construction (stored-token restore,
    /// which may refresh over the network), the TCP/TLS connect, and the MCP
    /// initialization handshake.
    ///
    /// The caller is responsible for URL validation; this is the transport
    /// layer only, so tests can drive it against a loopback listener. A
    /// validated `endpoint` pins the server host to the addresses that passed
    /// validation for the transport and the OAuth client alike, and holds
    /// every other name they resolve to the same public-address policy (see
    /// [`super::egress`]); `None` is for built-in endpoints and tests.
    pub(crate) async fn connect_http_with_timeout(
        server_name: CompactString,
        url: &str,
        headers: &HashMap<String, String>,
        oauth: Option<&OAuthConfig>,
        endpoint: Option<&ValidatedEndpoint>,
        initialize_timeout: Duration,
    ) -> anyhow::Result<Self> {
        tracing::debug!(
            "MCP HTTP transport configured ({} headers, OAuth: {})",
            headers.len(),
            oauth.is_some(),
        );
        let custom_headers = parse_headers(headers)?;
        let cfg =
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                url,
            )
            .custom_headers(custom_headers);
        let oauth_settings = oauth.and_then(|o| o.settings());
        let elicitation = super::elicitation::Handler::default();
        let tools_changed = elicitation.tools_changed.clone();
        let auxiliary_changed = elicitation.auxiliary_changed.clone();
        // Every response body and SSE event is capped (see `bounded_http`).
        let http_client = BoundedHttpClient::new(
            http_client_for(initialize_timeout, endpoint)?,
            super::bounded_http::MCP_HTTP_MAX_BODY_BYTES,
        )
        .with_forms(elicitation.forms.clone());

        let connect = async {
            if let Some(settings) = oauth_settings {
                let auth_client = super::oauth::build_auth_client(
                    &server_name,
                    url,
                    &settings,
                    http_client,
                    endpoint,
                )
                .await?;
                type AuthHttpClient = rmcp::transport::StreamableHttpClientTransport<
                    rmcp::transport::auth::AuthClient<BoundedHttpClient>,
                >;
                let transport = AuthHttpClient::with_client(auth_client, cfg);
                serve_client(elicitation.clone(), transport)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
                    })
            } else {
                type HttpClient = rmcp::transport::StreamableHttpClientTransport<BoundedHttpClient>;
                let transport = HttpClient::with_client(http_client, cfg);
                serve_client(elicitation.clone(), transport)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
                    })
            }
        };
        let initialized = tokio::select! {
            biased;
            _ = crate::agent::runner::current_work_scope_cancelled() => anyhow::bail!("MCP HTTP connection cancelled"),
            result = tokio::time::timeout(initialize_timeout, connect) => result,
        };
        let running_service = match initialized {
            Ok(result) => result?,
            Err(_) => anyhow::bail!(
                "MCP HTTP connection failed for '{server_name}': initialization timed out after {} ms",
                initialize_timeout.as_millis()
            ),
        };
        Ok(Self {
            server_name,
            trusted_identity: None,
            running_service,
            elicitation,
            source_proof: None,
            liveness: std::sync::Arc::new(()),
            auxiliary: Arc::new(super::catalogue::AuxiliaryCache::with_flag(
                auxiliary_changed,
            )),
            tools: Arc::new(ToolListCache::with_flag(tools_changed)),
            options: Default::default(),
            defer: Default::default(),
            link: None,
        })
    }

    pub fn peer(&self) -> rmcp::service::Peer<RoleClient> {
        self.running_service.peer().clone()
    }

    /// The peer of the live connection generation: the reconnect link's
    /// current peer for a URL server, otherwise this handle's own.
    pub(crate) fn live_peer(&self) -> rmcp::service::Peer<RoleClient> {
        match &self.link {
            Some(link) => link.conn().peer,
            None => self.peer(),
        }
    }

    pub(crate) fn with_source_proof(mut self, proof: super::project::ServerSourceProof) -> Self {
        let proof = std::sync::Arc::new(proof);
        self.link = self.reconnect_link(&proof);
        self.source_proof = Some(proof);
        self
    }

    /// The auto-reconnect link of a URL server captured with its exact
    /// definition and workspace; `None` for stdio servers (manual reconnect).
    fn reconnect_link(
        &self,
        proof: &Arc<super::project::ServerSourceProof>,
    ) -> Option<Arc<super::reconnect::ServerLink>> {
        let (workspace, name, config) = proof.reconnect_target()?;
        let proof = Arc::clone(proof);
        let connector: super::reconnect::Connector = Arc::new(move || {
            let (workspace, name, config, proof) = (
                workspace.clone(),
                name.clone(),
                config.clone(),
                proof.clone(),
            );
            Box::pin(async move {
                anyhow::ensure!(proof.is_current(), "MCP source changed");
                // The link owns the attempt budget, so no inner retries.
                Self::connect_bound_with_timeout_in(
                    CompactString::new(&name),
                    &config,
                    config.options().startup_timeout(MCP_INITIALIZE_TIMEOUT),
                    workspace.root(),
                    Some(&workspace),
                    Some(&|| proof.is_current()),
                    false,
                )
                .await
            })
        });
        Some(super::reconnect::ServerLink::new(
            self.server_name.clone(),
            super::reconnect::Conn {
                peer: self.peer(),
                elicitation: self.elicitation.clone(),
                tools: Arc::clone(&self.tools),
                auxiliary: Arc::clone(&self.auxiliary),
            },
            connector,
            super::reconnect::RECONNECT_BASE_DELAY,
        ))
    }

    /// Called by the owned connection actor after its immutable source proof
    /// is attached. Diagnostic and picker consumers only read the cached cell.
    #[cfg(test)]
    pub(crate) async fn discover_auxiliary(&self) {
        self.auxiliary
            .ensure(
                &super::catalogue::RpcClient::capture(self),
                Duration::from_secs(3),
            )
            .await;
    }

    /// Fill (or, after a prompts/resources `list_changed` notification,
    /// refresh) the auxiliary catalogue in a detached task so prompt/resource
    /// discovery never sits on the connect critical path. Status and the
    /// pickers already treat a pending cell as empty. The task owns only an
    /// [`RpcClient`](super::catalogue::RpcClient) capture with a weak liveness
    /// witness, so retirement ends it at the next request boundary.
    pub(crate) fn spawn_auxiliary_discovery(&self) {
        if self.auxiliary.initialized() && !self.auxiliary.is_stale() {
            return;
        }
        let cache = Arc::clone(&self.auxiliary);
        let client = super::catalogue::RpcClient::capture(self);
        tokio::spawn(async move {
            cache.ensure(&client, Duration::from_secs(3)).await;
        });
    }

    #[cfg(test)]
    pub(crate) fn auxiliary_mark_stale_for_test(&self) {
        self.elicitation
            .auxiliary_changed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Refresh the catalogue now when the server announced a change, so a
    /// prompt or resource lookup never acts on a removed entry.
    pub(crate) async fn refresh_auxiliary_if_stale(&self) {
        if self.auxiliary.is_stale() {
            self.auxiliary
                .ensure(
                    &super::catalogue::RpcClient::capture(self),
                    Duration::from_secs(3),
                )
                .await;
        }
    }

    /// The cached listing of this connection generation, listing (and
    /// bounding) once on first use. Same cell the agent build fills.
    pub async fn cached_tools(&self) -> Result<Arc<ListedTools>, ServiceError> {
        // The live generation's peer (like the agent build path): after a
        // reconnect the original peer is closed and listing on it would fail
        // and consume the reconnect staleness flag.
        let peer = self.live_peer();
        self.tools
            .get_or_list(|| {
                list_bounded_tools(
                    &peer,
                    self.options.list_timeout(MCP_LIST_TOOLS_TIMEOUT),
                    &self.options,
                    self.defer,
                )
            })
            .await
            .map(|cached| cached.listed)
    }
}

/// Build the reqwest client used for MCP HTTP transports. The transport wraps
/// it in [`BoundedHttpClient`], which caps response bodies and SSE events.
///
/// Only the connect phase is time-bounded here. A whole-request timeout would sever
/// the long-lived SSE stream and legitimate long tool calls; those are bounded
/// per RPC through [`PeerRequestOptions`] instead.
#[cfg(test)]
pub(crate) fn http_client(connect_timeout: Duration) -> anyhow::Result<reqwest::Client> {
    http_client_for(connect_timeout, None)
}

/// [`http_client`] pinned to a validated endpoint when there is one, so
/// neither the first connect nor an SSE reconnect resolves the host again.
fn http_client_for(
    connect_timeout: Duration,
    endpoint: Option<&ValidatedEndpoint>,
) -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout);
    match endpoint {
        Some(endpoint) => endpoint.pin(builder),
        None => builder,
    }
    .build()
    .map_err(|error| anyhow::anyhow!("MCP HTTP client construction failed: {error}"))
}

/// `tools/list` with a total time budget across every paginated page and a cap
/// on the number of pages. A timeout cancels the in-flight request on the
/// server side (rmcp sends `notifications/cancelled`) and surfaces as
/// [`ServiceError::Timeout`].
#[cfg(test)]
pub(crate) async fn list_all_tools_bounded(
    peer: &Peer<RoleClient>,
    timeout: Duration,
) -> Result<Vec<Tool>, ServiceError> {
    list_all_tools_selected(peer, timeout, &|_| true, MCP_LIST_TOOLS_MAX_TOOLS)
        .await
        .map(|found| found.tools)
}

/// Result of one bounded `tools/list` enumeration.
pub(crate) struct FoundTools {
    pub(crate) tools: Vec<Tool>,
    /// Tools excluded by the entry's selection.
    pub(crate) filtered: usize,
    pub(crate) over_cap: OverCap,
}

/// Bounded `tools/list` keeping only tools `admit` accepts. Selection
/// runs per page before the tool cap, so excluded tools never displace an
/// admitted one. Pagination continues past `cap` (within the time and page
/// bounds) only to count and name the tools the cap omits.
pub(crate) async fn list_all_tools_selected(
    peer: &Peer<RoleClient>,
    timeout: Duration,
    admit: &(dyn Fn(&str) -> bool + Sync),
    cap: usize,
) -> Result<FoundTools, ServiceError> {
    let mut filtered = 0_usize;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut tools = Vec::new();
    let mut over_cap = OverCap::default();
    let mut cursor: Option<String> = None;
    let mut pages = 0_usize;
    loop {
        let counting_overflow = tools.len() >= cap;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            if counting_overflow {
                break;
            }
            return Err(ServiceError::Timeout { timeout });
        }
        let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(
            PaginatedRequestParams::default().with_cursor(cursor.take()),
        ));
        let sent = peer
            .send_cancellable_request(request, PeerRequestOptions::with_timeout(remaining))
            .await;
        let response = match sent {
            Ok(handle) => handle.await_response().await,
            Err(error) => Err(error),
        };
        let result = match response {
            Ok(ServerResult::ListToolsResult(result)) => result,
            // Past the cap only counting is lost; keep the usable tools.
            Ok(_) | Err(_) if counting_overflow => break,
            Ok(_) => return Err(ServiceError::UnexpectedResponse),
            // Report the whole budget rather than the remaining slice.
            Err(ServiceError::Timeout { .. }) => return Err(ServiceError::Timeout { timeout }),
            Err(error) => return Err(error),
        };
        let page = result.tools.into_iter().filter(|tool| {
            let admitted = admit(tool.name.as_ref());
            filtered += usize::from(!admitted);
            admitted
        });
        for tool in page {
            if tools.len() < cap {
                tools.push(tool);
            } else {
                over_cap.count += 1;
                if over_cap.names.len() < MCP_OMITTED_TOOL_NAMES {
                    over_cap.names.push(tool.name.to_string());
                }
            }
        }
        pages += 1;
        match result.next_cursor {
            None => {
                over_cap.exact = true;
                break;
            }
            Some(_) if pages >= MCP_LIST_TOOLS_MAX_PAGES => {
                tracing::debug!(
                    "MCP tools/list stopped after {pages} pages; ignoring further cursors"
                );
                break;
            }
            next => cursor = next,
        }
    }
    Ok(FoundTools {
        tools,
        filtered,
        over_cap,
    })
}

/// Absolute ceiling for one `tools/call` that keeps sending progress
/// notifications (the call is otherwise bounded by its idle timeout).
pub(crate) const MCP_TOOL_PROGRESS_CEILING: Duration = Duration::from_secs(30 * 60);

/// `tools/call` with an idle timeout of `timeout` that every matching
/// `notifications/progress` restarts, under an absolute ceiling of
/// `max(timeout, MCP_TOOL_PROGRESS_CEILING)`. rmcp attaches a progress token
/// to every request, so a server that streams progress keeps a long call
/// alive while a silent one is still cut off after `timeout`. On expiry rmcp
/// notifies the server that the request was cancelled and the error is
/// [`ServiceError::Timeout`] carrying whichever bound fired.
#[cfg_attr(not(feature = "hooks"), allow(dead_code))]
pub(crate) async fn call_tool_bounded(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    timeout: Duration,
) -> Result<CallToolResult, ServiceError> {
    call_tool_observed(peer, params, timeout, None).await
}

/// Where a call's progress notifications are shown: the connection's
/// registry plus the transcript id and event channel of the call.
pub(crate) type ProgressDisplay = (
    super::progress::ProgressRegistry,
    compact_str::CompactString,
    tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
);

/// [`call_tool_bounded`] that also shows the call's progress notifications as
/// a live preview when `display` is given.
pub(crate) async fn call_tool_observed(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    timeout: Duration,
    display: Option<ProgressDisplay>,
) -> Result<CallToolResult, ServiceError> {
    call_tool_with_display(
        peer,
        params,
        timeout,
        timeout.max(MCP_TOOL_PROGRESS_CEILING),
        display,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn call_tool_with_ceiling(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    idle_timeout: Duration,
    ceiling: Duration,
) -> Result<CallToolResult, ServiceError> {
    call_tool_with_display(peer, params, idle_timeout, ceiling, None).await
}

async fn call_tool_with_display(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    idle_timeout: Duration,
    ceiling: Duration,
    display: Option<ProgressDisplay>,
) -> Result<CallToolResult, ServiceError> {
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let mut options = PeerRequestOptions::with_timeout(idle_timeout).reset_timeout_on_progress();
    options.max_total_timeout = Some(ceiling);
    let handle = peer.send_cancellable_request(request, options).await?;
    // Registered right after the send with nothing awaited in between; held
    // until the response (or an error) so the token never outlives the call.
    let _display = display.and_then(|(registry, id, events)| {
        registry.register(handle.progress_token.clone(), id, events)
    });
    match handle.await_response().await? {
        ServerResult::CallToolResult(result) => Ok(result),
        _ => Err(ServiceError::UnexpectedResponse),
    }
}

/// Collects up to [`MCP_STDERR_LIMIT`] bytes for connect diagnostics. It holds
/// only a `Weak`: once the connect path drops the buffer the task keeps
/// draining the pipe (so the server never blocks on a full stderr) but
/// discards every chunk without locking.
fn capture_stderr(
    mut stderr: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    buffer: std::sync::Weak<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = [0_u8; 1024];
        loop {
            let read = match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let Some(buffer) = buffer.upgrade() else {
                continue;
            };
            let mut captured = buffer.lock().unwrap_or_else(|error| error.into_inner());
            let remaining = MCP_STDERR_LIMIT.saturating_sub(captured.len());
            captured.extend_from_slice(&chunk[..read.min(remaining)]);
        }
    })
}

async fn stdio_connect_error(
    server_name: &str,
    reason: String,
    stderr_task: Option<JoinHandle<()>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
) -> anyhow::Error {
    if let Some(task) = stderr_task {
        let _ = tokio::time::timeout(MCP_STDERR_DRAIN_TIMEOUT, task).await;
    }
    let diagnostic = {
        let captured = stderr_buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        bounded_lossy_diagnostic(&captured)
    };
    bounded_stdio_error(server_name, &reason, &diagnostic)
}

fn bounded_stdio_error(server_name: &str, reason: &str, diagnostic: &str) -> anyhow::Error {
    let server_name = bounded_utf8(server_name.trim(), 256);
    let reason = bounded_utf8(reason.trim(), 512);
    let mut rendered = format!("MCP connection failed for '{server_name}': {reason}");
    if !diagnostic.is_empty() && rendered.len() < MCP_STDERR_LIMIT {
        append_bounded(&mut rendered, "; stderr: ", MCP_STDERR_LIMIT);
        append_bounded(&mut rendered, diagnostic, MCP_STDERR_LIMIT);
    }
    anyhow::anyhow!(rendered)
}

fn bounded_lossy_diagnostic(bytes: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(bytes);
    bounded_utf8(lossy.trim(), MCP_STDERR_LIMIT)
}

fn bounded_utf8(value: &str, limit: usize) -> String {
    let mut rendered = String::with_capacity(value.len().min(limit));
    append_bounded(&mut rendered, value, limit);
    rendered
}

fn append_bounded(rendered: &mut String, value: &str, limit: usize) {
    for character in value.chars() {
        if rendered.len() + character.len_utf8() > limit {
            break;
        }
        rendered.push(character);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn stdio_command(
    command: &str,
    args: &[String],
    workspace: &Path,
    configured_cwd: Option<&Path>,
    env: &HashMap<String, String>,
    inherit_env: &[String],
    sandbox_backend: Option<&str>,
    network: McpStdioNetwork,
    search_path: Option<&std::ffi::OsStr>,
) -> anyhow::Result<Command> {
    let cwd = match configured_cwd {
        Some(cwd) if cwd.is_absolute() => cwd.to_path_buf(),
        Some(cwd) => workspace.join(cwd),
        None => workspace.to_path_buf(),
    };
    let cwd = cwd.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "configured MCP stdio working directory '{}' is unavailable: {error}",
            cwd.display()
        )
    })?;
    // Resolve against the absolute entries of the parent's PATH before
    // clearing the child's ambient environment; relative/empty entries would
    // resolve against the workspace. A relative command with a directory
    // component is anchored only by an explicitly configured `cwd`. The
    // immutable absolute identity is what is ultimately passed to exec; the
    // child does not need PATH merely to locate itself.
    let program = crate::extras::executable_search::resolve_service_executable(
        command,
        search_path,
        configured_cwd.map(|_| cwd.as_path()),
    )
    .map_err(anyhow::Error::msg)?;

    let delegated = delegated_environment(inherit_env, env)?;

    if let Some(backend) = sandbox_backend {
        return Sandbox::new(true, backend)
            .wrap_workspace_service(
                &program,
                args,
                &cwd,
                &delegated,
                network == McpStdioNetwork::Deny,
            )
            .map_err(anyhow::Error::msg);
    }
    if network == McpStdioNetwork::Deny {
        anyhow::bail!("MCP stdio network denial requires an available workspace-service sandbox");
    }

    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(delegated);
    Ok(child)
}

fn delegated_environment(
    inherit_env: &[String],
    env: &HashMap<String, String>,
) -> anyhow::Result<Vec<(OsString, OsString)>> {
    let mut delegated = HashMap::<String, (OsString, OsString)>::new();
    for name in inherit_env {
        if name.is_empty() || name.contains('=') {
            anyhow::bail!(
                "MCP stdio inherited environment names must be non-empty and contain no '='"
            );
        }
        if let Some(value) = std::env::var_os(name) {
            delegated.insert(environment_identity(name), (OsString::from(name), value));
        }
    }
    for (name, value) in env {
        if name.is_empty() || name.contains('=') {
            anyhow::bail!("MCP stdio environment names must be non-empty and contain no '='");
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

fn parse_headers(
    headers: &HashMap<String, String>,
) -> anyhow::Result<HashMap<http::HeaderName, http::HeaderValue>> {
    let mut result = HashMap::new();
    for (name, value) in headers {
        let h_name: http::HeaderName = name
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header name '{name}': {e}"))?;
        let h_value: http::HeaderValue = value
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header value for '{name}': {e}"))?;
        result.insert(h_name, h_value);
    }
    Ok(result)
}

/// Validates a URL MCP server and returns its host with the addresses that
/// passed, for the HTTP clients to pin (see [`super::egress`]).
#[cfg(test)]
pub(crate) async fn validate_mcp_server_url(value: &str) -> anyhow::Result<ValidatedEndpoint> {
    validate_mcp_server_url_policy(value, false).await
}

#[cfg(test)]
pub(crate) async fn validate_mcp_server_url_policy(
    value: &str,
    allow_local: bool,
) -> anyhow::Result<ValidatedEndpoint> {
    validate_mcp_server_url_policy_cidrs(value, allow_local, &[]).await
}

/// As [`validate_mcp_server_url_policy`], additionally permitting the private
/// ranges listed in `allowed_cidrs` (the server's `allowed_cidrs` option).
pub(crate) async fn validate_mcp_server_url_policy_cidrs(
    value: &str,
    allow_local: bool,
    allowed_cidrs: &[String],
) -> anyhow::Result<ValidatedEndpoint> {
    validate_mcp_server_url_policy_with_cidrs(
        value,
        allow_local,
        allowed_cidrs,
        &super::egress::system_lookup(),
    )
    .await
}

#[cfg(test)]
pub(super) async fn validate_mcp_server_url_with(
    value: &str,
    lookup: &super::egress::Lookup,
) -> anyhow::Result<ValidatedEndpoint> {
    validate_mcp_server_url_policy_with(value, false, lookup).await
}

#[cfg(test)]
pub(super) async fn validate_mcp_server_url_policy_with(
    value: &str,
    allow_local: bool,
    lookup: &super::egress::Lookup,
) -> anyhow::Result<ValidatedEndpoint> {
    validate_mcp_server_url_policy_with_cidrs(value, allow_local, &[], lookup).await
}

pub(super) async fn validate_mcp_server_url_policy_with_cidrs(
    value: &str,
    allow_local: bool,
    allowed_cidrs: &[String],
    lookup: &super::egress::Lookup,
) -> anyhow::Result<ValidatedEndpoint> {
    let cidrs = parse_allowed_cidrs(allowed_cidrs)?;
    let (host, _port, literal_ip) = parse_mcp_server_url_policy(value, allow_local)?;
    let addresses = if let Some(address) = literal_ip {
        vec![address]
    } else {
        lookup(host.clone()).await.map_err(|error| {
            anyhow::anyhow!("could not resolve MCP server host '{host}': {error}")
        })?
    };

    let local =
        allow_local && !addresses.is_empty() && addresses.iter().all(|ip| is_loopback_ip(*ip));
    if !local {
        validate_resolved_addresses_in(&addresses, &cidrs)?;
    }
    let mut endpoint = ValidatedEndpoint::new(host, &addresses, literal_ip.is_some());
    if local {
        endpoint.allow_local_origin(reqwest::Url::parse(value)?);
    }
    Ok(endpoint)
}

pub(super) fn parse_mcp_server_url(value: &str) -> anyhow::Result<(String, u16, Option<IpAddr>)> {
    parse_mcp_server_url_policy(value, false)
}

fn parse_mcp_server_url_policy(
    value: &str,
    allow_local: bool,
) -> anyhow::Result<(String, u16, Option<IpAddr>)> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid MCP server URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("MCP server URL must use the http or https scheme");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("MCP server URL must not include a username or password");
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("MCP server URL must include a host"))?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_owned();
    if !allow_local
        && (host.eq_ignore_ascii_case("localhost")
            || host
                .to_ascii_lowercase()
                .strip_suffix(".localhost")
                .is_some())
    {
        anyhow::bail!("MCP server URL host is local; use a publicly routable HTTP(S) API endpoint");
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("MCP server URL must include a valid port"))?;
    let literal_ip = host.parse().ok();
    Ok((host, port, literal_ip))
}

pub(super) fn validate_resolved_addresses(addresses: &[IpAddr]) -> anyhow::Result<()> {
    validate_resolved_addresses_in(addresses, &[])
}

/// A parsed `allowed_cidrs` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AllowedCidr {
    network: IpAddr,
    bits: u8,
}

impl AllowedCidr {
    fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(address)) => prefix_matches(
                u128::from(u32::from(network)),
                u128::from(u32::from(address)),
                32,
                self.bits,
            ),
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                prefix_matches(u128::from(network), u128::from(address), 128, self.bits)
            }
            _ => false,
        }
    }
}

fn prefix_matches(network: u128, address: u128, width: u32, bits: u8) -> bool {
    let shift = width - u32::from(bits);
    shift >= 128 || network >> shift == address >> shift
}

/// Parses `allowed_cidrs`: `addr/bits` or a bare address (a single host).
pub(super) fn parse_allowed_cidrs(entries: &[String]) -> anyhow::Result<Vec<AllowedCidr>> {
    anyhow::ensure!(
        entries.len() <= 64,
        "MCP allowed_cidrs lists more than 64 ranges"
    );
    entries
        .iter()
        .map(|entry| {
            let (address, bits) = match entry.trim().split_once('/') {
                Some((address, bits)) => (address, Some(bits)),
                None => (entry.trim(), None),
            };
            let network: IpAddr = address.parse().map_err(|_| {
                anyhow::anyhow!(
                    "MCP allowed_cidrs entry '{entry}' is not an IP range such as 10.0.0.0/8"
                )
            })?;
            let max = if network.is_ipv4() { 32 } else { 128 };
            let bits = match bits {
                Some(bits) => bits.parse::<u8>().ok().filter(|bits| *bits <= max),
                None => Some(max),
            }
            .ok_or_else(|| {
                anyhow::anyhow!("MCP allowed_cidrs entry '{entry}' has an invalid prefix length")
            })?;
            Ok(AllowedCidr { network, bits })
        })
        .collect()
}

/// Cloud metadata endpoints that no `allowed_cidrs` entry can unlock.
fn is_metadata_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            matches!(
                ip.octets(),
                [169, 254, 169, 254]
                    | [169, 254, 170, 2]
                    | [169, 254, 169, 253]
                    | [100, 100, 100, 200]
                    | [192, 0, 0, 192]
                    | [168, 63, 129, 16]
            )
        }
        IpAddr::V6(ip) => {
            ip.to_ipv4_mapped()
                .is_some_and(|v4| is_metadata_ip(IpAddr::V4(v4)))
                || ["fd00:ec2::254", "fd20:ce::254"]
                    .iter()
                    .any(|metadata| ip == metadata.parse::<std::net::Ipv6Addr>().unwrap())
        }
    }
}

/// Private, LAN, VPN/CGNAT, ULA and link-local addresses, the only restricted
/// classes `allowed_cidrs` can unlock (never loopback, unspecified, multicast
/// or reserved space).
fn is_lan_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [first, second, ..] = ip.octets();
            first == 10
                || (first == 172 && (16..=31).contains(&second))
                || (first == 192 && second == 168)
                || (first == 100 && (64..=127).contains(&second))
                || (first == 169 && second == 254)
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_lan_ip(IpAddr::V4(v4));
            }
            let first = ip.segments()[0];
            first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

pub(super) fn validate_resolved_addresses_in(
    addresses: &[IpAddr],
    allowed: &[AllowedCidr],
) -> anyhow::Result<()> {
    if addresses.is_empty() {
        anyhow::bail!("MCP server host did not resolve to an IP address");
    }

    for address in addresses {
        if !is_restricted_ip(*address) {
            continue;
        }
        let unlocked = !is_metadata_ip(*address)
            && is_lan_ip(*address)
            && allowed.iter().any(|cidr| cidr.contains(*address));
        if !unlocked {
            anyhow::bail!(
                "MCP server URL resolves to non-public address {address}; use a publicly routable HTTP(S) API endpoint, or list its range in this server's `allowed_cidrs` (user config only) to permit a private network"
            );
        }
    }
    Ok(())
}

fn is_loopback_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
}

fn is_restricted_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_restricted_ipv4(address),
        IpAddr::V6(address) => {
            let octets = address.octets();
            let is_ipv4_compatible = octets[..12].iter().all(|octet| *octet == 0);
            let is_ipv4_mapped = octets[..10].iter().all(|octet| *octet == 0)
                && octets[10] == 0xff
                && octets[11] == 0xff;
            if is_ipv4_compatible || is_ipv4_mapped {
                return is_restricted_ipv4(Ipv4Addr::new(
                    octets[12], octets[13], octets[14], octets[15],
                ));
            }

            let segments = address.segments();
            let is_global_unicast = (0x2000..=0x3fff).contains(&segments[0]);
            let is_teredo = segments[0] == 0x2001 && segments[1] == 0;
            let is_benchmark = segments[0] == 0x2001 && segments[1] == 2 && segments[2] == 0;
            let is_orchid =
                segments[0] == 0x2001 && matches!(segments[1] & 0xfff0, 0x0010 | 0x0020);
            let is_documentation = (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0);
            let is_6to4 = segments[0] == 0x2002;

            !is_global_unicast
                || is_teredo
                || is_benchmark
                || is_orchid
                || is_documentation
                || is_6to4
        }
    }
}

fn is_restricted_ipv4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    // Azure WireServer: a public-range virtual IP only reachable from an Azure VM.
    address.octets() == [168, 63, 129, 16]
        || first == 0
        || first == 10
        || (first == 100 && (64..=127).contains(&second))
        || first == 127
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 0 && third == 0)
        || (first == 192 && second == 0 && third == 2)
        || (first == 192 && second == 88 && third == 99)
        || (first == 192 && second == 168)
        || (first == 198 && (18..=19).contains(&second))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        || first >= 224
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn stdout_lines_are_capped_but_long_streams_of_short_lines_are_not() {
        use tokio::io::AsyncReadExt as _;
        let reader = |bytes: Vec<u8>| super::LineCappedStdout {
            inner: std::io::Cursor::new(bytes),
            max_line: 16,
            current_line: 0,
            raw_line: Default::default(),
            forms: Default::default(),
            overflowed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let many_short_lines = "0123456789\n".repeat(1000).into_bytes();
        let mut collected = Vec::new();
        reader(many_short_lines.clone())
            .read_to_end(&mut collected)
            .await
            .unwrap();
        assert_eq!(collected, many_short_lines);

        let mut capped = reader(vec![b'x'; 64]);
        let error = capped.read_to_end(&mut Vec::new()).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(capped.overflowed.load(std::sync::atomic::Ordering::Acquire));
        // Once overflowed, the reader stays failed.
        assert!(capped.read_to_end(&mut Vec::new()).await.is_err());
        let mut completed = reader([vec![b'x'; 64], b"\nshort\n".to_vec()].concat());
        assert_eq!(
            completed
                .read_to_end(&mut Vec::new())
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData,
            "a newline in the same read must not hide an oversized completed line"
        );
    }

    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::{
        bounded_lossy_diagnostic, delegated_environment, http_client, parse_mcp_server_url,
        stdio_command, validate_mcp_server_url, validate_resolved_addresses,
    };

    #[tokio::test]
    async fn http_transport_does_not_follow_redirects() {
        use std::io::{Read, Write};
        let bind = || match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => Some(listener),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => None,
            Err(error) => panic!("loopback bind failed: {error}"),
        };
        let Some(target) = bind() else {
            return;
        };
        target.set_nonblocking(true).unwrap();
        let Some(redirect) = bind() else {
            return;
        };
        let redirect_url = format!("http://{}", redirect.local_addr().unwrap());
        let target_url = format!("http://{}", target.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\n\r\n"
            )
            .unwrap();
        });

        let response = http_client(Duration::from_secs(1))
            .unwrap()
            .get(redirect_url)
            .send()
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert!(
            matches!(target.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn stdio_command_resolves_path_lookup_before_spawn() {
        let command = stdio_command(
            "rustc",
            &[],
            std::path::Path::new("."),
            None,
            &HashMap::new(),
            &[],
            None,
            super::McpStdioNetwork::Inherit,
            std::env::var_os("PATH").as_deref(),
        )
        .unwrap();

        assert!(std::path::Path::new(command.as_std().get_program()).is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn stdio_command_ignores_relative_and_empty_path_entries() {
        use crate::extras::executable_search::PlantedExecutables;
        let fixture = PlantedExecutables::new("mcp");
        let resolve =
            |command: &str, cwd: Option<&std::path::Path>, search: Option<&std::ffi::OsStr>| {
                stdio_command(
                    command,
                    &[],
                    &fixture.workspace,
                    cwd,
                    &HashMap::new(),
                    &[],
                    None,
                    super::McpStdioNetwork::Inherit,
                    search,
                )
            };
        PlantedExecutables::plant(&fixture.workspace, "mcp-server");
        for search in fixture.hostile_search_paths() {
            let error = resolve("mcp-server", None, Some(search.as_os_str()))
                .unwrap_err()
                .to_string();
            assert!(error.contains("not found"), "{error}");
        }
        let trusted = PlantedExecutables::plant(&fixture.bin, "mcp-server");
        for search in fixture.hostile_search_paths() {
            let command = resolve("mcp-server", None, Some(search.as_os_str())).unwrap();
            assert_eq!(
                std::path::Path::new(command.as_std().get_program()),
                trusted
            );
        }

        // A relative command with a directory needs an explicit `cwd` anchor.
        let error = resolve("./mcp-server", None, None).unwrap_err().to_string();
        assert!(error.contains("directory component"), "{error}");
        let command = resolve("./mcp-server", Some(std::path::Path::new(".")), None).unwrap();
        assert_eq!(
            std::path::Path::new(command.as_std().get_program()),
            fixture.workspace.join("mcp-server")
        );
    }

    #[tokio::test]
    async fn stderr_capture_holds_no_buffer_once_the_connect_path_drops_it() {
        use tokio::io::AsyncWriteExt as _;
        let (mut writer, reader) = tokio::io::duplex(64);
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let task = super::capture_stderr(reader, std::sync::Arc::downgrade(&buffer));
        writer.write_all(b"first").await.unwrap();
        while buffer.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::sync::Arc::strong_count(&buffer),
            1,
            "the task owns no strong ref"
        );
        let weak = std::sync::Arc::downgrade(&buffer);
        assert_eq!(&*buffer.lock().unwrap(), b"first");
        drop(buffer);
        assert!(weak.upgrade().is_none(), "the buffer is freed at connect");
        // The pipe is still drained after the buffer is gone.
        writer.write_all(&[b'x'; 4096]).await.unwrap();
        drop(writer);
        task.await.unwrap();
    }

    #[test]
    fn lossy_stderr_rendering_remains_within_the_byte_cap() {
        let rendered = bounded_lossy_diagnostic(&vec![0xff; super::MCP_STDERR_LIMIT]);

        assert!(rendered.len() <= super::MCP_STDERR_LIMIT);
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
    }

    #[test]
    fn explicit_environment_overrides_inherited_value() {
        let name = "PATH";
        let explicit = HashMap::from([(name.to_string(), "explicit-path".to_string())]);
        let delegated = delegated_environment(&[name.to_string()], &explicit).unwrap();

        assert_eq!(delegated.len(), 1);
        assert_eq!(delegated[0].0, std::ffi::OsString::from(name));
        assert_eq!(delegated[0].1, std::ffi::OsString::from("explicit-path"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_environment_override_is_case_insensitive() {
        let explicit = HashMap::from([("Path".to_string(), "explicit-path".to_string())]);
        let delegated = delegated_environment(&["PATH".to_string()], &explicit).unwrap();

        assert_eq!(delegated.len(), 1);
        assert_eq!(delegated[0].0, std::ffi::OsString::from("Path"));
        assert_eq!(delegated[0].1, std::ffi::OsString::from("explicit-path"));
    }

    #[tokio::test]
    async fn rejects_local_and_private_mcp_server_urls() {
        for url in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "http://192.168.0.1",
            "http://10.0.0.1",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]",
        ] {
            let error = validate_mcp_server_url(url).await.unwrap_err();
            assert!(
                error.to_string().contains("publicly routable"),
                "unexpected error for {url}: {error}"
            );
        }
    }

    #[test]
    fn accepts_public_https_url_with_public_resolution() {
        let (host, port, literal_ip) = parse_mcp_server_url("https://api.example.com/mcp").unwrap();

        assert_eq!(host, "api.example.com");
        assert_eq!(port, 443);
        assert_eq!(literal_ip, None);
        assert!(
            validate_resolved_addresses(&[IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]).is_ok()
        );
    }

    #[test]
    fn rejects_non_http_scheme_and_embedded_credentials() {
        let scheme_error = parse_mcp_server_url("ftp://api.example.com").unwrap_err();
        assert!(scheme_error.to_string().contains("http or https"));

        let credentials_error =
            parse_mcp_server_url("https://user:password@api.example.com").unwrap_err();
        assert!(
            credentials_error
                .to_string()
                .contains("username or password")
        );
    }

    #[test]
    fn eager_servers_keep_the_small_tool_cap_and_deferrable_servers_a_larger_one() {
        use super::super::config::McpDefer;
        assert_eq!(super::tool_cap_for(McpDefer::Eager), 128);
        assert_eq!(super::tool_cap_for(McpDefer::Auto), 1024);
        assert_eq!(super::tool_cap_for(McpDefer::Deferred), 1024);
    }
}

#[cfg(test)]
mod local_policy_tests {
    use super::*;
    fn lookup(addresses: Vec<IpAddr>) -> super::super::egress::Lookup {
        std::sync::Arc::new(move |_| {
            let addresses = addresses.clone();
            Box::pin(async move { Ok(addresses) })
        })
    }
    // mini-agent-ygcvf: private ranges are opt-in per server, keep DNS pinning
    // and the metadata block, and the refusal names the option.
    #[tokio::test]
    async fn allowed_cidrs_unlock_only_listed_private_ranges_and_never_metadata() {
        let cidrs = |list: &[&str]| list.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
        let one = |ip: &str| lookup(vec![ip.parse().unwrap()]);
        // Default closed: no option, no private reach, and the notice says how.
        let error = validate_mcp_server_url_policy_with_cidrs(
            "http://wiki.corp/mcp",
            false,
            &[],
            &one("10.1.2.3"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("allowed_cidrs"), "{error}");
        // Listed ranges connect, pinned to the validated answers.
        for (ip, list) in [
            ("10.1.2.3", vec!["10.0.0.0/8"]),
            ("192.168.7.9", vec!["192.168.0.0/16", "10.0.0.0/8"]),
            ("172.20.0.5", vec!["172.16.0.0/12"]),
            ("100.101.102.103", vec!["100.64.0.0/10"]),
            ("fd12::5", vec!["fd00::/8"]),
            ("10.9.9.9", vec!["10.9.9.9"]),
        ] {
            let endpoint = validate_mcp_server_url_policy_with_cidrs(
                "https://internal.corp/mcp",
                false,
                &cidrs(&list),
                &one(ip),
            )
            .await
            .unwrap_or_else(|e| panic!("{ip}: {e}"));
            assert_eq!(endpoint.addresses(), vec![ip.parse::<IpAddr>().unwrap()]);
        }
        // Outside the listed range, or a mixed answer with one unlisted address.
        for (ips, list) in [
            (vec!["10.1.2.3"], vec!["192.168.0.0/16"]),
            (vec!["10.1.2.3", "192.168.1.1"], vec!["10.0.0.0/8"]),
            (vec!["10.1.2.3", "93.184.216.34"], vec![]),
        ] {
            let dns = lookup(ips.iter().map(|ip| ip.parse().unwrap()).collect());
            let result = validate_mcp_server_url_policy_with_cidrs(
                "https://internal.corp/mcp",
                false,
                &cidrs(&list),
                &dns,
            )
            .await;
            assert!(result.is_err(), "{ips:?} {list:?}");
        }
        // Metadata, loopback, unspecified, multicast and reserved space stay
        // refused even under the widest ranges.
        for ip in [
            "169.254.169.254",
            "169.254.170.2",
            "168.63.129.16",
            "::ffff:168.63.129.16",
            "100.100.100.200",
            "fd00:ec2::254",
            "fd20:ce::254",
            "FD20:00CE:0:0:0:0:0:254",
            "::ffff:169.254.169.254",
            "127.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
        ] {
            let result = validate_mcp_server_url_policy_with_cidrs(
                "https://internal.corp/mcp",
                false,
                &cidrs(&["0.0.0.0/0", "::/0"]),
                &one(ip),
            )
            .await;
            assert!(result.is_err(), "{ip} must stay blocked");
        }
        // Malformed entries are rejected, not ignored.
        for bad in ["10.0.0.0/33", "nope", "10.0.0.0/x", "::/129"] {
            assert!(
                validate_mcp_server_url_policy_with_cidrs(
                    "https://internal.corp/mcp",
                    false,
                    &cidrs(&[bad]),
                    &one("93.184.216.34"),
                )
                .await
                .is_err(),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn explicit_local_policy_accepts_only_pinned_loopback_and_does_not_widen_private_networks()
     {
        let local = lookup(vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()]);
        for url in [
            "http://localhost:3000/mcp",
            "https://dev.example:3000/mcp",
            "http://127.0.0.2/mcp",
            "http://[::1]/mcp",
            "http://[::ffff:127.0.0.1]/mcp",
        ] {
            let endpoint = validate_mcp_server_url_policy_with(url, true, &local)
                .await
                .unwrap();
            assert!(!endpoint.addresses().is_empty());
            assert!(
                validate_mcp_server_url_policy_with(url, false, &local)
                    .await
                    .is_err()
            );
        }
        for ip in [
            "10.0.0.1",
            "192.168.1.2",
            "169.254.169.254",
            "168.63.129.16",
            "0.0.0.0",
            "::",
            "fc00::1",
            "::ffff:192.168.1.2",
        ] {
            let dns = lookup(vec![ip.parse().unwrap()]);
            assert!(
                validate_mcp_server_url_policy_with("http://alias.example/mcp", true, &dns)
                    .await
                    .is_err()
            );
        }
        for addresses in [
            vec!["127.0.0.1", "93.184.216.34"],
            vec!["127.0.0.1", "10.0.0.1"],
        ] {
            let dns = lookup(
                addresses
                    .into_iter()
                    .map(|ip| ip.parse().unwrap())
                    .collect(),
            );
            assert!(
                validate_mcp_server_url_policy_with("http://alias.example/mcp", true, &dns)
                    .await
                    .is_err()
            );
        }
        assert!(
            validate_mcp_server_url_policy_with("http://user:password@localhost/mcp", true, &local)
                .await
                .is_err()
        );
        assert!(
            validate_mcp_server_url_policy_with("http://empty.example", true, &lookup(vec![]))
                .await
                .is_err()
        );
        let public = lookup(vec!["93.184.216.34".parse().unwrap()]);
        assert!(
            validate_mcp_server_url_policy_with("https://public.example", false, &public)
                .await
                .is_ok()
        );
        assert!(
            validate_mcp_server_url_policy_with("https://public.example", true, &public)
                .await
                .is_ok()
        );
    }
}
