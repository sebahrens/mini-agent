use super::*;
use crate::auth::{AuthResolver, CredentialSource, ProviderKind};
use std::{collections::HashMap, env::VarError};
#[test]
fn copilot81_credential_sources_are_declared_and_custom_routes_cannot_borrow_them() {
    assert_eq!(
        ProviderKind::from_name("github-copilot"),
        Some(ProviderKind::Copilot)
    );
    let missing = |_: &str| Err(VarError::NotPresent);
    let resolver = AuthResolver::new(ProviderKind::Copilot);
    assert_eq!(
        resolver.resolve_credential_with_env(missing).unwrap(),
        ResolvedCredential::CopilotLogin
    );
    assert_eq!(
        resolver.credential_source_with_env(missing).unwrap(),
        CredentialSource::CopilotLogin
    );
    assert_eq!(
        resolver
            .clone()
            .with_cli_key(Some("DIRECT_TOKEN"))
            .resolve_credential_with_env(missing)
            .unwrap(),
        ResolvedCredential::ApiKey("DIRECT_TOKEN".into())
    );
    assert_eq!(
        resolver
            .resolve_credential_with_env(|name| if name == "COPILOT_GITHUB_TOKEN" {
                Ok("GITHUB_BOOTSTRAP".into())
            } else {
                missing(name)
            })
            .unwrap(),
        ResolvedCredential::Bearer("GITHUB_BOOTSTRAP".into())
    );
    for resolver in [
        resolver.clone().with_env_override(Some("OWN_KEY")),
        resolver.clone().with_custom_provider_name(Some("gateway")),
        resolver
            .with_custom_provider_name(Some("copilot"))
            .with_credential_isolation(Some(true)),
    ] {
        assert!(
            resolver
                .resolve_credential_with_env(|name| {
                    if matches!(
                        name,
                        "GITHUB_TOKEN"
                            | "GH_TOKEN"
                            | "COPILOT_GITHUB_TOKEN"
                            | "GITHUB_COPILOT_API_TOKEN"
                    ) {
                        Ok("DO_NOT_BORROW".into())
                    } else {
                        missing(name)
                    }
                })
                .is_err()
        );
    }
    let custom=serde_json::from_value::<crate::config::CustomProviderConfig>(serde_json::json!({"provider_type":"copilot","base_url":"https://gateway.example","api_key_env":"OWN_KEY"})).unwrap();
    let client = crate::provider::create_client_with_env(
        "gateway",
        None,
        &HashMap::from([("gateway".into(), custom)]),
        None,
        |name| {
            if name == "OWN_KEY" {
                Ok("DIRECT_TOKEN".into())
            } else {
                missing(name)
            }
        },
    )
    .unwrap();
    assert!(matches!(client, AnyClient::Copilot(_)));
    assert!(!crate::config::Config::default().is_anthropic_native("copilot"));
}

use crate::config::ApiStyle;
use futures::StreamExt;
use rig::completion::{CompletionModel, Message};
use rig::streaming::StreamingChat;
use rig::tool::Tool;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Clone)]
struct Probe(Arc<AtomicUsize>);
impl rig::tool::Tool for Probe {
    const NAME: &'static str = "copilot_owned_probe";
    type Args = Value;
    type Output = String;
    type Error = std::io::Error;
    fn description(&self) -> String {
        "Read an inert owned test value".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn call(&self, _: Value) -> Result<String, std::io::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("OWNED_COPILOT_TOOL_OK".into())
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Value) {
    let mut data = Vec::new();
    while !data.ends_with(b"\r\n\r\n") {
        assert!(data.len() < 32 * 1024);
        data.push(socket.read_u8().await.unwrap());
    }
    let headers = String::from_utf8(data).unwrap().to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap();
    assert!(length < 256 * 1024);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    (headers, serde_json::from_slice(&body).unwrap())
}

fn reply(style: ApiStyle, step: usize) -> String {
    if style == ApiStyle::Responses {
        let response_item = if step == 0 {
            json!({"type":"function_call","id":"fc_copilot","call_id":"call-copilot","name":Probe::NAME,"arguments":"{}","status":"completed"})
        } else {
            json!({"type":"message","id":"msg_copilot","role":"assistant","status":"completed","content":[{"type":"output_text","text":"COPILOT_DONE","annotations":[]}]})
        };
        format!(
            "{}event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            if step == 1 {
                format!(
                    "event: response.output_text.delta\ndata: {}\n\n",
                    json!({"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_copilot","output_index":0,"content_index":0,"delta":"COPILOT_DONE"})
                )
            } else {
                String::new()
            },
            json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":response_item}),
            json!({"type":"response.completed","sequence_number":1,"response":{"id":format!("resp_copilot_{step}"),"object":"response","created_at":0,"status":"completed","model":"owned-deployment","output":[response_item],"tools":[],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}})
        )
    } else {
        let delta = if step == 0 {
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"call-copilot","type":"function","function":{"name":Probe::NAME,"arguments":"{}"}}]})
        } else {
            json!({"role":"assistant","content":"COPILOT_DONE"})
        };
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"chat_copilot","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"id":"chat_copilot","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":if step==0 {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})
        )
    }
}

async fn drive<M>(model: M, counter: Arc<AtomicUsize>)
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let agent = rig::agent::AgentBuilder::new(model)
        .tool(Probe(counter.clone()))
        .default_max_turns(3)
        .max_tokens(32)
        .build();
    let mut stream = agent
        .stream_chat("owned literal prompt", Vec::<Message>::new())
        .await;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        if let rig::agent::MultiTurnStreamItem::FinalResponse(response) = item.unwrap() {
            assert_eq!(response.output, "COPILOT_DONE");
            assert_eq!(
                (
                    response.usage().input_tokens,
                    response.usage().output_tokens
                ),
                (20, 4)
            );
            finished = true;
        }
    }
    assert!(finished);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn copilot81_actual_sdk_chat_and_responses_stream_tools_usage_and_app_identity() {
    for style in [ApiStyle::Completions, ApiStyle::Responses] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let customs=HashMap::from([("owned-copilot".into(),serde_json::from_value::<crate::config::CustomProviderConfig>(json!({"provider_type":"copilot","base_url":base,"api_key_env":"OWN_TOKEN","headers":{"authorization":"MUST_NOT_MERGE","x-api-key":"MUST_NOT_MERGE","x-goog-user-project":"MUST_NOT_MERGE","user-agent":"MUST_NOT_MERGE","editor-version":"MUST_NOT_MERGE"}})).unwrap())]);
        let client = crate::provider::create_client_with_env(
            "owned-copilot",
            None,
            &customs,
            None,
            |name| {
                if name == "OWN_TOKEN" {
                    Ok("OWNED_TOKEN".into())
                } else {
                    Err(VarError::NotPresent)
                }
            },
        )
        .unwrap();
        let name = if style == ApiStyle::Responses {
            "owned-codex"
        } else {
            "owned-chat"
        };
        let crate::provider::AnyModel::Copilot(model) = client.completion_model(name) else {
            panic!("must use actual Copilot converter")
        };
        assert_eq!(model.uses_responses(), style == ApiStyle::Responses);
        assert!(matches!(
            (style, super::instruction_protocol(&model)),
            (
                ApiStyle::Responses,
                crate::context::instructions::InstructionProtocol::Responses
            ) | (
                ApiStyle::Completions,
                crate::context::instructions::InstructionProtocol::Chat
            )
        ));
        assert_eq!(
            super::reasoning_protocol(&model),
            if style == ApiStyle::Responses {
                crate::provider::effort::Protocol::OpenAiResponses
            } else {
                crate::provider::effort::Protocol::Other
            }
        );
        let counter = Arc::new(AtomicUsize::new(0));
        let server = async {
            for step in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (headers, body) = read_request(&mut socket).await;
                let path = if style == ApiStyle::Responses {
                    "/responses"
                } else {
                    "/chat/completions"
                };
                assert!(headers.starts_with(&format!("post {path} http/1.1")));
                assert!(headers.contains("authorization: bearer owned_token"));
                assert!(headers.contains("copilot-integration-id: mini-agent"));
                assert!(headers.contains("editor-version: mini-agent/"));
                assert!(headers.contains("user-agent: mini-agent/"));
                assert!(!headers.contains("vscode"));
                assert!(!headers.contains("githubcopilotchat"));
                assert!(!headers.contains("must_not_merge"));
                assert!(headers.contains(if step == 0 {
                    "x-initiator: user"
                } else {
                    "x-initiator: agent"
                }));
                assert_eq!(body["model"], name);
                assert!(body.to_string().contains(if step == 0 {
                    "owned literal prompt"
                } else {
                    "OWNED_COPILOT_TOOL_OK"
                }));
                if style == ApiStyle::Responses {
                    assert_eq!(body["tools"][0]["strict"], true);
                }
                let body = reply(style, step);
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                server,
                crate::startup::heap_future(|| drive(model, counter))
            );
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn copilot81_guard_rejects_changed_routes_before_auth_and_uses_bounded_listing_owner() {
    let profile = Profile::for_host("github.com").unwrap();
    let auth = CopilotAuth::new(
        ResolvedCredential::Bearer("PRIVATE_GITHUB".into()),
        None,
        profile.clone(),
    )
    .unwrap();
    for url in [
        "https://evil.example/models",
        "https://api.githubcopilot.com/other",
        "https://api.githubcopilot.com/chat/completions?private=1",
    ] {
        let mut request = http::Request::get(url).body(Bytes::new()).unwrap();
        let error = auth.prepare(&mut request).await.err().unwrap();
        assert!(!error.to_string().contains("PRIVATE_GITHUB"));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let client = build_client(
        ResolvedCredential::ApiKey("OWNED_TOKEN".into()),
        Some(&base),
        profile,
        reqwest::Client::new(),
    )
    .unwrap();
    let server = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(socket.read_u8().await.unwrap());
            assert!(bytes.len() < 32768);
        }
        let headers = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
        assert!(headers.starts_with("get /models http/1.1"));
        assert!(headers.contains("authorization: bearer owned_token"));
        let body = json!({"data":[{"id":"owned-chat","object":"model"}]}).to_string();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    };
    let task = async {
        let models = client.bounded_model_listing().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "owned-chat");
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, task);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn copilot81_invalid_usage_rejects_before_a_tool_can_run_or_unsigned_totals_underflow() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let client = build_client(
        ResolvedCredential::ApiKey("OWNED_TOKEN".into()),
        Some(&base),
        Profile::for_host("github.com").unwrap(),
        reqwest::Client::new(),
    )
    .unwrap();
    let crate::provider::AnyModel::Copilot(model) = client.completion_model("owned-chat") else {
        panic!("converter")
    };
    let count = Arc::new(AtomicUsize::new(0));
    let task_count = count.clone();
    let server = async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut socket).await;
        let body =
            reply(ApiStyle::Completions, 0).replace("\"total_tokens\":12", "\"total_tokens\":1");
        assert!(body.contains("\"total_tokens\":1"));
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    };
    let task = crate::startup::heap_future(|| async move {
        let agent = rig::agent::AgentBuilder::new(model)
            .tool(Probe(task_count))
            .max_tokens(32)
            .build();
        let mut stream = agent.stream_chat("owned", Vec::<Message>::new()).await;
        let mut failed = false;
        while let Some(item) = stream.next().await {
            match item {
                Err(error) => {
                    assert!(error.to_string().contains("usage metrics"));
                    failed = true;
                }
                Ok(rig::agent::MultiTurnStreamItem::FinalResponse(_)) => {
                    panic!("malformed usage completed")
                }
                _ => (),
            }
        }
        assert!(failed);
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, task);
    })
    .await
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn copilot81_blocking_usage_underflow_and_actual_factory_redirect_fail_closed() {
    for mode in 0..3 {
        let redirect = mode == 1;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let custom = serde_json::from_value::<crate::config::CustomProviderConfig>(
            json!({"provider_type":"copilot","base_url":base,"api_key_env":"OWN_TOKEN"}),
        )
        .unwrap();
        let client = crate::provider::create_client_with_env(
            "owned-copilot",
            None,
            &HashMap::from([("owned-copilot".into(), custom)]),
            None,
            |name| {
                if name == "OWN_TOKEN" {
                    Ok("OWNED_TOKEN".into())
                } else {
                    Err(VarError::NotPresent)
                }
            },
        )
        .unwrap();
        let crate::provider::AnyModel::Copilot(model) = client.completion_model("owned-chat")
        else {
            panic!("converter")
        };
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, _) = read_request(&mut socket).await;
            assert!(headers.starts_with("post /chat/completions "));
            if redirect {
                socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {base}/chat/completions\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "authenticated redirect must not be followed, even on the same origin"
                );
            } else if mode == 2 {
                // No body is sent: private auth errors must preserve status but
                // retire the response immediately instead of reading an echo.
                socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n").await.unwrap();
                let mut byte = [0];
                assert!(
                    matches!(
                        tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                            .await
                            .unwrap(),
                        Ok(0) | Err(_)
                    ),
                    "private HTTP error must retire its unread body"
                );
            } else {
                let body = json!({"id":"owned","object":"chat.completion","created":0,"model":"owned-chat","choices":[{"index":0,"message":{"role":"assistant","content":"owned"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":1}}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        };
        let task = crate::startup::heap_future(|| async move {
            let error = model
                .completion_request("owned")
                .max_tokens(32)
                .send()
                .await
                .unwrap_err();
            if mode == 0 {
                assert!(error.to_string().contains("usage metrics"), "{error}");
            } else if mode == 2 {
                assert!(
                    error.to_string().contains("Copilot request was rejected"),
                    "{error}"
                );
                assert!(error.to_string().contains("403"), "{error}");
            }
            assert!(!error.to_string().contains("OWNED_TOKEN"));
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, task);
        })
        .await
        .unwrap();
    }
}
