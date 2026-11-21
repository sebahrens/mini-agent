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
    chatgpt: Option<crate::auth::oauth::Registration>,
    copilot: Option<std::sync::Arc<crate::provider::copilot::CopilotAuth>>,
    vertex: Option<std::sync::Arc<crate::provider::vertex::VertexAuth>>,
    bedrock: Option<std::sync::Arc<crate::provider::bedrock::BedrockAuth>>,
    #[cfg(test)]
    test_wire: Option<reqwest::Url>,
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
            chatgpt: None,
            copilot: None,
            bedrock: None,
            vertex: None,
            #[cfg(test)]
            test_wire: None,
        }))
    }
    pub(crate) fn with_chatgpt(
        client: reqwest::Client,
        registration: crate::auth::oauth::Registration,
    ) -> Self {
        Self(std::sync::Arc::new(ProviderBackend {
            client,
            identity: std::sync::Arc::new(()),
            chatgpt: Some(registration),
            copilot: None,
            bedrock: None,
            vertex: None,
            #[cfg(test)]
            test_wire: None,
        }))
    }
    #[cfg(feature = "bedrock")]
    pub(crate) fn with_bedrock(
        client: reqwest::Client,
        auth: std::sync::Arc<crate::provider::bedrock::BedrockAuth>,
    ) -> Self {
        Self(std::sync::Arc::new(ProviderBackend {
            client,
            identity: std::sync::Arc::new(()),
            bedrock: Some(auth),
            vertex: None,
            chatgpt: None,
            copilot: None,
            #[cfg(test)]
            test_wire: None,
        }))
    }
    #[cfg(feature = "vertex")]
    pub(crate) fn with_vertex(
        client: reqwest::Client,
        auth: std::sync::Arc<crate::provider::vertex::VertexAuth>,
    ) -> Self {
        Self(std::sync::Arc::new(ProviderBackend {
            client,
            identity: std::sync::Arc::new(()),
            bedrock: None,
            vertex: Some(auth),
            chatgpt: None,
            copilot: None,
            #[cfg(test)]
            test_wire: None,
        }))
    }
    pub(crate) fn with_copilot(
        client: reqwest::Client,
        auth: std::sync::Arc<crate::provider::copilot::CopilotAuth>,
    ) -> Self {
        Self(std::sync::Arc::new(ProviderBackend {
            client,
            identity: std::sync::Arc::new(()),
            bedrock: None,
            vertex: None,
            chatgpt: None,
            copilot: Some(auth),
            #[cfg(test)]
            test_wire: None,
        }))
    }
    pub(crate) fn is_copilot_login(&self) -> bool {
        self.0.copilot.as_ref().is_some_and(|auth| auth.is_login())
    }
    pub(crate) fn is_chatgpt(&self) -> bool {
        self.0.chatgpt.is_some()
    }
    /// The service label of a private-auth backend whose error bodies are never
    /// echoed (see `private_error_text`).
    fn private_error_message(&self) -> Option<&'static str> {
        if self.is_chatgpt() {
            Some("ChatGPT")
        } else if self.0.copilot.is_some() {
            Some("Copilot")
        } else {
            None
        }
    }
    #[cfg(test)]
    pub(crate) fn chatgpt_test_wire(
        registration: crate::auth::oauth::Registration,
        url: reqwest::Url,
    ) -> Self {
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        Self(std::sync::Arc::new(ProviderBackend {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            identity: std::sync::Arc::new(()),
            chatgpt: Some(registration),
            copilot: None,
            bedrock: None,
            vertex: None,
            test_wire: Some(url),
        }))
    }
    async fn prepare_request(
        &self,
        request: Request<Bytes>,
    ) -> http_client::Result<reqwest::RequestBuilder> {
        self.prepare_request_applied(request)
            .await
            .map(|(builder, _)| builder)
    }

    /// Sends `request`. When it carried a command-issued credential
    /// (`api_key_commands`) and got `401`, the credential is renewed and the
    /// same request is resent exactly once (mini-agent-toywy).
    async fn send_with_credentials(
        &self,
        request: Request<Bytes>,
    ) -> http_client::Result<reqwest::Response> {
        let (parts, body) = request.into_parts();
        let rebuild = |headers: http::HeaderMap| -> http_client::Result<Request<Bytes>> {
            let mut again = Request::builder()
                .method(parts.method.clone())
                .uri(parts.uri.clone())
                .version(parts.version)
                .body(body.clone())
                .map_err(http_client::Error::Protocol)?;
            *again.headers_mut() = headers;
            Ok(again)
        };
        let mut original_headers = parts.headers.clone();
        let (builder, applied) = self
            .prepare_request_applied(rebuild(original_headers.clone())?)
            .await?;
        let response = builder.send().await.map_err(transport_error)?;
        if response.status() != http::StatusCode::UNAUTHORIZED
            || !self
                .renew_after_unauthorized(&applied, &mut original_headers)
                .await
        {
            return Ok(response);
        }
        self.prepare_request(rebuild(original_headers)?)
            .await?
            .send()
            .await
            .map_err(transport_error)
    }

    /// Renews the credential a `401` rejected so the request can be resent
    /// once: a command-issued key (`api_key_commands`), or the cached ChatGPT
    /// access token / Copilot model token, which are otherwise reused until
    /// their local expiry even after the provider invalidated them.
    async fn renew_after_unauthorized(
        &self,
        applied: &[crate::key_command::Applied],
        headers: &mut http::HeaderMap,
    ) -> bool {
        if !applied.is_empty() {
            return crate::key_command::renew_after_unauthorized(applied).await;
        }
        if let Some(registration) = &self.0.chatgpt {
            return registration.invalidate().await.is_ok();
        }
        match &self.0.copilot {
            Some(auth) => auth.renew_after_unauthorized(headers).await,
            None => false,
        }
    }

    async fn prepare_request_applied(
        &self,
        mut request: Request<Bytes>,
    ) -> http_client::Result<(reqwest::RequestBuilder, Vec<crate::key_command::Applied>)> {
        let (mut headers, mut uri) = (std::mem::take(request.headers_mut()), request.uri().clone());
        let applied = crate::key_command::apply_all(&mut headers, &mut uri).await;
        *request.headers_mut() = headers;
        *request.uri_mut() = uri;
        #[cfg(feature = "bedrock")]
        if let Some(auth) = &self.0.bedrock {
            auth.prepare(&mut request).await?;
        }
        #[cfg(feature = "vertex")]
        if let Some(auth) = &self.0.vertex {
            auth.prepare(&mut request).await?;
        }
        if let Some(auth) = &self.0.copilot {
            auth.prepare(&mut request).await?;
        }
        if let Some(registration) = &self.0.chatgpt {
            let prepare = async {
                crate::auth::openai_plan::endpoint(request.method(), request.uri())?;
                if request.method() == http::Method::POST {
                    *request.body_mut() = crate::auth::openai_plan::body(request.body())?.into();
                }
                let token = registration.access_token().await?;
                let mut header = http::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| anyhow::anyhow!("Invalid ChatGPT access token"))?;
                header.set_sensitive(true);
                request
                    .headers_mut()
                    .insert(http::header::AUTHORIZATION, header);
                Ok::<(), anyhow::Error>(())
            }
            .await;
            prepare.map_err(|error| match error.downcast::<crate::auth::AuthFailure>() {
                // Keep the typed failure on the chain so the retry policy can
                // tell a transient refresh error from a revoked grant.
                Ok(failure) => http_client::Error::Instance(Box::new(failure)),
                Err(error) => http_client::Error::Instance(error.into()),
            })?;
        }
        strip_empty_bearer(request.headers_mut());
        let (parts, body) = request.into_parts();
        let destination = parts.uri.to_string();
        #[cfg(test)]
        let destination = match &self.0.test_wire {
            Some(base) => base
                .join(
                    parts
                        .uri
                        .path_and_query()
                        .map(|p| p.as_str())
                        .unwrap_or("/"),
                )
                .unwrap()
                .to_string(),
            None => destination,
        };
        Ok((
            self.0
                .client
                .request(parts.method, destination)
                .version(parts.version)
                .headers(parts.headers)
                .body(body),
            applied,
        ))
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
        // Same path as completions, so command-issued credentials are renewed
        // after a 401 here too.
        let mut response = self
            .send_with_credentials(request.map(Bytes::from))
            .await
            .map_err(|error| anyhow::anyhow!(listing_transport_message(&error.to_string())))?;
        let listing_status = response.status();
        if !listing_status.is_success() {
            anyhow::bail!("{}", listing_status_message(listing_status.as_u16()));
        }
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
        if self.is_chatgpt() {
            crate::auth::openai_plan::models(&body)
        } else {
            Ok(body)
        }
    }
}

/// Removes an `Authorization: Bearer ` header with no token. A keyless local
/// server (LM Studio, llama.cpp, vLLM) is built with an empty key, which the
/// SDK would otherwise send as an empty bearer (mini-agent-nvnkx.41).
fn strip_empty_bearer(headers: &mut http::HeaderMap) {
    if headers
        .get(http::header::AUTHORIZATION)
        .is_some_and(|value| {
            value
                .as_bytes()
                .trim_ascii()
                .eq_ignore_ascii_case(b"bearer")
        })
    {
        headers.remove(http::header::AUTHORIZATION);
    }
}

/// A closed, secret-free description of a listing transport failure, so DNS,
/// TLS, refusal and timeout are distinguishable.
pub(crate) fn listing_transport_message(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("official") {
        "model listing refused: saved credentials are restricted to the official endpoint (check base_url)"
    } else if lower.contains("dns") || lower.contains("lookup") || lower.contains("resolve") {
        "model listing failed: host name could not be resolved (check base_url)"
    } else if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        "model listing failed: TLS handshake or certificate error"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "model listing failed: request timed out"
    } else if lower.contains("refused") || lower.contains("connect") {
        "model listing failed: connection refused or unreachable (check base_url)"
    } else {
        "model listing transport failed"
    }
}

pub(crate) fn listing_status_message(status: u16) -> String {
    match status {
        401 | 403 => format!("model listing HTTP status {status} (check the API key)"),
        404 => format!(
            "model listing HTTP status {status} (this endpoint has no model list; check base_url)"
        ),
        _ => format!("model listing HTTP status {status}"),
    }
}

/// A failed provider response together with the retry-relevant response
/// headers (`Retry-After`, `retry-after-ms`, `x-should-retry`, rate-limit
/// reset headers). Only those
/// headers are kept, so credentials or cookies never ride the error chain.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub(crate) struct ProviderRetryAfterError {
    pub(crate) status: http::StatusCode,
    pub(crate) headers: http::HeaderMap,
    #[source]
    source: http_client::Error,
}

#[cfg(test)]
impl ProviderRetryAfterError {
    /// A synthetic failed response carrying `headers`, for classifier tests.
    pub(crate) fn for_test(status: u16, headers: &[(&str, &str)], body: &str) -> Self {
        let status = http::StatusCode::from_u16(status).unwrap();
        Self {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        http::HeaderValue::from_str(value).unwrap(),
                    )
                })
                .collect(),
            source: http_client::Error::InvalidStatusCodeWithMessage(status, body.to_owned()),
        }
    }
}

/// Whether a response header carries retry guidance worth preserving.
pub(crate) fn is_retry_hint_header(name: &http::HeaderName) -> bool {
    let name = name.as_str();
    matches!(name, "retry-after" | "retry-after-ms" | "x-should-retry")
        || (name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset"))
        || name.starts_with("x-ratelimit-reset")
}

fn retry_hint_headers(headers: &http::HeaderMap) -> http::HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| is_retry_hint_header(name))
        .take(32)
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Upper bound on the error body kept and read; the rest is discarded.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Reads at most `MAX_ERROR_BODY_BYTES` of a non-2xx body so a gateway that
/// streams an endless or huge error cannot grow memory without limit.
async fn read_error_body(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    let mut truncated = false;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_ERROR_BODY_BYTES - body.len();
                if chunk.len() > room {
                    body.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
                if body.len() == MAX_ERROR_BODY_BYTES {
                    // Exactly at the cap; anything further is dropped.
                    truncated = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                return format!("failed to read error response body: {error}");
            }
        }
    }
    let mut text = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        text.push_str("... [error body truncated]");
    }
    text
}

/// Closed set of provider error codes that may be lifted out of a private-auth
/// error body. Only these literals ever reach the message, never body text.
const PRIVATE_ERROR_CODES: [&str; 8] = [
    "model_max_prompt_tokens_exceeded",
    "usage_limit_reached",
    "context_length_exceeded",
    "rate_limit_exceeded",
    "model_not_supported",
    "model_not_found",
    "unsupported_api_for_model",
    "content_filter",
];

/// Codes that mean the selected model is not available to this account.
const MODEL_AVAILABILITY_CODES: [&str; 3] = [
    "model_not_supported",
    "model_not_found",
    "unsupported_api_for_model",
];

/// Bytes and time spent looking for a code in a private-auth error body.
const PRIVATE_ERROR_PEEK_BYTES: usize = 16 * 1024;
const PRIVATE_ERROR_PEEK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);

fn closed_error_code(body: &[u8]) -> Option<&'static str> {
    let lower = String::from_utf8_lossy(body).to_ascii_lowercase();
    PRIVATE_ERROR_CODES
        .into_iter()
        .find(|code| lower.contains(code))
}

/// Reads a bounded, time-limited prefix of the body and returns only a code
/// from the closed set; the response is dropped (retired) afterwards.
async fn peek_private_error_code(mut response: reqwest::Response) -> Option<&'static str> {
    let read = async {
        let mut body = Vec::new();
        while body.len() < PRIVATE_ERROR_PEEK_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let room = PRIVATE_ERROR_PEEK_BYTES - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                _ => break,
            }
        }
        body
    };
    let body = tokio::time::timeout(PRIVATE_ERROR_PEEK_TIMEOUT, read)
        .await
        .unwrap_or_default();
    closed_error_code(&body)
}

/// The no-echo message for a private-auth failure, derived from the status and
/// at most one closed-set error code. 5xx and 429 are not blamed on the account.
fn private_error_text(label: &str, status: http::StatusCode, code: Option<&str>) -> String {
    if let Some(code) = code {
        if MODEL_AVAILABILITY_CODES.contains(&code) {
            return format!(
                "{label} request was rejected: this model is not available on your plan or not enabled for your account; run /models to pick another"
            );
        }
        if code == "content_filter" {
            return format!("{label} request was rejected: the content filter blocked the request");
        }
        return format!("{label} request was rejected: {code}");
    }
    let code = status.as_u16();
    match code {
        429 => format!("{label} rate limit reached; the service asked to slow down"),
        500..=599 => format!("{label} service error (HTTP {code})"),
        _ => format!("{label} request was rejected; check your account access and plan limits"),
    }
}

async fn response_error(
    response: reqwest::Response,
    private_auth: Option<&'static str>,
) -> http_client::Error {
    let status = response.status();
    let hint_headers = retry_hint_headers(response.headers());
    let body = if let Some(label) = private_auth {
        let code = peek_private_error_code(response).await;
        private_error_text(label, status, code)
    } else {
        read_error_body(response).await
    };
    let source = http_client::Error::InvalidStatusCodeWithMessage(status, body);
    if hint_headers.is_empty() {
        source
    } else {
        http_client::Error::Instance(Box::new(ProviderRetryAfterError {
            status,
            headers: hint_headers,
            source,
        }))
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
    let body: LazyBody<U> =
        Box::pin(async move { response.bytes().await.map(U::from).map_err(transport_error) });
    builder.body(body).map_err(http_client::Error::Protocol)
}

/// Wraps a reqwest failure without its request URL. rig puts the Gemini API key
/// (and any command-issued secret rewritten into it) in the query string, and
/// reqwest's `Display` appends ` for url (...)`; stripping the URL keeps the key
/// out of error text, logs and session records (mini-agent-h6wfe).
fn transport_error(error: reqwest::Error) -> http_client::Error {
    http_client::Error::Instance(Box::new(error.without_url()))
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
        let this = self.clone();
        let request = request.map(Into::into);
        async move {
            let mut response = this.send_with_credentials(request).await?;
            if !response.status().is_success() {
                return Err(response_error(response, this.private_error_message()).await);
            }
            if this.is_chatgpt() {
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                    if chunk.len() > crate::auth::openai_plan::MAX_BODY.saturating_sub(bytes.len())
                    {
                        return Err(http_client::Error::Instance(
                            anyhow::anyhow!("ChatGPT response byte limit exceeded").into(),
                        ));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let bytes = crate::auth::openai_plan::completed_sse(&bytes)
                    .map_err(|e| http_client::Error::Instance(e.into()))?;
                let body: LazyBody<U> = Box::pin(async move { Ok(U::from(Bytes::from(bytes))) });
                return Response::builder()
                    .status(http::StatusCode::OK)
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .map_err(http_client::Error::Protocol);
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
        let requires_signed_body =
            self.0.bedrock.is_some() || self.0.vertex.is_some() || self.0.copilot.is_some();
        let unsupported = self.is_chatgpt();
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
            if unsupported {
                return Err(http_client::Error::Instance(
                    anyhow::anyhow!(
                        "Multipart APIs are not supported by ChatGPT plan authentication"
                    )
                    .into(),
                ));
            }
            let response = request.send().await.map_err(transport_error)?;
            if !response.status().is_success() {
                return Err(response_error(response, None).await);
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
            let response = match client.send_with_credentials(request).await {
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
                return Err(response_error(response, client.private_error_message()).await);
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
            let body: http_client::sse::BoxedStream = Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(transport_error)),
            );
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

    #[test]
    fn private_error_text_lifts_only_closed_codes_and_blames_the_right_cause() {
        let code = closed_error_code(
            br#"{"error":{"code":"model_max_prompt_tokens_exceeded","message":"secret echo"}}"#,
        );
        let overflow = private_error_text("Copilot", http::StatusCode::BAD_REQUEST, code);
        assert!(crate::retry::is_context_length_error_message(&overflow));
        assert!(!overflow.contains("secret echo"));

        let code = closed_error_code(b"{\"type\":\"usage_limit_reached\"}");
        let quota = private_error_text("ChatGPT", http::StatusCode::TOO_MANY_REQUESTS, code);
        let error = http_client::Error::InvalidStatusCodeWithMessage(
            http::StatusCode::TOO_MANY_REQUESTS,
            quota,
        );
        assert!(!crate::retry::is_retryable(&error));

        let unavailable =
            private_error_text("Copilot", http::StatusCode::SERVICE_UNAVAILABLE, None);
        assert!(!unavailable.contains("account access"), "{unavailable}");
        assert!(closed_error_code(b"free text only").is_none());

        let code = closed_error_code(
            br#"{"error":{"code":"model_not_supported","message":"secret echo gpt-9"}}"#,
        );
        let model = private_error_text("Copilot", http::StatusCode::BAD_REQUEST, code);
        assert!(model.contains("model is not available"), "{model}");
        assert!(model.contains("/models"), "{model}");
        assert!(!model.contains("secret echo") && !model.contains("model_not_supported"));
        let filtered = private_error_text(
            "ChatGPT",
            http::StatusCode::BAD_REQUEST,
            closed_error_code(b"{\"code\":\"content_filter\"}"),
        );
        assert!(filtered.contains("content filter"), "{filtered}");
    }

    #[test]
    fn empty_bearer_is_stripped_and_real_credentials_are_kept() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, "Bearer ".parse().unwrap());
        strip_empty_bearer(&mut headers);
        assert!(headers.get(http::header::AUTHORIZATION).is_none());
        headers.insert(http::header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        strip_empty_bearer(&mut headers);
        assert!(headers.get(http::header::AUTHORIZATION).is_some());
    }

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

    // mini-agent-h6wfe: a connection failure must not echo the key= query.
    #[tokio::test]
    async fn transport_error_text_never_contains_the_url_query_key() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let client = ProviderHttpClient::new(reqwest::Client::new());
        let secret = "AIzaSECRETKEY123";
        let build = || {
            Request::builder()
                .uri(format!(
                    "http://127.0.0.1:{port}/v1beta/m?alt=sse&key={secret}"
                ))
                .body(Vec::<u8>::new())
                .unwrap()
        };
        let streaming = client.send_streaming(build()).await.err().expect("refused");
        let buffered = client
            .send::<Vec<u8>, Bytes>(build())
            .await
            .err()
            .expect("refused");
        for error in [streaming, buffered] {
            let text = format!("{error} / {error:?}");
            assert!(!text.contains(secret), "key leaked: {text}");
            assert!(!text.contains("key="), "query leaked: {text}");
        }
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
                    max_retry_after_ms: 0,
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
