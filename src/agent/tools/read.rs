use std::path::Path;

use rig::tool::Tool;
use tokio::io::AsyncReadExt;

use crate::agent::tools::crc::crc32_hex;
use crate::agent::tools::{
    AskSender, PermCheck, ReadArgs, ToolError, check_perm_bound_path, check_perm_path, edit_system,
};
use crate::config::types::EditSystem;

const DEFAULT_MAX_TEXT_SIZE: u64 = 1024 * 1024;

pub struct ReadTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_text_file_size: u64,
    pub max_lines: u64,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
}

impl ReadTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        max_text_file_size: Option<u64>,
        max_lines: u64,
    ) -> Self {
        ReadTool {
            permission,
            ask_tx,
            max_text_file_size: max_text_file_size.unwrap_or(DEFAULT_MAX_TEXT_SIZE),
            max_lines,
            workspace: None,
        }
    }

    pub fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
        self.workspace = Some(crate::agent::tools::capture_workspace_binding(root));
        self
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }
}

impl Tool for ReadTool {
    const NAME: &'static str = "read";

    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    fn description(&self) -> String {
        match edit_system() {
            EditSystem::Similarity => format!(
                "Read the contents of a file. Supports text files. Defaults to first {} lines. Use offset/limit for large files.",
                self.max_lines
            ),
            EditSystem::Hashedit => format!(
                "Read file contents with CRC-32 tagged lines for tag-based editing. Each line is prefixed with 'N|TAG' where TAG is an 8-char hex CRC-32 of the line content. Use these tags with the edit tool for CAS-guarded edits. Defaults to first {} lines.",
                self.max_lines
            ),
        }
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                "offset": { "type": "integer", "description": "Line number to start from (1-indexed)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        let workspace_root =
            crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let requested =
            crate::agent::tools::resolve_tool_path(workspace_root.as_deref(), &args.path);
        let path = requested.to_string_lossy().into_owned();
        let relative = Path::new(&args.path);
        let bound_workspace = if !relative.is_absolute() && !args.path.starts_with('~') {
            self.workspace.as_ref()
        } else {
            None
        };
        let capability_file = bound_workspace
            .map(|workspace| workspace.open_relative(relative))
            .transpose()?;
        let offset = args.offset.unwrap_or(1).saturating_sub(1);
        let limit = args.limit.unwrap_or(self.max_lines as usize);
        tracing::debug!(
            "tool read start: path={}, offset={}, limit={}",
            path,
            offset,
            limit,
        );
        let (resolved, coaching) = if let Some(workspace) = bound_workspace {
            (
                None,
                check_perm_bound_path(&self.permission, &self.ask_tx, "read", workspace, relative)
                    .await?,
            )
        } else {
            let resolved = tokio::fs::canonicalize(&requested).await?;
            let coaching = check_perm_path(
                &self.permission,
                &self.ask_tx,
                "read",
                &resolved.to_string_lossy(),
            )
            .await?;
            (Some(resolved), coaching)
        };
        let mut file = if let Some(file) = capability_file {
            tokio::fs::File::from_std(file)
        } else {
            crate::fs::open_stable_file(
                resolved
                    .as_deref()
                    .expect("ambient read must resolve an external path"),
            )
            .await?
        };

        if let Some(msg) = crate::agent::tools::track_read(&path, offset, limit) {
            tracing::debug!("tool read blocked (repeated): path={}", path);
            return Err(ToolError::Msg(msg));
        }

        let metadata = file.metadata().await?;
        let file_size = metadata.len();
        if file_size > self.max_text_file_size {
            tracing::warn!(
                "tool read file too large: path={}, size={}, max={}",
                path,
                file_size,
                self.max_text_file_size,
            );
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes). Maximum allowed file size is {} bytes.",
                file_size, self.max_text_file_size
            )));
        }
        let mut content = String::new();
        file.read_to_string(&mut content).await?;
        let total_lines = content.lines().count();

        let (start, end) = read_bounds(offset, limit, total_lines);

        let es = edit_system();

        let excerpt: String = match es {
            EditSystem::Hashedit => {
                // Annotate each line with CRC-32 tag
                content
                    .lines()
                    .skip(start)
                    .take(end - start)
                    .enumerate()
                    .map(|(i, line)| {
                        let line_num = start + i + 1;
                        let tag = crc32_hex(line.as_bytes());
                        let line_num_width = if total_lines >= 1000 { 4 } else { 3 };
                        format!(
                            "{:>width$}|{} {}",
                            line_num,
                            tag,
                            line,
                            width = line_num_width
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            EditSystem::Similarity => {
                // Plain text (original behavior)
                content
                    .lines()
                    .skip(start)
                    .take(end - start)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        let info = match es {
            EditSystem::Hashedit => {
                let file_crc = crc32_hex(content.replace("\r\n", "\n").as_bytes());
                format!(
                    "File: {} ({} lines total, lines {}-{}) [CRC: {}]\n\n{}",
                    path,
                    total_lines,
                    display_start(start, total_lines),
                    end,
                    file_crc,
                    excerpt
                )
            }
            EditSystem::Similarity => {
                format!(
                    "File: {} ({} lines total, showing lines {}-{})\n\n{}",
                    path,
                    total_lines,
                    display_start(start, total_lines),
                    end,
                    excerpt
                )
            }
        };

        let info = if end < total_lines {
            let remaining = total_lines - end;
            format!(
                "{}\n\n[truncated after {} lines — {} more lines (lines {}-{}); re-call with offset/limit to see more]",
                info,
                end - start,
                remaining,
                end + 1,
                total_lines,
            )
        } else {
            info
        };

        let info = match coaching {
            Some(msg) => format!("{}\n\n{}", msg, info),
            None => info,
        };

        tracing::debug!(
            "tool read done: path={}, total_lines={}, returned_lines={}",
            path,
            total_lines,
            end - start,
        );
        Ok(info)
    }
}

fn read_bounds(offset: usize, limit: usize, total_lines: usize) -> (usize, usize) {
    let start = offset.min(total_lines);
    let end = start.saturating_add(limit).min(total_lines);
    (start, end)
}

fn display_start(start: usize, total_lines: usize) -> usize {
    if total_lines == 0 { 0 } else { start + 1 }
}

#[cfg(test)]
mod tests {
    use super::{display_start, read_bounds};

    #[test]
    fn read_bounds_clamps_offset_past_eof() {
        assert_eq!(read_bounds(20, 10, 5), (5, 5));
    }

    #[test]
    fn read_bounds_uses_requested_window_inside_file() {
        assert_eq!(read_bounds(2, 3, 10), (2, 5));
    }

    #[test]
    fn display_start_handles_empty_file() {
        assert_eq!(display_start(0, 0), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_swap_after_permission_check_is_rejected() {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};

        use rig::tool::Tool;

        use super::ReadTool;
        use crate::agent::tools::ReadArgs;
        use crate::permission::ask::UserDecision;
        use crate::permission::checker::PermissionChecker;
        use crate::permission::{PermissionConfigs, SecurityMode};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let temp = std::env::temp_dir().join(format!(
            "zerostack_read_toctou_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&temp).unwrap();

        let checked_target = temp.join("checked.txt");
        let swapped_target = temp.join("swapped.txt");
        let link = temp.join("input.txt");
        std::fs::write(&checked_target, "checked contents\n").unwrap();
        std::fs::write(&swapped_target, "swapped contents\n").unwrap();
        std::os::unix::fs::symlink(&checked_target, &link).unwrap();

        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Restrictive,
            Some(PathBuf::from(&temp)),
            Some(vec!["restrictive".to_string()]),
        );
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = ReadTool::new(Some(Arc::new(Mutex::new(checker))), Some(ask_tx), None, 100);

        let call = tool.call(ReadArgs {
            path: link.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
        });
        let swap = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(
                PathBuf::from(&request.input),
                std::fs::canonicalize(&checked_target).unwrap()
            );
            std::fs::remove_file(&checked_target).unwrap();
            std::os::unix::fs::symlink(&swapped_target, &checked_target).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, swap);
        let error = result.expect_err("read must reject a swapped permission-checked target");
        assert!(error.to_string().contains("Path changed"));

        std::fs::remove_dir_all(temp).unwrap();
    }
}
