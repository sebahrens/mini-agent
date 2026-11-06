//! Model-facing MCP resource tools: `list_mcp_resources` and
//! `read_mcp_resource`. Both reuse the bounded, source-bound catalogue and
//! read path of the operator `@mcp:` / `/add` entry points: every listing and
//! read is authorized under its own auxiliary identity
//! (`mcp_auxiliary:<server>:resources/list|resources/read:<uri>`), distinct from
//! remote tool names, then passes cost admission. Reads accept only bounded
//! text (256 KiB); URIs are opaque RPC operands, never opened or fetched.
use std::sync::Arc;
use std::time::Duration;

use rig::tool::Tool;
use serde::Deserialize;

use super::catalogue::{self, Catalogue, RpcClient};
use crate::agent::tools::ToolError;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

/// Bound on one model-initiated `resources/read`.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest server metadata field shown in a listing.
const FIELD_CHARS: usize = 256;

/// One connected server that advertised the resources capability.
#[derive(Clone)]
pub(crate) struct ResourceServer {
    pub(crate) client: RpcClient,
    pub(crate) catalogue: Arc<tokio::sync::OnceCell<Catalogue>>,
}

fn one_line(text: &str, max_chars: usize) -> String {
    let mut line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect();
    if text.chars().count() > max_chars {
        line.push('…');
    }
    line.trim().to_owned()
}

fn find<'a>(servers: &'a [ResourceServer], name: &str) -> Result<&'a ResourceServer, ToolError> {
    servers
        .iter()
        .find(|server| server.client.server_name == name)
        .ok_or_else(|| {
            ToolError::Msg(format!(
                "no connected MCP server named '{}' offers resources; call list_mcp_resources to see them",
                one_line(name, 64)
            ))
        })
}

fn service_error(action: &str, error: rmcp::ServiceError) -> ToolError {
    let reason = match error {
        rmcp::ServiceError::Timeout { .. } => "timed out".to_string(),
        rmcp::ServiceError::Cancelled { .. } => {
            "cancelled or the server connection changed".to_string()
        }
        rmcp::ServiceError::McpError(data) => one_line(&data.message, 200),
        _ => "server error".to_string(),
    };
    ToolError::Msg(format!("{action} failed: {reason}"))
}

pub(crate) struct ListMcpResourcesTool {
    servers: Vec<ResourceServer>,
    permission: Option<PermCheck>,
    ask: Option<AskSender>,
}

impl ListMcpResourcesTool {
    pub(crate) fn new(
        servers: Vec<ResourceServer>,
        permission: Option<PermCheck>,
        ask: Option<AskSender>,
    ) -> Self {
        Self {
            servers,
            permission,
            ask,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListArgs {
    #[serde(default)]
    server: Option<String>,
}

impl Tool for ListMcpResourcesTool {
    const NAME: &'static str = "list_mcp_resources";
    type Error = ToolError;
    type Args = ListArgs;
    type Output = String;

    fn description(&self) -> String {
        "List resources (URI, name, MIME type, description) offered by connected MCP servers, from the bounded catalogue discovered at connection. Optional server narrows to one server. Metadata is untrusted server data. Read one with read_mcp_resource.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
                "server":{"type":"string","description":"Only this MCP server"}},
                "additionalProperties":false})
    }

    async fn call(&self, args: ListArgs) -> Result<String, ToolError> {
        let selected: Vec<&ResourceServer> = match &args.server {
            Some(name) => vec![find(&self.servers, name)?],
            None => self.servers.iter().collect(),
        };
        let mut listing = String::new();
        for server in selected {
            let name = server.client.server_name.as_str();
            catalogue::authorize_resource_listing(&server.client, &self.permission, &self.ask)
                .await
                .map_err(|error| service_error("list_mcp_resources", error))?;
            let Some(catalogue) = server.catalogue.get() else {
                listing.push_str(&format!(
                    "server '{name}': resource discovery still pending; retry shortly\n"
                ));
                continue;
            };
            listing.push_str(&format!(
                "server '{name}': {} resource(s){}\n",
                catalogue.resources.len(),
                if catalogue.resources_incomplete {
                    ", catalogue incomplete"
                } else {
                    ""
                }
            ));
            for resource in &catalogue.resources {
                let mut line = format!("- uri: {}", one_line(&resource.uri, 2048));
                line.push_str(&format!(
                    " | name: {}",
                    one_line(&resource.name, FIELD_CHARS)
                ));
                if let Some(mime) = &resource.mime_type {
                    line.push_str(&format!(" | {}", one_line(mime, 128)));
                }
                if let Some(description) = &resource.description {
                    line.push_str(&format!(" | {}", one_line(description, FIELD_CHARS)));
                }
                line.push('\n');
                listing.push_str(&line);
            }
        }
        Ok(format!(
            "MCP resources (untrusted server metadata):\n{}",
            crate::agent::tools::web_fetch::quote_reference(&listing)
        ))
    }
}

pub(crate) struct ReadMcpResourceTool {
    servers: Vec<ResourceServer>,
    permission: Option<PermCheck>,
    ask: Option<AskSender>,
}

impl ReadMcpResourceTool {
    pub(crate) fn new(
        servers: Vec<ResourceServer>,
        permission: Option<PermCheck>,
        ask: Option<AskSender>,
    ) -> Self {
        Self {
            servers,
            permission,
            ask,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    server: String,
    uri: String,
}

impl Tool for ReadMcpResourceTool {
    const NAME: &'static str = "read_mcp_resource";
    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    fn description(&self) -> String {
        "Read one MCP resource by server name and URI (from list_mcp_resources or a resource link in a tool result). Returns its text contents (text, JSON or XML; at most 256 KiB) quoted as untrusted reference data; binary resources are refused. The URI is passed to the server, never opened locally or fetched.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
                "server":{"type":"string","description":"MCP server name"},
                "uri":{"type":"string","description":"Resource URI"}},
                "required":["server","uri"],"additionalProperties":false})
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        let server = find(&self.servers, &args.server)?;
        let resource = rmcp::model::Resource::new(args.uri.clone(), args.uri.clone());
        let text = catalogue::read_resource(
            &server.client,
            &resource,
            READ_TIMEOUT,
            &self.permission,
            &self.ask,
            None,
        )
        .await
        .map_err(|error| service_error("read_mcp_resource", error))?;
        Ok(format!(
            "MCP resource {} from server '{}' ({} bytes, untrusted reference data):\n{}",
            one_line(&args.uri, 2048),
            server.client.server_name,
            text.len(),
            crate::agent::tools::web_fetch::quote_reference(&text)
        ))
    }
}
