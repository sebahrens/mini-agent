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
/// How long a fetched resource stays reusable for continuation pages.
const CACHE_TTL: Duration = Duration::from_secs(120);
/// Resources kept for continuation pages (each at most 4 MiB).
const CACHE_ENTRIES: usize = 4;
/// Longest server metadata field shown in a listing.
const FIELD_CHARS: usize = 256;

/// One connected server that advertised the resources capability.
#[derive(Clone)]
pub(crate) struct ResourceServer {
    pub(crate) client: RpcClient,
    pub(crate) catalogue: Arc<catalogue::AuxiliaryCache>,
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
    /// Matching resources to skip (per server; pass `server` with it).
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
        "List resources (URI, name, MIME type, description) and resource templates (parameterised URI patterns such as repo://{owner}/{repo}/file, listed as uri_template lines; fill in the variables and read the result with read_mcp_resource) offered by connected MCP servers, from the bounded catalogue discovered at connection and refreshed when the server announces a change. Optional server narrows to one server; query filters by substring; offset/limit page through a long listing. Metadata is untrusted server data. Read one with read_mcp_resource.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
                "server":{"type":"string","description":"Only this MCP server"},
                "query":{"type":"string","description":"Only resources whose URI, name or description contains this text (case-insensitive)"},
                "offset":{"type":"integer","minimum":0,"description":"Matching resources to skip, counted within one server: pass it together with server (continuation hints name both)"},
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
            if server.catalogue.is_stale() {
                server
                    .catalogue
                    .ensure(&server.client, std::time::Duration::from_secs(3))
                    .await;
            }
            let Some(catalogue) = server.catalogue.get() else {
                listing.push_str(&format!(
                    "server '{name}': resource discovery still pending; retry shortly\n"
                ));
                continue;
            };
            let needle = args.query.as_deref().map(str::to_lowercase);
            listing.push_str(&render_server_listing(
                name,
                &catalogue,
                needle.as_deref(),
                args.offset.unwrap_or(0),
                args.limit.unwrap_or(catalogue::MAX_ITEMS),
                args.server.is_some(),
                args.query.as_deref(),
            ));
        }
        Ok(format!(
            "MCP resources (untrusted server metadata):\n{}",
            crate::agent::tools::web_fetch::quote_reference(&listing)
        ))
    }
}

/// The listing block of one server. `offset`/`limit` apply to this server's
/// matches only, so when the call did not name a server (`scoped` false) the
/// continuation hint names it.
fn render_server_listing(
    name: &str,
    catalogue: &Catalogue,
    needle: Option<&str>,
    offset: usize,
    limit: usize,
    scoped: bool,
    query: Option<&str>,
) -> String {
    let mut listing = String::new();
    let matching: Vec<&rmcp::model::Resource> = catalogue
        .resources
        .iter()
        .filter(|resource| {
            needle.is_none_or(|needle| {
                resource.uri.to_lowercase().contains(needle)
                    || resource.name.to_lowercase().contains(needle)
                    || resource
                        .description
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(needle))
            })
        })
        .collect();
    let offset = offset.min(matching.len());
    let limit = limit.max(1);
    let end = offset.saturating_add(limit).min(matching.len());
    listing.push_str(&format!(
        "server '{name}': {} resource(s){}; showing {}-{} of {} matching\n",
        catalogue.resources.len(),
        if catalogue.resources_incomplete {
            ", catalogue incomplete"
        } else {
            ""
        },
        if offset >= matching.len() {
            0
        } else {
            offset + 1
        },
        if offset >= matching.len() { 0 } else { end },
        matching.len()
    ));
    if offset >= matching.len() && !matching.is_empty() {
        listing.push_str(&format!(
            "  offset is past the last of {} matching resource(s); call again with offset=0 to start over\n",
            matching.len()
        ));
    }
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
        // Offset and limit apply per server, so an unscoped listing must
        // name the server or the next page would skip entries of every
        // other one; a changed query would change what the offset means.
        let mut hint = String::new();
        if !scoped {
            hint.push_str(&format!("server={} ", one_line(name, 64)));
        }
        hint.push_str(&format!("offset={end}"));
        if let Some(query) = query {
            hint.push_str(&format!(" query={}", one_line(query, FIELD_CHARS)));
        }
        if limit != catalogue::MAX_ITEMS {
            hint.push_str(&format!(" limit={limit}"));
        }
        listing.push_str(&format!(
            "  more matches: call again with {hint} to continue\n"
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
    listing.push_str(&render_templates(catalogue, needle));
    listing
}

/// The resource-template block of one server: parameterised URI patterns the
/// model can fill in and pass to `read_mcp_resource`. Bounded like the other
/// catalogues, with an incomplete note; `offset`/`limit` page resources only.
fn render_templates(catalogue: &Catalogue, needle: Option<&str>) -> String {
    let omitted = catalogue.templates_omitted;
    if catalogue.templates.is_empty() && !catalogue.templates_incomplete && omitted.total() == 0 {
        return String::new();
    }
    let matching: Vec<&rmcp::model::ResourceTemplate> = catalogue
        .templates
        .iter()
        .filter(|template| {
            needle.is_none_or(|needle| {
                template.uri_template.to_lowercase().contains(needle)
                    || template.name.to_lowercase().contains(needle)
                    || template
                        .description
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(needle))
            })
        })
        .collect();
    let mut block = format!(
        "  resource templates: {} template(s){}; {} matching (fill in the {{variables}} and pass the URI to read_mcp_resource)\n",
        catalogue.templates.len(),
        if catalogue.templates_incomplete {
            ", template catalogue incomplete"
        } else {
            ""
        },
        matching.len()
    );
    if omitted.total() > 0 {
        block.push_str(&format!(
            "  templates not listed: {} skipped as invalid or duplicate, {} beyond the {}-template / {} KiB catalogue bound\n",
            omitted.invalid,
            omitted.over_bound,
            catalogue::MAX_ITEMS,
            catalogue::MAX_METADATA_BYTES / 1024
        ));
    } else if catalogue.templates_incomplete {
        block.push_str("  the server's template listing could not be enumerated completely\n");
    }
    for template in matching {
        let mut line = format!(
            "- uri_template: {} | name: {}",
            one_line(&template.uri_template, 2048),
            one_line(&template.name, FIELD_CHARS)
        );
        if let Some(mime) = &template.mime_type {
            line.push_str(&format!(" | {}", one_line(mime, 128)));
        }
        if let Some(description) = &template.description {
            line.push_str(&format!(" | {}", one_line(description, FIELD_CHARS)));
        }
        line.push('\n');
        block.push_str(&line);
    }
    block
}

/// A resource text fetched for paging, reused by its continuation pages.
struct CachedRead {
    server: String,
    uri: String,
    read: catalogue::ResourceText,
    hash: String,
    at: std::time::Instant,
}

/// First 12 hex digits of the SHA-256 of the text: a version label for paging.
fn content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

pub(crate) struct ReadMcpResourceTool {
    servers: Vec<ResourceServer>,
    permission: Option<PermCheck>,
    ask: Option<AskSender>,
    /// Per-session cache so continuation pages do not re-read or re-prompt.
    cache: std::sync::Mutex<Vec<CachedRead>>,
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
            cache: std::sync::Mutex::new(Vec::new()),
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
    /// `content_hash` from the page being continued; a different current
    /// content is reported and read from the start instead of spliced.
    #[serde(default)]
    content_hash: Option<String>,
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
        "Read one MCP resource by server name and URI (from list_mcp_resources or a resource link in a tool result). Returns its text contents (any textual MIME type) quoted as untrusted reference data, at most 256 KiB per call; for a longer resource the result says how to continue with offset (and content_hash); continuation pages within two minutes reuse the first read instead of fetching and asking again, and a changed resource is reported. Binary resources are refused with the reason. The URI is passed to the server, never opened locally or fetched.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
                "server":{"type":"string","description":"MCP server name"},
                "uri":{"type":"string","description":"Resource URI"},
                "offset":{"type":"integer","minimum":0,"description":"Byte offset to start from (to continue a truncated read)"},
                "length":{"type":"integer","minimum":1,"description":"Most bytes to return (default and maximum 262144)"},
                "content_hash":{"type":"string","description":"content_hash shown on the page being continued; if the resource changed, the read restarts from the beginning and says so"}},
                "required":["server","uri"],"additionalProperties":false})
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        let server = find(&self.servers, &args.server)?;
        let requested_offset = args.offset.unwrap_or(0);
        let mut notes: Vec<String> = Vec::new();
        // A continuation page within the window reuses the first read: no
        // second server request and no second permission prompt.
        let cached = (requested_offset > 0)
            .then(|| self.cached(&server.client.server_name, &args.uri))
            .flatten();
        let (read, hash, offset) = match cached {
            Some((read, hash)) => {
                if args.content_hash.as_deref().is_some_and(|h| h != hash) {
                    notes.push(format!(
                        "the resource changed since the page you are continuing (content_hash {} -> {hash}); showing from the start of the current content",
                        one_line(args.content_hash.as_deref().unwrap_or(""), 32)
                    ));
                    (read, hash, 0)
                } else {
                    (read, hash, requested_offset)
                }
            }
            None => {
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
                let hash = content_hash(&read.text);
                let previous = self.remember(&server.client.server_name, &args.uri, &read, &hash);
                let mut offset = requested_offset;
                if let Some(expected) = args.content_hash.as_deref()
                    && expected != hash
                {
                    notes.push(format!(
                        "the resource changed since the page you are continuing (content_hash {} -> {hash}); showing from the start of the current content",
                        one_line(expected, 32)
                    ));
                    offset = 0;
                } else if requested_offset == 0
                    && let Some(previous) = previous
                    && previous != hash
                {
                    notes.push(format!(
                        "the resource changed since it was last read in this session (content_hash {previous} -> {hash})"
                    ));
                }
                (read, hash, offset)
            }
        };
        let length = args
            .length
            .unwrap_or(catalogue::MAX_TEXT_BYTES)
            .clamp(1, catalogue::MAX_TEXT_BYTES);
        let (shown, start, end) = page(&read.text, offset, length);
        let mut header = format!(
            "MCP resource {} from server '{}' (bytes {start}-{end} of {}, content_hash {hash}, untrusted reference data)",
            one_line(&args.uri, 2048),
            server.client.server_name,
            read.text.len(),
        );
        if end < read.text.len() {
            header.push_str(&format!(
                "\n[truncated: call read_mcp_resource again with offset={end} content_hash={hash} to continue; pages within {} s reuse this read]",
                CACHE_TTL.as_secs()
            ));
        } else if read.fetch_truncated {
            header.push_str(&format!(
                "\n[the server sent more than {} bytes; the rest cannot be read]",
                catalogue::MAX_FETCH_BYTES
            ));
        }
        for note in notes.iter().chain(&read.notes) {
            header.push_str(&format!("\n[{note}]"));
        }
        Ok(format!(
            "{header}:\n{}",
            crate::agent::tools::web_fetch::quote_reference(shown)
        ))
    }
}

impl ReadMcpResourceTool {
    fn cached(&self, server: &str, uri: &str) -> Option<(catalogue::ResourceText, String)> {
        let mut cache = self.cache.lock().expect("resource cache");
        cache.retain(|entry| entry.at.elapsed() < CACHE_TTL);
        cache
            .iter()
            .find(|entry| entry.server == server && entry.uri == uri)
            .map(|entry| (clone_text(&entry.read), entry.hash.clone()))
    }

    /// Store a fresh read; returns the hash of the previous read of the same
    /// resource, if one was still cached.
    fn remember(
        &self,
        server: &str,
        uri: &str,
        read: &catalogue::ResourceText,
        hash: &str,
    ) -> Option<String> {
        let mut cache = self.cache.lock().expect("resource cache");
        let previous = cache
            .iter()
            .position(|entry| entry.server == server && entry.uri == uri)
            .map(|index| cache.remove(index).hash);
        if cache.len() >= CACHE_ENTRIES {
            cache.remove(0);
        }
        cache.push(CachedRead {
            server: server.to_owned(),
            uri: uri.to_owned(),
            read: clone_text(read),
            hash: hash.to_owned(),
            at: std::time::Instant::now(),
        });
        previous
    }
}

fn clone_text(read: &catalogue::ResourceText) -> catalogue::ResourceText {
    catalogue::ResourceText {
        text: read.text.clone(),
        fetch_truncated: read.fetch_truncated,
        notes: read.notes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Catalogue, page, render_server_listing};

    // mini-agent-u0ddh: templates are listed, filtered, bounded and flagged.
    #[test]
    fn listing_shows_resource_templates_with_filter_and_incomplete_flag() {
        let template = |uri: &str, name: &str| {
            serde_json::from_value::<rmcp::model::ResourceTemplate>(serde_json::json!({
                "uriTemplate": uri, "name": name, "description": "files", "mimeType": "text/plain"
            }))
            .unwrap()
        };
        let mut catalogue = Catalogue {
            templates: vec![
                template("repo://{owner}/{repo}/blob/{path}", "file"),
                template("issue://{id}", "issue"),
            ],
            ..Catalogue::default()
        };
        let all = render_server_listing("gh", &catalogue, None, 0, 128, true, None);
        assert!(
            all.contains("resource templates: 2 template(s); 2 matching"),
            "{all}"
        );
        assert!(
            all.contains("- uri_template: repo://{owner}/{repo}/blob/{path} | name: file"),
            "{all}"
        );
        // Resources stay empty yet the templates make the parameterised URIs discoverable.
        assert!(!all.contains("- uri: "), "{all}");
        let filtered = render_server_listing("gh", &catalogue, Some("issue"), 0, 128, true, None);
        assert!(
            filtered.contains("1 matching") && !filtered.contains("repo://"),
            "{filtered}"
        );
        catalogue.templates_incomplete = true;
        catalogue.templates_omitted.over_bound = 3;
        let flagged = render_server_listing("gh", &catalogue, None, 0, 128, true, None);
        assert!(
            flagged.contains("template catalogue incomplete"),
            "{flagged}"
        );
        assert!(flagged.contains("3 beyond the 128-template"), "{flagged}");
        // A server with no templates adds nothing to the listing.
        let none = render_server_listing("gh", &Catalogue::default(), None, 0, 128, true, None);
        assert!(!none.contains("template"), "{none}");
    }

    fn catalogue_of(prefix: &str, count: usize) -> Catalogue {
        Catalogue {
            resources: (0..count)
                .map(|n| {
                    rmcp::model::Resource::new(format!("{prefix}://{n}"), format!("{prefix}{n}"))
                })
                .collect(),
            ..Catalogue::default()
        }
    }

    fn uris(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|l| l.strip_prefix("- uri: "))
            .map(|l| l.split(" | ").next().unwrap().to_owned())
            .collect()
    }

    /// Follow the printed `more matches:` hints the way a model would: the
    /// first call names no server, later calls pass only what a hint names.
    fn walk(servers: &[(&str, Catalogue)]) -> Vec<String> {
        let first: String = servers
            .iter()
            .map(|(n, c)| render_server_listing(n, c, None, 0, 128, false, None))
            .collect();
        let mut seen = uris(&first);
        // Each server block carries its own hint.
        for hint in first.lines().filter(|l| l.contains("more matches")) {
            let name = hint
                .split("server=")
                .nth(1)
                .expect("unscoped hint names the server")
                .split_whitespace()
                .next()
                .unwrap();
            let catalogue = &servers.iter().find(|(n, _)| *n == name).unwrap().1;
            let mut hint = hint.to_owned();
            loop {
                let offset: usize = hint
                    .split("offset=")
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                let page = render_server_listing(name, catalogue, None, offset, 128, true, None);
                seen.extend(uris(&page));
                match page.lines().find(|l| l.contains("more matches")) {
                    Some(next) => hint = next.to_owned(),
                    None => break,
                }
            }
        }
        seen
    }

    #[test]
    fn paging_by_the_printed_hints_visits_every_resource_of_every_server() {
        let servers = [("a", catalogue_of("a", 300)), ("b", catalogue_of("b", 300))];
        let seen = walk(&servers);
        assert_eq!(seen.len(), 600);
        let unique: std::collections::HashSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), 600, "no resource shown twice or skipped");
        // A scoped call needs no server in its hint.
        let scoped = render_server_listing("a", &servers[0].1, None, 0, 128, true, None);
        assert!(
            scoped.contains("call again with offset=128 to continue"),
            "{scoped}"
        );
    }

    // mini-agent-xhvu5: an offset past the matches does not print an inverted range.
    #[test]
    fn listing_past_the_end_reports_an_empty_range_and_a_reset_hint() {
        let resource = |n: usize| {
            serde_json::from_value::<rmcp::model::Resource>(serde_json::json!({
                "uri": format!("mem://r{n}"), "name": format!("r{n}")
            }))
            .unwrap()
        };
        let catalogue = Catalogue {
            resources: (0..5).map(resource).collect(),
            ..Catalogue::default()
        };
        let past = render_server_listing("gh", &catalogue, None, 50, 128, true, None);
        assert!(past.contains("showing 0-0 of 5 matching"), "{past}");
        assert!(past.contains("offset=0"), "{past}");
        assert!(!past.contains("mem://r"), "{past}");
        let inside = render_server_listing("gh", &catalogue, None, 2, 128, true, None);
        assert!(inside.contains("showing 3-5 of 5 matching"), "{inside}");
    }

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
