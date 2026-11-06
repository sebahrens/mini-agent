//! Read-only code navigation through a running language server:
//! definition, references, implementation, type definition, hover, document
//! symbols and workspace symbols. Requests reuse the capability-bound
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
}

impl LspManager {
    /// Runs one navigation request for an already-authorized, already
    /// synchronized canonical workspace file.
    pub(crate) async fn navigate(
        &self,
        path: &Path,
        query: &NavQuery,
    ) -> Result<NavAnswer, String> {
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
        let params = match query.op {
            NavOp::Definition | NavOp::TypeDefinition | NavOp::Implementation | NavOp::Hover => {
                json!({ "textDocument": { "uri": uri }, "position": position })
            }
            NavOp::References => json!({
                "textDocument": { "uri": uri },
                "position": position,
                "context": { "includeDeclaration": true }
            }),
            NavOp::Symbols => json!({ "textDocument": { "uri": uri } }),
            NavOp::WorkspaceSymbols => json!({ "query": query.query }),
        };
        let result = client.query(query.op.method(), params, NAV_TIMEOUT).await?;
        let map = |uri: &str| client.parent_path(uri);
        Ok(match query.op {
            NavOp::Hover => {
                let (text, truncated) = hover_text(&result);
                NavAnswer::Hover { text, truncated }
            }
            NavOp::Symbols => symbols_answer(&result, Some(path), &map),
            NavOp::WorkspaceSymbols => symbols_answer(&result, None, &map),
            _ => locations_answer(&result, &map),
        })
    }
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
        assert!(NavOp::parse("rename").is_none());
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
