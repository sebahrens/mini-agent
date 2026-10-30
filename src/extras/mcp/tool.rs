use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use compact_str::CompactString;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use rmcp::model::{CallToolRequestParams, ContentBlock, JsonObject};
use rmcp::service::{Peer, RoleClient, ServiceError};

use crate::agent::tools::check_mcp_perm;
use crate::extras::mcp::client::call_tool_bounded;
use crate::extras::mcp::config::TrustedMcpServer;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub(super) const MCP_TOOL_DESCRIPTION_MAX_BYTES: usize = 4 * 1024;
pub(super) const MCP_TOOL_SCHEMA_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub struct McpToolError(pub CompactString);

impl fmt::Display for McpToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for McpToolError {}

pub struct McpTool {
    pub server_name: CompactString,
    pub trusted_identity: Option<TrustedMcpServer>,
    pub(crate) source_proof: Option<std::sync::Arc<super::project::ServerSourceProof>>,
    pub(crate) client_is_live: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    pub(crate) elicitation: super::elicitation::Handler,
    pub definition: rmcp::model::Tool,
    pub(super) model_description: Arc<str>,
    pub(super) model_parameters: Arc<serde_json::Value>,
    pub peer: Peer<RoleClient>,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    /// Name registered with the model. Equals `definition.name` unless another
    /// server exposes the same tool name, in which case the manager namespaces
    /// it as `<server>__<tool>`. The wire-level call and the permission key
    /// always use the bare `definition.name`.
    pub registered_name: CompactString,
    /// Bound on one `tools/call` round trip.
    pub call_timeout: Duration,
    /// Opaque private-storage owner for oversized MCP results.
    pub(super) spill_scope: CompactString,
}

impl McpTool {
    /// Candidate alias for a duplicate or provider-incompatible tool name.
    pub fn namespaced_name(server_name: &str, tool_name: &str) -> CompactString {
        CompactString::new(format!("{server_name}__{tool_name}"))
    }

    pub(super) fn bounded_model_metadata(
        definition: &rmcp::model::Tool,
    ) -> Result<(String, serde_json::Value, bool), &'static str> {
        let description = definition.description.as_deref().unwrap_or("");
        let (description, truncated) = truncate_utf8_bytes(
            description,
            MCP_TOOL_DESCRIPTION_MAX_BYTES,
            "\n[description truncated]",
        );
        // One serialization for the size bound; the model value is the same
        // object, so it is cloned rather than serialized a second time.
        if serde_json::to_vec(&*definition.input_schema)
            .map_err(|_| "input schema could not be serialized")?
            .len()
            > MCP_TOOL_SCHEMA_MAX_BYTES
        {
            return Err("input schema exceeds 16 KiB");
        }
        let schema = serde_json::Value::Object((*definition.input_schema).clone());
        Ok((description, schema, truncated))
    }
}

fn truncate_utf8_bytes(text: &str, max_bytes: usize, marker: &str) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let prefix_budget = max_bytes.saturating_sub(marker.len());
    let mut end = prefix_budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = text[..end].to_string();
    bounded.push_str(marker);
    (bounded, true)
}

fn parse_arguments(args: &str) -> Result<Option<JsonObject>, McpToolError> {
    serde_json::from_str(args).map_err(|error| {
        McpToolError(CompactString::new(format!(
            "invalid MCP tool arguments: {error}"
        )))
    })
}

fn bounded_mcp_output(output: &str, spill_scope: &str, tool_name: &str) -> String {
    bounded_mcp_output_after(String::new(), output, spill_scope, tool_name)
}

/// `bounded_mcp_output` rendered after `head` (trusted host text that stays
/// outside the fence), without copying the fenced result a second time.
fn bounded_mcp_output_after(
    head: String,
    output: &str,
    spill_scope: &str,
    tool_name: &str,
) -> String {
    bounded_mcp_output_with_after(head, output, |full_output| {
        crate::session::storage::save_tool_output(spill_scope, tool_name, full_output)
    })
}

#[cfg(test)]
fn bounded_mcp_output_with(
    output: &str,
    save: impl FnOnce(&str) -> anyhow::Result<PathBuf>,
) -> String {
    bounded_mcp_output_with_after(String::new(), output, save)
}

fn bounded_mcp_output_with_after(
    head: String,
    output: &str,
    save: impl FnOnce(&str) -> anyhow::Result<PathBuf>,
) -> String {
    // Bytes bound characters from above, so only a long result is counted.
    if output.len() <= crate::session::TOOL_RESULT_SAVE_THRESHOLD {
        return delimit_mcp_body_after(head, output, None);
    }
    let output_chars = output.chars().count();
    if output_chars <= crate::session::TOOL_RESULT_SAVE_THRESHOLD {
        return delimit_mcp_body_after(head, output, None);
    }

    match save(output) {
        Ok(path) => {
            let (body, omitted) = truncated_mcp_body(output, output_chars);
            delimit_mcp_body_after(
                head,
                &body,
                Some(format!(
                    "[mcp host: full output saved to: {}; {output_chars} characters; {omitted} omitted]",
                    path.display()
                )),
            )
        }
        Err(error) => {
            tracing::debug!(%error, "failed to spill oversized MCP tool result");
            format_unsaved_mcp_output(head, output, output_chars, &error.to_string())
        }
    }
}

fn truncated_mcp_body(output: &str, output_chars: usize) -> (String, usize) {
    let head: String = output
        .chars()
        .take(crate::session::TOOL_RESULT_HEAD_CHARS)
        .collect();
    let tail_start = output_chars.saturating_sub(crate::session::TOOL_RESULT_TAIL_CHARS);
    let tail: String = output.chars().skip(tail_start).collect();
    let omitted = output_chars.saturating_sub(
        crate::session::TOOL_RESULT_HEAD_CHARS + crate::session::TOOL_RESULT_TAIL_CHARS,
    );
    (format!("{head}\n…\n{tail}"), omitted)
}

fn delimit_mcp_body_after(head: String, body: &str, host_notice: Option<String>) -> String {
    let mut rendered = head;
    // Each line gains a two-byte prefix; reserve for the common line length so
    // a large body is not regrown repeatedly.
    rendered
        .reserve(body.len() + body.len() / 16 + 64 + host_notice.as_ref().map_or(0, String::len));
    rendered.push_str("[mcp output begins]\n");
    for line in body.lines() {
        rendered.push_str("> ");
        rendered.push_str(line);
        rendered.push('\n');
    }
    if body.is_empty() {
        rendered.push_str("> \n");
    }
    rendered.push_str("[mcp output ends]");
    if let Some(notice) = host_notice {
        rendered.push('\n');
        rendered.push_str(&notice);
    }
    rendered
}

fn format_unsaved_mcp_output(
    head: String,
    output: &str,
    output_chars: usize,
    error: &str,
) -> String {
    let (body, omitted) = truncated_mcp_body(output, output_chars);
    let diagnostic = error
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(300)
        .collect::<String>();

    delimit_mcp_body_after(
        head,
        &body,
        Some(format!(
            "[mcp host: output could not be saved; {output_chars} characters; {omitted} omitted; re-run with a narrower request: {diagnostic}]"
        )),
    )
}

impl ToolDyn for McpTool {
    fn name(&self) -> String {
        self.registered_name.to_string()
    }

    fn description(&self) -> String {
        self.model_description.to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        (*self.model_parameters).clone()
    }

    fn call(&self, args: String) -> WasmBoxedFuture<'_, Result<String, ToolError>> {
        let server_name = self.server_name.clone();
        let trusted_identity = self.trusted_identity;
        let tool_name = self.definition.name.to_string();
        let peer = self.peer.clone();
        let permission = self.permission.clone();
        let ask_tx = self.ask_tx.clone();
        let call_timeout = self.call_timeout;
        let spill_scope = self.spill_scope.clone();
        let source_proof = self.source_proof.clone();
        let client_is_live = self.client_is_live.clone();
        let elicitation = self.elicitation.clone();

        Box::pin(async move {
            ensure_client_current(source_proof.as_deref(), client_is_live.as_ref())?;
            // Set when any wait between the two source checks actually
            // suspended; only then can the approved source have outlived its
            // revision unnoticed. Never carried across calls.
            let suspended = AtomicBool::new(false);
            let perm_key = format!("mcp_tool:{server_name}:{tool_name}");
            let coaching = track_suspension(
                &suspended,
                check_mcp_perm(
                    &permission,
                    &ask_tx,
                    &perm_key,
                    trusted_identity,
                    &tool_name,
                ),
            )
            .await
            .map_err(|e| {
                ToolError::ToolCallError(Box::new(McpToolError(CompactString::new(e.to_string()))))
            })?;

            let arguments = parse_arguments(&args)
                .map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
            let params = arguments
                .map(|a| CallToolRequestParams::new(tool_name.clone()).with_arguments(a))
                .unwrap_or_else(|| CallToolRequestParams::new(tool_name.clone()));

            // Hook/permission waits may also outlive this run's financial or
            // clarification admission. Financial preparation can suspend, so
            // source/client and question admission follow it at actual RPC.
            track_suspension(&suspended, crate::spend::run::admit_current_effect())
                .await
                .map_err(|error| {
                    ToolError::ToolCallError(Box::new(McpToolError(error.to_string().into())))
                })?;
            if let Some(required) = crate::question::required() {
                return Err(ToolError::ToolCallError(Box::new(required)));
            }
            // Permission/hook waits may outlive the approved source revision.
            // Recheck immediately before the exact retained peer receives RPC.
            // When every wait above completed without suspending, the first
            // check is still the same synchronous step; the cheap liveness
            // check always repeats and the source check repeats on suspension.
            if suspended.load(Ordering::Relaxed) {
                ensure_client_current(source_proof.as_deref(), client_is_live.as_ref())?;
            } else if !client_is_live() {
                return Err(client_retired_error());
            }
            let proof = source_proof.clone();
            let live = client_is_live.clone();
            let _elicitation_lease = elicitation.enter(
                server_name.to_string(),
                ask_tx.clone(),
                std::sync::Arc::new(move || {
                    live() && proof.as_ref().is_none_or(|proof| proof.is_current())
                }),
                None,
            );
            let result = call_tool_bounded(&peer, params, call_timeout)
                .await
                .map_err(|e| {
                    let message = match e {
                        ServiceError::Timeout { .. } => format!(
                            "MCP tool '{tool_name}' on server '{server_name}' timed out after {} ms; \
                             the server may be hung or the request too large. Retry with a \
                             narrower request, or raise `mcp_tool_timeout_secs` in the config.",
                            call_timeout.as_millis()
                        ),
                        other => format!("MCP tool error: {other}"),
                    };
                    ToolError::ToolCallError(Box::new(McpToolError(CompactString::new(message))))
                })?;

            if result.is_error.unwrap_or(false) {
                let mut error_msg = String::new();
                for text in result.content.iter().filter_map(|content| match content {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                }) {
                    if !error_msg.is_empty() {
                        error_msg.push('\n');
                    }
                    error_msg.push_str(text);
                }
                // A failing server may describe the failure only in
                // structuredContent; without it the model sees a bare
                // "returned an error" and cannot act on the reason.
                if let Some(structured) = &result.structured_content
                    && let Some(rendered) = render_structured_content(structured)
                    && rendered != error_msg
                {
                    if !error_msg.is_empty() {
                        error_msg.push('\n');
                    }
                    error_msg.push_str(&rendered);
                }
                let msg = if error_msg.is_empty() {
                    "MCP tool returned an error".to_string()
                } else {
                    bounded_mcp_output(
                        &error_msg,
                        &spill_scope,
                        &format!("mcp:{server_name}:{tool_name}:error"),
                    )
                };
                return Err(ToolError::ToolCallError(Box::new(McpToolError(
                    CompactString::new(msg),
                ))));
            }

            // Host coaching is trusted host text: it stays outside the fence
            // that marks untrusted server output, and is written first so the
            // fenced result is rendered once.
            let head = coaching.map_or_else(String::new, |mut msg| {
                msg.push_str("\n\n");
                msg
            });
            Ok(render_success_content(
                result.content,
                result.structured_content.as_ref(),
                |response| {
                    bounded_mcp_output_after(
                        head,
                        response,
                        &spill_scope,
                        &format!("mcp:{server_name}:{tool_name}"),
                    )
                },
            ))
        })
    }
}

fn ensure_client_current(
    proof: Option<&super::project::ServerSourceProof>,
    client_is_live: &(dyn Fn() -> bool + Send + Sync),
) -> Result<(), ToolError> {
    if !client_is_live() {
        return Err(client_retired_error());
    }
    ensure_source_current(proof)
}

fn client_retired_error() -> ToolError {
    ToolError::ToolCallError(Box::new(McpToolError(
        "MCP connection retired; reconnect before calling tools".into(),
    )))
}

/// Await `future`, recording in `suspended` whether it ever returned pending.
async fn track_suspension<F: std::future::Future>(suspended: &AtomicBool, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        let polled = future.as_mut().poll(cx);
        if polled.is_pending() {
            suspended.store(true, Ordering::Relaxed);
        }
        polled
    })
    .await
}

fn ensure_source_current(
    proof: Option<&super::project::ServerSourceProof>,
) -> Result<(), ToolError> {
    if proof.is_some_and(|proof| !proof.is_current()) {
        Err(ToolError::ToolCallError(Box::new(McpToolError(
            "MCP configuration source changed; reconnect before calling tools".into(),
        ))))
    } else {
        Ok(())
    }
}

/// Render a successful result's content blocks for the model.
///
/// Blocks are separated by newlines so adjacent text blocks never run
/// together. Supported images use the same envelope as the native read tool.
/// Bound and fence only the text before serializing that envelope: spilling or
/// truncating its base64 would make Rig fall back to text and lose the images.
fn render_success_content(
    blocks: Vec<ContentBlock>,
    structured: Option<&serde_json::Value>,
    bound_response: impl FnOnce(&str) -> String,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    let mut encoded_bytes = 0usize;
    let mut unsupported = 0_usize;
    for item in blocks {
        match item {
            ContentBlock::Text(t) => parts.push(t.text),
            ContentBlock::Image(img) => {
                let placeholder = format!(
                    "[image: {}, {} bytes]",
                    img.mime_type,
                    base64_decoded_len(&img.data)
                );
                collect_image(
                    &mut images,
                    &mut encoded_bytes,
                    &mut parts,
                    &img.mime_type,
                    img.data,
                    placeholder,
                );
            }
            ContentBlock::Resource(r) => match r.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    parts.push(text);
                }
                rmcp::model::ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                    let placeholder = format!(
                        "[resource: {uri}, {mime}, {} bytes]",
                        base64_decoded_len(&blob)
                    );
                    collect_image(
                        &mut images,
                        &mut encoded_bytes,
                        &mut parts,
                        mime,
                        blob,
                        placeholder,
                    );
                }
                _ => unsupported += 1,
            },
            _ => unsupported += 1,
        }
    }
    // A single block (the common case) is moved, not joined into a copy.
    let mut content = if parts.len() == 1 {
        parts.pop().unwrap_or_default()
    } else {
        parts.join("\n")
    };
    // A successful result may carry its whole payload in
    // structuredContent with an empty `content` array. Dropping it
    // leaves the model with empty delimiters, so it may repeat a
    // side-effecting call or invent an answer. Render it whenever it
    // is not already the text the server also sent, so a server that
    // duplicates its payload does not double the output.
    if let Some(structured) = structured
        && let Some(rendered) = render_structured_content(structured)
        && content.trim() != rendered.trim()
    {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&rendered);
    }
    if unsupported > 0 {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&format!(
            "[{unsupported} content block(s) of a kind this client cannot render were omitted]"
        ));
    }
    let response = bound_response(&content);
    if images.is_empty() {
        response
    } else {
        serde_json::json!({"response": response, "parts": images}).to_string()
    }
}

fn collect_image(
    images: &mut Vec<serde_json::Value>,
    encoded_bytes: &mut usize,
    text: &mut Vec<String>,
    mime: &str,
    data: String,
    placeholder: String,
) {
    use crate::session::tool_image::{MAX_TOOL_IMAGE_ENCODED_BYTES, MAX_TOOL_IMAGES};

    let Some(mime_type) = crate::session::user::ImageMime::parse(mime) else {
        text.push(if mime.starts_with("image/") {
            format!("{placeholder} [image omitted: unsupported image format]")
        } else {
            placeholder
        });
        return;
    };
    if images.len() >= MAX_TOOL_IMAGES
        || data.len() > MAX_TOOL_IMAGE_ENCODED_BYTES.saturating_sub(*encoded_bytes)
    {
        text.push(format!(
            "{placeholder} [image omitted: tool image limit (8 images, 7 MiB encoded) exceeded]"
        ));
    } else if let Err(reason) = crate::session::tool_image::inspect_literal_image(&data, &mime_type)
    {
        text.push(format!("{placeholder} [image omitted: {reason}]"));
    } else {
        *encoded_bytes += data.len();
        images.push(serde_json::json!({"type": "image", "mimeType": mime, "data": data}));
    }
}

/// Decoded size of a base64 payload, without decoding it.
fn base64_decoded_len(data: &str) -> usize {
    let significant = data
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .count();
    let padding = data
        .trim_end()
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    (significant / 4 * 3 + (significant % 4).saturating_sub(1)).saturating_sub(padding.min(2))
}

/// Render a successful result's `structuredContent` for the model.
///
/// `null` carries nothing a caller can use, so it is treated as absent; every
/// other value is rendered as pretty JSON, which stays inside the existing
/// output size and spill limits.
fn render_structured_content(structured: &serde_json::Value) -> Option<String> {
    if structured.is_null() {
        return None;
    }
    serde_json::to_string_pretty(structured).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspension_tracking_distinguishes_ready_from_pending_waits() {
        use std::task::{Context, Poll, Waker};
        let flag = AtomicBool::new(false);
        let mut ready = std::pin::pin!(track_suspension(&flag, async { 7 }));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(7));
        assert!(
            !flag.load(Ordering::Relaxed),
            "an immediately ready wait is not a suspension"
        );

        let mut yielded = false;
        let mut waits = std::pin::pin!(track_suspension(
            &flag,
            std::future::poll_fn(move |cx| {
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
        ));
        assert!(waits.as_mut().poll(&mut cx).is_pending());
        assert!(flag.load(Ordering::Relaxed));
        assert!(waits.as_mut().poll(&mut cx).is_ready());
        assert!(
            flag.load(Ordering::Relaxed),
            "the flag stays set once a wait suspended"
        );
    }

    #[test]
    fn malformed_arguments_are_rejected_instead_of_becoming_no_arguments() {
        let error = parse_arguments("{broken").unwrap_err();
        assert!(error.to_string().contains("invalid MCP tool arguments"));
        assert_eq!(parse_arguments("null").unwrap(), None);
        assert_eq!(
            parse_arguments(r#"{"key":"value"}"#).unwrap().unwrap()["key"],
            "value"
        );
    }

    #[test]
    fn model_metadata_truncates_descriptions_on_utf8_boundaries() {
        let definition: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
            "name": "chatty",
            "description": "界".repeat(MCP_TOOL_DESCRIPTION_MAX_BYTES),
            "inputSchema": {"type": "object"}
        }))
        .unwrap();
        let (description, schema, truncated) =
            McpTool::bounded_model_metadata(&definition).unwrap();

        assert!(truncated);
        assert!(description.len() <= MCP_TOOL_DESCRIPTION_MAX_BYTES);
        assert!(description.ends_with("[description truncated]"));
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn model_metadata_rejects_oversized_schemas() {
        let definition: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
            "name": "chatty",
            "inputSchema": {
                "type": "object",
                "description": "x".repeat(MCP_TOOL_SCHEMA_MAX_BYTES)
            }
        }))
        .unwrap();

        assert_eq!(
            McpTool::bounded_model_metadata(&definition),
            Err("input schema exceeds 16 KiB")
        );
    }

    fn png_data() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(include_bytes!(
            "../../agent/tools/fixtures/read-image64/3x2.png"
        ))
    }

    fn image_block(data: &str) -> ContentBlock {
        serde_json::from_value(serde_json::json!({
            "type": "image", "mimeType": "image/png", "data": data
        }))
        .unwrap()
    }

    fn text_block(text: &str) -> ContentBlock {
        serde_json::from_value(serde_json::json!({"type": "text", "text": text})).unwrap()
    }

    fn render_small(blocks: Vec<ContentBlock>, structured: Option<&serde_json::Value>) -> String {
        render_success_content(blocks, structured, |response| {
            bounded_mcp_output_with(response, |_| panic!("small text must not spill"))
        })
    }

    fn retained_images(output: &str) -> Vec<crate::session::tool_image::PersistedToolImage> {
        let result = rig::message::ToolResult {
            id: "mcp-image".into(),
            call_id: None,
            content: rig::message::ToolResultContent::from_tool_output(output),
        };
        crate::session::tool_image::from_result(&result).unwrap()
    }

    #[test]
    fn success_image_envelope_preserves_text_and_reaches_tool_image_conversion() {
        let data = png_data();
        let output = render_small(vec![text_block("first"), image_block(&data)], None);
        let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            envelope,
            serde_json::json!({
                "response": "[mcp output begins]\n> first\n[mcp output ends]",
                "parts": [{"type": "image", "mimeType": "image/png", "data": data}]
            })
        );
        let images = retained_images(&output);
        assert_eq!(images.len(), 1);
        assert_eq!(serde_json::to_value(&images[0]).unwrap()["data"], data);
    }

    #[test]
    fn success_mixed_blocks_keep_text_and_image_resource_order() {
        use base64::Engine;
        let png = png_data();
        let gif = base64::engine::general_purpose::STANDARD.encode(include_bytes!(
            "../../agent/tools/fixtures/read-image64/3x2.gif"
        ));
        let resource = serde_json::from_value(serde_json::json!({
            "type": "resource", "resource": {
                "uri": "urn:image:second", "mimeType": "image/gif", "blob": gif
            }
        }))
        .unwrap();
        let structured = serde_json::json!({"answer": 42});
        let output = render_small(
            vec![
                text_block("first"),
                image_block(&png),
                text_block("middle"),
                resource,
                text_block("last"),
            ],
            Some(&structured),
        );
        let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            envelope["response"],
            "[mcp output begins]\n> first\n> middle\n> last\n> {\n>   \"answer\": 42\n> }\n[mcp output ends]"
        );
        assert_eq!(
            envelope["parts"],
            serde_json::json!([
                {"type": "image", "mimeType": "image/png", "data": png},
                {"type": "image", "mimeType": "image/gif", "data": gif}
            ])
        );
        let images = retained_images(&output);
        assert_eq!(images.len(), 2);
        assert_eq!(serde_json::to_value(&images[0]).unwrap()["data"], png);
        assert_eq!(serde_json::to_value(&images[1]).unwrap()["data"], gif);
    }

    #[test]
    fn success_images_over_count_limit_keep_excess_placeholders() {
        use crate::session::tool_image::MAX_TOOL_IMAGES;
        let data = png_data();
        let blocks = (0..MAX_TOOL_IMAGES + 1)
            .map(|_| image_block(&data))
            .collect();
        let output = render_small(blocks, None);
        let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(retained_images(&output).len(), MAX_TOOL_IMAGES);
        let response = envelope["response"].as_str().unwrap();
        assert_eq!(response.matches("[image: image/png,").count(), 1);
        assert_eq!(response.matches("image omitted:").count(), 1);
        assert!(response.contains("8 images, 7 MiB encoded"));
    }

    // The persistence validator inspects headers without decompressing pixels.
    fn padded_png(encoded_len: usize) -> String {
        use base64::Engine;
        let mut bytes = include_bytes!("../../agent/tools/fixtures/read-image64/3x2.png").to_vec();
        bytes.resize(encoded_len / 4 * 3, 0);
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert_eq!(data.len(), encoded_len);
        data
    }

    #[test]
    fn success_images_at_byte_limit_survive_and_excess_resource_is_omitted() {
        use crate::session::tool_image::MAX_TOOL_IMAGE_ENCODED_BYTES;
        let first = padded_png(4 * 1024 * 1024);
        let second = padded_png(MAX_TOOL_IMAGE_ENCODED_BYTES - first.len());
        let resource = serde_json::from_value(serde_json::json!({
            "type": "resource", "resource": {
                "uri": "urn:image:excess", "mimeType": "image/png", "blob": png_data()
            }
        }))
        .unwrap();
        let output = render_small(
            vec![image_block(&first), image_block(&second), resource],
            None,
        );
        let images = retained_images(&output);
        assert_eq!(images.len(), 2);
        assert_eq!(
            images
                .iter()
                .map(|image| image.encoded_len())
                .sum::<usize>(),
            MAX_TOOL_IMAGE_ENCODED_BYTES
        );
        let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
        let response = envelope["response"].as_str().unwrap();
        assert!(response.contains("[resource: urn:image:excess, image/png,"));
        assert!(response.contains("image omitted:"));
        assert!(!response.contains(&first));
    }

    #[test]
    fn success_oversized_image_does_not_consume_budget_for_later_images() {
        use crate::session::tool_image::MAX_TOOL_IMAGE_ENCODED_BYTES;
        let output = render_small(
            vec![
                image_block(&"A".repeat(MAX_TOOL_IMAGE_ENCODED_BYTES + 1)),
                image_block(&png_data()),
            ],
            None,
        );
        assert_eq!(retained_images(&output).len(), 1);
        let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(
            envelope["response"]
                .as_str()
                .unwrap()
                .contains("image omitted:")
        );
    }

    #[test]
    fn success_all_supported_image_formats_preserve_source_order_and_replay() {
        use base64::Engine;
        let formats = [
            (
                "image/png",
                include_bytes!("../../agent/tools/fixtures/read-image64/3x2.png").as_slice(),
            ),
            (
                "image/jpeg",
                include_bytes!("../../agent/tools/fixtures/read-image64/3x2.jpg").as_slice(),
            ),
            (
                "image/gif",
                include_bytes!("../../agent/tools/fixtures/read-image64/3x2.gif").as_slice(),
            ),
            (
                "image/webp",
                include_bytes!("../../agent/tools/fixtures/read-image64/3x2-lossless.webp")
                    .as_slice(),
            ),
        ];
        let blocks = formats
            .iter()
            .map(|(mime, bytes)| {
                serde_json::from_value(serde_json::json!({
                    "type": "image", "mimeType": mime,
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes)
                }))
                .unwrap()
            })
            .collect();
        let images = retained_images(&render_small(blocks, None));
        assert_eq!(images.len(), formats.len());
        for (image, (mime, bytes)) in images.iter().zip(formats) {
            let value = serde_json::to_value(image).unwrap();
            assert_eq!(value["mime_type"], mime);
            assert_eq!(
                value["data"],
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
        }
    }

    #[test]
    fn success_invalid_images_never_discard_valid_neighbors_or_consume_budget() {
        use base64::Engine;
        let wrong_mime = base64::engine::general_purpose::STANDARD.encode(include_bytes!(
            "../../agent/tools/fixtures/read-image64/3x2.gif"
        ));
        let over_single_image_limit = padded_png(7 * 1024 * 1024);
        let output = render_small(
            vec![
                image_block("not-base64!"),
                image_block("https://never.invalid/remote.png"),
                image_block(&wrong_mime),
                image_block(&over_single_image_limit),
                image_block(&png_data()),
            ],
            None,
        );
        let images = retained_images(&output);
        assert_eq!(images.len(), 1);
        assert_eq!(
            serde_json::to_value(&images[0]).unwrap()["data"],
            png_data()
        );
        let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
        let response = envelope["response"].as_str().unwrap();
        assert_eq!(response.matches("[image omitted:").count(), 4);
        assert!(response.contains("Invalid tool image base64"));
        assert!(response.contains("MIME type does not match"));
        assert!(response.contains("5 MiB"));
        assert!(!response.contains("not-base64!"));
        assert!(!response.contains("never.invalid"));
        assert!(!response.contains(&over_single_image_limit));
    }

    #[test]
    fn unsupported_image_mime_and_non_image_blobs_stay_plain_placeholders() {
        let blocks = serde_json::from_value(serde_json::json!([
            {"type": "text", "text": "first"},
            {"type": "image", "data": "aGVsbG8=", "mimeType": "image/svg+xml"},
            {"type": "resource", "resource": {
                "uri": "file:///report.pdf", "mimeType": "application/pdf", "blob": "AAECAwQF"
            }},
            {"type": "resource", "resource": {"uri": "urn:binary", "blob": "aA=="}}
        ]))
        .unwrap();
        assert_eq!(
            render_small(blocks, None),
            "[mcp output begins]\n> first\n> [image: image/svg+xml, 5 bytes] [image omitted: unsupported image format]\n> [resource: file:///report.pdf, application/pdf, 6 bytes]\n> [resource: urn:binary, application/octet-stream, 1 bytes]\n[mcp output ends]"
        );
    }

    #[test]
    fn no_image_results_are_unchanged_byte_for_byte() {
        for (blocks, structured, expected) in [
            (vec![], None, "[mcp output begins]\n> \n[mcp output ends]"),
            (
                vec![text_block("first"), text_block("second\n雪")],
                None,
                "[mcp output begins]\n> first\n> second\n> 雪\n[mcp output ends]",
            ),
            (
                vec![text_block("42")],
                Some(serde_json::json!(42)),
                "[mcp output begins]\n> 42\n[mcp output ends]",
            ),
            (
                vec![],
                Some(serde_json::json!({"answer": 42})),
                "[mcp output begins]\n> {\n>   \"answer\": 42\n> }\n[mcp output ends]",
            ),
        ] {
            assert_eq!(render_small(blocks, structured.as_ref()), expected);
        }
    }

    #[test]
    fn image_envelope_bypasses_bounds_while_response_spills_or_truncates() {
        let data = padded_png(32 * 1024);
        // Large base64 alone must never trigger a text spill.
        assert_eq!(
            retained_images(&render_small(vec![image_block(&data)], None)).len(),
            1
        );
        let text = "雪".repeat(crate::session::TOOL_RESULT_SAVE_THRESHOLD + 1);
        for fail_save in [false, true] {
            let output = render_success_content(
                vec![text_block(&text), image_block(&data)],
                None,
                |response| {
                    bounded_mcp_output_with_after("host coaching\n\n".into(), response, |saved| {
                        assert_eq!(saved, text);
                        if fail_save {
                            anyhow::bail!("disk full");
                        }
                        Ok(PathBuf::from("/private/mcp-output.txt"))
                    })
                },
            );
            let envelope: serde_json::Value = serde_json::from_str(&output).unwrap();
            let response = envelope["response"].as_str().unwrap();
            assert!(response.starts_with("host coaching\n\n[mcp output begins]\n> "));
            assert!(response.contains("[mcp output ends]\n[mcp host:"));
            assert!(response.contains(if fail_save {
                "output could not be saved"
            } else {
                "full output saved to:"
            }));
            assert!(response.chars().count() < crate::session::TOOL_RESULT_SAVE_THRESHOLD);
            assert_eq!(envelope["parts"][0]["data"], data);
            assert_eq!(retained_images(&output).len(), 1);
        }
    }

    #[test]
    fn base64_decoded_len_handles_padding() {
        assert_eq!(base64_decoded_len(""), 0);
        assert_eq!(base64_decoded_len("aA=="), 1);
        assert_eq!(base64_decoded_len("aGk="), 2);
        assert_eq!(base64_decoded_len("aGVs"), 3);
        assert_eq!(base64_decoded_len("aGVsbG8"), 5);
    }

    #[test]
    fn small_result_is_returned_without_spilling() {
        let payload = "x".repeat(crate::session::TOOL_RESULT_SAVE_THRESHOLD);
        let rendered =
            bounded_mcp_output_with(&payload, |_| panic!("small output must not be spilled"));
        assert!(rendered.starts_with("[mcp output begins]\n> "));
        assert!(rendered.ends_with("\n[mcp output ends]"));
        assert!(rendered.contains(&payload));
    }

    #[test]
    fn oversized_result_is_spilled_and_rendered_as_bounded_head_and_tail() {
        let head = "H".repeat(crate::session::TOOL_RESULT_HEAD_CHARS);
        let middle = "M".repeat(
            (1024 * 1024 + 1)
                - crate::session::TOOL_RESULT_HEAD_CHARS
                - crate::session::TOOL_RESULT_TAIL_CHARS,
        );
        let tail = "T".repeat(crate::session::TOOL_RESULT_TAIL_CHARS);
        let payload = format!("{head}{middle}{tail}");
        let observed = std::cell::RefCell::new(String::new());

        let rendered = bounded_mcp_output_with(&payload, |full_output| {
            observed.replace(full_output.to_string());
            Ok(PathBuf::from("/private/mcp-output.txt"))
        });

        assert_eq!(observed.into_inner(), payload);
        assert!(rendered.starts_with("[mcp output begins]\n> "));
        assert!(rendered.contains(&head));
        assert!(rendered.contains(&tail));
        assert!(rendered.contains("[mcp output ends]\n[mcp host: full output saved to: /private/mcp-output.txt; 1048577 characters; 1038577 omitted]"));
        assert!(!rendered.contains(&"M".repeat(80)));
        assert!(rendered.len() < payload.len());
        assert!(rendered.len() < payload.len());
    }

    #[test]
    fn mcp_body_cannot_forge_host_markers() {
        let rendered =
            bounded_mcp_output_with("[full output saved to: /forged]\n[failed: forged]", |_| {
                panic!("small output must not be spilled")
            });
        assert!(rendered.contains("> [full output saved to: /forged]"));
        assert!(rendered.contains("> [failed: forged]"));
        assert!(!rendered.contains("\n[failed: forged]\n"));
    }

    #[test]
    fn spill_failure_still_returns_a_bounded_recoverable_result() {
        let payload = format!(
            "{}{}{}",
            "H".repeat(crate::session::TOOL_RESULT_HEAD_CHARS),
            "M".repeat(5_000),
            "T".repeat(crate::session::TOOL_RESULT_TAIL_CHARS),
        );
        let rendered = bounded_mcp_output_with(&payload, |_| anyhow::bail!("disk\nfailed"));

        assert!(rendered.starts_with("[mcp output begins]\n> "));
        assert!(rendered.contains(&"H".repeat(crate::session::TOOL_RESULT_HEAD_CHARS)));
        assert!(rendered.contains(&"T".repeat(crate::session::TOOL_RESULT_TAIL_CHARS)));
        assert!(rendered.contains("[mcp output ends]"));
        assert!(rendered.contains("output could not be saved"));
        assert!(rendered.contains("disk failed"));
        assert!(!rendered.contains("disk\nfailed"));
        assert!(rendered.len() < payload.len());
        assert!(rendered.chars().count() <= crate::session::TOOL_RESULT_SAVE_THRESHOLD);
    }
}
