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
        let call_timeout = self.call_timeout;

        Box::pin(async move {
            let perm_key = format!("mcp_tool:{server_name}:{tool_name}");
            let coaching = check_mcp_perm(
                &permission,
                &ask_tx,
                &perm_key,
                trusted_identity,
                &tool_name,
            )
            .await
            .map_err(|e| {
                ToolError::ToolCallError(Box::new(McpToolError(CompactString::new(e.to_string()))))
            })?;

            let arguments: Option<JsonObject> = serde_json::from_str(&args).unwrap_or_default();
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
                let error_msg = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
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
                    ContentBlock::Text(t) => content.push_str(&t.text),
                    ContentBlock::Image(img) => {
                        content.push_str(&format!("data:{};base64,{}", img.mime_type, img.data));
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
            Ok(content)
        })
    }
}
