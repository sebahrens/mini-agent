//! Parent-side bridge from the brokered `call_tool` effect to the agent's tool server.
//!
//! The JavaScript worker never sees this module: it sends a closed typed `CallTool` request and
//! the parent broker validates it against the invocation grant, then dispatches through the same
//! tool server (and therefore the same per-tool permission checks, MCP clients and result
//! shaping) a model-issued call uses. Only read-only tools are reachable: a fixed set of
//! built-ins plus MCP tools that declare `readOnlyHint`.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use rig::tool::server::WeakToolServerHandle;

/// Built-in tools a script may call: they read workspace, catalogue or network state and never
/// change the workspace. Interactive or costly tools (`ask_user_question`, `advisor`,
/// `schedule_wakeup`) and everything that can mutate (`edit`, `write`, `shell`, `git`, `task`)
/// are deliberately absent.
pub(crate) const CALLABLE_BUILTIN_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find_files",
    "list_dir",
    "lsp_navigate",
    "lsp_diagnostics",
    "web_search",
    "web_fetch",
    "memory_read",
    "memory_search",
    "list_mcp_resources",
    "read_mcp_resource",
    "job_status",
    "task_evidence",
    "todo_read",
];

/// How the bridge classifies a requested tool name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolAccess {
    /// Registered and read-only: callable.
    Allowed,
    /// Registered, but not in the read-only set.
    NotReadOnly,
    /// No tool of that name is registered.
    Unknown,
}

/// Why a dispatched call did not produce a result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ToolInvokeFailure {
    /// The owning agent (tool server) has been dropped.
    Retired,
    /// The tool ran and reported an error; the text is its own message.
    Failed(String),
}

pub(crate) type ToolInvokeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, ToolInvokeFailure>> + Send + 'a>>;

/// What the broker needs from the agent: classification and dispatch. Production binds the real
/// tool server; tests bind a deterministic fake.
pub(crate) trait ToolInvoker: Send + Sync {
    fn classify(&self, name: &str) -> ToolAccess;
    fn invoke<'a>(&'a self, name: &'a str, arguments_json: &'a str) -> ToolInvokeFuture<'a>;
}

/// Late-bound handle: the `js` tool is registered before the tool server exists, so the agent
/// builder binds the invoker once after the agent is built (as the MCP search tool does).
#[derive(Clone, Default)]
pub(crate) struct ToolBridge(Arc<OnceLock<Arc<dyn ToolInvoker>>>);

impl ToolBridge {
    pub(crate) fn bind(&self, invoker: Arc<dyn ToolInvoker>) -> Result<(), &'static str> {
        self.0
            .set(invoker)
            .map_err(|_| "JavaScript tool bridge already bound")
    }

    pub(crate) fn invoker(&self) -> Option<&Arc<dyn ToolInvoker>> {
        self.0.get()
    }
}

/// Invoker over the real tool server.
pub(crate) struct ServerToolInvoker {
    server: WeakToolServerHandle,
    /// Every registered tool name at bind time.
    registered: BTreeSet<String>,
    /// Registered tools a script may call (read-only built-ins and read-only MCP tools).
    callable: BTreeSet<String>,
}

impl ServerToolInvoker {
    pub(crate) fn new(
        server: WeakToolServerHandle,
        registered: impl IntoIterator<Item = String>,
        read_only_mcp: impl IntoIterator<Item = String>,
    ) -> Self {
        let registered: BTreeSet<String> = registered.into_iter().collect();
        let callable = registered
            .iter()
            .filter(|name| CALLABLE_BUILTIN_TOOLS.contains(&name.as_str()))
            .cloned()
            .chain(read_only_mcp)
            .collect();
        Self {
            server,
            registered,
            callable,
        }
    }
}

impl ToolInvoker for ServerToolInvoker {
    fn classify(&self, name: &str) -> ToolAccess {
        if self.callable.contains(name) {
            ToolAccess::Allowed
        } else if self.registered.contains(name) {
            ToolAccess::NotReadOnly
        } else {
            ToolAccess::Unknown
        }
    }

    fn invoke<'a>(&'a self, name: &'a str, arguments_json: &'a str) -> ToolInvokeFuture<'a> {
        Box::pin(async move {
            let server = self.server.upgrade().ok_or(ToolInvokeFailure::Retired)?;
            server
                .call_tool(name, arguments_json)
                .await
                .map_err(|error| ToolInvokeFailure::Failed(error.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callable_set_never_contains_a_mutating_or_interactive_tool() {
        for forbidden in [
            "edit",
            "write",
            "shell",
            "git",
            "task",
            "notebook_edit",
            "ask_user_question",
            "advisor",
            "schedule_wakeup",
            "js",
        ] {
            assert!(
                !CALLABLE_BUILTIN_TOOLS.contains(&forbidden),
                "{forbidden} must not be callable from a script"
            );
        }
        for name in CALLABLE_BUILTIN_TOOLS {
            assert_ne!(
                crate::agent::tools::tool_class(name),
                crate::agent::tools::ToolClass::Mutating,
                "{name} must be classified read-only"
            );
        }
    }
}
