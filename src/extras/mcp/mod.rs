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
}

impl McpClientManager {
    /// Wrap already-connected handles. Handles should be sorted by server name
    /// so tool ordering and collision handling stay deterministic.
    pub fn from_handles(handles: Vec<client::McpClientHandle>) -> Self {
        Self {
            handles,
            notices: Vec::new(),
            tool_notices: Mutex::new(Vec::new()),
        }
    }

    fn with_notices(handles: Vec<client::McpClientHandle>, notices: Vec<CompactString>) -> Self {
        Self {
            handles,
            notices,
            tool_notices: Mutex::new(Vec::new()),
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
                    for definition in tools {
                        all_tools.push(McpTool {
                            server_name: server_name.clone(),
                            trusted_identity,
                            registered_name: CompactString::new(definition.name.as_ref()),
                            definition,
                            peer: peer.clone(),
                            permission: permission.clone(),
                            ask_tx: ask_tx.clone(),
                            call_timeout: timeouts.call,
                        });
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

    /// Rename every tool whose bare name is exposed by more than one server to
    /// `<server>__<tool>`. Deterministic because `tools` follows the sorted
    /// server order established by `connect_all_in`.
    fn namespace_duplicate_tool_names(&self, tools: &mut [McpTool]) {
        let mut owners: HashMap<String, Vec<CompactString>> = HashMap::new();
        for tool in tools.iter() {
            let servers = owners.entry(tool.definition.name.to_string()).or_default();
            if !servers.contains(&tool.server_name) {
                servers.push(tool.server_name.clone());
            }
        }
        let mut colliding: Vec<String> = owners
            .into_iter()
            .filter(|(_, servers)| servers.len() > 1)
            .map(|(name, _)| name)
            .collect();
        colliding.sort_unstable();
        if colliding.is_empty() {
            return;
        }

        let mut notice = String::from("MCP tool name collision; registered as ");
        let mut first = true;
        for tool in tools.iter_mut() {
            let bare = tool.definition.name.as_ref();
            if !colliding.iter().any(|name| name == bare) {
                continue;
            }
            tool.registered_name = McpTool::namespaced_name(&tool.server_name, bare);
            if !first {
                notice.push_str(", ");
            }
            first = false;
            notice.push_str(&tool.registered_name);
        }
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
        // Command servers commonly own an exclusive local resource. Stop and
        // reap the old tree before initializing its replacement; otherwise the
        // replacement can fail on a port, socket, lock, or PID-file collision.
        // Remote HTTP transports keep transactional connect-before-replace
        // behavior because they do not own a local service process.
        if matches!(cfg, config::McpServerConfig::Command { .. })
            && let Some(index) = self
                .handles
                .iter()
                .position(|handle| handle.server_name == name)
        {
            let mut previous = self.handles.remove(index);
            let _ = previous.running_service.close().await;
        }
        let handle =
            client::McpClientHandle::connect_in(CompactString::new(name), cfg, workspace).await?;
        self.handles.retain(|h| h.server_name != name);
        self.handles.push(handle);
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
}
