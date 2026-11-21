//! Opt-in remote view of a running session: a small HTTP server that serves
//! one static page and streams the session's events to an authenticated
//! browser, which can steer the turn, answer permission asks and cancel.
//!
//! Security model: the listener binds loopback unless the caller explicitly
//! allows a wider address (there is no TLS here, so a wider bind belongs
//! behind an SSH tunnel or a TLS-terminating proxy); every request needs the
//! random per-run token (`Authorization: Bearer` or `?token=`), compared in
//! constant time; the `Host` header must name the bound address (DNS
//! rebinding); bodies, headers, connections and idle time are bounded. The
//! server never executes anything itself: steering text and permission
//! answers are handed to the session through [`Hub`] channels and take the
//! same steering queue and permission prompt as local input.

pub mod bridge;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, broadcast, mpsc};

const PAGE: &str = include_str!("page.html");
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_BACKLOG: usize = 256;

/// What a browser asked for, handed to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCommand {
    /// Text for the steering queue of the running turn.
    Steer(String),
    /// Answer to a permission ask previously published by id.
    Permission {
        id: String,
        allow: bool,
    },
    Cancel,
}

/// Session side of the server: publishes events, receives browser commands.
#[derive(Clone)]
pub struct Hub {
    events: broadcast::Sender<String>,
    pending: Arc<Mutex<HashMap<String, ()>>>,
    commands: mpsc::Sender<RemoteCommand>,
}

impl Hub {
    /// Publishes one JSON event (`{"type":...}`) to every connected browser.
    pub fn publish(&self, event: &serde_json::Value) {
        let _ = self.events.send(event.to_string());
    }

    /// Publishes a permission ask the browser may answer once.
    pub fn publish_permission(&self, id: &str, summary: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), ());
        self.publish(&serde_json::json!({"type": "permission", "id": id, "summary": summary}));
    }

    /// Withdraws an ask answered locally, so a late remote answer is ignored.
    pub fn withdraw_permission(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }
}

#[derive(Debug)]
pub struct RemoteOptions {
    pub host: IpAddr,
    pub port: u16,
    /// Permit a non-loopback bind. Without TLS this is the caller's explicit risk.
    pub allow_non_loopback: bool,
}

impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            allow_non_loopback: false,
        }
    }
}

pub struct RemoteServer {
    pub addr: SocketAddr,
    pub token: String,
    pub hub: Hub,
    pub commands: mpsc::Receiver<RemoteCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl RemoteServer {
    /// URL carrying the token, to open on the other device.
    pub fn url(&self) -> String {
        format!("http://{}/?token={}", self.addr, self.token)
    }
}

impl Drop for RemoteServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Starts the server. Fails for a non-loopback host unless allowed.
pub async fn start(options: RemoteOptions) -> anyhow::Result<RemoteServer> {
    if !options.host.is_loopback() && !options.allow_non_loopback {
        anyhow::bail!(
            "remote control binds loopback only; {} needs the explicit non-loopback opt-in (and TLS in front)",
            options.host
        );
    }
    let listener = TcpListener::bind(SocketAddr::new(options.host, options.port)).await?;
    let addr = listener.local_addr()?;
    let token = random_token();
    let (events, _) = broadcast::channel(EVENT_BACKLOG);
    let (commands_tx, commands) = mpsc::channel(64);
    let hub = Hub {
        events,
        pending: Arc::new(Mutex::new(HashMap::new())),
        commands: commands_tx,
    };
    let shared = Arc::new(Shared {
        token: token.clone(),
        addr,
        hub: hub.clone(),
    });
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                let _ = handle(stream, &shared).await;
                drop(permit);
            });
        }
    });
    Ok(RemoteServer {
        addr,
        token,
        hub,
        commands,
        task,
    })
}

struct Shared {
    token: String,
    addr: SocketAddr,
    hub: Hub,
}

struct Request {
    method: String,
    path: String,
    query_token: Option<String>,
    bearer: Option<String>,
    host: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> anyhow::Result<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            anyhow::bail!("headers too large");
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("closed");
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query_token = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(str::to_string);
    let (mut bearer, mut host, mut length) = (None, None, 0usize);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "authorization" => bearer = value.strip_prefix("Bearer ").map(str::to_string),
            "host" => host = Some(value.to_string()),
            "content-length" => length = value.parse().unwrap_or(usize::MAX),
            _ => {}
        }
    }
    if length > MAX_BODY_BYTES {
        anyhow::bail!("body too large");
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("closed");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok(Request {
        method,
        path: path.to_string(),
        query_token,
        bearer,
        host,
        body,
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

fn host_is_bound_address(host: &str, addr: SocketAddr) -> bool {
    let host = host.to_ascii_lowercase();
    let port = addr.port();
    host == addr.to_string()
        || (addr.ip().is_loopback()
            && [
                format!("localhost:{port}"),
                format!("127.0.0.1:{port}"),
                format!("[::1]:{port}"),
            ]
            .contains(&host))
}

async fn handle(mut stream: TcpStream, shared: &Shared) -> anyhow::Result<()> {
    let request = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        _ => {
            respond(&mut stream, "400 Bad Request", "text/plain", "bad request").await?;
            return Ok(());
        }
    };
    if !request
        .host
        .as_deref()
        .is_some_and(|host| host_is_bound_address(host, shared.addr))
    {
        respond(&mut stream, "403 Forbidden", "text/plain", "bad host").await?;
        return Ok(());
    }
    let supplied = request
        .bearer
        .as_deref()
        .or(request.query_token.as_deref())
        .unwrap_or_default();
    if !constant_time_eq(supplied.as_bytes(), shared.token.as_bytes()) {
        respond(
            &mut stream,
            "401 Unauthorized",
            "text/plain",
            "unauthorized",
        )
        .await?;
        return Ok(());
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE).await?,
        ("GET", "/events") => stream_events(&mut stream, shared).await?,
        ("POST", "/steer") => {
            let text = json_field(&request.body, "text")
                .filter(|t| !t.trim().is_empty() && t.len() <= MAX_BODY_BYTES);
            match text {
                Some(text) => {
                    command(&mut stream, shared, RemoteCommand::Steer(text)).await?;
                }
                None => {
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain",
                        "text required",
                    )
                    .await?
                }
            }
        }
        ("POST", "/permission") => {
            let value: serde_json::Value =
                serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null);
            let id = value.get("id").and_then(|v| v.as_str()).map(str::to_string);
            let allow = value.get("allow").and_then(|v| v.as_bool());
            let (Some(id), Some(allow)) = (id, allow) else {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    "id and allow required",
                )
                .await?;
                return Ok(());
            };
            // An answer is accepted once, for an ask that is still pending.
            let known = shared
                .hub
                .pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id)
                .is_some();
            if known {
                command(&mut stream, shared, RemoteCommand::Permission { id, allow }).await?;
            } else {
                respond(
                    &mut stream,
                    "404 Not Found",
                    "text/plain",
                    "no such pending ask",
                )
                .await?;
            }
        }
        ("POST", "/cancel") => command(&mut stream, shared, RemoteCommand::Cancel).await?,
        _ => respond(&mut stream, "404 Not Found", "text/plain", "not found").await?,
    }
    Ok(())
}

fn json_field(body: &[u8], key: &str) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

async fn command(
    stream: &mut TcpStream,
    shared: &Shared,
    command: RemoteCommand,
) -> std::io::Result<()> {
    match shared.hub.commands.try_send(command) {
        Ok(()) => respond(stream, "202 Accepted", "text/plain", "ok").await,
        Err(_) => respond(stream, "503 Service Unavailable", "text/plain", "busy").await,
    }
}

async fn stream_events(stream: &mut TcpStream, shared: &Shared) -> std::io::Result<()> {
    let mut rx = shared.hub.events.subscribe();
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n: connected\n\n",
        )
        .await?;
    loop {
        match rx.recv().await {
            Ok(event) => {
                // Events are single-line JSON, so one `data:` line frames them.
                stream
                    .write_all(format!("data: {}\n\n", event.replace('\n', " ")).as_bytes())
                    .await?;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
