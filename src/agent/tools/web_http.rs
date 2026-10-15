//! Parent-only bounded HTTP policy shared by native tools and the JS broker.
//! The worker has no client or socket authority. DNS answers are checked before
//! permission approval, and the transport pins exactly those approved addresses.

#[cfg(all(feature = "js", feature = "sandbox"))]
use crate::extras::js::types::EffectServiceError;
use reqwest::Url;
#[cfg(all(feature = "js", feature = "sandbox"))]
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
#[cfg(any(test, all(feature = "js", feature = "sandbox")))]
use std::time::Instant;

pub(crate) const FETCH_MAX_DESTINATION_ADDRESSES: usize = 32;
pub(crate) const FETCH_MAX_REDIRECTS: usize = 5;
pub(crate) const NATIVE_MAX_URL_BYTES: usize = 8192;
pub(crate) const FETCH_DNS_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const FETCH_DNS_MAX_IN_FLIGHT: usize = 4;

pub(crate) fn process_dns_budget() -> Arc<tokio::sync::Semaphore> {
    static BUDGET: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    BUDGET
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(FETCH_DNS_MAX_IN_FLIGHT)))
        .clone()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchOrigin {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: u16,
}

#[derive(Debug, Clone)]
pub(crate) enum FetchOriginPolicy {
    Unrestricted,
    Exact(Vec<FetchOrigin>),
    Deny,
}

#[derive(Debug, Clone)]
pub(crate) struct FetchPolicy {
    pub(crate) origins: FetchOriginPolicy,
    pub(crate) allow_http: bool,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            origins: FetchOriginPolicy::Unrestricted,
            allow_http: false,
        }
    }
}

impl FetchPolicy {
    pub(crate) fn from_settings(origins: Option<&[String]>, allow_http: bool) -> Self {
        let Some(origins) = origins else {
            return Self {
                allow_http,
                ..Self::default()
            };
        };
        if origins.is_empty() {
            return Self {
                origins: FetchOriginPolicy::Deny,
                allow_http,
            };
        }
        let parsed = origins
            .iter()
            .map(|origin| parse_configured_origin(origin, allow_http))
            .collect::<Result<Vec<_>, _>>();
        Self {
            origins: parsed
                .map(FetchOriginPolicy::Exact)
                .unwrap_or(FetchOriginPolicy::Deny),
            allow_http,
        }
    }

    pub(crate) fn authorize(&self, raw_url: &str) -> Result<Url, FetchError> {
        let url = normalize_fetch_url(raw_url, true)?;
        self.authorize_normalized(url)
    }

    pub(crate) fn authorize_normalized(&self, url: Url) -> Result<Url, FetchError> {
        if url.scheme() == "http" && !self.allow_http {
            return Err(FetchError::SchemeDenied);
        }
        let origin = fetch_origin(&url)?;
        match &self.origins {
            FetchOriginPolicy::Unrestricted => Ok(url),
            FetchOriginPolicy::Exact(origins) if origins.contains(&origin) => Ok(url),
            FetchOriginPolicy::Exact(_) | FetchOriginPolicy::Deny => Err(FetchError::OriginDenied),
        }
    }
}

pub(crate) fn parse_configured_origin(
    raw: &str,
    allow_http: bool,
) -> Result<FetchOrigin, FetchError> {
    let url = normalize_fetch_url(raw, allow_http)?;
    if url.path() != "/" || url.query().is_some() {
        return Err(FetchError::InvalidOrigin);
    }
    fetch_origin(&url)
}

pub(crate) fn normalize_fetch_url(raw: &str, allow_http: bool) -> Result<Url, FetchError> {
    let mut url = Url::parse(raw).map_err(|_| FetchError::InvalidUrl)?;
    match url.scheme() {
        "https" => {}
        "http" if allow_http => {}
        _ => return Err(FetchError::SchemeDenied),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::EmbeddedCredentials);
    }
    if url.fragment().is_some() {
        return Err(FetchError::FragmentDenied);
    }
    let host = url.host_str().ok_or(FetchError::MissingHost)?;
    if host.is_empty() || host.ends_with('.') {
        return Err(FetchError::InvalidHost);
    }
    let port = url.port_or_known_default().ok_or(FetchError::MissingPort)?;
    if url.port() == Some(port) && matches!((url.scheme(), port), ("https", 443) | ("http", 80)) {
        url.set_port(None).map_err(|_| FetchError::InvalidUrl)?;
    }
    Ok(url)
}

pub(crate) fn fetch_origin(url: &Url) -> Result<FetchOrigin, FetchError> {
    Ok(FetchOrigin {
        scheme: url.scheme().to_string(),
        host: url
            .host_str()
            .ok_or(FetchError::MissingHost)?
            .to_ascii_lowercase(),
        port: url.port_or_known_default().ok_or(FetchError::MissingPort)?,
    })
}

pub(crate) fn validate_public_destinations(addresses: &[SocketAddr]) -> Result<(), FetchError> {
    if addresses.len() > FETCH_MAX_DESTINATION_ADDRESSES {
        Err(FetchError::TooManyDestinations)
    } else if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        Err(FetchError::DestinationDenied)
    } else {
        Ok(())
    }
}

pub(crate) fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

pub(crate) fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    ![
        (0x0000_0000, 8),  // current network / unspecified
        (0x0a00_0000, 8),  // RFC1918
        (0x6440_0000, 10), // shared address space
        (0x7f00_0000, 8),  // loopback
        (0xa9fe_0000, 16), // link-local and metadata
        (0xac10_0000, 12), // RFC1918
        (0xc000_0000, 24), // IETF protocol assignments
        (0xc000_0200, 24), // TEST-NET-1
        (0xc058_6300, 24), // deprecated 6to4 relay anycast
        (0xc0a8_0000, 16), // RFC1918
        (0xc612_0000, 15), // benchmarking
        (0xc633_6400, 24), // TEST-NET-2
        (0xcb00_7100, 24), // TEST-NET-3
        (0xe000_0000, 4),  // multicast
        (0xf000_0000, 4),  // reserved and limited broadcast
    ]
    .into_iter()
    .any(|(network, prefix)| ipv4_in_prefix(value, network, prefix))
}

pub(crate) fn ipv4_in_prefix(address: u32, network: u32, prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
    address & mask == network & mask
}

pub(crate) fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let value = u128::from(address);
    if !ipv6_in_prefix(value, 0x2000_u128 << 112, 3) {
        return false;
    }
    ![
        (0x2001_u128 << 112, 23),     // IETF special-use
        (0x2001_0db8_u128 << 96, 32), // documentation
        (0x2002_u128 << 112, 16),     // 6to4 transition addresses
        (0x3fff_u128 << 112, 20),     // documentation
    ]
    .into_iter()
    .any(|(network, prefix)| ipv6_in_prefix(value, network, prefix))
}

pub(crate) fn ipv6_in_prefix(address: u128, network: u128, prefix: u8) -> bool {
    let mask = u128::MAX.checked_shl(u32::from(128 - prefix)).unwrap_or(0);
    address & mask == network & mask
}

pub(crate) const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const FETCH_READ_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const FETCH_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(all(feature = "js", feature = "sandbox"))]
pub(crate) const FETCH_CANCELLATION_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const FETCH_REQUEST_HEADER_MAX_BYTES: usize = 16 * 1024;
pub(crate) const FETCH_REQUEST_HEADER_MAX_COUNT: usize = 64;
pub(crate) const FETCH_REQUEST_BODY_MAX_BYTES: usize = 256 * 1024;
pub(crate) const FETCH_RESPONSE_HEADER_MAX_BYTES: usize = 64 * 1024;
pub(crate) const FETCH_RESPONSE_HEADER_MAX_COUNT: usize = 128;
pub(crate) const FETCH_RESPONSE_BODY_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchResult {
    pub status: u16,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct FetchRequest {
    pub(crate) method: reqwest::Method,
    pub(crate) headers: reqwest::header::HeaderMap,
    pub(crate) body: Option<Vec<u8>>,
}

impl FetchRequest {
    pub(crate) fn get() -> Self {
        Self {
            method: reqwest::Method::GET,
            headers: reqwest::header::HeaderMap::new(),
            body: None,
        }
    }

    pub(crate) fn try_new(
        method: &str,
        headers: &[(String, String)],
        body: Option<String>,
    ) -> Result<Self, FetchError> {
        let method = match method {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            _ => {
                return Err(FetchError::InvalidOptions(
                    "method must be GET or POST".to_string(),
                ));
            }
        };
        if method == reqwest::Method::GET && body.is_some() {
            return Err(FetchError::InvalidOptions(
                "GET requests cannot have a body".to_string(),
            ));
        }
        let mut parsed_headers = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| FetchError::InvalidOptions("invalid header name".to_string()))?;
            if is_forbidden_fetch_header(&name) {
                return Err(FetchError::InvalidOptions(
                    "header is controlled by the host".to_string(),
                ));
            }
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| FetchError::InvalidOptions("invalid header value".to_string()))?;
            parsed_headers.append(name, value);
        }
        validate_header_limits(
            &parsed_headers,
            FETCH_REQUEST_HEADER_MAX_COUNT,
            FETCH_REQUEST_HEADER_MAX_BYTES,
            FetchError::RequestHeadersTooLarge,
        )?;
        let body = body.map(String::into_bytes);
        if body
            .as_ref()
            .is_some_and(|body| body.len() > FETCH_REQUEST_BODY_MAX_BYTES)
        {
            return Err(FetchError::RequestBodyTooLarge);
        }
        Ok(Self {
            method,
            headers: parsed_headers,
            body,
        })
    }
}

pub(crate) fn is_forbidden_fetch_header(name: &reqwest::header::HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-authorization"
            | "proxy-connection"
            | "authorization"
            | "cookie"
            | "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-real-ip"
            | "via"
            | "upgrade"
            | "te"
            | "trailer"
            | "accept-encoding"
    ) || name.starts_with("proxy-")
        || name.starts_with("sec-")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchError {
    InvalidUrl,
    SchemeDenied,
    EmbeddedCredentials,
    FragmentDenied,
    MissingHost,
    InvalidHost,
    MissingPort,
    InvalidOrigin,
    OriginDenied,
    DnsResolutionFailed,
    DestinationDenied,
    TooManyDestinations,
    #[cfg(all(feature = "js", feature = "sandbox"))]
    Permission(EffectServiceError),
    TooManyRedirects,
    RedirectReplayDenied,
    CrossOriginRedirectDenied,
    #[cfg(all(feature = "js", feature = "sandbox"))]
    UnauditedRedirectDenied,
    InvalidOptions(String),
    ClientBuild(String),
    Cancelled,
    TimedOut,
    #[cfg(all(feature = "js", feature = "sandbox"))]
    OutcomeUnknown,
    RequestHeadersTooLarge,
    RequestBodyTooLarge,
    RequestFailed(String),
    ResponseHeadersTooLarge,
    ResponseBodyTooLarge,
    UnsupportedContentEncoding,
    InvalidRedirect,
    InvalidUtf8,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl => formatter.write_str("fetch URL is invalid or ambiguous"),
            Self::SchemeDenied => formatter.write_str("fetch URL scheme is not allowed"),
            Self::EmbeddedCredentials => {
                formatter.write_str("fetch URL must not contain embedded credentials")
            }
            Self::FragmentDenied => formatter.write_str("fetch URL fragments are not supported"),
            Self::MissingHost => formatter.write_str("fetch URL has no host"),
            Self::InvalidHost => formatter.write_str("fetch URL host is invalid or ambiguous"),
            Self::MissingPort => formatter.write_str("fetch URL has no effective port"),
            Self::InvalidOrigin => {
                formatter.write_str("fetch origin configuration must contain only an origin")
            }
            Self::OriginDenied => {
                formatter.write_str("fetch URL origin is not allowed by configuration")
            }
            Self::DnsResolutionFailed => formatter.write_str("fetch DNS resolution failed"),
            Self::DestinationDenied => {
                formatter.write_str("fetch destination is not a public network address")
            }
            Self::TooManyDestinations => {
                formatter.write_str("fetch DNS answer exceeds the destination address limit")
            }
            #[cfg(all(feature = "js", feature = "sandbox"))]
            Self::Permission(error) => write!(formatter, "fetch permission denied: {error}"),
            Self::TooManyRedirects => formatter.write_str("fetch redirect limit exceeded"),
            Self::RedirectReplayDenied => {
                formatter.write_str("fetch refuses to replay a non-GET request after a redirect")
            }
            Self::CrossOriginRedirectDenied => formatter.write_str(
                "fetch refuses to forward caller headers across an origin-changing redirect",
            ),
            #[cfg(all(feature = "js", feature = "sandbox"))]
            Self::UnauditedRedirectDenied => {
                formatter.write_str("fetch redirect requires an independent durable audit record")
            }
            Self::InvalidOptions(message) => write!(formatter, "invalid fetch options: {message}"),
            Self::ClientBuild(message) => write!(formatter, "fetch client setup failed: {message}"),
            Self::Cancelled => formatter.write_str("fetch cancelled"),
            Self::TimedOut => formatter.write_str("fetch timed out"),
            #[cfg(all(feature = "js", feature = "sandbox"))]
            Self::OutcomeUnknown => {
                formatter.write_str("fetch outcome is unknown after request dispatch")
            }
            Self::RequestHeadersTooLarge => {
                formatter.write_str("fetch request headers exceed the configured limit")
            }
            Self::RequestBodyTooLarge => {
                formatter.write_str("fetch request body exceeds the configured limit")
            }
            Self::RequestFailed(message) => write!(formatter, "fetch request failed: {message}"),
            Self::ResponseHeadersTooLarge => {
                formatter.write_str("fetch response headers exceed the configured limit")
            }
            Self::ResponseBodyTooLarge => {
                formatter.write_str("fetch response body exceeds the configured limit")
            }
            Self::UnsupportedContentEncoding => {
                formatter.write_str("fetch response content encoding is not supported")
            }
            Self::InvalidRedirect => formatter.write_str("fetch response has an invalid redirect"),
            Self::InvalidUtf8 => formatter.write_str("fetch response body is not valid UTF-8"),
        }
    }
}

impl std::error::Error for FetchError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchTransportOutcome {
    Complete(FetchResult),
    Redirect(Url),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchLimits {
    pub(crate) connect_timeout: Duration,
    pub(crate) read_timeout: Duration,
    pub(crate) total_timeout: Duration,
    pub(crate) request_header_max_bytes: usize,
    pub(crate) request_header_max_count: usize,
    pub(crate) request_body_max_bytes: usize,
    pub(crate) response_header_max_bytes: usize,
    pub(crate) response_header_max_count: usize,
    pub(crate) response_body_max_bytes: usize,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            connect_timeout: FETCH_CONNECT_TIMEOUT,
            read_timeout: FETCH_READ_TIMEOUT,
            total_timeout: FETCH_TOTAL_TIMEOUT,
            request_header_max_bytes: FETCH_REQUEST_HEADER_MAX_BYTES,
            request_header_max_count: FETCH_REQUEST_HEADER_MAX_COUNT,
            request_body_max_bytes: FETCH_REQUEST_BODY_MAX_BYTES,
            response_header_max_bytes: FETCH_RESPONSE_HEADER_MAX_BYTES,
            response_header_max_count: FETCH_RESPONSE_HEADER_MAX_COUNT,
            response_body_max_bytes: FETCH_RESPONSE_BODY_MAX_BYTES,
        }
    }
}

#[cfg(all(feature = "js", feature = "sandbox"))]
#[derive(Clone)]
pub(crate) struct FetchTransport {
    client: reqwest::blocking::Client,
    limits: FetchLimits,
}

#[cfg(all(feature = "js", feature = "sandbox"))]
impl FetchTransport {
    pub(crate) fn new(resolved_host: Option<(&str, &[SocketAddr])>) -> Result<Self, FetchError> {
        Self::with_limits(FetchLimits::default(), resolved_host)
    }

    pub(crate) fn with_limits(
        limits: FetchLimits,
        resolved_host: Option<(&str, &[SocketAddr])>,
    ) -> Result<Self, FetchError> {
        let mut builder = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .pool_max_idle_per_host(0)
            .connect_timeout(limits.connect_timeout)
            // The blocking client applies this bound to the initial response
            // wait and every subsequent body read. `execute` separately owns
            // the total wall-clock deadline across all successful reads.
            .timeout(limits.read_timeout)
            .user_agent(concat!("mini-agent-js-fetch/", env!("CARGO_PKG_VERSION")));
        if let Some((host, addresses)) = resolved_host {
            builder = builder.resolve_to_addrs(host, addresses);
        }
        let client = builder
            .build()
            .map_err(|error| FetchError::ClientBuild(error.to_string()))?;
        Ok(Self { client, limits })
    }

    pub(crate) fn execute(
        &self,
        url: Url,
        request: &FetchRequest,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<FetchTransportOutcome, FetchError> {
        if is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        validate_header_limits(
            &request.headers,
            self.limits.request_header_max_count,
            self.limits.request_header_max_bytes,
            FetchError::RequestHeadersTooLarge,
        )?;
        if request
            .body
            .as_ref()
            .is_some_and(|body| body.len() > self.limits.request_body_max_bytes)
        {
            return Err(FetchError::RequestBodyTooLarge);
        }

        let started = Instant::now();
        let mut builder = self
            .client
            .request(request.method.clone(), url.clone())
            .headers(request.headers.clone());
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        let mut response = builder.send().map_err(map_reqwest_error)?;
        if is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        if started.elapsed() >= self.limits.total_timeout {
            return Err(FetchError::TimedOut);
        }

        validate_header_limits(
            response.headers(),
            self.limits.response_header_max_count,
            self.limits.response_header_max_bytes,
            FetchError::ResponseHeadersTooLarge,
        )?;
        reject_encoded_response(response.headers())?;

        if is_followable_redirect(response.status()) {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or(FetchError::InvalidRedirect)?
                .to_str()
                .map_err(|_| FetchError::InvalidRedirect)?;
            let redirect = url
                .join(location)
                .map_err(|_| FetchError::InvalidRedirect)?;
            return Ok(FetchTransportOutcome::Redirect(redirect));
        }

        if response
            .content_length()
            .is_some_and(|length| length > self.limits.response_body_max_bytes as u64)
        {
            return Err(FetchError::ResponseBodyTooLarge);
        }

        let mut body = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            if is_cancelled() {
                return Err(FetchError::Cancelled);
            }
            if started.elapsed() >= self.limits.total_timeout {
                return Err(FetchError::TimedOut);
            }
            let read = response.read(&mut buffer).map_err(map_io_error)?;
            if read == 0 {
                break;
            }
            if body.len().saturating_add(read) > self.limits.response_body_max_bytes {
                return Err(FetchError::ResponseBodyTooLarge);
            }
            body.extend_from_slice(&buffer[..read]);
        }
        if started.elapsed() >= self.limits.total_timeout {
            return Err(FetchError::TimedOut);
        }
        let text = String::from_utf8(body).map_err(|_| FetchError::InvalidUtf8)?;
        Ok(FetchTransportOutcome::Complete(FetchResult {
            status: response.status().as_u16(),
            text,
        }))
    }
}

pub(crate) fn is_followable_redirect(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::MOVED_PERMANENTLY
            | reqwest::StatusCode::FOUND
            | reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT
    )
}

pub(crate) fn validate_header_limits(
    headers: &reqwest::header::HeaderMap,
    max_count: usize,
    max_bytes: usize,
    error: FetchError,
) -> Result<(), FetchError> {
    if headers.len() > max_count {
        return Err(error);
    }
    let mut total = 0_usize;
    for (name, value) in headers {
        total = total
            .checked_add(name.as_str().len())
            .and_then(|total| total.checked_add(value.as_bytes().len()))
            .and_then(|total| total.checked_add(4))
            .ok_or_else(|| error.clone())?;
        if total > max_bytes {
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn reject_encoded_response(
    headers: &reqwest::header::HeaderMap,
) -> Result<(), FetchError> {
    for encoding in headers.get_all(reqwest::header::CONTENT_ENCODING) {
        if !encoding
            .to_str()
            .is_ok_and(|encoding| encoding.eq_ignore_ascii_case("identity"))
        {
            return Err(FetchError::UnsupportedContentEncoding);
        }
    }
    Ok(())
}

pub(crate) fn map_reqwest_error(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::TimedOut
    } else {
        FetchError::RequestFailed(error.to_string())
    }
}

#[cfg(all(feature = "js", feature = "sandbox"))]
pub(crate) fn map_io_error(error: std::io::Error) -> FetchError {
    let mut source = std::error::Error::source(&error);
    let mut timed_out = error.kind() == std::io::ErrorKind::TimedOut
        || error.to_string().to_ascii_lowercase().contains("timed out");
    while let Some(current) = source {
        timed_out |= current
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
            || current
                .to_string()
                .to_ascii_lowercase()
                .contains("timed out");
        source = current.source();
    }
    if timed_out {
        FetchError::TimedOut
    } else {
        FetchError::RequestFailed(error.to_string())
    }
}

/// The OS resolver's actual blocking closure owns both the process-wide permit
/// and (for native tools) the current turn's scoped child until it returns.
pub(crate) fn resolve_os_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>, FetchError> {
    use std::net::ToSocketAddrs;
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|_| FetchError::DnsResolutionFailed)?
        .take(FETCH_MAX_DESTINATION_ADDRESSES + 1)
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        Err(FetchError::DnsResolutionFailed)
    } else {
        Ok(addresses)
    }
}

/// One pinned, redirect-disabled asynchronous HTTP exchange. The caller owns
/// the absolute deadline spanning DNS, approval, every redirect and body read.
pub(crate) async fn send_async(
    url: Url,
    request: &FetchRequest,
    addresses: &[SocketAddr],
) -> Result<FetchTransportOutcome, FetchError> {
    let limits = FetchLimits::default();
    validate_header_limits(
        &request.headers,
        limits.request_header_max_count,
        limits.request_header_max_bytes,
        FetchError::RequestHeadersTooLarge,
    )?;
    if request
        .body
        .as_ref()
        .is_some_and(|b| b.len() > limits.request_body_max_bytes)
    {
        return Err(FetchError::RequestBodyTooLarge);
    }
    let host = url.host_str().ok_or(FetchError::MissingHost)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .pool_max_idle_per_host(0)
        .connect_timeout(limits.connect_timeout)
        .read_timeout(limits.read_timeout)
        .timeout(limits.total_timeout)
        .user_agent(concat!("mini-agent-web/", env!("CARGO_PKG_VERSION")))
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|e| FetchError::ClientBuild(e.to_string()))?;
    let mut builder = client
        .request(request.method.clone(), url.clone())
        .headers(request.headers.clone());
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }
    let mut response = builder.send().await.map_err(map_reqwest_error)?;
    validate_header_limits(
        response.headers(),
        limits.response_header_max_count,
        limits.response_header_max_bytes,
        FetchError::ResponseHeadersTooLarge,
    )?;
    reject_encoded_response(response.headers())?;
    if is_followable_redirect(response.status()) {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or(FetchError::InvalidRedirect)?
            .to_str()
            .map_err(|_| FetchError::InvalidRedirect)?;
        return Ok(FetchTransportOutcome::Redirect(
            url.join(location)
                .map_err(|_| FetchError::InvalidRedirect)?,
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > limits.response_body_max_bytes as u64)
    {
        return Err(FetchError::ResponseBodyTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? {
        if body.len().saturating_add(chunk.len()) > limits.response_body_max_bytes {
            return Err(FetchError::ResponseBodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(FetchTransportOutcome::Complete(FetchResult {
        status: response.status().as_u16(),
        text: String::from_utf8(body).map_err(|_| FetchError::InvalidUtf8)?,
    }))
}

#[derive(Clone)]
pub(crate) struct NativeWebClient {
    policy: FetchPolicy,
    permission: Option<super::PermCheck>,
    ask_tx: Option<super::AskSender>,
    #[cfg(test)]
    pub(crate) test_resolution: Option<(Vec<SocketAddr>, Vec<SocketAddr>)>,
    #[cfg(test)]
    pub(crate) test_transport_http: bool,
    #[cfg(test)]
    pub(crate) test_total_timeout: Duration,
}

impl NativeWebClient {
    pub(crate) fn new(
        policy: FetchPolicy,
        permission: Option<super::PermCheck>,
        ask_tx: Option<super::AskSender>,
    ) -> Self {
        Self {
            policy,
            permission,
            ask_tx,
            #[cfg(test)]
            test_resolution: None,
            #[cfg(test)]
            test_transport_http: false,
            #[cfg(test)]
            test_total_timeout: FETCH_TOTAL_TIMEOUT,
        }
    }

    #[cfg(all(test, feature = "acp"))]
    pub(crate) fn configure_permissions_for_test(
        &mut self,
        permission: Option<super::PermCheck>,
        ask_tx: Option<super::AskSender>,
    ) {
        self.permission = permission;
        self.ask_tx = ask_tx;
    }

    #[cfg(test)]
    pub(crate) fn permission_for_test(&self) -> super::PermCheck {
        self.permission
            .clone()
            .expect("fixture must configure permission")
    }

    async fn resolve(&self, url: &Url) -> Result<Vec<SocketAddr>, FetchError> {
        #[cfg(test)]
        if let Some((approved, _)) = &self.test_resolution {
            return Ok(approved.clone());
        }
        let host = url.host_str().ok_or(FetchError::MissingHost)?.to_owned();
        let port = url.port_or_known_default().ok_or(FetchError::MissingPort)?;
        if let Ok(address) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(address, port)]);
        }
        tokio::time::timeout(FETCH_DNS_TIMEOUT, async move {
            let permit = process_dns_budget()
                .acquire_owned()
                .await
                .map_err(|_| FetchError::DnsResolutionFailed)?;
            crate::agent::runner::spawn_blocking_scoped(move || {
                let _permit = permit;
                resolve_os_addresses(&host, port)
            })
            .await
            .map_err(|_| FetchError::DnsResolutionFailed)?
        })
        .await
        .map_err(|_| FetchError::TimedOut)?
    }

    /// A dropped/cancelled native request cannot dispatch later: approval and
    /// socket exchange are in this future; only the bounded OS resolver can
    /// outlive it, retaining its semaphore permit and turn ownership.
    pub(crate) async fn execute(
        &self,
        raw_url: &str,
        request: &FetchRequest,
        tool: &str,
        follow: bool,
    ) -> Result<(Url, FetchResult), super::ToolError> {
        let work = async {
            let mut url = self.policy.authorize(raw_url).map_err(native_error)?;
            for redirects in 0..=FETCH_MAX_REDIRECTS {
                if url.as_str().len() > NATIVE_MAX_URL_BYTES {
                    return Err(super::ToolError::Msg(
                        "web URL exceeds the 8192-byte normalized URL limit".into(),
                    ));
                }
                let mut addresses = self.resolve(&url).await.map_err(native_error)?;
                addresses.sort_unstable();
                addresses.dedup();
                validate_public_destinations(&addresses).map_err(native_error)?;
                // The approval includes the normalized URL and complete public
                // answer. A host-wide grant never skips DNS/redirect validation.
                let key = format!(
                    "{} destinations=[{}]",
                    url.as_str(),
                    addresses
                        .iter()
                        .map(SocketAddr::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                );
                let origin_scope = crate::permission::pattern::web_origin_pattern(&url);
                super::check_perm_with_suggestion(
                    &self.permission,
                    &self.ask_tx,
                    tool,
                    &key,
                    Some(origin_scope),
                )
                .await?;
                if crate::agent::runner::current_work_scope_is_cancelled() {
                    return Err(native_error(FetchError::Cancelled));
                }
                #[cfg(test)]
                let addresses = self
                    .test_resolution
                    .as_ref()
                    .map(|(_, transport)| transport.clone())
                    .unwrap_or(addresses);
                let send_url = url.clone();
                #[cfg(test)]
                let send_url = if self.test_transport_http {
                    let mut local = send_url;
                    local.set_scheme("http").unwrap();
                    local.set_port(Some(addresses[0].port())).unwrap();
                    local
                } else {
                    send_url
                };
                match send_async(send_url, request, &addresses)
                    .await
                    .map_err(native_error)?
                {
                    FetchTransportOutcome::Complete(result) => return Ok((url, result)),
                    FetchTransportOutcome::Redirect(next) => {
                        if !follow || request.method != reqwest::Method::GET {
                            return Err(native_error(FetchError::RedirectReplayDenied));
                        }
                        if redirects == FETCH_MAX_REDIRECTS {
                            return Err(native_error(FetchError::TooManyRedirects));
                        }
                        if !request.headers.is_empty()
                            && fetch_origin(&url).map_err(native_error)?
                                != fetch_origin(&next).map_err(native_error)?
                        {
                            return Err(native_error(FetchError::CrossOriginRedirectDenied));
                        }
                        url = self.policy.authorize(next.as_str()).map_err(native_error)?;
                    }
                }
            }
            Err(native_error(FetchError::TooManyRedirects))
        };
        #[cfg(not(test))]
        let total_timeout = FETCH_TOTAL_TIMEOUT;
        #[cfg(test)]
        let total_timeout = self.test_total_timeout;
        tokio::select! { biased;
            _ = crate::agent::runner::current_work_scope_cancelled() => Err(native_error(FetchError::Cancelled)),
            result = tokio::time::timeout(total_timeout, work) => result.unwrap_or_else(|_| Err(native_error(FetchError::TimedOut))),
        }
    }
}

fn native_error(error: FetchError) -> super::ToolError {
    // Transport diagnostics may contain URLs or backend detail. Do not expose
    // backend strings, response error bodies or credentials to the model.
    let message = match error {
        FetchError::ClientBuild(_) => "web HTTP client setup failed".to_string(),
        FetchError::RequestFailed(_) => "web HTTP request failed".to_string(),
        other => other.to_string(),
    };
    super::ToolError::Msg(message)
}

#[cfg(test)]
pub(crate) mod web_http_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Owned loopback peer with bounded socket reads/writes and unconditional
    /// thread settlement. No network-policy production bypass is configured.
    pub(crate) struct Peer {
        pub(crate) address: SocketAddr,
        pub(crate) requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Peer {
        pub(crate) fn new(responses: Vec<String>) -> Self {
            Self::new_bytes(responses.into_iter().map(String::into_bytes).collect())
        }
        pub(crate) fn new_bytes(responses: Vec<Vec<u8>>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = stop.clone();
            let thread = std::thread::spawn(move || {
                let end = Instant::now() + Duration::from_secs(15);
                let mut responses = responses.into_iter();
                while !stopping.load(Ordering::Acquire) && Instant::now() < end {
                    match listener.accept() {
                        Ok((mut socket, _)) => {
                            // Darwin can inherit the listener's nonblocking
                            // status. Large bodies need the bounded write
                            // timeout, rather than a partial WouldBlock write.
                            socket.set_nonblocking(false).unwrap();
                            socket
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            socket
                                .set_write_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut bytes = Vec::new();
                            let mut buffer = [0; 4096];
                            while bytes.len() < 300_000 {
                                match socket.read(&mut buffer) {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                                }
                                if let Some(header_end) =
                                    bytes.windows(4).position(|x| x == b"\r\n\r\n")
                                {
                                    let headers = String::from_utf8_lossy(&bytes[..header_end])
                                        .to_ascii_lowercase();
                                    let length = headers
                                        .lines()
                                        .find_map(|l| {
                                            l.strip_prefix("content-length:")
                                                .and_then(|s| s.trim().parse::<usize>().ok())
                                        })
                                        .unwrap_or(0);
                                    if bytes.len() >= header_end + 4 + length {
                                        break;
                                    }
                                }
                            }
                            captured
                                .lock()
                                .unwrap()
                                .push(String::from_utf8_lossy(&bytes).into_owned());
                            if let Some(response) = responses.next() {
                                if response.is_empty() {
                                    while !stopping.load(Ordering::Acquire) && Instant::now() < end
                                    {
                                        std::thread::sleep(Duration::from_millis(5));
                                    }
                                } else {
                                    let _ = socket.write_all(&response);
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                address,
                requests,
                stop,
                thread: Some(thread),
            }
        }
        pub(crate) fn response(body: &str) -> String {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        pub(crate) fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }
    impl Drop for Peer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    pub(crate) fn allowed_permission() -> super::super::PermCheck {
        Arc::new(Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                crate::permission::SecurityMode::Yolo,
                None,
                None,
            )
            .unwrap(),
        ))
    }
    pub(crate) fn fixture_client(peer: &Peer) -> NativeWebClient {
        let mut client = NativeWebClient::new(
            FetchPolicy::from_settings(None, true),
            Some(allowed_permission()),
            None,
        );
        client.test_resolution = Some((
            vec![SocketAddr::new(
                "93.184.216.34".parse().unwrap(),
                peer.address.port(),
            )],
            vec![peer.address],
        ));
        client
    }
    #[tokio::test]
    async fn web_http_pinned_native_exchange_and_private_denial_use_actual_loopback_peer() {
        let peer = Peer::new(vec![Peer::response("héllo")]);
        let client = fixture_client(&peer);
        let (_, page) = client
            .execute(
                &format!("http://fixture.invalid:{}/docs", peer.address.port()),
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap();
        assert_eq!(page.text, "héllo");
        assert_eq!(page.status, 200);
        assert_eq!(peer.count(), 1);
        let mut denied = client.clone();
        denied.test_resolution = Some((vec![peer.address], vec![peer.address]));
        let error = denied
            .execute(
                "http://fixture.invalid/private",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a public network address"));
        assert_eq!(peer.count(), 1);
    }
    #[tokio::test]
    async fn web_http_native_redirect_reauthorizes_and_rejects_credentials_and_loops() {
        let redirect = "HTTP/1.1 302 Found\r\nLocation: https://user:secret@evil.invalid/a\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
        let peer = Peer::new(vec![redirect]);
        let client = fixture_client(&peer);
        let error = client
            .execute(
                "http://fixture.invalid/a",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("embedded credentials"));
        assert_eq!(peer.count(), 1);
        let redirect="HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
        let peer = Peer::new(vec![redirect; 7]);
        let client = fixture_client(&peer);
        let error = client
            .execute(
                "http://fixture.invalid/a",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("redirect limit"));
        assert_eq!(peer.count(), 6);
    }
    #[tokio::test]
    async fn web_http_native_wire_caps_encoding_utf8_and_credential_redirect_denial() {
        for (response, expected) in [
            (Peer::response(&"x".repeat(FETCH_RESPONSE_BODY_MAX_BYTES+1)),"response body exceeds"),
            ("HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\nx".into(),"content encoding"),
            ("HTTP/1.1 302 Found\r\nLocation: https://evil.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),"replay a non-GET"),
        ] {
            let peer=Peer::new(vec![response]);let client=fixture_client(&peer);
            let request=FetchRequest::try_new("POST", &[("x-api-key".into(),"owned-secret".into())],Some("{}".into())).unwrap();
            let message=client.execute("http://fixture.invalid/search",&request,"web_search",false).await.unwrap_err().to_string();
            assert!(message.contains(expected),"{message}"); assert!(!message.contains("owned-secret"));assert_eq!(peer.count(),1);
        }
    }
    #[test]
    fn web_http_native_public_policy_and_request_scope_are_not_weakened_by_allow() {
        for url in [
            "file:///etc/passwd",
            "https://user:secret@example.com",
            "https://example.com/#fragment",
            "https://example.com./",
        ] {
            assert!(FetchPolicy::default().authorize(url).is_err());
        }
        assert!(
            FetchPolicy::default()
                .authorize("http://example.com/")
                .is_err()
        );
        let exact = FetchPolicy::from_settings(Some(&["https://docs.rs".into()]), false);
        assert!(exact.authorize("https://docs.rs/a?q=1").is_ok());
        assert!(exact.authorize("https://docs.rs.evil.invalid/a").is_err());
        assert!(
            FetchPolicy::from_settings(Some(&[]), false)
                .authorize("https://docs.rs/")
                .is_err()
        );
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                validate_public_destinations(&[SocketAddr::new(ip.parse().unwrap(), 443)]).is_err()
            );
        }
        assert!(
            FetchRequest::try_new(
                "GET",
                &[("authorization".into(), "Bearer secret".into())],
                None
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod web_permission_cancel_tests {
    use super::web_http_tests::{Peer, fixture_client};
    use super::*;
    use crate::permission::checker::{CheckResult, PermissionChecker};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
    fn permission(config: PermissionConfig, mode: SecurityMode) -> super::super::PermCheck {
        Arc::new(std::sync::Mutex::new(
            PermissionChecker::new(&PermissionConfigs::from(config), mode, None, None).unwrap(),
        ))
    }
    #[test]
    fn web_permission_network_defaults_and_explicit_deny_remain_authoritative() {
        for name in ["web_fetch", "web_search"] {
            for (mode, expected) in [
                (SecurityMode::Standard, CheckResult::Ask),
                (SecurityMode::Guarded, CheckResult::Ask),
                (SecurityMode::Restrictive, CheckResult::Ask),
                (
                    SecurityMode::ReadOnly,
                    CheckResult::Denied("Tool not allowed in read-only mode".into()),
                ),
                (
                    SecurityMode::PlanWrite,
                    CheckResult::Denied("Tool not allowed in planwrite mode".into()),
                ),
            ] {
                let checker = permission(PermissionConfig::default(), mode);
                let actual = checker
                    .lock()
                    .unwrap()
                    .check(name, "https://docs.rs/a destinations=[93.184.216.34:443]");
                match expected {
                    CheckResult::Ask => assert_eq!(actual, CheckResult::Ask),
                    _ => assert!(
                        matches!(actual, CheckResult::Denied(_)),
                        "{mode:?}: {actual:?}"
                    ),
                }
            }
            let config = PermissionConfig {
                web_fetch: Some(ToolPerm::Simple(Action::Deny)),
                web_search: Some(ToolPerm::Simple(Action::Deny)),
                ..Default::default()
            };
            let checker = permission(config, SecurityMode::Yolo);
            assert!(matches!(
                checker.lock().unwrap().check(name, "https://docs.rs/"),
                CheckResult::Denied(_)
            ));
        }
    }
    #[tokio::test]
    async fn web_permission_first_host_approval_scopes_exact_origin_and_cancellation_installs_no_grant()
     {
        let peer = Peer::new(vec![Peer::response("one"), Peer::response("two")]);
        let mut client = fixture_client(&peer);
        let permissions = permission(PermissionConfig::default(), SecurityMode::Standard);
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        client.permission = Some(permissions.clone());
        client.ask_tx = Some(tx);
        let get_request = FetchRequest::get();
        let request = client.execute(
            "http://fixture.invalid/one",
            &get_request,
            "web_fetch",
            true,
        );
        let approve = async {
            let ask = rx.recv().await.unwrap().into_permission();
            assert!(
                ask.input
                    .starts_with("http://fixture.invalid/one destinations=[")
            );
            assert_eq!(ask.tool.as_str(), "web_fetch");
            ask.reply
                .send(crate::permission::ask::UserDecision::AllowAlways(
                    ask.suggested_pattern.unwrap(),
                ))
                .unwrap();
        };
        let (result, ()) = tokio::join!(request, approve);
        assert_eq!(result.unwrap().1.text, "one");
        let second = client
            .execute(
                "http://fixture.invalid/two",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap();
        assert_eq!(second.1.text, "two");
        assert!(rx.try_recv().is_err());
        assert_eq!(peer.count(), 2);
        assert_eq!(
            permissions.lock().unwrap().check(
                "web_fetch",
                "http://fixture.invalid.evil/one destinations=[93.184.216.34:80]"
            ),
            CheckResult::Ask
        );
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cancel = scope.cancellation_handle();
        let interrupted = async {
            scope
                .run(client.execute(
                    "http://other.invalid/three",
                    &FetchRequest::get(),
                    "web_fetch",
                    true,
                ))
                .await
        };
        let refuse = async {
            let ask = rx.recv().await.unwrap().into_permission();
            cancel.cancel();
            let _ = ask
                .reply
                .send(crate::permission::ask::UserDecision::AllowAlways(
                    ask.suggested_pattern.unwrap(),
                ));
        };
        let (result, ()) = tokio::join!(interrupted, refuse);
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(peer.count(), 2);
        assert_eq!(
            permissions.lock().unwrap().check(
                "web_fetch",
                "http://other.invalid/three destinations=[93.184.216.34:80]"
            ),
            CheckResult::Ask
        );
    }
    #[tokio::test]
    async fn web_permission_headless_ask_and_redirect_denial_never_contact_disallowed_host() {
        let peer=Peer::new(vec!["HTTP/1.1 302 Found\r\nLocation: https://evil.invalid/a\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()]);
        let mut client = fixture_client(&peer);
        client.permission = Some(permission(
            PermissionConfig::default(),
            SecurityMode::Standard,
        ));
        assert!(
            client
                .execute(
                    "http://fixture.invalid/a",
                    &FetchRequest::get(),
                    "web_fetch",
                    true
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("non-interactive")
        );
        assert_eq!(peer.count(), 0);
        let config = PermissionConfig {
            web_fetch: Some(ToolPerm::Granular(std::collections::HashMap::from([
                ("http://fixture.invalid/*".into(), Action::Allow),
                ("https://evil.invalid/*".into(), Action::Deny),
            ]))),
            ..Default::default()
        };
        client.permission = Some(permission(config, SecurityMode::Standard));
        assert!(
            client
                .execute(
                    "http://fixture.invalid/a",
                    &FetchRequest::get(),
                    "web_fetch",
                    true
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("deny rule")
        );
        assert_eq!(peer.count(), 1);
    }
    #[tokio::test]
    async fn web_http_cancelled_stalled_socket_returns_without_unowned_network_work() {
        let peer = Peer::new(vec![String::new()]);
        let client = fixture_client(&peer);
        let scope = crate::agent::runner::AgentWorkScope::new();
        let worker = scope.clone();
        let task = tokio::spawn(async move {
            worker
                .run(client.execute(
                    "http://fixture.invalid/stalled",
                    &FetchRequest::get(),
                    "web_fetch",
                    true,
                ))
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while peer.count() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        scope.cancellation_handle().cancel();
        let message = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(message.contains("cancelled"));
        assert_eq!(peer.count(), 1);
        tokio::time::timeout(Duration::from_secs(1), scope.wait_idle())
            .await
            .unwrap();
        assert_eq!(scope.active_children(), 0);
    }
    #[tokio::test]
    async fn web_http_cancelled_os_dns_lookup_keeps_permit_and_turn_until_actual_closure_retires() {
        let (scope, started, release) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        let client = NativeWebClient::new(
            FetchPolicy::from_settings(None, true),
            Some(web_http_tests::allowed_permission()),
            None,
        );
        let worker = scope.clone();
        let task = tokio::spawn(async move {
            worker
                .run(client.execute("http://localhost/", &FetchRequest::get(), "web_fetch", true))
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if started.try_recv().is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(scope.active_children(), 1);
        scope.cancellation_handle().cancel();
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), scope.wait_idle())
                .await
                .is_err()
        );
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), scope.wait_idle())
            .await
            .unwrap();
        assert_eq!(scope.active_children(), 0);
    }
}

#[cfg(test)]
mod web_opaque_and_literal_tests {
    use super::*;
    use crate::permission::checker::{CheckResult, PermissionChecker};
    use crate::permission::{PermissionConfigs, SecurityMode};
    #[test]
    fn web_permission_plain_config_and_opaque_nonpath_allowalways_semantics_are_preserved() {
        let mut checker = PermissionChecker::new(
            &PermissionConfigs::from(crate::permission::PermissionConfig {
                default: Some(crate::permission::Action::Ask),
                ..Default::default()
            }),
            SecurityMode::Standard,
            None,
            None,
        )
        .unwrap();
        for tool in ["shell", "js/fetch", "mcp_tool", "web_fetch", "web_search"] {
            let authored = "https://*.example/**";
            checker.add_session_allowlist(tool.into(), authored);
            assert_eq!(checker.check(tool, authored), CheckResult::Allowed);
            assert_eq!(
                checker.check(tool, "https://docs.example/a"),
                CheckResult::Ask
            );
            checker.add_session_allowlist(tool.into(), "mini-agent-literal-web-origin-v1:zz");
            assert_eq!(
                checker.check(tool, "https://malformed.example/a"),
                CheckResult::Ask
            );
        }
        let encoded = crate::permission::pattern::web_origin_pattern(
            &Url::parse("https://docs.rs/").unwrap(),
        );
        checker.add_session_allowlist("js/fetch".into(), &encoded);
        assert_eq!(
            checker.check(
                "js/fetch",
                "https://docs.rs/a destinations=[93.184.216.34:443]"
            ),
            CheckResult::Ask
        );
        checker.add_session_allowlist("web_fetch".into(), &encoded);
        assert_eq!(
            checker.check(
                "web_fetch",
                "https://docs.rs/a destinations=[93.184.216.34:443]"
            ),
            CheckResult::Allowed
        );
        assert_eq!(
            checker.check(
                "web_fetch",
                "https://docs.rs:8443/a destinations=[93.184.216.34:8443]"
            ),
            CheckResult::Ask
        );
        let mut restored = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            None,
            None,
        )
        .unwrap();
        restored.load_session_allowlist(&[
            ("web_fetch".into(), encoded.clone()),
            ("js/fetch".into(), encoded),
        ]);
        assert_eq!(
            restored.check(
                "web_fetch",
                "https://docs.rs/other destinations=[93.184.216.34:443]"
            ),
            CheckResult::Allowed
        );
        assert_eq!(
            restored.check(
                "web_fetch",
                "https://docs.rs.evil/other destinations=[93.184.216.34:443]"
            ),
            CheckResult::Ask
        );
        assert_eq!(
            restored.check(
                "js/fetch",
                "https://docs.rs/other destinations=[93.184.216.34:443]"
            ),
            CheckResult::Ask
        );
    }
    #[tokio::test]
    async fn web_http_literal_ipv6_resolution_skips_dns_and_private_mapped_forms_are_denied() {
        let client = NativeWebClient::new(
            FetchPolicy::default(),
            Some(web_http_tests::allowed_permission()),
            None,
        );
        let url = Url::parse("https://[2606:4700:4700::1111]/").unwrap();
        let addresses = client.resolve(&url).await.unwrap();
        assert_eq!(
            addresses,
            vec![SocketAddr::new(
                "2606:4700:4700::1111".parse().unwrap(),
                443
            )]
        );
        assert!(validate_public_destinations(&addresses).is_ok());
        let url = Url::parse("https://[::ffff:127.0.0.1]/").unwrap();
        assert!(validate_public_destinations(&client.resolve(&url).await.unwrap()).is_err());
    }
}

#[cfg(test)]
mod web_deadline_tests {
    use super::*;
    use web_http_tests::{Peer, fixture_client};
    #[tokio::test]
    async fn web_http_absolute_deadline_expires_pending_approval_without_late_grant_or_dispatch() {
        let peer = Peer::new(vec![]);
        let mut client = fixture_client(&peer);
        let checker = std::sync::Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &crate::permission::PermissionConfigs::default(),
                crate::permission::SecurityMode::Standard,
                None,
                None,
            )
            .unwrap(),
        ));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        client.ask_tx = Some(tx);
        client.permission = Some(checker.clone());
        client.test_total_timeout = Duration::from_millis(75);
        let request = FetchRequest::get();
        let fetch = client.execute("http://fixture.invalid/docs", &request, "web_fetch", true);
        let late = async {
            let ask = rx.recv().await.unwrap().into_permission();
            tokio::time::sleep(Duration::from_millis(125)).await;
            assert!(ask.reply.is_closed());
            assert!(
                ask.reply
                    .send(crate::permission::ask::UserDecision::AllowAlways(
                        ask.suggested_pattern.unwrap()
                    ))
                    .is_err()
            );
        };
        let (result, ()) = tokio::join!(fetch, late);
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert_eq!(peer.count(), 0);
        assert_eq!(
            checker.lock().unwrap().check(
                "web_fetch",
                "http://fixture.invalid/docs destinations=[93.184.216.34:80]"
            ),
            crate::permission::checker::CheckResult::Ask
        );
    }
    #[tokio::test]
    async fn web_http_absolute_deadline_bounds_a_live_stalled_exchange() {
        let peer = Peer::new(vec![String::new()]);
        let mut client = fixture_client(&peer);
        client.test_total_timeout = Duration::from_millis(75);
        let before = std::time::Instant::now();
        let error = client
            .execute(
                "http://fixture.invalid/docs",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"));
        assert!(before.elapsed() < Duration::from_secs(1));
        assert_eq!(peer.count(), 1);
    }
}

#[cfg(test)]
mod web_stream_body_tests {
    use super::*;
    use web_http_tests::Peer;

    #[tokio::test]
    async fn web_http_chunked_body_cap_and_non_utf8_are_rejected_on_actual_wire() {
        let body = vec![b'x'; FETCH_RESPONSE_BODY_MAX_BYTES + 1];
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            body.len()
        )
        .into_bytes();
        chunked.extend(body);
        chunked.extend(b"\r\n0\r\n\r\n");
        let peer = Peer::new_bytes(vec![chunked]);
        let error = send_async(
            Url::parse(&format!(
                "http://fixture.invalid:{}/body",
                peer.address.port()
            ))
            .unwrap(),
            &FetchRequest::get(),
            &[peer.address],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, FetchError::ResponseBodyTooLarge),
            "{error:?}"
        );
        assert_eq!(peer.count(), 1);

        let mut invalid =
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n".to_vec();
        invalid.push(0xff);
        let peer = Peer::new_bytes(vec![invalid]);
        let error = send_async(
            Url::parse(&format!(
                "http://fixture.invalid:{}/body",
                peer.address.port()
            ))
            .unwrap(),
            &FetchRequest::get(),
            &[peer.address],
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FetchError::InvalidUtf8), "{error:?}");
        assert_eq!(peer.count(), 1);
    }
}

#[cfg(test)]
mod web_redirect_size_tests {
    use super::*;
    use web_http_tests::{Peer, fixture_client};

    #[tokio::test]
    async fn web_http_oversized_redirect_and_normalized_url_are_denied_before_second_dispatch() {
        let location = format!("/{}", "x".repeat(NATIVE_MAX_URL_BYTES));
        let peer = Peer::new(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )]);
        let client = fixture_client(&peer);
        let error = client
            .execute(
                "http://fixture.invalid/a",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "web URL exceeds the 8192-byte normalized URL limit");
        assert_eq!(peer.count(), 1);

        // A bounded raw Unicode input can grow during URL percent encoding.
        let url = format!("http://fixture.invalid/{}", "é".repeat(2000));
        assert!(url.len() < NATIVE_MAX_URL_BYTES);
        let error = client
            .execute(&url, &FetchRequest::get(), "web_fetch", true)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "web URL exceeds the 8192-byte normalized URL limit");
        assert_eq!(peer.count(), 1);
    }
}

#[cfg(test)]
mod web_absent_checker_tests {
    use super::*;
    use web_http_tests::{Peer, fixture_client};

    #[tokio::test]
    async fn web_http_operator_skip_uses_ordinary_permission_contract_and_keeps_url_guards() {
        let peer = Peer::new(vec![Peer::response("operator-authorized")]);
        let mut client = fixture_client(&peer);
        client.permission = None;
        let (_, response) = client
            .execute(
                "http://fixture.invalid/docs",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap();
        assert_eq!(response.text, "operator-authorized");
        assert_eq!(peer.count(), 1);
        let error = client
            .execute(
                "http://user:secret@fixture.invalid/docs",
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("embedded credentials"));
        assert_eq!(peer.count(), 1);
        client.test_resolution = None;
        let error = client
            .execute(
                &format!("http://{}/private", peer.address),
                &FetchRequest::get(),
                "web_fetch",
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a public network address"));
        assert_eq!(peer.count(), 1);
    }
}
