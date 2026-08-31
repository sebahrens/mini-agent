pub mod client;
pub mod config;
pub mod oauth;
pub mod tool;

use std::collections::HashMap;
use std::sync::Arc;

use compact_str::CompactString;
use tool::McpTool;

use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

/// Maximum concurrent MCP server connection attempts. Limits the number of
/// processes and network connections opened simultaneously during startup.
const MAX_MCP_CONNECT_CONCURRENCY: usize = 8;

/// Maximum concurrent tool-list RPCs across connected MCP servers.
const MAX_MCP_TOOL_CONCURRENCY: usize = 8;

pub struct McpClientManager {
    pub handles: Vec<client::McpClientHandle>,
    /// Connection failures collected during `connect_all`, to be surfaced by the
    /// TUI via the renderer. We do NOT log these at `warn` because that writes to
    /// stderr, which corrupts the alt-screen TUI (overlapping the input box).
    pub notices: Vec<CompactString>,
}

impl McpClientManager {
    pub(crate) async fn connect_all_in_binding(
        configs: &HashMap<String, config::McpServerConfig>,
        workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        if let Err(error) = workspace.validate() {
            return Self {
                handles: Vec::new(),
                notices: vec![CompactString::new(format!(
                    "MCP workspace is no longer valid: {error}"
                ))],
            };
        }
        Self::connect_all_in(configs, workspace.root()).await
    }

    async fn connect_all_in(
        configs: &HashMap<String, config::McpServerConfig>,
        workspace: &std::path::Path,
    ) -> Self {
        tracing::debug!("MCP connecting to {} servers", configs.len());
        if configs.is_empty() {
            return Self {
                handles: Vec::new(),
                notices: Vec::new(),
            };
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
        Self { handles, notices }
    }

    /// Drain and return any pending connection notices.
    pub fn take_notices(&mut self) -> Vec<CompactString> {
        std::mem::take(&mut self.notices)
    }

    pub async fn collect_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
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
                let permission = permission.clone();
                let ask_tx = ask_tx.clone();
                async move {
                    let _permit = sem.acquire_owned().await;
                    let result = peer.list_all_tools().await;
                    (
                        server_name,
                        trusted_identity,
                        peer,
                        permission,
                        ask_tx,
                        result,
                    )
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;

        let mut all_tools = Vec::new();
        for (server_name, trusted_identity, peer, permission, ask_tx, result) in results {
            match result {
                Ok(tools) => {
                    tracing::debug!("MCP server '{}': {} tools listed", server_name, tools.len(),);
                    for definition in tools {
                        all_tools.push(McpTool {
                            server_name: server_name.clone(),
                            trusted_identity,
                            definition,
                            peer: peer.clone(),
                            permission: permission.clone(),
                            ask_tx: ask_tx.clone(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to list tools from MCP server '{}': {e}",
                        server_name
                    );
                }
            }
        }
        all_tools
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
        let manager = McpClientManager {
            handles: Vec::new(),
            notices: Vec::new(),
        };
        let tools = manager.collect_tools(None, None).await;
        assert!(tools.is_empty());
    }
}
