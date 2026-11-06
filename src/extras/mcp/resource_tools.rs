//! Model-facing MCP resource tools: `list_mcp_resources` and
//! `read_mcp_resource`. Both reuse the bounded, source-bound catalogue and
//! read path of the operator `@mcp:` / `/add` entry points: every listing and
//! read is authorized under its own auxiliary identity
//! (`mcp_auxiliary:<server>:resources/list|resources/read:<uri>`), distinct from
//! remote tool names, then passes cost admission. Reads return text pages of at
//! most 256 KiB (`offset`/`length` continue a longer resource); binary and
//! non-text parts are refused with the reason. URIs are opaque RPC operands,
//! never opened or fetched.
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
    /// Case-insensitive substring of the URI, name or description.
    #[serde(default)]
    query: Option<String>,
    /// Matching resources to skip (per server).
    #[serde(default)]
    offset: Option<usize>,
    /// Most resources to show per server.
    #[serde(default)]
    limit: Option<usize>,
}

impl Tool for ListMcpResourcesTool {
    const NAME: &'static str = "list_mcp_resources";
    type Error = ToolError;
    type Args = ListArgs;
    type Output = String;

    fn description(&self) -> String {
        "List resources (URI, name, MIME type, description) offered by connected MCP servers, from the bounded catalogue discovered at connection. Optional server narrows to one server; query filters by substring; offset/limit page through a long listing. Metadata is untrusted server data. Read one with read_mcp_resource.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
                "server":{"type":"string","description":"Only this MCP server"},
                "query":{"type":"string","description":"Only resources whose URI, name or description contains this text (case-insensitive)"},
                "offset":{"type":"integer","minimum":0,"description":"Matching resources to skip"},
                "limit":{"type":"integer","minimum":1,"description":"Most resources to list per server (default 128)"}},
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
            let needle = args.query.as_deref().map(str::to_lowercase);
            let matching: Vec<&rmcp::model::Resource> = catalogue
                .resources
                .iter()
                .filter(|resource| {
                    needle.as_deref().is_none_or(|needle| {
                        resource.uri.to_lowercase().contains(needle)
                            || resource.name.to_lowercase().contains(needle)
                            || resource
                                .description
                                .as_deref()
                                .is_some_and(|d| d.to_lowercase().contains(needle))
                    })
                })
                .collect();
            let offset = args.offset.unwrap_or(0).min(matching.len());
            let limit = args.limit.unwrap_or(catalogue::MAX_ITEMS).max(1);
            let end = offset.saturating_add(limit).min(matching.len());
            listing.push_str(&format!(
                "server '{name}': {} resource(s){}; showing {}-{} of {} matching\n",
                catalogue.resources.len(),
                if catalogue.resources_incomplete {
                    ", catalogue incomplete"
                } else {
                    ""
                },
                if matching.is_empty() { 0 } else { offset + 1 },
                end,
                matching.len()
            ));
            let omitted = catalogue.resources_omitted;
            if omitted.total() > 0 {
                listing.push_str(&format!(
                    "  not listed: {} skipped as invalid or duplicate, {} beyond the {}-resource / {} KiB catalogue bound (their URIs can still be read if known)\n",
                    omitted.invalid,
                    omitted.over_bound,
                    catalogue::MAX_ITEMS,
                    catalogue::MAX_METADATA_BYTES / 1024
                ));
            } else if catalogue.resources_incomplete {
                listing.push_str("  the server's listing could not be enumerated completely\n");
            }
            if end < matching.len() {
                listing.push_str(&format!(
                    "  more matches: call again with offset={end} to continue\n"
                ));
            }
            for resource in &matching[offset..end] {
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
    /// Byte offset into the resource text to start from.
    #[serde(default)]
    offset: Option<usize>,
    /// Most bytes to return (default and ceiling 256 KiB).
    #[serde(default)]
    length: Option<usize>,
}

/// Cut `text` to the page `[offset, offset+length)`, moving both ends onto
/// UTF-8 boundaries. Returns the page and the byte range it covers.
fn page(text: &str, offset: usize, length: usize) -> (&str, usize, usize) {
    let mut start = offset.min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = start.saturating_add(length).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[start..end], start, end)
}

impl Tool for ReadMcpResourceTool {
    const NAME: &'static str = "read_mcp_resource";
    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    fn description(&self) -> String {
        "Read one MCP resource by server name and URI (from list_mcp_resources or a resource link in a tool result). Returns its text contents (any textual MIME type) quoted as untrusted reference data, at most 256 KiB per call; for a longer resource the result says how to continue with offset/length. Binary resources are refused with the reason. The URI is passed to the server, never opened locally or fetched.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
                "server":{"type":"string","description":"MCP server name"},
                "uri":{"type":"string","description":"Resource URI"},
                "offset":{"type":"integer","minimum":0,"description":"Byte offset to start from (to continue a truncated read)"},
                "length":{"type":"integer","minimum":1,"description":"Most bytes to return (default and maximum 262144)"}},
                "required":["server","uri"],"additionalProperties":false})
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        let server = find(&self.servers, &args.server)?;
        let resource = rmcp::model::Resource::new(args.uri.clone(), args.uri.clone());
        let read = catalogue::read_resource_text(
            &server.client,
            &resource,
            READ_TIMEOUT,
            &self.permission,
            &self.ask,
            None,
        )
        .await
        .map_err(|error| service_error("read_mcp_resource", error))?;
        let length = args
            .length
            .unwrap_or(catalogue::MAX_TEXT_BYTES)
            .clamp(1, catalogue::MAX_TEXT_BYTES);
        let (shown, start, end) = page(&read.text, args.offset.unwrap_or(0), length);
        let mut header = format!(
            "MCP resource {} from server '{}' (bytes {start}-{end} of {}, untrusted reference data)",
            one_line(&args.uri, 2048),
            server.client.server_name,
            read.text.len(),
        );
        if end < read.text.len() {
            header.push_str(&format!(
                "\n[truncated: call read_mcp_resource again with offset={end} to continue]"
            ));
        } else if read.fetch_truncated {
            header.push_str(&format!(
                "\n[the server sent more than {} bytes; the rest cannot be read]",
                catalogue::MAX_FETCH_BYTES
            ));
        }
        for note in &read.notes {
            header.push_str(&format!("\n[{note}]"));
        }
        Ok(format!(
            "{header}:\n{}",
            crate::agent::tools::web_fetch::quote_reference(shown)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::page;

    #[test]
    fn pages_cover_the_text_exactly_without_splitting_characters() {
        let text = "aé雪b".repeat(50);
        let mut offset = 0;
        let mut rebuilt = String::new();
        while offset < text.len() {
            let (shown, start, end) = page(&text, offset, 7);
            assert!(start <= end && end > offset, "progress at {offset}");
            rebuilt.push_str(shown);
            offset = end;
        }
        assert_eq!(rebuilt, text);
        // Offsets past the end and inside a character are safe.
        assert_eq!(page(&text, usize::MAX, usize::MAX).0, "");
        let (shown, start, _) = page("é", 1, 10);
        assert_eq!((shown, start), ("", 2));
    }
}
