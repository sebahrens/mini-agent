//! DNS-rebinding defence for user-configured URL MCP servers.
//!
//! [`super::client`] refuses a URL server whose host resolves to a loopback,
//! private, link-local or otherwise non-public address. Checking one lookup is
//! not enough: reqwest resolves the host again for every new connection (the
//! first connect, each SSE reconnect) and the OAuth manager talks to hosts of
//! its own, so a name that answered with a public address during validation
//! could answer `127.0.0.1` or `169.254.169.254` a moment later.
//!
//! * The streamable-HTTP transport and the OAuth client pin the validated
//!   server host to the addresses that passed validation
//!   ([`ValidatedEndpoint::pin`], `ClientBuilder::resolve_to_addrs`), so they
//!   never look it up again.
//! * Every other name those clients resolve (an OAuth authorization server on
//!   another host, a redirect target) goes through [`PublicOnlyResolver`],
//!   which refuses a lookup that yields any non-public address.
//! * A literal-IP URL needs no lookup; the OAuth client checks it before each
//!   request and each redirect hop.
//!
//! A configured HTTP(S) proxy resolves names itself, so neither pinning nor
//! the resolver applies to proxied requests.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use rmcp::transport::auth::{
    OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRedirectPolicy,
    OAuthHttpRequest,
};

use super::client::{parse_mcp_server_url, validate_resolved_addresses};

/// Whole-request bound for one OAuth HTTP operation when rmcp names none
/// (rmcp's own default).
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest OAuth HTTP response body accepted (rmcp's own cap).
const OAUTH_HTTP_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Most redirect hops the OAuth client follows (reqwest's default limit).
const OAUTH_HTTP_MAX_REDIRECTS: usize = 10;

type LookupFuture = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// Name resolution used for validation and by [`PublicOnlyResolver`]:
/// the system resolver in production, a fixture in tests.
pub(crate) type Lookup = Arc<dyn Fn(String) -> LookupFuture + Send + Sync>;

/// The system resolver, run on the blocking pool (owned by the current agent
/// work scope when there is one).
pub(crate) fn system_lookup() -> Lookup {
    Arc::new(|host: String| {
        Box::pin(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                (host.as_str(), 0)
                    .to_socket_addrs()
                    .map(|addresses| addresses.map(|address| address.ip()).collect())
            })
            .await
            .map_err(io::Error::other)?
        })
    })
}

/// A URL MCP server host together with the addresses that passed validation.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedEndpoint {
    host: String,
    addresses: Vec<SocketAddr>,
    literal: bool,
    local_origin: Option<reqwest::Url>,
}

impl ValidatedEndpoint {
    pub(super) fn new(host: String, addresses: &[IpAddr], literal: bool) -> Self {
        Self {
            host,
            // Port 0: reqwest connects to the port the URL names.
            addresses: addresses
                .iter()
                .map(|address| SocketAddr::new(*address, 0))
                .collect(),
            literal,
            local_origin: None,
        }
    }

    pub(super) fn allow_local_origin(&mut self, url: reqwest::Url) {
        self.local_origin = Some(url);
    }

    fn accepts_local_origin(&self, url: &reqwest::Url) -> bool {
        self.local_origin
            .as_ref()
            .is_some_and(|origin| origin.origin() == url.origin())
    }

    /// The addresses the server host is pinned to.
    #[cfg(test)]
    pub(crate) fn addresses(&self) -> Vec<IpAddr> {
        self.addresses.iter().map(SocketAddr::ip).collect()
    }

    /// Pins the validated host to its validated addresses and routes every
    /// other name through [`PublicOnlyResolver`].
    pub(crate) fn pin(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        self.pin_with(builder, system_lookup())
    }

    fn pin_with(&self, builder: reqwest::ClientBuilder, lookup: Lookup) -> reqwest::ClientBuilder {
        self.pin_with_source(builder, lookup, None)
    }

    fn pin_with_source(
        &self,
        builder: reqwest::ClientBuilder,
        lookup: Lookup,
        source: Option<SourceValidator>,
    ) -> reqwest::ClientBuilder {
        let builder = if self.local_origin.is_some() {
            builder.no_proxy()
        } else {
            builder
        };
        let mut resolver = PublicOnlyResolver::new(lookup);
        resolver.source = source;
        let builder = builder.dns_resolver(resolver);
        if self.literal {
            builder
        } else {
            builder.resolve_to_addrs(&self.host, &self.addresses)
        }
    }
}

/// A reqwest resolver that refuses any lookup yielding a loopback, private,
/// link-local or otherwise non-public address, so a name cannot be rebound
/// to an internal service between validation and connect.
#[derive(Clone)]
pub(crate) struct PublicOnlyResolver {
    lookup: Lookup,
    source: Option<SourceValidator>,
}

/// An owned source check for an explicit login; never a permission grant.
/// Failures are static so raw definitions and credentials cannot enter errors.
pub(crate) type SourceValidator = Arc<dyn Fn() -> bool + Send + Sync>;

fn source_is_current(source: &Option<SourceValidator>) -> bool {
    source.as_ref().is_none_or(|validate| validate())
}

impl PublicOnlyResolver {
    pub(crate) fn new(lookup: Lookup) -> Self {
        Self {
            lookup,
            source: None,
        }
    }
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let lookup = self.lookup.clone();
        let host = name.as_str().to_owned();
        let source = self.source.clone();
        Box::pin(async move {
            if !source_is_current(&source) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "MCP OAuth source changed",
                )
                .into());
            }
            let addresses = lookup(host.clone()).await?;
            if !source_is_current(&source) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "MCP OAuth source changed",
                )
                .into());
            }
            validate_resolved_addresses(&addresses).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("MCP host '{host}' refused: {error}"),
                )
            })?;
            let addresses: reqwest::dns::Addrs = Box::new(
                addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address, 0)),
            );
            Ok(addresses)
        })
    }
}

/// Refuses a URL whose host is a local name or a literal non-public
/// address. Names are left to [`PublicOnlyResolver`].
fn check_url_host(url: &reqwest::Url) -> Result<(), String> {
    let (_, _, literal) = parse_mcp_server_url(url.as_str()).map_err(|error| error.to_string())?;
    if let Some(address) = literal {
        validate_resolved_addresses(&[address]).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// rmcp OAuth HTTP client whose every request, redirect hop and name lookup
/// is held to the same public-address policy as the MCP transport.
pub(crate) struct GuardedOAuthHttpClient {
    /// Never follows redirects itself; [`Self::send`] walks them by hand so one
    /// connection pool and TLS context serve both redirect policies.
    client: reqwest::Client,
    endpoint: ValidatedEndpoint,
    source: Option<SourceValidator>,
}

impl GuardedOAuthHttpClient {
    pub(crate) fn new(endpoint: &ValidatedEndpoint) -> anyhow::Result<Self> {
        Self::with_lookup(endpoint, system_lookup())
    }

    pub(crate) fn with_source_validator(
        endpoint: &ValidatedEndpoint,
        source: SourceValidator,
    ) -> anyhow::Result<Self> {
        Self::with_lookup_and_source(endpoint, system_lookup(), Some(source))
    }

    fn with_lookup(endpoint: &ValidatedEndpoint, lookup: Lookup) -> anyhow::Result<Self> {
        Self::with_lookup_and_source(endpoint, lookup, None)
    }

    fn with_lookup_and_source(
        endpoint: &ValidatedEndpoint,
        lookup: Lookup,
        source: Option<SourceValidator>,
    ) -> anyhow::Result<Self> {
        let client = endpoint
            .pin_with_source(reqwest::Client::builder(), lookup, source.clone())
            .timeout(OAUTH_HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                anyhow::anyhow!("MCP OAuth HTTP client construction failed: {error}")
            })?;
        Ok(Self {
            client,
            endpoint: endpoint.clone(),
            source,
        })
    }
}

impl GuardedOAuthHttpClient {
    async fn send(
        &self,
        request: http::Request<Vec<u8>>,
        follow_redirects: bool,
        timeout: Option<Duration>,
    ) -> Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
        if !source_is_current(&self.source) {
            return Err(OAuthHttpClientError::new("MCP OAuth source changed"));
        }
        let mut request = reqwest::Request::try_from(request)
            .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
        if !self.endpoint.accepts_local_origin(request.url()) {
            check_url_host(request.url()).map_err(OAuthHttpClientError::new)?;
        } else if !request.url().username().is_empty() || request.url().password().is_some() {
            return Err(OAuthHttpClientError::new(
                "MCP URL must not contain credentials",
            ));
        }
        // A local-origin endpoint never follows redirects.
        let follow_redirects = follow_redirects && self.endpoint.local_origin.is_none();
        // One whole-operation budget across every hop, as reqwest applies it.
        let deadline = tokio::time::Instant::now() + timeout.unwrap_or(OAUTH_HTTP_TIMEOUT);
        let mut hops = 0usize;
        let response = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            *request.timeout_mut() = Some(remaining);
            let next = if follow_redirects {
                request.try_clone()
            } else {
                None
            };
            let response = self
                .client
                .execute(request)
                .await
                .map_err(|error| OAuthHttpClientError::new(error_chain(&error)))?;
            let Some(next) = next else { break response };
            let Some((code, target)) = redirect_target(&response) else {
                break response;
            };
            if !source_is_current(&self.source) {
                return Err(OAuthHttpClientError::new("MCP OAuth source changed"));
            }
            // `previous().len()` of the equivalent redirect policy is hops + 1.
            if hops + 1 >= OAUTH_HTTP_MAX_REDIRECTS {
                return Err(OAuthHttpClientError::new("too many redirects"));
            }
            check_url_host(&target).map_err(OAuthHttpClientError::new)?;
            request = redirected_request(next, code, target);
            hops += 1;
        };
        if !source_is_current(&self.source) {
            return Err(OAuthHttpClientError::new("MCP OAuth source changed"));
        }

        let mut builder = http::Response::builder()
            .status(response.status())
            .version(response.version());
        for (name, value) in response.headers() {
            builder = builder.header(name, value);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
            if chunk.len() > OAUTH_HTTP_MAX_BODY_BYTES - body.len() {
                return Err(OAuthHttpClientError::new(format!(
                    "OAuth HTTP response body exceeds {OAUTH_HTTP_MAX_BODY_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        if !source_is_current(&self.source) {
            return Err(OAuthHttpClientError::new("MCP OAuth source changed"));
        }
        builder
            .body(body)
            .map_err(|error| OAuthHttpClientError::new(error.to_string()))
    }
}

impl OAuthHttpClient for GuardedOAuthHttpClient {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let OAuthHttpRequest {
                request,
                redirect_policy,
                timeout,
                ..
            } = request;
            let follow = matches!(redirect_policy, OAuthHttpRedirectPolicy::Follow);
            self.send(request, follow, timeout).await
        })
    }
}

/// The absolute `Location` of a redirect status reqwest itself would follow,
/// and `None` for any other response (including an unusable `Location`).
fn redirect_target(response: &reqwest::Response) -> Option<(reqwest::StatusCode, reqwest::Url)> {
    use reqwest::StatusCode;
    let code = response.status();
    if !matches!(
        code,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    ) {
        return None;
    }
    let location = response.headers().get(reqwest::header::LOCATION)?;
    Some((code, response.url().join(location.to_str().ok()?).ok()?))
}

/// The request for the next hop, with reqwest's redirect semantics: 301/302/303
/// become a body-less GET (HEAD stays HEAD), 307/308 keep method and body, and
/// credentials never cross to another origin.
fn redirected_request(
    mut next: reqwest::Request,
    code: reqwest::StatusCode,
    target: reqwest::Url,
) -> reqwest::Request {
    use reqwest::header;
    use reqwest::{Method, StatusCode};
    let cross_origin = next.url().host_str() != target.host_str()
        || next.url().port_or_known_default() != target.port_or_known_default();
    let drops_body = matches!(
        code,
        StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
    ) && *next.method() != Method::HEAD
        && (code == StatusCode::SEE_OTHER || *next.method() == Method::POST);
    if drops_body {
        *next.method_mut() = Method::GET;
        *next.body_mut() = None;
        for name in [
            header::TRANSFER_ENCODING,
            header::CONTENT_ENCODING,
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
        ] {
            next.headers_mut().remove(name);
        }
    }
    if cross_origin {
        for name in [
            header::AUTHORIZATION,
            header::COOKIE,
            header::PROXY_AUTHORIZATION,
            header::WWW_AUTHENTICATE,
        ] {
            next.headers_mut().remove(name);
        }
    }
    *next.url_mut() = target;
    next
}

/// An error and its sources on one line, so a refusal from the resolver or
/// the redirect policy is visible in the message rmcp reports.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const PUBLIC: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    /// A lookup that answers each call from `answers` in turn (repeating the
    /// last one) and counts its calls.
    fn scripted_lookup(answers: Vec<IpAddr>) -> (Lookup, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let lookup: Lookup = Arc::new(move |_host: String| {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            let answer = answers[call.min(answers.len() - 1)];
            Box::pin(async move { Ok(vec![answer]) })
        });
        (lookup, calls)
    }

    /// A loopback HTTP server that answers every request with `response`
    /// and counts the connections it accepted.
    fn loopback_server(response: &'static str) -> Option<(u16, Arc<AtomicUsize>)> {
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("loopback bind failed: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Some((port, accepted))
    }

    #[tokio::test]
    async fn a_name_rebound_to_loopback_after_validation_is_refused() {
        let Some((port, accepted)) =
            loopback_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        else {
            return;
        };
        // The first lookup (validation) answers a public address, every later
        // one answers loopback: a DNS-rebinding attack.
        let (lookup, calls) = scripted_lookup(vec![PUBLIC, LOOPBACK]);
        let url = format!("http://rebind.example:{port}/mcp");
        let endpoint = super::super::client::validate_mcp_server_url_with(&url, &lookup)
            .await
            .expect("the first answer is public");
        assert_eq!(endpoint.addresses(), vec![PUBLIC]);

        // Any client that resolves the name again (here: one not pinned to
        // the validated host) meets the rebound answer and must refuse it.
        let other = ValidatedEndpoint::new("unrelated.example".into(), &[PUBLIC], false);
        let client = other
            .pin_with(reqwest::Client::builder(), lookup)
            .build()
            .unwrap();
        let error = client.get(&url).send().await.unwrap_err();
        let message = error_chain(&error);
        assert!(
            message.contains("non-public address 127.0.0.1"),
            "{message}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            0,
            "no connection may reach loopback"
        );
    }

    #[tokio::test]
    async fn a_pinned_host_is_never_resolved_again() {
        let Some((port, accepted)) =
            loopback_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        else {
            return;
        };
        // Pinning is what a validated endpoint gets; pin to the loopback
        // fixture so the test observes where the client really connects.
        let (lookup, calls) = scripted_lookup(vec![PUBLIC]);
        let endpoint = ValidatedEndpoint::new("pinned.example".into(), &[LOOPBACK], false);
        let client = endpoint
            .pin_with(reqwest::Client::builder(), lookup)
            .build()
            .unwrap();
        for _ in 0..2 {
            let response = client
                .get(format!("http://pinned.example:{port}/"))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a pinned host is not looked up"
        );
        assert!(accepted.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn oauth_client_refuses_literal_internal_targets_and_redirects() {
        let (lookup, _) = scripted_lookup(vec![PUBLIC]);
        let endpoint = ValidatedEndpoint::new("auth.example".into(), &[LOOPBACK], false);
        let client = GuardedOAuthHttpClient::with_lookup(&endpoint, lookup).unwrap();
        let get = |uri: String| http::Request::get(uri).body(Vec::new()).unwrap();

        let error = client
            .send(
                get("http://169.254.169.254/latest/meta-data".into()),
                true,
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("non-public address"), "{error}");

        let Some((port, _)) = loopback_server(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n",
        ) else {
            return;
        };
        let error = client
            .send(
                get(format!("http://auth.example:{port}/.well-known/x")),
                true,
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("non-public address"), "{error}");
        // Without redirects the 302 itself is returned for rmcp to inspect.
        let response = client
            .send(
                get(format!("http://auth.example:{port}/token")),
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::FOUND);
    }

    #[tokio::test]
    async fn oauth_client_follows_redirects_by_hand_with_one_client_and_a_hop_limit() {
        let Some((final_port, final_hits)) =
            loopback_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        else {
            return;
        };
        let (lookup, _) = scripted_lookup(vec![PUBLIC]);
        let endpoint = ValidatedEndpoint::new("auth.example".into(), &[LOOPBACK], false);
        let client = GuardedOAuthHttpClient::with_lookup(&endpoint, lookup).unwrap();

        // A 302 to another port of the pinned (non-literal) host is followed,
        // and a POST becomes a body-less GET.
        let redirect: &'static str = Box::leak(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://auth.example:{final_port}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let (port, first_hits) = loopback_server(redirect).unwrap();
        let post = http::Request::post(format!("http://auth.example:{port}/start"))
            .body(b"payload".to_vec())
            .unwrap();
        let response = client.send(post, true, None).await.unwrap();
        assert_eq!(response.body(), b"ok");
        assert_eq!(first_hits.load(Ordering::SeqCst), 1);
        assert_eq!(final_hits.load(Ordering::SeqCst), 1);

        // A redirect loop stops at the same limit the reqwest policy enforced.
        let (loop_port, loop_hits) = loopback_server(Box::leak(
            "HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string()
                .into_boxed_str(),
        ))
        .unwrap();
        let error = client
            .send(
                http::Request::get(format!("http://auth.example:{loop_port}/"))
                    .body(Vec::new())
                    .unwrap(),
                true,
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("too many redirects"), "{error}");
        assert_eq!(loop_hits.load(Ordering::SeqCst), OAUTH_HTTP_MAX_REDIRECTS);
    }
}

#[cfg(test)]
mod source_revision_tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    struct OwnedHttp {
        url: String,
        requests: Arc<std::sync::atomic::AtomicUsize>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        task: Option<tokio::task::JoinHandle<()>>,
    }
    impl OwnedHttp {
        async fn new(revoke: Option<std::path::PathBuf>) -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = requests.clone();
            let (stop, mut stopping) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! { biased; _ = &mut stopping => break, accepted = listener.accept() => accepted.unwrap() };
                    let (mut stream, _) = accepted;
                    tokio::time::timeout(Duration::from_secs(2), async {
                        let mut bytes = Vec::new();
                        let mut chunk = [0u8; 512];
                        while !bytes.windows(4).any(|row| row == b"\r\n\r\n") {
                            let count = stream.read(&mut chunk).await.unwrap();
                            if count == 0 { return; }
                            bytes.extend_from_slice(&chunk[..count]);
                            assert!(bytes.len() <= 4096);
                        }
                        counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        if let Some(path) = &revoke {
                            crate::fs::private_atomic_write_sync(path, b"{\"mcpServers\":{},\"revision\":2}").unwrap();
                        }
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await;
                    }).await.expect("owned HTTP socket must be bounded");
                }
            });
            Self {
                url: format!("http://{address}"),
                requests,
                stop: Some(stop),
                task: Some(task),
            }
        }
        async fn finish(mut self) {
            self.stop.take().unwrap().send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(3), self.task.take().unwrap())
                .await
                .unwrap()
                .unwrap();
        }
    }
    impl Drop for OwnedHttp {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }

    #[tokio::test]
    async fn oauth_source_changed_during_metadata_refuses_registration_and_keeps_wire_positive() {
        for revoke in [false, true] {
            let root = crate::tests::OwnedTestDirectory::new("mcp-oauth-source-wire");
            let path = root.path().join(".mcp.json");
            crate::fs::private_atomic_create_sync(&path, b"{\"mcpServers\":{}}").unwrap();
            let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let before = super::super::project::FileSnapshot::read(
                &workspace,
                std::path::Path::new(".mcp.json"),
            )
            .unwrap()
            .content;
            let source = workspace.clone();
            let guard: SourceValidator = Arc::new(move || {
                super::super::project::FileSnapshot::read(
                    &source,
                    std::path::Path::new(".mcp.json"),
                )
                .is_ok_and(|snapshot| snapshot.content == before)
            });
            let server = OwnedHttp::new(revoke.then_some(path.clone())).await;
            let endpoint = super::super::client::validate_mcp_server_url_policy(&server.url, true)
                .await
                .unwrap();
            let client = GuardedOAuthHttpClient::with_source_validator(&endpoint, guard).unwrap();
            let metadata = client
                .send(
                    http::Request::builder()
                        .uri(format!("{}/metadata", server.url))
                        .body(Vec::new())
                        .unwrap(),
                    false,
                    Some(Duration::from_secs(2)),
                )
                .await;
            assert_eq!(
                metadata.is_err(),
                revoke,
                "metadata response must not advance a stale source"
            );
            assert_eq!(
                server.requests.load(std::sync::atomic::Ordering::Acquire),
                1
            );
            if !revoke {
                crate::fs::private_atomic_write_sync(&path, b"{\"mcpServers\":{},\"revision\":2}")
                    .unwrap();
            }
            let registration = client
                .send(
                    http::Request::builder()
                        .method("POST")
                        .uri(format!("{}/register", server.url))
                        .body(Vec::new())
                        .unwrap(),
                    false,
                    Some(Duration::from_secs(2)),
                )
                .await;
            assert!(registration.is_err());
            assert_eq!(
                server.requests.load(std::sync::atomic::Ordering::Acquire),
                1,
                "no stale registration socket may be created"
            );
            drop(client);
            server.finish().await;
        }
    }

    #[tokio::test]
    async fn oauth_source_revalidation_after_owned_dns_answer_rejects_the_addresses() {
        use reqwest::dns::Resolve as _;
        let current = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let changed = current.clone();
        let lookup: Lookup = Arc::new(move |_| {
            changed.store(false, std::sync::atomic::Ordering::Release);
            Box::pin(async { Ok(vec!["8.8.8.8".parse().unwrap()]) })
        });
        let mut resolver = PublicOnlyResolver::new(lookup);
        resolver.source = Some(Arc::new(move || {
            current.load(std::sync::atomic::Ordering::Acquire)
        }));
        let result = resolver.resolve("owned.example".parse().unwrap()).await;
        assert!(
            result.is_err(),
            "a lookup result cannot authorize a changed source"
        );
        // The resolver alone is called: no socket or external request exists.
    }
}
