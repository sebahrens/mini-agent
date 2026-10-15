use super::*;

#[test]
fn oauth47_plan_body_is_stateless_and_preserves_tools_schema_data() {
    let source = serde_json::json!({"model":"gpt-test", "max_output_tokens":50,"temperature":0.2,"store":true,"stream":false,"instructions":"STATIC", "input":[{"type":"message","role":"system","content":"history"},{"role":"user","content":"literal"}],"tools":[{"type":"function","name":"Write","parameters":{"type":"object","properties":{"cache_control":{"type":"string"}},"additionalProperties":false}}]});
    let prepared: serde_json::Value = serde_json::from_slice(
        &crate::auth::openai_plan::body(&serde_json::to_vec(&source).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(prepared["store"], false);
    assert_eq!(prepared["stream"], true);
    assert!(prepared.get("max_output_tokens").is_none() && prepared.get("temperature").is_none());
    assert_eq!(prepared["instructions"], source["instructions"]);
    assert_eq!(prepared["input"][0]["tools"], source["tools"]);
    assert_eq!(prepared["input"][1]["role"], "developer");
    assert_eq!(prepared["input"][2], source["input"][1]);
    assert_eq!(
        prepared["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    for patch in [
        serde_json::json!({"previous_response_id":"resp_private"}),
        serde_json::json!({"tools":[{"type":"tool_search"}]}),
        serde_json::json!({"input":[{"type":"reasoning","id":"r_without_encrypted"}]}),
    ] {
        let mut invalid = source.clone();
        invalid
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(crate::auth::openai_plan::body(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
    for uri in [
        "http://api.openai.com/v1/responses",
        "https://api.openai.com.evil/v1/responses",
        "https://api.openai.com/v1/chat/completions",
        "https://api.openai.com/v1/responses?key=x",
    ] {
        assert!(
            crate::auth::openai_plan::endpoint(&http::Method::POST, &uri.parse().unwrap()).is_err()
        );
    }
    crate::auth::openai_plan::endpoint(
        &http::Method::POST,
        &"https://api.openai.com/v1/responses".parse().unwrap(),
    )
    .unwrap();
}

#[test]
fn oauth47_auxiliary_completion_requires_actual_completed_envelope() {
    let response = serde_json::json!({"id":"resp1","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":2}});
    let bytes = format!(
        "data: {{\"type\":\"response.created\"}}\n\ndata: {}\n\n",
        serde_json::json!({"type":"response.completed","response":response})
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &crate::auth::openai_plan::completed_sse(bytes.as_bytes()).unwrap()
        )
        .unwrap(),
        response
    );
    for invalid in [
        b"data: {\"type\":\"response.failed\"}\n\n".as_slice(),
        b"data: {\"type\":\"response.created\"}\n\n",
        b"data: private-invalid-json\n\n",
    ] {
        let error = crate::auth::openai_plan::completed_sse(invalid)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private-invalid-json"));
    }
}

#[test]
fn oauth47_private_store_rejects_malformed_and_retains_host_on_signout() {
    let root = crate::tests::OwnedTestDirectory::new("oauth47-store");
    let private = root.path().join("credentials");
    let mut store = LockedStore::open(private.clone(), true).unwrap().unwrap();
    let host = store.data.host.clone();
    store.save().unwrap();
    assert!(
        LockedStore::open(private.clone(), false).is_err(),
        "same-file writer exclusion"
    );
    drop(store);
    let reopened = LockedStore::open(private.clone(), false).unwrap().unwrap();
    assert_eq!(reopened.data.host, host);
    drop(reopened);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = private.join("openai.json");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(LockedStore::open(private.clone(), false).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn oauth47_callback_is_exact_state_and_registration_bound() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let login = Login {
        listener,
        root: PathBuf::new(),
        host: "host".into(),
        revision: 0,
        selected: None,
        state: "expected".into(),
        nonce: "nonce".into(),
        verifier: "verifier".into(),
        redirect: "redirect".into(),
        url: "url".into(),
        endpoints: None,
    };
    assert_eq!(
        login
            .callback(
                "GET /auth/callback?state=expected&code=code&client_id=oaiapp_mine HTTP/1.1\r\n\r\n"
            )
            .unwrap(),
        Some(("code".into(), "oaiapp_mine".into()))
    );
    assert!(
        login
            .callback("GET /auth/callback?state=expected&error=access_denied HTTP/1.1\r\n\r\n")
            .unwrap()
            .is_none()
    );
    for query in [
        "state=wrong&code=c&client_id=oaiapp_x",
        "state=expected&state=expected&code=c&client_id=oaiapp_x",
        "state=expected&code=c&client_id=dynamic_agent_client",
        "state=expected&code=c",
    ] {
        assert!(
            login
                .callback(&format!("GET /auth/callback?{query} HTTP/1.1\r\n\r\n"))
                .is_err()
        );
    }
    assert!(
        login
            .callback("GET /other?state=expected&code=c&client_id=oaiapp_x HTTP/1.1\r\n\r\n")
            .is_err()
    );
}

// Generated only for synthetic local fixtures; authenticates no external account.
fn signed_id(client: &str, nonce: &str, subject: &str, exp: u64) -> String {
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
    let pair = RsaKeyPair::from_pkcs8(include_bytes!("fixtures/test-only-rsa.pk8")).unwrap();
    let header = B64.encode(br#"{"alg":"RS256","kid":"test-only","typ":"JWT"}"#);
    let claims=B64.encode(serde_json::to_vec(&serde_json::json!({"iss":ISSUER,"aud":client,"sub":subject,"exp":exp,"nonce":nonce,"email":"fixture@example.invalid"})).unwrap());
    let message = format!("{header}.{claims}");
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{message}.{}", B64.encode(signature))
}

#[test]
fn oauth47_identity_signature_nonce_client_expiry_and_plan_scopes_are_enforced() {
    let keys: Jwks = serde_json::from_str(include_str!("fixtures/test-only-jwks.json")).unwrap();
    let token = signed_id("oaiapp_own", "nonce", "subject", 1000);
    assert_eq!(
        verify_id(&token, &keys, "oaiapp_own", Some("nonce"), 900)
            .unwrap()
            .sub,
        "subject"
    );
    for (client, nonce, at) in [
        ("foreign", "nonce", 900),
        ("oaiapp_own", "wrong", 900),
        ("oaiapp_own", "nonce", 1000),
    ] {
        assert!(verify_id(&token, &keys, client, Some(nonce), at).is_err());
    }
    let mut forged = token.into_bytes();
    let last = forged.len() - 10;
    forged[last] = if forged[last] == b'A' { b'B' } else { b'A' };
    assert!(
        verify_id(
            std::str::from_utf8(&forged).unwrap(),
            &keys,
            "oaiapp_own",
            Some("nonce"),
            900
        )
        .is_err()
    );
    let response = |scope: &str| TokenResponse {
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        id_token: signed_id("oaiapp_own", "nonce", "subject", 1000),
        token_type: "Bearer".into(),
        expires_in: 60,
        scope: scope.into(),
    };
    assert!(
        accept_tokens(
            response(SCOPES),
            &keys,
            "oaiapp_own",
            Some("nonce"),
            Some("foreign-account"),
            900
        )
        .is_err()
    );
    assert!(
        accept_tokens(
            response("openid profile email offline_access"),
            &keys,
            "oaiapp_own",
            Some("nonce"),
            None,
            900
        )
        .is_err()
    );
}

#[tokio::test]
async fn oauth47_actual_loopback_exchange_refresh_and_logout_keep_registration_private() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let roots = crate::tests::OwnedTestDirectory::new("oauth47-roundtrip");
    let root = roots.path().join("credentials");
    let mut store = LockedStore::open(root.clone(), true).unwrap().unwrap();
    store.save().unwrap();
    let host = store.data.host.clone();
    let revision = store.data.revision;
    drop(store);
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", server.local_addr().unwrap());
    let server_task = async move {
        let mut requests = Vec::new();
        for index in 0..5 {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), server.accept())
                .await
                .unwrap()
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 8192);
                header.push(socket.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(length < 8192);
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            requests.push((header.clone(), String::from_utf8(body).unwrap()));
            let body=match index {
                0|2=>serde_json::to_vec(&serde_json::json!({"access_token":format!("access-{index}"),"refresh_token":format!("refresh-{index}"),"id_token":signed_id("oaiapp_own","nonce","subject",now().unwrap()+3600),"token_type":"Bearer","expires_in":if index==0{1}else{3600},"scope":SCOPES})).unwrap(),
                1|3=>include_bytes!("fixtures/test-only-jwks.json").to_vec(),
                4=>Vec::new(),_=>unreachable!(),
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
        requests
    };
    let operation = async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let callback = listener.local_addr().unwrap();
        let endpoints = AuthEndpoints {
            token: format!("{origin}/token"),
            jwks: format!("{origin}/jwks"),
        };
        let login = Login {
            listener,
            root: root.clone(),
            host: host.clone(),
            revision,
            selected: None,
            state: "state".into(),
            nonce: "nonce".into(),
            verifier: "verifier".into(),
            redirect: format!("http://{callback}/auth/callback"),
            url: "unused".into(),
            endpoints: Some(endpoints.clone()),
        };
        let callback_task = async move {
            let mut socket = tokio::net::TcpStream::connect(callback).await.unwrap();
            socket.write_all(format!("GET /auth/callback?state=state&code=fixture_code&client_id=oaiapp_own HTTP/1.1\r\nHost: {callback}\r\n\r\n").as_bytes()).await.unwrap();
            let mut body = Vec::new();
            socket.read_to_end(&mut body).await.unwrap();
            assert!(body.starts_with(b"HTTP/1.1 200"));
        };
        let (result, ()) = tokio::join!(login.complete(), callback_task);
        result.unwrap();
        assert!(
            tokio::net::TcpStream::connect(callback).await.is_err(),
            "completed callback listener is closed"
        );
        let store = LockedStore::open(root.clone(), false).unwrap().unwrap();
        assert_eq!(store.data.host, host);
        assert_eq!(store.data.active.as_deref(), Some("oaiapp_own"));
        assert_eq!(store.data.accounts[0].subject, "subject");
        drop(store);
        let registration = Registration {
            root: root.clone(),
            client_id: "oaiapp_own".into(),
            endpoints: Some(endpoints),
        };
        assert_eq!(registration.access_token().await.unwrap(), "access-2");
        let signout_root = root.clone();
        let revoke = format!("{origin}/revoke");
        let message =
            crate::agent::runner::spawn_blocking_scoped(move || logout_at(signout_root, &revoke))
                .await
                .unwrap()
                .unwrap();
        assert!(message.contains("revoked"));
        assert!(registration.access_token().await.is_err());
        let store = LockedStore::open(root, false).unwrap().unwrap();
        assert_eq!(store.data.host, host);
        assert!(store.data.active.is_none());
        assert!(store.data.accounts[0].tokens.is_none());
        assert_eq!(store.data.accounts[0].client_id, "oaiapp_own");
        drop(store);
    };
    let (requests, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server_task, operation)
    })
    .await
    .unwrap();
    assert!(
        requests[0].1.contains("grant_type=authorization_code")
            && requests[0].1.contains("client_id=oaiapp_own")
            && requests[0].1.contains("code_verifier=verifier")
    );
    assert!(
        requests[2].1.contains("grant_type=refresh_token")
            && requests[2].1.contains("refresh_token=refresh-0")
    );
    assert!(
        requests[4].1.contains("token=refresh-2")
            && requests[4].1.contains("token_type_hint=refresh_token")
    );
}

#[derive(Clone)]
struct EchoTool(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl rig::tool::Tool for EchoTool {
    const NAME: &'static str = "Echo";
    type Args = serde_json::Value;
    type Output = String;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Owned local echo".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false})
    }
    async fn call(&self, args: Self::Args) -> Result<String, Self::Error> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(args["text"].as_str().unwrap().into())
    }
}

#[tokio::test]
async fn oauth47_actual_sdk_plan_tool_followup_and_auxiliary_use_one_selected_registration() {
    use futures::StreamExt;
    use rig::{client::CompletionClient, streaming::StreamingPrompt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let roots = crate::tests::OwnedTestDirectory::new("oauth47-sdk");
    let root = roots.path().join("credentials");
    let mut store = LockedStore::open(root.clone(), true).unwrap().unwrap();
    store.data.accounts.push(Account {
        client_id: "oaiapp_fixture".into(),
        subject: "subject".into(),
        email: None,
        tokens: Some(Tokens {
            access: "owned-access".into(),
            refresh: "unused-refresh".into(),
            id: "unused-id".into(),
            scopes: SCOPES.into(),
            expires: now().unwrap() + 3600,
        }),
    });
    store.data.active = Some("oaiapp_fixture".into());
    store.save().unwrap();
    drop(store);
    let registration = Registration {
        root,
        client_id: "oaiapp_fixture".into(),
        endpoints: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let backend = crate::provider_http::ProviderHttpClient::chatgpt_test_wire(
        registration,
        origin.parse().unwrap(),
    );
    let client = rig::providers::openai::Client::builder()
        .api_key("")
        .http_client(backend.clone())
        .build()
        .unwrap();
    let effects = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let agent = client
        .agent("gpt-test")
        .preamble("STATIC")
        .max_tokens(100)
        .temperature(0.2)
        .tool(EchoTool(effects.clone()))
        .build();
    let server = async move {
        let mut bodies = Vec::new();
        for index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 8192);
                header.push(socket.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer owned-access\r\n")
            );
            let length = header
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(length < 128 * 1024);
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            if index == 3 {
                let private = "PRIVATE_PROVIDER_DIAGNOSTIC owned-access";
                socket.write_all(format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{private}", private.len()).as_bytes()).await.unwrap();
                continue;
            }
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["store"], false);
            assert_eq!(body["stream"], true);
            assert!(body.get("max_output_tokens").is_none());
            assert!(body.get("temperature").is_none());
            assert_eq!(body["instructions"], "STATIC");
            assert_eq!(body["input"][0]["type"], "additional_tools");
            assert_eq!(body["input"][0]["tools"][0]["name"], "Echo");
            if index == 1 {
                assert!(
                    body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|i| i["type"] == "function_call_output" && i["call_id"] == "call_one")
                );
            }
            bodies.push(body);
            let response_item = if index == 0 {
                serde_json::json!({"type":"function_call","id":"fc_one","call_id":"call_one","name":"Echo","arguments":"{\"text\":\"owned-result\"}","status":"completed"})
            } else {
                serde_json::json!({"type":"message","id":format!("msg{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"DONE","annotations":[]}]})
            };
            let response = serde_json::json!({"id":format!("resp{index}"),"object":"response","created_at":1,"status":"completed","model":"gpt-test","output":[response_item.clone()],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}});
            let mut events = Vec::new();
            if index != 0 {
                events.push(serde_json::json!({"type":"response.output_text.delta","item_id":format!("msg{index}"),"output_index":0,"content_index":0,"sequence_number":0,"delta":"DONE"}));
            }
            events.push(serde_json::json!({"type":"response.output_item.done","output_index":0,"sequence_number":1,"item":response_item}));
            events.push(serde_json::json!({"type":"response.completed","sequence_number":2,"response":response}));
            let bytes = events
                .into_iter()
                .map(|v| format!("data: {v}\n\n"))
                .collect::<String>();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",bytes.len(),bytes).as_bytes()).await.unwrap();
        }
        bodies
    };
    let operation = async {
        let mut stream = agent.stream_prompt("literal").max_turns(2).await;
        let mut final_seen = false;
        while let Some(item) = stream.next().await {
            if let rig::agent::MultiTurnStreamItem::FinalResponse(result) = item.unwrap() {
                assert!(result.output.contains("DONE"));
                final_seen = true;
            }
        }
        assert!(final_seen);
        assert_eq!(effects.load(std::sync::atomic::Ordering::SeqCst), 1);
        use rig::completion::Prompt;
        assert_eq!(agent.prompt("auxiliary").await.unwrap(), "DONE");
        let error = backend
            .listing_get(
                http::Request::builder()
                    .uri("https://untrusted.invalid/v1/models")
                    .body(Vec::new())
                    .unwrap(),
                1024,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("official"));
        use rig::http_client::HttpClientExt;
        let error = backend
            .send_streaming(
                http::Request::builder()
                    .method(http::Method::POST)
                    .uri("https://api.openai.com/v1/responses")
                    .body(br#"{"model":"gpt-test","input":[]}"#.to_vec())
                    .unwrap(),
            )
            .await
            .err()
            .expect("actual HTTP rejection");
        assert!(!error.to_string().contains("PRIVATE_PROVIDER_DIAGNOSTIC"));
        assert!(!error.to_string().contains("owned-access"));
        let rig::http_client::Error::Instance(source) = error else {
            panic!("retry hint carrier")
        };
        let hint = source
            .downcast_ref::<crate::provider_http::ProviderRetryAfterError>()
            .unwrap();
        assert_eq!(hint.status, http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(hint.retry_after, "2");
    };
    let (bodies, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, operation)
    })
    .await
    .unwrap();
    assert_eq!(bodies.len(), 3);
    assert_eq!(
        bodies[0]["input"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap()[0]["text"],
        "literal"
    );
}

#[test]
fn oauth47_actual_client_login_precedes_keys_but_never_crosses_custom_routes() {
    let roots = crate::tests::OwnedTestDirectory::new("oauth47-precedence");
    let mut vars = [
        "ZS_CONFIG_DIR",
        "ZS_DATA_DIR",
        "ZS_LOCAL_DATA_DIR",
        "ZS_STATE_DIR",
        "ZS_CACHE_DIR",
        "ZS_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(roots.path().join(name).into_os_string())))
    .to_vec();
    vars.extend([
        ("OPENAI_BASE_URL", None),
        ("OPENAI_API_KEY", Some("api-key-canary".into())),
    ]);
    let _environment = crate::tests::ScopedProcessEnv::set(&vars);
    let mut store = LockedStore::open(root().unwrap(), true).unwrap().unwrap();
    store.data.accounts.push(Account {
        client_id: "oaiapp_fixture".into(),
        subject: "subject".into(),
        email: None,
        tokens: Some(Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            id: "id".into(),
            scopes: SCOPES.into(),
            expires: now().unwrap() + 3600,
        }),
    });
    store.data.active = Some("oaiapp_fixture".into());
    store.save().unwrap();
    drop(store);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let login = runtime.block_on(Login::begin(Some(1))).unwrap();
    let url = reqwest::Url::parse(login.url()).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "oaiapp_fixture");
    assert_eq!(params["id_token_hint"], "id");
    assert!(!params.contains_key("agent_name_hint"));
    let displayed = reqwest::Url::parse(&login.display_url().unwrap()).unwrap();
    assert!(
        !displayed
            .query_pairs()
            .any(|(key, _)| key == "id_token_hint")
    );
    assert!(
        displayed
            .query_pairs()
            .any(|(key, value)| key == "state" && value == login.state)
    );
    let old_state = login.state.clone();
    let old_nonce = login.nonce.clone();
    let old_verifier = login.verifier.clone();
    let checkpoint = std::fs::read(root().unwrap().join("openai.json")).unwrap();
    let retry = runtime.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut login = login;
        let endpoint = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", endpoint.local_addr().unwrap());
        login.endpoints = Some(AuthEndpoints { token: origin.clone(), jwks: origin });
        let address = login.listener.local_addr().unwrap();
        let state = login.state.clone();
        let server = async {
            let (mut socket, _) = endpoint.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 8192);
                header.push(socket.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let len = header.lines().find_map(|line| line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map(|(_, n)| n.trim().parse::<usize>().unwrap())).unwrap();
            assert!(len < 8192);
            let mut body = vec![0; len];
            socket.read_exact(&mut body).await.unwrap();
            let form: std::collections::HashMap<_,_> = reqwest::Url::parse(&format!("http://local/?{}", String::from_utf8(body).unwrap())).unwrap().query_pairs().into_owned().collect();
            assert_eq!(form["client_id"], "oaiapp_fixture");
            assert_eq!(form["code"], "expired-code");
            let body = r#"{"error":"invalid_grant"}"#;
            socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        };
        let browser = async {
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(format!("GET /auth/callback?state={state}&code=expired-code HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes()).await.unwrap();
            let mut reply = Vec::new(); socket.read_to_end(&mut reply).await.unwrap();
            assert!(reply.starts_with(b"HTTP/1.1 200"));
        };
        let (_, (), result) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(server, browser, login.complete()) }).await.unwrap();
        result.unwrap_err().downcast::<CodeRetry>().unwrap()
    });
    assert_eq!(
        std::fs::read(root().unwrap().join("openai.json")).unwrap(),
        checkpoint
    );
    let retried = runtime
        .block_on(Login::begin_with_retry(None, Some(retry)))
        .unwrap();
    assert_eq!(
        retried.selected,
        Some(("oaiapp_fixture".into(), Some("subject".into())))
    );
    assert_ne!(retried.state, old_state);
    assert_ne!(retried.nonce, old_nonce);
    assert_ne!(retried.verifier, old_verifier);
    let retried_url = reqwest::Url::parse(retried.url()).unwrap();
    assert!(
        !retried_url
            .query_pairs()
            .any(|(key, _)| key == "agent_name_hint")
    );
    drop(retried);
    let config = [("openai".into(), "config-key-canary".into())].into();
    let selected =
        crate::provider::create_client("openai", None, &Default::default(), Some(&config)).unwrap();
    let crate::provider::AnyClient::OpenAI(crate::provider::OpenAiClient::Responses(client)) =
        selected
    else {
        panic!("plan route")
    };
    assert!(client.http_client().is_chatgpt());
    let explicit = crate::provider::create_client(
        "openai",
        Some("explicit-key"),
        &Default::default(),
        Some(&config),
    )
    .unwrap();
    let crate::provider::AnyClient::OpenAI(crate::provider::OpenAiClient::Responses(client)) =
        explicit
    else {
        panic!("API route")
    };
    assert!(!client.http_client().is_chatgpt());
    let custom = serde_json::from_value(
        serde_json::json!({"provider_type":"openai","base_url":"https://gateway.invalid/v1"}),
    )
    .unwrap();
    let providers = [("openai".into(), custom)].into();
    assert!(
        crate::provider::create_client("openai", None, &providers, Some(&config)).is_err(),
        "isolated endpoint cannot borrow a vendor key or saved subscription"
    );
    for value in [
        None,
        Some("https://api.openai.com/v1"),
        Some("https://api.openai.com:443/v1/"),
    ] {
        assert!(official_api_base(value));
    }
    for value in [
        "http://api.openai.com/v1",
        "https://api.openai.com.evil/v1",
        "https://user@api.openai.com/v1",
        "https://api.openai.com/v1?q=x",
    ] {
        assert!(!official_api_base(Some(value)));
    }
}

#[test]
fn oauth47_stale_signin_cannot_replace_a_newer_or_different_host_registration() {
    let roots = crate::tests::OwnedTestDirectory::new("oauth47-stale");
    let root = roots.path().join("credentials");
    let mut store = LockedStore::open(root.clone(), true).unwrap().unwrap();
    store.save().unwrap();
    let host = store.data.host.clone();
    let old = store.data.revision;
    store.save().unwrap();
    let bytes = std::fs::read(root.join("openai.json")).unwrap();
    let values = || {
        let keys = serde_json::from_str(include_str!("fixtures/test-only-jwks.json")).unwrap();
        accept_tokens(
            TokenResponse {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                id_token: signed_id("oaiapp_own", "nonce", "subject", 1000),
                token_type: "Bearer".into(),
                expires_in: 60,
                scope: SCOPES.into(),
            },
            &keys,
            "oaiapp_own",
            Some("nonce"),
            None,
            900,
        )
        .unwrap()
    };
    for expected in [(&host[..], old), ("foreign-host", store.data.revision)] {
        let (tokens, claims) = values();
        assert!(
            store
                .install_login(expected, "oaiapp_own".into(), tokens, claims)
                .is_err()
        );
        assert_eq!(std::fs::read(root.join("openai.json")).unwrap(), bytes);
        assert!(store.data.active.is_none());
    }
    let expected = store.data.revision;
    let (tokens, claims) = values();
    store
        .install_login((&host, expected), "oaiapp_own".into(), tokens, claims)
        .unwrap();
    let accepted = std::fs::read(root.join("openai.json")).unwrap();
    let (tokens, claims) = values();
    assert!(
        store
            .install_login((&host, expected), "oaiapp_own".into(), tokens, claims)
            .is_err()
    );
    assert_eq!(std::fs::read(root.join("openai.json")).unwrap(), accepted);
}

#[tokio::test]
async fn oauth47_revoked_refresh_preserves_a_reauthorization_fence_without_key_fallback() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let roots = crate::tests::OwnedTestDirectory::new("oauth47-revoked");
    let root = roots.path().join("credentials");
    let mut store = LockedStore::open(root.clone(), true).unwrap().unwrap();
    store.data.accounts.push(Account {
        client_id: "oaiapp_own".into(),
        subject: "subject".into(),
        email: None,
        tokens: Some(Tokens {
            access: "expired".into(),
            refresh: "private-refresh".into(),
            id: "id".into(),
            scopes: SCOPES.into(),
            expires: 0,
        }),
    });
    store.data.active = Some("oaiapp_own".into());
    store.save().unwrap();
    drop(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let registration = Registration {
        root: root.clone(),
        client_id: "oaiapp_own".into(),
        endpoints: Some(AuthEndpoints {
            token: endpoint.clone(),
            jwks: endpoint,
        }),
    };
    let server = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(socket.read_u8().await.unwrap());
            assert!(headers.len() < 8192);
        }
        let body = r#"{"error":"invalid_grant","error_description":"PRIVATE_PROVIDER_DIAGNOSTIC"}"#;
        socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    };
    let operation = async {
        let error = registration.access_token().await.unwrap_err().to_string();
        assert!(error.contains("sign-in") || error.contains("login"));
        assert!(
            !error.contains("PRIVATE_PROVIDER_DIAGNOSTIC") && !error.contains("private-refresh")
        );
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, operation)
    })
    .await
    .unwrap();
    let store = LockedStore::open(root, false).unwrap().unwrap();
    assert_eq!(store.data.active.as_deref(), Some("oaiapp_own"));
    assert!(store.data.accounts[0].tokens.is_none());
    store.data.validate().unwrap();
    drop(store);
    assert!(
        registration.access_token().await.is_err(),
        "a second request must require sign-in before any additional HTTP"
    );
}

#[test]
fn oauth47_cli_and_slash_commands_expose_login_without_an_api_key() {
    use clap::Parser;
    let cli = crate::cli::Cli::try_parse_from([
        "mini-agent",
        "--login",
        "openai",
        "--login-account",
        "new",
    ])
    .unwrap();
    assert_eq!(cli.login.as_deref(), Some("openai"));
    assert_eq!(cli.login_account.as_deref(), Some("new"));
    assert!(cli.api_key.is_none());
    assert!(
        crate::cli::Cli::try_parse_from(["mini-agent", "--login", "openai", "--logout", "openai"])
            .is_err()
    );
    assert!(crate::cli::Cli::try_parse_from(["mini-agent", "--login-account", "1"]).is_err());
    for command in ["/login", "/logout"] {
        assert!(crate::ui::slash::routes_to_providers(command));
        assert!(crate::ui::pickers::list::available_commands().contains(&command));
    }
}

fn labels47_store() -> Store {
    let mut store = Store::new();
    store.active = Some("owned-labels-app".into());
    store.accounts.push(Account {
        client_id: "owned-labels-app".into(),
        subject: "PRIVATE_LABEL_SUBJECT".into(),
        email: Some("PRIVATE_LABEL_EMAIL@example.invalid".into()),
        tokens: Some(Tokens {
            access: "PRIVATE_LABEL_ACCESS".into(),
            refresh: "PRIVATE_LABEL_REFRESH".into(),
            id: "PRIVATE_LABEL_ID".into(),
            scopes: SCOPES.into(),
            expires: 1,
        }),
    });
    store
}

#[test]
fn login_labels47_openai_reader_is_bounded_nonrepairing_and_preserves_the_revocation_fence() {
    use crate::auth::LoginRecordState as S;
    let root = crate::tests::OwnedTestDirectory::new("labels47-openai-reader");
    let credentials = root.path().join("explicit-credentials");
    assert_eq!(diagnostic_state_at(&credentials), S::Absent);
    assert!(!credentials.exists());
    let directory = credentials.join("provider-oauth");
    crate::fs::ensure_private_directory(&directory).unwrap();
    let path = directory.join("openai.json");
    let mut data = labels47_store();
    let bytes = serde_json::to_vec(&data).unwrap();
    crate::fs::private_atomic_write_sync(&path, &bytes).unwrap();
    assert_eq!(
        diagnostic_state_at(&credentials),
        S::Stored,
        "expired access token still has a refresh; this is presence, not a health probe"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!directory.join("session.lock").exists());
    data.accounts[0].tokens = None;
    crate::fs::private_atomic_write_sync(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
    assert_eq!(diagnostic_state_at(&credentials), S::SignInRequired);
    data.active = None;
    crate::fs::private_atomic_write_sync(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
    assert_eq!(diagnostic_state_at(&credentials), S::Absent);
    for invalid in [b"not json".to_vec(), vec![b'x'; MAX_BYTES + 1]] {
        crate::fs::private_atomic_write_sync(&path, &invalid).unwrap();
        assert_eq!(diagnostic_state_at(&credentials), S::Unavailable);
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
    }
    assert!(!directory.join("session.lock").exists());
}

#[cfg(unix)]
#[test]
fn login_labels47_private_inspection_rejects_links_and_preserves_permissive_modes() {
    use crate::auth::LoginRecordState as S;
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = crate::tests::OwnedTestDirectory::new("labels47-reader-modes");
    let credentials = root.path().join("credentials");
    let directory = credentials.join("provider-oauth");
    crate::fs::ensure_private_directory(&directory).unwrap();
    let path = directory.join("openai.json");
    let bytes = serde_json::to_vec(&labels47_store()).unwrap();
    crate::fs::private_atomic_write_sync(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(diagnostic_state_at(&credentials), S::Unavailable);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(diagnostic_state_at(&credentials), S::Unavailable);
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o755
    );
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = root.path().join("target.json");
    std::fs::rename(&path, &target).unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(diagnostic_state_at(&credentials), S::Unavailable);
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
    std::fs::remove_file(&path).unwrap();
    let outside = root.path().join("outside");
    std::fs::rename(&directory, &outside).unwrap();
    symlink(&outside, &directory).unwrap();
    assert_eq!(diagnostic_state_at(&credentials), S::Unavailable);
    assert!(!outside.join("session.lock").exists());
}

#[test]
fn login_labels47_doctor_and_setup_use_explicit_saved_account_state_and_actual_route_precedence() {
    use crate::auth::{CredentialSource, LoginRecordState as S};
    use crate::diagnostics::{CredentialReport as R, provider_credential_with_env as inspect};
    use std::env::VarError;
    let root = crate::tests::OwnedTestDirectory::new("labels47-operator");
    let credentials = root.path().join("captured");
    let directory = credentials.join("provider-oauth");
    crate::fs::ensure_private_directory(&directory).unwrap();
    let path = directory.join("openai.json");
    let mut data = labels47_store();
    crate::fs::private_atomic_write_sync(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
    let cfg: crate::config::Config =
        serde_json::from_value(serde_json::json!({"api_keys":{"openai":"PRIVATE_CONFIG_KEY"}}))
            .unwrap();
    let env = |name: &str| {
        if name == "OPENAI_API_KEY" {
            Ok("PRIVATE_ENV_KEY".into())
        } else {
            Err(VarError::NotPresent)
        }
    };
    let saved = inspect(&cfg, "openai", None, &credentials, env);
    assert_eq!(saved, R::ChatGpt(S::Stored));
    assert_eq!(saved.setup_label(), "auth: saved ChatGPT login");
    assert_eq!(
        inspect(&cfg, "openai", Some("PRIVATE_CLI"), &credentials, env),
        R::Source(CredentialSource::Cli)
    );
    assert_eq!(
        inspect(&cfg, "openai", None, &root.path().join("other"), env),
        R::Source(CredentialSource::Environment("OPENAI_API_KEY".into()))
    );
    assert_eq!(
        inspect(&cfg, "openai", None, &credentials, |name| {
            if name == "OPENAI_BASE_URL" {
                Ok("https://gateway.example/v1".into())
            } else {
                env(name)
            }
        }),
        R::Source(CredentialSource::Environment("OPENAI_API_KEY".into()))
    );
    let custom: crate::config::Config = serde_json::from_value(serde_json::json!({"custom_providers":{"openai":{"provider_type":"openai","base_url":"https://api.openai.com/v1","inherit_builtin_key":true}}})).unwrap();
    assert_eq!(
        inspect(&custom, "openai", None, &credentials, env),
        R::Source(CredentialSource::Environment("OPENAI_API_KEY".into()))
    );
    data.accounts[0].tokens = None;
    crate::fs::private_atomic_write_sync(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
    let fence = inspect(&cfg, "openai", None, &credentials, env);
    assert_eq!(fence, R::ChatGpt(S::SignInRequired));
    assert_eq!(fence.setup_label(), "auth: ChatGPT sign-in required");
    // Render the actual Doctor with the supplied root, with no global store
    // selection and no token/account details escaping into the report.
    let _env =
        crate::tests::ScopedProcessEnv::set(&[("OPENAI_BASE_URL", None), ("OPENAI_API_KEY", None)]);
    let paths = crate::paths::AppPaths {
        config_dir: root.path().join("config"),
        data_dir: root.path().join("data"),
        local_data_dir: root.path().join("local"),
        state_dir: root.path().join("state"),
        cache_dir: root.path().join("cache"),
        credentials_dir: credentials.clone(),
        project_dir: Some(root.path().to_owned()),
    };
    let cli = crate::cli::Cli {
        provider: Some("openai".into()),
        no_sandbox: true,
        ..Default::default()
    };
    let report =
        crate::diagnostics::DoctorReport::capture(&cli, &cfg, &paths, None, None, false).render();
    assert!(
        report.contains("saved ChatGPT registration; sign-in required"),
        "{report}"
    );
    assert!(!report.contains("PRIVATE_"));
    assert!(!directory.join("session.lock").exists());
}
