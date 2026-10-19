use std::path::Path;

use rig::tool::Tool;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::tools::crc::{Crc32, crc32_hex};
use crate::agent::tools::{
    AskSender, PermCheck, ReadArgs, ReadTracker, ToolError, check_perm_bound_path,
    check_perm_canonicalized_path, edit_system,
};
use crate::config::types::EditSystem;

const DEFAULT_MAX_TEXT_SIZE: u64 = 1024 * 1024;

/// Feed one already-validated slice of line content to the whole-file CRC and,
/// when the line is inside the requested window, to the retained excerpt.
fn absorb(
    chunk: &[u8],
    hashedit: bool,
    file_crc: &mut Crc32,
    selected: bool,
    retained: &mut Vec<u8>,
) {
    if chunk.is_empty() {
        return;
    }
    if hashedit {
        file_crc.update(chunk);
    }
    if selected {
        retained.extend_from_slice(chunk);
    }
}

fn not_utf8(path: &str) -> ToolError {
    ToolError::Msg(format!(
        "Cannot read '{path}' as text because it is not valid UTF-8. Use the shell tool with `strings`, `xxd`, or another binary-aware command to inspect it."
    ))
}

fn excerpt_cap_exceeded(cap: u64) -> ToolError {
    ToolError::Msg(format!(
        "Requested text window exceeds the {cap} byte read output cap. Re-call with a smaller limit or narrower offset/limit range; for very long or non-text lines, use the shell tool with `head`, `cut`, or `strings`."
    ))
}

/// Incremental UTF-8 validation across chunk boundaries. Only the bytes of a
/// possibly incomplete trailing sequence are retained, so validation costs no
/// memory proportional to the line or the file.
#[derive(Default)]
struct Utf8Stream {
    pending: [u8; 4],
    pending_len: usize,
}

impl Utf8Stream {
    fn push(&mut self, mut bytes: &[u8]) -> bool {
        while self.pending_len > 0 {
            if bytes.is_empty() {
                return true;
            }
            if self.pending_len == self.pending.len() {
                return false;
            }
            self.pending[self.pending_len] = bytes[0];
            self.pending_len += 1;
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.pending[..self.pending_len]) {
                Ok(_) => self.pending_len = 0,
                Err(error) if error.error_len().is_none() => {}
                Err(_) => return false,
            }
        }
        match std::str::from_utf8(bytes) {
            Ok(_) => true,
            Err(error) if error.error_len().is_some() => false,
            Err(error) => {
                let tail = &bytes[error.valid_up_to()..];
                if tail.len() > self.pending.len() {
                    return false;
                }
                self.pending[..tail.len()].copy_from_slice(tail);
                self.pending_len = tail.len();
                true
            }
        }
    }

    /// Whether the stream ended on a complete sequence.
    fn finish(&self) -> bool {
        self.pending_len == 0
    }
}

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

    #[cfg(test)]
    pub fn with_workspace_root(mut self, root: std::path::PathBuf) -> Self {
        self.workspace = Some(crate::agent::tools::capture_workspace_binding(root));
        self
    }

    #[cfg(test)]
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
        let text = match edit_system() {
            EditSystem::Similarity => format!(
                "Read a UTF-8 text file with right-aligned 'N| content' line numbers. Defaults to the first {} lines. Files larger than {} bytes require an explicit offset or limit; the selected text must fit within that byte cap. Use a smaller line window for large files.",
                self.max_lines, self.max_text_file_size
            ),
            EditSystem::Hashedit => format!(
                "Read a UTF-8 text file with CRC-32 tagged lines for tag-based editing. Each line is prefixed with 'N|TAG' where TAG is an 8-char hex CRC-32 of the line content. Use these tags with the edit tool for CAS-guarded edits. Defaults to the first {} lines. Files larger than {} bytes require an explicit offset or limit; the selected text must fit within that byte cap.",
                self.max_lines, self.max_text_file_size
            ),
        };
        format!(
            "{text} PNG, JPEG, GIF and WebP images are returned as image blocks (whole regular file, at most 5 MiB) when the captured model/transport supports them; line offset/limit do not apply and an image read never authorizes a text overwrite. .ipynb files default to bounded virtual cell views; offset/limit then address virtual lines. Set raw_notebook:true for raw JSON, raw CRC tags or full-overwrite authorization."
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                "offset": { "type": "integer", "description": "Line number to start from (1-indexed)" },
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of lines to read" },
                "raw_notebook": { "type": "boolean", "description": "For .ipynb: read raw JSON instead of virtual cell/source/output lines; required for raw hashedit or full-file overwrite authorization", "default": false }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: ReadArgs) -> Result<String, ToolError> {
        self.call_with_extensions(args, &rig::tool::ToolCallExtensions::new())
            .await
    }
    async fn call_with_extensions(
        &self,
        args: ReadArgs,
        extensions: &rig::tool::ToolCallExtensions,
    ) -> Result<String, ToolError> {
        Ok(self.call_structured(args, extensions).await?.output)
    }
    async fn call_structured(
        &self,
        args: ReadArgs,
        extensions: &rig::tool::ToolCallExtensions,
    ) -> Result<rig::tool::ToolReturn<String>, ToolError> {
        let mut notice = None;
        #[cfg(feature = "multimodal")]
        let mut image = None;
        let output = self
            .read_inner(
                args,
                extensions,
                &mut notice,
                #[cfg(feature = "multimodal")]
                &mut image,
            )
            .await?;
        #[cfg(feature = "multimodal")]
        let notice = notice.map(|notice| {
            if image.is_some() {
                notice.for_image_result()
            } else {
                notice
            }
        });
        let result = match notice {
            Some(notice) => notice.read_result(output),
            None => rig::tool::ToolReturn::success(output),
        };
        #[cfg(feature = "multimodal")]
        let result = if let Some(image) = image {
            rig::tool::ToolReturn {
                output: image.output(&result.output),
                ..result
            }
        } else {
            result
        };
        Ok(result)
    }
}

impl ReadTool {
    async fn read_inner(
        &self,
        args: ReadArgs,
        extensions: &rig::tool::ToolCallExtensions,
        notice: &mut Option<crate::context::instructions::ContextNotice>,
        #[cfg(feature = "multimodal")] image: &mut Option<super::read_image::ImageRead>,
    ) -> Result<String, ToolError> {
        let notebook_view =
            !args.raw_notebook && super::notebook::is_notebook(Path::new(&args.path));
        let limit = args.limit.unwrap_or(self.max_lines as usize);
        let limit = if notebook_view {
            limit.min(self.max_lines as usize)
        } else {
            limit
        };
        if limit == 0 {
            return Err(ToolError::Msg(r#"read limit must be at least 1 line. Example: {"path":"src/main.rs","offset":1,"limit":100}"#.into()));
        }
        crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let target = crate::agent::tools::resolve_tool_target(self.workspace.as_ref(), &args.path);
        let requested = target.requested;
        let path = requested.to_string_lossy().into_owned();
        let relative = target.bound_relative.as_deref().unwrap_or(Path::new(""));
        let bound_workspace = target.bound_relative.as_ref().and(self.workspace.as_ref());
        let capability_file = bound_workspace
            .map(|workspace| {
                (if notebook_view {
                    workspace.open_regular_relative(relative)
                } else {
                    workspace.open_relative(relative)
                })
                .map_err(|error| crate::agent::tools::bound_open_error(workspace, relative, error))
            })
            .transpose()?;
        let offset = args.offset.unwrap_or(1).saturating_sub(1);
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
            let coaching = check_perm_canonicalized_path(
                &self.permission,
                &self.ask_tx,
                "read",
                &resolved.to_string_lossy(),
            )
            .await?;
            (Some(resolved), coaching)
        };
        let spill_file = resolved.as_deref().and_then(|resolved| {
            self.permission.as_ref().and_then(|permission| {
                permission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .open_tool_output_file(resolved)
            })
        });
        if let Some(instructions) = &self.read_tracker.instructions {
            let instruction_target = if bound_workspace.is_some() {
                relative
            } else {
                resolved.as_deref().unwrap_or(&requested)
            };
            *notice = instructions.notice(instruction_target, extensions).await?;
        }
        let file = if let Some(file) = capability_file {
            tokio::fs::File::from_std(file)
        } else if let Some(file) = spill_file {
            tokio::fs::File::from_std(file?)
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

        if notebook_view {
            use tokio::io::AsyncReadExt;
            let cap = super::notebook::byte_limit(self.max_text_file_size);
            if !metadata.is_file() || metadata.len() > cap {
                return Err(ToolError::Msg("Notebook view requires a regular bounded notebook; use raw_notebook:true for raw line windows".into()));
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            file.take(cap.saturating_add(1))
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() as u64 > cap {
                return Err(ToolError::Msg(
                    "Notebook grew beyond the raw-file cap".into(),
                ));
            }
            let render_path = path.clone();
            let view = crate::agent::runner::spawn_blocking_scoped(move || {
                super::notebook::view(&bytes, &render_path, offset, limit, cap)
            })
            .await
            .map_err(|_| ToolError::Msg("Notebook view job failed".into()))??;
            if let Some(blocked) = self.read_tracker.check_notebook_view(
                &permission_path,
                offset,
                limit,
                &metadata,
                view.crc,
            ) {
                return Err(ToolError::Msg(blocked));
            }
            self.read_tracker.record_notebook_view(
                &permission_path,
                offset,
                limit,
                &metadata,
                view.crc,
            );
            return Ok(match coaching {
                Some(coaching) => format!("{coaching}\n\n{}", view.text),
                None => view.text,
            });
        }

        let mut reader = BufReader::new(file);
        #[cfg(not(feature = "multimodal"))]
        if super::read_image::ImageFormat::from_magic(reader.fill_buf().await?).is_some() {
            return Err(ToolError::Msg(
                "Image reads require the multimodal feature".into(),
            ));
        }
        #[cfg(feature = "multimodal")]
        if let Some(format) = super::read_image::ImageFormat::from_magic(reader.fill_buf().await?) {
            use base64::Engine;
            use tokio::io::AsyncReadExt;
            self.read_tracker
                .image_support
                .check(format)
                .map_err(|message| ToolError::Msg(message.into()))?;
            if args.offset.is_some() || args.limit.is_some() {
                return Err(ToolError::Msg(
                    "Image reads do not accept text line offset/limit; read the whole image".into(),
                ));
            }
            let cap = super::read_image::MAX_IMAGE_BYTES;
            if !metadata.is_file() || metadata.len() > cap as u64 {
                return Err(ToolError::Msg(
                    "Image reads require a regular file no larger than 5 MiB".into(),
                ));
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            reader
                .take((cap + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > cap {
                return Err(ToolError::Msg(
                    "Image grew beyond the 5 MiB input limit".into(),
                ));
            }
            let info = super::read_image::inspect(&bytes)
                .map_err(|message| ToolError::Msg(message.into()))?
                .ok_or_else(|| ToolError::Msg("Image signature changed during the read".into()))?;
            self.read_tracker.untrack_read_path(&permission_path);
            let summary = format!(
                "Image: {path} ({}; {} x {}; {} bytes)",
                info.format.mime(),
                info.width,
                info.height,
                info.bytes
            );
            *image = Some(super::read_image::ImageRead {
                mime: info.format.mime(),
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            });
            return Ok(match coaching {
                Some(coaching) => format!("{coaching}\n\n{summary}"),
                None => summary,
            });
        }
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
        let hashedit = es == EditSystem::Hashedit;
        let requested_end = offset.saturating_add(limit);
        let mut excerpt_lines: Vec<String> = Vec::with_capacity(limit.min(256));
        let mut excerpt_bytes = 0_u64;
        let mut total_lines = 0_usize;
        let mut has_more_lines = false;
        let mut file_crc = Crc32::new();
        let mut served_content_crc = Crc32::new();
        let mut utf8 = Utf8Stream::default();
        // Lines are consumed in bounded chunks: an unselected or oversized line
        // is scanned without ever being retained, so a single arbitrarily long
        // line cannot allocate proportionally to its length in the parent.
        // Scanning is also bounded by the size observed when the file was
        // opened, so a file growing under the read cannot extend it without end.
        let mut scan_budget = file_size;
        let mut grew_during_read = false;

        'lines: loop {
            let line_index = total_lines;
            let selected = line_index >= offset && line_index < requested_end;
            let separator_bytes = u64::from(!excerpt_lines.is_empty());
            let mut retained: Vec<u8> = Vec::new();
            let mut held_cr = false;
            let mut saw_bytes = false;
            let mut terminated = false;

            loop {
                if scan_budget == 0 {
                    if !reader.fill_buf().await?.is_empty() {
                        grew_during_read = true;
                    }
                    break;
                }
                let available = reader.fill_buf().await?;
                if available.is_empty() {
                    break;
                }
                let visible = available.len().min(scan_budget as usize);
                let available = &available[..visible];
                let (body, consumed, ends_line) =
                    match available.iter().position(|byte| *byte == b'\n') {
                        Some(position) => (&available[..position], position + 1, true),
                        None => (available, available.len(), false),
                    };
                if !utf8.push(body) {
                    return Err(not_utf8(&path));
                }
                // A carriage return at the end of a chunk is held back until the
                // next byte decides whether it belongs to a CRLF terminator.
                if !body.is_empty() {
                    let mut chunk = body;
                    if held_cr {
                        absorb(b"\r", hashedit, &mut file_crc, selected, &mut retained);
                        held_cr = false;
                    }
                    if chunk.last() == Some(&b'\r') {
                        held_cr = true;
                        chunk = &chunk[..chunk.len() - 1];
                    }
                    absorb(chunk, hashedit, &mut file_crc, selected, &mut retained);
                }
                reader.consume(consumed);
                scan_budget = scan_budget.saturating_sub(consumed as u64);
                saw_bytes = true;
                if selected
                    && excerpt_bytes
                        .saturating_add(separator_bytes)
                        .saturating_add(retained.len() as u64)
                        > self.max_text_file_size
                {
                    return Err(excerpt_cap_exceeded(self.max_text_file_size));
                }
                if ends_line {
                    terminated = true;
                    break;
                }
            }

            if !saw_bytes {
                break 'lines;
            }
            if terminated {
                // A carriage return immediately before the newline is part of
                // the terminator: the held byte is simply dropped, belonging to
                // neither the CRC nor the excerpt.
                if hashedit {
                    file_crc.update(b"\n");
                }
            } else if held_cr {
                absorb(b"\r", hashedit, &mut file_crc, selected, &mut retained);
            }

            // A multi-byte sequence never spans a newline, so an incomplete
            // sequence at the end of a line is invalid rather than pending.
            if !utf8.finish() {
                return Err(not_utf8(&path));
            }

            total_lines += 1;
            if selected {
                let line = String::from_utf8(retained)
                    .expect("line bytes were validated as UTF-8 while streaming");
                excerpt_bytes = excerpt_bytes
                    .saturating_add(separator_bytes)
                    .saturating_add(line.len() as u64);
                served_content_crc.update(&(line.len() as u64).to_le_bytes());
                served_content_crc.update(line.as_bytes());
                excerpt_lines.push(line);
            }

            if !scan_to_eof && total_lines > requested_end {
                has_more_lines = true;
                break 'lines;
            }
        }

        if grew_during_read {
            has_more_lines = true;
            if hashedit {
                return Err(ToolError::Msg(format!(
                    "'{path}' grew while it was being read, so the whole-file CRC would not describe the file on disk. Read it again to obtain a current CRC."
                )));
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
    #[tokio::test]
    async fn zero_read_limit_is_rejected_before_path_or_permission_effects() {
        use super::*;
        let tool = ReadTool::new(None, None, None, 100);
        let error = tool
            .call(crate::agent::tools::ReadArgs {
                raw_notebook: false,
                path: "missing-file".into(),
                offset: Some(1),
                limit: Some(0),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("read limit must be at least 1"));
        assert!(error.contains(r#"{"path":"src/main.rs","offset":1,"limit":100}"#));
        assert!(!error.contains("No such file"));
        let tool = ReadTool::new(None, None, None, 0);
        assert!(
            tool.call(crate::agent::tools::ReadArgs {
                raw_notebook: false,
                path: "missing-file".into(),
                offset: None,
                limit: None
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("read limit")
        );
    }

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
                raw_notebook: false,
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
                raw_notebook: false,
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
                raw_notebook: false,
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
                raw_notebook: false,
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
            raw_notebook: false,
            path: link.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
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
