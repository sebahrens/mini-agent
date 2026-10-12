use crate::http_client::sse::BoxedStream;
use bytes::Bytes;
pub use http::{HeaderMap, HeaderValue, Method, Request, Response, Uri, request::Builder};
use http::{HeaderName, StatusCode};
use reqwest::Body;
pub mod multipart;
pub mod retry;
pub mod sse;
use crate::wasm_compat::*;
pub use multipart::MultipartForm;
pub use reqwest::Client as ReqwestClient;
use std::pin::Pin;

/// Compile-time contract for mini-agent’s response-source preservation backport.
pub const MINI_AGENT_HTTP_RESPONSE_SOURCE_CHAIN: bool = true;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Http error: {0}")]
    Protocol(#[from] http::Error),
    #[error("Invalid status code: {0}")]
    InvalidStatusCode(StatusCode),
    #[error("Invalid status code {0} with message: {1}")]
    InvalidStatusCodeWithMessage(StatusCode, String),
    #[error("Header value outside of legal range: {0}")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("Request in error state, cannot access headers")]
    NoHeaders,
    #[error("Stream ended")]
    StreamEnded,
    #[error("Invalid content type was returned: {0:?}")]
    InvalidContentType(HeaderValue),
    #[cfg(not(target_family = "wasm"))]
    #[error("Http client error: {0}")]
    Instance(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

    #[cfg(target_family = "wasm")]
    #[error("Http client error: {0}")]
    Instance(#[from] Box<dyn std::error::Error + 'static>),
}

impl Error {
    pub(crate) fn non_success_status(&self) -> Option<StatusCode> {
        match self {
            Self::InvalidStatusCode(status) | Self::InvalidStatusCodeWithMessage(status, _) => {
                Some(*status)
            }
            Self::Instance(error) => nested_http_response(error.as_ref())
                .and_then(|response| response.non_success_status()),
            _ => None,
        }
    }

    pub(crate) fn non_success_body(&self) -> Option<&str> {
        match self {
            Self::InvalidStatusCodeWithMessage(_, body) => Some(body.as_str()),
            Self::Instance(error) => nested_http_response(error.as_ref())
                .and_then(|response| response.non_success_body()),
            _ => None,
        }
    }
}

// mini-agent backport: error metadata wrappers keep their canonical HTTP
// response as a source. Recognize that response before streaming conversion
// would flatten the wrapper to text. The bound handles cyclic/malicious Error
// implementations; do not recurse through another Instance after finding it.
fn nested_http_response<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a Error> {
    let mut current = Some(error);
    for _ in 0..32 {
        let candidate = current?;
        if let Some(response) = candidate.downcast_ref::<Error>()
            && matches!(
                response,
                Error::InvalidStatusCode(_) | Error::InvalidStatusCodeWithMessage(..)
            )
        {
            return Some(response);
        }
        current = candidate.source();
    }
    None
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(not(target_family = "wasm"))]
pub(crate) fn instance_error<E: std::error::Error + Send + Sync + 'static>(error: E) -> Error {
    Error::Instance(error.into())
}

#[cfg(target_family = "wasm")]
fn instance_error<E: std::error::Error + 'static>(error: E) -> Error {
    Error::Instance(error.into())
}

async fn non_success_status_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let message = response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
    Error::InvalidStatusCodeWithMessage(status, message)
}

pub type LazyBytes = WasmBoxedFuture<'static, Result<Bytes>>;
pub type LazyBody<T> = WasmBoxedFuture<'static, Result<T>>;

pub type StreamingResponse = Response<BoxedStream>;

#[derive(Debug, Clone, Copy)]
pub struct NoBody;

impl From<NoBody> for Bytes {
    fn from(_: NoBody) -> Self {
        Bytes::new()
    }
}

impl From<NoBody> for Body {
    fn from(_: NoBody) -> Self {
        reqwest::Body::default()
    }
}

pub async fn text(response: Response<LazyBody<Vec<u8>>>) -> Result<String> {
    let text = response.into_body().await?;
    Ok(String::from(String::from_utf8_lossy(&text)))
}

pub fn make_auth_header(key: impl AsRef<str>) -> Result<(HeaderName, HeaderValue)> {
    Ok((
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.as_ref()))?,
    ))
}

pub fn bearer_auth_header(headers: &mut HeaderMap, key: impl AsRef<str>) -> Result<()> {
    let (k, v) = make_auth_header(key)?;

    headers.insert(k, v);

    Ok(())
}

pub fn with_bearer_auth(mut req: Builder, auth: &str) -> Result<Builder> {
    bearer_auth_header(req.headers_mut().ok_or(Error::NoHeaders)?, auth)?;

    Ok(req)
}

/// A helper trait to make generic requests (both regular and SSE) possible.
pub trait HttpClientExt: WasmCompatSend + WasmCompatSync {
    /// Send a HTTP request, get a response back (as bytes). Response must be able to be turned back into Bytes.
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        T: WasmCompatSend,
        U: From<Bytes>,
        U: WasmCompatSend + 'static;

    /// Send a HTTP request with a multipart body, get a response back (as bytes). Response must be able to be turned back into Bytes (although usually for the response, you will probably want to specify Bytes anyway).
    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static;

    /// Send a HTTP request, get a streamed response back (as a stream of [`bytes::Bytes`].)
    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend;
}

impl HttpClientExt for reqwest::Client {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        U: From<Bytes> + WasmCompatSend,
    {
        let (parts, body) = req.into_parts();
        let req = self
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body.into());

        async move {
            let response = req.send().await.map_err(instance_error)?;
            if !response.status().is_success() {
                return Err(non_success_status_error(response).await);
            }

            let mut res = Response::builder().status(response.status());

            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }

            let body: LazyBody<U> = Box::pin(async {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| Error::Instance(e.into()))?;

                let body = U::from(bytes);
                Ok(body)
            });

            res.body(body).map_err(Error::Protocol)
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        let (parts, body) = req.into_parts();
        let body = reqwest::multipart::Form::from(body);

        let req = self
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .multipart(body);

        async move {
            let response = req.send().await.map_err(instance_error)?;
            if !response.status().is_success() {
                return Err(non_success_status_error(response).await);
            }

            let mut res = Response::builder().status(response.status());

            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }

            let body: LazyBody<U> = Box::pin(async {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| Error::Instance(e.into()))?;

                let body = U::from(bytes);
                Ok(body)
            });

            res.body(body).map_err(Error::Protocol)
        }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let (parts, body) = req.into_parts();

        let client = self.clone();

        async move {
            let req = self
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body.into())
                .build()
                .map_err(|error| Error::Instance(error.into()))?;
            let response: reqwest::Response = client.execute(req).await.map_err(instance_error)?;
            if !response.status().is_success() {
                return Err(non_success_status_error(response).await);
            }

            #[cfg(not(target_family = "wasm"))]
            let mut res = Response::builder()
                .status(response.status())
                .version(response.version());

            #[cfg(target_family = "wasm")]
            let mut res = Response::builder().status(response.status());

            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }

            use futures::StreamExt;

            let mapped_stream: Pin<Box<dyn WasmCompatSendStream<InnerItem = Result<Bytes>>>> =
                Box::pin(
                    response
                        .bytes_stream()
                        .map(|chunk| chunk.map_err(|e| Error::Instance(Box::new(e)))),
                );

            res.body(mapped_stream).map_err(Error::Protocol)
        }
    }
}

#[cfg(feature = "reqwest-middleware")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest-middleware")))]
impl HttpClientExt for reqwest_middleware::ClientWithMiddleware {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        U: From<Bytes> + WasmCompatSend,
    {
        let (parts, body) = req.into_parts();
        let req = self
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body.into());

        async move {
            let response = req.send().await.map_err(instance_error)?;
            if !response.status().is_success() {
                return Err(non_success_status_error(response).await);
            }

            let mut res = Response::builder().status(response.status());

            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }

            let body: LazyBody<U> = Box::pin(async {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| Error::Instance(e.into()))?;

                let body = U::from(bytes);
                Ok(body)
            });

            res.body(body).map_err(Error::Protocol)
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        let (parts, body) = req.into_parts();
        let body = reqwest::multipart::Form::from(body);

        let req = self
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .multipart(body);

        async move {
            let response = req.send().await.map_err(instance_error)?;
            if !response.status().is_success() {
                return Err(non_success_status_error(response).await);
            }

            let mut res = Response::builder().status(response.status());

            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }

            let body: LazyBody<U> = Box::pin(async {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| Error::Instance(e.into()))?;

                let body = U::from(bytes);
                Ok(body)
            });

            res.body(body).map_err(Error::Protocol)
        }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let (parts, body) = req.into_parts();

        let client = self.clone();

        async move {
            let req = self
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body.into())
                .build()
                .map_err(|error| Error::Instance(error.into()))?;
            let response: reqwest::Response = client.execute(req).await.map_err(instance_error)?;
            if !response.status().is_success() {
                return Err(non_success_status_error(response).await);
            }

            #[cfg(not(target_family = "wasm"))]
            let mut res = Response::builder()
                .status(response.status())
                .version(response.version());

            #[cfg(target_family = "wasm")]
            let mut res = Response::builder().status(response.status());

            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }

            use futures::StreamExt;

            let mapped_stream: Pin<Box<dyn WasmCompatSendStream<InnerItem = Result<Bytes>>>> =
                Box::pin(
                    response
                        .bytes_stream()
                        .map(|chunk| chunk.map_err(|e| Error::Instance(Box::new(e)))),
                );

            res.body(mapped_stream).map_err(Error::Protocol)
        }
    }
}

#[cfg(test)]
mod metadata_source_tests {
    use super::*;
    use std::error::Error as StdError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct Wrapper(Box<dyn StdError + Send + Sync>);
    impl std::fmt::Display for Wrapper {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "request metadata")
        }
    }
    impl StdError for Wrapper {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(self.0.as_ref())
        }
    }

    #[test]
    fn request_owned_wrapper_preserves_canonical_http_status_and_body() {
        let error = Error::Instance(Box::new(Wrapper(Box::new(
            Error::InvalidStatusCodeWithMessage(
                StatusCode::TOO_MANY_REQUESTS,
                "actual provider body".into(),
            ),
        ))));
        assert_eq!(
            error.non_success_status(),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
        assert_eq!(error.non_success_body(), Some("actual provider body"));
        let completion = crate::completion::CompletionError::from_stream_transport(error);
        assert!(matches!(
            completion,
            crate::completion::CompletionError::HttpError(_)
        ));
        assert_eq!(
            completion.provider_response_status(),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
        assert_eq!(
            completion.provider_response_body(),
            Some("actual provider body")
        );
        assert!(
            completion
                .source()
                .unwrap()
                .source()
                .unwrap()
                .downcast_ref::<Wrapper>()
                .is_some()
        );
    }

    #[test]
    fn non_http_transport_wrapper_retains_upstream_classification() {
        let error = Error::Instance(Box::new(Wrapper(Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "transport timeout",
        )))));
        assert_eq!(error.non_success_status(), None);
        assert_eq!(error.non_success_body(), None);
        let completion = crate::completion::CompletionError::from_stream_transport(error);
        assert!(matches!(
            completion,
            crate::completion::CompletionError::ProviderError(_)
        ));
    }

    #[derive(Debug, Default)]
    struct Cycle(AtomicUsize);
    impl std::fmt::Display for Cycle {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "cycle")
        }
    }
    impl StdError for Cycle {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Some(self)
        }
    }

    #[test]
    fn cyclic_and_excessive_source_chains_are_bounded() {
        let cycle = Cycle::default();
        assert!(nested_http_response(&cycle).is_none());
        assert!(cycle.0.load(Ordering::Relaxed) <= 32);
        let mut bounded =
            Error::InvalidStatusCodeWithMessage(StatusCode::SERVICE_UNAVAILABLE, "body".into());
        for _ in 0..31 {
            bounded = Error::Instance(Box::new(bounded));
        }
        assert_eq!(
            bounded.non_success_status(),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        let mut excessive = Error::InvalidStatusCode(StatusCode::SERVICE_UNAVAILABLE);
        for _ in 0..34 {
            excessive = Error::Instance(Box::new(excessive));
        }
        assert_eq!(excessive.non_success_status(), None);
    }
}
