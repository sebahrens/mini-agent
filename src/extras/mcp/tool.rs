use std::fmt;
use std::path::PathBuf;
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
    pub definition: rmcp::model::Tool,
    pub(super) model_description: String,
    pub(super) model_parameters: serde_json::Value,
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
    /// Deterministic name used when two servers expose the same tool name.
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
        let schema = serde_json::to_value(&definition.input_schema)
            .map_err(|_| "input schema could not be serialized")?;
        if serde_json::to_vec(&schema)
            .map_err(|_| "input schema could not be serialized")?
            .len()
            > MCP_TOOL_SCHEMA_MAX_BYTES
        {
            return Err("input schema exceeds 16 KiB");
        }
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
    bounded_mcp_output_with(output, |full_output| {
        crate::session::storage::save_tool_output(spill_scope, tool_name, full_output)
    })
}

fn bounded_mcp_output_with(
    output: &str,
    save: impl FnOnce(&str) -> anyhow::Result<PathBuf>,
) -> String {
    let output_chars = output.chars().count();
    if output_chars <= crate::session::TOOL_RESULT_SAVE_THRESHOLD {
        return output.to_string();
    }

    match save(output) {
        Ok(path) => crate::session::format_truncated_tool_output(output, output_chars, &path),
        Err(error) => {
            tracing::debug!(%error, "failed to spill oversized MCP tool result");
            format_unsaved_mcp_output(output, output_chars, &error.to_string())
        }
    }
}

fn format_unsaved_mcp_output(output: &str, output_chars: usize, error: &str) -> String {
    let head: String = output
        .chars()
        .take(crate::session::TOOL_RESULT_HEAD_CHARS)
        .collect();
    let tail_start = output_chars.saturating_sub(crate::session::TOOL_RESULT_TAIL_CHARS);
    let tail: String = output.chars().skip(tail_start).collect();
    let omitted = output_chars.saturating_sub(
        crate::session::TOOL_RESULT_HEAD_CHARS + crate::session::TOOL_RESULT_TAIL_CHARS,
    );
    let diagnostic = error
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(300)
        .collect::<String>();

    format!(
        "{head}\n\n[tool output truncated: {output_chars} characters; {omitted} omitted]\n[full output could not be saved; re-run the MCP tool with a narrower request: {diagnostic}]\n\n{tail}"
    )
}

impl ToolDyn for McpTool {
    fn name(&self) -> String {
        self.registered_name.to_string()
    }

    fn description(&self) -> String {
        self.model_description.clone()
    }

    fn parameters(&self) -> serde_json::Value {
        self.model_parameters.clone()
    }

    fn call(&self, args: String) -> WasmBoxedFuture<'_, Result<String, ToolError>> {
        let server_name = self.server_name.clone();
        let trusted_identity = self.trusted_identity;
        let tool_name = self.definition.name.to_string();
        let peer = self.peer.clone();
        let permission = self.permission.clone();
        let ask_tx = self.ask_tx.clone();
        let registered_name = self.registered_name.clone();
        let call_timeout = self.call_timeout;
        let spill_scope = self.spill_scope.clone();

        Box::pin(async move {
            let perm_key = format!("mcp_tool:{server_name}:{tool_name}");
            let coaching = check_mcp_perm(
                &permission,
                &ask_tx,
                &perm_key,
                trusted_identity,
                &tool_name,
                &registered_name,
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

            let mut content = String::new();
            for item in result.content {
                match item {
                    ContentBlock::Text(t) => content.push_str(&t.text),
                    ContentBlock::Image(img) => {
                        content.push_str("data:");
                        content.push_str(&img.mime_type);
                        content.push_str(";base64,");
                        content.push_str(&img.data);
                    }
                    ContentBlock::Resource(r) => match &r.resource {
                        rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                            content.push_str(text);
                        }
                        rmcp::model::ResourceContents::BlobResourceContents { blob, .. } => {
                            content.push_str(blob);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if let Some(msg) = coaching {
                content = format!("{}\n\n{}", msg, content);
            }
            Ok(bounded_mcp_output(
                &content,
                &spill_scope,
                &format!("mcp:{server_name}:{tool_name}"),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn small_result_is_returned_without_spilling() {
        let payload = "x".repeat(crate::session::TOOL_RESULT_SAVE_THRESHOLD);
        let rendered =
            bounded_mcp_output_with(&payload, |_| panic!("small output must not be spilled"));
        assert_eq!(rendered, payload);
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
        assert!(rendered.starts_with(&head));
        assert!(rendered.ends_with(&tail));
        assert!(rendered.contains("[tool output truncated: 1048577 characters; 1038577 omitted]"));
        assert!(rendered.contains("[full output saved to: /private/mcp-output.txt;"));
        assert!(!rendered.contains(&"M".repeat(80)));
        assert!(rendered.len() < payload.len());
        assert!(
            rendered.chars().count() <= crate::session::TOOL_RESULT_SAVE_THRESHOLD,
            "the model-visible recovery view must remain below the ordinary tool-result spill threshold"
        );
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

        assert!(rendered.starts_with(&"H".repeat(crate::session::TOOL_RESULT_HEAD_CHARS)));
        assert!(rendered.ends_with(&"T".repeat(crate::session::TOOL_RESULT_TAIL_CHARS)));
        assert!(rendered.contains("full output could not be saved"));
        assert!(rendered.contains("disk failed"));
        assert!(!rendered.contains("disk\nfailed"));
        assert!(rendered.len() < payload.len());
        assert!(rendered.chars().count() <= crate::session::TOOL_RESULT_SAVE_THRESHOLD);
    }
}
