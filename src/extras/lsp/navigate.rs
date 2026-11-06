//! Read-only code navigation through a running language server:
//! definition, references, implementation, type definition, hover, document
//! symbols, workspace symbols, call hierarchy, rename preview and code
//! actions. Requests reuse the capability-bound
//! document sync of the diagnostics path; every result location is mapped
//! back to a canonical workspace path. A location outside the workspace
//! (dependency or standard-library source) is returned as an `external` item
//! with its canonical absolute path when it is an existing regular file; the
//! tool layer re-checks it against the `read` rules before naming it. Anything
//! else (non-file URIs, missing files, control characters, symlink aliases of
//! workspace files) is counted but never named.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::LspManager;

/// Bound for one navigation request; indexing servers can be slow to answer
/// the first query.
const NAV_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an empty answer waits for a server that is still indexing before
/// the result is reported as "still indexing".
const INDEX_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(8)
};
/// Rename/code-action previews: files, edits per file and total edits shown.
pub(crate) const MAX_EDIT_FILES: usize = 50;
pub(crate) const MAX_EDITS_PER_FILE: usize = 50;
pub(crate) const MAX_EDITS_TOTAL: usize = 500;
/// Most code actions listed.
pub(crate) const MAX_ACTIONS: usize = 50;
/// Replacement text in a preview is cut at this many bytes.
const MAX_EDIT_TEXT_BYTES: usize = 200;
/// Longest accepted `new_name`.
pub(crate) const MAX_NEW_NAME_BYTES: usize = 256;
/// Most locations or symbols returned in one result.
pub(crate) const MAX_NAV_ITEMS: usize = 100;
/// Hover text is cut at this many bytes (UTF-8 safe).
pub(crate) const MAX_HOVER_BYTES: usize = 8 * 1024;
/// Symbol names and details are cut at this many bytes.
const MAX_NAME_BYTES: usize = 256;
/// Nested document symbols deeper than this are not listed.
const MAX_SYMBOL_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavOp {
    Definition,
    TypeDefinition,
    Implementation,
    References,
    Hover,
    Symbols,
    WorkspaceSymbols,
    IncomingCalls,
    OutgoingCalls,
    Rename,
    CodeActions,
}

impl NavOp {
    pub(crate) fn parse(op: &str) -> Option<Self> {
        Some(match op {
            "definition" => Self::Definition,
            "type_definition" => Self::TypeDefinition,
            "implementation" => Self::Implementation,
            "references" => Self::References,
            "hover" => Self::Hover,
            "symbols" | "document_symbols" => Self::Symbols,
            "workspace_symbols" => Self::WorkspaceSymbols,
            "incoming_calls" => Self::IncomingCalls,
            "outgoing_calls" => Self::OutgoingCalls,
            "rename" => Self::Rename,
            "code_actions" | "code_action" => Self::CodeActions,
            _ => return None,
        })
    }

    pub(crate) fn needs_position(self) -> bool {
        !matches!(self, Self::Symbols | Self::WorkspaceSymbols)
    }

    fn method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::TypeDefinition => "textDocument/typeDefinition",
            Self::Implementation => "textDocument/implementation",
            Self::References => "textDocument/references",
            Self::Hover => "textDocument/hover",
            Self::Symbols => "textDocument/documentSymbol",
            Self::WorkspaceSymbols => "workspace/symbol",
            Self::IncomingCalls => "callHierarchy/incomingCalls",
            Self::OutgoingCalls => "callHierarchy/outgoingCalls",
            Self::Rename => "textDocument/rename",
            Self::CodeActions => "textDocument/codeAction",
        }
    }

    fn provider(self) -> &'static str {
        match self {
            Self::Definition => "definitionProvider",
            Self::TypeDefinition => "typeDefinitionProvider",
            Self::Implementation => "implementationProvider",
            Self::References => "referencesProvider",
            Self::Hover => "hoverProvider",
            Self::Symbols => "documentSymbolProvider",
            Self::WorkspaceSymbols => "workspaceSymbolProvider",
            Self::IncomingCalls | Self::OutgoingCalls => "callHierarchyProvider",
            Self::Rename => "renameProvider",
            Self::CodeActions => "codeActionProvider",
        }
    }
}

/// One navigation request. `line`/`character` are zero-based LSP positions
/// (character in UTF-16 code units, as the protocol defines).
#[derive(Debug, Clone)]
pub(crate) struct NavQuery {
    pub op: NavOp,
    pub line: u32,
    pub character: u32,
    pub query: String,
    /// code_actions: end of the range (defaults to the start position).
    pub end: Option<(u32, u32)>,
    /// rename: the new identifier.
    pub new_name: String,
    /// code_actions: 1-based index of the action whose edit to preview.
    pub action_index: Option<u32>,
}

/// A result location inside the workspace. Positions are zero-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavItem {
    pub path: PathBuf,
    pub line: u32,
    pub character: u32,
    /// Symbol label (`kind name`) for symbol results, `None` for plain
    /// locations.
    pub label: Option<String>,
    /// Nesting depth for document symbols.
    pub depth: usize,
    /// The location is a canonical absolute path outside the workspace
    /// (dependency, standard library). The tool layer re-checks it against
    /// the `read` permission before naming it.
    pub external: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NavAnswer {
    Items {
        items: Vec<NavItem>,
        /// Locations outside the workspace (or unmappable URIs), omitted.
        outside: usize,
        /// Results dropped by [`MAX_NAV_ITEMS`].
        truncated: usize,
    },
    Hover {
        text: String,
        truncated: bool,
    },
    /// A workspace edit shown for review; nothing is applied.
    Edit(EditPreview),
    Actions {
        actions: Vec<ActionView>,
        truncated: usize,
    },
}

/// One text edit, zero-based positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditView {
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileEdits {
    pub path: PathBuf,
    pub edits: Vec<EditView>,
    /// Edits for this file dropped by the per-file or total cap.
    pub omitted_edits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct EditPreview {
    pub files: Vec<FileEdits>,
    /// `create`/`rename`/`delete` resource operations inside the workspace.
    pub file_ops: Vec<String>,
    /// Files or operations outside the workspace (omitted, never named).
    pub outside: usize,
    /// Files dropped by [`MAX_EDIT_FILES`].
    pub omitted_files: usize,
    pub total_edits: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionView {
    pub title: String,
    pub kind: Option<String>,
    pub preferred: bool,
    pub edit_files: usize,
    pub edit_count: usize,
    pub has_command: bool,
    pub disabled: Option<String>,
}

/// Whether a server answer carries nothing, so it may only mean "not ready".
fn result_is_empty(op: NavOp, result: &Value) -> bool {
    match result {
        Value::Null => true,
        Value::Array(entries) => entries.is_empty(),
        Value::Object(_) if op == NavOp::Hover => hover_text(result).0.is_empty(),
        _ => false,
    }
}

impl LspManager {
    /// Runs one navigation request for an already-authorized, already
    /// synchronized canonical workspace file. The flag is true when the answer
    /// is empty while the server was still indexing, so it is not a verdict.
    pub(crate) async fn navigate(
        &self,
        path: &Path,
        query: &NavQuery,
    ) -> Result<(NavAnswer, bool), String> {
        let client = self
            .client_for(path)
            .await
            .ok_or_else(|| "no language server is available for this file".to_string())?;
        if !client.supports(query.op.provider()) {
            return Err(format!(
                "the language server does not support {} (no {} capability)",
                query.op.method(),
                query.op.provider()
            ));
        }
        let uri = client
            .wire_uri(path)
            .ok_or_else(|| "file is outside the language-server workspace".to_string())?;
        let position = json!({ "line": query.line, "character": query.character });
        let map = |uri: &str| client.parent_path(uri);
        // One request; an empty answer from a server that is still indexing
        // waits (bounded) for it to finish and asks once more.
        let ask = |method: &'static str, params: Value| {
            let client = &client;
            async move {
                let mut result = client.query(method, params.clone(), NAV_TIMEOUT).await?;
                let mut indexing = false;
                if result_is_empty(query.op, &result) && client.is_indexing() {
                    if client.wait_until_idle(INDEX_WAIT).await {
                        result = client.query(method, params, NAV_TIMEOUT).await?;
                    }
                    indexing = client.is_indexing() && result_is_empty(query.op, &result);
                }
                Ok::<_, String>((result, indexing))
            }
        };
        let (answer, indexing) = match query.op {
            NavOp::IncomingCalls | NavOp::OutgoingCalls => {
                let (prepared, indexing) = ask(
                    "textDocument/prepareCallHierarchy",
                    json!({ "textDocument": { "uri": uri }, "position": position }),
                )
                .await?;
                let Some(item) = prepared.as_array().and_then(|items| items.first()).cloned()
                else {
                    return Ok((
                        NavAnswer::Items {
                            items: Vec::new(),
                            outside: 0,
                            truncated: 0,
                        },
                        indexing,
                    ));
                };
                let key = if query.op == NavOp::IncomingCalls {
                    "from"
                } else {
                    "to"
                };
                let calls = client
                    .query(query.op.method(), json!({ "item": item }), NAV_TIMEOUT)
                    .await?;
                (calls_answer(&calls, key, &map), false)
            }
            NavOp::Rename => {
                let (result, indexing) = ask(
                    "textDocument/rename",
                    json!({
                        "textDocument": { "uri": uri },
                        "position": position,
                        "newName": query.new_name,
                    }),
                )
                .await?;
                (NavAnswer::Edit(edit_preview(&result, &map)), indexing)
            }
            NavOp::CodeActions => {
                let (end_line, end_character) = query.end.unwrap_or((query.line, query.character));
                let parent_uri = super::client::file_uri(path).unwrap_or_default();
                let context = self.diagnostics_for_range(&parent_uri, query.line, end_line);
                let (result, indexing) = ask(
                    "textDocument/codeAction",
                    json!({
                        "textDocument": { "uri": uri },
                        "range": {
                            "start": position,
                            "end": { "line": end_line, "character": end_character }
                        },
                        "context": { "diagnostics": context },
                    }),
                )
                .await?;
                let answer = match query.action_index {
                    Some(index) => {
                        let entries = result.as_array().map(Vec::as_slice).unwrap_or_default();
                        let entry = usize::try_from(index)
                            .ok()
                            .and_then(|index| index.checked_sub(1))
                            .and_then(|index| entries.get(index))
                            .ok_or_else(|| {
                                format!(
                                    "no code action number {index}; the server offered {}",
                                    entries.len()
                                )
                            })?;
                        match entry.get("edit") {
                            Some(edit) => NavAnswer::Edit(edit_preview(edit, &map)),
                            None => {
                                return Err(
                                    "that code action carries no edit (server commands are never executed)"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    None => actions_answer(&result),
                };
                (answer, indexing)
            }
            op => {
                let params = match op {
                    NavOp::Definition
                    | NavOp::TypeDefinition
                    | NavOp::Implementation
                    | NavOp::Hover => {
                        json!({ "textDocument": { "uri": uri }, "position": position })
                    }
                    NavOp::References => json!({
                        "textDocument": { "uri": uri },
                        "position": position,
                        "context": { "includeDeclaration": true }
                    }),
                    NavOp::Symbols => json!({ "textDocument": { "uri": uri } }),
                    _ => json!({ "query": query.query }),
                };
                let (result, indexing) = ask(op.method(), params).await?;
                let answer = match op {
                    NavOp::Hover => {
                        let (text, truncated) = hover_text(&result);
                        NavAnswer::Hover { text, truncated }
                    }
                    NavOp::Symbols => symbols_answer(&result, Some(path), &map),
                    NavOp::WorkspaceSymbols => symbols_answer(&result, None, &map),
                    _ => locations_answer(&result, &map),
                };
                (answer, indexing)
            }
        };
        Ok((answer, indexing))
    }

    /// Cached diagnostics whose line span overlaps `[start, end]`, as LSP
    /// JSON for a code-action request context.
    fn diagnostics_for_range(&self, uri: &str, start: u32, end: u32) -> Vec<Value> {
        self.inner
            .diags
            .lock()
            .unwrap()
            .get(uri)
            .map(|entry| {
                entry
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| {
                        diagnostic.range.start.line <= end && diagnostic.range.end.line >= start
                    })
                    .take(20)
                    .filter_map(|diagnostic| serde_json::to_value(diagnostic).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// `CallHierarchyIncomingCall[]` / `OutgoingCall[]`: the item under `key`
/// (`from` or `to`) of each entry, shown at its selection range.
pub(crate) fn calls_answer(
    result: &Value,
    key: &str,
    map: &dyn Fn(&str) -> Option<PathBuf>,
) -> NavAnswer {
    let mut items: Vec<NavItem> = Vec::new();
    let mut outside = 0;
    let mut truncated = 0;
    for entry in result.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(call) = entry.get(key) else {
            continue;
        };
        let uri = call.get("uri").and_then(Value::as_str);
        let (path, external) = match uri.and_then(map) {
            Some(path) => (path, false),
            None => match uri.and_then(external_location) {
                Some(path) => (path, true),
                None => {
                    outside += 1;
                    continue;
                }
            },
        };
        let (line, character) =
            position_of(call.get("selectionRange").or_else(|| call.get("range")));
        let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = symbol_kind(call.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let item = NavItem {
            path,
            line,
            character,
            label: Some(format!("{kind} {}", clean_name(name))),
            depth: 0,
            external,
        };
        if items.contains(&item) {
            continue;
        }
        if items.len() >= MAX_NAV_ITEMS {
            truncated += 1;
            continue;
        }
        items.push(item);
    }
    NavAnswer::Items {
        items,
        outside,
        truncated,
    }
}

/// A `WorkspaceEdit` (`changes` and/or `documentChanges`) reduced to a bounded
/// per-file preview. Only workspace files are named; anything else is counted.
pub(crate) fn edit_preview(edit: &Value, map: &dyn Fn(&str) -> Option<PathBuf>) -> EditPreview {
    let mut preview = EditPreview::default();
    let add = |preview: &mut EditPreview, uri: &str, edits: &Value| {
        let Some(path) = map(uri) else {
            preview.outside += 1;
            return;
        };
        let entries = edits.as_array().map(Vec::as_slice).unwrap_or_default();
        preview.total_edits += entries.len();
        let index = match preview.files.iter().position(|file| file.path == path) {
            Some(index) => index,
            None if preview.files.len() >= MAX_EDIT_FILES => {
                preview.omitted_files += 1;
                return;
            }
            None => {
                preview.files.push(FileEdits {
                    path,
                    edits: Vec::new(),
                    omitted_edits: 0,
                });
                preview.files.len() - 1
            }
        };
        let shown_total: usize = preview.files.iter().map(|file| file.edits.len()).sum();
        let file = &mut preview.files[index];
        for (offset, entry) in entries.iter().enumerate() {
            let range = entry.get("range");
            if file.edits.len() >= MAX_EDITS_PER_FILE || shown_total + offset >= MAX_EDITS_TOTAL {
                file.omitted_edits += 1;
                continue;
            }
            let end = range.and_then(|range| range.get("end"));
            let get = |key: &str| {
                end.and_then(|end| end.get(key))
                    .and_then(Value::as_u64)
                    .map_or(0, |value| value.min(u64::from(u32::MAX)) as u32)
            };
            file.edits.push(EditView {
                start: position_of(range),
                end: (get("line"), get("character")),
                new_text: truncate(
                    entry.get("newText").and_then(Value::as_str).unwrap_or(""),
                    MAX_EDIT_TEXT_BYTES,
                )
                .0,
            });
        }
    };
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            add(&mut preview, uri, edits);
        }
    }
    for change in edit
        .get("documentChanges")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match change.get("kind").and_then(Value::as_str) {
            Some(kind @ ("create" | "delete")) => {
                match change.get("uri").and_then(Value::as_str).and_then(map) {
                    Some(path) if preview.file_ops.len() < MAX_EDIT_FILES => preview.file_ops.push(
                        format!("{kind} {}", clean_name(&path.display().to_string())),
                    ),
                    Some(_) => preview.omitted_files += 1,
                    None => preview.outside += 1,
                }
            }
            Some("rename") => {
                let old = change.get("oldUri").and_then(Value::as_str).and_then(map);
                let new = change.get("newUri").and_then(Value::as_str).and_then(map);
                match (old, new) {
                    (Some(old), Some(new)) if preview.file_ops.len() < MAX_EDIT_FILES => {
                        preview.file_ops.push(format!(
                            "rename {} -> {}",
                            clean_name(&old.display().to_string()),
                            clean_name(&new.display().to_string())
                        ));
                    }
                    (Some(_), Some(_)) => preview.omitted_files += 1,
                    _ => preview.outside += 1,
                }
            }
            _ => {
                let uri = change
                    .get("textDocument")
                    .and_then(|document| document.get("uri"))
                    .and_then(Value::as_str);
                if let (Some(uri), Some(edits)) = (uri, change.get("edits")) {
                    add(&mut preview, uri, edits);
                }
            }
        }
    }
    preview
}

/// `(Command | CodeAction)[]` reduced to titles with an edit summary. Server
/// commands are described, never executed.
pub(crate) fn actions_answer(result: &Value) -> NavAnswer {
    let entries = result.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut actions = Vec::new();
    let mut truncated = 0;
    for entry in entries {
        if actions.len() >= MAX_ACTIONS {
            truncated += 1;
            continue;
        }
        let (mut edit_files, mut edit_count) = (0, 0);
        if let Some(edit) = entry.get("edit") {
            let mut files = std::collections::HashSet::new();
            if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
                for (uri, edits) in changes {
                    files.insert(uri.clone());
                    edit_count += edits.as_array().map_or(0, Vec::len);
                }
            }
            for change in edit
                .get("documentChanges")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if let Some(uri) = change
                    .get("textDocument")
                    .and_then(|document| document.get("uri"))
                    .and_then(Value::as_str)
                {
                    files.insert(uri.to_string());
                    edit_count += change
                        .get("edits")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                } else {
                    edit_count += 1;
                }
            }
            edit_files = files.len();
        }
        let has_command = entry
            .get("command")
            .is_some_and(|command| command.is_string() || command.get("command").is_some());
        actions.push(ActionView {
            title: clean_name(entry.get("title").and_then(Value::as_str).unwrap_or("?")),
            kind: entry.get("kind").and_then(Value::as_str).map(clean_name),
            preferred: entry.get("isPreferred").and_then(Value::as_bool) == Some(true),
            edit_files,
            edit_count,
            has_command,
            disabled: entry
                .get("disabled")
                .and_then(|disabled| disabled.get("reason"))
                .and_then(Value::as_str)
                .map(clean_name),
        });
    }
    NavAnswer::Actions { actions, truncated }
}

fn position_of(range: Option<&Value>) -> (u32, u32) {
    let start = range.and_then(|range| range.get("start"));
    let get = |key: &str| {
        start
            .and_then(|start| start.get(key))
            .and_then(Value::as_u64)
            .map_or(0, |value| value.min(u64::from(u32::MAX)) as u32)
    };
    (get("line"), get("character"))
}

fn truncate(value: &str, max: usize) -> (String, bool) {
    if value.len() <= max {
        return (value.to_string(), false);
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

fn clean_name(value: &str) -> String {
    let single_line: String = value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    truncate(single_line.trim(), MAX_NAME_BYTES).0
}

/// Longest absolute path named for a location outside the workspace.
const MAX_EXTERNAL_PATH_BYTES: usize = 4096;

/// The canonical absolute path of a `file:` URI the server reported outside
/// the workspace (dependency source, standard library). Only an existing
/// regular file is named; the canonical target is used, so a symlink alias
/// never changes which path the permission check judges. Paths that are not
/// UTF-8, carry control characters (a hostile server could use them to forge
/// output lines) or exceed [`MAX_EXTERNAL_PATH_BYTES`] stay omitted.
fn external_location(uri: &str) -> Option<PathBuf> {
    let path = super::client::file_path(uri)?;
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    let text = canonical.to_str()?;
    if text.len() > MAX_EXTERNAL_PATH_BYTES || text.chars().any(char::is_control) {
        return None;
    }
    std::fs::metadata(&canonical)
        .ok()?
        .is_file()
        .then_some(canonical)
}

/// Location, Location[] or LocationLink[] (definition-style responses).
pub(crate) fn locations_answer(result: &Value, map: &dyn Fn(&str) -> Option<PathBuf>) -> NavAnswer {
    let entries: Vec<&Value> = match result {
        Value::Array(entries) => entries.iter().collect(),
        Value::Object(_) => vec![result],
        _ => Vec::new(),
    };
    let mut items = Vec::new();
    let mut outside = 0;
    let mut truncated = 0;
    for entry in entries {
        let (uri, range) = match entry.get("targetUri") {
            Some(uri) => (
                uri.as_str(),
                entry
                    .get("targetSelectionRange")
                    .or_else(|| entry.get("targetRange")),
            ),
            None => (entry.get("uri").and_then(Value::as_str), entry.get("range")),
        };
        let (path, external) = match uri.and_then(map) {
            Some(path) => (path, false),
            None => match uri.and_then(external_location) {
                Some(path) => (path, true),
                None => {
                    outside += 1;
                    continue;
                }
            },
        };
        let (line, character) = position_of(range);
        let item = NavItem {
            path,
            line,
            character,
            label: None,
            depth: 0,
            external,
        };
        if items.contains(&item) {
            continue;
        }
        if items.len() >= MAX_NAV_ITEMS {
            truncated += 1;
            continue;
        }
        items.push(item);
    }
    NavAnswer::Items {
        items,
        outside,
        truncated,
    }
}

/// DocumentSymbol[] (nested), SymbolInformation[] or WorkspaceSymbol[].
/// `document` is the queried file for document symbols, whose nested form
/// carries no URI.
pub(crate) fn symbols_answer(
    result: &Value,
    document: Option<&Path>,
    map: &dyn Fn(&str) -> Option<PathBuf>,
) -> NavAnswer {
    let mut items = Vec::new();
    let mut outside = 0;
    let mut truncated = 0;
    let mut stack: Vec<(&Value, usize)> = result
        .as_array()
        .map(|entries| entries.iter().rev().map(|entry| (entry, 0)).collect())
        .unwrap_or_default();
    while let Some((entry, depth)) = stack.pop() {
        let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = symbol_kind(entry.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let detail = entry
            .get("containerName")
            .and_then(Value::as_str)
            .filter(|container| !container.is_empty())
            .map(|container| format!(" (in {})", clean_name(container)))
            .unwrap_or_default();
        let (path, range, external) = match entry.get("location") {
            Some(location) => {
                let uri = location.get("uri").and_then(Value::as_str);
                match uri.and_then(map) {
                    Some(path) => (Some(path), location.get("range"), false),
                    None => {
                        let path = uri.and_then(external_location);
                        let external = path.is_some();
                        (path, location.get("range"), external)
                    }
                }
            }
            None => (
                document.map(Path::to_path_buf),
                entry.get("selectionRange").or_else(|| entry.get("range")),
                false,
            ),
        };
        if depth < MAX_SYMBOL_DEPTH
            && let Some(children) = entry.get("children").and_then(Value::as_array)
        {
            stack.extend(children.iter().rev().map(|child| (child, depth + 1)));
        }
        let Some(path) = path else {
            outside += 1;
            continue;
        };
        if items.len() >= MAX_NAV_ITEMS {
            truncated += 1;
            continue;
        }
        let (line, character) = position_of(range);
        items.push(NavItem {
            path,
            line,
            character,
            label: Some(format!("{kind} {}{detail}", clean_name(name))),
            depth,
            external,
        });
    }
    NavAnswer::Items {
        items,
        outside,
        truncated,
    }
}

/// Hover `contents`: MarkupContent, MarkedString or MarkedString[].
pub(crate) fn hover_text(result: &Value) -> (String, bool) {
    fn piece(value: &Value) -> Option<String> {
        match value {
            Value::String(text) => Some(text.clone()),
            Value::Object(object) => {
                let text = object.get("value")?.as_str()?;
                match object.get("language").and_then(Value::as_str) {
                    Some(language) => Some(format!("```{language}\n{text}\n```")),
                    None => Some(text.to_string()),
                }
            }
            _ => None,
        }
    }
    let contents = result.get("contents").unwrap_or(&Value::Null);
    let text = match contents {
        Value::Array(parts) => parts
            .iter()
            .filter_map(piece)
            .collect::<Vec<_>>()
            .join("\n\n"),
        other => piece(other).unwrap_or_default(),
    };
    truncate(text.trim(), MAX_HOVER_BYTES)
}

fn symbol_kind(kind: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enum-member",
        "struct",
        "event",
        "operator",
        "type-parameter",
    ];
    usize::try_from(kind)
        .ok()
        .and_then(|kind| kind.checked_sub(1))
        .and_then(|index| KINDS.get(index))
        .copied()
        .unwrap_or("symbol")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(uri: &str) -> Option<PathBuf> {
        uri.strip_prefix("file:///ws/")
            .map(|rest| PathBuf::from("/ws").join(rest))
    }

    #[test]
    fn locations_accept_scalar_array_and_links_and_hide_outside_paths() {
        let range =
            json!({"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 5}});
        let scalar = locations_answer(&json!({"uri": "file:///ws/a.rs", "range": range}), &map);
        assert_eq!(
            scalar,
            NavAnswer::Items {
                items: vec![NavItem {
                    path: "/ws/a.rs".into(),
                    line: 4,
                    character: 2,
                    label: None,
                    depth: 0,
                    external: false,
                }],
                outside: 0,
                truncated: 0
            }
        );
        let links = locations_answer(
            &json!([
                {"targetUri": "file:///ws/b.rs", "targetRange": range, "targetSelectionRange":
                    {"start": {"line": 9, "character": 1}}},
                {"uri": "file:///home/user/.rustup/lib.rs", "range": range},
                {"uri": "file:///ws/a.rs", "range": range},
                {"uri": "file:///ws/a.rs", "range": range}
            ]),
            &map,
        );
        let NavAnswer::Items { items, outside, .. } = links else {
            panic!("items");
        };
        assert_eq!(outside, 1);
        assert_eq!(items.len(), 2, "duplicates collapse");
        assert_eq!((items[0].line, items[0].character), (9, 1));
        assert_eq!(
            locations_answer(&Value::Null, &map),
            NavAnswer::Items {
                items: Vec::new(),
                outside: 0,
                truncated: 0
            }
        );
    }

    #[test]
    fn locations_are_capped_with_a_count() {
        let many: Vec<Value> = (0..150)
            .map(|line| json!({"uri": "file:///ws/a.rs", "range": {"start": {"line": line, "character": 0}}}))
            .collect();
        let NavAnswer::Items {
            items, truncated, ..
        } = locations_answer(&Value::Array(many), &map)
        else {
            panic!("items");
        };
        assert_eq!(items.len(), MAX_NAV_ITEMS);
        assert_eq!(truncated, 50);
    }

    #[test]
    fn symbols_flatten_nested_and_flat_forms() {
        let nested = json!([{
            "name": "Outer", "kind": 23, "range": {}, "selectionRange": {"start": {"line": 1, "character": 7}},
            "children": [{"name": "inner\nfn", "kind": 6, "selectionRange": {"start": {"line": 2, "character": 4}}}]
        }]);
        let NavAnswer::Items { items, .. } =
            symbols_answer(&nested, Some(Path::new("/ws/a.rs")), &map)
        else {
            panic!("items");
        };
        assert_eq!(items[0].label.as_deref(), Some("struct Outer"));
        assert_eq!(items[1].label.as_deref(), Some("method inner fn"));
        assert_eq!((items[1].depth, items[1].line), (1, 2));
        let flat = json!([
            {"name": "f", "kind": 12, "containerName": "m",
             "location": {"uri": "file:///ws/b.rs", "range": {"start": {"line": 3, "character": 0}}}},
            {"name": "g", "kind": 12, "location": {"uri": "file:///elsewhere/c.rs"}}
        ]);
        let NavAnswer::Items { items, outside, .. } = symbols_answer(&flat, None, &map) else {
            panic!("items");
        };
        assert_eq!(outside, 1);
        assert_eq!(items[0].label.as_deref(), Some("function f (in m)"));
        assert_eq!(items[0].path, PathBuf::from("/ws/b.rs"));
    }

    #[test]
    fn hover_accepts_markup_marked_strings_and_is_capped() {
        assert_eq!(
            hover_text(&json!({"contents": {"kind": "markdown", "value": "**x**"}})),
            ("**x**".to_string(), false)
        );
        assert_eq!(
            hover_text(&json!({"contents": [{"language": "rust", "value": "fn f()"}, "doc"]})).0,
            "```rust\nfn f()\n```\n\ndoc"
        );
        let (text, truncated) = hover_text(&json!({"contents": "é".repeat(MAX_HOVER_BYTES)}));
        assert!(truncated);
        assert!(text.len() <= MAX_HOVER_BYTES);
        assert_eq!(hover_text(&Value::Null), (String::new(), false));
    }

    #[test]
    fn ops_parse_and_report_position_needs() {
        assert_eq!(NavOp::parse("definition"), Some(NavOp::Definition));
        assert_eq!(NavOp::parse("symbols"), Some(NavOp::Symbols));
        assert_eq!(NavOp::parse("rename"), Some(NavOp::Rename));
        assert_eq!(NavOp::parse("incoming_calls"), Some(NavOp::IncomingCalls));
        assert_eq!(NavOp::parse("code_actions"), Some(NavOp::CodeActions));
        assert!(NavOp::parse("apply_rename").is_none());
        assert!(NavOp::Rename.needs_position());
        assert!(NavOp::References.needs_position());
        assert!(!NavOp::WorkspaceSymbols.needs_position());
    }
}

#[cfg(test)]
mod external_tests {
    use super::*;

    fn none(_: &str) -> Option<PathBuf> {
        None
    }

    #[test]
    fn existing_outside_file_is_named_canonically_and_flagged() {
        let dir = std::env::temp_dir().join(format!("nav-external-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rs");
        std::fs::write(&file, "x").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        let uri = super::super::client::file_uri(&canonical).unwrap();
        let range = json!({"start": {"line": 1, "character": 2}});
        let answer = locations_answer(&json!([{"uri": uri, "range": range}]), &none);
        let NavAnswer::Items { items, outside, .. } = answer else {
            panic!("items");
        };
        assert_eq!(outside, 0);
        assert_eq!(items.len(), 1);
        assert!(items[0].external);
        assert_eq!(items[0].path, canonical);
        // Directories, missing files, non-file schemes and relative URIs
        // stay omitted (counted, never named).
        let bad = locations_answer(
            &json!([
                {"uri": super::super::client::file_uri(&std::fs::canonicalize(&dir).unwrap()).unwrap(), "range": range},
                {"uri": "file:///definitely/missing/lib.rs", "range": range},
                {"uri": "jar:file:///x.jar!/A.class", "range": range},
                {"uri": "https://example.invalid/lib.rs", "range": range},
            ]),
            &none,
        );
        let NavAnswer::Items { items, outside, .. } = bad else {
            panic!("items");
        };
        assert!(items.is_empty());
        assert_eq!(outside, 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn control_characters_in_an_outside_path_are_never_named() {
        let dir = std::env::temp_dir().join(format!("nav-ctrl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("evil\nINJECTED line.rs");
        std::fs::write(&file, "x").unwrap();
        let uri = super::super::client::file_uri(&std::fs::canonicalize(&file).unwrap()).unwrap();
        let answer = locations_answer(
            &json!({"uri": uri, "range": {"start": {"line": 0, "character": 0}}}),
            &none,
        );
        let NavAnswer::Items { items, outside, .. } = answer else {
            panic!("items");
        };
        assert!(items.is_empty());
        assert_eq!(outside, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
