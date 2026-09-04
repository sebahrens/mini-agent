use std::borrow::Cow;
use std::fmt;
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

/// Hard ceiling for data accepted from one untrusted MCP `tools/call`
/// response before it is handed to the model.
const MAX_MCP_TOOL_RESULT_BYTES: usize = 1024 * 1024;

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
}

impl McpTool {
    /// Deterministic name used when two servers expose the same tool name.
    pub fn namespaced_name(server_name: &str, tool_name: &str) -> CompactString {
        CompactString::new(format!("{server_name}__{tool_name}"))
    }
}

fn parse_arguments(args: &str) -> Result<Option<JsonObject>, McpToolError> {
    serde_json::from_str(args).map_err(|error| {
        McpToolError(CompactString::new(format!(
            "invalid MCP tool arguments: {error}"
        )))
    })
}

fn append_bounded(output: &mut String, value: &str) -> Result<(), McpToolError> {
    if output.len().saturating_add(value.len()) > MAX_MCP_TOOL_RESULT_BYTES {
        return Err(McpToolError(CompactString::new(format!(
            "MCP tool result exceeded the {} byte limit",
            MAX_MCP_TOOL_RESULT_BYTES
        ))));
    }
    output.push_str(value);
    Ok(())
}

impl ToolDyn for McpTool {
    fn name(&self) -> String {
        self.registered_name.to_string()
    }

    fn description(&self) -> String {
        self.definition
            .description
            .clone()
            .unwrap_or(Cow::from(""))
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(&self.definition.input_schema).unwrap_or_default()
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
                        append_bounded(&mut error_msg, "\n")
                            .map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
                    }
                    append_bounded(&mut error_msg, text)
                        .map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
                }
                let msg = if error_msg.is_empty() {
                    "MCP tool returned an error".to_string()
                } else {
                    error_msg
                };
                return Err(ToolError::ToolCallError(Box::new(McpToolError(
                    CompactString::new(msg),
                ))));
            }

            let mut content = String::new();
            for item in result.content {
                match item {
                    ContentBlock::Text(t) => append_bounded(&mut content, &t.text),
                    ContentBlock::Image(img) => append_bounded(&mut content, "data:")
                        .and_then(|()| append_bounded(&mut content, &img.mime_type))
                        .and_then(|()| append_bounded(&mut content, ";base64,"))
                        .and_then(|()| append_bounded(&mut content, &img.data)),
                    ContentBlock::Resource(r) => match &r.resource {
                        rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                            append_bounded(&mut content, text)
                        }
                        rmcp::model::ResourceContents::BlobResourceContents { blob, .. } => {
                            append_bounded(&mut content, blob)
                        }
                        _ => Ok(()),
                    },
                    _ => Ok(()),
                }
                .map_err(|error| ToolError::ToolCallError(Box::new(error)))?;
            }
            if let Some(msg) = coaching {
                content = format!("{}\n\n{}", msg, content);
            }
            Ok(content)
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
    fn cumulative_result_size_is_bounded() {
        let mut accumulated = "a".repeat(MAX_MCP_TOOL_RESULT_BYTES - 1);
        append_bounded(&mut accumulated, "b").unwrap();
        assert_eq!(accumulated.len(), MAX_MCP_TOOL_RESULT_BYTES);
        let error = append_bounded(&mut accumulated, "c").unwrap_err();
        assert!(error.to_string().contains("exceeded"));
        assert_eq!(accumulated.len(), MAX_MCP_TOOL_RESULT_BYTES);
    }
}
