use std::path::Path;

use rig::tool::Tool;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::tools::crc::{Crc32, crc32_hex};
use crate::agent::tools::{
    AskSender, PermCheck, ReadArgs, ReadTracker, ToolError, check_perm_bound_path, check_perm_path,
    edit_system,
};
use crate::config::types::EditSystem;

const DEFAULT_MAX_TEXT_SIZE: u64 = 1024 * 1024;

pub struct ReadTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_text_file_size: u64,
    pub max_lines: u64,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    read_tracker: ReadTracker,
}

impl ReadTool {
    #[cfg(test)]
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        max_text_file_size: Option<u64>,
        max_lines: u64,
    ) -> Self {
        Self::new_with_tracker(
            permission,
            ask_tx,
            max_text_file_size,
            max_lines,
            ReadTracker::new(true),
        )
    }

    pub(crate) fn new_with_tracker(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        max_text_file_size: Option<u64>,
        max_lines: u64,
        read_tracker: ReadTracker,
    ) -> Self {
        ReadTool {
            permission,
            ask_tx,
            max_text_file_size: max_text_file_size.unwrap_or(DEFAULT_MAX_TEXT_SIZE),
            max_lines,
            workspace: None,
            read_tracker,
        }
    }

    pub fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
        self.workspace = Some(crate::agent::tools::capture_workspace_binding(root));
        self
    }

    pub(crate) fn with_workspace(self, root: impl Into<std::path::PathBuf>) -> Self {
        self.with_workspace_root(root.into())
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
                "Read a UTF-8 text file with right-aligned 'N| content' line numbers. Defaults to the first {} lines. Files larger than {} bytes require an explicit offset or limit; the selected text must fit within that byte cap. Use a smaller line window for large files.",
                self.max_lines, self.max_text_file_size
            ),
            EditSystem::Hashedit => format!(
                "Read a UTF-8 text file with CRC-32 tagged lines for tag-based editing. Each line is prefixed with 'N|TAG' where TAG is an 8-char hex CRC-32 of the line content. Use these tags with the edit tool for CAS-guarded edits. Defaults to the first {} lines. Files larger than {} bytes require an explicit offset or limit; the selected text must fit within that byte cap.",
                self.max_lines, self.max_text_file_size
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
        let file = if let Some(file) = capability_file {
            tokio::fs::File::from_std(file)
        } else {
            crate::fs::open_stable_file(
                resolved
                    .as_deref()
                    .expect("ambient read must resolve an external path"),
            )
            .await?
        };
        let permission_path = resolved
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        let metadata = file.metadata().await?;

        let file_size = metadata.len();
        let explicitly_bounded = args.offset.is_some() || args.limit.is_some();
        if file_size > self.max_text_file_size && !explicitly_bounded {
            tracing::warn!(
                "tool read requires a bounded window: path={}, size={}, max={}",
                path,
                file_size,
                self.max_text_file_size,
            );
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes) for an unbounded read. The read output cap is {} bytes; re-call with an explicit offset and/or limit to select a smaller line window.",
                file_size, self.max_text_file_size
            )));
        }
        let es = edit_system();
        let oversized = file_size > self.max_text_file_size;
        let scan_to_eof = !oversized || es == EditSystem::Hashedit;
        let requested_end = offset.saturating_add(limit);
        let mut reader = BufReader::new(file);
        let mut raw_line = Vec::new();
        let mut excerpt_lines = Vec::with_capacity(limit.min(256));
        let mut excerpt_bytes = 0_u64;
        let mut total_lines = 0_usize;
        let mut has_more_lines = false;
        let mut file_crc = Crc32::new();
        let mut served_content_crc = Crc32::new();

        loop {
            raw_line.clear();
            let bytes_read = reader.read_until(b'\n', &mut raw_line).await?;
            if bytes_read == 0 {
                break;
            }

            std::str::from_utf8(&raw_line).map_err(|_| {
                ToolError::Msg(format!(
                    "Cannot read '{}' as text because it is not valid UTF-8. Use the shell tool with `strings`, `xxd`, or another binary-aware command to inspect it.",
                    path
                ))
            })?;

            if es == EditSystem::Hashedit {
                if raw_line.ends_with(b"\r\n") {
                    file_crc.update(&raw_line[..raw_line.len() - 2]);
                    file_crc.update(b"\n");
                } else {
                    file_crc.update(&raw_line);
                }
            }

            let mut content_end = raw_line.len();
            if raw_line.get(content_end.saturating_sub(1)) == Some(&b'\n') {
                content_end -= 1;
                if raw_line.get(content_end.saturating_sub(1)) == Some(&b'\r') {
                    content_end -= 1;
                }
            }
            let line = std::str::from_utf8(&raw_line[..content_end])
                .expect("the complete line was already validated as UTF-8");
            let line_index = total_lines;
            total_lines += 1;

            if line_index >= offset && line_index < requested_end {
                let separator_bytes = u64::from(!excerpt_lines.is_empty());
                let next_excerpt_bytes = excerpt_bytes
                    .saturating_add(separator_bytes)
                    .saturating_add(line.len() as u64);
                if next_excerpt_bytes > self.max_text_file_size {
                    return Err(ToolError::Msg(format!(
                        "Requested text window exceeds the {} byte read output cap. Re-call with a smaller limit or narrower offset/limit range; for very long or non-text lines, use the shell tool with `head`, `cut`, or `strings`.",
                        self.max_text_file_size
                    )));
                }
                excerpt_bytes = next_excerpt_bytes;
                excerpt_lines.push(line.to_string());
                served_content_crc.update(&(line.len() as u64).to_le_bytes());
                served_content_crc.update(line.as_bytes());
            }

            if !scan_to_eof && total_lines > requested_end {
                has_more_lines = true;
                break;
            }
        }

        let served_content_crc = served_content_crc.finalize();
        if let Some(msg) = self.read_tracker.check_read(
            &permission_path,
            offset,
            limit,
            &metadata,
            served_content_crc,
        ) {
            tracing::debug!("tool read blocked (repeated): path={}", path);
            return Err(ToolError::Msg(msg));
        }

        let start = offset.min(total_lines);
        let end = start.saturating_add(excerpt_lines.len());
        let line_num_width = line_number_width(total_lines.max(end));

        let excerpt: String = match es {
            EditSystem::Hashedit => {
                // Annotate each line with CRC-32 tag
                excerpt_lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let line_num = start + i + 1;
                        let tag = crc32_hex(line.as_bytes());
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
            EditSystem::Similarity => numbered_excerpt(&excerpt_lines, start, line_num_width),
        };

        let total_lines_label = if has_more_lines {
            format!("at least {total_lines} lines total")
        } else {
            format!("{total_lines} lines total")
        };

        let requested_line = offset.saturating_add(1);
        let past_eof = excerpt_lines.is_empty() && offset >= total_lines;
        let info = match (es, past_eof, total_lines) {
            (EditSystem::Hashedit, true, 0) => format!(
                "File: {} (0 lines total; empty file) [CRC: {}]",
                path,
                file_crc.finalize_hex(),
            ),
            (EditSystem::Hashedit, true, _) => format!(
                "File: {} ({} lines total; requested offset {} is past EOF at line {}) [CRC: {}]",
                path,
                total_lines,
                requested_line,
                total_lines,
                file_crc.finalize_hex(),
            ),
            (EditSystem::Hashedit, false, _) => {
                format!(
                    "File: {} ({}, lines {}-{}) [CRC: {}]\n\n{}",
                    path,
                    total_lines_label,
                    display_start(start, total_lines),
                    end,
                    file_crc.finalize_hex(),
                    excerpt
                )
            }
            (EditSystem::Similarity, true, 0) => {
                format!("File: {} (0 lines total; empty file)", path)
            }
            (EditSystem::Similarity, true, _) => format!(
                "File: {} ({} lines total; requested offset {} is past EOF at line {})",
                path, total_lines, requested_line, total_lines,
            ),
            (EditSystem::Similarity, false, _) => {
                format!(
                    "File: {} ({}, showing lines {}-{})\n\n{}",
                    path,
                    total_lines_label,
                    display_start(start, total_lines),
                    end,
                    excerpt
                )
            }
        };

        let info = if has_more_lines {
            format!(
                "{}\n\n[truncated after {} lines — more lines are available; re-call with offset {} and a bounded limit to continue]",
                info,
                end - start,
                end + 1,
            )
        } else if end < total_lines {
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
        self.read_tracker.record_read(
            &permission_path,
            offset,
            limit,
            &metadata,
            served_content_crc,
        );
        Ok(info)
    }
}

fn display_start(start: usize, total_lines: usize) -> usize {
    if total_lines == 0 { 0 } else { start + 1 }
}

fn line_number_width(total_lines: usize) -> usize {
    total_lines.max(1).to_string().len().max(3)
}

fn numbered_excerpt(lines: &[String], start: usize, width: usize) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}| {}", start + i + 1, line, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{display_start, line_number_width, numbered_excerpt};

    use rig::tool::Tool;

    use super::ReadTool;
    use crate::agent::tools::ReadArgs;

    #[test]
    fn display_start_handles_empty_file() {
        assert_eq!(display_start(0, 0), 0);
    }

    #[test]
    fn line_numbers_are_at_least_three_characters_and_expand_for_large_files() {
        assert_eq!(line_number_width(0), 3);
        assert_eq!(line_number_width(99), 3);
        assert_eq!(line_number_width(1_000), 4);
        assert_eq!(line_number_width(100_000), 6);
    }

    #[test]
    fn similarity_excerpt_has_right_aligned_absolute_line_numbers() {
        let lines = vec!["ninety-nine".to_string(), "one hundred".to_string()];
        assert_eq!(
            numbered_excerpt(&lines, 98, 3),
            " 99| ninety-nine\n100| one hundred"
        );
    }

    #[tokio::test]
    async fn offset_past_eof_reports_eof_without_an_inverted_line_range() {
        let path =
            std::env::temp_dir().join(format!("mini-agent-read-past-eof-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let output = ReadTool::new(None, None, None, 100)
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: Some(6),
                limit: Some(2),
            })
            .await
            .unwrap();

        assert!(
            output.contains("requested offset 6 is past EOF at line 2"),
            "{output}"
        );
        assert!(!output.contains("lines 6-2"), "{output}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn explicit_line_window_reads_a_file_larger_than_the_byte_cap() {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-read-large-window-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "zero\none\ntwo\nthree\nfour\nfive\n").unwrap();
        let tool = ReadTool::new(None, None, Some(12), 100);

        let output = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: Some(3),
                limit: Some(2),
            })
            .await
            .expect("bounded large-file read should succeed");

        assert!(output.contains("two"), "{output}");
        assert!(output.contains("three"), "{output}");
        assert!(!output.contains("four"), "{output}");
        assert!(output.contains("more lines are available"), "{output}");
        assert!(output.contains("offset 5"), "{output}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn oversized_unbounded_read_explains_how_to_select_a_window() {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-read-large-unbounded-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let tool = ReadTool::new(None, None, Some(4), 100);

        let error = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .expect_err("unbounded large-file read should retain the safety cap")
            .to_string();

        assert!(error.contains("explicit offset and/or limit"), "{error}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn invalid_utf8_returns_binary_aware_guidance() {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-read-invalid-utf8-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, [b'o', b'k', b'\n', 0xff, b'\n']).unwrap();
        let tool = ReadTool::new(None, None, None, 100);

        let error = tool
            .call(ReadArgs {
                path: path.to_string_lossy().into_owned(),
                offset: None,
                limit: None,
            })
            .await
            .expect_err("non-UTF-8 input must be rejected")
            .to_string();

        assert!(error.contains("not valid UTF-8"), "{error}");
        assert!(error.contains("strings"), "{error}");
        assert!(error.contains("xxd"), "{error}");
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_swap_after_permission_check_is_rejected() {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};

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
        )
        .expect("valid permission test configuration");
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
