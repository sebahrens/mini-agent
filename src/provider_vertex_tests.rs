use super::*;
use crate::auth::{AuthResolver, CredentialSource, ProviderKind};
use futures::StreamExt;
use rig::completion::{CompletionModel, Message};
use rig::streaming::StreamingChat;
use rig::tool::Tool;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env::VarError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn env(name: &str) -> Result<String, VarError> {
    match name {
        "GOOGLE_CLOUD_PROJECT" => Ok("owned-project".into()),
        "GOOGLE_CLOUD_LOCATION" => Ok("us-east5".into()),
        _ => Err(VarError::NotPresent),
    }
}
#[test]
fn vertex81_config_adc_typed_token_and_custom_isolation() {
    assert_eq!(
        ProviderKind::from_name("vertex-ai"),
        Some(ProviderKind::Vertex)
    );
    assert!(crate::config::Config::default().is_anthropic_native("vertex"));
    let route = route(&env).unwrap();
    assert_eq!(
        endpoint(None, &route).unwrap(),
        "https://us-east5-aiplatform.googleapis.com"
    );
    assert_eq!(
        endpoint(None, &VertexRoute::new("owned-project", "global").unwrap()).unwrap(),
        "https://aiplatform.googleapis.com"
    );
    for id in ["", "..", "a/b", "a%2fb", "a?x", "a\\b", "a#b", "a b"] {
        assert!(VertexRoute::new(id, "us-east5").is_err());
        assert!(route.path(id, true).is_err());
    }
    for bad in [
        "https://user:secret@cloud.example",
        "https://cloud.example/v1",
        "https://cloud.example?secret",
        "file:///x",
    ] {
        assert!(endpoint(Some(bad), &route).is_err());
    }
    let resolver = AuthResolver::new(ProviderKind::Vertex);
    assert_eq!(
        resolver.resolve_credential_with_env(env).unwrap(),
        ResolvedCredential::GoogleAdc
    );
    assert_eq!(
        resolver.credential_source_with_env(env).unwrap(),
        CredentialSource::GoogleAdc
    );
    assert_eq!(
        resolver
            .clone()
            .with_cli_key(Some("owned"))
            .resolve_credential_with_env(env)
            .unwrap(),
        ResolvedCredential::Bearer("owned".into())
    );
    assert_eq!(
        resolver
            .clone()
            .with_config_keys(Some(&HashMap::from([(
                "vertex".into(),
                "config-owned".into()
            )])))
            .resolve_credential_with_env(env)
            .unwrap(),
        ResolvedCredential::Bearer("config-owned".into())
    );
    assert_eq!(
        resolver
            .resolve_credential_with_env(|key| if key == "VERTEX_ACCESS_TOKEN" {
                Ok("token-owned".into())
            } else {
                env(key)
            })
            .unwrap(),
        ResolvedCredential::Bearer("token-owned".into())
    );
    for resolver in [
        resolver
            .clone()
            .with_env_override(Some("OWN_GATEWAY_TOKEN")),
        resolver.clone().with_custom_provider_name(Some("gateway")),
        resolver
            .with_custom_provider_name(Some("vertex"))
            .with_credential_isolation(Some(true)),
    ] {
        assert!(
            resolver
                .resolve_credential_with_env(|key| {
                    if key == "ANTHROPIC_API_KEY" || key == "GEMINI_API_KEY" {
                        Ok("DO_NOT_BORROW".into())
                    } else {
                        env(key)
                    }
                })
                .is_err()
        );
    }
}

#[tokio::test]
async fn vertex81_auth_destination_binding_precedes_adc_and_preserves_body() {
    let route = route(&env).unwrap();
    let auth = VertexAuth::new(
        ResolvedCredential::GoogleAdc,
        "https://cloud.example",
        route.clone(),
    )
    .unwrap();
    for path in [
        "https://evil.example/v1/projects/owned-project/locations/us-east5/publishers/anthropic/models/claude-owned:streamRawPredict",
        "https://cloud.example/v1/projects/other/locations/us-east5/publishers/anthropic/models/claude-owned:streamRawPredict",
        "https://cloud.example/v1/projects/owned-project/locations/us-east5/publishers/google/models/claude-owned:streamRawPredict",
        "https://cloud.example/v1/projects/owned-project/locations/us-east5/publishers/anthropic/models/claude-owned:streamRawPredict?x=1",
    ] {
        let mut request = http::Request::post(path)
            .body(Bytes::from_static(b"exact bytes"))
            .unwrap();
        assert!(auth.prepare(&mut request).await.is_err());
        assert!(auth.adc.get().is_none());
        assert_eq!(request.body(), b"exact bytes".as_slice());
    }
    let auth = VertexAuth::new(
        ResolvedCredential::Bearer("OWNED_TOKEN".into()),
        "https://cloud.example",
        route.clone(),
    )
    .unwrap();
    for streaming in [false, true] {
        let mut request = http::Request::post(format!(
            "https://cloud.example{}",
            route.path("claude-owned@20260101", streaming).unwrap()
        ))
        .header("x-api-key", "WRONG")
        .header("x-goog-user-project", "WRONG")
        .body(Bytes::from_static(b"exact bytes"))
        .unwrap();
        auth.prepare(&mut request).await.unwrap();
        assert_eq!(request.body(), b"exact bytes".as_slice());
        assert_eq!(request.headers()["authorization"], "Bearer OWNED_TOKEN");
        assert!(request.headers()["authorization"].is_sensitive());
        assert!(!request.headers().contains_key("x-api-key"));
        assert!(!request.headers().contains_key("x-goog-user-project"));
    }
}
#[derive(Clone)]
struct Probe(Arc<std::sync::atomic::AtomicUsize>);
impl Tool for Probe {
    const NAME: &'static str = "vertex_owned_probe";
    type Args = Value;
    type Output = String;
    type Error = std::io::Error;
    fn description(&self) -> String {
        "Return an owned inert fixture".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn call(&self, _: Value) -> Result<String, std::io::Error> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("OWNED_VERTEX_TOOL_OK".into())
    }
}
async fn request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>, Value) {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 32768);
        header.push(socket.read_u8().await.unwrap());
    }
    let header = String::from_utf8(header).unwrap().to_ascii_lowercase();
    let length: usize = header
        .lines()
        .find_map(|l| {
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap();
    assert!(length < 1024 * 1024);
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap();
    (header, bytes, body)
}
fn response(step: usize) -> String {
    let event = |kind: &str, value: Value| format!("event: {kind}\ndata: {value}\n\n");
    let mut body = event(
        "message_start",
        json!({"type":"message_start","message":{"id":format!("vertex-{step}"),"type":"message","role":"assistant","model":"us.anthropic.owned","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
    );
    if step == 0 {
        body += &event(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_vertex","name":Probe::NAME,"input":{}}}),
        );
        body += &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        );
    } else {
        body += &event(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        );
        body += &event(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"VERTEX_DONE"}}),
        );
    }
    body += &event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    );
    body += &event(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":if step==0 {"tool_use"}else{"end_turn"},"stop_sequence":null},"usage":{"output_tokens":2}}),
    );
    body += &event("message_stop", json!({"type":"message_stop"}));
    body
}

#[tokio::test]
async fn vertex81_real_sdk_adc_and_declared_token_stream_tools_usage_and_exact_native_view() {
    for adc in [false, true] {
        let temp = crate::tests::OwnedTestDirectory::new("vertex81-wire");
        let tokens = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let file = temp.path().join("adc.json");
        std::fs::write(&file,json!({"type":"authorized_user","client_id":"owned-client","client_secret":"PRIVATE_CLIENT_SECRET","refresh_token":"PRIVATE_REFRESH_TOKEN","quota_project_id":"owned-quota","token_uri":format!("http://{}/token",tokens.local_addr().unwrap())}).to_string()).unwrap();
        let _env = crate::tests::ScopedProcessEnv::set(&[
            (
                "GOOGLE_APPLICATION_CREDENTIALS",
                Some(file.into_os_string()),
            ),
            ("NO_PROXY", Some("*".into())),
            ("no_proxy", Some("*".into())),
        ]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let custom=HashMap::from([("owned-vertex".into(),serde_json::from_value::<crate::config::CustomProviderConfig>(json!({"provider_type":"vertex","base_url":endpoint,"inherit_builtin_key":true,"headers":{"authorization":"MUST_NOT_MERGE","x-api-key":"MUST_NOT_MERGE","x-goog-user-project":"MUST_NOT_MERGE"}})).unwrap())]);
        let client =
            crate::provider::create_client_with_env("owned-vertex", None, &custom, None, |name| {
                if name == "VERTEX_ACCESS_TOKEN" && !adc {
                    Ok("OWNED_TOKEN".into())
                } else {
                    env(name)
                }
            })
            .unwrap();
        let crate::provider::AnyModel::Anthropic(model) =
            client.completion_model("claude-owned@20260101")
        else {
            panic!("native converter required")
        };
        let probe = model
            .completion_request(Message::user("owned literal task"))
            .max_tokens(32)
            .build();
        let view = model.replay_request_body(probe.clone()).unwrap();
        assert_eq!(view["anthropic_version"], "vertex-2023-10-16");
        assert!(view.get("model").is_none());
        assert_eq!(
            model.replay_endpoint().unwrap().path(),
            "/v1/projects/owned-project/locations/us-east5/publishers/anthropic/models/claude-owned@20260101:streamRawPredict"
        );
        let mut overridden = probe.clone();
        overridden.model = Some("claude-other".into());
        assert!(model.replay_request_body(overridden).is_err());
        let mut overridden = probe;
        overridden.additional_params = Some(json!({"anthropic_version":"WRONG"}));
        assert!(model.replay_request_body(overridden).is_err());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_count = count.clone();
        let token_server = async {
            if adc {
                let (mut socket, _) = tokens.accept().await.unwrap();
                let (headers, _, body) = request(&mut socket).await;
                assert!(headers.starts_with("post /token http/1.1"));
                assert_eq!(body["grant_type"], "refresh_token");
                let body =
                    json!({"access_token":"OWNED_TOKEN","token_type":"Bearer","expires_in":3600})
                        .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        };
        let server = async {
            for step in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (headers, _, body) = request(&mut socket).await;
                assert!(headers.starts_with("post /v1/projects/owned-project/locations/us-east5/publishers/anthropic/models/claude-owned@20260101:streamrawpredict http/1.1"));
                assert!(headers.contains("authorization: bearer owned_token"));
                assert!(!headers.contains("must_not_merge"));
                assert!(!headers.contains("x-api-key:"));
                assert!(!headers.contains("anthropic-version:"));
                assert_eq!(headers.contains("x-goog-user-project: owned-quota"), adc);
                assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
                assert!(body.get("model").is_none());
                if step == 0 {
                    assert!(body.to_string().contains("owned literal task"));
                    assert!(body.to_string().contains(Probe::NAME));
                } else {
                    assert!(body.to_string().contains("OWNED_VERTEX_TOOL_OK"));
                }
                let body = response(step);
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        };
        let task = crate::startup::heap_future(|| async move {
            let agent = rig::agent::AgentBuilder::new(model)
                .tool(Probe(task_count))
                .max_tokens(32)
                .default_max_turns(3)
                .build();
            let mut stream = agent
                .stream_chat("owned literal task", Vec::<Message>::new())
                .await;
            let mut finished = false;
            while let Some(item) = stream.next().await {
                if let rig::agent::MultiTurnStreamItem::FinalResponse(result) = item.unwrap() {
                    assert_eq!(result.output, "VERTEX_DONE");
                    assert_eq!(
                        (result.usage().input_tokens, result.usage().output_tokens),
                        (20, 4)
                    );
                    finished = true;
                }
            }
            assert!(finished);
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(token_server, server, task);
        })
        .await
        .unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn vertex81_blocking_sdk_uses_raw_predict_and_same_typed_body_contract() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let client = build_client(
        ResolvedCredential::Bearer("OWNED_TOKEN".into()),
        &endpoint,
        route(&env).unwrap(),
        reqwest::Client::new(),
    )
    .unwrap();
    let crate::provider::AnyModel::Anthropic(model) =
        client.completion_model("claude-owned@20260101")
    else {
        panic!("native required")
    };
    let request = model
        .completion_request(Message::user("owned blocking request"))
        .max_tokens(32)
        .build();
    let mut expected = model.replay_request_body(request.clone()).unwrap();
    expected.as_object_mut().unwrap().remove("stream");
    let server = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, _, body) = self::request(&mut socket).await;
        assert!(headers.starts_with("post /v1/projects/owned-project/locations/us-east5/publishers/anthropic/models/claude-owned@20260101:rawpredict http/1.1"));
        assert_eq!(body, expected);
        let body=json!({"id":"owned-blocking","type":"message","role":"assistant","model":"claude-owned@20260101","content":[{"type":"text","text":"BLOCKING_OK"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":2}}).to_string();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    };
    let task = async {
        let result = model.completion(request).await.unwrap();
        assert_eq!(
            (result.usage.input_tokens, result.usage.output_tokens),
            (10, 2)
        );
        assert!(
            serde_json::to_string(&result.choice)
                .unwrap()
                .contains("BLOCKING_OK")
        );
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, task);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn vertex81_same_origin_redirect_cannot_bypass_project_binding() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let custom = HashMap::from([(
        "owned-vertex".into(),
        serde_json::from_value::<crate::config::CustomProviderConfig>(
            json!({"provider_type":"vertex","base_url":endpoint,"api_key_env":"OWN_TOKEN"}),
        )
        .unwrap(),
    )]);
    let client =
        crate::provider::create_client_with_env("owned-vertex", None, &custom, None, |name| {
            if name == "OWN_TOKEN" {
                Ok("OWNED_TOKEN".into())
            } else {
                env(name)
            }
        })
        .unwrap();
    let crate::provider::AnyModel::Anthropic(model) = client.completion_model("claude-owned")
    else {
        panic!("native required")
    };
    let server = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, _, _) = request(&mut socket).await;
        assert!(headers.contains("authorization: bearer owned_token"));
        socket.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /v1/projects/other/locations/us-east5/publishers/anthropic/models/claude-owned:rawPredict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "credential-bearing redirected request escaped the fixed route"
        );
    };
    let task = async {
        assert!(
            model
                .completion(
                    model
                        .completion_request(Message::user("owned"))
                        .max_tokens(32)
                        .build()
                )
                .await
                .is_err()
        );
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, task);
    })
    .await
    .unwrap();
}
