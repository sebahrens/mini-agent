use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use compact_str::CompactString;
use futures::FutureExt;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::extras::mcp::client::McpClientHandle;

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
        "ZS_CONFIG_DIR",
        "ZS_DATA_DIR",
        "ZS_LOCAL_DATA_DIR",
        "ZS_STATE_DIR",
        "ZS_CACHE_DIR",
        "ZS_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.0.join(name).into_os_string())));
    let _environment = crate::tests::ScopedProcessEnv::set(&overrides);
    let paths = crate::paths::process_paths().unwrap();
    assert!(paths.credentials_dir.starts_with(&root.0));
    assert!(paths.data_dir.starts_with(&root.0));
    for oauth_enabled in [false, true] {
        for cancel in [false, true] {
            let (url, listener, received) = spawn_silent_listener().await;
            let oauth =
                oauth_enabled.then_some(crate::extras::mcp::config::OAuthConfig::Enabled(true));
            if let Some(oauth) = &oauth {
                let store = crate::extras::mcp::oauth::FileCredentialStore::for_paths(
                    &paths,
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
            let scope = crate::agent::runner::AgentWorkScope::new();
            let result = {
                let connecting = scope.run(McpClientHandle::connect_http_with_timeout(
                    CompactString::new("silent-server"),
                    &url,
                    &headers,
                    oauth.as_ref(),
                    if cancel {
                        Duration::from_secs(10)
                    } else {
                        Duration::from_millis(300)
                    },
                ));
                tokio::pin!(connecting);
                tokio::time::timeout(Duration::from_secs(5), async {
                    if cancel {
                        tokio::select! {
                            result = &mut connecting => panic!("connection finished before cancellation: {}", result.is_ok()),
                            _ = received.notified() => scope.cancellation_handle().cancel(),
                        }
                    }
                    connecting.await
                }).await
            };
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
                assert!(
                    received.notified().now_or_never().is_some(),
                    "the timeout must cover a real network request"
                );
                assert!(message.contains("silent-server"), "{message}");
                assert!(message.contains("timed out"), "{message}");
            }
        }
    }
}
