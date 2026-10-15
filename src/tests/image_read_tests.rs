//! Image-read admission and canonical history, selected from the ReadTool,
//! persist_headless_turn_input and provider request-conversion callers.
use base64::Engine;
use rig::{
    OneOrMany,
    completion::Message,
    message::{AssistantContent, ToolResult, ToolResultContent, UserContent},
};

const PNG: &[u8] = include_bytes!("../agent/tools/fixtures/read-image64/3x2.png");
fn image() -> rig::message::Image {
    rig::message::Image {
        data: rig::message::DocumentSourceKind::Base64(
            base64::engine::general_purpose::STANDARD.encode(PNG),
        ),
        media_type: Some(rig::message::ImageMediaType::PNG),
        ..Default::default()
    }
}
fn interactions() -> Vec<Message> {
    let mut messages = vec![
        Message::Assistant {
            id: None,
            content: OneOrMany::many(vec![
                AssistantContent::tool_call(
                    "image-call",
                    "read",
                    serde_json::json!({"path":"pixel.png"}),
                ),
                AssistantContent::tool_call(
                    "text-call",
                    "read",
                    serde_json::json!({"path":"notes.txt"}),
                ),
            ])
            .unwrap(),
        },
        Message::User {
            content: OneOrMany::many(vec![
                UserContent::ToolResult(ToolResult {
                    id: "image-call".into(),
                    call_id: None,
                    content: OneOrMany::many(vec![
                        ToolResultContent::text("Image: pixel.png"),
                        ToolResultContent::Image(image()),
                    ])
                    .unwrap(),
                }),
                UserContent::ToolResult(ToolResult {
                    id: "text-call".into(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text("notes remain text")),
                }),
            ])
            .unwrap(),
        },
    ];
    for message in &mut messages {
        match message {
            Message::Assistant { content, .. } => {
                for part in content.iter_mut() {
                    if let AssistantContent::ToolCall(call) = part {
                        call.call_id = Some(call.id.clone());
                    }
                }
            }
            Message::User { content } => {
                for part in content.iter_mut() {
                    if let UserContent::ToolResult(result) = part {
                        result.call_id = Some(result.id.clone());
                    }
                }
            }
            _ => {}
        }
    }
    messages
}

#[test]
fn image_read64_canonical_tool_images_survive_headless_storage_and_replay() {
    let mut session = crate::session::Session::new("anthropic", "fixture", 200_000, "");
    crate::print::persist_headless_turn_input(
        &mut session,
        &crate::agent::input::PromptInput::text("read it".into()),
        "seen",
        &interactions(),
    )
    .unwrap();
    let encoded = serde_json::to_string(&session).unwrap();
    let restored: crate::session::Session = serde_json::from_str(&encoded).unwrap();
    let history = crate::agent::runner::convert_history(&restored);
    let images: Vec<_> = history
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .flat_map(|part| match part {
            UserContent::ToolResult(result) => result.content.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .filter_map(|part| match part {
            ToolResultContent::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    assert_eq!(images, vec![&image()]);
    assert!(encoded.contains("image/png"));
    assert!(
        !format!(
            "{:?}",
            crate::session::tool_image::PersistedToolImage::from_rig(&image()).unwrap()
        )
        .contains(&base64::engine::general_purpose::STANDARD.encode(PNG))
    );
    let mut tampered: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    let rows = tampered["messages"].as_array_mut().unwrap();
    let row = rows
        .iter_mut()
        .find(|row| row["tool"]["images"].is_array())
        .unwrap();
    row["role"] = serde_json::json!("user");
    assert!(serde_json::from_value::<crate::session::Session>(tampered).is_err());
}

#[test]
fn image_read64_import_rejects_urls_bad_mime_and_oversized_base64_before_use() {
    let valid = serde_json::to_value(
        crate::session::tool_image::PersistedToolImage::from_rig(&image()).unwrap(),
    )
    .unwrap();
    let mut bad = valid.clone();
    bad["mime_type"] = serde_json::json!("image/jpeg");
    assert!(serde_json::from_value::<crate::session::tool_image::PersistedToolImage>(bad).is_err());
    let mut bad = valid.clone();
    bad["data"] = serde_json::json!("https://never.invalid/image.png");
    assert!(serde_json::from_value::<crate::session::tool_image::PersistedToolImage>(bad).is_err());
    let mut bad = valid;
    bad["data"] = serde_json::json!("A".repeat(7 * 1024 * 1024));
    assert!(serde_json::from_value::<crate::session::tool_image::PersistedToolImage>(bad).is_err());
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn image_read64_actual_read_keeps_bytes_permissions_and_overwrite_proof_separate() {
    use crate::agent::tools::{ReadArgs, ReadTool, ReadTracker, read_image::ImageReadSupport};
    use rig::tool::Tool;
    let root = super::OwnedTestDirectory::new("image64-read");
    let fixtures: [(&str, &[u8]); 6] = [
        ("png", PNG),
        (
            "jpeg",
            include_bytes!("../agent/tools/fixtures/read-image64/3x2.jpg"),
        ),
        (
            "gif",
            include_bytes!("../agent/tools/fixtures/read-image64/3x2.gif"),
        ),
        (
            "webp",
            include_bytes!("../agent/tools/fixtures/read-image64/3x2-lossy.webp"),
        ),
        (
            "lossless",
            include_bytes!("../agent/tools/fixtures/read-image64/3x2-lossless.webp"),
        ),
        (
            "alpha",
            include_bytes!("../agent/tools/fixtures/read-image64/3x2-alpha.webp"),
        ),
    ];
    let mut tracker = ReadTracker::new(true);
    tracker.image_support = ImageReadSupport::captured("anthropic", "claude-fable-5-1", false);
    let tool = ReadTool::new_with_tracker(None, None, Some(16), 100, tracker.clone())
        .with_workspace(root.path());
    let args = |path: &str| ReadArgs {
        path: path.into(),
        offset: None,
        limit: None,
        raw_notebook: false,
    };
    for (name, bytes) in fixtures {
        std::fs::write(root.path().join(name), bytes).unwrap();
        let output = tool.call(args(name)).await.unwrap();
        let parts = ToolResultContent::from_tool_output(output.clone());
        let [
            ToolResultContent::Text(text),
            ToolResultContent::Image(image),
        ] = parts.iter().collect::<Vec<_>>().as_slice()
        else {
            panic!("missing structured image: {output}")
        };
        assert!(text.text.contains("3 x 2"), "{}", text.text);
        let rig::message::DocumentSourceKind::Base64(data) = &image.data else {
            panic!("not literal")
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            bytes
        );
        let path = root.path().join(name);
        assert!(!tracker.permits_full_overwrite(
            path.to_str().unwrap(),
            &std::fs::metadata(&path).unwrap(),
            crate::agent::tools::crc::crc32(bytes),
            1
        ));
        assert!(
            tool.call(args(name)).await.is_ok(),
            "images do not claim a raw text read"
        );
    }
    let mut line = args("png");
    line.limit = Some(1);
    assert!(
        tool.call(line)
            .await
            .unwrap_err()
            .to_string()
            .contains("offset/limit")
    );
    let mut unsupported = tracker.clone();
    unsupported.image_support = ImageReadSupport::captured("openai", "gpt-3.5-turbo", false);
    let error = ReadTool::new_with_tracker(None, None, None, 100, unsupported)
        .with_workspace(root.path())
        .call(args("png"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no vision"), "{error}");
    let mut unknown = tracker.clone();
    unknown.image_support = ImageReadSupport::captured("openai", "owned-unknown-gateway", false);
    assert!(
        ReadTool::new_with_tracker(None, None, None, 100, unknown)
            .with_workspace(root.path())
            .call(args("png"))
            .await
            .is_ok()
    );
    let mut gemini = tracker;
    gemini.image_support = ImageReadSupport::captured("gemini", "gemini-2.5-pro", true);
    assert!(
        ReadTool::new_with_tracker(None, None, None, 100, gemini)
            .with_workspace(root.path())
            .call(args("gif"))
            .await
            .unwrap_err()
            .to_string()
            .contains("GIF")
    );
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn image_read64_actual_provider_bodies_keep_image_bytes_and_batch_correlation() {
    use crate::provider::{AnyModel, OpenAiModel};
    use rig::completion::{CompletionModel, CompletionRequest};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (protocol, style) in [
        ("anthropic", "completions"),
        ("openai", "completions"),
        ("openai", "responses"),
        ("openrouter", "completions"),
        ("gemini", "completions"),
        ("ollama", "completions"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cfg: crate::config::Config=serde_json::from_value(serde_json::json!({"custom_providers":{"image64-wire":{"provider_type":protocol,"base_url":format!("http://{address}"),"api_style":style}}})).unwrap();
        let client = crate::provider::create_client(
            "image64-wire",
            Some("unused-owned-fixture"),
            &cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        let model = client.completion_model("owned-vision-fixture");
        let request = CompletionRequest {
            model: None,
            preamble: Some("immutable image policy".into()),
            chat_history: OneOrMany::many(interactions()).unwrap(),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(64),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };
        let send = async {
            match &model {
                AnyModel::Anthropic(model) => model.completion(request).await.map(|_| ()),
                AnyModel::OpenAI(OpenAiModel::Completions(model)) => {
                    model.completion(request).await.map(|_| ())
                }
                AnyModel::OpenAI(OpenAiModel::Responses(model)) => {
                    model.completion(request).await.map(|_| ())
                }
                AnyModel::OpenRouter(model, _) => model.completion(request).await.map(|_| ()),
                AnyModel::Gemini(model) => model.completion(request).await.map(|_| ()),
                AnyModel::Ollama(model) => model.completion(request).await.map(|_| ()),
                AnyModel::Copilot(model) => model.completion(request).await.map(|_| ()),
            }
        };
        // Both futures are owned by this join; no detached fixture task or
        // process survives an assertion or timeout. A 400 ends the request
        // after observing the actual serialized wire body, without a model.
        let receive = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                assert!(bytes.len() < 64 * 1024);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse().unwrap())
                })
                .unwrap();
            assert!(length < 64 * 1024);
            while bytes.len() < end + length {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[end..end + length]).unwrap();
            socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
            body
        });
        let (result, body) = tokio::join!(send, receive);
        assert!(result.is_err());
        let body =
            body.unwrap_or_else(|_| panic!("{protocol}/{style} never reached HTTP: {result:?}"));
        let text = body.to_string();
        assert!(
            text.contains(&base64::engine::general_purpose::STANDARD.encode(PNG)),
            "{protocol}/{style}: {body}"
        );
        assert!(
            text.contains("image-call")
                && text.contains("text-call")
                && text.contains("notes remain text"),
            "{protocol}/{style}: {body}"
        );
        if protocol == "openai" && style == "responses" {
            let input = body["input"].as_array().unwrap();
            let output = input
                .iter()
                .find(|part| {
                    part["type"] == "function_call_output" && part["call_id"] == "image-call"
                })
                .unwrap();
            assert!(
                output["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|part| part["type"] == "input_image")
            );
        } else if protocol == "anthropic" {
            let messages = body["messages"].as_array().unwrap();
            assert!(
                messages
                    .iter()
                    .flat_map(|m| m["content"].as_array().into_iter().flatten())
                    .any(|part| part["type"] == "tool_result"
                        && part["tool_use_id"] == "image-call"
                        && part["content"]
                            .as_array()
                            .is_some_and(|parts| parts.iter().any(|part| part["type"] == "image")))
            );
        } else {
            let messages = body
                .get("messages")
                .or_else(|| body.get("contents"))
                .unwrap()
                .as_array()
                .unwrap();
            let last = messages.last().unwrap().to_string();
            assert!(
                last.contains(&base64::engine::general_purpose::STANDARD.encode(PNG)),
                "{protocol}: {body}"
            );
            assert!(
                !last.contains("notes remain text"),
                "bridge follows all original results: {body}"
            );
        }
    }
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn image_read64_existing_read_deny_and_byte_limit_precede_payload() {
    use crate::agent::tools::{ReadArgs, ReadTool, ReadTracker, read_image::ImageReadSupport};
    use crate::permission::{
        Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm,
        checker::PermissionChecker,
    };
    use rig::tool::Tool;
    let root = super::OwnedTestDirectory::new("image64-deny");
    std::fs::write(root.path().join("image"), PNG).unwrap();
    let mut tracker = ReadTracker::new(true);
    tracker.image_support = ImageReadSupport::captured("anthropic", "claude-fable-5-1", false);
    let configs = PermissionConfigs {
        glob: PermissionConfig {
            read: Some(ToolPerm::Simple(Action::Deny)),
            ..Default::default()
        },
        ..Default::default()
    };
    let checker = std::sync::Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &configs,
            SecurityMode::Standard,
            Some(root.path().to_owned()),
            None,
        )
        .unwrap(),
    ));
    let args = || ReadArgs {
        path: "image".into(),
        offset: None,
        limit: None,
        raw_notebook: false,
    };
    let denied = ReadTool::new_with_tracker(Some(checker), None, None, 100, tracker.clone())
        .with_workspace(root.path())
        .call(args())
        .await
        .unwrap_err()
        .to_string();
    assert!(denied.to_lowercase().contains("denied"), "{denied}");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(root.path().join("image"))
        .unwrap();
    file.set_len(crate::agent::tools::read_image::MAX_IMAGE_BYTES as u64 + 1)
        .unwrap();
    drop(file);
    let error = ReadTool::new_with_tracker(None, None, None, 100, tracker)
        .with_workspace(root.path())
        .call(args())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("5 MiB"), "{error}");
}

#[cfg(feature = "multimodal")]
#[test]
fn image_read64_capability_uses_actual_backend_model_override_semantics() {
    use crate::agent::tools::read_image::ImageFormat;
    // No HTTP is performed: the exact immutable route/model is sufficient.
    for style in ["completions", "responses"] {
        let cfg:crate::config::Config=serde_json::from_value(serde_json::json!({"custom_providers":{"image64-cap":{"provider_type":"openai","base_url":"http://127.0.0.1:1","api_style":style}}})).unwrap();
        let client = crate::provider::create_client(
            "image64-cap",
            Some("unused"),
            &cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        let model = client.completion_model("gpt-3.5-turbo");
        let override_model = serde_json::json!({"model":"gpt-4.1"});
        assert_eq!(
            model
                .image_read_support(Some(&override_model))
                .check(ImageFormat::Png)
                .is_ok(),
            style == "completions"
        );
        let model = client.completion_model("gpt-4.1");
        let override_model = serde_json::json!({"model":"gpt-3.5-turbo"});
        assert_eq!(
            model
                .image_read_support(Some(&override_model))
                .check(ImageFormat::Png)
                .is_ok(),
            style == "responses"
        );
    }
}

#[cfg(not(feature = "multimodal"))]
#[tokio::test]
async fn image_read64_feature_off_retains_data_but_refuses_read_and_dispatch() {
    use rig::tool::Tool;
    let root = super::OwnedTestDirectory::new("image64-off");
    std::fs::write(root.path().join("pixel.png"), PNG).unwrap();
    let error = crate::agent::tools::ReadTool::new(None, None, None, 100)
        .with_workspace(root.path())
        .call(crate::agent::tools::ReadArgs {
            path: "pixel.png".into(),
            offset: None,
            limit: None,
            raw_notebook: false,
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("multimodal"), "{error}");
    let history = interactions();
    assert!(crate::agent::input::history_has_images(&history));
    let model = rig::providers::openrouter::Client::builder()
        .api_key("unused")
        .base_url("http://127.0.0.1:1")
        .http_client(crate::provider_http::ProviderHttpClient::default())
        .build()
        .unwrap();
    use rig::client::CompletionClient;
    let agent = crate::provider::AnyAgent::without_skills(
        crate::provider::AnyAgentInner::OpenRouter(model.agent("fixture").build()),
    );
    let turn = agent
        .run_print(
            "inspect",
            false,
            false,
            &Default::default(),
            history.clone(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.unwrap().to_string().contains("multimodal"));
    assert_eq!(history, interactions());
}

#[test]
fn image_read64_borrowed_batch_limits_precede_payload_decoding() {
    let mut invalid = image();
    invalid.data = rig::message::DocumentSourceKind::Base64("not base64!".into());
    let result = |images: Vec<rig::message::Image>| ToolResult {
        id: "image-call".into(),
        call_id: None,
        content: OneOrMany::many(images.into_iter().map(ToolResultContent::Image)).unwrap(),
    };
    assert!(
        crate::session::tool_image::from_result(&result(vec![invalid.clone(); 9]))
            .unwrap_err()
            .contains("aggregate")
    );
    assert!(
        crate::session::tool_image::from_result(&result(vec![invalid]))
            .unwrap_err()
            .contains("base64")
    );
    let mut large = image();
    large.data = rig::message::DocumentSourceKind::Base64("!".repeat(4 * 1024 * 1024));
    assert!(
        crate::session::tool_image::from_result(&result(vec![large.clone(), large]))
            .unwrap_err()
            .contains("aggregate")
    );
}

#[test]
fn image_read64_hybrid_response_preserves_literal_text_and_object_shape() {
    for (response, expected) in [
        (
            serde_json::json!("literal \"text\"\nline"),
            "literal \"text\"\nline".to_owned(),
        ),
        (serde_json::json!({"value":1}), "{\"value\":1}".to_owned()),
    ] {
        let parts = ToolResultContent::from_tool_output(
            serde_json::json!({"response":response,"parts":[]}).to_string(),
        );
        let ToolResultContent::Text(text) = parts.first() else {
            panic!("missing response text")
        };
        assert_eq!(text.text, expected);
        assert_eq!(parts.len(), 1);
    }
}

#[cfg(feature = "multimodal")]
#[tokio::test]
async fn image_read64_incomplete_and_ambiguous_batches_refuse_before_http() {
    use crate::provider::{AnyModel, OpenAiModel};
    use rig::completion::{CompletionModel, CompletionRequest};
    for case in ["incomplete", "orphan", "alias", "duplicate"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "custom_providers":{"image64-negative":{"provider_type":"openai",
                "base_url":format!("http://{address}"),"api_style":"completions"}}
        }))
        .unwrap();
        let client = crate::provider::create_client(
            "image64-negative",
            Some("unused-owned-fixture"),
            &cfg.custom_providers_map(),
            None,
        )
        .unwrap();
        let AnyModel::OpenAI(OpenAiModel::Completions(model)) =
            client.completion_model("owned-vision-fixture")
        else {
            unreachable!()
        };
        let mut history = interactions();
        if case == "alias" {
            let Message::Assistant { content, .. } = &mut history[0] else {
                unreachable!()
            };
            let AssistantContent::ToolCall(call) = content.iter_mut().next().unwrap() else {
                unreachable!()
            };
            call.call_id = Some("text-call".into());
        } else {
            let Message::User { content } = &mut history[1] else {
                unreachable!()
            };
            let mut parts: Vec<_> = content.iter().cloned().collect();
            match case {
                "incomplete" => {
                    parts.pop();
                }
                "orphan" => {
                    let UserContent::ToolResult(result) = &mut parts[0] else {
                        unreachable!()
                    };
                    result.id = "unadvertised".into();
                    result.call_id = Some("unadvertised".into());
                }
                "duplicate" => parts.push(parts[0].clone()),
                _ => unreachable!(),
            }
            *content = OneOrMany::many(parts).unwrap();
        }
        let request = CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::many(history).unwrap(),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(64),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), model.completion(request))
                .await
                .unwrap_or_else(|_| panic!("{case}: image correlation did not refuse before HTTP"));
        assert!(
            format!("{result:?}").contains("incomplete or ambiguous call correlation"),
            "{case}: {result:?}"
        );
        assert!(
            matches!(listener.into_std().unwrap().accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "{case}: unexpected HTTP connection"
        );
    }
}
