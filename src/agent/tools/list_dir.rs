use std::path::Path;

use rig::tool::Tool;

use super::find_files::BoundDirectory;
use crate::agent::tools::{
    AskSender, ListDirArgs, PermCheck, ToolError, check_perm_bound_path, check_perm_path,
};

pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}

const NOT_A_DIRECTORY_HINT: &str =
    "Use read to view the file, or pass its parent directory as path.";

/// Deepest tree `depth` accepts.
const MAX_LIST_DEPTH: usize = 3;
/// Rows collected for one depth 2-3 tree before the listing is reported incomplete.
const MAX_TREE_ROWS: usize = 10_000;
/// Safety ceiling for a flat (depth 1) listing; offset paging reaches every entry below it.
const MAX_FLAT_ROWS: usize = 1_000_000;

fn row_cap(max_depth: usize) -> usize {
    if max_depth <= 1 {
        MAX_FLAT_ROWS
    } else {
        MAX_TREE_ROWS
    }
}

struct Row {
    level: usize,
    name: String,
    kind: String,
    size: String,
    link_target: Option<String>,
}

fn collect_level(
    directory: &BoundDirectory,
    prefix: &str,
    level: usize,
    max_depth: usize,
    rows: &mut Vec<Row>,
    incomplete: &mut bool,
) -> std::io::Result<()> {
    let mut entries = directory.list_entries()?;
    entries.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    for entry in entries {
        if rows.len() >= row_cap(max_depth) {
            *incomplete = true;
            return Ok(());
        }
        let name = format!("{prefix}{}", entry.file_name.to_string_lossy());
        let kind = if entry.is_directory {
            format!("dir({})", entry.child_count)
        } else if entry.is_link {
            "link".to_string()
        } else {
            "file".to_string()
        };
        let size = if !entry.is_directory && !entry.is_link {
            format_size(entry.size)
        } else {
            String::new()
        };
        let link_target = entry
            .link_target
            .as_ref()
            .map(|target| target.to_string_lossy().into_owned());
        rows.push(Row {
            level,
            name: name.clone(),
            kind,
            size,
            link_target,
        });
        if entry.is_directory
            && level < max_depth
            && let Ok(child) = directory.child_directory(&entry.file_name)
        {
            // An unreadable subdirectory keeps its own row and is skipped.
            let _ = collect_level(
                &child,
                &format!("{name}/"),
                level + 1,
                max_depth,
                rows,
                incomplete,
            );
        }
    }
    Ok(())
}

pub struct ListDirTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    /// `None` = no truncation (matches the historical behaviour).
    /// `Some(n)` = show the first `n` entries with a recovery hint.
    pub max_entries: Option<u64>,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
}

impl ListDirTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        max_entries: Option<u64>,
    ) -> Self {
        ListDirTool {
            permission,
            ask_tx,
            max_entries,
            workspace: None,
        }
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_workspace(self, root: impl Into<std::path::PathBuf>) -> Self {
        self.with_workspace_binding(crate::agent::tools::capture_workspace_binding(root.into()))
    }
}

impl Tool for ListDirTool {
    const NAME: &'static str = "list_dir";

    type Error = ToolError;
    type Args = ListDirArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "List files and directories in a directory. Respects .gitignore by default (the listing counts the ignored entries it hid; pass no_ignore=true to show them) and hides VCS metadata, node_modules and target directories beside a build manifest unless requested explicitly. Shows type, size, entry count for subdirectories. Sorted: directories first, then alphabetical.{} Page with offset (the truncation notice names the next offset); depth 2-3 expands subdirectories into an indented tree under the same cap.",
            match self.max_entries {
                Some(cap) => format!(" Returns at most {cap} entries per call."),
                None => String::new(),
            }
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (defaults to current working directory)"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Entries to skip, to continue a truncated listing (default 0)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Return at most this many entries (never more than the configured cap)"
                },
                "no_ignore": {
                    "type": "boolean",
                    "default": false,
                    "description": "Also list entries hidden by .gitignore/.ignore (generated output, .env.local, vendored copies). Default false; a listing that hid ignored entries says how many. Permission deny rules, VCS metadata and dependency/build directories still apply"
                },
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIST_DEPTH,
                    "description": "Levels to list: 1 (default) lists this directory, 2-3 also expand subdirectories"
                }
            },
            "required": []
        })
    }

    async fn call(&self, args: ListDirArgs) -> Result<String, ToolError> {
        let depth = args.depth.unwrap_or(1);
        if depth == 0 || depth > MAX_LIST_DEPTH {
            return Err(ToolError::Msg(format!(
                "list_dir depth must be between 1 and {MAX_LIST_DEPTH}"
            )));
        }
        if args.limit == Some(0) {
            return Err(ToolError::Msg("list_dir limit must be at least 1".into()));
        }
        let offset = args.offset.unwrap_or(0);
        crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let target = crate::agent::tools::resolve_tool_target(
            self.workspace.as_ref(),
            args.path.as_deref().unwrap_or("."),
        );
        let requested = target.requested;
        let path = requested.to_string_lossy().into_owned();
        crate::agent::tools::precheck_perm_path_deny(&self.permission, "list_dir", &path)?;
        let relative = target.bound_relative.as_deref().unwrap_or(Path::new(""));
        let bound_workspace = target.bound_relative.as_ref().and(self.workspace.as_ref());
        tracing::debug!("tool list_dir start: path={}", path);
        let (bound_directory, coaching) = if let Some(workspace) = bound_workspace {
            let logical = workspace.logical_relative_path(relative)?;
            let directory = workspace
                .open_relative_directory_file(relative)
                .map_err(|error| {
                    if crate::agent::tools::names_regular_file(&logical) {
                        crate::agent::tools::not_a_directory_error(
                            "list_dir",
                            &logical,
                            NOT_A_DIRECTORY_HINT,
                        )
                    } else {
                        crate::agent::tools::bound_open_error(workspace, relative, error)
                    }
                })?;
            let bound = BoundDirectory::from_file(&logical, directory)?
                .with_hidden_filter(crate::agent::tools::walk_deny_filter(
                    &self.permission,
                    "list_dir",
                    Some(workspace.root()),
                ))
                .with_include_ignored(args.no_ignore);
            let coaching = check_perm_bound_path(
                &self.permission,
                &self.ask_tx,
                "list_dir",
                workspace,
                relative,
            )
            .await?;
            (bound, coaching)
        } else {
            let resolved = tokio::fs::canonicalize(&requested).await?;
            let checked_metadata = crate::fs::stable_path_metadata(&resolved).await?;
            if checked_metadata.is_file() {
                return Err(crate::agent::tools::not_a_directory_error(
                    "list_dir",
                    &resolved,
                    NOT_A_DIRECTORY_HINT,
                ));
            }
            // Pin the exact authorized directory before an interactive
            // permission wait can race a pathname replacement.
            let bound = BoundDirectory::open(&resolved, &checked_metadata)?
                .with_hidden_filter(crate::agent::tools::walk_deny_filter(
                    &self.permission,
                    "list_dir",
                    None,
                ))
                .with_include_ignored(args.no_ignore);
            let coaching = check_perm_path(
                &self.permission,
                &self.ask_tx,
                "list_dir",
                &resolved.to_string_lossy(),
            )
            .await?;
            (bound, coaching)
        };

        let ignore_mode = bound_directory.ignore_mode();
        let notice_root = bound_directory.approved_root().to_path_buf();
        let (rows, incomplete) = crate::agent::runner::spawn_blocking_scoped(move || {
            let mut rows = Vec::new();
            let mut incomplete = false;
            collect_level(&bound_directory, "", 1, depth, &mut rows, &mut incomplete)
                .map(|()| (rows, incomplete))
        })
        .await
        .map_err(|error| ToolError::Msg(format!("list_dir directory reader failed: {error}")))??;

        let ignore_notice = ignore_mode.notice(&notice_root);
        if rows.is_empty() {
            let msg = format!("Listing {}:\n(empty directory){ignore_notice}", path);
            return Ok(match coaching {
                Some(c) => format!("{}\n\n{}", c, msg),
                None => msg,
            });
        }

        let total_entries = rows.len();
        let cap = match (self.max_entries.map(|c| c as usize), args.limit) {
            (Some(cap), Some(limit)) => Some(cap.min(limit)),
            (None, limit) => limit,
            (cap, None) => cap,
        };
        let start = offset.min(total_entries);
        let end = cap.map_or(total_entries, |cap| {
            total_entries.min(start.saturating_add(cap))
        });
        let page = &rows[start..end];
        let total_label = if incomplete {
            format!("at least {total_entries}")
        } else {
            total_entries.to_string()
        };
        let mut result = match (depth, start) {
            (1, 0) => format!("Listing {}:\n", path),
            (1, _) => format!(
                "Listing {} (entries {}-{} of {total_label}):\n",
                path,
                start + 1,
                end
            ),
            (_, _) => format!(
                "Listing {} (depth {depth}; entries {}-{} of {total_label}):\n",
                path,
                start + 1,
                end
            ),
        };
        if page.is_empty() {
            result.push_str(&format!(
                "(no entries at offset {offset}; the listing has {total_label} entries)\n"
            ));
        }
        let max_name = page
            .iter()
            .map(|row| row.name.chars().count() + 2 * (row.level - 1))
            .max()
            .unwrap_or(0);
        for row in page {
            let indent = "  ".repeat(row.level - 1);
            let padded = format!(
                "{:width$}",
                row.name,
                width = max_name.saturating_sub(2 * (row.level - 1))
            );
            let size_str = if row.size.is_empty() {
                String::new()
            } else {
                format!("  {}", row.size)
            };
            let target = row
                .link_target
                .as_ref()
                .map(|target| format!(" -> {target}"))
                .unwrap_or_default();
            result.push_str(&format!(
                "  {indent}[{}]  {}{}{}\n",
                row.kind, padded, target, size_str
            ));
        }
        if end < total_entries {
            result.push_str(&format!(
                "\n[truncated after {} entries — {} more; continue with offset={end}, list a subdirectory, or use find_files with a narrower pattern]",
                end - start,
                total_entries - end,
            ));
        } else if incomplete && depth <= 1 {
            result.push_str(&format!(
                "\n[listing stopped at {MAX_FLAT_ROWS} entries; use find_files with a narrower pattern or shell to reach the rest]"
            ));
        } else if incomplete {
            result.push_str(&format!(
                "\n[listing stopped at {MAX_TREE_ROWS} entries; use a smaller depth or list a subdirectory]"
            ));
        }
        result.push_str(&ignore_notice);
        tracing::debug!(
            "tool list_dir done: path={}, entries={}",
            path,
            total_entries,
        );
        if let Some(msg) = coaching {
            result = format!("{}\n\n{}", msg, result);
        }
        Ok(result)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use rig::tool::Tool;

    use super::ListDirTool;
    use crate::agent::tools::ListDirArgs;
    use crate::permission::ask::UserDecision;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{PermissionConfigs, SecurityMode};

    #[tokio::test]
    async fn descriptor_bound_listing_reports_symlinks_without_following_them() {
        let temp = std::env::temp_dir().join(format!(
            "mini-agent-list-dir-link-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("target.txt"), "target").unwrap();
        std::os::unix::fs::symlink("target.txt", temp.join("link.txt")).unwrap();
        let listing = ListDirTool::new(None, None, None)
            .with_workspace(&temp)
            .call(ListDirArgs {
                path: None,
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(listing.contains("[link]  link.txt"), "{listing}");
        assert!(listing.contains("-> target.txt"), "{listing}");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[tokio::test]
    async fn flat_listing_pages_past_ten_thousand_entries() {
        let temp = std::env::temp_dir().join(format!(
            "mini-agent-list-dir-big-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        for i in 0..10_050 {
            std::fs::write(temp.join(format!("f{i:05}")), "").unwrap();
        }
        let listing = ListDirTool::new(None, None, None)
            .with_workspace(&temp)
            .call(ListDirArgs {
                path: None,
                offset: Some(10_040),
                limit: Some(100),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(listing.contains("f10049"), "{listing}");
        assert!(
            listing.contains("entries 10041-10050 of 10050"),
            "{listing}"
        );
        assert!(!listing.contains("listing stopped"), "{listing}");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[tokio::test]
    async fn links_sort_with_files_after_directories() {
        let temp = std::env::temp_dir().join(format!(
            "mini-agent-list-dir-link-sort-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp.join("z-dir")).unwrap();
        std::fs::write(temp.join("a-file"), "file").unwrap();
        std::os::unix::fs::symlink("a-file", temp.join("b-link")).unwrap();

        let listing = ListDirTool::new(None, None, None)
            .with_workspace(&temp)
            .call(ListDirArgs {
                path: None,
                ..Default::default()
            })
            .await
            .unwrap();
        let dir = listing.find("z-dir").unwrap();
        let file = listing.find("a-file").unwrap();
        let link = listing.find("b-link").unwrap();

        assert!(dir < file && file < link, "{listing}");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[tokio::test]
    async fn descriptor_bound_listing_ignores_an_aba_swap_during_permission_wait() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let temp = std::env::temp_dir().join(format!(
            "mini-agent_list_dir_toctou_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let checked_target = temp.join("checked");
        let swapped_target = temp.join("swapped");
        let link = temp.join("input");
        std::fs::create_dir_all(&checked_target).unwrap();
        std::fs::create_dir_all(&swapped_target).unwrap();
        std::fs::write(checked_target.join("checked.txt"), "checked").unwrap();
        std::fs::write(swapped_target.join("swapped.txt"), "swapped").unwrap();
        std::os::unix::fs::symlink(&checked_target, &link).unwrap();
        let original_target = temp.join("checked-original");

        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Restrictive,
            Some(PathBuf::from(&temp)),
            Some(vec!["restrictive".to_string()]),
        )
        .expect("valid permission test configuration");
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = ListDirTool::new(Some(Arc::new(Mutex::new(checker))), Some(ask_tx), None);

        let call = tool.call(ListDirArgs {
            path: Some(link.to_string_lossy().into_owned()),
            ..Default::default()
        });
        let swap = async {
            let request = ask_rx
                .recv()
                .await
                .expect("permission request")
                .into_permission();
            assert_eq!(
                PathBuf::from(&request.input),
                std::fs::canonicalize(&checked_target).unwrap()
            );
            std::fs::rename(&checked_target, &original_target).unwrap();
            std::os::unix::fs::symlink(&swapped_target, &checked_target).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, swap);
        let listing = result.expect("descriptor-bound listing must retain the authorized target");
        assert!(listing.contains("checked.txt"));
        assert!(!listing.contains("swapped.txt"));

        std::fs::remove_file(&checked_target).unwrap();
        std::fs::rename(&original_target, &checked_target).unwrap();
        assert_eq!(
            std::fs::read_to_string(checked_target.join("checked.txt")).unwrap(),
            "checked"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    // mini-agent-10dje: naming a regular file explains the directory-only
    // contract instead of surfacing `Not a directory (os error 20)`.
    #[tokio::test]
    async fn file_path_reports_a_clear_not_a_directory_error() {
        let temp = std::env::temp_dir().join(format!(
            "mini-agent-list-dir-file-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("notes.txt"), "notes").unwrap();
        let absolute = temp.join("notes.txt").to_string_lossy().into_owned();
        for (tool, path) in [
            (
                ListDirTool::new(None, None, None).with_workspace(&temp),
                "notes.txt",
            ),
            (ListDirTool::new(None, None, None), absolute.as_str()),
        ] {
            let error = tool
                .call(ListDirArgs {
                    path: Some(path.to_string()),
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("is a file, not a directory") && error.contains("notes.txt"),
                "{path}: {error}"
            );
            assert!(error.contains("Use read"), "{path}: {error}");
            assert!(!error.contains("os error"), "{path}: {error}");
        }
        std::fs::remove_dir_all(temp).unwrap();
    }
}
