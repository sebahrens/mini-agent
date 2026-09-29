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
        }
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
        let builder = builder.dns_resolver(PublicOnlyResolver::new(lookup));
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
}

impl PublicOnlyResolver {
    pub(crate) fn new(lookup: Lookup) -> Self {
        Self { lookup }
    }
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let lookup = self.lookup.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = lookup(host.clone()).await?;
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
    follow_redirects: reqwest::Client,
    stop_redirects: reqwest::Client,
}

impl GuardedOAuthHttpClient {
    pub(crate) fn new(endpoint: &ValidatedEndpoint) -> anyhow::Result<Self> {
        Self::with_lookup(endpoint, system_lookup())
    }

    fn with_lookup(endpoint: &ValidatedEndpoint, lookup: Lookup) -> anyhow::Result<Self> {
        let build = |policy: reqwest::redirect::Policy| {
            endpoint
                .pin_with(reqwest::Client::builder(), lookup.clone())
                .timeout(OAUTH_HTTP_TIMEOUT)
                .redirect(policy)
                .build()
                .map_err(|error| {
                    anyhow::anyhow!("MCP OAuth HTTP client construction failed: {error}")
                })
        };
        let follow = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= OAUTH_HTTP_MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match check_url_host(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(error) => attempt.error(error),
            }
        });
        Ok(Self {
            follow_redirects: build(follow)?,
            stop_redirects: build(reqwest::redirect::Policy::none())?,
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
        let client = if follow_redirects {
            &self.follow_redirects
        } else {
            &self.stop_redirects
        };
        let mut request = reqwest::Request::try_from(request)
            .map_err(|error| OAuthHttpClientError::new(error.to_string()))?;
        check_url_host(request.url()).map_err(OAuthHttpClientError::new)?;
        if let Some(timeout) = timeout {
            *request.timeout_mut() = Some(timeout);
        }
        let response = client
            .execute(request)
            .await
            .map_err(|error| OAuthHttpClientError::new(error_chain(&error)))?;

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
            loopback_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
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
            loopback_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
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
}
