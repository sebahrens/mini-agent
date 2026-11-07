//! `lsp_diagnostics` agent tool: on-demand language-server diagnostics.
//! Post-edit diagnostics are appended to edit/write results automatically;
//! this tool is for querying a file before editing it, or surveying the
//! whole project.

use std::path::Path;
use std::time::Duration;

use crate::extras::lsp::query::{DiagTarget, FileDiagStatus, MAX_QUERY_WAIT};

use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{
    AskSender, PermCheck, ToolError, check_perm_canonical_path, check_perm_path_with_suggestion,
};
use crate::extras::lsp::LspManager;

pub struct LspTool {
    pub manager: LspManager,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LspArgs {
    /// File to inspect. Omit to list diagnostics for every file.
    pub path: Option<String>,
    /// Diagnostics to skip (page with the offset the previous truncation
    /// footer names).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Most diagnostics to return (default 200, at most 1000).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Only `error` or only `warning` diagnostics.
    #[serde(default)]
    pub severity: Option<DiagSeverity>,
    /// Project-wide listing only: restrict to files under this
    /// workspace-relative directory.
    #[serde(default)]
    pub directory: Option<String>,
    /// Per-file query only: milliseconds to wait for the server to publish
    /// (default 3000, or the server's `wait_ms`; at most 60000).
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagSeverity {
    Error,
    Warning,
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
        "Get language-server diagnostics (errors/warnings). With `path`: diagnostics for that file, synced from disk first. Without: every file that currently has diagnostics (optionally under `directory`). Up to 200 diagnostics are returned per call (`limit`, at most 1000), errors first; a truncation line states how many diagnostics and files are hidden and the `offset` to continue with. `severity` narrows to errors or warnings. A per-file result says explicitly when the file is clean (the server published nothing to report) versus when no server handles it, the file could not be synchronized, the server is unavailable, or the server did not answer within the wait (status unknown; retry with a larger `wait_ms`, at most 60000). The project-wide form covers only files a server has published diagnostics for.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to inspect (optional; omit for all files)" },
                "offset": { "type": "integer", "minimum": 0, "description": "Diagnostics to skip; pass the offset named by the previous truncation line" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Most diagnostics to return (default 200)" },
                "severity": { "type": "string", "enum": ["error", "warning"], "description": "Only diagnostics of this severity" },
                "directory": { "type": "string", "description": "Project-wide listing only: workspace-relative directory to restrict to" },
                "wait_ms": { "type": "integer", "minimum": 1, "maximum": 60000, "description": "Per-file query only: milliseconds to wait for the server to publish (default 3000 or the server's wait_ms)" }
            }
        })
    }

    async fn call(&self, args: LspArgs) -> Result<String, ToolError> {
        let limit = args
            .limit
            .unwrap_or(crate::extras::lsp::QUERY_DIAG_LIMIT_DEFAULT);
        if limit == 0 || limit > crate::extras::lsp::QUERY_DIAG_LIMIT_MAX {
            return Err(ToolError::Msg(format!(
                "limit must be between 1 and {}",
                crate::extras::lsp::QUERY_DIAG_LIMIT_MAX
            )));
        }
        let severity = args.severity.map(|severity| match severity {
            DiagSeverity::Error => lsp_types::DiagnosticSeverity::ERROR,
            DiagSeverity::Warning => lsp_types::DiagnosticSeverity::WARNING,
        });
        let offset = args.offset.unwrap_or(0);
        let page = crate::extras::lsp::DiagPage {
            offset,
            limit,
            severity,
        };
        let wait = match args.wait_ms {
            None => None,
            Some(_) if args.path.is_none() => {
                return Err(ToolError::Msg(
                    "wait_ms applies only to a per-file query; pass path".to_string(),
                ));
            }
            Some(ms) if ms == 0 || u128::from(ms) > MAX_QUERY_WAIT.as_millis() => {
                return Err(ToolError::Msg(format!(
                    "wait_ms must be between 1 and {}",
                    MAX_QUERY_WAIT.as_millis()
                )));
            }
            Some(ms) => Some(Duration::from_millis(ms)),
        };
        if args.path.is_some() && args.directory.is_some() {
            return Err(ToolError::Msg(
                "directory applies only to the project-wide listing; omit path".to_string(),
            ));
        }
        let directory = match args.directory.as_deref() {
            None => None,
            Some(dir) => {
                let dir = Path::new(dir);
                if dir.is_absolute()
                    || dir.components().any(|c| {
                        !matches!(
                            c,
                            std::path::Component::Normal(_) | std::path::Component::CurDir
                        )
                    })
                {
                    return Err(ToolError::Msg(
                        "directory must be a workspace-relative path without `..`".to_string(),
                    ));
                }
                normalize_directory_filter(dir)
            }
        };
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
                let target = if bound_relative {
                    DiagTarget::Relative(relative)
                } else {
                    DiagTarget::Absolute(&path)
                };
                let status = self
                    .manager
                    .file_diagnostics_status(target, wait, page)
                    .await;
                let output = render_file_status(&expanded, status, page);
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
                for uri in self.manager.diagnostic_candidate_uris() {
                    let Some(binding) = self.manager.bind_diagnostic_uri(&uri).await else {
                        continue;
                    };
                    let Ok(permission_path) = canonical_permission_path(binding.path()) else {
                        continue;
                    };
                    if let Some(dir) = &directory
                        && !binding
                            .path()
                            .strip_prefix(&root)
                            .is_ok_and(|relative| relative.starts_with(dir))
                    {
                        continue;
                    }
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
                            .snapshot_bound_diagnostics_with(&binding, usize::MAX, severity)
                            .await
                    {
                        snapshots.push(snapshot);
                    }
                    // `binding` and its descriptor are released here before
                    // the next cache candidate is opened.
                }
                let mut output = self
                    .manager
                    .diagnostics_page_for_snapshots(&snapshots, offset, limit)
                    .map(|block| format!("Files with diagnostics:\n{block}"))
                    .unwrap_or_else(|| "No diagnostics.".to_string());
                output.push('\n');
                output.push_str(&self.manager.project_listing_scope_note());
                let dropped = self.manager.dropped_diagnostic_files();
                if dropped > 0 {
                    output.push_str(&format!(
                        "\n[{dropped} files' diagnostics dropped: cache full]"
                    ));
                }
                Ok(with_coaching(coaching, output))
            }
        }
    }
}

/// Text for one per-file outcome; only `Clean` reads as a clean file.
fn render_file_status(
    path: &str,
    status: FileDiagStatus,
    page: crate::extras::lsp::DiagPage,
) -> String {
    match status {
        FileDiagStatus::Diagnostics(block) => block.trim_start().to_string(),
        FileDiagStatus::Clean { server } => {
            let filtered = if page.severity.is_some() || page.offset > 0 {
                " (nothing matched the severity/offset window)"
            } else {
                ""
            };
            format!(
                "No diagnostics for {path}. LSP ({server}) published diagnostics for it and none were reported{filtered}."
            )
        }
        FileDiagStatus::NoServer {
            outside_workspace: true,
            ..
        } => format!(
            "Not checked: {path} is outside the workspace, so no language server handles it. This is not a clean result."
        ),
        FileDiagStatus::NoServer {
            extension,
            withheld,
            ..
        } => {
            let kind = extension.unwrap_or_else(|| "no extension".to_string());
            match withheld {
                Some(why) => format!(
                    "Not checked: no language server is running for {path} ({kind}): {why}. This is not a clean result."
                ),
                None => format!(
                    "Not checked: no language server is configured for {path} ({kind} files). Add one under [lsp.servers.<name>] with `command` and `extensions`. This is not a clean result."
                ),
            }
        }
        FileDiagStatus::NotSynced { server, reason } => format!(
            "Not checked: {path} was not synchronized to LSP ({server}): {reason}. This is not a clean result."
        ),
        FileDiagStatus::Unavailable {
            server,
            retry_in_ms,
        } => {
            let retry = match retry_in_ms {
                Some(ms) => format!("; it is cooling down after a failure, retry in about {ms} ms"),
                None => "; it could not be started (is the binary on PATH?)".to_string(),
            };
            format!(
                "Not checked: LSP ({server}) is unavailable for {path}{retry}. This is not a clean result."
            )
        }
        FileDiagStatus::TimedOut {
            server,
            wait_ms,
            partial,
        } => {
            let notice = format!(
                "LSP ({server}) did not publish diagnostics for {path} within {wait_ms} ms; the file's status is unknown (the server may still be checking). Retry with a larger `wait_ms` (at most {}).",
                MAX_QUERY_WAIT.as_millis()
            );
            match partial {
                Some(block) => format!("{}\n\n{notice}", block.trim_start()),
                None => format!("Status unknown: {notice}"),
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

#[derive(Deserialize, Default)]
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
    /// rename: the new identifier.
    #[serde(default)]
    pub new_name: Option<String>,
    /// code_actions: 1-based end line of the range (default `line`).
    #[serde(default)]
    pub end_line: Option<u32>,
    /// code_actions: 1-based end column of the range (default `column`).
    #[serde(default)]
    pub end_column: Option<u32>,
    /// code_actions: 1-based number of an action whose edit to preview.
    #[serde(default)]
    pub action: Option<u32>,
    /// Results (or, for rename and a code action's edit, edits) to skip; pass
    /// the `next_offset` named by the previous truncation line.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Most results or edits to return (default 100 results, 200 edits; at
    /// most 1000).
    #[serde(default)]
    pub limit: Option<usize>,
    /// rename / a code action's edit: show the complete replacement text of
    /// the edit with this 1-based number from the listing.
    #[serde(default)]
    pub edit: Option<usize>,
    /// rename / a code action's edit: show every replacement in full (a page
    /// ends early at a byte budget and names the `next_offset`).
    #[serde(default)]
    pub full_text: Option<bool>,
    /// With `edit`: byte offset into that edit's replacement text.
    #[serde(default)]
    pub text_offset: Option<usize>,
}

/// Largest `limit` of an `lsp_navigate` page.
const NAV_LIMIT_MAX: usize = 1000;
const NAV_ITEMS_DEFAULT: usize = 100;
const NAV_EDITS_DEFAULT: usize = 200;

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
        "Semantic code navigation through the project's language server. `op`: definition, type_definition, implementation, references (need `line` and `column`, 1-based, pointing at the symbol), hover (type/docs at that position), symbols (outline of `path`), workspace_symbols (search by `query`; `path` selects the language server), incoming_calls / outgoing_calls (call hierarchy of the function at `line`/`column`), rename (needs `new_name`; returns the server's workspace edit as a preview and changes NO file; it needs `edit` permission for `path`; apply the edits yourself with `edit`, using the complete replacement text from `edit`=N / `full_text`) and code_actions (quick fixes and refactors for `line`/`column`, optionally through `end_line`/`end_column`; pass `action`=N to preview that action's edit; server commands are never executed). An empty answer from a server that is still indexing says so instead of reporting no results. Returns workspace-relative `path:line:column` locations (1-based), sorted by path and position (symbols keep outline order), 100 per page: pass `offset` (the `next_offset` a truncation line names) and optionally `limit` (at most 1000) to read the rest; at most 5000 results are retained per request. Rename and code-action edit previews page the same way over their edits (200 per page, up to 10000 edits in 500 files); each edit is numbered and long replacement text is cut in the listing with a note: pass `edit`=N for that edit's complete replacement text (continue with `text_offset`), or `full_text`=true for complete text on every edit of the page. Definitions in dependencies or the standard library come back as absolute paths marked `(outside workspace)`; open them with `read` (subject to read permission).".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["definition", "type_definition", "implementation", "references", "hover", "symbols", "workspace_symbols", "incoming_calls", "outgoing_calls", "rename", "code_actions"]
                },
                "path": { "type": "string", "description": "File to query" },
                "line": { "type": "integer", "minimum": 1, "description": "1-based line (position ops)" },
                "column": { "type": "integer", "minimum": 1, "description": "1-based column (position ops)" },
                "query": { "type": "string", "description": "Symbol filter for workspace_symbols" },
                "new_name": { "type": "string", "maxLength": 256, "description": "rename: the new identifier" },
                "end_line": { "type": "integer", "minimum": 1, "description": "code_actions: 1-based end line of the range" },
                "end_column": { "type": "integer", "minimum": 1, "description": "code_actions: 1-based end column of the range" },
                "action": { "type": "integer", "minimum": 1, "description": "code_actions: preview the edit of this action number" },
                "offset": { "type": "integer", "minimum": 0, "description": "Results (rename / action previews: edits) to skip; pass the next_offset of the previous page" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Most results or edits per page (default 100 results, 200 edits)" },
                "edit": { "type": "integer", "minimum": 1, "description": "rename / code action edit: complete replacement text of the edit with this number from the listing" },
                "full_text": { "type": "boolean", "description": "rename / code action edit: show every replacement in full (pages end early at a byte budget; continue with next_offset)" },
                "text_offset": { "type": "integer", "minimum": 0, "description": "With edit: byte offset into that edit's replacement text (continuation)" }
            },
            "required": ["op", "path"]
        })
    }

    async fn call(&self, args: LspNavigateArgs) -> Result<String, ToolError> {
        use crate::extras::lsp::navigate::{NavAnswer, NavOp, NavQuery};
        let op = NavOp::parse(args.op.trim()).ok_or_else(|| {
            ToolError::Msg(format!(
                "Unknown op '{}'. Use definition, type_definition, implementation, references, hover, symbols, workspace_symbols, incoming_calls, outgoing_calls, rename or code_actions.",
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
        let new_name = args.new_name.clone().unwrap_or_default();
        if op == NavOp::Rename {
            let valid = !new_name.trim().is_empty()
                && new_name.len() <= crate::extras::lsp::navigate::MAX_NEW_NAME_BYTES
                && !new_name.chars().any(char::is_control)
                && !new_name.chars().any(char::is_whitespace);
            if !valid {
                return Err(ToolError::Msg(
                    "rename needs a `new_name`: a single identifier of at most 256 bytes, without whitespace or control characters.".to_string(),
                ));
            }
        } else if args.new_name.is_some() {
            return Err(ToolError::Msg(
                "new_name is supported only by rename.".to_string(),
            ));
        }
        let end = match (op, args.end_line, args.end_column) {
            (NavOp::CodeActions, Some(line @ 1..), Some(column @ 1..)) => {
                Some((line - 1, column - 1))
            }
            (NavOp::CodeActions, None, None) => None,
            (NavOp::CodeActions, _, _) => {
                return Err(ToolError::Msg(
                    "code_actions needs both end_line and end_column (1-based) or neither."
                        .to_string(),
                ));
            }
            (_, None, None) => None,
            _ => {
                return Err(ToolError::Msg(
                    "end_line and end_column are supported only by code_actions.".to_string(),
                ));
            }
        };
        if args.action.is_some() && op != NavOp::CodeActions {
            return Err(ToolError::Msg(
                "action is supported only by code_actions.".to_string(),
            ));
        }
        let edit_preview_call =
            op == NavOp::Rename || (op == NavOp::CodeActions && args.action.is_some());
        if !edit_preview_call
            && (args.edit.is_some() || args.full_text.is_some() || args.text_offset.is_some())
        {
            return Err(ToolError::Msg(
                "edit, full_text and text_offset apply only to rename and to code_actions with `action`.".to_string(),
            ));
        }
        if args.edit == Some(0) {
            return Err(ToolError::Msg("edit is a 1-based edit number.".to_string()));
        }
        if args.text_offset.is_some() && args.edit.is_none() {
            return Err(ToolError::Msg("text_offset needs `edit`.".to_string()));
        }
        if args.edit.is_some() && args.full_text == Some(true) {
            return Err(ToolError::Msg(
                "pass either edit (one edit in full) or full_text (a page in full), not both."
                    .to_string(),
            ));
        }
        let text_view = TextView {
            full: args.full_text.unwrap_or(false),
            edit: args.edit,
            text_offset: args.text_offset.unwrap_or(0),
        };
        let pages = match op {
            NavOp::Hover => false,
            NavOp::CodeActions => args.action.is_some(),
            _ => true,
        };
        if !pages && (args.offset.is_some() || args.limit.is_some()) {
            return Err(ToolError::Msg(
                "offset and limit are supported only by ops that return lists of results or edits (not hover, or code_actions without `action`).".to_string(),
            ));
        }
        if args
            .limit
            .is_some_and(|limit| limit == 0 || limit > NAV_LIMIT_MAX)
        {
            return Err(ToolError::Msg(format!(
                "limit must be between 1 and {NAV_LIMIT_MAX}."
            )));
        }
        let offset = args.offset.unwrap_or(0);
        let edit_op = matches!(op, NavOp::Rename | NavOp::CodeActions);
        let limit = args.limit.unwrap_or(if edit_op {
            NAV_EDITS_DEFAULT
        } else {
            NAV_ITEMS_DEFAULT
        });
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
            let reason = self
                .manager
                .withheld_explanation(&path)
                .map_or_else(String::new, |why| format!(" ({why})"));
            return Err(ToolError::Msg(format!(
                "No language server is configured for '{expanded}'{reason}."
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
        // A rename proposes edits to source files: it also needs `edit`
        // permission for the file, though nothing is written here.
        let coaching = if op == NavOp::Rename {
            let edit_coaching = check_perm_canonical_path(
                &self.permission,
                &self.ask_tx,
                "edit",
                permission_path.as_ref(),
                false,
            )
            .await?;
            coaching.or(edit_coaching)
        } else {
            coaching
        };
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
            end,
            new_name,
            action_index: args.action,
        };
        let (answer, indexing) = self
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
            } => self.render_items(items, outside, truncated, op, (offset, limit)),
            NavAnswer::Edit(preview) => {
                let deny = super::walk_deny_filter(&self.permission, "read", Some(&root));
                render_edit_preview_with(
                    &preview,
                    &root,
                    deny.as_ref(),
                    op == NavOp::Rename,
                    &query.new_name,
                    (offset, limit),
                    text_view,
                )
            }
            NavAnswer::Actions { actions, truncated } => {
                render_actions(&actions, truncated, indexing)
            }
        };
        let output = if indexing {
            let note = "[the language server is still indexing, so this answer may be incomplete; retry in a few seconds]";
            format!("{output}\n{note}")
        } else {
            output
        };
        Ok(with_coaching(coaching, output))
    }
}

impl LspNavigateTool {
    /// Filters (permissions), orders and pages a location/symbol answer.
    pub(crate) fn render_items(
        &self,
        items: Vec<crate::extras::lsp::navigate::NavItem>,
        outside: usize,
        truncated: usize,
        op: crate::extras::lsp::navigate::NavOp,
        (offset, limit): (usize, usize),
    ) -> String {
        use crate::extras::lsp::navigate::NavOp;
        let root = self.manager.root().to_path_buf();
        // Results are filtered like a workspace walk: a read deny
        // rule hides the location rather than naming the file.
        let deny = super::walk_deny_filter(&self.permission, "read", Some(&root));
        let mut denied = 0usize;
        let mut outside = outside;
        let mut lines = Vec::new();
        let mut items = items;
        // Stable order across calls, so pages do not overlap. The
        // outline of a document keeps its tree order.
        if op != NavOp::Symbols {
            items.sort_by(|a, b| {
                (a.external, &a.path, a.line, a.character, &a.label).cmp(&(
                    b.external,
                    &b.path,
                    b.line,
                    b.character,
                    &b.label,
                ))
            });
        }
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
        let total = lines.len();
        let end = offset.saturating_add(limit).min(total);
        let mut output = if lines.is_empty() {
            "No results.".to_string()
        } else if offset >= total {
            format!("No results at offset {offset} ({total} in total).")
        } else {
            lines[offset..end].join("\n")
        };
        if offset < total && end < total {
            // Only the name-driven ops have a query to narrow.
            let narrow = if matches!(op, NavOp::WorkspaceSymbols | NavOp::Symbols) {
                ", or narrow the query"
            } else {
                ""
            };
            output.push_str(&format!(
                "\n[showing {}-{end} of {total} results; next_offset={end}: pass offset={end} to continue{narrow}]",
                offset + 1
            ));
        }
        if truncated > 0 {
            output.push_str(&format!(
                "\n[{truncated} more results beyond the {}-result ceiling are not reachable by paging]",
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
}

/// How replacement text is shown in an edit preview.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TextView {
    /// Show every replacement in full instead of a compact prefix; a page ends
    /// early at [`FULL_PAGE_BYTES`] and names the `next_offset`.
    pub full: bool,
    /// Show only this edit (1-based, the number printed in the listing) with
    /// its complete replacement text.
    pub edit: Option<usize>,
    /// Byte offset into that edit's replacement text.
    pub text_offset: usize,
}

/// Replacement text emitted per page in `full_text` mode before the page ends.
const FULL_PAGE_BYTES: usize = 48 * 1024;
/// Bytes of one edit's replacement text per `edit` call.
const EDIT_TEXT_PAGE_BYTES: usize = 32 * 1024;

fn char_floor(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
pub(crate) fn render_edit_preview(
    preview: &crate::extras::lsp::navigate::EditPreview,
    root: &Path,
    deny: Option<&super::find_files::PathFilter>,
    rename: bool,
    new_name: &str,
    window: (usize, usize),
) -> String {
    render_edit_preview_with(
        preview,
        root,
        deny,
        rename,
        new_name,
        window,
        TextView::default(),
    )
}

pub(crate) fn render_edit_preview_with(
    preview: &crate::extras::lsp::navigate::EditPreview,
    root: &Path,
    deny: Option<&super::find_files::PathFilter>,
    rename: bool,
    new_name: &str,
    (offset, limit): (usize, usize),
    view: TextView,
) -> String {
    use crate::extras::lsp::navigate::{EDIT_SUMMARY_BYTES, EditView};
    let mut hidden = 0usize;
    let mut visible: Vec<&crate::extras::lsp::navigate::FileEdits> = Vec::new();
    for file in &preview.files {
        if deny.is_some_and(|deny| deny(&file.path, false)) {
            hidden += 1;
        } else {
            visible.push(file);
        }
    }
    // Stable order across calls (path, then position) so pages do not overlap.
    visible.sort_by(|a, b| a.path.cmp(&b.path));
    let files = visible.len();
    // Every previewable edit in listing order: (file, edit, omitted-after).
    let mut flat: Vec<(String, &EditView, Option<usize>)> = Vec::new();
    for file in &visible {
        let mut edits: Vec<_> = file.edits.iter().collect();
        edits.sort_by_key(|edit| (edit.start, edit.end));
        let shown = file
            .path
            .strip_prefix(root)
            .unwrap_or(&file.path)
            .display()
            .to_string()
            .replace('\\', "/");
        let last = edits.len().saturating_sub(1);
        for (index, edit) in edits.into_iter().enumerate() {
            let omitted = (index == last && file.omitted_edits > 0).then_some(file.omitted_edits);
            flat.push((shown.clone(), edit, omitted));
        }
    }
    let shown_edits = flat.len();
    let heading = if rename {
        format!(
            "Rename to `{new_name}`: {} edits in {files} files. PREVIEW ONLY, no file was modified; apply the edits with `edit` (use `edit`=N or `full_text`=true here for complete replacement text).",
            preview.total_edits
        )
    } else {
        format!(
            "Code action edit: {} edits in {files} files. PREVIEW ONLY, no file was modified; apply the edits with `edit` (use `edit`=N or `full_text`=true here for complete replacement text).",
            preview.total_edits
        )
    };
    if let Some(number) = view.edit {
        return render_single_edit(&heading, &flat, number, view.text_offset);
    }
    let end = offset.saturating_add(limit).min(shown_edits);
    let mut body = String::new();
    let mut page_end = end;
    let mut emitted_text = 0usize;
    let mut last_file: Option<&str> = None;
    let mut files_in_page = 0usize;
    for (position, (file, edit, omitted)) in flat.iter().enumerate().take(end).skip(offset) {
        let text_cost = if view.full {
            edit.new_text.len()
        } else {
            edit.new_text.len().min(EDIT_SUMMARY_BYTES)
        };
        if view.full && emitted_text > 0 && emitted_text + text_cost > FULL_PAGE_BYTES {
            page_end = position;
            break;
        }
        emitted_text += text_cost;
        if last_file != Some(file.as_str()) {
            files_in_page += 1;
            body.push_str(&format!("{file}\n"));
            last_file = Some(file.as_str());
        }
        let shown_text = if view.full {
            edit.new_text.as_str()
        } else {
            &edit.new_text[..char_floor(&edit.new_text, EDIT_SUMMARY_BYTES)]
        };
        let mut note = String::new();
        if !view.full && edit.full_len > shown_text.len() {
            note.push_str(&format!(
                " [text cut at {} of {} bytes; pass edit={} for the full replacement or full_text=true]",
                shown_text.len(),
                edit.full_len,
                position + 1
            ));
        }
        if edit.cut {
            note.push_str(&format!(
                " [only the first {} of {} bytes of the replacement are retained]",
                edit.new_text.len(),
                edit.full_len
            ));
        }
        body.push_str(&format!(
            "  #{} {}:{}-{}:{} -> {:?}{note}\n",
            position + 1,
            edit.start.0 + 1,
            edit.start.1 + 1,
            edit.end.0 + 1,
            edit.end.1 + 1,
            shown_text,
        ));
        if let Some(omitted) = omitted {
            body.push_str(&format!(
                "  [{omitted} more edits in this file beyond the {}-edit bound are not previewable]\n",
                crate::extras::lsp::navigate::MAX_EDITS_PER_FILE
            ));
        }
    }
    if offset == 0 {
        for operation in &preview.file_ops {
            body.push_str(&format!("file operation: {operation}\n"));
        }
    }
    let mut out = if files == 0 && preview.file_ops.is_empty() {
        format!("{heading}\n(the server returned no edits)")
    } else if offset >= shown_edits && shown_edits > 0 {
        format!("{heading}\nNo edits at offset {offset} ({shown_edits} previewable in total).")
    } else {
        format!("{heading}\n{body}")
    };
    if offset < shown_edits && page_end < shown_edits {
        let reason = if page_end < end {
            " (page ended early at the full-text byte budget)"
        } else {
            ""
        };
        out.push_str(&format!(
            "[showing edits {}-{page_end} of {shown_edits} across {files} files ({files_in_page} in this page){reason}; next_offset={page_end}: pass offset={page_end} to continue]\n",
            offset + 1
        ));
    }
    if preview.omitted_files > 0 {
        out.push_str(&format!(
            "[{} more files beyond the {}-file bound are not previewable; split the change]\n",
            preview.omitted_files,
            crate::extras::lsp::navigate::MAX_EDIT_FILES
        ));
    }
    if preview.outside > 0 {
        out.push_str(&format!(
            "[{} edits or operations outside the workspace omitted]\n",
            preview.outside
        ));
    }
    if hidden > 0 {
        out.push_str(&format!("[{hidden} files hidden by permission rules]\n"));
    }
    out.trim_end().to_string()
}

/// One edit's complete replacement text, paged by bytes.
fn render_single_edit(
    heading: &str,
    flat: &[(
        String,
        &crate::extras::lsp::navigate::EditView,
        Option<usize>,
    )],
    number: usize,
    text_offset: usize,
) -> String {
    let Some((file, edit, _)) = number.checked_sub(1).and_then(|index| flat.get(index)) else {
        return format!(
            "{heading}\nNo edit number {number}; the listing has {} previewable edits (numbered #1.. in the order shown).",
            flat.len()
        );
    };
    let text = edit.new_text.as_str();
    let start = if text_offset >= text.len() {
        text.len()
    } else {
        char_floor(text, text_offset)
    };
    let stop = if start + EDIT_TEXT_PAGE_BYTES >= text.len() {
        text.len()
    } else {
        char_floor(text, start + EDIT_TEXT_PAGE_BYTES)
    };
    // Control characters other than newline and tab (terminal escapes) are
    // shown as escapes so server text cannot drive the terminal.
    let mut controls = 0usize;
    let shown: String = text[start..stop]
        .chars()
        .flat_map(|ch| {
            if ch.is_control() && !matches!(ch, '\n' | '\t') {
                controls += 1;
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect();
    let mut out = format!(
        "{heading}\nEdit #{number} in {file}: replace {}:{}-{}:{} with the following text (bytes {start}-{stop} of {}):\n<<<<<<< REPLACEMENT\n{}\n>>>>>>> END\n",
        edit.start.0 + 1,
        edit.start.1 + 1,
        edit.end.0 + 1,
        edit.end.1 + 1,
        edit.full_len,
        shown,
    );
    if controls > 0 {
        out.push_str(&format!(
            "[{controls} control characters are shown as escapes]\n"
        ));
    }
    if stop < text.len() {
        out.push_str(&format!(
            "[more replacement text follows; next text_offset={stop}: pass edit={number}, text_offset={stop} to continue]\n"
        ));
    }
    if edit.cut {
        out.push_str(&format!(
            "[only the first {} of {} bytes of this replacement are retained; the rest cannot be shown]\n",
            edit.new_text.len(),
            edit.full_len
        ));
    }
    out.trim_end().to_string()
}

fn render_actions(
    actions: &[crate::extras::lsp::navigate::ActionView],
    truncated: usize,
    indexing: bool,
) -> String {
    if actions.is_empty() {
        return if indexing {
            "No code actions.".to_string()
        } else {
            "No code actions available at that position.".to_string()
        };
    }
    let mut out = String::new();
    for (index, action) in actions.iter().enumerate() {
        out.push_str(&format!("{}. {}", index + 1, action.title));
        if let Some(kind) = &action.kind {
            out.push_str(&format!(" [{kind}]"));
        }
        if action.preferred {
            out.push_str(" (preferred)");
        }
        if action.edit_count > 0 {
            out.push_str(&format!(
                " (edits {} places in {} files)",
                action.edit_count, action.edit_files
            ));
        }
        if action.has_command {
            out.push_str(" (runs a server command; never executed here)");
        }
        if let Some(reason) = &action.disabled {
            out.push_str(&format!(" (disabled: {reason})"));
        }
        out.push('\n');
    }
    if truncated > 0 {
        out.push_str(&format!(
            "[{truncated} more actions not shown (limit {})]\n",
            crate::extras::lsp::navigate::MAX_ACTIONS
        ));
    }
    out.push_str("Pass `action`=N to preview an action's edit.");
    out
}

/// Drops `.` components so `./src`, `src/` and `src//sub` compare against
/// workspace-relative paths by component; an empty result (`.`, `./`, ``)
/// means "no filter" rather than matching nothing.
fn normalize_directory_filter(dir: &Path) -> Option<std::path::PathBuf> {
    let normalized: std::path::PathBuf = dir
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

#[cfg(test)]
mod directory_filter_tests {
    use super::*;

    fn keeps(dir: &str, file: &str) -> bool {
        match normalize_directory_filter(Path::new(dir)) {
            None => true,
            Some(dir) => Path::new(file).starts_with(dir),
        }
    }

    #[test]
    fn curdir_forms_do_not_hide_cached_diagnostics() {
        assert!(keeps(".", "src/a.rs"));
        assert!(keeps("./", "src/a.rs"));
        assert!(keeps("", "src/a.rs"));
        assert!(keeps("./src", "src/a.rs"));
        assert!(keeps("src/", "src/a.rs"));
        assert!(keeps("src//sub", "src/sub/a.rs"));
        assert!(keeps("./src/./sub", "src/sub/a.rs"));
    }

    #[test]
    fn directory_filter_still_excludes_other_trees() {
        assert!(!keeps("./src", "tests/a.rs"));
        assert!(!keeps("src//sub", "src/other/a.rs"));
        assert!(!keeps("src", "srcx/a.rs"));
        assert_eq!(normalize_directory_filter(Path::new(".")), None);
    }
}
