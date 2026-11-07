use super::*;

fn rules(entries: &[&str]) -> Vec<HostRule> {
    parse_rules(&entries.iter().map(|e| e.to_string()).collect::<Vec<_>>()).unwrap()
}

#[test]
fn host_rules_match_exact_names_subdomains_and_ports_only() {
    let list = rules(&["registry.npmjs.org", "*.github.com", "git.example.com:8443"]);
    let allowed = |host: &str, port: u16| list.iter().any(|rule| rule.matches(host, port));
    assert!(allowed("registry.npmjs.org", 443));
    assert!(allowed("registry.npmjs.org", 80));
    assert!(
        !allowed("registry.npmjs.org", 22),
        "default ports are 80 and 443"
    );
    assert!(!allowed("evilregistry.npmjs.org", 443));
    assert!(!allowed("registry.npmjs.org.evil.com", 443));
    assert!(allowed("api.github.com", 443));
    assert!(allowed("a.b.github.com", 443));
    assert!(!allowed("github.com", 443), "*. matches subdomains only");
    assert!(!allowed("notgithub.com", 443));
    assert!(!allowed("xgithub.com", 443));
    assert!(allowed("git.example.com", 8443));
    assert!(!allowed("git.example.com", 443));
}

#[test]
fn malformed_host_rules_are_rejected() {
    for bad in [
        "",
        "*",
        "*.com",
        "https://a.com",
        "a.com/path",
        "a b.com",
        "a.com:0",
        "a.com:x",
        "ex*ample.com",
        "[::1]",
        "a..com",
        "ünï.com",
        "a.com:99999",
    ] {
        assert!(HostRule::parse(bad).is_err(), "{bad:?} should be refused");
    }
    let many: Vec<String> = (0..=MAX_RULES)
        .map(|i| format!("h{i}.example.com"))
        .collect();
    assert!(parse_rules(&many).is_err());
    assert_eq!(HostRule::parse("EXAMPLE.com").unwrap().host, "example.com");
}

#[test]
fn non_public_destinations_are_forbidden() {
    for ip in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "168.63.129.16",
        "::ffff:168.63.129.16",
        "0.0.0.0",
        "100.64.0.1",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "fd12::1",
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
        "ff02::1",
    ] {
        assert!(is_forbidden_ip(ip.parse().unwrap()), "{ip}");
    }
    for ip in [
        "8.8.8.8",
        "140.82.112.3",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
    ] {
        assert!(!is_forbidden_ip(ip.parse().unwrap()), "{ip}");
    }
}

#[test]
fn ipv6_transition_addresses_embedding_forbidden_ipv4_are_forbidden() {
    for ip in [
        "64:ff9b::a9fe:a9fe", // NAT64 -> 169.254.169.254
        "64:ff9b::7f00:1",    // NAT64 -> 127.0.0.1
        "64:ff9b::a00:1",     // NAT64 -> 10.0.0.1
        "64:ff9b:1::1",       // local-use NAT64
        "64:ff9b:1:abcd::8.8.8.8",
        "2002:7f00:1::",     // 6to4 -> 127.0.0.1
        "2002:a9fe:a9fe::1", // 6to4 -> 169.254.169.254
        "2002:c0a8:101::",   // 6to4 -> 192.168.1.1
        "::127.0.0.1",       // IPv4-compatible
        "::169.254.169.254",
        "2001:0:a9fe:a9fe::1", // Teredo server in the metadata range
        "2001:0:808:808::1",   // Teredo: client address is obfuscated
        "198.18.0.1",
        "198.19.255.255",
        "192.0.0.8",
        "64:ff9b::c612:1", // NAT64 -> 198.18.0.1
    ] {
        assert!(is_forbidden_ip(ip.parse().unwrap()), "{ip}");
    }
    for ip in [
        "64:ff9b::808:808", // NAT64 -> 8.8.8.8
        "2002:808:808::1",  // 6to4 -> 8.8.8.8
        "198.17.255.255",
        "198.20.0.1",
        "192.0.1.1",
    ] {
        assert!(!is_forbidden_ip(ip.parse().unwrap()), "{ip}");
    }
}

#[test]
fn requests_parse_to_the_destination_that_is_judged() {
    let connect =
        parse_request(b"CONNECT Registry.NPMJS.org.:443 HTTP/1.1\r\nHost: x\r\nProxy-Authorization: Basic abc\r\n\r\n")
            .unwrap();
    assert!(connect.connect);
    assert_eq!(
        (connect.host.as_str(), connect.port),
        ("registry.npmjs.org", 443)
    );
    assert_eq!(connect.auth.as_deref(), Some("Basic abc"));

    let get = parse_request(
        b"GET http://example.com:8080/a/b?q=1 HTTP/1.1\r\nHost: other.example\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nX-A: 1\r\n\r\n",
    )
    .unwrap();
    assert_eq!((get.host.as_str(), get.port), ("example.com", 8080));
    let forwarded = String::from_utf8(get.forward_head).unwrap();
    assert!(
        forwarded.starts_with("GET /a/b?q=1 HTTP/1.1\r\n"),
        "{forwarded}"
    );
    assert!(forwarded.contains("X-A: 1") && forwarded.ends_with("Connection: close\r\n\r\n"));
    assert!(
        !forwarded.to_ascii_lowercase().contains("proxy-"),
        "{forwarded}"
    );

    // The client's mismatched Host never reaches the origin: the vetted URI
    // authority is the only Host forwarded.
    assert!(
        forwarded.contains("\r\nHost: example.com:8080\r\n"),
        "{forwarded}"
    );
    assert!(!forwarded.contains("other.example"), "{forwarded}");

    let query_only = parse_request(b"GET http://example.com?x=1 HTTP/1.1\r\n\r\n").unwrap();
    assert!(
        String::from_utf8(query_only.forward_head)
            .unwrap()
            .starts_with("GET /?x=1 ")
    );
    let v6 = parse_request(b"CONNECT [2606:4700::1111]:443 HTTP/1.1\r\n\r\n").unwrap();
    assert_eq!(v6.host, "2606:4700::1111");

    for bad in [
        &b"GET ftp://a.com/ HTTP/1.1\r\n\r\n"[..],
        b"GET /relative HTTP/1.1\r\n\r\n",
        b"GET http://user:pw@a.com/ HTTP/1.1\r\n\r\n",
        b"CONNECT a.com:0 HTTP/1.1\r\n\r\n",
        b"CONNECT a.com:443 HTTP/2\r\n\r\n",
        b"CONNECT\r\n\r\n",
    ] {
        assert!(
            parse_request(bad).is_err(),
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
}

async fn exchange(proxy: &NetworkProxy, request: &str) -> String {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    let _ =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;
    response
}

fn auth(proxy: &NetworkProxy) -> String {
    let url = proxy
        .env()
        .into_iter()
        .find(|(k, _)| k == "HTTPS_PROXY")
        .unwrap()
        .1;
    let credentials = url
        .strip_prefix("http://")
        .unwrap()
        .split('@')
        .next()
        .unwrap()
        .to_string();
    format!(
        "Proxy-Authorization: Basic {}\r\n",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

#[tokio::test]
async fn proxy_requires_its_token_and_refuses_unlisted_and_loopback_destinations() {
    // `localhost` is listed on purpose: the allowlist cannot reach loopback.
    let proxy = NetworkProxy::start(
        rules(&["localhost", "127.0.0.1", "169.254.169.254", "10.0.0.1"]),
        Arc::default(),
    )
    .unwrap();
    assert!(
        proxy
            .seatbelt_rule()
            .contains(&format!("localhost:{}", proxy.port()))
    );
    assert!(
        proxy
            .env()
            .iter()
            .any(|(k, v)| k == "http_proxy" && v.contains("127.0.0.1"))
    );
    assert!(
        format!("{proxy:?}").find("mini-agent:").is_none(),
        "token must not be in Debug"
    );

    let no_auth = exchange(&proxy, "CONNECT localhost:443 HTTP/1.1\r\n\r\n").await;
    assert!(no_auth.starts_with("HTTP/1.1 407"), "{no_auth}");
    let wrong = exchange(
        &proxy,
        "CONNECT localhost:443 HTTP/1.1\r\nProxy-Authorization: Basic bm9wZTpub3Bl\r\n\r\n",
    )
    .await;
    assert!(wrong.starts_with("HTTP/1.1 407"), "{wrong}");

    // Not listed and nobody to ask: refused before any connection.
    let unlisted = exchange(
        &proxy,
        &format!(
            "CONNECT unlisted.example.net:443 HTTP/1.1\r\n{}\r\n",
            auth(&proxy)
        ),
    )
    .await;
    assert!(unlisted.starts_with("HTTP/1.1 403"), "{unlisted}");
    assert!(
        unlisted.contains("not in sandbox-network-hosts"),
        "{unlisted}"
    );

    // Listed but loopback (and the literal IPs of metadata/private ranges).
    for target in [
        "localhost:443",
        "127.0.0.1:443",
        "169.254.169.254:80",
        "10.0.0.1:443",
    ] {
        let response = exchange(
            &proxy,
            &format!("CONNECT {target} HTTP/1.1\r\n{}\r\n", auth(&proxy)),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{target}: {response}");
    }
    // A plain-HTTP request to a loopback server is refused the same way.
    let plain = exchange(
        &proxy,
        &format!("GET http://localhost/ HTTP/1.1\r\n{}\r\n", auth(&proxy)),
    )
    .await;
    assert!(plain.starts_with("HTTP/1.1 403"), "{plain}");
}

fn shared_with_prompt(
    rules: Vec<HostRule>,
    decision: Option<UserDecision>,
) -> (Arc<Shared>, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let prompt: Arc<Mutex<Option<AskSender>>> = Arc::default();
    if let Some(decision) = decision {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<UserRequest>(4);
        *prompt.lock().unwrap() = Some(tx);
        let log = asked.clone();
        tokio::spawn(async move {
            while let Some(UserRequest::Permission(ask)) = rx.recv().await {
                assert_eq!(ask.tool.as_str(), "sandbox_network");
                log.lock().unwrap().push(ask.input.clone());
                let _ = ask.reply.send(decision.clone());
            }
        });
    }
    let shared = Shared {
        rules,
        session: Mutex::default(),
        prompt,
        inflight: Mutex::default(),
        denied: Mutex::default(),
        expected_auth: String::new(),
    };
    (Arc::new(shared), asked)
}

#[tokio::test]
async fn unlisted_hosts_prompt_and_always_is_remembered_for_the_session() {
    let (shared, asked) = shared_with_prompt(
        rules(&["listed.example"]),
        Some(UserDecision::AllowAlways("x".into())),
    );
    shared.decide("listed.example", 443).await.unwrap();
    assert!(
        asked.lock().unwrap().is_empty(),
        "listed hosts never prompt"
    );
    shared.decide("new.example", 443).await.unwrap();
    shared.decide("new.example", 443).await.unwrap();
    assert_eq!(asked.lock().unwrap().as_slice(), ["new.example:443"]);
    // A different port is a different approval.
    shared.decide("new.example", 8443).await.unwrap();
    assert_eq!(asked.lock().unwrap().len(), 2);

    let (once, asked) = shared_with_prompt(Vec::new(), Some(UserDecision::AllowOnce));
    once.decide("a.example", 443).await.unwrap();
    once.decide("a.example", 443).await.unwrap();
    assert_eq!(
        asked.lock().unwrap().len(),
        2,
        "allow-once is not remembered"
    );

    let (denied, _) = shared_with_prompt(Vec::new(), Some(UserDecision::Deny));
    assert!(denied.decide("a.example", 443).await.is_err());
    // Default closed: with nobody to ask an empty list allows nothing.
    let (silent, _) = shared_with_prompt(Vec::new(), None);
    assert!(
        silent
            .decide("a.example", 443)
            .await
            .unwrap_err()
            .contains("sandbox-network-hosts")
    );
}

#[cfg(unix)]
async fn exchange_socket(path: &std::path::Path, request: &str) -> String {
    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    let _ =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;
    response
}

/// The Unix socket (Linux bwrap route) is the same filtering proxy: it needs
/// the token, refuses unlisted and non-public destinations, is private to the
/// owner, and disappears with the proxy.
#[cfg(unix)]
#[tokio::test]
async fn unix_socket_route_applies_the_same_token_and_destination_rules() {
    use std::os::unix::fs::PermissionsExt;
    let proxy =
        NetworkProxy::start_with_socket(rules(&["localhost", "169.254.169.254"]), Arc::default())
            .unwrap();
    let path = proxy.socket_path().unwrap().to_path_buf();
    // No host loopback listener: the token alone must not make it reachable.
    assert_eq!(proxy.port(), 0, "socket-only proxy must not bind TCP");
    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700, "socket directory must be private");
    assert!(
        NetworkProxy::start(rules(&[]), Arc::default())
            .unwrap()
            .socket_path()
            .is_none()
    );

    let no_auth = exchange_socket(&path, "CONNECT localhost:443 HTTP/1.1\r\n\r\n").await;
    assert!(no_auth.starts_with("HTTP/1.1 407"), "{no_auth}");
    let wrong = exchange_socket(
        &path,
        "CONNECT localhost:443 HTTP/1.1\r\nProxy-Authorization: Basic bm9wZTpub3Bl\r\n\r\n",
    )
    .await;
    assert!(wrong.starts_with("HTTP/1.1 407"), "{wrong}");
    // The forwarder-side environment carries the same credential.
    let forwarder_env = proxy.env_for_forwarder();
    let url = &forwarder_env
        .iter()
        .find(|(k, _)| k == "https_proxy")
        .unwrap()
        .1;
    assert!(
        url.ends_with(&format!("@127.0.0.1:{FORWARDER_PORT}")),
        "{url}"
    );

    let header = auth(&proxy);
    let loopback = exchange_socket(
        &path,
        &format!("CONNECT localhost:443 HTTP/1.1\r\n{header}\r\n"),
    )
    .await;
    assert!(loopback.starts_with("HTTP/1.1 403"), "{loopback}");
    let metadata = exchange_socket(
        &path,
        &format!("CONNECT 169.254.169.254:80 HTTP/1.1\r\n{header}\r\n"),
    )
    .await;
    assert!(metadata.starts_with("HTTP/1.1 403"), "{metadata}");
    let unlisted = exchange_socket(
        &path,
        &format!("CONNECT unlisted.example.net:443 HTTP/1.1\r\n{header}\r\n"),
    )
    .await;
    assert!(unlisted.starts_with("HTTP/1.1 403"), "{unlisted}");
    assert!(
        unlisted.contains("not in sandbox-network-hosts"),
        "{unlisted}"
    );

    drop(proxy);
    // The listener thread keeps its Arc-free state; the guard is gone with the
    // last handle, so the directory must be removed.
    assert!(!path.exists());
}

#[test]
fn accept_errors_back_off_between_50_and_500_ms_and_log_once_per_streak() {
    let mut backoff = AcceptBackoff::default();
    let steps: Vec<(Duration, bool)> = (0..8).map(|_| backoff.failed()).collect();
    assert_eq!(steps[0], (Duration::from_millis(50), true));
    assert_eq!(steps[1], (Duration::from_millis(100), false));
    assert_eq!(steps[4].0, Duration::from_millis(500));
    assert!(steps.iter().all(|(d, _)| *d <= Duration::from_millis(500)));
    assert_eq!(steps.iter().filter(|(_, first)| *first).count(), 1);
    backoff.succeeded();
    assert_eq!(backoff.failed(), (Duration::from_millis(50), true));
    for _ in 0..100 {
        assert!(backoff.failed().0 <= Duration::from_millis(500));
    }
}

mod control_channel {
    use super::*;
    use crate::headless_control::{ControlState, classify_line};

    fn control() -> ControlState {
        ControlState::new(
            crate::print::HeadlessOutputPolicy::legacy(false, false),
            None,
            Duration::from_secs(30),
        )
    }

    fn answer(state: &ControlState, id: &str, inner: &str) {
        let line = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{id}","response":{inner}}}}}"#
        );
        state.handle(classify_line(&line).unwrap());
    }

    async fn first_pending(state: &ControlState) -> String {
        loop {
            if let Some(id) = state.pending_ids().into_iter().next() {
                return id;
            }
            tokio::task::yield_now().await;
        }
    }

    fn headless_shared() -> Arc<Shared> {
        shared_with_prompt(Vec::new(), None).0
    }

    #[tokio::test]
    async fn five_parallel_connects_to_one_host_make_one_control_request() {
        let shared = headless_shared();
        let state = Arc::new(control());
        let tasks: Vec<_> = (0..5)
            .map(|_| {
                let (shared, state) = (shared.clone(), state.clone());
                tokio::spawn(
                    async move { shared.decide_with("pkg.example", 443, Some(&state)).await },
                )
            })
            .collect();
        let id = first_pending(&state).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(state.pending_ids(), vec![id.clone()], "exactly one ask");
        answer(&state, &id, r#"{"behavior":"allow"}"#);
        for task in tasks {
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn a_plain_allow_is_for_the_waiting_connections_only() {
        let shared = headless_shared();
        let state = Arc::new(control());
        let first = tokio::spawn({
            let (shared, state) = (shared.clone(), state.clone());
            async move { shared.decide_with("a.example", 443, Some(&state)).await }
        });
        let id = first_pending(&state).await;
        answer(&state, &id, r#"{"behavior":"allow"}"#);
        first.await.unwrap().unwrap();
        // Default closed: the next connection asks again.
        let second = tokio::spawn({
            let (shared, state) = (shared.clone(), state.clone());
            async move { shared.decide_with("a.example", 443, Some(&state)).await }
        });
        let id = first_pending(&state).await;
        answer(&state, &id, r#"{"behavior":"deny","message":"no"}"#);
        assert!(second.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_run_scoped_allow_is_remembered_for_that_host_port_only() {
        let shared = headless_shared();
        let state = Arc::new(control());
        let task = tokio::spawn({
            let (shared, state) = (shared.clone(), state.clone());
            async move { shared.decide_with("a.example", 443, Some(&state)).await }
        });
        let id = first_pending(&state).await;
        answer(&state, &id, r#"{"behavior":"allow","scope":"run"}"#);
        task.await.unwrap().unwrap();
        shared
            .decide_with("a.example", 443, Some(&state))
            .await
            .unwrap();
        assert!(state.pending_ids().is_empty(), "no second ask");
        // Another port and another host are not covered.
        let other = tokio::spawn({
            let (shared, state) = (shared.clone(), state.clone());
            async move { shared.decide_with("a.example", 8443, Some(&state)).await }
        });
        let id = first_pending(&state).await;
        answer(&state, &id, r#"{"behavior":"deny"}"#);
        assert!(other.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_denial_is_remembered_briefly_and_one_host_does_not_block_another() {
        let shared = headless_shared();
        let state = Arc::new(control());
        // An unanswered ask for one host...
        let stuck = tokio::spawn({
            let (shared, state) = (shared.clone(), state.clone());
            async move { shared.decide_with("slow.example", 443, Some(&state)).await }
        });
        let slow = first_pending(&state).await;
        // ...does not hold up another host.
        let other = tokio::spawn({
            let (shared, state) = (shared.clone(), state.clone());
            async move { shared.decide_with("fast.example", 443, Some(&state)).await }
        });
        let fast = loop {
            if let Some(id) = state.pending_ids().into_iter().find(|id| *id != slow) {
                break id;
            }
            tokio::task::yield_now().await;
        };
        answer(&state, &fast, r#"{"behavior":"deny"}"#);
        assert!(other.await.unwrap().is_err());
        // The denial is remembered: no new ask for fast.example.
        assert!(
            shared
                .decide_with("fast.example", 443, Some(&state))
                .await
                .is_err()
        );
        assert_eq!(state.pending_ids(), vec![slow.clone()]);
        answer(&state, &slow, r#"{"behavior":"allow"}"#);
        stuck.await.unwrap().unwrap();
        // The memory expires.
        shared.denied.lock().unwrap().insert(
            "fast.example:443".into(),
            std::time::Instant::now() - DENY_MEMORY,
        );
        let again = tokio::spawn({
            let (shared, state) = (shared.clone(), state.clone());
            async move { shared.decide_with("fast.example", 443, Some(&state)).await }
        });
        let id = first_pending(&state).await;
        answer(&state, &id, r#"{"behavior":"allow"}"#);
        again.await.unwrap().unwrap();
    }
}

#[test]
fn client_host_headers_are_replaced_by_the_vetted_uri_authority() {
    for (request, expected) in [
        // Different case and spacing of the header name, duplicates and a
        // Host that names another virtual host on the same front end.
        (
            &b"GET http://allowed.example/ HTTP/1.1\r\nHost: other.example\r\n\r\n"[..],
            "allowed.example",
        ),
        (
            &b"GET http://allowed.example/p HTTP/1.1\r\nhOsT : other.example\r\nHOST: third.example\r\n\r\n"[..],
            "allowed.example",
        ),
        (
            &b"GET http://allowed.example:8080/ HTTP/1.1\r\nX-A: 1\r\nHost: allowed.example:80\r\n\r\n"[..],
            "allowed.example:8080",
        ),
        (&b"GET http://allowed.example/ HTTP/1.1\r\n\r\n"[..], "allowed.example"),
    ] {
        let parsed = parse_request(request).unwrap();
        let forwarded = String::from_utf8(parsed.forward_head).unwrap();
        let hosts: Vec<_> = forwarded
            .split("\r\n")
            .filter(|line| line.to_ascii_lowercase().starts_with("host"))
            .collect();
        assert_eq!(hosts, [format!("Host: {expected}")], "{forwarded}");
        assert!(!forwarded.contains("other.example") && !forwarded.contains("third.example"));
    }
}
