//! Automatic reconnection of a remote (URL) MCP server whose transport closed
//! mid-session (server restart, network drop, expired session).
//!
//! One [`ServerLink`] belongs to each URL server's [`McpClientHandle`] and is
//! shared by that server's tools. Before a tool call sends its request the
//! tool asks the link for the live connection. When the current transport is
//! closed the link re-establishes it from the *same captured definition*
//! (never a re-read configuration), up to [`MAX_RECONNECT_ATTEMPTS`] times with
//! a doubling backoff starting at [`RECONNECT_BASE_DELAY`]. While it runs the
//! server shows as `pending` (`/mcp`, `/doctor`); after the last failed attempt
//! it is marked `failed` until the user reconnects it by hand. Stdio servers
//! have no link and stay manual.
//!
//! A call that was already in flight when the connection dropped is never
//! replayed: it fails, and only the next call reconnects, so a non-idempotent
//! tool is not executed twice. The link never outlives the handle it belongs
//! to: dropping the handle retires the link and closes any replacement.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use compact_str::CompactString;
use futures::future::BoxFuture;
use rmcp::service::{Peer, RoleClient};

use super::client::McpClientHandle;
use super::health::{AttemptKind, AttemptOutcome, Health};

/// Reconnect attempts after a dropped transport before the server is failed.
pub(crate) const MAX_RECONNECT_ATTEMPTS: usize = 5;
/// Delay before the first attempt; each later attempt doubles it.
pub(crate) const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);

/// The live pieces of one connection generation.
#[derive(Clone)]
pub(crate) struct Conn {
    pub(crate) peer: Peer<RoleClient>,
    pub(crate) elicitation: super::elicitation::Handler,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkState {
    Connected,
    Reconnecting,
    Failed,
}

pub(crate) type Connector =
    Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<McpClientHandle>> + Send + Sync>;

pub(crate) struct ServerLink {
    name: CompactString,
    current: Mutex<Conn>,
    state: Mutex<LinkState>,
    /// Keeps the replacement connection alive (dropping it cancels it).
    replacement: Mutex<Option<McpClientHandle>>,
    retired: AtomicBool,
    serialise: tokio::sync::Mutex<()>,
    connector: Connector,
    base_delay: Duration,
    health: Mutex<Option<Health>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

impl ServerLink {
    pub(crate) fn new(
        name: CompactString,
        initial: Conn,
        connector: Connector,
        base_delay: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            name,
            current: Mutex::new(initial),
            state: Mutex::new(LinkState::Connected),
            replacement: Mutex::new(None),
            retired: AtomicBool::new(false),
            serialise: tokio::sync::Mutex::new(()),
            connector,
            base_delay,
            health: Mutex::new(None),
        })
    }

    pub(crate) fn bind_health(&self, health: Health) {
        *lock(&self.health) = Some(health);
    }

    pub(crate) fn state(&self) -> LinkState {
        *lock(&self.state)
    }

    /// Bounded, credential-free reason shown for a failed server.
    pub(crate) fn failure_message(&self) -> String {
        format!(
            "connection dropped; {MAX_RECONNECT_ATTEMPTS} reconnect attempts failed (reconnect from /mcp)"
        )
    }

    pub(crate) fn conn(&self) -> Conn {
        lock(&self.current).clone()
    }

    /// Dropping the owning handle ends the link and its replacement.
    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        drop(lock(&self.replacement).take());
    }

    /// The live connection, reconnecting first when the transport closed.
    pub(crate) async fn ensure_connected(&self) -> Result<Conn, String> {
        let _serial = self.serialise.lock().await;
        let retired = || "MCP connection retired; reconnect before calling tools".to_owned();
        if self.retired.load(Ordering::Acquire) {
            return Err(retired());
        }
        let conn = self.conn();
        if !conn.peer.is_transport_closed() {
            return Ok(conn);
        }
        if self.state() == LinkState::Failed {
            return Err(format!(
                "MCP server '{}' {}",
                self.name,
                self.failure_message()
            ));
        }
        *lock(&self.state) = LinkState::Reconnecting;
        let attempt = lock(&self.health)
            .clone()
            .map(|health| health.begin(&self.name, AttemptKind::Connection));
        let mut delay = self.base_delay;
        for number in 1..=MAX_RECONNECT_ATTEMPTS {
            let waited = tokio::select! {
                biased;
                _ = crate::agent::runner::current_work_scope_cancelled() => false,
                _ = tokio::time::sleep(delay) => true,
            };
            delay = delay.saturating_mul(2);
            if !waited || self.retired.load(Ordering::Acquire) {
                *lock(&self.state) = LinkState::Connected;
                if let Some(attempt) = attempt {
                    attempt.finish(AttemptOutcome::Cancelled);
                }
                return Err(retired_or_cancelled(self.retired.load(Ordering::Acquire)));
            }
            tracing::debug!(
                "MCP reconnecting dropped server '{}' (attempt {number}/{MAX_RECONNECT_ATTEMPTS})",
                self.name
            );
            let connected = tokio::select! {
                biased;
                _ = crate::agent::runner::current_work_scope_cancelled() => None,
                result = (self.connector)() => Some(result),
            };
            match connected {
                None => {
                    *lock(&self.state) = LinkState::Connected;
                    if let Some(attempt) = attempt {
                        attempt.finish(AttemptOutcome::Cancelled);
                    }
                    return Err("MCP reconnect cancelled".to_owned());
                }
                Some(Ok(handle)) => {
                    if self.retired.load(Ordering::Acquire) {
                        return Err(retired());
                    }
                    let fresh = Conn {
                        peer: handle.peer(),
                        elicitation: handle.elicitation.clone(),
                    };
                    let previous = lock(&self.replacement).replace(handle);
                    drop(previous);
                    *lock(&self.current) = fresh.clone();
                    *lock(&self.state) = LinkState::Connected;
                    if let Some(attempt) = attempt {
                        attempt.finish(AttemptOutcome::Succeeded);
                    }
                    return Ok(fresh);
                }
                Some(Err(_)) => {}
            }
        }
        *lock(&self.state) = LinkState::Failed;
        if let Some(attempt) = attempt {
            attempt.finish(AttemptOutcome::Failed);
        }
        Err(format!(
            "MCP server '{}' {}",
            self.name,
            self.failure_message()
        ))
    }
}

fn retired_or_cancelled(retired: bool) -> String {
    if retired {
        "MCP connection retired; reconnect before calling tools".to_owned()
    } else {
        "MCP reconnect cancelled".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    async fn read_request(
        stream: &mut tokio::net::TcpStream,
    ) -> Option<(String, serde_json::Value)> {
        let mut buffer = Vec::new();
        let header_end = loop {
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let method = head.split(' ').next()?.to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buffer.len() < header_end + length {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = serde_json::from_slice(&buffer[header_end..header_end + length])
            .unwrap_or(serde_json::Value::Null);
        Some((method, body))
    }

    /// A tiny stateless streamable-HTTP MCP server. `fail_initialize` answers
    /// that many `initialize` POSTs with `fail_status` first. Returns the URL
    /// and a counter of `initialize` requests received.
    async fn serve(
        fail_initialize: usize,
        fail_status: &'static str,
    ) -> Option<(String, Arc<AtomicUsize>)> {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("loopback bind failed: {error}"),
        };
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let initializes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&initializes);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let Some((method, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    let response = match (method.as_str(), body.get("id")) {
                        ("POST", None) => http_response("202 Accepted", "text/plain", b""),
                        ("POST", Some(id)) => match body["method"].as_str() {
                            Some("initialize") => {
                                if counter.fetch_add(1, Ordering::SeqCst) < fail_initialize {
                                    http_response(fail_status, "text/plain", b"nope")
                                } else {
                                    http_response(
                                        "200 OK",
                                        "application/json",
                                        &serde_json::to_vec(&serde_json::json!({
                                            "jsonrpc": "2.0", "id": id,
                                            "result": {
                                                "protocolVersion": body["params"]["protocolVersion"],
                                                "capabilities": {"tools": {}},
                                                "serverInfo": {"name": "t", "version": "0"}
                                            }
                                        }))
                                        .unwrap(),
                                    )
                                }
                            }
                            _ => http_response("404 Not Found", "text/plain", b""),
                        },
                        _ => http_response("405 Method Not Allowed", "text/plain", b""),
                    };
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Some((url, initializes))
    }

    async fn connect(url: &str) -> anyhow::Result<McpClientHandle> {
        McpClientHandle::connect_http_with_timeout(
            "t".into(),
            url,
            &Default::default(),
            None,
            None,
            Duration::from_secs(5),
        )
        .await
    }

    fn conn_of(handle: &McpClientHandle) -> Conn {
        Conn {
            peer: handle.peer(),
            elicitation: handle.elicitation.clone(),
        }
    }

    #[tokio::test]
    async fn dropped_transport_reconnects_and_the_next_call_uses_the_new_peer() {
        let Some((url, initializes)) = serve(0, "200 OK").await else {
            return;
        };
        let original = connect(&url).await.unwrap();
        let connector_url = url.clone();
        let connector: Connector = Arc::new(move || {
            let url = connector_url.clone();
            async move { connect(&url).await }.boxed()
        });
        let link = ServerLink::new(
            "t".into(),
            conn_of(&original),
            connector,
            Duration::from_millis(5),
        );
        let health = Health::default();
        link.bind_health(health.clone());
        // Healthy: no reconnect, same peer.
        link.ensure_connected().await.unwrap();
        assert_eq!(initializes.load(Ordering::SeqCst), 1);
        // The transport drops.
        original.running_service.cancellation_token().cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !original.peer().is_transport_closed() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let fresh = link.ensure_connected().await.unwrap();
        assert!(!fresh.peer.is_transport_closed());
        assert_eq!(initializes.load(Ordering::SeqCst), 2);
        assert_eq!(link.state(), LinkState::Connected);
        assert_eq!(
            health.snapshot().servers["t"].connection.unwrap().outcome,
            AttemptOutcome::Succeeded
        );
        // A second call reuses the replacement without reconnecting again.
        link.ensure_connected().await.unwrap();
        assert_eq!(initializes.load(Ordering::SeqCst), 2);
        // Retiring (the owning handle dropped) refuses and closes it.
        link.retire();
        assert!(link.ensure_connected().await.is_err());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fresh.peer.is_transport_closed() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn five_failed_attempts_mark_the_server_failed_and_stop_retrying() {
        let Some((url, _)) = serve(0, "200 OK").await else {
            return;
        };
        let original = connect(&url).await.unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let connector: Connector = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err(anyhow::anyhow!("down")) }.boxed()
        });
        let link = ServerLink::new(
            "t".into(),
            conn_of(&original),
            connector,
            Duration::from_millis(1),
        );
        let health = Health::default();
        link.bind_health(health.clone());
        original.running_service.cancellation_token().cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !original.peer().is_transport_closed() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let error = link.ensure_connected().await.err().unwrap();
        assert!(error.contains("5 reconnect attempts failed"), "{error}");
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_RECONNECT_ATTEMPTS);
        assert_eq!(link.state(), LinkState::Failed);
        assert_eq!(
            health.snapshot().servers["t"].connection.unwrap().outcome,
            AttemptOutcome::Failed
        );
        // Failed is sticky: no further attempts until a manual reconnect.
        assert!(link.ensure_connected().await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_RECONNECT_ATTEMPTS);
    }

    #[tokio::test]
    async fn backoff_doubles_from_the_base_delay() {
        let Some((url, _)) = serve(0, "200 OK").await else {
            return;
        };
        let original = connect(&url).await.unwrap();
        let connector: Connector = Arc::new(|| async { Err(anyhow::anyhow!("down")) }.boxed());
        let link = ServerLink::new(
            "t".into(),
            conn_of(&original),
            connector,
            Duration::from_millis(20),
        );
        original.running_service.cancellation_token().cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let _ = link.ensure_connected().await;
        // 20 + 40 + 80 + 160 + 320 ms of sleeping at least.
        assert!(started.elapsed() >= Duration::from_millis(620));
    }

    #[tokio::test]
    async fn first_connect_retries_transient_5xx_but_not_4xx() {
        let Some((url, initializes)) = serve(2, "503 Service Unavailable").await else {
            return;
        };
        let handle =
            super::super::client::connect_http_retrying_for_test(&url, Duration::from_millis(5))
                .await
                .unwrap();
        assert!(!handle.peer().is_transport_closed());
        assert_eq!(initializes.load(Ordering::SeqCst), 3);

        // 4xx (and auth) responses are not retried.
        let Some((url, initializes)) = serve(100, "401 Unauthorized").await else {
            return;
        };
        let result =
            super::super::client::connect_http_retrying_for_test(&url, Duration::from_millis(5))
                .await;
        assert!(result.is_err());
        assert_eq!(initializes.load(Ordering::SeqCst), 1);

        // A persistent 5xx stops after the initial attempt plus 3 retries.
        let Some((url, initializes)) = serve(100, "500 Internal Server Error").await else {
            return;
        };
        let result =
            super::super::client::connect_http_retrying_for_test(&url, Duration::from_millis(5))
                .await;
        assert!(result.is_err());
        assert_eq!(initializes.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn first_connect_total_time_is_bounded_by_the_startup_budget() {
        use super::super::client::retry_transient_connect;
        // A hanging attempt is cut off at the budget and never retried.
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let started = std::time::Instant::now();
        let result: anyhow::Result<()> = retry_transient_connect(
            || {
                counter.fetch_add(1, Ordering::SeqCst);
                async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(())
                }
            },
            3,
            Duration::from_millis(5),
            Duration::from_millis(100),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_secs(2));

        // A server-requested wait longer than the remaining budget stops retries.
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let result: anyhow::Result<()> = retry_transient_connect(
            || {
                counter.fetch_add(1, Ordering::SeqCst);
                async {
                    Err(anyhow::anyhow!(
                        "HTTP 429 Too Many Requests [retry-after: 30]: slow down"
                    ))
                }
            },
            3,
            Duration::from_millis(5),
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn first_connect_retries_429_and_408_and_honours_retry_after() {
        use super::super::client::retry_transient_connect;
        for status in ["HTTP 429 Too Many Requests", "HTTP 408 Request Timeout"] {
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&attempts);
            let result: anyhow::Result<u8> = retry_transient_connect(
                || {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n == 0 {
                            Err(anyhow::anyhow!("{status} [retry-after: 0]: later"))
                        } else {
                            Ok(7)
                        }
                    }
                },
                3,
                Duration::from_millis(5),
                Duration::from_secs(5),
            )
            .await;
            assert_eq!(result.unwrap(), 7, "{status}");
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
        }
    }
}
