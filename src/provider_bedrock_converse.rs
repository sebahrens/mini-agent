//! Bedrock Converse / ConverseStream wire layer (mini-agent-nvnkx.44).
//!
//! This is the non-Anthropic Bedrock route: Amazon Nova, Llama, Mistral,
//! DeepSeek and other model ids that are not `anthropic.*` use
//! `POST /model/{modelId}/converse-stream` instead of the native Anthropic
//! Messages path. The module owns the three pure pieces of that route:
//!
//! * [`request_body`] encodes a rig `CompletionRequest` as a Converse body;
//! * [`build_request`] builds the URL; the Bedrock auth owner signs it with the
//!   existing SigV4 signer (the same destination checks and credential chain
//!   as the Anthropic route);
//! * [`EventStream`] / [`StreamAccumulator`] decode the
//!   `application/vnd.amazon.eventstream` binary framing and the Converse
//!   events into text, tool-use and usage items.
//!
//! The wire formats follow the AWS Bedrock Runtime API reference. They were
//! exercised against recorded frames and loopback servers only, never a live
//! account. [`ConverseModel`] adapts the layer to rig's `CompletionModel`; the
//! request is signed (SigV4) or given the Bedrock API key by the Bedrock auth
//! owner inside `ProviderHttpClient`, with the same destination pinning as the
//! Anthropic route.
use crate::provider_http::ProviderHttpClient;
use bytes::Bytes;
use futures::StreamExt;
use rig::OneOrMany;
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, GetTokenUsage,
    Message, StopReason, Usage,
};
use rig::http_client::HttpClientExt;
use rig::message::{AssistantContent, ToolResultContent, UserContent};
use rig::streaming::{RawStreamingChoice, RawStreamingToolCall, StreamingCompletionResponse};
use serde::{Deserialize, Serialize};
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

/// Build the unsigned Converse request under `base` (the pinned regional
/// origin, or a gateway prefix). Signing / the API key is applied at the HTTP
/// owner, which pins the destination before attaching any credential.
pub(crate) fn build_request(
    base: &str,
    model: &str,
    body: &Value,
    streaming: bool,
) -> anyhow::Result<http::Request<Bytes>> {
    let uri = format!("{}{}", base.trim_end_matches('/'), path(model, streaming)?);
    Ok(http::Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::ACCEPT,
            if streaming {
                "application/vnd.amazon.eventstream"
            } else {
                "application/json"
            },
        )
        .body(Bytes::from(serde_json::to_vec(body)?))?)
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
        cache_read: u64,
        cache_write: u64,
    },
}

fn usage_item(usage: &Value) -> Item {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    Item::Usage {
        input_tokens: count("inputTokens"),
        output_tokens: count("outputTokens"),
        cache_read: count("cacheReadInputTokens"),
        cache_write: count("cacheWriteInputTokens"),
    }
}

/// Decode a non-streaming `converse` response into the same [`Item`]s the
/// stream produces (text and tool calls in order, then stop reason, usage).
pub(crate) fn parse_response(response: &Value) -> Vec<Item> {
    let mut items = Vec::new();
    for block in response["output"]["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(text) = block["text"].as_str() {
            if !text.is_empty() {
                items.push(Item::Text(text.to_owned()));
            }
        } else if let Some(tool) = block.get("toolUse") {
            items.push(Item::ToolCall {
                id: tool["toolUseId"].as_str().unwrap_or_default().into(),
                name: tool["name"].as_str().unwrap_or_default().into(),
                arguments: tool.get("input").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }
    items.push(Item::Stop(
        response["stopReason"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    ));
    items.push(usage_item(&response["usage"]));
    items
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "model_context_window_exceeded" => StopReason::ContextWindow,
        "guardrail_intervened" | "content_filtered" => StopReason::ContentFilter,
        _ => StopReason::Other,
    }
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
            "metadata" => items.push(usage_item(&event["usage"])),
            _ => {}
        }
        Ok(items)
    }
}

// ---- rig CompletionModel --------------------------------------------------

/// What a streamed Converse response ends with: stop reason and usage.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConverseFinal {
    pub stop_reason: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl ConverseFinal {
    fn absorb(&mut self, item: &Item) {
        match item {
            Item::Stop(reason) => self.stop_reason.clone_from(reason),
            Item::Usage {
                input_tokens,
                output_tokens,
                cache_read,
                cache_write,
            } => {
                self.input_tokens = *input_tokens;
                self.output_tokens = *output_tokens;
                self.cache_read = *cache_read;
                self.cache_write = *cache_write;
            }
            _ => {}
        }
    }
}

impl GetTokenUsage for ConverseFinal {
    fn token_usage(&self) -> Usage {
        let mut usage = Usage::new();
        usage.input_tokens = self.input_tokens;
        usage.output_tokens = self.output_tokens;
        usage.cached_input_tokens = self.cache_read;
        usage.cache_creation_input_tokens = self.cache_write;
        usage.total_tokens =
            self.input_tokens + self.output_tokens + self.cache_read + self.cache_write;
        usage
    }
    fn output_truncated(&self) -> bool {
        self.stop_reason == "max_tokens"
    }
    fn stop_reason(&self) -> Option<StopReason> {
        (!self.stop_reason.is_empty()).then(|| stop_reason(&self.stop_reason))
    }
}

/// Handle used by rig's `make`: the authenticated transport and the Converse
/// base URL (the endpoint without its `/anthropic` suffix).
#[derive(Clone)]
pub struct ConverseClient {
    pub(crate) http: ProviderHttpClient,
    pub(crate) base: String,
}

/// Bedrock Converse / ConverseStream completion model for every non-Anthropic
/// model id. Requests go through the Bedrock-authenticated `ProviderHttpClient`,
/// which pins the destination and signs (SigV4) or attaches the Bedrock API key.
#[derive(Clone)]
pub struct ConverseModel {
    pub model: String,
    client: ConverseClient,
}

impl ConverseModel {
    pub(crate) fn new(http: ProviderHttpClient, base: String, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            client: ConverseClient { http, base },
        }
    }

    fn request(
        &self,
        request: &CompletionRequest,
        streaming: bool,
    ) -> Result<http::Request<Bytes>, CompletionError> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        build_request(&self.client.base, model, &request_body(request), streaming)
            .map_err(|error| CompletionError::RequestError(error.into()))
    }
}

struct StreamState {
    bytes: rig::http_client::sse::BoxedStream,
    frames: EventStream,
    accumulator: StreamAccumulator,
    queue: std::collections::VecDeque<RawStreamingChoice<ConverseFinal>>,
    last: ConverseFinal,
    ended: bool,
}

impl StreamState {
    fn fail(&mut self, message: String) -> CompletionError {
        self.ended = true;
        CompletionError::ResponseError(message)
    }

    /// The next queued stream choice, reading and decoding more of the wire as
    /// needed. `None` at the end of the stream.
    async fn next(&mut self) -> Option<Result<RawStreamingChoice<ConverseFinal>, CompletionError>> {
        loop {
            if let Some(choice) = self.queue.pop_front() {
                return Some(Ok(choice));
            }
            if self.ended {
                return None;
            }
            match self.frames.next_frame() {
                Err(error) => return Some(Err(self.fail(error.to_string()))),
                Ok(Some(frame)) => {
                    match self.accumulator.frame(&frame) {
                        Err(error) => return Some(Err(self.fail(error.to_string()))),
                        Ok(items) => {
                            for item in items {
                                self.last.absorb(&item);
                                match item {
                                    Item::Text(text) => {
                                        self.queue.push_back(RawStreamingChoice::Message(text));
                                    }
                                    Item::ToolCall {
                                        id,
                                        name,
                                        arguments,
                                    } => self.queue.push_back(RawStreamingChoice::ToolCall(
                                        RawStreamingToolCall::new(id, name, arguments),
                                    )),
                                    Item::Stop(_) | Item::Usage { .. } => {}
                                }
                            }
                        }
                    }
                    continue;
                }
                Ok(None) => {}
            }
            match self.bytes.next().await {
                Some(Ok(bytes)) => self.frames.push(&bytes),
                Some(Err(error)) => {
                    self.ended = true;
                    return Some(Err(CompletionError::HttpError(error)));
                }
                None if self.last.stop_reason.is_empty() => {
                    return Some(Err(
                        self.fail("Bedrock stream ended before messageStop".into())
                    ));
                }
                None => {
                    self.ended = true;
                    self.queue
                        .push_back(RawStreamingChoice::FinalResponse(self.last.clone()));
                }
            }
        }
    }
}

impl CompletionModel for ConverseModel {
    type Response = Value;
    type StreamingResponse = ConverseFinal;
    type Client = ConverseClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            client: client.clone(),
        }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        let http_request = self.request(&request, false)?;
        let response = self.client.http.send::<_, Bytes>(http_request).await?;
        let bytes = response.into_body().await?;
        let raw: Value = serde_json::from_slice(&bytes)?;
        let mut last = ConverseFinal::default();
        let mut choice = Vec::new();
        for item in parse_response(&raw) {
            last.absorb(&item);
            match item {
                Item::Text(text) => choice.push(AssistantContent::text(text)),
                Item::ToolCall {
                    id,
                    name,
                    arguments,
                } => choice.push(AssistantContent::tool_call(id, name, arguments)),
                Item::Stop(_) | Item::Usage { .. } => {}
            }
        }
        let choice = OneOrMany::many(choice).map_err(|_| {
            CompletionError::ResponseError("Bedrock Converse response had no content".into())
        })?;
        Ok(CompletionResponse {
            choice,
            usage: last.token_usage(),
            raw_response: raw,
            message_id: None,
        })
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        let http_request = self.request(&request, true)?;
        let response = self.client.http.send_streaming(http_request).await?;
        let state = StreamState {
            bytes: response.into_body(),
            frames: EventStream::default(),
            accumulator: StreamAccumulator::default(),
            queue: Default::default(),
            last: ConverseFinal::default(),
            ended: false,
        };
        let stream = futures::stream::unfold(state, |mut state| async move {
            state.next().await.map(|item| (item, state))
        });
        Ok(StreamingCompletionResponse::stream(Box::pin(stream)))
    }
}

#[cfg(test)]
fn header(name: &str, value: &str) -> Vec<u8> {
    let mut out = vec![name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out.push(7);
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

#[cfg(test)]
pub(crate) fn test_frame(message_type: &str, kind: &str, payload: &Value) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_frame as frame;

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
                    output_tokens: 5,
                    cache_read: 0,
                    cache_write: 0,
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
    fn the_converse_request_targets_the_base_and_stream_or_buffered_path() {
        let body = json!({"messages": []});
        let streamed = build_request(
            "https://bedrock-runtime.us-east-1.amazonaws.com/",
            "amazon.nova-pro-v1:0",
            &body,
            true,
        )
        .unwrap();
        assert_eq!(streamed.method(), http::Method::POST);
        assert_eq!(
            streamed.uri().to_string(),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/amazon.nova-pro-v1%3A0/converse-stream"
        );
        assert_eq!(
            streamed.headers()["accept"],
            "application/vnd.amazon.eventstream"
        );
        let buffered = build_request("https://gw.example/aws", "m", &body, false).unwrap();
        assert_eq!(
            buffered.uri().to_string(),
            "https://gw.example/aws/model/m/converse"
        );
        assert!(buffered.headers().get("authorization").is_none());
    }

    #[test]
    fn non_streaming_response_decodes_text_tool_use_and_cache_usage() {
        let items = parse_response(&json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "ok"},
                {"toolUse": {"toolUseId": "t9", "name": "read", "input": {"path": "x"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 3, "outputTokens": 4, "cacheReadInputTokens": 2}
        }));
        assert_eq!(
            items,
            vec![
                Item::Text("ok".into()),
                Item::ToolCall {
                    id: "t9".into(),
                    name: "read".into(),
                    arguments: json!({"path": "x"}),
                },
                Item::Stop("tool_use".into()),
                Item::Usage {
                    input_tokens: 3,
                    output_tokens: 4,
                    cache_read: 2,
                    cache_write: 0,
                },
            ]
        );
        let mut last = ConverseFinal::default();
        for item in &items {
            last.absorb(item);
        }
        let usage = last.token_usage();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.total_tokens
            ),
            (3, 2, 9)
        );
        assert_eq!(last.stop_reason(), Some(StopReason::ToolUse));
        assert!(!last.output_truncated());
    }
}
