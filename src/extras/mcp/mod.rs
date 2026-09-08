pub mod client;
pub mod config;
pub mod oauth;
pub mod tool;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use compact_str::CompactString;
use tool::McpTool;

use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

/// Maximum concurrent MCP server connection attempts. Limits the number of
/// processes and network connections opened simultaneously during startup.
const MAX_MCP_CONNECT_CONCURRENCY: usize = 8;

/// Maximum concurrent tool-list RPCs across connected MCP servers.
const MAX_MCP_TOOL_CONCURRENCY: usize = 8;

/// Default bound on one MCP `tools/call` round trip (`mcp_tool_timeout_secs`).
pub const DEFAULT_MCP_TOOL_TIMEOUT_SECS: u64 = 120;

/// Time budgets applied while collecting and invoking MCP tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpToolTimeouts {
    /// Whole `tools/list` enumeration for one server, all pages included.
    pub list: Duration,
    /// One `tools/call` round trip.
    pub call: Duration,
}

impl Default for McpToolTimeouts {
    fn default() -> Self {
        Self {
            list: client::MCP_LIST_TOOLS_TIMEOUT,
            call: Duration::from_secs(DEFAULT_MCP_TOOL_TIMEOUT_SECS),
        }
    }
}

impl McpToolTimeouts {
    /// Apply the configured `mcp_tool_timeout_secs` (clamped to at least one
    /// second so a zero can never disable the bound) over the defaults.
    pub fn from_config_secs(call_secs: Option<u64>) -> Self {
        Self {
            call: Duration::from_secs(call_secs.unwrap_or(DEFAULT_MCP_TOOL_TIMEOUT_SECS).max(1)),
            ..Self::default()
        }
    }
}

pub struct McpClientManager {
    pub handles: Vec<client::McpClientHandle>,
    /// Connection failures collected during `connect_all`, to be surfaced by the
    /// TUI via the renderer. We do NOT log these at `warn` because that writes to
    /// stderr, which corrupts the alt-screen TUI (overlapping the input box).
    pub notices: Vec<CompactString>,
    /// Notices produced while collecting tools (`tools/list` failures, tool
    /// name collisions). Collection runs through a shared reference during
    /// agent construction, so these are kept behind a mutex and drained
    /// together with `notices` by [`Self::take_notices`].
    tool_notices: Mutex<Vec<CompactString>>,
    /// Opaque private-storage scope shared by every tool collected from this
    /// manager. MCP calls do not own a durable mini-agent Session, so their
    /// oversized result artifacts use a process-manager scope instead.
    spill_scope: CompactString,
}

impl McpClientManager {
    /// Wrap already-connected handles. Handles should be sorted by server name
    /// so tool ordering and collision handling stay deterministic.
    pub fn from_handles(handles: Vec<client::McpClientHandle>) -> Self {
        Self {
            handles,
            notices: Vec::new(),
            tool_notices: Mutex::new(Vec::new()),
            spill_scope: CompactString::new(uuid::Uuid::new_v4().to_string()),
        }
    }

    fn with_notices(handles: Vec<client::McpClientHandle>, notices: Vec<CompactString>) -> Self {
        Self {
            handles,
            notices,
            tool_notices: Mutex::new(Vec::new()),
            spill_scope: CompactString::new(uuid::Uuid::new_v4().to_string()),
        }
    }

    fn push_tool_notice(&self, notice: String) {
        // Info lands in the log file, never on stderr, so the TUI stays clean.
        tracing::info!("{notice}");
        self.tool_notices
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(CompactString::new(notice));
    }

    pub(crate) async fn connect_all_in_binding(
        configs: &HashMap<String, config::McpServerConfig>,
        workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        if let Err(error) = workspace.validate() {
            return Self::with_notices(
                Vec::new(),
                vec![CompactString::new(format!(
                    "MCP workspace is no longer valid: {error}"
                ))],
            );
        }
        Self::connect_all_in(configs, workspace.root()).await
    }

    async fn connect_all_in(
        configs: &HashMap<String, config::McpServerConfig>,
        workspace: &std::path::Path,
    ) -> Self {
        tracing::debug!("MCP connecting to {} servers", configs.len());
        if configs.is_empty() {
            return Self::from_handles(Vec::new());
        }

        // Collect and sort by name so handles/notices are deterministic
        // regardless of HashMap seed or completion order.
        let mut sorted: Vec<(String, config::McpServerConfig)> = configs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let workspace_path = workspace.to_path_buf();
        let sem = Arc::new(tokio::sync::Semaphore::new(MAX_MCP_CONNECT_CONCURRENCY));

        // Build futures in sorted order so join_all returns results in the
        // same order, giving stable handle position regardless of which
        // connections finish first.
        let futs: Vec<_> = sorted
            .into_iter()
            .map(|(name, cfg)| {
                let sem = sem.clone();
                let workspace = workspace_path.clone();
                async move {
                    let _permit = sem.acquire_owned().await;
                    let result = client::McpClientHandle::connect_in(
                        CompactString::new(name.clone()),
                        &cfg,
                        &workspace,
                    )
                    .await;
                    (name, result)
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        let mut handles = Vec::with_capacity(results.len());
        let mut notices = Vec::new();
        for (name, result) in results {
            match result {
                Ok(handle) => {
                    tracing::info!("Connected to MCP server '{}'", name);
                    handles.push(handle);
                }
                Err(e) => {
                    tracing::debug!("Failed to connect to MCP server '{}': {e}", name);
                    notices.push(CompactString::new(format!(
                        "MCP server '{name}' not connected: {e}"
                    )));
                }
            }
        }
        Self::with_notices(handles, notices)
    }

    /// Drain and return any pending connection and tool-collection notices.
    pub fn take_notices(&mut self) -> Vec<CompactString> {
        let mut notices = std::mem::take(&mut self.notices);
        notices.append(
            self.tool_notices
                .get_mut()
                .unwrap_or_else(|error| error.into_inner()),
        );
        notices
    }

    /// Collect tools from every connected server with the default
    /// [`McpToolTimeouts`].
    pub async fn collect_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<McpTool> {
        self.collect_tools_with_timeouts(permission, ask_tx, McpToolTimeouts::default())
            .await
    }

    /// Collect tools from every connected server.
    ///
    /// Each server's `tools/list` is bounded by `timeouts.list`, so one hung
    /// server only costs its own budget and never blocks the others. Failures
    /// and timeouts become notices (see [`Self::take_notices`]) rather than
    /// `warn` logs, which would write to stderr under the alt-screen TUI.
    ///
    /// Tool names must be unique across the whole tool set; a duplicate would
    /// silently replace its predecessor downstream. When two servers expose the
    /// same name, every colliding tool is registered as `<server>__<tool>` and
    /// a notice lists the renames. The permission key keeps the bare name.
    pub async fn collect_tools_with_timeouts(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        timeouts: McpToolTimeouts,
    ) -> Vec<McpTool> {
        tracing::debug!("MCP collecting tools from {} handles", self.handles.len());
        if self.handles.is_empty() {
            return Vec::new();
        }

        let sem = Arc::new(tokio::sync::Semaphore::new(MAX_MCP_TOOL_CONCURRENCY));

        // Clone per-handle state (Peer is an Arc wrapper; cloning is cheap).
        // Futures are built in handle order, which is already sorted by server
        // name from connect_all_in, so join_all yields stable server ordering.
        let futs: Vec<_> = self
            .handles
            .iter()
            .map(|handle| {
                let sem = sem.clone();
                let peer = handle.peer();
                let server_name = handle.server_name.clone();
                let trusted_identity = handle.trusted_identity;
                async move {
                    let _permit = sem.acquire_owned().await;
                    let result = client::list_all_tools_bounded(&peer, timeouts.list).await;
                    (server_name, trusted_identity, peer, result)
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        let mut all_tools = Vec::new();
        for (server_name, trusted_identity, peer, result) in results {
            match result {
                Ok(tools) => {
                    tracing::debug!("MCP server '{}': {} tools listed", server_name, tools.len(),);
                    let reached_tool_cap = tools.len() == client::MCP_LIST_TOOLS_MAX_TOOLS;
                    let mut truncated_descriptions = 0usize;
                    let mut rejected_schemas = 0usize;
                    for definition in tools {
                        let (model_description, model_parameters, description_truncated) =
                            match McpTool::bounded_model_metadata(&definition) {
                                Ok(metadata) => metadata,
                                Err(_) => {
                                    rejected_schemas += 1;
                                    continue;
                                }
                            };
                        truncated_descriptions += usize::from(description_truncated);
                        all_tools.push(McpTool {
                            server_name: server_name.clone(),
                            trusted_identity,
                            registered_name: CompactString::new(definition.name.as_ref()),
                            definition,
                            model_description,
                            model_parameters,
                            peer: peer.clone(),
                            permission: permission.clone(),
                            ask_tx: ask_tx.clone(),
                            call_timeout: timeouts.call,
                            spill_scope: self.spill_scope.clone(),
                        });
                    }
                    if reached_tool_cap {
                        self.push_tool_notice(format!(
                            "MCP server '{server_name}' tool catalog is limited to at most {} tools",
                            client::MCP_LIST_TOOLS_MAX_TOOLS,
                        ));
                    }
                    if truncated_descriptions > 0 {
                        self.push_tool_notice(format!(
                            "MCP server '{server_name}' had {truncated_descriptions} tool description(s) truncated to {} bytes",
                            tool::MCP_TOOL_DESCRIPTION_MAX_BYTES,
                        ));
                    }
                    if rejected_schemas > 0 {
                        self.push_tool_notice(format!(
                            "MCP server '{server_name}' had {rejected_schemas} tool(s) omitted because their input schema was invalid or exceeded {} bytes",
                            tool::MCP_TOOL_SCHEMA_MAX_BYTES,
                        ));
                    }
                }
                Err(rmcp::ServiceError::Timeout { .. }) => {
                    tracing::debug!(
                        "MCP server '{}': tools/list timed out after {} ms",
                        server_name,
                        timeouts.list.as_millis()
                    );
                    self.push_tool_notice(format!(
                        "MCP server '{server_name}' tools unavailable: tools/list timed out after {} ms",
                        timeouts.list.as_millis()
                    ));
                }
                Err(e) => {
                    tracing::debug!(
                        "Failed to list tools from MCP server '{}': {e}",
                        server_name
                    );
                    self.push_tool_notice(format!(
                        "MCP server '{server_name}' tools unavailable: tools/list failed: {e}"
                    ));
                }
            }
        }
        self.namespace_duplicate_tool_names(&mut all_tools);
        all_tools
    }

    /// Allocate one final registered name per tool against a single set of
    /// names already in use.
    ///
    /// A bare name exposed by more than one server becomes `<server>__<tool>`,
    /// but that generated name can equal another server's unchanged name, or
    /// another generated name (`a__b` + tool `c` collides with server `a` +
    /// tool `b__c`). Allocating against the complete used-name set — unchanged
    /// names first, then generated ones in sorted server order — keeps the
    /// mapping injective, so a requested tool can never route to the wrong
    /// server. Permission identities keep the bare tool name.
    fn namespace_duplicate_tool_names(&self, tools: &mut [McpTool]) {
        let entries: Vec<(String, String)> = tools
            .iter()
            .map(|tool| {
                (
                    tool.server_name.to_string(),
                    tool.definition.name.to_string(),
                )
            })
            .collect();
        let allocated = allocate_registered_tool_names(&entries);
        let mut renamed: Vec<String> = Vec::new();
        for (tool, name) in tools.iter_mut().zip(allocated) {
            if tool.registered_name.as_str() != name {
                tool.registered_name = CompactString::new(&name);
                renamed.push(name);
            }
        }
        if renamed.is_empty() {
            return;
        }
        let mut notice = String::from("MCP tool name collision; registered as ");
        notice.push_str(&renamed.join(", "));
        notice.push_str(" (permission keys keep the bare tool name)");
        self.push_tool_notice(notice);
    }

    /// (Re)connect a single server, replacing any existing handle for it.
    /// Used after an interactive OAuth login so the server's tools become
    /// available without restarting the session.
    async fn reconnect_in(
        &mut self,
        name: &str,
        cfg: &config::McpServerConfig,
        workspace: &std::path::Path,
    ) -> anyhow::Result<()> {
        tracing::info!("MCP reconnecting server '{}'", name);
        // Connect first. A failed replacement must leave the currently usable
        // connection installed; callers can retry without restarting the app.
        let handle =
            client::McpClientHandle::connect_in(CompactString::new(name), cfg, workspace).await?;
        let previous = self
            .handles
            .iter()
            .position(|existing| existing.server_name == name)
            .map(|index| self.handles.remove(index));
        self.handles.push(handle);
        self.handles
            .sort_unstable_by(|left, right| left.server_name.cmp(&right.server_name));
        if let Some(mut previous) = previous {
            let _ = previous.running_service.close().await;
        }
        Ok(())
    }

    pub(crate) async fn reconnect_in_binding(
        &mut self,
        name: &str,
        cfg: &config::McpServerConfig,
        workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> anyhow::Result<()> {
        workspace.validate().map_err(anyhow::Error::msg)?;
        self.reconnect_in(name, cfg, workspace.root()).await
    }

    pub async fn shutdown(self) {
        tracing::debug!("MCP shutting down {} connections", self.handles.len());
        for mut handle in self.handles {
            let name = handle.server_name.clone();
            // Explicitly shut down the running service so child processes and
            // HTTP connections are cleaned up properly, rather than relying on
            // Drop which may not await teardown.
            let _ = handle.running_service.close().await;
            tracing::debug!("Disconnected from MCP server '{}'", name);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::extras::mcp::config::{McpServerConfig, McpStdioNetwork};

    /// Returns a Command config pointing at a binary that exits immediately
    /// with a non-zero code (simulates a server that refuses to connect).
    fn failing_config() -> McpServerConfig {
        McpServerConfig::Command {
            command: if cfg!(windows) {
                "cmd".to_string()
            } else {
                "false".to_string()
            },
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            inherit_env: Vec::new(),
            sandbox: None,
            network: McpStdioNetwork::Inherit,
        }
    }

    #[tokio::test]
    async fn connect_all_in_empty_configs_returns_empty_manager() {
        let configs = HashMap::new();
        let workspace = std::env::current_dir().unwrap();
        let manager = McpClientManager::connect_all_in(&configs, &workspace).await;
        assert!(manager.handles.is_empty());
        assert!(manager.notices.is_empty());
    }

    #[tokio::test]
    async fn connect_all_in_failing_servers_produce_notices_not_panics() {
        let mut configs = HashMap::new();
        configs.insert("alpha".to_string(), failing_config());
        configs.insert("beta".to_string(), failing_config());
        let workspace = std::env::current_dir().unwrap();
        let manager = McpClientManager::connect_all_in(&configs, &workspace).await;
        // Both fail: no handles, two notices in stable (sorted) name order.
        assert!(manager.handles.is_empty());
        assert_eq!(manager.notices.len(), 2);
        assert!(
            manager.notices[0].contains("alpha"),
            "first notice should name alpha"
        );
        assert!(
            manager.notices[1].contains("beta"),
            "second notice should name beta"
        );
    }

    #[tokio::test]
    async fn connect_all_in_stable_notice_order_independent_of_map_iteration() {
        // Insert keys in a non-alphabetical order to confirm HashMap iteration
        // order does not leak through to the output.
        let mut configs = HashMap::new();
        configs.insert("zeta".to_string(), failing_config());
        configs.insert("alpha".to_string(), failing_config());
        configs.insert("mu".to_string(), failing_config());
        let workspace = std::env::current_dir().unwrap();
        let manager = McpClientManager::connect_all_in(&configs, &workspace).await;
        assert_eq!(manager.notices.len(), 3);
        assert!(manager.notices[0].contains("alpha"));
        assert!(manager.notices[1].contains("mu"));
        assert!(manager.notices[2].contains("zeta"));
    }

    #[tokio::test]
    async fn collect_tools_empty_handles_returns_empty_vec() {
        let manager = McpClientManager::from_handles(Vec::new());
        let tools = manager.collect_tools(None, None).await;
        assert!(tools.is_empty());
    }

    #[test]
    fn tool_timeouts_apply_config_and_clamp_zero() {
        assert_eq!(
            McpToolTimeouts::from_config_secs(None).call,
            std::time::Duration::from_secs(DEFAULT_MCP_TOOL_TIMEOUT_SECS)
        );
        assert_eq!(
            McpToolTimeouts::from_config_secs(Some(7)).call,
            std::time::Duration::from_secs(7)
        );
        assert_eq!(
            McpToolTimeouts::from_config_secs(Some(0)).call,
            std::time::Duration::from_secs(1)
        );
        assert_eq!(
            McpToolTimeouts::from_config_secs(Some(7)).list,
            McpToolTimeouts::default().list
        );
    }

    #[test]
    fn take_notices_drains_tool_notices_once() {
        let mut manager = McpClientManager::from_handles(Vec::new());
        manager.push_tool_notice("first".to_string());
        assert_eq!(manager.take_notices(), vec![CompactString::new("first")]);
        assert!(manager.take_notices().is_empty());
    }

    fn allocate(entries: &[(&str, &str)]) -> Vec<String> {
        let owned: Vec<(String, String)> = entries
            .iter()
            .map(|(server, tool)| ((*server).to_string(), (*tool).to_string()))
            .collect();
        allocate_registered_tool_names(&owned)
    }

    fn assert_unique(names: &[String]) {
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate registered names: {names:?}"
        );
    }

    #[test]
    fn unique_bare_names_are_registered_unchanged() {
        let names = allocate(&[("alpha", "one"), ("beta", "two")]);
        assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn a_generated_name_never_collides_with_an_unchanged_one() {
        // alpha/probe and beta/probe both namespace to `<server>__probe`, while
        // gamma already exposes a unique tool literally called `alpha__probe`.
        let names = allocate(&[
            ("alpha", "probe"),
            ("beta", "probe"),
            ("gamma", "alpha__probe"),
        ]);
        assert_unique(&names);
        assert_eq!(names[2], "alpha__probe", "an unchanged name keeps its form");
        assert_ne!(names[0], names[2]);
        assert_eq!(names[1], "beta__probe");
    }

    #[test]
    fn separator_collisions_between_generated_names_are_disambiguated() {
        // server `a__b` + tool `c` and server `a` + tool `b__c` both want
        // `a__b__c` once each bare name is shared.
        let names = allocate(&[
            ("a__b", "c"),
            ("other", "c"),
            ("a", "b__c"),
            ("another", "b__c"),
        ]);
        assert_unique(&names);
        assert!(names.iter().all(|name| !name.is_empty()));
    }

    #[test]
    fn a_repeated_definition_from_one_server_still_gets_a_distinct_name() {
        let names = allocate(&[("alpha", "probe"), ("alpha", "probe")]);
        assert_unique(&names);
    }

    #[test]
    fn registered_names_stay_within_provider_limits() {
        let long_server = "s".repeat(50);
        let long_tool = "t".repeat(50);
        let names = allocate(&[
            (long_server.as_str(), long_tool.as_str()),
            ("other", long_tool.as_str()),
        ]);
        assert_unique(&names);
        for name in &names {
            assert!(name.len() <= MAX_REGISTERED_TOOL_NAME, "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{name}"
            );
        }
    }

    #[test]
    fn allocation_is_deterministic_for_the_same_catalog() {
        let entries = [
            ("alpha", "probe"),
            ("beta", "probe"),
            ("gamma", "alpha__probe"),
            ("alpha", "probe"),
        ];
        assert_eq!(allocate(&entries), allocate(&entries));
    }
}

/// Longest tool name accepted by the strictest supported provider schema.
const MAX_REGISTERED_TOOL_NAME: usize = 64;

/// Final registered name for every `(server, bare tool)` pair, in input order.
///
/// A bare name exposed by more than one server is namespaced as
/// `<server>__<tool>`, but that generated name can equal another server's
/// unchanged name, or another generated name (`a__b` + tool `c` collides with
/// server `a` + tool `b__c`). Names are therefore allocated against one used
/// set: unchanged names are reserved first, then generated names in input
/// order, each disambiguated if it is already taken. The result is injective,
/// so the collected catalog never registers one name twice and a requested
/// tool cannot route to the wrong server.
fn allocate_registered_tool_names(entries: &[(String, String)]) -> Vec<String> {
    let mut owners: HashMap<&str, Vec<&str>> = HashMap::new();
    for (server, bare) in entries {
        let servers = owners.entry(bare.as_str()).or_default();
        if !servers.contains(&server.as_str()) {
            servers.push(server.as_str());
        }
    }

    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut names: Vec<Option<String>> = vec![None; entries.len()];
    // Reserve every name that keeps its bare form first, so a generated name
    // can never displace one.
    for (index, (_, bare)) in entries.iter().enumerate() {
        let shared = owners.get(bare.as_str()).is_some_and(|s| s.len() > 1);
        if !shared && used.insert(bare.clone()) {
            names[index] = Some(bare.clone());
        }
    }
    for (index, (server, bare)) in entries.iter().enumerate() {
        if names[index].is_some() {
            continue;
        }
        let candidate = McpTool::namespaced_name(server, bare).to_string();
        names[index] = Some(allocate_registered_name(candidate, &mut used));
    }
    names
        .into_iter()
        .map(|name| name.expect("every entry is allocated"))
        .collect()
}

/// Reduce `name` to the character set every supported provider accepts and
/// make it unique within `used`, reserving the result.
fn allocate_registered_name(name: String, used: &mut std::collections::HashSet<String>) -> String {
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    let base = truncate_registered_name(&sanitized, MAX_REGISTERED_TOOL_NAME);
    if used.insert(base.clone()) {
        return base;
    }
    for suffix in 2..u32::MAX {
        let tail = format!("_{suffix}");
        let head = truncate_registered_name(&base, MAX_REGISTERED_TOOL_NAME - tail.len());
        let candidate = format!("{head}{tail}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("a unique registered tool name is always reachable")
}

/// Truncate on a character boundary; the sanitizer already removed every
/// non-ASCII character, but keep this total for any future relaxation.
fn truncate_registered_name(name: &str, limit: usize) -> String {
    if name.len() <= limit {
        return name.to_string();
    }
    let mut end = limit;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}
