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
#[cfg(test)]
pub(super) fn pending_diffs(tool: &str, args: &Value) -> Vec<ToolCallContent> {
    pending_preview(tool, args).0
}

/// Approval-dialog content: the diffs of [`pending_diffs`] followed, when any
/// part of the proposed change is not shown, by a visible text block saying
/// what is missing. `_meta` markers are ignored by IDE clients, so the notice
/// is ordinary content.
pub(super) fn pending_content(tool: &str, args: &Value) -> Vec<ToolCallContent> {
    let (mut content, notice) = pending_preview(tool, args);
    if let Some(notice) = notice {
        content.push(ToolCallContent::from(
            agent_client_protocol::schema::v1::ContentBlock::Text(
                agent_client_protocol::schema::v1::TextContent::new(notice),
            ),
        ));
    }
    content
}

fn pending_preview(tool: &str, args: &Value) -> (Vec<ToolCallContent>, Option<String>) {
    let Some(path) = str_arg(args, "path") else {
        return (Vec::new(), None);
    };
    match tool {
        "write" => {
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return (Vec::new(), None);
            };
            let (text, truncated) = bounded_text(content);
            let mut diff = Diff::new(path, text);
            let mut notice = None;
            if truncated {
                diff.meta = Some(truncated_meta());
                notice = Some(format!(
                    "Preview truncated: file content beyond {} KiB is not shown ({} bytes total). \
                     Approve only if you trust the whole write.",
                    MAX_DIFF_BYTES / 1024,
                    content.len()
                ));
            }
            (vec![ToolCallContent::Diff(diff)], notice)
        }
        "edit" => {
            let Some(raw) = str_arg(args, "block") else {
                return (
                    Vec::new(),
                    Some(
                        "No text preview is available for this edit; review the request below."
                            .to_owned(),
                    ),
                );
            };
            let Ok(blocks) = crate::agent::tools::edit::parse_blocks(raw) else {
                return (
                    Vec::new(),
                    Some(
                        "Preview unavailable: the edit blocks could not be parsed, so no diff is \
                         shown. Review the request below."
                            .to_owned(),
                    ),
                );
            };
            let total = blocks.len();
            let mut budget = MAX_DIFF_BYTES;
            let mut out = Vec::new();
            let mut cut_blocks = 0usize;
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
                    cut_blocks += 1;
                }
                out.push(ToolCallContent::Diff(diff));
            }
            // Blocks skipped by count or byte budget would otherwise vanish
            // silently: mark the last emitted diff and say how many.
            let skipped = total.saturating_sub(out.len());
            if skipped > 0
                && let Some(ToolCallContent::Diff(last)) = out.last_mut()
            {
                let meta = last.meta.get_or_insert_with(truncated_meta);
                meta.insert("mini-agent-truncated".to_owned(), Value::Bool(true));
                meta.insert(
                    "mini-agent-omitted-blocks".to_owned(),
                    Value::from(skipped as u64),
                );
            }
            let mut parts = Vec::new();
            if skipped > 0 {
                parts.push(format!("{skipped} more edit block(s) not shown"));
            }
            if cut_blocks > 0 {
                parts.push(format!(
                    "{cut_blocks} block(s) cut at {} KiB",
                    MAX_DIFF_BYTES / 1024
                ));
            }
            let notice = (!parts.is_empty()).then(|| {
                format!(
                    "Preview truncated: {}. The change being approved is larger than shown.",
                    parts.join("; ")
                )
            });
            (out, notice)
        }
        _ => (Vec::new(), None),
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

    fn blocks(n: usize, body: &str) -> String {
        (0..n)
            .map(|i| format!("<<<<<<< SEARCH\nold{i}{body}\n=======\nnew{i}\n>>>>>>> REPLACE"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn omitted(out: &[ToolCallContent]) -> Option<u64> {
        out.iter().rev().find_map(|c| match c {
            ToolCallContent::Diff(d) => d.meta.as_ref()?.get("mini-agent-omitted-blocks")?.as_u64(),
            _ => None,
        })
    }

    #[test]
    fn skipped_blocks_by_count_are_marked_with_count() {
        let out = pending_diffs("edit", &json!({"path": "a.rs", "block": blocks(20, "")}));
        assert_eq!(out.len(), MAX_PREVIEW_BLOCKS);
        assert_eq!(omitted(&out), Some(4));
        let ToolCallContent::Diff(last) = out.last().unwrap() else {
            panic!()
        };
        assert!(
            last.meta
                .as_ref()
                .unwrap()
                .contains_key("mini-agent-truncated")
        );
        let full = pending_diffs("edit", &json!({"path": "a.rs", "block": blocks(16, "")}));
        assert_eq!(omitted(&full), None);
    }

    #[test]
    fn skipped_blocks_by_byte_budget_are_marked_with_count() {
        let body = "z".repeat(MAX_DIFF_BYTES / 2);
        let out = pending_diffs("edit", &json!({"path": "a.rs", "block": blocks(6, &body)}));
        assert!(out.len() < 6);
        assert_eq!(omitted(&out), Some((6 - out.len()) as u64));
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

    fn text_blocks(out: &[ToolCallContent]) -> Vec<String> {
        out.iter()
            .filter_map(|c| match c {
                ToolCallContent::Content(content) => match &content.content {
                    agent_client_protocol::schema::v1::ContentBlock::Text(t) => {
                        Some(t.text.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn omissions_add_a_visible_text_block_not_just_meta() {
        let big = "y".repeat(MAX_DIFF_BYTES * 2);
        let w = pending_content("write", &json!({"path": "a", "content": big}));
        assert!(text_blocks(&w)[0].contains("beyond 64 KiB is not shown"));
        let e = pending_content("edit", &json!({"path": "a.rs", "block": blocks(20, "")}));
        assert!(text_blocks(&e)[0].contains("4 more edit block(s) not shown"));
        let bad = pending_content("edit", &json!({"path": "a.rs", "block": "garbage"}));
        assert!(text_blocks(&bad)[0].contains("could not be parsed"));
        let ops = pending_content("edit", &json!({"path": "a.rs", "edits": []}));
        assert!(text_blocks(&ops)[0].contains("No text preview"));
        // A complete preview adds no notice.
        let ok = pending_content("edit", &json!({"path": "a.rs", "block": blocks(2, "")}));
        assert!(text_blocks(&ok).is_empty());
        let small = pending_content("write", &json!({"path": "a", "content": "x"}));
        assert!(text_blocks(&small).is_empty());
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
