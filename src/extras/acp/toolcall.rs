//! ACP tool-call presentation: titles, kinds, file locations and diff content.
//!
//! Everything here is display metadata derived from arguments the model sent
//! or from bounded [`MutationDiff`] rows the tool published. Nothing reads the
//! filesystem, and every string is bounded and sanitised before it reaches an
//! IDE.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use agent_client_protocol::schema::v1::{Diff, ToolCallContent, ToolCallLocation, ToolKind};
use serde_json::Value;

use crate::agent::tools::mutation_diff::{DiffKind, MAX_DIFF_BYTES, MutationDiff};

const TITLE_MAX_CHARS: usize = 120;
/// Edit blocks previewed in one permission request.
const MAX_PREVIEW_BLOCKS: usize = 16;
/// Pending edit/write argument sets kept for permission previews.
const MAX_PENDING: usize = 64;

pub(super) fn kind_for(tool: &str) -> ToolKind {
    match tool {
        "read" | "list_dir" | "lsp_navigate" => ToolKind::Read,
        "edit" | "write" | "notebook_edit" => ToolKind::Edit,
        "shell" | "job_status" => ToolKind::Execute,
        "grep" | "find_files" | "web_search" | "lsp_diagnostics" => ToolKind::Search,
        "web_fetch" => ToolKind::Fetch,
        "todo_write" | "todo_read" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

fn clip(text: &str, max: usize) -> String {
    let clean = crate::ui::events::sanitize_for_review(text);
    let one_line: String = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut out: String = one_line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)?.as_str().filter(|value| !value.is_empty())
}

/// Descriptive title: the tool plus its path, command or pattern.
pub(super) fn title_for(tool: &str, args: &Value) -> String {
    let subject = match tool {
        "shell" => str_arg(args, "command"),
        "grep" | "find_files" => str_arg(args, "pattern"),
        "web_fetch" => str_arg(args, "url"),
        "web_search" => str_arg(args, "query"),
        _ => str_arg(args, "path"),
    };
    match subject {
        Some(subject) => clip(&format!("{tool} {subject}"), TITLE_MAX_CHARS),
        None => clip(tool, TITLE_MAX_CHARS),
    }
}

/// Files a path-taking tool touches, for follow-along in the IDE.
pub(super) fn locations_for(tool: &str, args: &Value) -> Vec<ToolCallLocation> {
    if !matches!(
        tool,
        "read" | "edit" | "write" | "notebook_edit" | "list_dir" | "grep" | "find_files"
    ) {
        return Vec::new();
    }
    let Some(path) = str_arg(args, "path") else {
        return Vec::new();
    };
    let mut location = ToolCallLocation::new(path);
    if tool == "read"
        && let Some(offset) = args.get("offset").and_then(Value::as_u64)
        && let Ok(offset) = u32::try_from(offset)
    {
        location = location.line(offset.saturating_sub(1));
    }
    vec![location]
}

fn bounded_text(text: &str) -> (String, bool) {
    if text.len() <= MAX_DIFF_BYTES {
        return (text.to_owned(), false);
    }
    let mut end = MAX_DIFF_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn truncated_meta() -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([("mini-agent-truncated".to_owned(), Value::Bool(true))])
}

/// Diff shown in the approval dialog, computed from the proposed arguments
/// only. `edit` with `edits` operations has no text-level preview before it
/// runs; its recorded mutation preview follows the result.
pub(super) fn pending_diffs(tool: &str, args: &Value) -> Vec<ToolCallContent> {
    let Some(path) = str_arg(args, "path") else {
        return Vec::new();
    };
    match tool {
        "write" => {
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return Vec::new();
            };
            let (text, truncated) = bounded_text(content);
            let mut diff = Diff::new(path, text);
            if truncated {
                diff.meta = Some(truncated_meta());
            }
            vec![ToolCallContent::Diff(diff)]
        }
        "edit" => {
            let Some(raw) = str_arg(args, "block") else {
                return Vec::new();
            };
            let Ok(blocks) = crate::agent::tools::edit::parse_blocks(raw) else {
                return Vec::new();
            };
            let mut budget = MAX_DIFF_BYTES;
            let mut out = Vec::new();
            for block in blocks.into_iter().take(MAX_PREVIEW_BLOCKS) {
                if budget == 0 {
                    break;
                }
                let (old, old_cut) = bounded_text(&block.search);
                let (new, new_cut) = bounded_text(&block.replace);
                budget = budget.saturating_sub(old.len() + new.len());
                let mut diff = Diff::new(path, new).old_text(old);
                if old_cut || new_cut {
                    diff.meta = Some(truncated_meta());
                }
                out.push(ToolCallContent::Diff(diff));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The recorded mutation as ACP diff content. The rows are already bounded
/// hunks, so `old_text`/`new_text` carry the hunk lines (context included),
/// not whole files; `mini-agent-partial` marks that.
pub(super) fn mutation_diff_content(diff: &MutationDiff) -> Option<ToolCallContent> {
    if !diff.bounded() || diff.content_unchanged {
        return None;
    }
    let mut old = Vec::new();
    let mut new = Vec::new();
    for row in &diff.rows {
        let text = crate::ui::events::sanitize_for_review(&row.text);
        match row.kind {
            DiffKind::Context => {
                old.push(text.to_string());
                new.push(text.to_string());
            }
            DiffKind::Delete => old.push(text.to_string()),
            DiffKind::Add => new.push(text.to_string()),
            DiffKind::Hunk | DiffKind::Notice => {}
        }
    }
    if old.is_empty() && new.is_empty() {
        return None;
    }
    let created = diff.before_sha256.is_none();
    let mut out = Diff::new(diff.path.as_str(), new.join("\n"));
    if !created {
        out = out.old_text(old.join("\n"));
    }
    let mut meta = serde_json::Map::new();
    meta.insert("mini-agent-partial".to_owned(), Value::Bool(!created));
    if diff.omitted_rows > 0 || diff.clipped_rows > 0 {
        meta.insert("mini-agent-truncated".to_owned(), Value::Bool(true));
    }
    out.meta = Some(meta);
    Some(ToolCallContent::Diff(out))
}

type PendingKey = String;

fn pending() -> &'static Mutex<VecDeque<(PendingKey, Value)>> {
    static PENDING: OnceLock<Mutex<VecDeque<(PendingKey, Value)>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Remember an edit/write call's arguments until its result, so the
/// permission bridge (a separate task) can build a diff for the dialog.
pub(super) fn remember_args(tool_call_id: &str, tool: &str, args: &Value) {
    if !matches!(tool, "edit" | "write") {
        return;
    }
    let mut guard = pending().lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|(id, _)| id != tool_call_id);
    if guard.len() >= MAX_PENDING {
        guard.pop_front();
    }
    guard.push_back((tool_call_id.to_owned(), args.clone()));
}

pub(super) fn recall_args(tool_call_id: &str) -> Option<Value> {
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(id, _)| id == tool_call_id)
        .map(|(_, args)| args.clone())
}

pub(super) fn forget_args(tool_call_id: &str) {
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(id, _)| id != tool_call_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_titles_and_locations() {
        assert_eq!(kind_for("edit"), ToolKind::Edit);
        assert_eq!(kind_for("shell"), ToolKind::Execute);
        assert_eq!(kind_for("grep"), ToolKind::Search);
        assert_eq!(kind_for("web_fetch"), ToolKind::Fetch);
        assert_eq!(kind_for("mystery"), ToolKind::Other);
        let args = json!({"path": "src/a.rs", "offset": 10});
        assert_eq!(title_for("read", &args), "read src/a.rs");
        let locations = locations_for("read", &args);
        assert_eq!(locations[0].path.to_str(), Some("src/a.rs"));
        assert_eq!(locations[0].line, Some(9));
        assert!(locations_for("shell", &json!({"command": "ls"})).is_empty());
    }

    #[test]
    fn title_is_single_line_and_bounded() {
        let command = format!("echo a\n\u{202e}{}", "x".repeat(500));
        let title = title_for("shell", &json!({ "command": command }));
        assert!(!title.contains('\n'));
        assert!(!title.contains('\u{202e}'));
        assert!(title.chars().count() <= TITLE_MAX_CHARS);
    }

    #[test]
    fn write_preview_is_bounded_and_marked() {
        let big = "y".repeat(MAX_DIFF_BYTES * 2);
        let out = pending_diffs("write", &json!({"path": "a.txt", "content": big}));
        let [ToolCallContent::Diff(diff)] = out.as_slice() else {
            panic!("expected one diff");
        };
        assert_eq!(diff.new_text.len(), MAX_DIFF_BYTES);
        assert!(
            diff.meta
                .as_ref()
                .unwrap()
                .contains_key("mini-agent-truncated")
        );
    }

    #[test]
    fn edit_block_preview_and_hostile_inputs() {
        let block = "<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE";
        let out = pending_diffs("edit", &json!({"path": "a.rs", "block": block}));
        let [ToolCallContent::Diff(diff)] = out.as_slice() else {
            panic!("expected one diff");
        };
        assert_eq!(diff.old_text.as_deref(), Some("old"));
        assert_eq!(diff.new_text, "new");
        assert!(pending_diffs("edit", &json!({"path": "a.rs", "block": "garbage"})).is_empty());
        assert!(pending_diffs("edit", &json!({"block": block})).is_empty());
        assert!(pending_diffs("write", &json!({"path": 5, "content": "x"})).is_empty());
        assert!(pending_diffs("shell", &json!({"path": "a"})).is_empty());
    }

    #[test]
    fn pending_args_are_bounded() {
        for index in 0..(MAX_PENDING + 10) {
            remember_args(&format!("bound-{index}"), "write", &json!({"path": "a"}));
        }
        assert!(recall_args("bound-0").is_none());
        assert!(recall_args(&format!("bound-{}", MAX_PENDING + 9)).is_some());
        forget_args(&format!("bound-{}", MAX_PENDING + 9));
        assert!(recall_args(&format!("bound-{}", MAX_PENDING + 9)).is_none());
        remember_args("shell-1", "shell", &json!({"command": "x"}));
        assert!(recall_args("shell-1").is_none());
    }
}
