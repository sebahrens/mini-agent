use std::collections::HashMap;
use std::time::Duration;

use compact_str::CompactString;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use crate::extras::mcp::client::McpClientHandle;

/// Bind a loopback listener that accepts connections, drains whatever the
/// client sends, and never writes a byte back: a server that is reachable but
/// hung.
async fn spawn_silent_listener() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut sink = [0_u8; 1024];
                while let Ok(read) = socket.read(&mut sink).await {
                    if read == 0 {
                        break;
                    }
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), task)
}

#[tokio::test]
async fn mcp_http_connect_to_silent_server_times_out() {
    let (url, listener) = spawn_silent_listener().await;
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        McpClientHandle::connect_http_with_timeout(
            CompactString::new("silent-http"),
            &url,
            &HashMap::new(),
            None,
            Duration::from_millis(300),
        ),
    )
    .await
    .expect("HTTP connect against a silent server must be bounded")
    .err()
    .expect("a silent server must not produce a connected handle");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "connect must fail shortly after the initialize timeout"
    );
    let message = error.to_string();
    assert!(message.contains("silent-http"), "{message}");
    assert!(message.contains("timed out"), "{message}");
    listener.abort();
}

#[tokio::test]
async fn mcp_http_oauth_connect_to_silent_server_times_out() {
    // OAuth resolves the same process paths installed by startup. Hold the
    // test environment lock so unrelated path-override fixtures cannot swap
    // those inputs or delete their temporary roots during this connection.
    let _environment = crate::tests::ScopedProcessEnv::set(&[]);
    let (url, listener) = spawn_silent_listener().await;
    let oauth = crate::extras::mcp::config::OAuthConfig::Enabled(true);
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        McpClientHandle::connect_http_with_timeout(
            CompactString::new("silent-oauth"),
            &url,
            &HashMap::new(),
            Some(&oauth),
            Duration::from_millis(300),
        ),
    )
    .await
    .expect("OAuth HTTP connect against a silent server must be bounded")
    .err()
    .expect("a silent server must not produce a connected handle");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(error.to_string().contains("silent-oauth"), "{error}");
    listener.abort();
}
