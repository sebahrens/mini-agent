use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use compact_str::CompactString;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::extras::mcp::client::McpClientHandle;

/// Upper bound that only distinguishes a bounded outcome from a hang. It is
/// deliberately generous so scheduler stalls during a loaded full test run
/// cannot turn a correct deadline or cancellation into a failure.
const HUNG_BOUND: Duration = Duration::from_secs(30);

/// Accept requests and drain their bytes without replying. The notification
/// lets cancellation target an actual in-flight network operation.
async fn spawn_silent_listener() -> (String, tokio::task::JoinHandle<()>, Arc<Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(Notify::new());
    let notify = received.clone();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let notify = notify.clone();
            connections.spawn(async move {
                let mut sink = [0_u8; 1024];
                while let Ok(read) = socket.read(&mut sink).await {
                    if read == 0 {
                        break;
                    }
                    notify.notify_one();
                }
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), task, received)
}

#[tokio::test]
async fn mcp_http_initialization_honors_deadlines_and_scope_cancellation() {
    // OAuth reads the startup-owned process paths. Keep unrelated path override
    // tests from swapping or deleting them while this matrix is running.
    struct CredentialRoot(std::path::PathBuf);
    impl Drop for CredentialRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let root = std::env::temp_dir().join(format!("mini-agent-mcp-http-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let root = CredentialRoot(root.canonicalize().unwrap());
    let overrides = [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.0.join(name).into_os_string())));
    let _environment = crate::tests::ScopedProcessEnv::set(&overrides);
    let paths = crate::paths::process_paths_ref().unwrap();
    assert!(paths.credentials_dir.starts_with(&root.0));
    assert!(paths.data_dir.starts_with(&root.0));
    for oauth_enabled in [false, true] {
        // The cancellation run goes first: it waits until the request reaches
        // the fixture, so it measures how long this host currently needs to
        // put an initialization request on the wire. The deadline run then
        // uses a budget scaled from that measurement, so a loaded host cannot
        // make a fixed deadline fire before the request has even been sent.
        let mut observed_arrival = Duration::ZERO;
        for cancel in [true, false] {
            let (http_url, listener, received) = spawn_silent_listener().await;
            // OAuth endpoints must be HTTPS. The silent TCP fixture does not
            // complete TLS, which is exactly what this deadline/cancellation
            // test needs: the request reaches the network and then blocks.
            let url = if oauth_enabled {
                http_url.replacen("http://", "https://", 1)
            } else {
                http_url
            };
            let oauth =
                oauth_enabled.then_some(crate::extras::mcp::config::OAuthConfig::Enabled(true));
            if let Some(oauth) = &oauth {
                let store = crate::extras::mcp::oauth::FileCredentialStore::for_paths(
                    paths,
                    "silent-server",
                    &url,
                    &oauth.settings().unwrap(),
                )
                .unwrap();
                let credentials = serde_json::from_value(serde_json::json!({
                    "client_id": "fixture-client",
                    "token_response": {"access_token": "fixture-only-access-token", "token_type": "Bearer"},
                    "granted_scopes": []
                })).unwrap();
                store.write_blocking(&credentials).unwrap();
            }
            let headers = HashMap::new();
            let initialize_timeout = if cancel {
                // Never reached: cancellation fires once the request has
                // arrived, however long the host takes to send it.
                Duration::from_secs(120)
            } else {
                // Far below the production 10 s bound, but always several
                // times what this host just needed to send the same request.
                (observed_arrival * 4).clamp(Duration::from_secs(2), Duration::from_secs(30))
            };
            let scope = crate::agent::runner::AgentWorkScope::new();
            let started = std::time::Instant::now();
            let result = {
                let connecting = scope.run(McpClientHandle::connect_http_with_timeout(
                    CompactString::new("silent-server"),
                    &url,
                    &headers,
                    oauth.as_ref(),
                    None,
                    initialize_timeout,
                ));
                tokio::pin!(connecting);
                // Generous bound that only separates "honored its deadline or
                // cancellation" from "hung"; it is never the tight edge.
                tokio::time::timeout(initialize_timeout + HUNG_BOUND, async {
                    if cancel {
                        tokio::select! {
                            result = &mut connecting => panic!("connection finished before cancellation: {}", result.is_ok()),
                            _ = received.notified() => {
                                observed_arrival = started.elapsed();
                                scope.cancellation_handle().cancel();
                            }
                        }
                        // Cancellation must stop initialization promptly, far
                        // inside its still-running 120 s initialize deadline.
                        return tokio::time::timeout(HUNG_BOUND, connecting)
                            .await
                            .expect("scope cancellation must stop HTTP initialization promptly");
                    }
                    connecting.await
                })
                .await
            };
            if !cancel {
                // The request bytes may already sit in the fixture's socket
                // buffer while its reader task has not been scheduled yet.
                // Let the reader drain them before tearing the fixture down,
                // so this proof does not race the scheduler.
                assert!(
                    tokio::time::timeout(HUNG_BOUND, received.notified())
                        .await
                        .is_ok(),
                    "the timeout must cover a real network request (OAuth: {oauth_enabled})"
                );
            }
            listener.abort();
            assert!(listener.await.is_err_and(|error| error.is_cancelled()));
            let error = result
                .expect("HTTP initialization must honor its deadline or cancellation")
                .err()
                .expect("a silent server must not produce a connected handle");
            let message = error.to_string();
            if cancel {
                assert_eq!(message, "MCP HTTP connection cancelled");
            } else {
                assert!(message.contains("silent-server"), "{message}");
                assert!(message.contains("timed out"), "{message}");
            }
        }
    }
}
