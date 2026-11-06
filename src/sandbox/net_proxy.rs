//! `sandbox-network = "allowlist"`: a loopback HTTP proxy that is the only
//! network a contained shell command can reach.
//!
//! The Seatbelt profile denies all network access except outbound connections
//! to this proxy's loopback port, and the command environment points
//! `HTTP(S)_PROXY` at it. The proxy forwards `CONNECT host:port` tunnels and
//! plain `http://` requests, but only to hosts that match
//! `sandbox-network-hosts`, were approved earlier in the session, or are
//! approved at a prompt right now. It resolves names itself and refuses
//! loopback, private, link-local and other non-public destinations, so a
//! listed name cannot be pointed back at the machine or its metadata service.
//! Every request must carry a per-process token, so another local process
//! cannot borrow the proxy.
//!
//! Linux `bwrap` cannot reach a host loopback port (the command runs in an
//! empty network namespace), so on that backend the proxy also listens on a
//! private Unix socket. The socket is bind-mounted into the namespace, where
//! `net_forwarder` re-exposes it as `127.0.0.1:FORWARDER_PORT` for the
//! command's proxy variables. Filtering, prompting and the token check are the
//! same single code path; the namespace has no other route out.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::permission::ask::{AskRequest, AskSender, UserDecision, UserRequest};

const MAX_RULES: usize = 128;
const MAX_HEAD_BYTES: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Ports a rule without an explicit `:port` covers.
const DEFAULT_PORTS: [u16; 2] = [80, 443];

/// One `sandbox-network-hosts` entry: `host`, `*.suffix` or either with
/// `:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    host: String,
    subdomains_only: bool,
    port: Option<u16>,
}

impl HostRule {
    pub fn parse(entry: &str) -> Result<Self, String> {
        let invalid = |why: &str| format!("sandbox-network-hosts: invalid entry {entry:?}: {why}");
        let lower = entry.trim().to_ascii_lowercase();
        if lower.is_empty() || lower.len() > 253 {
            return Err(invalid("empty or too long"));
        }
        let (host_part, port) = match lower.rsplit_once(':') {
            Some((host, port)) => (
                host,
                Some(
                    port.parse::<u16>()
                        .ok()
                        .filter(|port| *port != 0)
                        .ok_or_else(|| invalid("bad port"))?,
                ),
            ),
            None => (lower.as_str(), None),
        };
        let (host, subdomains_only) = match host_part.strip_prefix("*.") {
            Some(rest) => (rest, true),
            None => (host_part, false),
        };
        let valid_label = |label: &str| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        if !host.split('.').all(valid_label) {
            return Err(invalid(
                "expected an ASCII host name such as registry.npmjs.org or *.github.com (no scheme, path, wildcard elsewhere or IPv6)",
            ));
        }
        if subdomains_only && !host.contains('.') {
            return Err(invalid(
                "a wildcard needs a registrable suffix like *.example.com",
            ));
        }
        Ok(Self {
            host: host.to_string(),
            subdomains_only,
            port,
        })
    }

    fn matches(&self, host: &str, port: u16) -> bool {
        let port_ok = match self.port {
            Some(expected) => expected == port,
            None => DEFAULT_PORTS.contains(&port),
        };
        port_ok
            && if self.subdomains_only {
                host.len() > self.host.len() + 1
                    && host.ends_with(&self.host)
                    && host.as_bytes()[host.len() - self.host.len() - 1] == b'.'
            } else {
                host == self.host
            }
    }
}

/// Backoff for a listener whose `accept` keeps failing (EMFILE, ENFILE,
/// ENOMEM): sleep 50 ms doubling to 500 ms instead of spinning a core, and
/// report only the first failure of a streak so it can be logged once.
#[derive(Debug, Default)]
pub(crate) struct AcceptBackoff {
    failures: u32,
}

impl AcceptBackoff {
    const MIN: Duration = Duration::from_millis(50);
    const MAX: Duration = Duration::from_millis(500);

    /// Records a failure; returns how long to sleep and whether this is the
    /// first failure since the last success.
    pub(crate) fn failed(&mut self) -> (Duration, bool) {
        let first = self.failures == 0;
        let delay = Self::MIN
            .saturating_mul(1u32 << self.failures.min(10))
            .min(Self::MAX);
        self.failures = self.failures.saturating_add(1);
        (delay, first)
    }

    pub(crate) fn succeeded(&mut self) {
        self.failures = 0;
    }
}

/// Parse and bound the configured list.
pub fn parse_rules(entries: &[String]) -> Result<Vec<HostRule>, String> {
    if entries.len() > MAX_RULES {
        return Err(format!(
            "sandbox-network-hosts accepts at most {MAX_RULES} entries"
        ));
    }
    entries.iter().map(|entry| HostRule::parse(entry)).collect()
}

/// Destinations the proxy never connects to, whatever the allowlist says.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => forbidden_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return forbidden_v4(mapped);
            }
            if embedded_prefix_always_forbidden(v6) {
                return true;
            }
            if let Some(embedded) = embedded_v4(v6) {
                return forbidden_v4(embedded);
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link local
                || v6 == Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0)
        }
    }
}

/// The IPv4 address an IPv6 transition address carries: NAT64
/// (`64:ff9b::/96`), 6to4 (`2002::/16`), Teredo's server (`2001::/32`) and the
/// deprecated IPv4-compatible `::a.b.c.d`. A host with such a route would
/// deliver the connection to the embedded IPv4 destination.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    let v4 = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return Some(v4(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return Some(v4(s[1], s[2]));
    }
    if s[0] == 0x2001 && s[1] == 0 {
        return Some(v4(s[2], s[3]));
    }
    if s[..6] == [0; 6] {
        return Some(v4(s[6], s[7]));
    }
    None
}

/// Transition prefixes refused whatever they embed: local-use NAT64
/// (`64:ff9b:1::/48`, embedding is prefix-length dependent) and Teredo, whose
/// client address is obfuscated.
fn embedded_prefix_always_forbidden(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1) || (s[0] == 0x2001 && s[1] == 0)
}

fn forbidden_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..=127).contains(&b)) // carrier-grade NAT
        || (a == 198 && (b == 18 || b == 19)) // benchmarking 198.18.0.0/15
        || ip.octets()[..3] == [192, 0, 0] // IETF protocol assignments 192.0.0.0/24
        || a >= 240
}

struct Shared {
    rules: Vec<HostRule>,
    session: Mutex<HashSet<String>>,
    prompt: Arc<Mutex<Option<AskSender>>>,
    /// Asks in progress, by `host:port`; later connections wait on the answer.
    inflight: Mutex<HashMap<String, tokio::sync::watch::Receiver<Option<Verdict>>>>,
    /// Refusals remembered for `DENY_MEMORY`.
    denied: Mutex<HashMap<String, std::time::Instant>>,
    expected_auth: String,
}

/// The running proxy. Lives for the rest of the process.
pub struct NetworkProxy {
    port: u16,
    token: String,
    /// Private Unix socket for namespaces without host loopback (Linux bwrap).
    #[cfg(unix)]
    socket: Option<UnixSocket>,
}

/// A 0700 directory holding the proxy's Unix socket, removed on drop.
#[cfg(unix)]
struct UnixSocket {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for UnixSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(unix)]
fn bind_private_socket() -> std::io::Result<(UnixSocket, std::os::unix::net::UnixListener)> {
    use std::os::unix::fs::DirBuilderExt;
    // `sun_path` holds about 100 bytes; a long TMPDIR falls back to /tmp.
    let name = format!(
        "mini-agent-net-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..16]
    );
    let mut dir = std::env::temp_dir().join(&name);
    if dir.as_os_str().len() > 80 {
        dir = std::path::PathBuf::from("/tmp").join(&name);
    }
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let path = dir.join("proxy.sock");
    let guard = UnixSocket {
        dir,
        path: path.clone(),
    };
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    Ok((guard, listener))
}

/// Loopback port the in-namespace forwarder listens on (private namespace).
#[cfg(unix)]
pub const FORWARDER_PORT: u16 = 3128;

impl std::fmt::Debug for NetworkProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NetworkProxy")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl NetworkProxy {
    /// Bind `127.0.0.1:0` and serve on a dedicated thread, independent of the
    /// caller's runtime.
    pub fn start(
        rules: Vec<HostRule>,
        prompt: Arc<Mutex<Option<AskSender>>>,
    ) -> std::io::Result<Arc<Self>> {
        Self::start_inner(rules, prompt, false)
    }

    /// As [`Self::start`], and also listen on a private Unix socket for
    /// network namespaces that cannot reach host loopback.
    #[cfg(unix)]
    pub fn start_with_socket(
        rules: Vec<HostRule>,
        prompt: Arc<Mutex<Option<AskSender>>>,
    ) -> std::io::Result<Arc<Self>> {
        Self::start_inner(rules, prompt, true)
    }

    fn start_inner(
        rules: Vec<HostRule>,
        prompt: Arc<Mutex<Option<AskSender>>>,
        with_socket: bool,
    ) -> std::io::Result<Arc<Self>> {
        #[cfg(not(unix))]
        let _ = with_socket;
        #[cfg(unix)]
        let (socket, unix_listener) = if with_socket {
            let (guard, listener) = bind_private_socket()?;
            (Some(guard), Some(listener))
        } else {
            (None, None)
        };
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let expected_auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("mini-agent:{token}"))
        );
        let shared = Arc::new(Shared {
            rules,
            session: Mutex::new(HashSet::new()),
            prompt,
            inflight: Mutex::default(),
            denied: Mutex::default(),
            expected_auth,
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("sandbox-net-proxy".into())
            .spawn(move || {
                runtime.block_on(async move {
                    #[cfg(unix)]
                    if let Some(unix_listener) = unix_listener
                        && let Ok(unix_listener) = tokio::net::UnixListener::from_std(unix_listener)
                    {
                        let shared = shared.clone();
                        tokio::spawn(async move {
                            let mut backoff = AcceptBackoff::default();
                            loop {
                                let Ok((client, _)) = unix_listener.accept().await else {
                                    let (delay, first) = backoff.failed();
                                    if first {
                                        tracing::warn!(
                                            "sandbox network proxy: unix accept failed; backing off"
                                        );
                                    }
                                    tokio::time::sleep(delay).await;
                                    continue;
                                };
                                backoff.succeeded();
                                tokio::spawn(handle(client, shared.clone()));
                            }
                        });
                    }
                    let Ok(listener) = TcpListener::from_std(listener) else {
                        return;
                    };
                    let mut backoff = AcceptBackoff::default();
                    loop {
                        let Ok((client, _)) = listener.accept().await else {
                            let (delay, first) = backoff.failed();
                            if first {
                                tracing::warn!("sandbox network proxy: accept failed; backing off");
                            }
                            tokio::time::sleep(delay).await;
                            continue;
                        };
                        backoff.succeeded();
                        tokio::spawn(handle(client, shared.clone()));
                    }
                });
            })?;
        Ok(Arc::new(Self {
            port,
            token,
            #[cfg(unix)]
            socket,
        }))
    }

    /// Host path of the Unix socket, when started with one.
    #[cfg(unix)]
    pub fn socket_path(&self) -> Option<&std::path::Path> {
        self.socket.as_ref().map(|socket| socket.path.as_path())
    }

    /// Proxy variables for a command whose namespace reaches the proxy only
    /// through the forwarder on `127.0.0.1:FORWARDER_PORT`.
    #[cfg(unix)]
    pub fn env_for_forwarder(&self) -> Vec<(String, String)> {
        self.env_at(FORWARDER_PORT)
    }

    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Seatbelt rule letting a command connect to this proxy and nothing else.
    pub fn seatbelt_rule(&self) -> String {
        format!(
            "\n(allow network-outbound (remote ip \"localhost:{}\"))\n",
            self.port
        )
    }

    /// Proxy variables for the command environment.
    pub fn env(&self) -> Vec<(String, String)> {
        self.env_at(self.port)
    }

    fn env_at(&self, port: u16) -> Vec<(String, String)> {
        let url = format!("http://mini-agent:{}@127.0.0.1:{port}", self.token);
        let mut vars = Vec::new();
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            vars.push((name.to_string(), url.clone()));
            vars.push((name.to_ascii_lowercase(), url.clone()));
        }
        vars.push(("NO_PROXY".into(), String::new()));
        vars.push(("no_proxy".into(), String::new()));
        vars
    }
}

struct Request {
    host: String,
    port: u16,
    /// Origin-form request line plus headers for plain HTTP; unused by CONNECT.
    forward_head: Vec<u8>,
    auth: Option<String>,
    connect: bool,
}

/// Split a head into the request line and header lines.
fn parse_request(head: &[u8]) -> Result<Request, &'static str> {
    let text = std::str::from_utf8(head).map_err(|_| "request is not valid UTF-8")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().ok_or("empty request")?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed request line");
    };
    if !version.starts_with("HTTP/1.") {
        return Err("only HTTP/1.x proxy requests are supported");
    }
    let mut auth = None;
    let mut headers = Vec::new();
    for header in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = header.split_once(':') else {
            return Err("malformed header");
        };
        let lower = name.trim().to_ascii_lowercase();
        match lower.as_str() {
            "proxy-authorization" => auth = Some(value.trim().to_string()),
            "proxy-connection" | "connection" | "keep-alive" => {}
            _ => headers.push(header.to_string()),
        }
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(target, 443)?;
        return Ok(Request {
            host,
            port,
            forward_head: Vec::new(),
            auth,
            connect: true,
        });
    }
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))
        .ok_or("only http:// URLs and CONNECT are supported")?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    let (host, port) = split_authority(authority, 80)?;
    let mut forward = format!("{method} {path} HTTP/1.1\r\n");
    for header in headers {
        forward.push_str(&header);
        forward.push_str("\r\n");
    }
    forward.push_str("Connection: close\r\n\r\n");
    Ok(Request {
        host,
        port,
        forward_head: forward.into_bytes(),
        auth,
        connect: false,
    })
}

/// `host`, `host:port` or `[v6]:port`. Userinfo is rejected.
fn split_authority(authority: &str, default_port: u16) -> Result<(String, u16), &'static str> {
    if authority.contains('@') {
        return Err("userinfo in the target is not allowed");
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or("malformed IPv6 target")?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse().map_err(|_| "bad port")?,
            None if tail.is_empty() => default_port,
            None => return Err("malformed IPv6 target"),
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().map_err(|_| "bad port")?),
            None => (authority, default_port),
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || port == 0 || host.len() > 253 || host.contains(['/', ' ', '\0']) {
        return Err("invalid target host or port");
    }
    Ok((host, port))
}

async fn read_head<S: AsyncRead + Unpin>(client: &mut S) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let leftover = buffer.split_off(end + 4);
            return Ok((buffer, leftover));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(std::io::Error::other("request head too large"));
        }
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

async fn respond<S: AsyncWrite + Unpin>(client: &mut S, status: &str, body: &str, extra: &str) {
    let message = format!(
        "HTTP/1.1 {status}\r\n{extra}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}\n",
        body.len() + 1
    );
    let _ = client.write_all(message.as_bytes()).await;
    let _ = client.shutdown().await;
}

fn tokens_equal(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(mut client: S, shared: Arc<Shared>) {
    let Ok(Ok((head, leftover))) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await
    else {
        return;
    };
    let request = match parse_request(&head) {
        Ok(request) => request,
        Err(why) => return respond(&mut client, "400 Bad Request", why, "").await,
    };
    if !request
        .auth
        .as_deref()
        .is_some_and(|auth| tokens_equal(auth, &shared.expected_auth))
    {
        return respond(
            &mut client,
            "407 Proxy Authentication Required",
            "missing or wrong proxy credentials",
            "Proxy-Authenticate: Basic realm=\"mini-agent\"\r\n",
        )
        .await;
    }
    if let Err(why) = shared.decide(&request.host, request.port).await {
        return respond(&mut client, "403 Forbidden", &why, "").await;
    }
    let upstream = match connect_public(&request.host, request.port).await {
        Ok(stream) => stream,
        Err(why) => return respond(&mut client, "403 Forbidden", &why, "").await,
    };
    let mut upstream = upstream;
    if request.connect {
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
    } else if upstream.write_all(&request.forward_head).await.is_err() {
        return;
    }
    if !leftover.is_empty() && upstream.write_all(&leftover).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// Resolve `host` ourselves, drop non-public addresses and connect to the
/// first reachable one, so the address that was checked is the one used.
async fn connect_public(host: &str, port: u16) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("cannot resolve {host}: {error}"))?
        .collect();
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|addr| !is_forbidden_ip(addr.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(format!(
            "{host} resolves only to loopback, private or link-local addresses, which the sandbox proxy never reaches"
        ));
    }
    for addr in allowed {
        if let Ok(Ok(stream)) =
            tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await
        {
            return Ok(stream);
        }
    }
    Err(format!("could not connect to {host}:{port}"))
}

/// How long a refused or unanswered `host:port` stays refused without asking
/// again, so a retry loop does not re-prompt the driver or user per attempt.
const DENY_MEMORY: Duration = Duration::from_secs(30);

type Verdict = Result<(), String>;

/// Removes an in-flight ask when its leader finishes or is cancelled.
struct InflightGuard<'a> {
    shared: &'a Shared,
    key: &'a str,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.shared
            .inflight
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(self.key);
    }
}

impl Shared {
    async fn decide(&self, host: &str, port: u16) -> Result<(), String> {
        self.decide_with(host, port, crate::headless_control::state())
            .await
    }

    fn remembered(&self, key: &str) -> Option<Verdict> {
        if self
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(key)
        {
            return Some(Ok(()));
        }
        let mut denied = self
            .denied
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match denied.get(key) {
            Some(at) if at.elapsed() < DENY_MEMORY => Some(Err(denied_message(key))),
            Some(_) => {
                denied.remove(key);
                None
            }
            None => None,
        }
    }

    async fn decide_with(
        &self,
        host: &str,
        port: u16,
        control: Option<&crate::headless_control::ControlState>,
    ) -> Result<(), String> {
        if self.rules.iter().any(|rule| rule.matches(host, port)) {
            return Ok(());
        }
        let key = format!("{host}:{port}");
        loop {
            if let Some(verdict) = self.remembered(&key) {
                return verdict;
            }
            // One ask per host:port at a time; other hosts are not held up.
            // Connections that arrive while it is pending share its answer.
            let role = {
                let mut inflight = self
                    .inflight
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                match inflight.get(&key) {
                    Some(existing) => Err(existing.clone()),
                    None => {
                        let (tx, rx) = tokio::sync::watch::channel(None);
                        inflight.insert(key.clone(), rx);
                        Ok(tx)
                    }
                }
            };
            let tx = match role {
                Ok(tx) => tx,
                Err(mut follower) => {
                    let answer = follower.wait_for(Option::is_some).await;
                    match answer {
                        Ok(verdict) => {
                            let verdict = verdict.clone();
                            return verdict.unwrap_or_else(|| Err(denied_message(&key)));
                        }
                        // The leader was cancelled (its client went away): ask again.
                        Err(_) => continue,
                    }
                }
            };
            let _guard = InflightGuard {
                shared: self,
                key: &key,
            };
            let verdict = self.ask_once(&key, control).await;
            let _ = tx.send(Some(verdict.clone()));
            return verdict;
        }
    }

    /// Asks the driver or user about `key`, remembering the answer as far as
    /// the reply allows: an always-allow (TUI) or a run-scoped allow (driver)
    /// lasts for the session; a refusal for `DENY_MEMORY`.
    async fn ask_once(
        &self,
        key: &str,
        control: Option<&crate::headless_control::ControlState>,
    ) -> Verdict {
        let sender = self
            .prompt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let remember_allow = |this: &Self| {
            this.session
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(key.to_owned());
        };
        let deny = |this: &Self| {
            this.denied
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(key.to_owned(), std::time::Instant::now());
            Err(denied_message(key))
        };
        let Some(sender) = sender else {
            // Headless with `--stream-json-control`: the driver answers. A
            // plain allow is for the connections waiting now; only a reply
            // that says `"scope":"run"` is remembered for the rest of the run.
            if let Some(control) = control {
                return match control.ask_network(key).await {
                    crate::headless_control::Reply::Allow => Ok(()),
                    crate::headless_control::Reply::AllowForRun => {
                        remember_allow(self);
                        Ok(())
                    }
                    crate::headless_control::Reply::Deny(_) => deny(self),
                };
            }
            return Err(denied_message(key));
        };
        let (reply, answer) = tokio::sync::oneshot::channel();
        let request = AskRequest {
            diagnostic: None,
            #[cfg(feature = "subagents")]
            background: None,
            agent_name: None,
            tool: compact_str::CompactString::new("sandbox_network"),
            input: key.to_owned(),
            #[cfg(feature = "acp")]
            tool_call_id: None,
            suggested_pattern: None,
            additional_allow_patterns: Vec::new(),
            reply,
        };
        if sender.send(UserRequest::Permission(request)).await.is_err() {
            return Err(denied_message(key));
        }
        match tokio::time::timeout(PROMPT_TIMEOUT, answer).await {
            Ok(Ok(UserDecision::AllowOnce)) => Ok(()),
            Ok(Ok(UserDecision::AllowAlways(_))) => {
                remember_allow(self);
                Ok(())
            }
            _ => deny(self),
        }
    }
}

fn denied_message(key: &str) -> String {
    format!(
        "{key} is not in sandbox-network-hosts and was not approved; add it to sandbox-network-hosts to allow it"
    )
}

#[cfg(test)]
mod tests;
