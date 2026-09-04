pub(crate) mod bash;
pub(crate) mod crc;
pub(crate) mod edit;
pub(crate) mod find_files;
pub(crate) mod grep;
pub(crate) mod list_dir;
#[cfg(feature = "lsp")]
pub(crate) mod lsp;
pub(crate) mod memoize;
pub(crate) mod normalize;
pub(crate) mod read;
pub(crate) mod todo;
pub(crate) mod write;

pub(crate) use normalize::{levenshtein_similarity, normalize_whitespace};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use crate::config::types::EditSystem;

static EDIT_SYSTEM: Mutex<EditSystem> = Mutex::new(EditSystem::Similarity);

pub(crate) fn set_edit_system(es: EditSystem) {
    *EDIT_SYSTEM.lock().unwrap_or_else(|e| e.into_inner()) = es;
}

pub(crate) fn edit_system() -> EditSystem {
    *EDIT_SYSTEM.lock().unwrap_or_else(|e| e.into_inner())
}

/// Resolve a tool path against the immutable workspace selected when the
/// agent was built. Absolute and home-relative paths retain their historical
/// meaning; only relative paths are workspace-bound.
pub(crate) fn resolve_tool_path(workspace_root: Option<&Path>, path: &str) -> PathBuf {
    let expanded = PathBuf::from(crate::fs::expand_tilde(path));
    if expanded.is_absolute() {
        expanded
    } else if let Some(root) = workspace_root {
        root.join(expanded)
    } else {
        expanded
    }
}

pub(crate) fn capture_workspace_binding(root: PathBuf) -> Arc<crate::paths::WorkspaceBinding> {
    Arc::new(
        crate::paths::WorkspaceBinding::capture(&root)
            .expect("agent workspace must exist while tools are constructed"),
    )
}

pub(crate) fn validate_workspace_binding(
    workspace: Option<&Arc<crate::paths::WorkspaceBinding>>,
) -> Result<Option<PathBuf>, ToolError> {
    if let Some(workspace) = workspace {
        workspace.validate().map_err(ToolError::Msg)?;
        Ok(Some(workspace.root().to_path_buf()))
    } else {
        Ok(None)
    }
}

/// Repeated-read policy and history owned by one built agent/session.
///
/// Clones share history only when deliberately injected into tools belonging
/// to the same build. Constructing another tracker creates an independent
/// policy boundary, even within the same process.
#[derive(Clone, Debug)]
pub(crate) struct ReadTracker {
    deny_repeated_reads: bool,
    tracked: std::sync::Arc<Mutex<Vec<TrackedRead>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReadVersion {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl ReadVersion {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedRead {
    path: String,
    offset: usize,
    limit: usize,
    version: ReadVersion,
}

impl Default for ReadTracker {
    fn default() -> Self {
        Self::new(true)
    }
}

impl ReadTracker {
    pub(crate) fn new(deny_repeated_reads: bool) -> Self {
        Self {
            deny_repeated_reads,
            tracked: std::sync::Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn check_read(
        &self,
        path: &str,
        offset: usize,
        limit: usize,
        metadata: &std::fs::Metadata,
    ) -> Option<String> {
        if !self.deny_repeated_reads {
            return None;
        }
        let version = ReadVersion::from_metadata(metadata);
        let tracked = self.tracked.lock().unwrap_or_else(|e| e.into_inner());
        if !tracked.iter().any(|entry| {
            entry.path == path
                && entry.offset == offset
                && entry.limit == limit
                && entry.version == version
        }) {
            return None;
        }
        let end = offset + limit;
        Some(format!(
            "read blocked: {path} (lines {}-{}) was already read and has not been modified since. Use the previous result or read a different section.",
            offset + 1,
            if end > 0 { end } else { offset + 1 }
        ))
    }

    pub(crate) fn record_read(
        &self,
        path: &str,
        offset: usize,
        limit: usize,
        metadata: &std::fs::Metadata,
    ) {
        if !self.deny_repeated_reads {
            return;
        }
        let mut tracked = self.tracked.lock().unwrap_or_else(|e| e.into_inner());
        tracked
            .retain(|entry| entry.path != path || entry.offset != offset || entry.limit != limit);
        tracked.push(TrackedRead {
            path: path.to_string(),
            offset,
            limit,
            version: ReadVersion::from_metadata(metadata),
        });
    }

    #[cfg(test)]
    pub(crate) fn track_read(&self, path: &str, offset: usize, limit: usize) -> Option<String> {
        let metadata = std::fs::metadata(".").expect("test process has a current directory");
        let blocked = self.check_read(path, offset, limit, &metadata);
        if blocked.is_none() {
            self.record_read(path, offset, limit, &metadata);
        }
        blocked
    }

    pub(crate) fn untrack_read_path(&self, path: &str) {
        let mut tracked = self.tracked.lock().unwrap_or_else(|e| e.into_inner());
        tracked.retain(|entry| entry.path != path);
    }
}

pub(crate) fn combine_coaching(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) if first == second => Some(first),
        (Some(first), Some(second)) => Some(format!("{first}\n\n{second}")),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
pub use bash::BashTool;
pub use bash::ShellTool;
pub use edit::EditTool;
pub use find_files::FindFilesTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
pub use read::ReadTool;
pub use todo::WriteTodoList;
pub use write::WriteTool;

use std::io;

use compact_str::CompactString;
use serde::Deserialize;

#[cfg(feature = "mcp")]
use crate::extras::mcp::config::TrustedMcpServer;
use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::{CheckResult, PermCheck};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Msg(String),
}

impl From<io::Error> for ToolError {
    fn from(e: io::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

pub fn is_skip_dir(name: &str) -> bool {
    matches!(name, "node_modules" | "target")
}

#[derive(Deserialize)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct EditArgs {
    pub path: String,
    #[serde(default)]
    pub block: Option<String>,
    #[serde(default)]
    pub file_crc: Option<String>,
    #[serde(default)]
    pub edits: Option<Vec<EditOp>>,
}

#[derive(Debug, Clone)]
pub(crate) struct EditBlock {
    pub search: String,
    pub replace: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EditOp {
    pub line: Option<String>,
    pub lines: Option<String>,
    pub text: String,
}

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
    pub timeout: Option<u64>,
}

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
    pub context_lines: Option<usize>,
}

#[derive(Deserialize)]
pub struct FindFilesArgs {
    pub pattern: String,
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub struct ListDirArgs {
    pub path: Option<String>,
}

async fn handle_ask_inner(
    ask_tx: &AskSender,
    permission: &PermCheck,
    tool: &str,
    input: &str,
    suggested_pattern: Option<String>,
    additional_allow_patterns: Vec<String>,
    correlation_tool: &str,
) -> Result<(), ToolError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    ask_tx
        .send(AskRequest {
            tool: CompactString::new(tool),
            input: input.to_string(),
            tool_call_id: crate::permission::ask::take_tool_call_id(correlation_tool),
            suggested_pattern,
            additional_allow_patterns: additional_allow_patterns.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| ToolError::Msg("Permission system unavailable".to_string()))?;
    match reply_rx.await {
        Ok(UserDecision::AllowOnce) => Ok(()),
        Ok(UserDecision::AllowAlways(pattern)) => {
            let mut checker = permission.lock().unwrap_or_else(|e| e.into_inner());
            checker.add_session_allowlist(tool.to_string(), &pattern);
            for additional in additional_allow_patterns {
                checker.add_session_allowlist(tool.to_string(), &additional);
            }
            Ok(())
        }
        _ => Err(ToolError::Msg("Permission denied by user".to_string())),
    }
}

pub async fn check_perm(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    input_key: &str,
) -> Result<Option<String>, ToolError> {
    let Some(perm) = permission else {
        return Ok(None);
    };
    let result = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.check(tool, input_key)
    };
    match result {
        CheckResult::Allowed => Ok(None),
        CheckResult::AllowedWithCoaching(msg) => Ok(Some(msg)),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, input_key, None, Vec::new(), tool).await?;
            Ok(None)
        }
    }
}

#[cfg(feature = "mcp")]
pub(crate) async fn check_mcp_perm(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    input_key: &str,
    trusted_identity: Option<TrustedMcpServer>,
    mcp_tool_name: &str,
    correlation_tool: &str,
) -> Result<Option<String>, ToolError> {
    let Some(perm) = permission else {
        return Ok(None);
    };
    let result = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.check_mcp(input_key, trusted_identity, mcp_tool_name)
    };
    match result {
        CheckResult::Allowed => Ok(None),
        CheckResult::AllowedWithCoaching(msg) => Ok(Some(msg)),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(
                tx,
                perm,
                "mcp_tool",
                input_key,
                None,
                Vec::new(),
                correlation_tool,
            )
            .await?;
            Ok(None)
        }
    }
}

pub async fn check_perm_path(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<Option<String>, ToolError> {
    check_perm_path_with_suggestion(permission, ask_tx, tool, path, None, Vec::new()).await
}

pub(crate) async fn check_perm_path_with_suggestion(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
    suggested_pattern: Option<String>,
    additional_allow_patterns: Vec<String>,
) -> Result<Option<String>, ToolError> {
    let Some(perm) = permission else {
        return Ok(None);
    };
    let result = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.check_path(tool, path)
    };
    match result {
        CheckResult::Allowed => Ok(None),
        CheckResult::AllowedWithCoaching(msg) => Ok(Some(msg)),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(
                tx,
                perm,
                tool,
                path,
                suggested_pattern,
                additional_allow_patterns,
                tool,
            )
            .await?;
            Ok(None)
        }
    }
}

/// Check a canonical path whose exact filesystem object is held open by the
/// caller. `external` is derived lexically from that canonical path and the
/// canonical workspace root, so the checker must not resolve the live pathname
/// again while an interactive decision is pending.
#[cfg(feature = "lsp")]
pub(crate) async fn check_perm_canonical_path(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
    external: bool,
) -> Result<Option<String>, ToolError> {
    let Some(perm) = permission else {
        return Ok(None);
    };
    let result = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.check_canonical_path(tool, path, external)
    };
    match result {
        CheckResult::Allowed => Ok(None),
        CheckResult::AllowedWithCoaching(msg) => Ok(Some(msg)),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, path, None, Vec::new(), tool).await?;
            Ok(None)
        }
    }
}

pub(crate) async fn check_perm_bound_path(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    workspace: &crate::paths::WorkspaceBinding,
    relative: &Path,
) -> Result<Option<String>, ToolError> {
    let logical = workspace.logical_relative_path(relative)?;
    let logical = logical
        .to_str()
        .ok_or_else(|| ToolError::Msg("bound workspace path is not valid UTF-8".to_string()))?;
    let Some(perm) = permission else {
        return Ok(None);
    };
    let result = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        guard.check_bound_path(tool, logical)
    };
    match result {
        CheckResult::Allowed => Ok(None),
        CheckResult::AllowedWithCoaching(msg) => Ok(Some(msg)),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, logical, None, Vec::new(), tool).await?;
            Ok(None)
        }
    }
}
