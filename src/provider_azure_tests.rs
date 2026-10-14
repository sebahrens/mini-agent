use super::*;
use crate::auth::{AuthResolver, CredentialSource, ProviderKind};
use crate::provider::{OpenAiModel, create_client_with_env};
use futures::StreamExt;
use rig::completion::{CompletionModel, Message};
use rig::streaming::StreamingChat;
use rig::tool::Tool;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env::VarError;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn azure81_kind_endpoint_and_deployment_protocol_are_explicit() {
    for name in ["azure", "azure-openai", "azure_openai", "AZURE"] {
        assert_eq!(ProviderKind::from_name(name), Some(ProviderKind::Azure));
    }
    for input in [
        "https://tenant.openai.azure.com",
        "https://tenant.openai.azure.com/",
        "https://tenant.openai.azure.com/openai/v1/",
    ] {
        assert_eq!(
            normalize_endpoint(Some(input)).unwrap(),
            "https://tenant.openai.azure.com/openai/v1"
        );
    }
    for input in [
        "https://private-user:private-secret@tenant.openai.azure.com",
        "https://tenant.openai.azure.com/openai/deployments/deployment",
        "https://tenant.openai.azure.com?api-version=preview",
        "file:///private",
        "garbage",
    ] {
        let error = normalize_endpoint(Some(input)).unwrap_err().to_string();
        assert!(!error.contains("private-secret"));
    }
    assert!(normalize_endpoint(None).is_err());
    assert_eq!(api_style(None), ApiStyle::Responses);
    assert!(!ProviderKind::Azure.is_builtin_endpoint("https://tenant.openai.azure.com"));
}

#[test]
fn azure81_auth_precedence_is_typed_and_never_borrows_openai_secrets() {
    let env = |name: &str| match name {
        "AZURE_OPENAI_API_KEY" => Ok("azure-key".into()),
        "AZURE_OPENAI_AD_TOKEN" => Ok("azure-token".into()),
        "OPENAI_API_KEY" => Ok("wrong-openai-secret".into()),
        _ => Err(VarError::NotPresent),
    };
    let resolver = AuthResolver::new(ProviderKind::Azure);
    assert_eq!(
        resolver.resolve_credential_with_env(env).unwrap(),
        ResolvedCredential::ApiKey("azure-key".into())
    );
    assert_eq!(
        resolver
            .clone()
            .with_cli_key(Some("cli-key"))
            .resolve_credential_with_env(env)
            .unwrap(),
        ResolvedCredential::ApiKey("cli-key".into())
    );
    let token_env = |name: &str| {
        if name == "AZURE_OPENAI_AD_TOKEN" {
            Ok("azure-token".into())
        } else {
            Err(VarError::NotPresent)
        }
    };
    assert_eq!(
        resolver.resolve_credential_with_env(token_env).unwrap(),
        ResolvedCredential::Bearer("azure-token".into())
    );
    assert_eq!(
        resolver.credential_source_with_env(token_env).unwrap(),
        CredentialSource::Environment("AZURE_OPENAI_AD_TOKEN".into())
    );
    let keys = HashMap::from([
        ("azure".into(), "config-key".into()),
        ("openai".into(), "wrong-config-secret".into()),
    ]);
    assert_eq!(
        resolver
            .clone()
            .with_config_keys(Some(&keys))
            .resolve_credential_with_env(|_| Err(VarError::NotPresent))
            .unwrap(),
        ResolvedCredential::ApiKey("config-key".into())
    );
    assert!(
        resolver
            .resolve_credential_with_env(|name| if name == "OPENAI_API_KEY" {
                Ok("wrong-openai-secret".into())
            } else {
                Err(VarError::NotPresent)
            })
            .is_err()
    );
}

#[test]
fn azure81_custom_routes_remain_isolated_and_missing_endpoint_fails_before_auth() {
    let custom: CustomProviderConfig = serde_json::from_value(
        json!({"provider_type":"azure","base_url":"https://gateway.example/openai/v1"}),
    )
    .unwrap();
    for name in ["azure", "gateway"] {
        let providers = HashMap::from([(name.into(), custom.clone())]);
        assert!(
            create_client_with_env(name, None, &providers, None, |key| match key {
                "AZURE_OPENAI_API_KEY" | "AZURE_OPENAI_AD_TOKEN" | "OPENAI_API_KEY" =>
                    Ok("must-not-leak".into()),
                _ => Err(VarError::NotPresent),
            })
            .is_err()
        );
    }
    assert!(
        create_client_with_env("azure", None, &HashMap::new(), None, |key| {
            assert_eq!(key, "AZURE_OPENAI_ENDPOINT");
            Err(VarError::NotPresent)
        })
        .is_err()
    );
    let mut custom = custom;
    custom.api_key_env = Some("OWN_AZURE_GATEWAY_KEY".into());
    let client = create_client_with_env(
        "gateway",
        None,
        &HashMap::from([("gateway".into(), custom)]),
        None,
        |key| {
            if key == "OWN_AZURE_GATEWAY_KEY" {
                Ok("own-key".into())
            } else {
                Err(VarError::NotPresent)
            }
        },
    )
    .unwrap();
    assert!(matches!(
        client,
        AnyClient::OpenAI(OpenAiClient::Responses(_))
    ));
}

#[test]
fn azure81_responses_config_validation_matches_the_actual_adapter() {
    let mut cfg: crate::config::Config = serde_json::from_value(json!({"provider":"azure","extra_body":{"previous_response_id":"must-be-stripped","include":["reasoning.encrypted_content"]}})).unwrap();
    let warnings = crate::config::load::validate_extra_body(&mut cfg).unwrap();
    assert!(!warnings.is_empty());
    assert!(
        cfg.extra_body
            .as_ref()
            .unwrap()
            .get("previous_response_id")
            .is_none()
    );
    let mut cfg: crate::config::Config = serde_json::from_value(
        json!({"provider":"azure","extra_body":{"include":["unsupported"]}}),
    )
    .unwrap();
    assert!(crate::config::load::validate_extra_body(&mut cfg).is_err());
}

#[derive(Clone)]
struct Probe(Arc<AtomicUsize>);
impl rig::tool::Tool for Probe {
    const NAME: &'static str = "azure_owned_probe";
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
        Ok("OWNED_AZURE_TOOL_OK".into())
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
            json!({"type":"function_call","id":"fc_azure","call_id":"call-azure","name":Probe::NAME,"arguments":"{}","status":"completed"})
        } else {
            json!({"type":"message","id":"msg_azure","role":"assistant","status":"completed","content":[{"type":"output_text","text":"AZURE_DONE","annotations":[]}]})
        };
        format!(
            "{}event: response.output_item.done\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            if step == 1 {
                format!(
                    "event: response.output_text.delta\ndata: {}\n\n",
                    json!({"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_azure","output_index":0,"content_index":0,"delta":"AZURE_DONE"})
                )
            } else {
                String::new()
            },
            json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":response_item}),
            json!({"type":"response.completed","sequence_number":1,"response":{"id":format!("resp_azure_{step}"),"object":"response","created_at":0,"status":"completed","model":"owned-deployment","output":[response_item],"tools":[],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}})
        )
    } else {
        let delta = if step == 0 {
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"call-azure","type":"function","function":{"name":Probe::NAME,"arguments":"{}"}}]})
        } else {
            json!({"role":"assistant","content":"AZURE_DONE"})
        };
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"chat_azure","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"id":"chat_azure","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":if step==0 {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})
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
            assert_eq!(response.output, "AZURE_DONE");
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
async fn azure81_wire_streaming_tools_usage_and_exclusive_auth_for_both_v1_apis() {
    for style in [ApiStyle::Responses, ApiStyle::Completions] {
        for auth in ["api-key", "bearer", "cli"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let custom = if style == ApiStyle::Completions {
                HashMap::from([("azure".into(), serde_json::from_value::<CustomProviderConfig>(json!({"provider_type":"azure","base_url":endpoint,"api_style":"completions","inherit_builtin_key":true,"headers":{"Authorization":"MUST_NOT_MERGE","Api-Key":"MUST_NOT_MERGE","X-Api-Key":"MUST_NOT_MERGE"}})).unwrap())])
            } else {
                HashMap::new()
            };
            let client = create_client_with_env(
                "azure",
                (auth == "cli").then_some("owned-cli-key"),
                &custom,
                None,
                |name| match name {
                    "AZURE_OPENAI_ENDPOINT" => Ok(endpoint.clone()),
                    "AZURE_OPENAI_API_KEY" if auth == "api-key" => Ok("owned-api-key".into()),
                    "AZURE_OPENAI_AD_TOKEN" => Ok("owned-bearer-token".into()),
                    _ => Err(VarError::NotPresent),
                },
            )
            .unwrap();
            let debug = match &client {
                AnyClient::OpenAI(OpenAiClient::Responses(c)) => format!("{c:?}"),
                AnyClient::OpenAI(OpenAiClient::Completions(c)) => format!("{c:?}"),
                _ => unreachable!(),
            };
            assert!(
                !debug.contains("owned-api-key")
                    && !debug.contains("owned-bearer-token")
                    && !debug.contains("owned-cli-key")
            );
            let model = client.completion_model("owned-deployment");
            let counter = Arc::new(AtomicUsize::new(0));
            let server = async {
                for step in 0..2 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let (headers, body) = read_request(&mut socket).await;
                    assert!(headers.starts_with(if style == ApiStyle::Responses {
                        "post /openai/v1/responses http/1.1"
                    } else {
                        "post /openai/v1/chat/completions http/1.1"
                    }));
                    if auth == "bearer" {
                        assert!(
                            headers.contains("\r\nauthorization: bearer owned-bearer-token\r\n")
                        );
                        assert!(!headers.contains("\r\napi-key:"));
                    } else {
                        assert!(headers.contains(if auth == "cli" {
                            "\r\napi-key: owned-cli-key\r\n"
                        } else {
                            "\r\napi-key: owned-api-key\r\n"
                        }));
                        assert!(!headers.contains("\r\nauthorization:"));
                    }
                    assert!(
                        !headers.contains("must_not_merge") && !headers.contains("\r\nx-api-key:")
                    );
                    assert_eq!(body["model"], "owned-deployment");
                    if step == 0 {
                        assert!(body.to_string().contains(Probe::NAME));
                        assert!(body.to_string().contains("owned literal prompt"));
                    } else {
                        assert!(body.to_string().contains("OWNED_AZURE_TOOL_OK"));
                        assert!(body.to_string().contains("call-azure"));
                    }
                    let body = reply(style, step);
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
            };
            let request = async {
                match model {
                    crate::provider::AnyModel::OpenAI(OpenAiModel::Responses(model)) => {
                        crate::startup::heap_future(|| drive(model, counter)).await
                    }
                    crate::provider::AnyModel::OpenAI(OpenAiModel::Completions(model)) => {
                        crate::startup::heap_future(|| drive(model, counter)).await
                    }
                    _ => panic!("Azure v1 must use the matching SDK adapter"),
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(server, request);
            })
            .await
            .unwrap();
        }
    }
}
