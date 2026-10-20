use super::tests::{profile, store};
use super::*;
use crate::tests::support::loopback_http::asynchronous::{
    accept_request, read_request_or_empty, respond,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn fixture() -> (tokio::net::TcpListener, Profile) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut profile = profile();
    profile.endpoints = Some(TestEndpoints {
        origin: format!("http://{}", listener.local_addr().unwrap()),
    });
    (listener, profile)
}
async fn request(listener: &tokio::net::TcpListener) -> (tokio::net::TcpStream, String, Value) {
    tokio::time::timeout(Duration::from_secs(4), async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request_or_empty(&mut socket).await;
        let len = request.body.len();
        let (headers, bytes) = (request.headers, request.body);
        let body = if len == 0 {
            json!({})
        } else if headers.contains("application/x-www-form-urlencoded") {
            let text = String::from_utf8(bytes).unwrap();
            Value::Object(
                reqwest::Url::parse(&format!("https://form.invalid/?{text}"))
                    .unwrap()
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                    .collect(),
            )
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (socket, headers, body)
    })
    .await
    .unwrap()
}
async fn answer(mut socket: tokio::net::TcpStream, status: u16, value: Value) {
    let body = value.to_string();
    respond(
        &mut socket,
        &format!("{status} Owned"),
        "application/json",
        &body,
    )
    .await;
    socket.shutdown().await.unwrap();
}
fn grant() -> Value {
    json!({"access_token":"NEW_GITHUB","refresh_token":"NEW_REFRESH","token_type":"bearer","expires_in":3600,"refresh_token_expires_in":7200})
}
fn model() -> Value {
    json!({"token":"OWNED_MODEL_TOKEN","expires_at":now().unwrap()+3600,"endpoints":{"api":"https://api.githubcopilot.com"}})
}
fn registration(directory: PathBuf, profile: Profile, expired: bool) -> Registration {
    let mut data = store(profile.clone());
    if expired {
        let tokens = data.tokens.as_mut().unwrap();
        tokens.expires = Some(now().unwrap() - 1);
        tokens.refresh = Some("OLD_REFRESH".into());
    }
    let generation = data.generation.clone();
    let mut locked = LockedStore::open(directory.clone(), true).unwrap().unwrap();
    locked.data = Some(data);
    locked.save().unwrap();
    drop(locked);
    Registration {
        directory,
        generation,
        profile,
    }
}

#[tokio::test]
async fn copilot81_device_flow_obeys_initial_interval_pending_and_slow_down_before_persisting() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-device");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let mut tasks = tokio::task::JoinSet::new();
    let p = profile.clone();
    let dir = directory.clone();
    tasks.spawn(async move { sign_in_profile(p, dir).await });
    let (socket, headers, body) = request(&listener).await;
    assert!(headers.starts_with("post /login/device/code "));
    assert_eq!(body["client_id"], "owned-mini-agent-app");
    assert_eq!(body["scope"], "read:user");
    let issued = std::time::Instant::now();
    answer(socket,200,json!({"device_code":"PRIVATE_DEVICE","user_code":"OWNED-CODE","verification_uri":profile.url("/login/device",false),"interval":1,"expires_in":30})).await;
    let (socket, headers, body) = request(&listener).await;
    assert!(issued.elapsed() >= Duration::from_millis(950));
    assert!(headers.starts_with("post /login/oauth/access_token "));
    assert_eq!(body["device_code"], "PRIVATE_DEVICE");
    assert_eq!(
        body["grant_type"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    answer(socket, 200, json!({"error":"authorization_pending"})).await;
    let (socket, _, _) = request(&listener).await;
    let slowed = std::time::Instant::now();
    answer(socket, 200, json!({"error":"slow_down"})).await;
    // This delay is the protocol's minimum; the test watches real request IO.
    let (socket, _, _) = tokio::time::timeout(Duration::from_secs(9), async {
        let (socket, request) = accept_request(&listener).await;
        let (headers, bytes) = (request.headers, request.body);
        (socket, headers, bytes)
    })
    .await
    .unwrap();
    assert!(slowed.elapsed() >= Duration::from_millis(5950));
    answer(socket, 200, grant()).await;
    let (socket, headers, _) = request(&listener).await;
    assert!(headers.starts_with("get /user "));
    assert!(headers.contains("authorization: bearer new_github"));
    answer(socket, 200, json!({"id":7,"login":"owned-user"})).await;
    let (socket, headers, _) = request(&listener).await;
    assert!(headers.starts_with("get /copilot_internal/v2/token "));
    assert!(headers.contains("authorization: token new_github"));
    assert!(!headers.contains("vscode"));
    answer(socket, 200, model()).await;
    tasks.join_next().await.unwrap().unwrap().unwrap();
    assert!(tasks.is_empty());
    let generation = {
        let locked = LockedStore::open(directory.clone(), false)
            .unwrap()
            .unwrap();
        let data = locked.data.as_ref().unwrap();
        assert_eq!(data.account, 7);
        assert!(data.tokens.as_ref().unwrap().verified);
        data.generation.clone()
    };
    let context = Registration {
        directory,
        profile,
        generation,
    }
    .auth_context()
    .await
    .unwrap();
    assert_eq!(context.api_key, "OWNED_MODEL_TOKEN");
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn copilot81_device_deadline_expires_without_an_early_poll_or_saved_grant() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-expiry");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let mut tasks = tokio::task::JoinSet::new();
    let dir = directory.clone();
    let p = profile.clone();
    tasks.spawn(async move { sign_in_profile(p, dir).await });
    let (socket, _, _) = request(&listener).await;
    answer(socket,200,json!({"device_code":"PRIVATE_DEVICE","user_code":"OWNED-CODE","verification_uri":profile.url("/login/device",false),"interval":5,"expires_in":1})).await;
    let error = tokio::time::timeout(Duration::from_secs(3), tasks.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(!error.to_string().contains("PRIVATE_DEVICE"));
    assert!(
        LockedStore::open(directory, false)
            .unwrap()
            .unwrap()
            .data
            .unwrap()
            .tokens
            .is_none()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn copilot81_cancelling_a_held_device_poll_retires_http_and_leaves_no_credential() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-cancel");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let mut tasks = tokio::task::JoinSet::new();
    let dir = directory.clone();
    let p = profile.clone();
    tasks.spawn(async move { sign_in_profile(p, dir).await });
    let (socket, _, _) = request(&listener).await;
    answer(socket,200,json!({"device_code":"PRIVATE_DEVICE","user_code":"OWNED-CODE","verification_uri":profile.url("/login/device",false),"interval":1,"expires_in":900})).await;
    let (mut held, headers, _) = request(&listener).await;
    assert!(headers.starts_with("post /login/oauth/access_token "));
    tasks.abort_all();
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap_err().is_cancelled());
    }
    let mut byte = [0];
    let closed = tokio::time::timeout(Duration::from_secs(2), held.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(closed, Ok(0) | Err(_)));
    assert!(
        LockedStore::open(directory, false)
            .unwrap()
            .unwrap()
            .data
            .unwrap()
            .tokens
            .is_none()
    );
}

#[tokio::test]
async fn copilot81_rotated_bootstrap_survives_user_request_failure_and_is_verified_before_model_use()
 {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-rotation");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let registration = registration(directory.clone(), profile, true);
    let mut tasks = tokio::task::JoinSet::new();
    let r = registration.clone();
    tasks.spawn(async move { r.auth_context().await });
    let (socket, headers, body) = request(&listener).await;
    assert!(headers.starts_with("post /login/oauth/access_token "));
    assert_eq!(body["refresh_token"], "OLD_REFRESH");
    answer(socket, 200, grant()).await;
    let (socket, headers, _) = request(&listener).await;
    assert!(headers.starts_with("get /user "));
    answer(socket, 503, json!({"error":"PRIVATE_PROVIDER_TEXT"})).await;
    let error = tasks.join_next().await.unwrap().unwrap().unwrap_err();
    assert!(!error.to_string().contains("PRIVATE_PROVIDER_TEXT"));
    {
        let locked = LockedStore::open(directory.clone(), false)
            .unwrap()
            .unwrap();
        let data = locked.data.as_ref().unwrap();
        let token = data.tokens.as_ref().unwrap();
        assert_eq!(token.access, "NEW_GITHUB");
        assert_eq!(token.refresh.as_deref(), Some("NEW_REFRESH"));
        assert!(!token.verified);
        assert!(data.model.is_none());
    }
    let r = registration.clone();
    tasks.spawn(async move { r.auth_context().await });
    let (socket, headers, _) = request(&listener).await;
    assert!(
        headers.starts_with("get /user "),
        "rotation was lost and an already-consumed refresh token was retried"
    );
    assert!(headers.contains("authorization: bearer new_github"));
    answer(socket, 200, json!({"id":7,"login":"owned-user"})).await;
    let (socket, headers, _) = request(&listener).await;
    assert!(headers.starts_with("get /copilot_internal/v2/token "));
    answer(socket, 200, model()).await;
    assert_eq!(
        tasks.join_next().await.unwrap().unwrap().unwrap().api_key,
        "OWNED_MODEL_TOKEN"
    );
    let locked = LockedStore::open(directory, false).unwrap().unwrap();
    assert!(
        locked
            .data
            .as_ref()
            .unwrap()
            .tokens
            .as_ref()
            .unwrap()
            .verified
    );
}

#[tokio::test]
async fn copilot81_refreshed_identity_mismatch_and_registration_replacement_fail_closed() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-identity");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let registration = registration(directory.clone(), profile, true);
    let mut tasks = tokio::task::JoinSet::new();
    let r = registration.clone();
    tasks.spawn(async move { r.auth_context().await });
    let (socket, _, _) = request(&listener).await;
    answer(socket, 200, grant()).await;
    let (socket, _, _) = request(&listener).await;
    answer(socket, 200, json!({"id":8,"login":"other-user"})).await;
    let error = tasks.join_next().await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("account changed"));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    {
        let mut locked = LockedStore::open(directory.clone(), false)
            .unwrap()
            .unwrap();
        let data = locked.data.as_mut().unwrap();
        assert_eq!(data.account, 7);
        assert!(data.model.is_none());
        assert!(!data.tokens.as_ref().unwrap().verified);
        data.generation = uuid::Uuid::new_v4().to_string();
        data.tokens = None;
        locked.save().unwrap();
    }
    assert!(
        registration
            .auth_context()
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("registration changed")
    );
}

#[tokio::test]
async fn copilot81_entire_credential_acquisition_has_a_real_30_second_deadline() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-deadline");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let registration = registration(directory.clone(), profile, true);
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move { registration.auth_context().await });
    let (mut held, _, _) = request(&listener).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let error = tokio::time::timeout(Duration::from_secs(2), tasks.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    let mut byte = [0];
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), held.read(&mut byte))
            .await
            .unwrap(),
        Ok(0) | Err(_)
    ));
    let locked = LockedStore::open(directory, false).unwrap().unwrap();
    assert_eq!(
        locked
            .data
            .as_ref()
            .unwrap()
            .tokens
            .as_ref()
            .unwrap()
            .access,
        "PRIVATE_GITHUB_ACCESS"
    );
}

#[tokio::test]
async fn copilot81_pending_login_cannot_restore_credentials_after_local_logout() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot81-login-generation");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let dir = directory.clone();
    let p = profile.clone();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move { sign_in_profile(p, dir).await });
    let (socket, _, _) = request(&listener).await;
    answer(socket,200,json!({"device_code":"PRIVATE_DEVICE","user_code":"OWNED-CODE","verification_uri":profile.url("/login/device",false),"interval":1,"expires_in":30})).await;
    let (socket, _, _) = request(&listener).await;
    // This is the exact local logout mutation while the grant is in flight.
    {
        let mut locked = LockedStore::open(directory.clone(), false)
            .unwrap()
            .unwrap();
        let data = locked.data.as_mut().unwrap();
        data.tokens = None;
        data.model = None;
        data.generation = uuid::Uuid::new_v4().to_string();
        locked.save().unwrap();
    }
    answer(socket, 200, grant()).await;
    let (socket, _, _) = request(&listener).await;
    answer(socket, 200, json!({"id":7,"login":"owned-user"})).await;
    let (socket, _, _) = request(&listener).await;
    answer(socket, 200, model()).await;
    let error = tasks.join_next().await.unwrap().unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("registration changed during sign-in")
    );
    let locked = LockedStore::open(directory, false).unwrap().unwrap();
    assert!(locked.data.as_ref().unwrap().tokens.is_none());
    assert!(locked.data.as_ref().unwrap().model.is_none());
}

#[tokio::test]
async fn copilot81_explicit_bootstrap_refreshes_only_an_expired_model_token_and_rejects_foreign_origins()
 {
    let (listener, profile) = fixture().await;
    let mut cache = ExternalTokenCache::default();
    let server = async {
        for token in ["FIRST_MODEL", "SECOND_MODEL"] {
            let (socket, headers, _) = request(&listener).await;
            assert!(headers.starts_with("get /copilot_internal/v2/token "));
            assert!(headers.contains("authorization: token private_explicit_github"));
            let mut value = model();
            value["token"] = json!(token);
            answer(socket, 200, value).await;
        }
        let (socket, _, _) = request(&listener).await;
        let mut value = model();
        value["endpoints"]["api"] = json!("https://attacker.example");
        answer(socket, 200, value).await;
    };
    let task = async {
        assert_eq!(
            external_context(&profile, "PRIVATE_EXPLICIT_GITHUB", &mut cache)
                .await
                .unwrap()
                .api_key,
            "FIRST_MODEL"
        );
        assert_eq!(
            external_context(&profile, "PRIVATE_EXPLICIT_GITHUB", &mut cache)
                .await
                .unwrap()
                .api_key,
            "FIRST_MODEL"
        );
        cache.0.as_mut().unwrap().expires = now().unwrap();
        assert_eq!(
            external_context(&profile, "PRIVATE_EXPLICIT_GITHUB", &mut cache)
                .await
                .unwrap()
                .api_key,
            "SECOND_MODEL"
        );
        cache.0.as_mut().unwrap().expires = now().unwrap();
        let error = external_context(&profile, "PRIVATE_EXPLICIT_GITHUB", &mut cache)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("approved API destination"));
        assert!(!error.to_string().contains("PRIVATE_EXPLICIT_GITHUB"));
        assert_eq!(cache.0.as_ref().unwrap().token, "SECOND_MODEL");
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, task);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn copilot_valid_model_token_is_served_without_reopening_the_locked_store() {
    let temp = crate::tests::OwnedTestDirectory::new("copilot-context-cache");
    let directory = temp.path().join("credentials");
    let (listener, profile) = fixture().await;
    let registration = registration(directory.clone(), profile, false);
    STORE_OPENS.with(|opens| opens.set(0));
    let mut tasks = tokio::task::JoinSet::new();
    let r = registration.clone();
    tasks.spawn(async move { r.auth_context().await });
    let (socket, headers, _) = request(&listener).await;
    assert!(headers.starts_with("get /copilot_internal/v2/token "));
    answer(socket, 200, model()).await;
    assert_eq!(
        tasks.join_next().await.unwrap().unwrap().unwrap().api_key,
        "OWNED_MODEL_TOKEN"
    );
    assert_eq!(STORE_OPENS.with(std::cell::Cell::get), 1);
    // Later requests reuse the token: no lock, no parse, no network.
    for _ in 0..5 {
        let context = registration.auth_context().await.unwrap();
        assert_eq!(context.api_key, "OWNED_MODEL_TOKEN");
    }
    assert_eq!(STORE_OPENS.with(std::cell::Cell::get), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    // Logging out removes the record: the cached token is not served.
    std::fs::remove_file(directory.join("session.json")).unwrap();
    assert!(registration.auth_context().await.is_err());
    assert_eq!(STORE_OPENS.with(std::cell::Cell::get), 2);
}
