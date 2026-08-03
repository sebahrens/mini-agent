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

pub(crate) fn count_dir_entries(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .map(|rd| rd.count() as u64)
        .unwrap_or(0)
}

pub struct ListDirTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    /// `None` = no truncation (matches the historical behaviour).
    /// `Some(n)` = show the first `n` entries with a recovery hint.
    pub max_entries: Option<u64>,
    workspace: std::path::PathBuf,
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
            workspace: std::env::current_dir().unwrap_or_default(),
        }
    }

    pub(crate) fn with_workspace(mut self, workspace: impl Into<std::path::PathBuf>) -> Self {
        self.workspace = workspace.into();
        self
    }
}

impl Tool for ListDirTool {
    const NAME: &'static str = "list_dir";

    type Error = ToolError;
    type Args = ListDirArgs;
    type Output = String;

    fn description(&self) -> String {
        "List files and directories in a directory. Respects .gitignore. Shows type, size, entry count for subdirectories. Sorted: directories first, then alphabetical.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (defaults to current working directory)"
                }
            },
            "required": []
        })
    }

    async fn call(&self, args: ListDirArgs) -> Result<String, ToolError> {
        let path =
            crate::fs::resolve_workspace_path(&self.workspace, args.path.as_deref().unwrap_or("."));
        let resolved = tokio::fs::canonicalize(&path).await?;
        let permission_path = resolved.to_string_lossy();
        tracing::debug!("tool list_dir start: path={}", path.display());
        let coaching =
            check_perm_path(&self.permission, &self.ask_tx, "list_dir", &permission_path).await?;
        let checked_metadata = crate::fs::stable_path_metadata(&resolved).await?;

        let walker = WalkBuilder::new(&resolved)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .hidden(false)
            .max_depth(Some(1))
            .filter_entry(|entry| {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    !is_skip_dir(entry.file_name().to_str().unwrap_or(""))
                } else {
                    true
                }
            })
            .build();

        let mut entries: Vec<(String, String, String)> = Vec::new();

        for entry in bound_directory.list_entries()? {
            let name = entry.file_name.to_string_lossy().to_string();
            let kind = if entry.metadata.is_dir() {
                format!("dir({})", entry.child_count)
            } else if entry.metadata.is_symlink() {
                "link".to_string()
            } else {
                "file".to_string()
            };

            let size = if entry.metadata.is_file() {
                format_size(entry.metadata.len())
            } else {
                String::new()
            };

            entries.push((name, kind, size));
        }

        entries.sort_by(|a, b| {
            let a_is_dir = a.1.starts_with("dir") || a.1 == "link";
            let b_is_dir = b.1.starts_with("dir") || b.1 == "link";
            if a_is_dir != b_is_dir {
                b_is_dir.cmp(&a_is_dir)
            } else {
                a.0.cmp(&b.0)
            }
        });

        if entries.is_empty() {
            let msg = format!("Listing {}:\n(empty directory)", path.display());
            return Ok(match coaching {
                Some(c) => format!("{}\n\n{}", c, msg),
                None => msg,
            });
        }

        let total_entries = entries.len();
        let cap = self.max_entries.map(|c| c as usize);
        let shown = cap.map(|c| total_entries.min(c)).unwrap_or(total_entries);
        let max_name = entries[..shown]
            .iter()
            .map(|e| e.0.len())
            .max()
            .unwrap_or(0);
        let mut result = format!("Listing {}:\n", path.display());
        for (name, kind, size) in &entries[..shown] {
            let padded = format!("{:width$}", name, width = max_name);
            let size_str = if size.is_empty() {
                String::new()
            } else {
                format!("  {}", size)
            };
            result.push_str(&format!("  [{}]  {}{}\n", kind, padded, size_str));
        }
        if let Some(cap) = cap
            && total_entries > cap
        {
            result.push_str(&format!(
                "\n[truncated after {} entries — {} more; list a subdirectory or use find_files with a narrower pattern]",
                cap,
                total_entries - cap,
            ));
        }
        tracing::debug!(
            "tool list_dir done: path={}, entries={}",
            path.display(),
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
    async fn descriptor_bound_listing_ignores_an_aba_swap_during_permission_wait() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let temp = std::env::temp_dir().join(format!(
            "zerostack_list_dir_toctou_test_{}_{}",
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
        );
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = ListDirTool::new(Some(Arc::new(Mutex::new(checker))), Some(ask_tx), None);

        let call = tool.call(ListDirArgs {
            path: Some(link.to_string_lossy().into_owned()),
        });
        let swap = async {
            let request = ask_rx.recv().await.expect("permission request");
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
}
