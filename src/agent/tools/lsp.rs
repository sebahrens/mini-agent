//! `lsp_diagnostics` agent tool: on-demand language-server diagnostics.
//! Post-edit diagnostics are appended to edit/write results automatically;
//! this tool is for querying a file before editing it, or surveying the
//! whole project.

use std::path::Path;
use std::time::Duration;

use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{
    AskSender, PermCheck, ToolError, check_perm_canonical_path, check_perm_path_with_suggestion,
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
#[serde(deny_unknown_fields)]
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
                let path = self
                    .manager
                    .resolve_path(Path::new(&expanded))
                    .map_err(ToolError::Msg)?;
                if !path.exists() {
                    return Err(ToolError::Msg(format!("File '{expanded}' does not exist.")));
                }
                let relative = Path::new(&expanded);
                let bound_relative = !relative.is_absolute() && !expanded.starts_with('~');
                let permission_path = canonical_permission_path(&path)?;
                let external = !path.starts_with(self.manager.root());
                if !tokio::fs::symlink_metadata(&path).await?.is_file() {
                    return Err(ToolError::Msg(format!(
                        "File '{expanded}' is not a regular file."
                    )));
                }
                let coaching = check_perm_canonical_path(
                    &self.permission,
                    &self.ask_tx,
                    Self::NAME,
                    permission_path.as_ref(),
                    external,
                )
                .await?;

                // Manager access begins only after the capability-bound read
                // path is authorized. Operational LSP failure remains open.
                let baseline = if bound_relative {
                    self.manager.notify_changed_relative(relative).await
                } else {
                    self.manager.notify_changed(&path).await
                };
                let diagnostics = if bound_relative {
                    self.manager
                        .diagnostics_block_relative(relative, QUERY_WAIT, baseline)
                        .await
                } else {
                    self.manager
                        .diagnostics_block_since(&path, QUERY_WAIT, baseline)
                        .await
                };
                let output = diagnostics
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
                    if check_perm_canonical_path(
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
                            .await
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

/// `lsp_navigate`: read-only semantic navigation (definition, references,
/// implementation, type definition, hover, document and workspace symbols)
/// through the configured language server.
pub struct LspNavigateTool {
    pub manager: LspManager,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspNavigateArgs {
    /// definition | type_definition | implementation | references | hover |
    /// symbols | workspace_symbols
    pub op: String,
    /// File the request is about (for workspace_symbols: any file of the
    /// language whose server should be asked).
    pub path: String,
    /// 1-based line of the symbol (position ops).
    pub line: Option<u32>,
    /// 1-based column of the symbol (position ops); UTF-16 code units.
    pub column: Option<u32>,
    /// Symbol name filter for workspace_symbols.
    pub query: Option<String>,
}

impl LspNavigateTool {
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

impl Tool for LspNavigateTool {
    const NAME: &'static str = "lsp_navigate";

    type Error = ToolError;
    type Args = LspNavigateArgs;
    type Output = String;

    fn description(&self) -> String {
        "Semantic code navigation through the project's language server. `op`: definition, type_definition, implementation, references (need `line` and `column`, 1-based, pointing at the symbol), hover (type/docs at that position), symbols (outline of `path`), workspace_symbols (search by `query`; `path` selects the language server). Returns workspace-relative `path:line:column` locations (1-based), capped at 100. Definitions in dependencies or the standard library come back as absolute paths marked `(outside workspace)`; open them with `read` (subject to read permission).".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["definition", "type_definition", "implementation", "references", "hover", "symbols", "workspace_symbols"]
                },
                "path": { "type": "string", "description": "File to query" },
                "line": { "type": "integer", "minimum": 1, "description": "1-based line (position ops)" },
                "column": { "type": "integer", "minimum": 1, "description": "1-based column (position ops)" },
                "query": { "type": "string", "description": "Symbol filter for workspace_symbols" }
            },
            "required": ["op", "path"]
        })
    }

    async fn call(&self, args: LspNavigateArgs) -> Result<String, ToolError> {
        use crate::extras::lsp::navigate::{NavAnswer, NavOp, NavQuery};
        let op = NavOp::parse(args.op.trim()).ok_or_else(|| {
            ToolError::Msg(format!(
                "Unknown op '{}'. Use definition, type_definition, implementation, references, hover, symbols or workspace_symbols.",
                args.op
            ))
        })?;
        let (line, character) = if op.needs_position() {
            match (args.line, args.column) {
                (Some(line @ 1..), Some(column @ 1..)) => (line - 1, column - 1),
                _ => {
                    return Err(ToolError::Msg(format!(
                        "op '{}' needs 1-based `line` and `column` of the symbol.",
                        args.op
                    )));
                }
            }
        } else {
            (0, 0)
        };
        let expanded = crate::fs::expand_tilde(&args.path);
        let path = self
            .manager
            .resolve_path(Path::new(&expanded))
            .map_err(ToolError::Msg)?;
        if !path.starts_with(self.manager.root()) {
            return Err(ToolError::Msg(
                "lsp_navigate only queries files inside the session workspace.".to_string(),
            ));
        }
        if !tokio::fs::symlink_metadata(&path).await?.is_file() {
            return Err(ToolError::Msg(format!(
                "File '{expanded}' is not a regular file."
            )));
        }
        if !self.manager.handles(&path) {
            return Err(ToolError::Msg(format!(
                "No language server is configured for '{expanded}'."
            )));
        }
        let permission_path = canonical_permission_path(&path)?;
        let coaching = check_perm_canonical_path(
            &self.permission,
            &self.ask_tx,
            Self::NAME,
            permission_path.as_ref(),
            false,
        )
        .await?;
        // Synchronize the authorized bytes so positions refer to the file on
        // disk, then ask the server.
        let relative = Path::new(&expanded);
        if !relative.is_absolute() && !expanded.starts_with('~') {
            self.manager.notify_changed_relative(relative).await;
        } else {
            self.manager.notify_changed(&path).await;
        }
        let query = NavQuery {
            op,
            line,
            character,
            query: args.query.unwrap_or_default(),
        };
        let answer = self
            .manager
            .navigate(&path, &query)
            .await
            .map_err(|error| ToolError::Msg(format!("lsp_navigate: {error}")))?;
        let root = self.manager.root().to_path_buf();
        let output = match answer {
            NavAnswer::Hover { text, truncated } => {
                if text.is_empty() {
                    "No hover information at that position.".to_string()
                } else if truncated {
                    format!(
                        "{text}\n[hover text truncated at {} bytes]",
                        crate::extras::lsp::navigate::MAX_HOVER_BYTES
                    )
                } else {
                    text
                }
            }
            NavAnswer::Items {
                items,
                outside,
                truncated,
            } => {
                // Results are filtered like a workspace walk: a read deny
                // rule hides the location rather than naming the file.
                let deny = super::walk_deny_filter(&self.permission, "read", Some(&root));
                let mut denied = 0usize;
                let mut outside = outside;
                let mut lines = Vec::new();
                for item in items {
                    if item.external {
                        // A location the server placed outside the workspace
                        // (dependency or standard library source) is named by
                        // its absolute path only when the `read` rules do not
                        // deny it, so the model can then `read` it under the
                        // ordinary permission flow. Nothing is read here.
                        if item.path.starts_with(&root) {
                            outside += 1;
                            continue;
                        }
                        let allowed = match (&self.permission, item.path.to_str()) {
                            (_, None) => false,
                            (None, Some(_)) => true,
                            (Some(permission), Some(path)) => !matches!(
                                permission
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .check_path("read", path),
                                crate::permission::checker::CheckResult::Denied(_)
                            ),
                        };
                        if !allowed {
                            denied += 1;
                            continue;
                        }
                    } else if deny.as_ref().is_some_and(|deny| deny(&item.path, false)) {
                        denied += 1;
                        continue;
                    }
                    let shown = item
                        .path
                        .strip_prefix(&root)
                        .unwrap_or(&item.path)
                        .display()
                        .to_string()
                        .replace('\\', "/");
                    let mut location = format!("{shown}:{}:{}", item.line + 1, item.character + 1);
                    if item.external {
                        location.push_str(" (outside workspace)");
                    }
                    lines.push(match item.label {
                        Some(label) => {
                            format!("{}{label} — {location}", "  ".repeat(item.depth))
                        }
                        None => location,
                    });
                }
                let mut output = if lines.is_empty() {
                    "No results.".to_string()
                } else {
                    lines.join("\n")
                };
                if truncated > 0 {
                    output.push_str(&format!(
                        "\n[{truncated} more results not shown (limit {}); narrow the query]",
                        crate::extras::lsp::navigate::MAX_NAV_ITEMS
                    ));
                }
                if outside > 0 {
                    output.push_str(&format!(
                        "\n[{outside} results outside the workspace omitted]"
                    ));
                }
                if denied > 0 {
                    output.push_str(&format!("\n[{denied} results hidden by permission rules]"));
                }
                output
            }
        };
        Ok(with_coaching(coaching, output))
    }
}
