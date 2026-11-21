//! Bedrock Converse / ConverseStream wire layer (mini-agent-nvnkx.44).
//!
//! This is the non-Anthropic Bedrock route: Amazon Nova, Llama, Mistral,
//! DeepSeek and other model ids that are not `anthropic.*` use
//! `POST /model/{modelId}/converse-stream` instead of the native Anthropic
//! Messages path. The module owns the three pure pieces of that route:
//!
//! * [`request_body`] encodes a rig `CompletionRequest` as a Converse body;
//! * [`signed_request`] builds the URL and signs it with the existing SigV4
//!   signer (the same destination checks and credential chain as the
//!   Anthropic route);
//! * [`EventStream`] / [`StreamAccumulator`] decode the
//!   `application/vnd.amazon.eventstream` binary framing and the Converse
//!   events into text, tool-use and usage items.
//!
//! The wire formats follow the AWS Bedrock Runtime API reference. They were
//! exercised against recorded frames only, never a live account. The module is
//! not yet attached to a rig `CompletionModel`, so it has no production
//! caller.
#![allow(dead_code)]

use bytes::Bytes;
use rig::completion::{CompletionRequest, Message};
use rig::message::{AssistantContent, ToolResultContent, UserContent};
use serde_json::{Value, json};

/// Whether a Bedrock model id belongs on the native Anthropic route. Ids may
/// carry a cross-region inference-profile prefix (`us.`, `eu.`, `global.`,
/// ...) before the vendor segment.
pub(crate) fn is_anthropic_model(model: &str) -> bool {
    let model = model.trim();
    model.starts_with("anthropic.")
        || model
            .split_once('.')
            .is_some_and(|(prefix, rest)| prefix.len() <= 6 && rest.starts_with("anthropic."))
        || model.contains(":inference-profile/") && model.contains("anthropic.")
}

/// Percent-encode one path segment (RFC 3986 unreserved stay literal).
fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `/model/{id}/converse` or `/model/{id}/converse-stream`.
pub(crate) fn path(model: &str, streaming: bool) -> anyhow::Result<String> {
    anyhow::ensure!(
        !model.is_empty()
            && model.len() <= 2048
            && model
                .bytes()
                .all(|b| b.is_ascii_graphic() && !matches!(b, b'?' | b'#')),
        "invalid Bedrock model id"
    );
    Ok(format!(
        "/model/{}/{}",
        encode_segment(model),
        if streaming {
            "converse-stream"
        } else {
            "converse"
        }
    ))
}

/// The regional runtime origin the Converse route is pinned to.
pub(crate) fn origin(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

fn tool_result_content(content: &ToolResultContent) -> Value {
    match content {
        ToolResultContent::Text(text) => json!({"text": text.text}),
        _ => json!({"text": "[non-text tool output omitted]"}),
    }
}

/// Encode a rig request as a Converse body.
pub(crate) fn request_body(request: &CompletionRequest) -> Value {
    let mut system: Vec<Value> = request
        .preamble
        .iter()
        .map(|text| json!({"text": text}))
        .collect();
    let mut messages: Vec<Value> = Vec::new();
    let push = |role: &str, blocks: Vec<Value>, messages: &mut Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        // Converse requires strictly alternating roles; merge neighbours.
        if let Some(last) = messages.last_mut()
            && last["role"] == role
        {
            last["content"]
                .as_array_mut()
                .expect("content is an array")
                .extend(blocks);
            return;
        }
        messages.push(json!({"role": role, "content": blocks}));
    };
    for message in request.chat_history.iter() {
        match message {
            Message::System { content } => system.push(json!({"text": content})),
            Message::User { content } => {
                let blocks = content
                    .iter()
                    .filter_map(|part| match part {
                        UserContent::Text(text) if !text.text.is_empty() => {
                            Some(json!({"text": text.text}))
                        }
                        UserContent::ToolResult(result) => Some(json!({"toolResult": {
                            "toolUseId": result.id,
                            "content": result.content.iter().map(tool_result_content).collect::<Vec<_>>(),
                            "status": if result.is_error == Some(true) { "error" } else { "success" },
                        }})),
                        _ => None,
                    })
                    .collect();
                push("user", blocks, &mut messages);
            }
            Message::Assistant { content, .. } => {
                let blocks = content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text(text) if !text.text.is_empty() => {
                            Some(json!({"text": text.text}))
                        }
                        AssistantContent::ToolCall(call) => Some(json!({"toolUse": {
                            "toolUseId": call.id,
                            "name": call.function.name,
                            "input": call.function.arguments,
                        }})),
                        _ => None,
                    })
                    .collect();
                push("assistant", blocks, &mut messages);
            }
        }
    }
    let mut body = json!({"messages": messages});
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    let mut inference = serde_json::Map::new();
    if let Some(max) = request.max_tokens {
        inference.insert("maxTokens".into(), json!(max));
    }
    if let Some(temperature) = request.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    if !inference.is_empty() {
        body["inferenceConfig"] = Value::Object(inference);
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({"toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": {"json": tool.parameters},
                }})
            })
            .collect();
        body["toolConfig"] = json!({"tools": tools});
    }
    body
}

/// Build the signed `converse-stream` request against the pinned regional
/// origin. Signing reuses the Anthropic route's SigV4 signer.
pub(crate) fn signed_request(
    region: &str,
    model: &str,
    body: &Value,
    credentials: aws_credential_types::Credentials,
    now: std::time::SystemTime,
) -> anyhow::Result<http::Request<Bytes>> {
    let uri = format!("{}{}", origin(region), path(model, true)?);
    let mut request = http::Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::ACCEPT, "application/vnd.amazon.eventstream")
        .body(Bytes::from(serde_json::to_vec(body)?))?;
    super::sign_request(&mut request, credentials, region, now)
        .map_err(|_| anyhow::anyhow!("Bedrock Converse request signing failed"))?;
    Ok(request)
}

// ---- event-stream framing -------------------------------------------------

const MAX_FRAME: usize = 16 * 1024 * 1024;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// One decoded event-stream message.
#[derive(Debug, PartialEq)]
pub(crate) struct Frame {
    pub(crate) message_type: String,
    /// `:event-type` for events, `:exception-type` for modelled errors.
    pub(crate) kind: String,
    pub(crate) payload: Vec<u8>,
}

/// Incremental decoder for `application/vnd.amazon.eventstream`.
#[derive(Default)]
pub(crate) struct EventStream {
    buffer: Vec<u8>,
}

impl EventStream {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, `Ok(None)` when more bytes are needed.
    pub(crate) fn next_frame(&mut self) -> anyhow::Result<Option<Frame>> {
        if self.buffer.len() < 12 {
            return Ok(None);
        }
        let be = |at: usize| u32::from_be_bytes(self.buffer[at..at + 4].try_into().unwrap());
        let total = be(0) as usize;
        let headers_len = be(4) as usize;
        anyhow::ensure!(
            (16..=MAX_FRAME).contains(&total) && headers_len <= total - 16,
            "malformed event-stream frame length"
        );
        anyhow::ensure!(
            crc32(&self.buffer[..8]) == be(8),
            "event-stream prelude checksum mismatch"
        );
        if self.buffer.len() < total {
            return Ok(None);
        }
        anyhow::ensure!(
            crc32(&self.buffer[..total - 4]) == be(total - 4),
            "event-stream message checksum mismatch"
        );
        let headers = &self.buffer[12..12 + headers_len];
        let payload = self.buffer[12 + headers_len..total - 4].to_vec();
        let (mut message_type, mut kind) = (String::new(), String::new());
        let mut at = 0;
        while at < headers.len() {
            let name_len = *headers.get(at).ok_or_else(truncated)? as usize;
            let name = headers
                .get(at + 1..at + 1 + name_len)
                .ok_or_else(truncated)?;
            at += 1 + name_len;
            let value_type = *headers.get(at).ok_or_else(truncated)?;
            at += 1;
            let size = match value_type {
                0 | 1 => 0,
                2 => 1,
                3 => 2,
                4 => 4,
                5 | 8 => 8,
                9 => 16,
                6 | 7 => {
                    let len = headers.get(at..at + 2).ok_or_else(truncated)?;
                    at += 2;
                    u16::from_be_bytes([len[0], len[1]]) as usize
                }
                _ => anyhow::bail!("unknown event-stream header type"),
            };
            let value = headers.get(at..at + size).ok_or_else(truncated)?;
            at += size;
            if value_type == 7 {
                let text = String::from_utf8_lossy(value).into_owned();
                match name {
                    b":message-type" => message_type = text,
                    b":event-type" | b":exception-type" => kind = text,
                    _ => {}
                }
            }
        }
        self.buffer.drain(..total);
        Ok(Some(Frame {
            message_type,
            kind,
            payload,
        }))
    }
}

fn truncated() -> anyhow::Error {
    anyhow::anyhow!("truncated event-stream header")
}

// ---- Converse events ------------------------------------------------------

/// What the stream produced, in order.
#[derive(Debug, PartialEq)]
pub(crate) enum Item {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    Stop(String),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
}

/// Folds Converse stream events into [`Item`]s, assembling each tool call's
/// JSON arguments from its partial fragments.
#[derive(Default)]
pub(crate) struct StreamAccumulator {
    tool: Option<(String, String, String)>,
}

impl StreamAccumulator {
    pub(crate) fn frame(&mut self, frame: &Frame) -> anyhow::Result<Vec<Item>> {
        if frame.message_type == "exception" {
            let message = serde_json::from_slice::<Value>(&frame.payload)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_owned))
                .unwrap_or_default();
            anyhow::bail!("Bedrock {}: {}", frame.kind, message);
        }
        let event: Value = serde_json::from_slice(&frame.payload)?;
        let mut items = Vec::new();
        match frame.kind.as_str() {
            "contentBlockStart" => {
                if let Some(tool) = event["start"].get("toolUse") {
                    self.tool = Some((
                        tool["toolUseId"].as_str().unwrap_or_default().into(),
                        tool["name"].as_str().unwrap_or_default().into(),
                        String::new(),
                    ));
                }
            }
            "contentBlockDelta" => {
                let delta = &event["delta"];
                if let Some(text) = delta["text"].as_str() {
                    items.push(Item::Text(text.to_owned()));
                } else if let Some(fragment) = delta["toolUse"]["input"].as_str()
                    && let Some((_, _, input)) = &mut self.tool
                {
                    input.push_str(fragment);
                }
            }
            "contentBlockStop" => {
                if let Some((id, name, input)) = self.tool.take() {
                    let arguments = if input.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&input)?
                    };
                    items.push(Item::ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            "messageStop" => items.push(Item::Stop(
                event["stopReason"].as_str().unwrap_or_default().to_owned(),
            )),
            "metadata" => items.push(Item::Usage {
                input_tokens: event["usage"]["inputTokens"].as_u64().unwrap_or(0),
                output_tokens: event["usage"]["outputTokens"].as_u64().unwrap_or(0),
            }),
            _ => {}
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> Vec<u8> {
        let mut out = vec![name.len() as u8];
        out.extend_from_slice(name.as_bytes());
        out.push(7);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn frame(message_type: &str, kind: &str, payload: &Value) -> Vec<u8> {
        let mut headers = header(":message-type", message_type);
        headers.extend(header(
            if message_type == "exception" {
                ":exception-type"
            } else {
                ":event-type"
            },
            kind,
        ));
        headers.extend(header(":content-type", "application/json"));
        let payload = serde_json::to_vec(payload).unwrap();
        let total = (16 + headers.len() + payload.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&crc32(&out).to_be_bytes());
        out.extend(headers);
        out.extend(payload);
        out.extend_from_slice(&crc32(&out).to_be_bytes());
        out
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn model_routing_and_paths() {
        for anthropic in [
            "anthropic.claude-sonnet-5-v1:0",
            "us.anthropic.claude-sonnet-5-v1:0",
            "global.anthropic.claude-opus-5-v1:0",
        ] {
            assert!(is_anthropic_model(anthropic), "{anthropic}");
        }
        for other in [
            "amazon.nova-pro-v1:0",
            "us.amazon.nova-lite-v1:0",
            "meta.llama3-1-70b-instruct-v1:0",
            "mistral.mistral-large-2407-v1:0",
            "deepseek.r1-v1:0",
        ] {
            assert!(!is_anthropic_model(other), "{other}");
        }
        assert_eq!(
            path("amazon.nova-pro-v1:0", true).unwrap(),
            "/model/amazon.nova-pro-v1%3A0/converse-stream"
        );
        assert!(path("a/../b?x", true).is_err());
        assert!(path("", false).is_err());
    }

    #[test]
    fn decodes_text_and_tool_use_across_split_chunks() {
        let mut wire = Vec::new();
        wire.extend(frame(
            "event",
            "messageStart",
            &json!({"role": "assistant"}),
        ));
        wire.extend(frame(
            "event",
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "Hel"}}),
        ));
        wire.extend(frame(
            "event",
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "lo"}}),
        ));
        wire.extend(frame(
            "event",
            "contentBlockStop",
            &json!({"contentBlockIndex": 0}),
        ));
        wire.extend(frame(
            "event",
            "contentBlockStart",
            &json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "t1", "name": "read"}}}),
        ));
        for fragment in ["{\"path\":", "\"a.rs\"}"] {
            wire.extend(frame(
                "event",
                "contentBlockDelta",
                &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": fragment}}}),
            ));
        }
        wire.extend(frame(
            "event",
            "contentBlockStop",
            &json!({"contentBlockIndex": 1}),
        ));
        wire.extend(frame(
            "event",
            "messageStop",
            &json!({"stopReason": "tool_use"}),
        ));
        wire.extend(frame(
            "event",
            "metadata",
            &json!({"usage": {"inputTokens": 12, "outputTokens": 5, "totalTokens": 17}}),
        ));
        let mut stream = EventStream::default();
        let mut acc = StreamAccumulator::default();
        let mut items = Vec::new();
        for chunk in wire.chunks(7) {
            stream.push(chunk);
            while let Some(frame) = stream.next_frame().unwrap() {
                items.extend(acc.frame(&frame).unwrap());
            }
        }
        assert_eq!(
            items,
            vec![
                Item::Text("Hel".into()),
                Item::Text("lo".into()),
                Item::ToolCall {
                    id: "t1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "a.rs"}),
                },
                Item::Stop("tool_use".into()),
                Item::Usage {
                    input_tokens: 12,
                    output_tokens: 5
                },
            ]
        );
    }

    #[test]
    fn corrupt_frames_and_modelled_exceptions_are_errors() {
        let mut wire = frame("event", "messageStart", &json!({}));
        let last = wire.len() - 1;
        wire[last] ^= 0xFF;
        let mut stream = EventStream::default();
        stream.push(&wire);
        assert!(stream.next_frame().is_err());

        let mut stream = EventStream::default();
        stream.push(&frame(
            "exception",
            "throttlingException",
            &json!({"message": "slow down"}),
        ));
        let frame = stream.next_frame().unwrap().unwrap();
        let error = StreamAccumulator::default().frame(&frame).unwrap_err();
        assert!(error.to_string().contains("throttlingException: slow down"));
    }

    #[test]
    fn request_body_maps_system_tools_and_tool_results() {
        use rig::OneOrMany;
        use rig::completion::ToolDefinition;
        let request = CompletionRequest {
            model: None,
            preamble: Some("be brief".into()),
            chat_history: OneOrMany::many(vec![
                Message::user("hi"),
                Message::Assistant {
                    id: None,
                    content: OneOrMany::many(vec![
                        AssistantContent::text("ok"),
                        AssistantContent::tool_call("t1", "read", json!({"path": "a"})),
                    ])
                    .unwrap(),
                },
                Message::User {
                    content: OneOrMany::one(UserContent::tool_result(
                        "t1",
                        OneOrMany::one(ToolResultContent::text("file body")),
                    )),
                },
            ])
            .unwrap(),
            documents: vec![],
            tools: vec![std::sync::Arc::new(ToolDefinition {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type": "object"}),
            })],
            temperature: Some(0.2),
            max_tokens: Some(512),
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };
        let body = request_body(&request);
        assert_eq!(body["system"], json!([{"text": "be brief"}]));
        assert_eq!(
            body["inferenceConfig"],
            json!({"maxTokens": 512, "temperature": 0.2})
        );
        assert_eq!(
            body["messages"][0],
            json!({"role": "user", "content": [{"text": "hi"}]})
        );
        assert_eq!(
            body["messages"][1]["content"][1],
            json!({"toolUse": {"toolUseId": "t1", "name": "read", "input": {"path": "a"}}})
        );
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"],
            json!({"toolUseId": "t1", "content": [{"text": "file body"}], "status": "success"})
        );
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "read");
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"],
            json!({"json": {"type": "object"}})
        );
    }

    #[test]
    fn the_converse_request_is_signed_for_the_pinned_regional_origin() {
        let credentials = aws_credential_types::Credentials::new(
            "AKIDEXAMPLE",
            "secret",
            Some("session".into()),
            None,
            "test",
        );
        let request = signed_request(
            "us-east-1",
            "amazon.nova-pro-v1:0",
            &json!({"messages": []}),
            credentials,
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        )
        .unwrap();
        assert_eq!(request.method(), http::Method::POST);
        assert_eq!(
            request.uri().to_string(),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/amazon.nova-pro-v1%3A0/converse-stream"
        );
        let auth = request.headers()["authorization"].to_str().unwrap();
        assert!(
            auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-east-1/bedrock/aws4_request"
            ),
            "{auth}"
        );
        assert!(request.headers().contains_key("x-amz-date"));
        assert_eq!(request.headers()["x-amz-security-token"], "session");
        assert!(request.headers().contains_key("x-amz-content-sha256"));
    }
}
