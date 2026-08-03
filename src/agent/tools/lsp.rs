//! `lsp_diagnostics` agent tool: on-demand language-server diagnostics.
//! Post-edit diagnostics are appended to edit/write results automatically;
//! this tool is for querying a file before editing it, or surveying the
//! whole project.

use std::path::Path;
use std::time::Duration;

use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{
    AskSender, PermCheck, ToolError, check_perm_bound_path, check_perm_path,
    check_perm_path_with_suggestion,
};
use crate::extras::lsp::LspManager;

/// Longer than the post-edit wait: an explicit query justifies giving the
/// server more time to catch up.
const QUERY_WAIT: Duration = Duration::from_secs(3);

pub struct LspTool {
    pub manager: LspManager,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

#[derive(Deserialize)]
pub struct LspArgs {
    /// File to inspect. Omit to list diagnostics for every file.
    pub path: Option<String>,
}

impl LspTool {
    pub fn new(
        manager: LspManager,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            manager,
            permission,
            ask_tx,
        }
    }
}

impl Tool for LspTool {
    const NAME: &'static str = "lsp_diagnostics";

    type Error = ToolError;
    type Args = LspArgs;
    type Output = String;

    fn description(&self) -> String {
        "Get language-server diagnostics (errors/warnings). With `path`: diagnostics for that file, synced from disk first. Without: every file that currently has diagnostics.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to inspect (optional; omit for all files)" }
            }
        })
    }

    async fn call(&self, args: LspArgs) -> Result<String, ToolError> {
        match args.path {
            Some(path) => {
                let expanded = crate::fs::expand_tilde(&path);
                let path = Path::new(&expanded);
                if !path.exists() {
                    return Err(ToolError::Msg(format!("File '{expanded}' does not exist.")));
                }
                let resolved = tokio::fs::canonicalize(path).await?;
                let permission_path = canonical_permission_path(&resolved)?;
                if !tokio::fs::symlink_metadata(&resolved).await?.is_file() {
                    return Err(ToolError::Msg(format!(
                        "File '{expanded}' is not a regular file."
                    )));
                }
                let coaching = check_perm_path(
                    &self.permission,
                    &self.ask_tx,
                    Self::NAME,
                    permission_path.as_ref(),
                )
                .await?;

                // Manager access begins only after the canonical read path is
                // authorized. Operational LSP failure remains fail-open.
                self.manager.notify_changed(&resolved).await;
                let output = self
                    .manager
                    .diagnostics_block(&resolved, QUERY_WAIT)
                    .await
                    .map(|block| block.trim_start().to_string())
                    .unwrap_or_else(|| format!("No diagnostics for {expanded}."));
                Ok(with_coaching(coaching, output))
            }
            None => {
                let root = tokio::fs::canonicalize(self.manager.root()).await?;
                let permission_root = canonical_permission_path(&root)?;
                let root_pattern = crate::permission::pattern::descendant_path_pattern(&root);
                let exact_root_pattern = crate::permission::pattern::exact_path_pattern(&root);
                let coaching = check_perm_path_with_suggestion(
                    &self.permission,
                    &self.ask_tx,
                    Self::NAME,
                    permission_root.as_ref(),
                    Some(root_pattern),
                    vec![exact_root_pattern],
                )
                .await?;

                // Root authorization precedes even enumerating cache keys.
                // Each file then receives its own path check so an explicit
                // deny rule cannot leak through the project-wide aggregate.
                let mut snapshots = Vec::new();
                let mut remaining_lines = crate::extras::lsp::MAX_DIAG_LINES;
                for uri in self.manager.diagnostic_candidate_uris() {
                    let Some(binding) = self.manager.bind_diagnostic_uri(&uri).await else {
                        continue;
                    };
                    let Ok(permission_path) = canonical_permission_path(binding.path()) else {
                        continue;
                    };
                    let external = !binding.path().starts_with(&root);
                    if check_perm_bound_path(
                        &self.permission,
                        &self.ask_tx,
                        Self::NAME,
                        permission_path.as_ref(),
                        external,
                    )
                    .await
                    .is_ok()
                        && let Some(snapshot) = self
                            .manager
                            .snapshot_bound_diagnostics(&binding, remaining_lines)
                    {
                        remaining_lines =
                            remaining_lines.saturating_sub(snapshot.retained_line_count());
                        let truncated = snapshot.is_truncated();
                        snapshots.push(snapshot);
                        if truncated {
                            break;
                        }
                    }
                    // `binding` and its descriptor are released here before
                    // the next cache candidate is opened.
                }
                let output = self
                    .manager
                    .all_diagnostics_block_for_snapshots(&snapshots)
                    .map(|block| format!("Files with diagnostics:\n{block}"))
                    .unwrap_or_else(|| "No diagnostics.".to_string());
                Ok(with_coaching(coaching, output))
            }
        }
    }
}

pub(crate) fn canonical_permission_path(
    path: &Path,
) -> Result<std::borrow::Cow<'_, str>, ToolError> {
    let path = path
        .to_str()
        .ok_or_else(|| ToolError::Msg("LSP diagnostics require a UTF-8 file path.".to_string()))?;
    Ok(crate::permission::pattern::normalize_policy_path(path))
}

fn with_coaching(coaching: Option<String>, output: String) -> String {
    match coaching {
        Some(coaching) => format!("{coaching}\n\n{output}"),
        None => output,
    }
}
