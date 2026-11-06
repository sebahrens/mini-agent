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
        prompt_lock: tokio::sync::Mutex::new(()),
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
