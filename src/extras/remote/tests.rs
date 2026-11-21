use super::*;

async fn http(addr: SocketAddr, request: String) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).to_string()
}

fn get(server: &RemoteServer, path: &str, auth: Option<&str>) -> String {
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    format!("GET {path} HTTP/1.1\r\nHost: {}\r\n{auth}\r\n", server.addr)
}

fn post(server: &RemoteServer, path: &str, body: &str, auth: Option<&str>) -> String {
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\n{auth}Content-Length: {}\r\n\r\n{body}",
        server.addr,
        body.len()
    )
}

#[tokio::test]
async fn default_bind_is_loopback_and_wider_binds_need_the_opt_in() {
    let server = start(RemoteOptions::default()).await.unwrap();
    assert!(server.addr.ip().is_loopback());
    assert_eq!(server.token.len(), 64);
    let wide = start(RemoteOptions {
        host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        ..Default::default()
    })
    .await;
    assert!(wide.is_err());
    assert!(wide.err().unwrap().to_string().contains("loopback"));
}

#[tokio::test]
async fn unauthenticated_and_wrong_token_requests_are_refused() {
    let server = start(RemoteOptions::default()).await.unwrap();
    let none = http(server.addr, get(&server, "/", None)).await;
    assert!(none.starts_with("HTTP/1.1 401"), "{none}");
    let wrong = http(server.addr, get(&server, "/", Some("nope"))).await;
    assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
    let steer = http(
        server.addr,
        post(&server, "/steer", r#"{"text":"x"}"#, None),
    )
    .await;
    assert!(steer.starts_with("HTTP/1.1 401"), "{steer}");
    let ok = http(server.addr, get(&server, "/", Some(&server.token))).await;
    assert!(
        ok.starts_with("HTTP/1.1 200") && ok.contains("mini-agent remote"),
        "{ok}"
    );
    let query = http(
        server.addr,
        format!(
            "GET /?token={} HTTP/1.1\r\nHost: {}\r\n\r\n",
            server.token, server.addr
        ),
    )
    .await;
    assert!(query.starts_with("HTTP/1.1 200"), "{query}");
}

#[tokio::test]
async fn foreign_host_header_is_refused_even_with_the_token() {
    let server = start(RemoteOptions::default()).await.unwrap();
    let request = format!(
        "GET / HTTP/1.1\r\nHost: evil.example\r\nAuthorization: Bearer {}\r\n\r\n",
        server.token
    );
    assert!(http(server.addr, request).await.starts_with("HTTP/1.1 403"));
}

#[tokio::test]
async fn steering_and_permission_answers_reach_the_session_once() {
    let mut server = start(RemoteOptions::default()).await.unwrap();
    let t = server.token.clone();
    let resp = http(
        server.addr,
        post(&server, "/steer", r#"{"text":"use tabs"}"#, Some(&t)),
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 202"), "{resp}");
    assert_eq!(
        server.commands.recv().await.unwrap(),
        RemoteCommand::Steer("use tabs".into())
    );
    let blank = http(
        server.addr,
        post(&server, "/steer", r#"{"text":"  "}"#, Some(&t)),
    )
    .await;
    assert!(blank.starts_with("HTTP/1.1 400"), "{blank}");

    // An unknown ask is rejected; a published one is answered exactly once.
    let unknown = http(
        server.addr,
        post(
            &server,
            "/permission",
            r#"{"id":"p1","allow":true}"#,
            Some(&t),
        ),
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 404"), "{unknown}");
    server.hub.publish_permission("p1", "shell: ls");
    let answer = post(
        &server,
        "/permission",
        r#"{"id":"p1","allow":true}"#,
        Some(&t),
    );
    assert!(
        http(server.addr, answer.clone())
            .await
            .starts_with("HTTP/1.1 202")
    );
    assert_eq!(
        server.commands.recv().await.unwrap(),
        RemoteCommand::Permission {
            id: "p1".into(),
            allow: true
        }
    );
    assert!(http(server.addr, answer).await.starts_with("HTTP/1.1 404"));
    server.hub.publish_permission("p2", "edit");
    server.hub.withdraw_permission("p2");
    let late = http(
        server.addr,
        post(
            &server,
            "/permission",
            r#"{"id":"p2","allow":true}"#,
            Some(&t),
        ),
    )
    .await;
    assert!(late.starts_with("HTTP/1.1 404"));

    let cancel = http(server.addr, post(&server, "/cancel", "{}", Some(&t))).await;
    assert!(cancel.starts_with("HTTP/1.1 202"));
    assert_eq!(server.commands.recv().await.unwrap(), RemoteCommand::Cancel);
}

#[tokio::test]
async fn events_stream_to_an_authenticated_client() {
    let server = start(RemoteOptions::default()).await.unwrap();
    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(get(&server, "/events", Some(&server.token)).as_bytes())
        .await
        .unwrap();
    let mut buf = vec![0u8; 4096];
    let mut got = String::new();
    // Wait for the connect comment so the subscription exists before publishing.
    while !got.contains(": connected") {
        let n = stream.read(&mut buf).await.unwrap();
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    server
        .hub
        .publish(&serde_json::json!({"type":"text","text":"hello"}));
    while !got.contains("hello") {
        let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(
        got.contains("text/event-stream") && got.contains("data: ") && got.contains("hello"),
        "{got}"
    );
}

#[test]
fn constant_time_compare_checks_length_and_content() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"ab"));
}
