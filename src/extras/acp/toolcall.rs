//! ACP tool-call presentation: titles, kinds, file locations and diff content.
//!
//! Everything here is display metadata derived from arguments the model sent
//! or from bounded [`MutationDiff`] rows the tool published. Nothing reads the
//! filesystem, and every string is bounded and sanitised before it reaches an
//! IDE.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use agent_client_protocol::schema::v1::{Diff, ToolCallContent, ToolCallLocation, ToolKind};
use serde_json::Value;

use crate::agent::tools::mutation_diff::{DiffKind, MAX_DIFF_BYTES, MutationDiff};

const TITLE_MAX_CHARS: usize = 120;
/// Characters of a `task` prompt kept in its call title.
const TASK_PROMPT_CHARS: usize = 60;
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

pub(super) fn clip(text: &str, max: usize) -> String {
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

/// Subject of a `task` call: the agent type and the first prompt or brief
/// objective, so a list of fan-out calls is distinguishable.
fn task_subject(args: &Value) -> Option<String> {
    let first = args
        .get("prompts")
        .and_then(Value::as_array)
        .and_then(|prompts| prompts.first())
        .and_then(Value::as_str)
        .or_else(|| {
            args.get("briefs")
                .and_then(Value::as_array)
                .and_then(|briefs| briefs.first())
                .and_then(|brief| brief.get("objective"))
                .and_then(Value::as_str)
        })
        .map(|text| clip(text, TASK_PROMPT_CHARS));
    match (str_arg(args, "agent_type"), first) {
        (Some(agent), Some(text)) => Some(format!("{agent}: {text}")),
        (Some(agent), None) => Some(agent.to_owned()),
        (None, text) => text,
    }
}

/// Subject of a `git` call: the operation and the paths it names.
fn git_subject(args: &Value) -> Option<String> {
    let operation = str_arg(args, "operation")?;
    let paths = args
        .get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|paths| !paths.is_empty());
    Some(match paths {
        Some(paths) => format!("{operation} {paths}"),
        None => operation.to_owned(),
    })
}

/// Descriptive title: the tool plus its path, command, pattern or, for
/// `task`, `git` and `job_status`, the subject that tells calls apart.
pub(super) fn title_for(tool: &str, args: &Value) -> String {
    let subject = match tool {
        "shell" => str_arg(args, "command").map(str::to_owned),
        "grep" | "find_files" => str_arg(args, "pattern").map(str::to_owned),
        "web_fetch" => str_arg(args, "url").map(str::to_owned),
        "web_search" => str_arg(args, "query").map(str::to_owned),
        "task" => task_subject(args),
        "git" => git_subject(args),
        "job_status" => str_arg(args, "id").map(str::to_owned),
        _ => str_arg(args, "path").map(str::to_owned),
    };
    match subject {
        Some(subject) => clip(&format!("{tool} {subject}"), TITLE_MAX_CHARS),
        None => clip(tool, TITLE_MAX_CHARS),
    }
}

/// ACP requires absolute file paths. The model usually sends workspace-relative
/// ones, so join them onto the session root (no filesystem access).
fn absolute_path(root: Option<&Path>, path: &str) -> String {
    match root {
        Some(root) if !Path::new(path).is_absolute() => {
            root.join(path).to_string_lossy().into_owned()
        }
        _ => path.to_owned(),
    }
}

/// Files a path-taking tool touches, for follow-along in the IDE.
pub(super) fn locations_for(
    tool: &str,
    args: &Value,
    root: Option<&Path>,
) -> Vec<ToolCallLocation> {
    if !matches!(
        tool,
        "read" | "edit" | "write" | "notebook_edit" | "list_dir" | "grep" | "find_files"
    ) {
        return Vec::new();
    }
    let Some(path) = str_arg(args, "path") else {
        return Vec::new();
    };
    let mut location = ToolCallLocation::new(absolute_path(root, path));
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

/// One announced call kept until its result: the title shown to the user and,
/// for `edit`/`write`, the arguments a permission preview is built from.
struct PendingCall {
    id: String,
    title: String,
    args: Option<Value>,
}

fn pending() -> &'static Mutex<VecDeque<PendingCall>> {
    static PENDING: OnceLock<Mutex<VecDeque<PendingCall>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Remember a call's title (every tool) and, for edit/write, its arguments
/// until the result, so the permission bridge (a separate task) can keep the
/// descriptive title and build a diff for the dialog. A relative `path` is
/// made absolute with `root` so the diff header is a valid ACP path.
pub(super) fn remember_args(
    tool_call_id: &str,
    tool: &str,
    args: &Value,
    title: &str,
    root: Option<&Path>,
) {
    let args = matches!(tool, "edit" | "write").then(|| {
        let mut args = args.clone();
        if let Some(path) = str_arg(&args, "path").map(|path| absolute_path(root, path))
            && let Some(object) = args.as_object_mut()
        {
            object.insert("path".to_owned(), Value::String(path));
        }
        args
    });
    let mut guard = pending().lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|call| call.id != tool_call_id);
    if guard.len() >= MAX_PENDING {
        guard.pop_front();
    }
    guard.push_back(PendingCall {
        id: tool_call_id.to_owned(),
        title: title.to_owned(),
        args,
    });
}

pub(super) fn recall_args(tool_call_id: &str) -> Option<Value> {
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|call| call.id == tool_call_id)
        .and_then(|call| call.args.clone())
}

/// The title the call was announced with, so an approval request does not
/// rename it to the bare tool name.
pub(super) fn recall_title(tool_call_id: &str) -> Option<String> {
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|call| call.id == tool_call_id)
        .map(|call| call.title.clone())
}

pub(super) fn forget_args(tool_call_id: &str) {
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|call| call.id != tool_call_id);
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
        let locations = locations_for("read", &args, None);
        assert_eq!(locations[0].path.to_str(), Some("src/a.rs"));
        assert_eq!(locations[0].line, Some(9));
        assert!(locations_for("shell", &json!({"command": "ls"}), None).is_empty());
    }

    #[test]
    fn locations_and_preview_paths_are_absolute_under_the_workspace() {
        let root = std::env::temp_dir().join("acp-root");
        let relative = json!({"path": "src/a.rs"});
        let locations = locations_for("edit", &relative, Some(&root));
        assert_eq!(locations[0].path, root.join("src/a.rs"));
        let absolute = root.join("b.rs");
        let args = json!({"path": absolute.to_string_lossy()});
        assert_eq!(locations_for("read", &args, Some(&root))[0].path, absolute);

        remember_args(
            "abs-1",
            "write",
            &json!({"path": "c.txt", "content": "x"}),
            "write c.txt",
            Some(&root),
        );
        let recalled = recall_args("abs-1").unwrap();
        let out = pending_diffs("write", &recalled);
        let [ToolCallContent::Diff(diff)] = out.as_slice() else {
            panic!("expected one diff");
        };
        assert_eq!(diff.path, root.join("c.txt"));
        forget_args("abs-1");
    }

    #[test]
    fn titles_name_the_subject_of_task_git_and_job_calls() {
        let task = json!({"agent_type": "explore", "prompts": ["find where retries are decided"]});
        assert_eq!(
            title_for("task", &task),
            "task explore: find where retries are decided"
        );
        let briefs = json!({"briefs": [{"objective": "map the parser"}]});
        assert_eq!(title_for("task", &briefs), "task map the parser");
        let long = json!({"prompts": ["p".repeat(200)]});
        assert!(title_for("task", &long).chars().count() <= TITLE_MAX_CHARS);
        let git = json!({"operation": "stage", "paths": ["a.rs", "b.rs"]});
        assert_eq!(title_for("git", &git), "git stage a.rs b.rs");
        assert_eq!(
            title_for("git", &json!({"operation": "status"})),
            "git status"
        );
        assert_eq!(
            title_for("job_status", &json!({"id": "job-3", "action": "wait"})),
            "job_status job-3"
        );
        assert_eq!(title_for("task", &json!({})), "task");
    }

    #[test]
    fn remembered_title_survives_for_every_tool() {
        remember_args(
            "title-1",
            "shell",
            &json!({"command": "cargo test"}),
            "shell cargo test",
            None,
        );
        assert_eq!(recall_title("title-1").as_deref(), Some("shell cargo test"));
        assert!(recall_args("title-1").is_none());
        forget_args("title-1");
        assert!(recall_title("title-1").is_none());
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
            remember_args(
                &format!("bound-{index}"),
                "write",
                &json!({"path": "a"}),
                "write a",
                None,
            );
        }
        assert!(recall_args("bound-0").is_none());
        assert!(recall_args(&format!("bound-{}", MAX_PENDING + 9)).is_some());
        forget_args(&format!("bound-{}", MAX_PENDING + 9));
        assert!(recall_args(&format!("bound-{}", MAX_PENDING + 9)).is_none());
        remember_args(
            "shell-1",
            "shell",
            &json!({"command": "x"}),
            "shell x",
            None,
        );
        assert!(recall_args("shell-1").is_none());
    }
}
