use google_cloud_auth::credentials::{AccessTokenCredentials, Builder};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn held_fetch() -> (
    AccessTokenCredentials,
    tokio::net::TcpStream,
    crate::tests::OwnedTestDirectory,
    crate::tests::ScopedProcessEnv,
) {
    let temp = crate::tests::OwnedTestDirectory::new("vertex81-adc");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let file = temp.path().join("adc.json");
    std::fs::write(&file,json!({"type":"authorized_user","client_id":"owned-client","client_secret":"PRIVATE_CLIENT_SECRET","refresh_token":"PRIVATE_REFRESH_TOKEN","token_uri":format!("http://{}/token",listener.local_addr().unwrap())}).to_string()).unwrap();
    let env = crate::tests::ScopedProcessEnv::set(&[
        (
            "GOOGLE_APPLICATION_CREDENTIALS",
            Some(file.into_os_string()),
        ),
        ("NO_PROXY", Some("*".into())),
        ("no_proxy", Some("*".into())),
    ]);
    let credentials = Builder::default().build_access_token_credentials().unwrap();
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    read_fetch(&mut socket).await;
    (credentials, socket, temp, env)
}
async fn read_fetch(socket: &mut tokio::net::TcpStream) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32768);
        bytes.push(socket.read_u8().await.unwrap());
    }
    let headers = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
    assert!(headers.starts_with("post /token http/1.1"));
    let len: usize = headers
        .lines()
        .find_map(|l| {
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap();
    assert!(len < 32768);
    let mut body = vec![0; len];
    socket.read_exact(&mut body).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["grant_type"], "refresh_token");
}
async fn closed(socket: &mut tokio::net::TcpStream) {
    let mut byte = [0];
    let n = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
        .await
        .expect("SDK credential fetch retained IO after client teardown")
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn vertex81_adc_drop_last_client_cancels_held_fetch() {
    let (credentials, mut socket, _temp, _env) = held_fetch().await;
    drop(credentials);
    closed(&mut socket).await;
}

#[tokio::test]
async fn vertex81_adc_token_fetch_30_second_deadline_is_real_not_a_waiter_timeout() {
    let (credentials, mut socket, _temp, _env) = held_fetch().await;
    let mut waiter = Box::pin(credentials.access_token());
    assert!(matches!(
        futures::poll!(waiter.as_mut()),
        std::task::Poll::Pending
    ));
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(matches!(
        futures::poll!(waiter.as_mut()),
        std::task::Poll::Pending
    ));
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    let error = tokio::time::timeout(Duration::from_secs(2), waiter.as_mut())
        .await
        .expect("SDK did not publish its fetch deadline")
        .unwrap_err();
    assert!(error.to_string().contains("30-second deadline"), "{error}");
    assert!(!error.to_string().contains("PRIVATE_"));
    closed(&mut socket).await;
}

#[test]
fn vertex81_adc_rejects_direct_and_nested_executable_sources_before_spawning() {
    let temp = crate::tests::OwnedTestDirectory::new("vertex81-no-exec");
    let file = temp.path().join("adc.json");
    let marker = temp.path().join("must-not-exist");
    let _env = crate::tests::ScopedProcessEnv::set(&[
        (
            "GOOGLE_APPLICATION_CREDENTIALS",
            Some(file.clone().into_os_string()),
        ),
        (
            "GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES",
            Some("1".into()),
        ),
    ]);
    for nested in [false, true] {
        let source = json!({"type":"external_account","credential_source":{"executable":{"command":format!("PRIVATE_COMMAND {}",marker.display())}}});
        let value = if nested {
            json!({"type":"impersonated_service_account","source_credentials":source})
        } else {
            source
        };
        std::fs::write(&file, value.to_string()).unwrap();
        let error = Builder::default()
            .build_access_token_credentials()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("executable ADC credential sources are unsupported")
        );
        assert!(!error.to_string().contains("PRIVATE_COMMAND"));
        assert!(!marker.exists());
    }
}

#[tokio::test]
async fn vertex81_adc_standard_user_refresh_replaces_bearer_and_retains_quota() {
    use google_cloud_auth::credentials::CacheableResource;
    let temp = crate::tests::OwnedTestDirectory::new("vertex81-refresh");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let file = temp.path().join("adc.json");
    std::fs::write(&file,json!({"type":"authorized_user","client_id":"owned-client","client_secret":"PRIVATE_CLIENT_SECRET","refresh_token":"PRIVATE_REFRESH_TOKEN","quota_project_id":"owned-quota","token_uri":format!("http://{}/token",listener.local_addr().unwrap())}).to_string()).unwrap();
    let _env = crate::tests::ScopedProcessEnv::set(&[
        (
            "GOOGLE_APPLICATION_CREDENTIALS",
            Some(file.into_os_string()),
        ),
        ("NO_PROXY", Some("*".into())),
        ("no_proxy", Some("*".into())),
    ]);
    let credentials = Builder::default().build().unwrap();
    for (step, token) in ["FIRST_TOKEN", "REFRESHED_TOKEN"].into_iter().enumerate() {
        if step == 1 {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::time::resume();
        }
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        read_fetch(&mut socket).await;
        let body = json!({"access_token":token,"token_type":"Bearer","expires_in":300}).to_string();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        let headers = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let CacheableResource::New { data, .. } =
                    credentials.headers(http::Extensions::new()).await.unwrap()
                else {
                    panic!("headers required")
                };
                if data["authorization"] == format!("Bearer {token}") {
                    break data;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(headers["x-goog-user-project"], "owned-quota");
    }
    drop(credentials);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}
