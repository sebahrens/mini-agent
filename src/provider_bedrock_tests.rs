use super::*;
use crate::auth::{AuthResolver, CredentialSource, ProviderKind};
use crate::tests::support::loopback_http::asynchronous::{
    bind, read_head, read_request, respond_sse,
};
use futures::StreamExt;
use rig::completion::Message;
use rig::streaming::StreamingChat;
use rig::tool::Tool;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env::VarError;
use tokio::io::AsyncWriteExt;

#[test]
fn bedrock81_region_endpoint_and_typed_chain_isolation() {
    assert!(crate::config::Config::default().is_anthropic_native("bedrock"));
    assert!(crate::config::Config::default().is_anthropic_native("AWS-BEDROCK"));
    assert_eq!(
        ProviderKind::from_name("aws-bedrock"),
        Some(ProviderKind::Bedrock)
    );
    assert_eq!(
        region(&|name| if name == "AWS_DEFAULT_REGION" {
            Ok("us-east-1".into())
        } else {
            Err(VarError::NotPresent)
        })
        .unwrap(),
        "us-east-1"
    );
    assert!(region(&|_| Ok("us-east-1.evil.example/".into())).is_err());
    assert_eq!(
        endpoint(None, "us-east-1").unwrap(),
        "https://bedrock-runtime.us-east-1.amazonaws.com/anthropic"
    );
    for bad in [
        "https://user:secret@aws.example/anthropic",
        "https://aws.example/model/x/invoke",
        "https://aws.example/anthropic?secret=ignored",
    ] {
        assert!(endpoint(Some(bad), "us-east-1").is_err());
    }
    let missing = |_: &str| Err(VarError::NotPresent);
    let resolver = AuthResolver::new(ProviderKind::Bedrock);
    assert_eq!(
        resolver.resolve_credential_with_env(missing).unwrap(),
        ResolvedCredential::AwsDefaultChain
    );
    assert_eq!(
        resolver.credential_source_with_env(missing).unwrap(),
        CredentialSource::AwsDefaultChain
    );
    assert!(
        resolver
            .clone()
            .with_env_override(Some("OWN_GATEWAY_KEY"))
            .resolve_credential_with_env(missing)
            .is_err()
    );
    assert!(
        resolver
            .clone()
            .with_custom_provider_name(Some("gateway"))
            .resolve_credential_with_env(missing)
            .is_err()
    );
    assert!(
        resolver
            .clone()
            .with_custom_provider_name(Some("bedrock"))
            .with_credential_isolation(Some(true))
            .resolve_credential_with_env(missing)
            .is_err()
    );
    assert_eq!(
        resolver
            .clone()
            .with_cli_key(Some("owned-key"))
            .resolve_credential_with_env(missing)
            .unwrap(),
        ResolvedCredential::ApiKey("owned-key".into())
    );
    assert_eq!(
        resolver
            .resolve_credential_with_env(|name| if name == "AWS_BEARER_TOKEN_BEDROCK" {
                Ok("owned-token".into())
            } else {
                Err(VarError::NotPresent)
            })
            .unwrap(),
        ResolvedCredential::ApiKey("owned-token".into())
    );
}

#[test]
fn bedrock81_sigv4_known_independent_hmac_vector_binds_exact_body_and_headers() {
    let body=Bytes::from_static(br#"{"model":"us.anthropic.claude-sonnet-5","max_tokens":32,"messages":[{"role":"user","content":"owned"}]}"#);
    let mut request = http::Request::builder()
        .method("POST")
        .uri("https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1/messages")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(body.clone())
        .unwrap();
    let credentials = aws_credential_types::Credentials::new(
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        Some("SESSION_TEST".into()),
        None,
        "owned-fixture",
    );
    sign_request(
        &mut request,
        credentials,
        "us-east-1",
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160),
    )
    .unwrap();
    assert_eq!(request.body(), &body);
    assert_eq!(
        request.headers()["x-amz-content-sha256"],
        "903d68e5ffe0ffa5cfedddd8609884c7d5e4a2945f8d0bc65af480e1f31deff2"
    );
    assert_eq!(
        request.headers()["authorization"],
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, SignedHeaders=anthropic-version;content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=7f46d36ff18d75f55ad28f796bc0f200c5b86c3ebbbfc80237ed110b1bed06dd"
    );
    assert!(request.headers()["authorization"].is_sensitive());
    assert!(request.headers()["x-amz-security-token"].is_sensitive());
}

#[derive(Clone)]
struct Probe(Arc<std::sync::atomic::AtomicUsize>);
impl Tool for Probe {
    const NAME: &'static str = "bedrock_owned_probe";
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
        Ok("OWNED_BEDROCK_TOOL_OK".into())
    }
}
fn response(step: usize) -> String {
    let event = |kind: &str, value: Value| format!("event: {kind}\ndata: {value}\n\n");
    let mut body = event(
        "message_start",
        json!({"type":"message_start","message":{"id":format!("bedrock-{step}"),"type":"message","role":"assistant","model":"us.anthropic.owned","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
    );
    if step == 0 {
        body += &event(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_bedrock","name":Probe::NAME,"input":{}}}),
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
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"BEDROCK_DONE"}}),
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
async fn bedrock81_real_sdk_key_env_profile_chain_streams_tools_and_usage_without_rewriting_body() {
    use sha2::Digest;
    for source in ["api-key", "env", "profile"] {
        let temp = crate::tests::OwnedTestDirectory::new("bedrock81-auth");
        let profile = temp.path().join("credentials");
        std::fs::write(&profile,"[owned]\naws_access_key_id = OWNED_PROFILE_ID\naws_secret_access_key = OWNED_PROFILE_SECRET\naws_session_token = OWNED_PROFILE_SESSION\n").unwrap();
        let empty_config = temp.path().join("config");
        std::fs::write(&empty_config, "").unwrap();
        let _env = crate::tests::ScopedProcessEnv::set(&[
            (
                "AWS_ACCESS_KEY_ID",
                (source == "env").then(|| "OWNED_ENV_ID".into()),
            ),
            (
                "AWS_SECRET_ACCESS_KEY",
                (source == "env").then(|| "OWNED_ENV_SECRET".into()),
            ),
            (
                "AWS_SESSION_TOKEN",
                (source == "env").then(|| "OWNED_ENV_SESSION".into()),
            ),
            ("AWS_PROFILE", Some("owned".into())),
            (
                "AWS_SHARED_CREDENTIALS_FILE",
                Some(profile.into_os_string()),
            ),
            ("AWS_CONFIG_FILE", Some(empty_config.into_os_string())),
            ("AWS_EC2_METADATA_DISABLED", Some("true".into())),
        ]);
        let (listener, base) = bind().await;
        let endpoint = format!("{base}/anthropic");
        let custom=HashMap::from([("owned-bedrock".into(),serde_json::from_value::<crate::config::CustomProviderConfig>(json!({"provider_type":"bedrock","base_url":endpoint,"inherit_builtin_key":true,"headers":{"authorization":"MUST_NOT_MERGE","x-api-key":"MUST_NOT_MERGE","x-amz-security-token":"MUST_NOT_MERGE"}})).unwrap())]);
        let client =
            crate::provider::create_client_with_env("owned-bedrock", None, &custom, None, |name| {
                match name {
                    "AWS_REGION" => Ok("us-east-1".into()),
                    "AWS_BEARER_TOKEN_BEDROCK" if source == "api-key" => {
                        Ok("OWNED_BEDROCK_KEY".into())
                    }
                    _ => Err(VarError::NotPresent),
                }
            })
            .unwrap();
        let crate::provider::AnyModel::Anthropic(model) =
            client.completion_model("us.anthropic.owned")
        else {
            panic!("must use actual Anthropic Messages converter")
        };
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_count = count.clone();
        let server = async {
            for step in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let req = read_request(&mut socket).await;
                let (headers, body) = (req.headers.clone(), req.json());
                assert!(headers.starts_with("post /anthropic/v1/messages http/1.1"));
                assert!(headers.contains("\r\nanthropic-version: 2023-06-01\r\n"));
                assert!(!headers.contains("must_not_merge"));
                if source == "api-key" {
                    assert!(headers.contains("\r\nx-api-key: owned_bedrock_key\r\n"));
                    assert!(!headers.contains("\r\nauthorization:"));
                } else {
                    let id = if source == "env" {
                        "owned_env_id"
                    } else {
                        "owned_profile_id"
                    };
                    assert!(headers.contains(&format!("credential={id}/")));
                    assert!(headers.contains("/us-east-1/bedrock/aws4_request"));
                    assert!(headers.contains("x-amz-security-token:"));
                    assert!(!headers.contains("\r\nx-api-key:"));
                    assert!(headers.contains(&format!(
                            "x-amz-content-sha256: {}",
                            sha2::Sha256::digest(&req.body)
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>()
                        )));
                }
                assert_eq!(body["model"], "us.anthropic.owned");
                assert!(body.get("anthropic_version").is_none());
                if step == 0 {
                    assert!(body.to_string().contains("owned literal task"));
                    assert!(body.to_string().contains(Probe::NAME));
                } else {
                    assert!(body.to_string().contains("OWNED_BEDROCK_TOOL_OK"));
                    assert!(body.to_string().contains("toolu_bedrock"));
                }
                let body = response(step);
                respond_sse(&mut socket, &body).await;
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
                    assert_eq!(result.output, "BEDROCK_DONE");
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
            tokio::join!(server, task);
        })
        .await
        .unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn bedrock81_process_profiles_are_rejected_before_env_fallback_or_dispatch() {
    for indirect in [false, true] {
        let temp = crate::tests::OwnedTestDirectory::new("bedrock81-no-process");
        let profile = temp.path().join("credentials");
        let config = temp.path().join("config");
        let marker = temp.path().join("must-not-exist");
        std::fs::write(
            &profile,
            format!(
                "[owned]\n{}\n[process]\ncredential_process = echo PRIVATE_COMMAND > {}\n",
                if indirect {
                    "role_arn = arn:aws:iam::000000000000:role/owned\nsource_profile = process"
                } else {
                    "credential_process = echo PRIVATE_COMMAND"
                },
                marker.display()
            ),
        )
        .unwrap();
        std::fs::write(&config, "").unwrap();
        let _env = crate::tests::ScopedProcessEnv::set(&[
            ("AWS_ACCESS_KEY_ID", Some("MUST_NOT_FALL_BACK".into())),
            ("AWS_SECRET_ACCESS_KEY", Some("MUST_NOT_FALL_BACK".into())),
            ("AWS_PROFILE", Some("owned".into())),
            (
                "AWS_SHARED_CREDENTIALS_FILE",
                Some(profile.into_os_string()),
            ),
            ("AWS_CONFIG_FILE", Some(config.into_os_string())),
            ("AWS_EC2_METADATA_DISABLED", Some("true".into())),
        ]);
        let auth = BedrockAuth::new(
            ResolvedCredential::AwsDefaultChain,
            "http://127.0.0.1:1/anthropic",
            "us-east-1",
        )
        .unwrap();
        let mut request = http::Request::builder()
            .method("POST")
            .uri("http://127.0.0.1:1/anthropic/v1/messages")
            .body(Bytes::from_static(b"owned canonical body"))
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), auth.prepare(&mut request))
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("credential_process profiles are unsupported")
        );
        assert!(!error.to_string().contains("PRIVATE_COMMAND"));
        assert!(!marker.exists());
        assert_eq!(request.body(), &Bytes::from_static(b"owned canonical body"));
        assert!(!auth.sdk.initialized());
    }
}

#[tokio::test]
async fn bedrock81_credentials_http_has_byte_and_time_limits_without_model_headers() {
    for case in ["ok", "oversize", "timeout"] {
        let (listener, base) = bind().await;
        let uri = format!("{base}/credential");
        let connector = CredentialsHttp(
            crate::provider::build_http_client("owned credentials", false, None, None).unwrap(),
            Duration::from_millis(50),
        );
        let server = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let header = read_head(&mut socket).await.headers;
            assert!(!header.contains("x-api-key:") && !header.contains("must_not_merge"));
            if case == "timeout" {
                tokio::time::sleep(Duration::from_millis(100)).await;
            } else {
                let body = "{\"AccessKeyId\":\"OWNED\"}";
                let length = if case == "oversize" {
                    AUTH_BODY_LIMIT + 1
                } else {
                    body.len()
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
            }
        };
        let req: http::Request<SdkBody> = http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(SdkBody::empty())
            .unwrap();
        let task = connector.call(req.try_into().unwrap());
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(task, server) })
                .await
                .unwrap();
        if case == "ok" {
            assert_eq!(
                result.unwrap().body().bytes().unwrap(),
                b"{\"AccessKeyId\":\"OWNED\"}"
            );
        } else {
            let error = result.unwrap_err();
            assert!(!error.to_string().contains("AccessKeyId"));
        }
    }
}

#[tokio::test]
async fn bedrock_profile_files_are_parsed_once_until_they_or_the_selecting_env_change() {
    let temp = crate::tests::OwnedTestDirectory::new("bedrock-profile-once");
    let credentials = temp.path().join("credentials");
    let config = temp.path().join("config");
    std::fs::write(
        &credentials,
        "[owned]\naws_access_key_id = OWNED_ID\naws_secret_access_key = OWNED_SECRET\n",
    )
    .unwrap();
    std::fs::write(&config, "").unwrap();
    let _env = crate::tests::ScopedProcessEnv::set(&[
        ("AWS_ACCESS_KEY_ID", Some("OWNED_ENV_ID".into())),
        ("AWS_SECRET_ACCESS_KEY", Some("OWNED_ENV_SECRET".into())),
        ("AWS_PROFILE", Some("owned".into())),
        (
            "AWS_SHARED_CREDENTIALS_FILE",
            Some(credentials.clone().into_os_string()),
        ),
        ("AWS_CONFIG_FILE", Some(config.into_os_string())),
        ("AWS_EC2_METADATA_DISABLED", Some("true".into())),
    ]);
    let auth = BedrockAuth::new(
        ResolvedCredential::AwsDefaultChain,
        "http://127.0.0.1:1/anthropic",
        "us-east-1",
    )
    .unwrap();
    async fn prepare(auth: &BedrockAuth) -> http_client::Result<()> {
        let mut request = http::Request::builder()
            .method("POST")
            .uri("http://127.0.0.1:1/anthropic/v1/messages")
            .body(Bytes::from_static(b"body"))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), auth.prepare(&mut request))
            .await
            .unwrap()
    }
    PROFILE_CHECKS.with(|checks| checks.set(0));
    prepare(&auth).await.unwrap();
    prepare(&auth).await.unwrap();
    prepare(&auth).await.unwrap();
    assert_eq!(PROFILE_CHECKS.with(std::cell::Cell::get), 1);
    // An edit that adds a credential_process is still rejected on the next
    // request, without a restart.
    std::fs::write(
        &credentials,
        "[owned]\naws_access_key_id = OWNED_ID\naws_secret_access_key = OWNED_SECRET\ncredential_process = echo PRIVATE_COMMAND\n",
    )
    .unwrap();
    let error = prepare(&auth).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("credential_process profiles are unsupported")
    );
    assert_eq!(PROFILE_CHECKS.with(std::cell::Cell::get), 2);
}
