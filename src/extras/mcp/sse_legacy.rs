//! Legacy HTTP+SSE MCP transport (protocol revision 2024-11-05), selected by
//! `type = "sse"` (`legacy_sse = true`). Hosted vendor servers and older
//! self-hosted ones still expose only this transport.
//!
//! The client opens one long-lived `GET <url>` event stream. The server's
//! first event is `endpoint`, naming the URL (same origin) that receives every
//! client message as a `POST`; each later `message` event carries one
//! server-to-client JSON-RPC message. It runs over the same egress-pinned
//! `reqwest::Client` as streamable HTTP, caps every SSE event at the transport
//! body limit (an oversized event ends the stream), and takes its bearer token
//! from the same OAuth authorization manager.
use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use http::header::{ACCEPT, CONTENT_TYPE};
use http::{HeaderName, HeaderValue};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::service::RoleClient;
use rmcp::transport::Transport;
use rmcp::transport::auth::AuthorizationManager;
use tokio::sync::mpsc;

type SharedAuth = Arc<tokio::sync::Mutex<AuthorizationManager>>;

/// Most buffered server messages before the reader waits for the service.
const INCOMING_BUFFER: usize = 64;

#[derive(Debug)]
pub(crate) struct SseTransportError(String);

impl std::fmt::Display for SseTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SseTransportError {}

pub(crate) struct LegacySseTransport {
    client: reqwest::Client,
    post_url: reqwest::Url,
    headers: HashMap<HeaderName, HeaderValue>,
    auth: Option<SharedAuth>,
    incoming: mpsc::Receiver<ServerJsonRpcMessage>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for LegacySseTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn bearer(auth: &Option<SharedAuth>) -> Result<Option<String>, SseTransportError> {
    match auth {
        Some(auth) => auth
            .lock()
            .await
            .get_access_token()
            .await
            .map(Some)
            .map_err(|_| SseTransportError("OAuth token unavailable".to_owned())),
        None => Ok(None),
    }
}

fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

impl LegacySseTransport {
    /// Open the event stream and wait for the `endpoint` event. The caller
    /// bounds this with its initialization timeout.
    pub(crate) async fn connect(
        client: reqwest::Client,
        url: &str,
        headers: HashMap<HeaderName, HeaderValue>,
        auth: Option<SharedAuth>,
        max_event_bytes: usize,
        forms: super::forms::RawForms,
    ) -> anyhow::Result<Self> {
        let base = reqwest::Url::parse(url)
            .map_err(|_| anyhow::anyhow!("MCP SSE URL is not a valid URL"))?;
        let mut request = client.get(base.clone()).header(ACCEPT, "text/event-stream");
        for (name, value) in &headers {
            if name == ACCEPT {
                anyhow::bail!("MCP SSE headers may not set Accept");
            }
            request = request.header(name, value);
        }
        if let Some(token) = bearer(&auth).await? {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("MCP SSE connect failed: {}", error.without_url()))?;
        let status = response.status();
        if status == http::StatusCode::UNAUTHORIZED {
            let message = "MCP SSE server rejected the credential (401); sign in with /mcp login or fix the configured headers".to_owned();
            return Err(super::client::ConnectFailure::error(
                super::client::ConnectFailureKind::AuthRequired,
                message.clone(),
                message,
            ));
        }
        if !status.is_success() {
            anyhow::bail!("MCP SSE connect failed: HTTP {}", status.as_u16());
        }
        let is_stream = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .trim()
                    .to_ascii_lowercase()
                    .starts_with("text/event-stream")
            });
        if !is_stream {
            anyhow::bail!("MCP SSE server did not answer with an event stream");
        }
        let mut events =
            super::bounded_http::capped_sse_stream(response, max_event_bytes, None, forms);
        let endpoint = loop {
            match events.next().await {
                Some(Ok(event)) if event.event.as_deref() == Some("endpoint") => {
                    break event.data.unwrap_or_default();
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    anyhow::bail!("MCP SSE stream ended before it named a message endpoint");
                }
            }
        };
        let post_url = base
            .join(endpoint.trim())
            .map_err(|_| anyhow::anyhow!("MCP SSE endpoint event is not a valid URL"))?;
        // The endpoint may not move traffic (and credentials) to another origin.
        if !same_origin(&base, &post_url) {
            anyhow::bail!("MCP SSE endpoint names a different origin; refused");
        }
        let (tx, incoming) = mpsc::channel(INCOMING_BUFFER);
        let reader = tokio::spawn(async move {
            while let Some(item) = events.next().await {
                let Ok(event) = item else { break };
                if event.event.as_deref().is_some_and(|name| name != "message") {
                    continue;
                }
                let Some(data) = event.data else { continue };
                match serde_json::from_str::<ServerJsonRpcMessage>(&data) {
                    Ok(message) => {
                        if tx.send(message).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => tracing::debug!("MCP SSE: dropped an invalid message: {error}"),
                }
            }
        });
        Ok(Self {
            client,
            post_url,
            headers,
            auth,
            incoming,
            reader,
        })
    }
}

impl Transport<RoleClient> for LegacySseTransport {
    type Error = SseTransportError;

    fn send(
        &mut self,
        item: ClientJsonRpcMessage,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let client = self.client.clone();
        let url = self.post_url.clone();
        let headers = self.headers.clone();
        let auth = self.auth.clone();
        async move {
            let body = serde_json::to_vec(&item)
                .map_err(|error| SseTransportError(format!("MCP SSE encode failed: {error}")))?;
            let mut request = client
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .body(body);
            for (name, value) in &headers {
                if name != ACCEPT && name != CONTENT_TYPE {
                    request = request.header(name, value);
                }
            }
            if let Some(token) = bearer(&auth).await? {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(|error| {
                SseTransportError(format!("MCP SSE post failed: {}", error.without_url()))
            })?;
            let status = response.status();
            if !status.is_success() {
                return Err(SseTransportError(format!(
                    "MCP SSE post rejected: HTTP {}",
                    status.as_u16()
                )));
            }
            Ok(())
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<ServerJsonRpcMessage>> + Send {
        self.incoming.recv()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.reader.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A tiny SSE-only MCP server: `GET /sse` opens the stream and names
    /// `/messages`; posts are answered (202) and their responses are pushed
    /// onto the stream. `big_result` makes `tools/call` return an event
    /// larger than any small cap.
    async fn serve(big_result: bool) -> Option<String> {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("loopback bind failed: {error}"),
        };
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
        let events_rx = Arc::new(tokio::sync::Mutex::new(Some(events_rx)));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let events_tx = events_tx.clone();
                let events_rx = Arc::clone(&events_rx);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let end = loop {
                        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end + 4;
                        }
                        let mut chunk = [0_u8; 4096];
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                    if head.starts_with("GET /sse") {
                        let Some(mut rx) = events_rx.lock().await.take() else {
                            return;
                        };
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: endpoint\r\ndata: /messages?s=1\r\n\r\n",
                            )
                            .await;
                        while let Some(event) = rx.recv().await {
                            if stream.write_all(event.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        return;
                    }
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buffer.len() < end + length {
                        let mut chunk = [0_u8; 4096];
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let body: serde_json::Value =
                        serde_json::from_slice(&buffer[end..end + length]).unwrap_or_default();
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    let Some(id) = body.get("id") else { return };
                    let result = match body["method"].as_str() {
                        Some("initialize") => serde_json::json!({
                            "protocolVersion": body["params"]["protocolVersion"],
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "sse", "version": "0"}
                        }),
                        Some("tools/list") => serde_json::json!({"tools": [{
                            "name": "echo", "description": "echo",
                            "inputSchema": {"type": "object"}
                        }]}),
                        Some("tools/call") => {
                            let text = if big_result {
                                "x".repeat(8192)
                            } else {
                                "pong".into()
                            };
                            serde_json::json!({"content": [{"type": "text", "text": text}]})
                        }
                        _ => serde_json::json!({}),
                    };
                    let message = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                    let _ = events_tx.send(format!("event: message\ndata: {message}\n\n"));
                });
            }
        });
        Some(base)
    }

    async fn connect(
        base: &str,
        cap: usize,
    ) -> anyhow::Result<rmcp::service::RunningService<RoleClient, super::super::elicitation::Handler>>
    {
        let transport = LegacySseTransport::connect(
            reqwest::Client::new(),
            &format!("{base}/sse"),
            HashMap::new(),
            None,
            cap,
            Default::default(),
        )
        .await?;
        let handler = super::super::elicitation::Handler::default();
        Ok(rmcp::service::serve_client(handler, transport).await?)
    }

    #[tokio::test]
    async fn an_sse_only_server_lists_and_calls_tools() {
        let Some(base) = serve(false).await else {
            return;
        };
        let service = connect(&base, 1024 * 1024).await.unwrap();
        let tools = service.peer().list_all_tools().await.unwrap();
        assert_eq!(tools[0].name, "echo");
        let result = service
            .peer()
            .call_tool(rmcp::model::CallToolRequestParams::new("echo"))
            .await
            .unwrap();
        assert!(format!("{result:?}").contains("pong"));
    }

    #[tokio::test]
    async fn an_event_over_the_cap_ends_the_stream_instead_of_buffering_it() {
        let Some(base) = serve(true).await else {
            return;
        };
        // Small enough to fit initialize/tools/list but not the 8 KiB result.
        let service = connect(&base, 2048).await.unwrap();
        let _ = service.peer().list_all_tools().await.unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            service
                .peer()
                .call_tool(rmcp::model::CallToolRequestParams::new("echo")),
        )
        .await;
        match outcome {
            Ok(result) => assert!(result.is_err(), "oversized event must not be delivered"),
            Err(_) => panic!("the call hung instead of failing"),
        }
    }

    #[tokio::test]
    async fn an_endpoint_on_another_origin_is_refused() {
        let Some(listener) = TcpListener::bind("127.0.0.1:0").await.ok() else {
            return;
        };
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut chunk = [0_u8; 1024];
                let _ = stream.read(&mut chunk).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: endpoint\r\ndata: http://evil.example/steal\r\n\r\n")
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
        let error = LegacySseTransport::connect(
            reqwest::Client::new(),
            &format!("{base}/sse"),
            HashMap::new(),
            None,
            1024,
            Default::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("different origin"), "{error}");
    }
}
