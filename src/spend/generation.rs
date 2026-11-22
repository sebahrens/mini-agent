//! OpenRouter generation lookup for an interrupted stream (mini-agent-qqknv).
//!
//! A stream that ends before its usage report is charged a byte-based
//! estimate. OpenRouter keeps the real token counts and cost of every
//! generation, addressable by the `gen-...` id that opens each stream chunk, so
//! one short, failure-tolerant lookup can replace the estimate. The lookup is
//! bounded by [`TIMEOUT`], runs off the request path, and never fails a run:
//! on any error or timeout the estimate stands. It applies only to requests
//! sent to OpenRouter's chat-completions endpoint.

use crate::event::UsageDelta;
use std::time::Duration;

/// Longest the lookup may take, end to end.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_SNIFF_BYTES: usize = 4 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Where and how to ask: the generation endpoint beside the request's
/// chat-completions endpoint, with the request's own credential.
#[derive(Clone)]
pub(crate) struct Target {
    client: reqwest::Client,
    endpoint: String,
    authorization: Option<http::HeaderValue>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Target {
    /// The lookup target for a request URI, or `None` when it is not an
    /// OpenRouter chat-completions request.
    pub(crate) fn for_request(
        client: &reqwest::Client,
        uri: &http::Uri,
        headers: &http::HeaderMap,
    ) -> Option<Self> {
        if !crate::provider_http::pin::applies_to(uri) {
            return None;
        }
        Self::build(client, uri, headers)
    }

    fn build(client: &reqwest::Client, uri: &http::Uri, headers: &http::HeaderMap) -> Option<Self> {
        let prefix = uri.path().strip_suffix("/chat/completions")?;
        let scheme = uri.scheme_str()?;
        let authority = uri.authority()?;
        let mut authorization = headers.get(http::header::AUTHORIZATION).cloned();
        if let Some(value) = &mut authorization {
            value.set_sensitive(true);
        }
        Some(Self {
            client: client.clone(),
            endpoint: format!("{scheme}://{authority}{prefix}/generation"),
            authorization,
        })
    }
}

/// A target plus the generation id its stream announced.
#[derive(Clone, Debug)]
pub(crate) struct Pending {
    pub(crate) target: Target,
    pub(crate) id: String,
}

/// Finds the generation id (`"id":"gen-..."`) in the first bytes of a stream.
#[derive(Default)]
pub(crate) struct IdSniffer {
    seen: Vec<u8>,
    done: bool,
}

impl IdSniffer {
    /// Feeds one chunk; returns the id the first time it is complete.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Option<String> {
        if self.done {
            return None;
        }
        let room = MAX_SNIFF_BYTES.saturating_sub(self.seen.len());
        self.seen.extend_from_slice(&chunk[..chunk.len().min(room)]);
        const KEY: &str = "\"id\":\"gen-";
        let found = {
            let text = String::from_utf8_lossy(&self.seen);
            text.find(KEY).and_then(|start| {
                let rest = &text[start + KEY.len() - "gen-".len()..];
                rest.find('"').map(|end| rest[..end].to_owned())
            })
        };
        if let Some(id) = found {
            self.finish();
            return valid_id(&id).then_some(id);
        }
        if self.seen.len() >= MAX_SNIFF_BYTES {
            self.finish();
        }
        None
    }

    fn finish(&mut self) {
        self.done = true;
        self.seen = Vec::new();
    }
}

fn valid_id(id: &str) -> bool {
    id.len() <= MAX_ID_BYTES
        && id.starts_with("gen-")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Asks the generation endpoint once, within `timeout`. `None` on any failure.
pub(crate) async fn lookup(pending: &Pending, timeout: Duration) -> Option<UsageDelta> {
    tokio::time::timeout(timeout, fetch(pending, timeout))
        .await
        .ok()
        .flatten()
}

async fn fetch(pending: &Pending, timeout: Duration) -> Option<UsageDelta> {
    if !valid_id(&pending.id) {
        return None;
    }
    let mut request = pending
        .target
        .client
        .get(&pending.target.endpoint)
        .query(&[("id", pending.id.as_str())])
        .timeout(timeout);
    if let Some(authorization) = &pending.target.authorization {
        request = request.header(http::header::AUTHORIZATION, authorization.clone());
    }
    let mut response = request.send().await.ok()?;
    if response.error_for_status_ref().is_err() {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    parse(&body)
}

/// The usage in a generation record: native token counts when reported (they
/// are what the provider billed), the normalised ones otherwise.
fn parse(body: &[u8]) -> Option<UsageDelta> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let data = value.get("data")?;
    let count = |native: &str, normalised: &str| {
        [native, normalised].iter().find_map(|key| {
            data.get(*key)
                .and_then(serde_json::Value::as_u64)
                .filter(|_| !data[*key].is_null())
        })
    };
    let input = count("native_tokens_prompt", "tokens_prompt")?;
    let output = count("native_tokens_completion", "tokens_completion")?;
    let cached = count("native_tokens_cached", "tokens_cached").unwrap_or(0);
    let reasoning = count("native_tokens_reasoning", "tokens_reasoning").unwrap_or(0);
    let cost = data
        .get("total_cost")
        .and_then(serde_json::Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .map_or(0, |cost| (cost * 1e9).round() as u64);
    Some(UsageDelta {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input.saturating_add(output),
        cached_input_tokens: cached.min(input),
        reasoning_tokens: reasoning.min(output),
        reported_cost_nano_usd: cost,
        ..Default::default()
    })
}

static PENDING: std::sync::Mutex<Vec<(String, tokio::task::JoinHandle<()>)>> =
    std::sync::Mutex::new(Vec::new());

/// Runs `task` (a lookup and its ledger amendment) in the background for
/// `session_id`; [`settle`] waits for it, bounded by [`TIMEOUT`].
pub(crate) fn run_in_background(
    session_id: &str,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let handle = tokio::task::spawn(task);
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|(_, handle)| !handle.is_finished());
    pending.push((session_id.to_owned(), handle));
}

/// Waits (at most [`TIMEOUT`] plus a margin) for the session's outstanding
/// lookups, so a flush writes the corrected rows and shutdown is never held
/// longer than the lookup bound.
pub(crate) async fn settle(session_id: &str) {
    let handles: Vec<_> = {
        let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
            .into_iter()
            .partition(|(id, _)| id == session_id);
        *pending = rest;
        mine.into_iter().map(|(_, handle)| handle).collect()
    };
    if handles.is_empty() {
        return;
    }
    let wait = futures::future::join_all(handles);
    let _ = tokio::time::timeout(TIMEOUT + Duration::from_millis(500), wait).await;
}

/// A lookup against a stub endpoint rooted at `base` (`http://host:port/api/v1`).
#[cfg(test)]
pub(crate) fn stub_pending(base: &str) -> Pending {
    let uri: http::Uri = format!("{base}/chat/completions").parse().unwrap();
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static("Bearer stub-key"),
    );
    Pending {
        target: Target::build(&reqwest::Client::new(), &uri, &headers).unwrap(),
        id: "gen-test-1".into(),
    }
}

/// Serves one generation answer on a loopback port and returns its base URL.
#[cfg(test)]
pub(crate) async fn stub_server(
    status: u16,
    body: &'static str,
    pause: Duration,
) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut seen = Vec::new();
        let mut buffer = [0u8; 1024];
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&buffer[..read]);
        }
        tokio::time::sleep(pause).await;
        let response = format!(
            "HTTP/1.1 {status} stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        String::from_utf8_lossy(&seen).into_owned()
    });
    (base, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn sniffer_finds_the_generation_id_across_chunks_and_rejects_odd_ids() {
        let mut sniffer = IdSniffer::default();
        assert_eq!(sniffer.feed(b"data: {\"id\":\"gen-17"), None);
        assert_eq!(
            sniffer.feed(b"00-abc_D\",\"choices\":[]}").as_deref(),
            Some("gen-1700-abc_D")
        );
        assert_eq!(
            sniffer.feed(b"{\"id\":\"gen-second\"}"),
            None,
            "first id only"
        );
        let mut odd = IdSniffer::default();
        assert_eq!(odd.feed(b"{\"id\":\"gen-a/../b\"}"), None);
        let mut other = IdSniffer::default();
        assert_eq!(other.feed(b"{\"id\":\"chatcmpl-1\"}"), None);
    }

    #[test]
    fn parse_prefers_native_counts_and_the_billed_cost() {
        let usage = parse(
            br#"{"data":{"tokens_prompt":10,"tokens_completion":4,"native_tokens_prompt":12,
            "native_tokens_completion":5,"native_tokens_cached":3,"native_tokens_reasoning":2,
            "total_cost":0.00042}}"#,
        )
        .unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (12, 5, 17)
        );
        assert_eq!((usage.cached_input_tokens, usage.reasoning_tokens), (3, 2));
        assert_eq!(usage.reported_cost_nano_usd, 420_000);
        assert!(parse(br#"{"data":{"tokens_prompt":1}}"#).is_none());
        assert!(parse(b"not json").is_none());
    }

    use super::{stub_pending as pending, stub_server as stub};

    #[tokio::test]
    async fn lookup_asks_the_generation_endpoint_with_the_request_credential() {
        let (base, server) = stub(
            200,
            r#"{"data":{"native_tokens_prompt":100,"native_tokens_completion":20,"total_cost":0.001}}"#,
            Duration::ZERO,
        )
        .await;
        let usage = lookup(&pending(&base), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (100, 20));
        let request = server.await.unwrap();
        assert!(
            request.starts_with("GET /api/v1/generation?id=gen-test-1 "),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer stub-key")
        );
    }

    #[tokio::test]
    async fn lookup_tolerates_errors_and_never_outlasts_its_timeout() {
        let (base, _) = stub(404, r#"{"error":"not found"}"#, Duration::ZERO).await;
        assert!(
            lookup(&pending(&base), Duration::from_secs(2))
                .await
                .is_none()
        );
        let (base, _) = stub(200, r#"{"data":{}}"#, Duration::from_secs(5)).await;
        let started = std::time::Instant::now();
        assert!(
            lookup(&pending(&base), Duration::from_millis(200))
                .await
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}/api/v1", listener.local_addr().unwrap())
        };
        assert!(
            lookup(&pending(&closed), Duration::from_secs(1))
                .await
                .is_none()
        );
    }

    #[test]
    fn only_openrouter_chat_completions_requests_have_a_target() {
        let client = reqwest::Client::new();
        let headers = http::HeaderMap::new();
        let uri: http::Uri = "https://openrouter.ai/api/v1/chat/completions"
            .parse()
            .unwrap();
        let target = Target::for_request(&client, &uri, &headers).unwrap();
        assert_eq!(target.endpoint, "https://openrouter.ai/api/v1/generation");
        let uri: http::Uri = "https://openrouter.ai/api/v1/responses".parse().unwrap();
        assert!(Target::for_request(&client, &uri, &headers).is_none());
    }
}
