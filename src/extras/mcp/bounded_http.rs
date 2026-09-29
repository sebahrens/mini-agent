//! Streamable-HTTP MCP client with bounded response bodies.
//!
//! rmcp's `reqwest::Client` transport buffers a whole JSON response, buffers
//! each SSE event until its terminating blank line, and copies a whole non-2xx
//! body into the error text, all without a size limit. [`BoundedHttpClient`]
//! implements the same [`StreamableHttpClient`] contract over the configured
//! `reqwest::Client`, but reads every body incrementally with a running byte
//! cap:
//!
//! * a JSON response body larger than the cap fails the request;
//! * one SSE event larger than the cap ends the stream (a request-scoped stream
//!   first answers its request with a JSON-RPC error naming the cap, because
//!   rmcp only logs stream errors and would otherwise leave the call pending
//!   until its timeout);
//! * a non-2xx body is read under the same cap and at most
//!   [`MCP_HTTP_ERROR_BODY_BYTES`] of it enters the error string.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use http::header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use http::{HeaderName, HeaderValue, StatusCode};
use rmcp::model::{
    ClientJsonRpcMessage, ErrorData, JsonRpcMessage, RequestId, ServerJsonRpcMessage,
};
use rmcp::transport::common::http_header::{
    EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient, StreamableHttpError,
    StreamableHttpPostResponse,
};
use sse_stream::{Sse, SseStream};

use super::client::MCP_STDIO_MAX_LINE_BYTES;

/// Largest HTTP response body, or single SSE event, accepted from a
/// streamable-HTTP MCP server. Matches the stdio per-message line cap so both
/// transports bound one protocol message the same way.
pub(crate) const MCP_HTTP_MAX_BODY_BYTES: usize = MCP_STDIO_MAX_LINE_BYTES;

/// Most bytes of a non-2xx response body copied into an error message.
pub(crate) const MCP_HTTP_ERROR_BODY_BYTES: usize = 4 * 1024;

type HttpError = StreamableHttpError<reqwest::Error>;

/// [`StreamableHttpClient`] over a `reqwest::Client` whose response bodies
/// and SSE events are capped at `max_body_bytes`.
#[derive(Clone, Debug)]
pub(crate) struct BoundedHttpClient {
    inner: reqwest::Client,
    max_body_bytes: usize,
}

impl BoundedHttpClient {
    pub(crate) fn new(inner: reqwest::Client, max_body_bytes: usize) -> Self {
        Self {
            inner,
            max_body_bytes,
        }
    }
}

fn body_cap_message(cap: usize) -> String {
    format!("MCP HTTP response exceeded the {cap}-byte size cap")
}

fn event_cap_message(cap: usize) -> String {
    format!("MCP HTTP SSE event exceeded the {cap}-byte size cap")
}

/// rmcp reserves these headers for the transport itself.
/// `MCP-Protocol-Version` is reserved too but deliberately passed through: the
/// rmcp worker injects it after initialization.
fn apply_custom_headers(
    mut builder: reqwest::RequestBuilder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, HttpError> {
    for (name, value) in custom_headers {
        if ["accept", HEADER_SESSION_ID, HEADER_LAST_EVENT_ID]
            .iter()
            .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
        {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        builder = builder.header(name, value);
    }
    Ok(builder)
}

/// The `scope=` parameter of a `WWW-Authenticate` header, quoted or bare.
fn scope_from_header(header: &str) -> Option<String> {
    let start = header.to_ascii_lowercase().find("scope=")? + "scope=".len();
    let value = &header[start..];
    if let Some(quoted) = value.strip_prefix('"') {
        return quoted.find('"').map(|end| quoted[..end].to_string());
    }
    let end = value
        .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .unwrap_or(value.len());
    (end > 0).then(|| value[..end].to_string())
}

fn header_str(value: &HeaderValue) -> Result<String, HttpError> {
    value.to_str().map(str::to_string).map_err(|_| {
        StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
            "invalid www-authenticate header value",
        ))
    })
}

/// The single HTTP status read in this module (an HTTP response, not a
/// process exit status; classified `NON-PROCESS` in the subprocess inventory).
fn response_code(response: &reqwest::Response) -> StatusCode {
    response.status()
}

fn starts_with_mime(content_type: Option<&str>, mime: &str) -> bool {
    content_type.is_some_and(|ct| ct.as_bytes().starts_with(mime.as_bytes()))
}

/// A response body read under a byte cap. When `exceeded`, `bytes` holds only
/// the prefix read before the cap was hit (possibly nothing, when the declared
/// `Content-Length` was already too large).
struct CappedBody {
    bytes: Vec<u8>,
    exceeded: bool,
}

async fn read_body_capped(
    response: &mut reqwest::Response,
    cap: usize,
) -> Result<CappedBody, reqwest::Error> {
    if response
        .content_length()
        .is_some_and(|declared| declared > cap as u64)
    {
        return Ok(CappedBody {
            bytes: Vec::new(),
            exceeded: true,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = cap - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok(CappedBody {
                bytes,
                exceeded: true,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(CappedBody {
        bytes,
        exceeded: false,
    })
}

/// Render at most [`MCP_HTTP_ERROR_BODY_BYTES`] of an error body as text.
fn error_body_excerpt(body: &CappedBody) -> String {
    let shown = &body.bytes[..body.bytes.len().min(MCP_HTTP_ERROR_BODY_BYTES)];
    let lossy = String::from_utf8_lossy(shown);
    let mut excerpt = String::with_capacity(lossy.len().min(MCP_HTTP_ERROR_BODY_BYTES));
    for character in lossy.chars() {
        if excerpt.len() + character.len_utf8() > MCP_HTTP_ERROR_BODY_BYTES {
            break;
        }
        excerpt.push(character);
    }
    if body.exceeded || body.bytes.len() > MCP_HTTP_ERROR_BODY_BYTES {
        excerpt.push_str(&format!(
            " [body truncated to {MCP_HTTP_ERROR_BODY_BYTES} bytes]"
        ));
    }
    excerpt
}

fn parse_json_rpc_error(body: &[u8]) -> Option<ServerJsonRpcMessage> {
    match serde_json::from_slice::<ServerJsonRpcMessage>(body) {
        Ok(message @ JsonRpcMessage::Error(_)) => Some(message),
        _ => None,
    }
}

/// Tracks the size of the SSE event currently being received. An event ends at
/// a blank line; `\r\n`, `\n`, and `\r` all terminate a line.
#[derive(Debug)]
struct EventSizeCounter {
    cap: usize,
    current: usize,
    line_has_content: bool,
    previous_cr: bool,
}

impl EventSizeCounter {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            current: 0,
            line_has_content: false,
            previous_cr: false,
        }
    }

    /// Account for `chunk`; returns the offset of the first byte that pushed
    /// the current event past the cap.
    fn accept(&mut self, chunk: &[u8]) -> Option<usize> {
        for (offset, &byte) in chunk.iter().enumerate() {
            let previous_cr = std::mem::replace(&mut self.previous_cr, byte == b'\r');
            match byte {
                // Second half of a `\r\n` terminator.
                b'\n' if previous_cr => {}
                b'\n' | b'\r' if self.line_has_content => {
                    self.line_has_content = false;
                    self.current += 1;
                }
                // A blank line dispatches the event.
                b'\n' | b'\r' => self.current = 0,
                _ => {
                    self.line_has_content = true;
                    self.current += 1;
                }
            }
            if self.current > self.cap {
                return Some(offset);
            }
        }
        None
    }
}

/// Error carried through the SSE parser's body stream.
#[derive(Debug)]
enum SseBodyError {
    Http(reqwest::Error),
    EventCapExceeded(usize),
}

impl std::fmt::Display for SseBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(error) => write!(f, "{error}"),
            Self::EventCapExceeded(cap) => f.write_str(&event_cap_message(*cap)),
        }
    }
}

impl std::error::Error for SseBodyError {}

enum BodyState {
    Reading(reqwest::Response, EventSizeCounter),
    Fail(SseBodyError),
    Done,
}

fn is_event_cap_error(error: &SseError) -> Option<usize> {
    match error {
        SseError::Body(inner) => match inner.downcast_ref::<SseBodyError>() {
            Some(SseBodyError::EventCapExceeded(cap)) => Some(*cap),
            _ => None,
        },
        _ => None,
    }
}

/// Parse a capped SSE body. When an event exceeds the cap and `request_id`
/// names the request this stream answers, the stream first yields a JSON-RPC
/// error response for it, then the cap error, then ends.
fn capped_sse_stream(
    response: reqwest::Response,
    cap: usize,
    request_id: Option<RequestId>,
) -> BoxStream<'static, Result<Sse, SseError>> {
    // The body as a byte stream that fails once one SSE event exceeds `cap`.
    // Bytes before the offending one are still delivered so events that
    // completed in the same chunk are not lost.
    let bytes = futures::stream::unfold(
        BodyState::Reading(response, EventSizeCounter::new(cap)),
        |state| async move {
            match state {
                BodyState::Reading(mut response, mut counter) => match response.chunk().await {
                    Ok(Some(chunk)) => match counter.accept(&chunk) {
                        None => Some((Ok(chunk), BodyState::Reading(response, counter))),
                        Some(offset) => {
                            let failure = SseBodyError::EventCapExceeded(counter.cap);
                            if offset == 0 {
                                Some((Err(failure), BodyState::Done))
                            } else {
                                Some((Ok(chunk.slice(..offset)), BodyState::Fail(failure)))
                            }
                        }
                    },
                    Ok(None) => None,
                    Err(error) => Some((Err(SseBodyError::Http(error)), BodyState::Done)),
                },
                BodyState::Fail(failure) => Some((Err(failure), BodyState::Done)),
                BodyState::Done => None,
            }
        },
    );
    SseStream::from_bytes_stream(bytes)
        .flat_map(move |event| {
            let items: Vec<Result<Sse, SseError>> = match event {
                Err(error) => match (is_event_cap_error(&error), &request_id) {
                    (Some(cap), Some(id)) => {
                        let response = ServerJsonRpcMessage::error(
                            ErrorData::internal_error(event_cap_message(cap), None),
                            Some(id.clone()),
                        );
                        match serde_json::to_string(&response) {
                            Ok(data) => vec![Ok(Sse::default().data(data)), Err(error)],
                            Err(_) => vec![Err(error)],
                        }
                    }
                    _ => vec![Err(error)],
                },
                ok => vec![ok],
            };
            futures::stream::iter(items)
        })
        .scan(false, |failed, item| {
            // Nothing follows the first error.
            if *failed {
                return futures::future::ready(None);
            }
            *failed = item.is_err();
            futures::future::ready(Some(item))
        })
        .boxed()
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = reqwest::Error;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, HttpError> {
        let mut request = self
            .inner
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "))
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        let request = apply_custom_headers(request, custom_headers)?;
        let response = request.send().await.map_err(StreamableHttpError::Client)?;
        if response_code(&response) == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        let response = response
            .error_for_status()
            .map_err(StreamableHttpError::Client)?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned());
        if !starts_with_mime(content_type.as_deref(), EVENT_STREAM_MIME_TYPE)
            && !starts_with_mime(content_type.as_deref(), JSON_MIME_TYPE)
        {
            return Err(StreamableHttpError::UnexpectedContentType(content_type));
        }
        Ok(capped_sse_stream(response, self.max_body_bytes, None))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        let mut request = self.inner.delete(uri.as_ref());
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        let request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        let request = apply_custom_headers(request, custom_headers)?;
        let response = request.send().await.map_err(StreamableHttpError::Client)?;
        if response_code(&response) == StatusCode::METHOD_NOT_ALLOWED {
            tracing::debug!("MCP server does not support deleting the HTTP session");
            return Ok(());
        }
        response
            .error_for_status()
            .map_err(StreamableHttpError::Client)?;
        Ok(())
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let cap = self.max_body_bytes;
        let body = serde_json::to_vec(&message)?;
        let mut request = self
            .inner
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "))
            .header(CONTENT_TYPE, JSON_MIME_TYPE);
        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }
        let mut request = apply_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let mut response = request
            .body(body)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;
        let code = response_code(&response);
        if code == StatusCode::UNAUTHORIZED
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                header_str(header)?,
            )));
        }
        if code == StatusCode::FORBIDDEN
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let header = header_str(header)?;
            let scope = scope_from_header(&header);
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(header, scope),
            ));
        }
        if matches!(code, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if code == StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned());
        let response_session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let request_id = match &message {
            ClientJsonRpcMessage::Request(request) => Some(request.id.clone()),
            _ => None,
        };
        // Some servers answer notifications and responses with an empty 200
        // instead of 202 Accepted.
        if code.is_success() && response.content_length() == Some(0) && request_id.is_none() {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !code.is_success() {
            let body = read_body_capped(&mut response, cap)
                .await
                .unwrap_or_else(|_| CappedBody {
                    bytes: b"<failed to read response body>".to_vec(),
                    exceeded: false,
                });
            // A non-2xx response may still carry a JSON-RPC error that should
            // reach the caller as an MCP error.
            if !body.exceeded
                && starts_with_mime(content_type.as_deref(), JSON_MIME_TYPE)
                && let Some(error) = parse_json_rpc_error(&body.bytes)
            {
                return Ok(StreamableHttpPostResponse::Json(error, response_session_id));
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {code}: {}", error_body_excerpt(&body)),
            )));
        }
        if starts_with_mime(content_type.as_deref(), EVENT_STREAM_MIME_TYPE) {
            return Ok(StreamableHttpPostResponse::Sse(
                capped_sse_stream(response, cap, request_id),
                response_session_id,
            ));
        }
        if starts_with_mime(content_type.as_deref(), JSON_MIME_TYPE) {
            let body = read_body_capped(&mut response, cap)
                .await
                .map_err(StreamableHttpError::Client)?;
            if body.exceeded {
                return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                    body_cap_message(cap),
                )));
            }
            // Like rmcp: a malformed 200 body (for example to a notification)
            // is treated as accepted rather than failing the transport.
            return Ok(
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body.bytes) {
                    Ok(message) => StreamableHttpPostResponse::Json(message, response_session_id),
                    Err(error) => {
                        tracing::warn!(
                            "could not parse MCP HTTP JSON response, treating as accepted: {error}"
                        );
                        StreamableHttpPostResponse::Accepted
                    }
                },
            );
        }
        tracing::error!("unexpected MCP HTTP content type: {content_type:?}");
        Err(StreamableHttpError::UnexpectedContentType(content_type))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use rmcp::model::CallToolRequestParams;
    use rmcp::service::ServiceError;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::{
        CappedBody, EventSizeCounter, MCP_HTTP_ERROR_BODY_BYTES, MCP_HTTP_MAX_BODY_BYTES,
        error_body_excerpt, scope_from_header,
    };
    use crate::extras::mcp::client::{McpClientHandle, call_tool_bounded, list_all_tools_bounded};

    /// Longest error text a capped failure may render: the cap message or a
    /// 4 KiB body excerpt plus rmcp's and our fixed prefixes.
    const MAX_ERROR_TEXT: usize = MCP_HTTP_ERROR_BODY_BYTES + 512;

    type Respond = Arc<dyn Fn(&str, &serde_json::Value) -> Vec<u8> + Send + Sync>;

    fn http_response(status_line: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        // No Content-Length: the body runs until the connection closes, so
        // the client has to cap it while streaming.
        let mut response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn json_result(id: &serde_json::Value, result: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .unwrap()
    }

    fn oversized_json(id: &serde_json::Value) -> Vec<u8> {
        let padding = "x".repeat(MCP_HTTP_MAX_BODY_BYTES);
        json_result(id, serde_json::json!({"tools": [], "padding": padding}))
    }

    async fn read_request(stream: &mut TcpStream) -> Option<(String, serde_json::Value)> {
        let mut buffer = Vec::new();
        let header_end = loop {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
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
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
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

    async fn handle(mut stream: TcpStream, respond: Respond) {
        let Some((method, body)) = read_request(&mut stream).await else {
            return;
        };
        let response = match (method.as_str(), body.get("id")) {
            ("POST", None) => http_response("202 Accepted", "text/plain", b""),
            ("POST", Some(id)) => match body["method"].as_str() {
                Some("initialize") => http_response(
                    "200 OK",
                    "application/json",
                    &json_result(
                        id,
                        serde_json::json!({
                            "protocolVersion": body["params"]["protocolVersion"],
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "oversized", "version": "0"}
                        }),
                    ),
                ),
                Some(rpc_method) => respond(rpc_method, id),
                None => http_response("400 Bad Request", "text/plain", b""),
            },
            _ => http_response("405 Method Not Allowed", "text/plain", b""),
        };
        // The client may hang up once it hits the cap.
        let _ = stream.write_all(&response).await;
        let _ = stream.shutdown().await;
    }

    /// Loopback streamable-HTTP MCP server that answers `initialize` itself
    /// and every other request through `respond`.
    async fn serve(respond: Respond) -> Option<String> {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("loopback bind failed: {error}"),
        };
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle(stream, Arc::clone(&respond)));
            }
        });
        Some(url)
    }

    async fn connect(url: &str) -> McpClientHandle {
        McpClientHandle::connect_http_with_timeout(
            "oversized".into(),
            url,
            &HashMap::new(),
            None,
            Duration::from_secs(10),
        )
        .await
        .unwrap()
    }

    fn assert_bounded_cap_error(error: &ServiceError) {
        assert!(
            !matches!(error, ServiceError::Timeout { .. }),
            "cap must fail the request, not stall it: {error}"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains(&MCP_HTTP_MAX_BODY_BYTES.to_string()),
            "error should name the cap: {rendered}"
        );
        assert!(rendered.len() <= MAX_ERROR_TEXT, "{}", rendered.len());
    }

    fn call_params() -> CallToolRequestParams {
        serde_json::from_value(serde_json::json!({"name": "probe", "arguments": {}})).unwrap()
    }

    #[tokio::test]
    async fn oversized_json_response_fails_tools_list_with_bounded_error() {
        let Some(url) = serve(Arc::new(|_, id| {
            http_response("200 OK", "application/json", &oversized_json(id))
        }))
        .await
        else {
            return;
        };
        let handle = connect(&url).await;

        let error = list_all_tools_bounded(&handle.peer(), Duration::from_secs(20))
            .await
            .unwrap_err();

        assert_bounded_cap_error(&error);
    }

    #[tokio::test]
    async fn oversized_sse_event_fails_tools_call_with_bounded_error() {
        let Some(url) = serve(Arc::new(|_, id| {
            let mut event = b"data: ".to_vec();
            event.extend_from_slice(&oversized_json(id));
            event.extend_from_slice(b"\n\n");
            http_response("200 OK", "text/event-stream", &event)
        }))
        .await
        else {
            return;
        };
        let handle = connect(&url).await;

        let error = call_tool_bounded(&handle.peer(), call_params(), Duration::from_secs(20))
            .await
            .unwrap_err();

        assert!(matches!(error, ServiceError::McpError(_)), "{error}");
        assert_bounded_cap_error(&error);
    }

    #[tokio::test]
    async fn non_success_body_is_truncated_before_entering_the_error() {
        let Some(url) = serve(Arc::new(|_, _| {
            http_response(
                "500 Internal Server Error",
                "text/plain",
                &vec![b'e'; 1024 * 1024],
            )
        }))
        .await
        else {
            return;
        };
        let handle = connect(&url).await;

        let error = call_tool_bounded(&handle.peer(), call_params(), Duration::from_secs(20))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("HTTP 500"), "{error}");
        assert!(error.contains("truncated"), "{error}");
        assert!(error.len() <= MAX_ERROR_TEXT, "{}", error.len());
    }

    #[tokio::test]
    async fn responses_within_the_cap_pass_through_json_and_sse() {
        let Some(url) = serve(Arc::new(|method, id| match method {
            "tools/list" => http_response(
                "200 OK",
                "application/json",
                &json_result(
                    id,
                    serde_json::json!({"tools": [{"name": "probe", "inputSchema": {"type": "object"}}]}),
                ),
            ),
            _ => {
                let result = json_result(
                    id,
                    serde_json::json!({"content": [{"type": "text", "text": "ok"}]}),
                );
                // A comment and a notification event first, then the
                // response, all with CRLF terminators.
                let mut body = b": keep-alive\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"hi\"}}\r\n\r\ndata: ".to_vec();
                body.extend_from_slice(&result);
                body.extend_from_slice(b"\r\n\r\n");
                http_response("200 OK", "text/event-stream", &body)
            }
        }))
        .await
        else {
            return;
        };
        let handle = connect(&url).await;

        let tools = list_all_tools_bounded(&handle.peer(), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        let result = call_tool_bounded(&handle.peer(), call_params(), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn event_counter_resets_at_blank_lines_for_every_terminator() {
        let mut counter = EventSizeCounter::new(8);
        for separator in [&b"\n\n"[..], b"\r\n\r\n", b"\r\r"] {
            for _ in 0..100 {
                assert_eq!(counter.accept(b"data: x"), None);
                assert_eq!(counter.accept(separator), None);
            }
        }
        // Terminators split across chunks.
        assert_eq!(counter.accept(b"data: x\r"), None);
        assert_eq!(counter.accept(b"\n\r"), None);
        assert_eq!(counter.accept(b"\ndata: x\n"), None);
        assert_eq!(counter.accept(b"\n"), None);

        // Many short lines without a blank line still form one event.
        let mut counter = EventSizeCounter::new(8);
        assert_eq!(counter.accept(b"a\nb\nc\nd\n"), None);
        assert_eq!(counter.accept(b"e\n"), Some(0));

        let mut counter = EventSizeCounter::new(4);
        assert_eq!(counter.accept(b"\n\n0123456789"), Some(6));
    }

    #[test]
    fn error_excerpt_is_bounded_and_utf8_safe() {
        let long = CappedBody {
            bytes: "é".repeat(MCP_HTTP_ERROR_BODY_BYTES).into_bytes(),
            exceeded: false,
        };
        let excerpt = error_body_excerpt(&long);
        assert!(excerpt.len() <= MCP_HTTP_ERROR_BODY_BYTES + 64);
        assert!(excerpt.ends_with(&format!(
            "[body truncated to {MCP_HTTP_ERROR_BODY_BYTES} bytes]"
        )));

        let short = CappedBody {
            bytes: b"bad gateway".to_vec(),
            exceeded: false,
        };
        assert_eq!(error_body_excerpt(&short), "bad gateway");

        let over_cap = CappedBody {
            bytes: Vec::new(),
            exceeded: true,
        };
        assert!(error_body_excerpt(&over_cap).contains("truncated"));
    }

    #[test]
    fn scope_is_read_from_quoted_and_bare_parameters() {
        assert_eq!(
            scope_from_header(
                r#"Bearer error="insufficient_scope", scope="files:read files:write""#
            ),
            Some("files:read files:write".to_string())
        );
        assert_eq!(
            scope_from_header("Bearer scope=read:data, error=x"),
            Some("read:data".to_string())
        );
        assert_eq!(scope_from_header("Bearer realm=x"), None);
    }
}
