//! Rig HTTP adapter preserving Retry-After on the failed request's own error.
//!
//! Rig 0.40's reqwest adapter drops failed-response headers. This adapter uses
//! the already-configured reqwest client and preserves exactly the retry hint;
//! it owns no mutable request metadata, so shared clients and nested agents
//! cannot consume another request's hint.

// Fail closed if a source package accidentally resolves the registry SDK.
const _: () = assert!(rig::http_client::MINI_AGENT_HTTP_RESPONSE_SOURCE_CHAIN);

use bytes::Bytes;
use rig::http_client::{self, HttpClientExt, LazyBody, MultipartForm, Request, Response};

#[derive(Clone)]
pub struct ProviderHttpClient(std::sync::Arc<ProviderBackend>);

struct ProviderBackend {
    client: reqwest::Client,
    identity: std::sync::Arc<()>,
    bedrock: Option<std::sync::Arc<crate::provider::bedrock::BedrockAuth>>,
}

impl Default for ProviderHttpClient {
    fn default() -> Self {
        Self::new(reqwest::Client::default())
    }
}

impl std::fmt::Debug for ProviderHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProviderHttpClient([REDACTED])")
    }
}

impl ProviderHttpClient {
    pub(crate) fn new(client: reqwest::Client) -> Self {
        Self(std::sync::Arc::new(ProviderBackend {
            client,
            identity: std::sync::Arc::new(()),
            bedrock: None,
        }))
    }
    pub(crate) fn with_bedrock(
        client: reqwest::Client,
        auth: std::sync::Arc<crate::provider::bedrock::BedrockAuth>,
    ) -> Self {
        Self(std::sync::Arc::new(ProviderBackend {
            client,
            identity: std::sync::Arc::new(()),
            bedrock: Some(auth),
        }))
    }

    async fn prepare_request(
        &self,
        mut request: Request<Bytes>,
    ) -> http_client::Result<reqwest::RequestBuilder> {
        if let Some(auth) = &self.0.bedrock {
            auth.prepare(&mut request).await?;
        }
        let (parts, body) = request.into_parts();
        Ok(self
            .0
            .client
            .request(parts.method, parts.uri.to_string())
            .version(parts.version)
            .headers(parts.headers)
            .body(body))
    }

    pub(crate) fn listing_identity(&self) -> std::sync::Weak<()> {
        std::sync::Arc::downgrade(&self.0.identity)
    }

    /// Listing GETs collect bounded bytes and never open a model usage stream.
    pub(crate) async fn listing_get(
        &self,
        request: http::Request<Vec<u8>>,
        limit: usize,
    ) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            request.method() == http::Method::GET,
            "invalid model listing request"
        );
        let mut response = self
            .prepare_request(request.map(|_| Bytes::new()))
            .await
            .map_err(|_| anyhow::anyhow!("model listing authentication failed"))?
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("model listing transport failed"))?;
        anyhow::ensure!(
            response.status().is_success(),
            "model listing HTTP status {}",
            response.status().as_u16()
        );
        anyhow::ensure!(
            response.content_length().is_none_or(|n| n <= limit as u64),
            "model listing byte limit exceeded"
        );
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("model listing body failed"))?
        {
            anyhow::ensure!(
                chunk.len() <= limit.saturating_sub(body.len()),
                "model listing byte limit exceeded"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub(crate) struct ProviderRetryAfterError {
    pub(crate) status: http::StatusCode,
    pub(crate) retry_after: http::HeaderValue,
    #[source]
    source: http_client::Error,
}

async fn response_error(response: reqwest::Response) -> http_client::Error {
    let status = response.status();
    let retry_after = response.headers().get(http::header::RETRY_AFTER).cloned();
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
    let source = http_client::Error::InvalidStatusCodeWithMessage(status, body);
    match retry_after {
        Some(retry_after) => http_client::Error::Instance(Box::new(ProviderRetryAfterError {
            status,
            retry_after,
            source,
        })),
        None => source,
    }
}

fn lazy_response<U>(response: reqwest::Response) -> http_client::Result<Response<LazyBody<U>>>
where
    U: From<Bytes> + Send + 'static,
{
    let mut builder = Response::builder()
        .status(response.status())
        .version(response.version());
    if let Some(headers) = builder.headers_mut() {
        *headers = response.headers().clone();
    }
    let body: LazyBody<U> = Box::pin(async move {
        response
            .bytes()
            .await
            .map(U::from)
            .map_err(|error| http_client::Error::Instance(Box::new(error)))
    });
    builder.body(body).map_err(http_client::Error::Protocol)
}

impl HttpClientExt for ProviderHttpClient {
    fn send<T, U>(
        &self,
        request: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        let client = self.clone();
        let request = request.map(Into::into);
        async move {
            let response = client
                .prepare_request(request)
                .await?
                .send()
                .await
                .map_err(|error| http_client::Error::Instance(Box::new(error)))?;
            if !response.status().is_success() {
                return Err(response_error(response).await);
            }
            lazy_response(response)
        }
    }

    fn send_multipart<U>(
        &self,
        request: Request<MultipartForm>,
    ) -> impl std::future::Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        let requires_signed_body = self.0.bedrock.is_some();
        let (parts, body) = request.into_parts();
        let request = self
            .0
            .client
            .request(parts.method, parts.uri.to_string())
            .version(parts.version)
            .headers(parts.headers)
            .multipart(reqwest::multipart::Form::from(body));
        async move {
            if requires_signed_body {
                return Err(http_client::Error::Instance(Box::new(
                    std::io::Error::other("Bedrock multipart is unsupported"),
                )));
            }
            let response = request
                .send()
                .await
                .map_err(|error| http_client::Error::Instance(Box::new(error)))?;
            if !response.status().is_success() {
                return Err(response_error(response).await);
            }
            lazy_response(response)
        }
    }

    fn send_streaming<T>(
        &self,
        request: Request<T>,
    ) -> impl std::future::Future<Output = http_client::Result<http_client::StreamingResponse>> + Send
    where
        T: Into<Bytes> + Send,
    {
        let client = self.clone();
        let request = request.map(Into::into);
        let budget_guard = crate::spend::run::Run::current().and_then(|run| run.sdk_stream_guard());
        async move {
            let response = match async {
                client
                    .prepare_request(request)
                    .await?
                    .send()
                    .await
                    .map_err(|error| http_client::Error::Instance(Box::new(error)))
            }
            .await
            {
                Ok(response) => response,
                Err(error) => {
                    if let Some(guard) = &budget_guard {
                        guard.finished();
                    }
                    return Err(error);
                }
            };
            if !response.status().is_success() {
                if let Some(guard) = &budget_guard {
                    guard.finished();
                }
                return Err(response_error(response).await);
            }
            if let Some(guard) = budget_guard {
                guard.opened();
            }
            let mut builder = Response::builder()
                .status(response.status())
                .version(response.version());
            if let Some(headers) = builder.headers_mut() {
                *headers = response.headers().clone();
            }
            use futures::StreamExt;
            let body: http_client::sse::BoxedStream =
                Box::pin(response.bytes_stream().map(|chunk| {
                    chunk.map_err(|error| http_client::Error::Instance(Box::new(error)))
                }));
            builder.body(body).map_err(http_client::Error::Protocol)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures::StreamExt;
    use rig::client::CompletionClient;
    use rig::streaming::StreamingChat;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    async fn read_request(socket: &mut TcpStream) -> String {
        read_request_parts(socket).await.0
    }

    async fn read_request_parts(socket: &mut TcpStream) -> (String, Vec<u8>) {
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            assert!(headers.len() < 16 * 1024);
            headers.push(socket.read_u8().await.unwrap());
        }
        let headers = String::from_utf8(headers).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(length < 16 * 1024);
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        let path = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_string();
        (path, body)
    }

    async fn respond(socket: &mut TcpStream, status: u16, hint: Option<&str>, body: &str) {
        let hint = hint
            .map(|value| format!("Retry-After: {value}\r\n"))
            .unwrap_or_default();
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = format!(
            "HTTP/1.1 {status} fixture\r\nContent-Type: {content_type}\r\n{hint}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    }

    fn completion_error(error: http_client::Error) -> rig::completion::CompletionError {
        rig::completion::CompletionError::HttpError(error)
    }

    /// A real Rig streaming failure, reusable by the midstream state test.
    pub(crate) async fn streaming_error(
        status: u16,
        hint: Option<&str>,
    ) -> rig::agent::StreamingError {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let client = rig::providers::openrouter::Client::builder()
            .api_key("unused-test-key")
            .base_url(url)
            .http_client(ProviderHttpClient::default())
            .build()
            .unwrap();
        let agent = rig::agent::AgentBuilder::new(client.completion_model("test/model"))
            .max_tokens(16)
            .build();
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            respond(
                &mut socket,
                status,
                hint,
                if status == 400 {
                    r#"{"error":{"message":"connection timed out"}}"#
                } else {
                    r#"{"error":{"message":"provider unavailable"}}"#
                },
            )
            .await;
        };
        let request = async {
            let mut stream = agent
                .stream_chat("hello", Vec::<rig::completion::Message>::new())
                .max_turns(1)
                .await;
            while let Some(item) = stream.next().await {
                if let Err(error) = item {
                    return error;
                }
            }
            panic!("HTTP failure did not surface through Rig");
        };
        let (_, error) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, request)
        })
        .await
        .expect("provider fixture exceeded its deadline");
        error
    }

    #[tokio::test]
    async fn buffered_and_multipart_responses_preserve_bodies_and_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let client = ProviderHttpClient::default();
        let server = async {
            for expected in ["/buffered", "/multipart", "/multipart-error"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (path, body) = read_request_parts(&mut socket).await;
                assert_eq!(path, expected);
                if path == "/buffered" {
                    assert_eq!(body, b"request bytes");
                } else {
                    assert!(String::from_utf8(body).unwrap().contains("multipart value"));
                }
                if path == "/multipart-error" {
                    respond(&mut socket, 429, Some("0"), "error body").await;
                } else {
                    respond(&mut socket, 200, Some("5"), "response bytes").await;
                }
            }
        };
        let requests = async {
            let request = Request::builder()
                .method("POST")
                .uri(format!("{url}/buffered"))
                .body("request bytes")
                .unwrap();
            let response = client.send::<_, Bytes>(request).await.unwrap();
            let response_code = response.status();
            assert_eq!(response_code, http::StatusCode::OK);
            assert_eq!(response.headers()[http::header::RETRY_AFTER], "5");
            assert_eq!(response.into_body().await.unwrap(), "response bytes");
            for path in ["/multipart", "/multipart-error"] {
                let request = Request::builder()
                    .method("POST")
                    .uri(format!("{url}{path}"))
                    .body(MultipartForm::new().text("field", "multipart value"))
                    .unwrap();
                let response = client.send_multipart::<Bytes>(request).await;
                if path == "/multipart" {
                    let response = response.unwrap();
                    assert_eq!(response.headers()[http::header::RETRY_AFTER], "5");
                    assert_eq!(response.into_body().await.unwrap(), "response bytes");
                } else {
                    let error = completion_error(response.err().expect("multipart HTTP failure"));
                    assert_eq!(error.provider_response_body(), Some("error body"));
                    // A zero hint cannot shorten the configured computed delay.
                    assert_eq!(
                        crate::retry::retry_delay_for_error(
                            Duration::from_millis(20),
                            Duration::from_millis(20),
                            &error
                        ),
                        Duration::from_millis(20)
                    );
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn configured_client_request_timeout_survives_the_adapter() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ProviderHttpClient::new(
            reqwest::Client::builder()
                .timeout(Duration::from_millis(20))
                .build()
                .unwrap(),
        );
        let request = Request::builder()
            .uri(format!("http://{}", listener.local_addr().unwrap()))
            .body(Vec::<u8>::new())
            .unwrap();
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let receive = async {
            let error = client
                .send_streaming(request)
                .await
                .err()
                .expect("configured timeout");
            let http_client::Error::Instance(source) = &error else {
                panic!("non-HTTP timeout changed classification")
            };
            assert!(
                source
                    .downcast_ref::<reqwest::Error>()
                    .unwrap()
                    .is_timeout()
            );
            assert!(crate::retry::is_retryable(&error));
            assert_eq!(
                crate::retry::retry_delay_for_error(Duration::ZERO, Duration::from_secs(1), &error),
                Duration::ZERO
            );
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, receive);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn concurrent_response_hints_and_later_unhinted_errors_are_request_owned() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let http = ProviderHttpClient::default();
        let server = async {
            let mut pending = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let path = read_request(&mut socket).await;
                pending.push((socket, path));
            }
            // Complete both outstanding requests in reverse arrival order.
            for (mut socket, path) in pending.into_iter().rev() {
                let (status, hint) = if path == "/one" {
                    (429, "2")
                } else {
                    (529, "7")
                };
                respond(&mut socket, status, Some(hint), "provider body").await;
            }
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(read_request(&mut socket).await, "/none");
            respond(&mut socket, 429, None, "unhinted body").await;
        };
        let requests = async {
            let request = |path: &str| {
                Request::builder()
                    .uri(format!("{url}{path}"))
                    .body(Vec::<u8>::new())
                    .unwrap()
            };
            let (one, two) = tokio::join!(
                http.send_streaming(request("/one")),
                http.send_streaming(request("/two"))
            );
            for (error, seconds) in [
                (one.err().expect("first HTTP request must fail"), 2),
                (two.err().expect("second HTTP request must fail"), 7),
            ] {
                let error = completion_error(error);
                assert!(crate::retry::is_retryable(&error));
                assert_eq!(
                    crate::retry::retry_delay_for_error(
                        Duration::ZERO,
                        Duration::from_secs(10),
                        &error
                    ),
                    Duration::from_secs(seconds)
                );
                assert!(error.to_string().contains("provider body"));
                assert!(
                    !error.to_string().contains("Retry-After"),
                    "headers must not become message text"
                );
            }
            let error = completion_error(
                http.send::<_, Bytes>(request("/none"))
                    .await
                    .err()
                    .expect("unhinted HTTP request must fail"),
            );
            assert_eq!(
                error.provider_response_status(),
                Some(http::StatusCode::TOO_MANY_REQUESTS)
            );
            assert!(crate::retry::is_retryable(&error));
            assert_eq!(
                crate::retry::retry_delay_for_error(
                    Duration::ZERO,
                    Duration::from_secs(10),
                    &error
                ),
                Duration::ZERO
            );
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn shared_sdk_streams_keep_concurrent_hints_and_no_header_isolation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = rig::providers::openrouter::Client::builder()
            .api_key("unused-test-key")
            .base_url(format!("http://{}", listener.local_addr().unwrap()))
            .http_client(ProviderHttpClient::default())
            .build()
            .unwrap();
        let one = client.agent("one").max_tokens(16).build();
        let two = client.agent("two").max_tokens(16).build();
        let server = async {
            let mut pending = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (_, body) = read_request_parts(&mut socket).await;
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let (status, hint) = match body["model"].as_str().unwrap() {
                    "one" => (429, "2"),
                    "two" => (529, "7"),
                    other => panic!("unexpected model {other}"),
                };
                pending.push((socket, status, hint));
            }
            for (mut socket, status, hint) in pending.into_iter().rev() {
                respond(&mut socket, status, Some(hint), "provider unavailable").await;
            }
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            respond(&mut socket, 429, None, "provider unavailable").await;
        };
        let requests = async {
            let first = async {
                let mut stream = one
                    .stream_chat("first", Vec::<rig::completion::Message>::new())
                    .await;
                stream
                    .next()
                    .await
                    .unwrap()
                    .err()
                    .expect("first HTTP failure")
            };
            let second = async {
                let mut stream = two
                    .stream_chat("second", Vec::<rig::completion::Message>::new())
                    .await;
                stream
                    .next()
                    .await
                    .unwrap()
                    .err()
                    .expect("second HTTP failure")
            };
            let (first, second) = tokio::join!(first, second);
            for (error, seconds) in [(first, 2), (second, 7)] {
                assert!(crate::retry::is_retryable(&error));
                assert_eq!(
                    crate::retry::retry_delay_for_error(
                        Duration::ZERO,
                        Duration::from_secs(10),
                        &error
                    ),
                    Duration::from_secs(seconds)
                );
            }
            let mut stream = one
                .stream_chat("later", Vec::<rig::completion::Message>::new())
                .await;
            let error = stream
                .next()
                .await
                .unwrap()
                .err()
                .expect("unhinted HTTP failure");
            assert!(crate::retry::is_retryable(&error));
            assert_eq!(
                crate::retry::retry_delay_for_error(
                    Duration::ZERO,
                    Duration::from_secs(10),
                    &error
                ),
                Duration::ZERO
            );
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, requests);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn real_streaming_errors_preserve_hints_and_structural_status_precedence() {
        let future =
            httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(3600));
        for (status, hint, retryable, delay) in [
            (429, Some("3"), true, 100),
            (529, Some("3"), true, 100),
            (429, Some(future.as_str()), true, 100),
            (429, Some("99999999999999999999999999"), true, 100),
            (429, Some("malformed"), true, 0),
            (429, Some("Sun, 06 Nov 1994 08:49:37 GMT"), true, 0),
            (400, Some("3"), false, 100),
        ] {
            let error = streaming_error(status, hint).await;
            assert_eq!(
                crate::retry::is_retryable(&error),
                retryable,
                "status {status}"
            );
            assert_eq!(
                crate::retry::retry_delay_for_error(
                    Duration::ZERO,
                    Duration::from_millis(100),
                    &error
                ),
                Duration::from_millis(delay)
            );
        }
    }

    #[tokio::test]
    async fn real_http_stream_retry_waits_for_the_capped_header_delay_and_recovers() {
        const SUCCESS: &str = concat!(
            "data: {\"id\":\"retry\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"test/model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"recovered\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"retry\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"test/model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        for status in [429, 529] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = rig::providers::openrouter::Client::builder()
                .api_key("unused-test-key")
                .base_url(format!("http://{}", listener.local_addr().unwrap()))
                .http_client(ProviderHttpClient::default())
                .build()
                .unwrap();
            let agent = rig::agent::AgentBuilder::new(client.completion_model("test/model"))
                .max_tokens(16)
                .build();
            let server = async {
                for (code, hint, body) in [
                    (status, Some("1"), "provider unavailable"),
                    (200, None, SUCCESS),
                ] {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    read_request(&mut socket).await;
                    respond(&mut socket, code, hint, body).await;
                }
            };
            let request = async {
                let config = crate::retry::RetryConfig {
                    max_attempts: 2,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 40,
                };
                let mut notices = Vec::new();
                let mut stream = crate::retry::retry_stream_chat_with(
                    &config,
                    || async {
                        agent
                            .stream_chat("hello", Vec::<rig::completion::Message>::new())
                            .max_turns(1)
                            .await
                    },
                    |notice| {
                        notices.push(notice);
                        std::future::ready(())
                    },
                )
                .await
                .unwrap();
                let mut recovered = false;
                while let Some(item) = stream.next().await {
                    if let rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                        rig::streaming::StreamedAssistantContent::Text(text),
                    ) = item.unwrap()
                    {
                        recovered |= text.text == "recovered";
                    }
                }
                assert!(recovered);
                assert_eq!(notices.len(), 1);
                assert_eq!(notices[0].delay, Duration::from_millis(40));
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(server, request);
            })
            .await
            .unwrap();
        }
    }
}
