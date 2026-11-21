use std::io::Read;
use std::path::{Path, PathBuf};

use super::mutation_diff::{DiffOptions, MutationDiff, MutationOutcome};
use rig::tool::{Tool, ToolCallExtensions, ToolReturn};
use tokio::io::AsyncReadExt;

use crate::agent::tools::crc::Crc32;
use crate::agent::tools::{
    AskSender, PermCheck, ReadTracker, ToolError, WriteArgs, check_perm_bound_path, check_perm_path,
};
#[cfg(feature = "lsp")]
use crate::extras::lsp::LspManager;

const DEFAULT_MAX_TEXT_SIZE: u64 = crate::config::DEFAULT_MAX_EDIT_FILE_SIZE;

pub(crate) fn full_text_fingerprint(bytes: &[u8]) -> Result<(u32, usize), ToolError> {
    std::str::from_utf8(bytes).map_err(|_| {
        ToolError::Msg(
            "overwrite=true requires a complete prior read of the current UTF-8 text file"
                .to_string(),
        )
    })?;
    let mut crc = Crc32::new();
    let mut lines = 0usize;
    for raw_line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let mut content_end = raw_line.len();
        if raw_line.get(content_end.saturating_sub(1)) == Some(&b'\n') {
            content_end -= 1;
            if raw_line.get(content_end.saturating_sub(1)) == Some(&b'\r') {
                content_end -= 1;
            }
        }
        let line = &raw_line[..content_end];
        crc.update(&(line.len() as u64).to_le_bytes());
        crc.update(line);
        lines += 1;
    }
    Ok((crc.finalize(), lines))
}

fn overwrite_not_authorized(path: &str) -> ToolError {
    ToolError::Msg(format!(
        "Cannot overwrite '{path}': overwrite=true requires a complete current read of the file. Read it from offset 1 through EOF, then retry without any intervening change. A read that clipped long lines (marked 'line N truncated') never authorizes overwrite because the clipped text was not seen: read those lines with character_offset windows and use edit for changes, or replace the file with the shell tool."
    ))
}

/// The not-authorized error plus, when the file cannot fit one read result,
/// the real cause: windows do not compose into a complete read. A file that
/// fits one result but has more lines than the default read window only
/// needs an explicit `limit` (read accepts any limit).
fn overwrite_not_authorized_sized(path: &str, bytes: usize, lines: usize) -> ToolError {
    /// Default `max_read_lines`; an explicit read `limit` may exceed it.
    const DEFAULT_READ_LINES: usize = 2_000;
    let limits = crate::agent::tools::limits::current();
    let chars_budget = limits
        .max_result_chars
        .saturating_sub(super::read::READ_RESULT_RESERVE_CHARS);
    if bytes <= chars_budget {
        let error = overwrite_not_authorized(path);
        if lines <= DEFAULT_READ_LINES {
            return error;
        }
        return ToolError::Msg(format!(
            "{error} This file has {lines} lines, more than the default read window: read it in one call with offset 1 and limit >= {lines}, then retry."
        ));
    }
    ToolError::Msg(format!(
        "Cannot overwrite '{path}': overwrite=true requires one complete read, but this file ({bytes} bytes, {lines} lines) is larger than a single read result can hold (max_result_chars={} characters, about {} bytes of text); reading it in several windows does not add up to a complete read. Use the edit tool for targeted changes, or replace it with the shell tool (for example write the new content to a temporary file and move it over this path).",
        limits.max_result_chars, chars_budget
    ))
}

fn create_error(path: &str, error: std::io::Error) -> ToolError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        ToolError::Msg(format!(
            "File '{path}' already exists. Use the edit tool for targeted changes. For an intentional full replacement, read the complete current file and then call the write tool with overwrite=true."
        ))
    } else {
        error.into()
    }
}

#[cfg(test)]
fn read_bounded_overwrite(
    reader: &mut impl Read,
    recorded: u64,
    limit: u64,
    path: &str,
) -> Result<Vec<u8>, ToolError> {
    if recorded > limit {
        return Err(overwrite_not_authorized(path));
    }
    let mut bytes = Vec::with_capacity(recorded.min(limit) as usize);
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(overwrite_not_authorized(path));
    }
    Ok(bytes)
}

async fn read_bounded_overwrite_async(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    recorded: u64,
    limit: u64,
    path: &str,
) -> Result<Vec<u8>, ToolError> {
    if recorded > limit {
        return Err(overwrite_not_authorized(path));
    }
    let mut bytes = Vec::with_capacity(recorded.min(limit) as usize);
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > limit {
        return Err(overwrite_not_authorized(path));
    }
    Ok(bytes)
}

pub struct WriteTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_text_file_size: u64,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    read_tracker: ReadTracker,
    diff_options: DiffOptions,
    formatter: Option<std::sync::Arc<super::format_after_edit::FormatAfterEdit>>,
    /// When `Some`, written files are synced to their language server and
    /// fresh diagnostics are appended to the tool result.
    #[cfg(feature = "lsp")]
    pub lsp: Option<LspManager>,
}

impl WriteTool {
    #[cfg(test)]
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        max_text_file_size: Option<u64>,
    ) -> Self {
        Self::new_with_tracker(
            permission,
            ask_tx,
            max_text_file_size,
            ReadTracker::new(true),
        )
    }

    pub(crate) fn new_with_tracker(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        max_text_file_size: Option<u64>,
        read_tracker: ReadTracker,
    ) -> Self {
        WriteTool {
            permission,
            ask_tx,
            max_text_file_size: max_text_file_size.unwrap_or(DEFAULT_MAX_TEXT_SIZE),
            workspace: None,
            read_tracker,
            diff_options: DiffOptions::default(),
            formatter: None,
            #[cfg(feature = "lsp")]
            lsp: None,
        }
    }

    #[cfg(test)]
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace = Some(crate::agent::tools::capture_workspace_binding(root));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_workspace(self, root: impl Into<PathBuf>) -> Self {
        self.with_workspace_root(root.into())
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub(crate) fn with_diff_options(mut self, options: DiffOptions) -> Self {
        self.diff_options = options;
        self
    }

    pub(crate) fn with_formatter(
        mut self,
        formatter: Option<std::sync::Arc<super::format_after_edit::FormatAfterEdit>>,
    ) -> Self {
        self.formatter = formatter;
        self
    }

    #[cfg(feature = "lsp")]
    pub fn with_lsp(mut self, lsp: Option<LspManager>) -> Self {
        self.lsp = lsp;
        self
    }
}

async fn resolve_write_path(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = crate::fs::resolve_symlink_target(path).await;
    let mut ancestor = if resolved.is_absolute() {
        resolved
    } else {
        std::env::current_dir()?.join(resolved)
    };
    let mut missing_components = Vec::new();

    loop {
        match tokio::fs::canonicalize(&ancestor).await {
            Ok(mut canonical) => {
                for component in missing_components.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(component) = ancestor.file_name().map(|name| name.to_os_string()) else {
                    return Err(error);
                };
                if !ancestor.pop() {
                    return Err(error);
                }
                missing_components.push(component);
            }
            Err(error) => return Err(error),
        }
    }
}

impl Tool for WriteTool {
    const NAME: &'static str = "write";

    type Error = ToolError;
    type Args = WriteArgs;
    type Output = String;

    fn description(&self) -> String {
        "Create a new file with the given content. Use edit for targeted changes to existing files. A complete current read permits one guarded full replacement with overwrite=true. Automatically creates parent directories.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" },
                "overwrite": { "type": "boolean", "description": "Replace an existing file only after its complete current contents were read (default false)" }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, args: WriteArgs) -> Result<String, ToolError> {
        self.apply(args, &ToolCallExtensions::new())
            .await
            .map(|result| result.output)
    }

    async fn call_structured(
        &self,
        args: WriteArgs,
        extensions: &ToolCallExtensions,
    ) -> Result<ToolReturn<String>, ToolError> {
        match self.apply(args, extensions).await {
            Ok(result) => Ok(result.into_return()),
            Err(ToolError::ContextReview(notice)) => Ok(notice.not_executed()),
            Err(error) => Err(error),
        }
    }
}

impl WriteTool {
    async fn apply(
        &self,
        mut args: WriteArgs,
        extensions: &ToolCallExtensions,
    ) -> Result<MutationOutcome, ToolError> {
        crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let target = crate::agent::tools::resolve_tool_target(self.workspace.as_ref(), &args.path);
        let requested = target.requested;
        let expanded = requested.to_string_lossy().into_owned();
        tracing::debug!(
            "tool write start: path={}, content_len={}",
            expanded,
            args.content.len(),
        );
        let bytes = args.content.len();
        if bytes as u64 > self.max_text_file_size {
            tracing::debug!(
                "tool write file too large: path={}, size={}, max={}",
                expanded,
                bytes,
                self.max_text_file_size,
            );
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes). Maximum allowed file size is {} bytes (max_edit_file_size).",
                bytes, self.max_text_file_size
            )));
        }
        if let Some(relative) = target.bound_relative.as_deref()
            && let Some(workspace) = &self.workspace
        {
            // Nothing is opened before the prompt, so the exclusive lane can be
            // given back while the user decides: the target is opened and read
            // only after the lane is re-acquired.
            let (coaching, _released) = super::concurrency::prompt_unlocked(check_perm_bound_path(
                &self.permission,
                &self.ask_tx,
                "write",
                workspace,
                relative,
            ))
            .await;
            let coaching = coaching?;
            if let Some(instructions) = &self.read_tracker.instructions {
                instructions.before_mutation(relative, extensions).await?;
            }
            let mut preview_before = None;
            let mut created = false;
            let mut checkpoint;
            let mut digests = None;
            let mut keep_bom = false;
            let mut bom_file = false;
            let mut hard_link_note = None;
            let mut client_written = false;
            // Open, stat and read run on the blocking pool; the decision logic stays here.
            enum Probe {
                Exists,
                TooLarge,
                Read(Box<(std::fs::Metadata, crate::fs::CheckedMetadata, Vec<u8>)>),
            }
            let probe = {
                let workspace = std::sync::Arc::clone(workspace);
                let relative = relative.to_path_buf();
                let overwrite = args.overwrite;
                let limit = self.max_text_file_size;
                crate::agent::runner::spawn_blocking_scoped(move || {
                    let mut existing = workspace.open_relative(&relative)?;
                    let metadata = existing.metadata()?;
                    if metadata.is_dir() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::IsADirectory,
                            "path is a directory, not a file",
                        ));
                    }
                    if !overwrite {
                        return Ok(Probe::Exists);
                    }
                    if metadata.len() > limit {
                        return Ok(Probe::TooLarge);
                    }
                    let expected = crate::fs::checked_file_metadata(&existing)?;
                    // Bind the read itself, not only the earlier metadata
                    // length: the file may grow while its descriptor remains
                    // open. A regrown file is reported the same way a file
                    // already too large at the metadata check is; a real read
                    // error propagates as an I/O error instead.
                    let mut current = Vec::with_capacity(metadata.len().min(limit) as usize);
                    (&mut existing)
                        .take(limit.saturating_add(1))
                        .read_to_end(&mut current)?;
                    if current.len() as u64 > limit {
                        return Ok(Probe::TooLarge);
                    }
                    Ok::<_, std::io::Error>(Probe::Read(Box::new((metadata, expected, current))))
                })
                .await
                .map_err(|_| ToolError::Msg("Write open job failed".to_string()))?
            };
            match probe {
                Ok(Probe::Exists) => {
                    return Err(create_error(
                        &expanded,
                        std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                    ));
                }
                Ok(Probe::TooLarge) => return Err(overwrite_not_authorized(&expanded)),
                Ok(Probe::Read(read)) => {
                    let (metadata, expected, mut current) = *read;
                    // A client that holds this file in an editor supplies its
                    // live text as the preimage, and receives the replacement.
                    // The target already passed the workspace, symlink and
                    // permission checks; only the bytes' source changes.
                    let mut client_overwrite = None;
                    if let Some(client) =
                        super::client_fs::read_write_client(self.read_tracker.client_fs.as_ref())
                        && std::str::from_utf8(&current).is_ok()
                    {
                        let client_path = super::client_fs::client_path(workspace, relative);
                        if let Some(text) =
                            super::client_fs::read_client_text(client, &client_path).await
                        {
                            current = text.clone().into_bytes();
                            client_overwrite =
                                Some((std::sync::Arc::clone(client), client_path, text));
                        }
                    }
                    // A UTF-8 BOM is not text: the read tool skips it, so the
                    // fingerprint covers the bytes after it and the mark is
                    // re-attached to the replacement.
                    let (text_bytes, had_bom) = super::text_encoding::strip_utf8_bom(&current);
                    bom_file = had_bom;
                    if client_overwrite.is_none() {
                        hard_link_note = super::hard_link_note(&metadata);
                    }
                    keep_bom = had_bom && !args.content.starts_with('\u{feff}');
                    let (crc, lines) = full_text_fingerprint(text_bytes)?;
                    if !self
                        .read_tracker
                        .permits_full_overwrite(&expanded, &metadata, crc, lines)
                    {
                        return Err(overwrite_not_authorized_sized(
                            &expanded,
                            current.len(),
                            lines,
                        ));
                    }
                    let base = crate::fs::ContentDigest::of(&current);
                    // One shared copy of the preimage serves both the checkpoint
                    // and the diff preview; `current` is dropped after the copy.
                    let (text_bytes, had_bom) = super::text_encoding::strip_utf8_bom(&current);
                    let shared: std::sync::Arc<str> = std::sync::Arc::from(
                        std::str::from_utf8(text_bytes)
                            .expect("full text fingerprint validates UTF-8"),
                    );
                    let raw_preimage: std::sync::Arc<[u8]> = if had_bom {
                        std::sync::Arc::from(&current[..])
                    } else {
                        std::sync::Arc::from(std::sync::Arc::clone(&shared))
                    };
                    drop(current);
                    if self.diff_options.max_lines > 0
                        && shared.len() <= super::mutation_diff::MAX_INPUT_BYTES
                        && super::mutation_diff::preimage_allowed(&self.permission, &expanded, true)
                    {
                        preview_before = Some(std::sync::Arc::clone(&shared));
                    }
                    checkpoint = crate::session::file_checkpoints::prepare(
                        workspace,
                        relative,
                        Some(crate::session::file_checkpoints::Preimage::new(
                            raw_preimage,
                            &base,
                        )),
                        super::mutation_diff::preimage_allowed(&self.permission, &expanded, true),
                    )
                    .await;
                    digests = Some(base);
                    if let Some((client, client_path, before)) = &client_overwrite {
                        client_written = true;
                        super::client_fs::write_client_text(
                            client,
                            client_path,
                            before,
                            &args.content,
                        )
                        .await
                        .map_err(|message| {
                            ToolError::Msg(format!(
                                "Write to '{expanded}' was not applied: {message}"
                            ))
                        })?;
                    } else {
                        let workspace = std::sync::Arc::clone(workspace);
                        let owned_relative = relative.to_path_buf();
                        // Move the content through the blocking publication and take
                        // it back instead of copying it.
                        let content = std::mem::take(&mut args.content);
                        let bom = keep_bom;
                        let (published, content) =
                            crate::agent::runner::spawn_blocking_scoped(move || {
                                let with_bom;
                                let bytes = if bom {
                                    with_bom =
                                        [&super::text_encoding::UTF8_BOM[..], content.as_bytes()]
                                            .concat();
                                    &with_bom[..]
                                } else {
                                    content.as_bytes()
                                };
                                let published = workspace.replace_relative_atomic_expecting(
                                    &owned_relative,
                                    bytes,
                                    &expected,
                                    &base,
                                );
                                (published, content)
                            })
                            .await
                            .map_err(|_| {
                                ToolError::Msg("Write publication job failed".to_string())
                            })?;
                        args.content = content;
                        published?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    checkpoint =
                        crate::session::file_checkpoints::prepare(workspace, relative, None, true)
                            .await;
                    let owned = std::sync::Arc::clone(workspace);
                    let owned_relative = relative.to_path_buf();
                    let content = std::mem::take(&mut args.content);
                    let (created_file, content) =
                        crate::agent::runner::spawn_blocking_scoped(move || {
                            let created =
                                owned.create_relative_atomic(&owned_relative, content.as_bytes());
                            (created, content)
                        })
                        .await
                        .map_err(|_| ToolError::Msg("Write creation job failed".to_string()))?;
                    args.content = content;
                    created_file.map_err(|error| match error.kind() {
                        std::io::ErrorKind::AlreadyExists => create_error(&expanded, error),
                        _ => crate::agent::tools::bound_open_error(workspace, relative, error),
                    })?;
                    created = true;
                }
                Err(error) => {
                    return Err(crate::agent::tools::bound_open_error(
                        workspace, relative, error,
                    ));
                }
            }
            crate::session::file_checkpoints::published();
            let (after_digest, written_len) = if keep_bom {
                let published =
                    [&super::text_encoding::UTF8_BOM[..], args.content.as_bytes()].concat();
                (crate::fs::ContentDigest::of(&published), published.len())
            } else {
                (
                    crate::fs::ContentDigest::of(args.content.as_bytes()),
                    args.content.len(),
                )
            };
            if let Some(checkpoint) = &mut checkpoint {
                checkpoint.committed(&after_digest, written_len);
            }
            self.read_tracker.untrack_read_path(&expanded);
            let formatted = match &self.formatter {
                // The formatter checks the file bytes against the text it is
                // given; a file that keeps a BOM is left as written.
                Some(formatter) if !keep_bom => {
                    crate::startup::heap_future(|| {
                        formatter.run(
                            &requested,
                            Some((workspace, relative)),
                            &args.content,
                            &self.permission,
                            &self.ask_tx,
                            self.max_text_file_size,
                        )
                    })
                    .await
                }
                _ => None,
            };
            let final_content = match &formatted {
                Some(formatted) => formatted.content.as_deref(),
                None => Some(args.content.as_str()),
            };
            if let Some(checkpoint) = &checkpoint
                && let Some(content) = formatted.as_ref().and_then(|f| f.content.as_deref())
            {
                checkpoint.formatted(Some(content));
            }
            // Unformatted output is exactly the bytes already digested above.
            let unformatted = formatted.is_none();
            let digest_exact = unformatted && !bom_file;
            // The model supplied exactly these bytes, so it has "seen" the
            // file: a following overwrite needs no read. A formatter rewrite
            // or an editor-held (client) write invalidates that knowledge.
            if unformatted
                && !client_written
                && let Ok((crc, lines)) = full_text_fingerprint(args.content.as_bytes())
            {
                let reopened = {
                    let workspace = std::sync::Arc::clone(workspace);
                    let relative = relative.to_path_buf();
                    crate::agent::runner::spawn_blocking_scoped(move || {
                        workspace
                            .open_relative(&relative)
                            .and_then(|file| file.metadata())
                    })
                    .await
                };
                if let Ok(Ok(metadata)) = reopened {
                    self.read_tracker
                        .record_authored_full_text(&expanded, &metadata, crc, lines);
                }
            }
            let mut diff = if created {
                final_content.and_then(|content| {
                    if digest_exact {
                        MutationDiff::replacement_with_digests(
                            &expanded,
                            None,
                            content,
                            self.diff_options,
                            None,
                            after_digest.hex(),
                        )
                    } else {
                        MutationDiff::replacement(&expanded, None, content, self.diff_options)
                    }
                })
            } else {
                preview_before
                    .as_deref()
                    .zip(final_content)
                    .and_then(|(before, content)| {
                        if digest_exact {
                            MutationDiff::replacement_with_digests(
                                &expanded,
                                Some(before),
                                content,
                                self.diff_options,
                                digests.map(|digest| digest.hex()),
                                after_digest.hex(),
                            )
                        } else {
                            MutationDiff::replacement(
                                &expanded,
                                Some(before),
                                content,
                                self.diff_options,
                            )
                        }
                    })
            };
            let mut result = format!("Written {} bytes to {}", bytes, expanded);
            if let Some(note) = &hard_link_note {
                result.push_str(&format!("\n  Note: {note}"));
            }
            if let Some(msg) = coaching {
                result = format!("{}\n\n{}", msg, result);
            }
            if let Some(formatted) = &formatted {
                result.push('\n');
                result.push_str(&formatted.status);
            }
            let private_fallback = formatted.as_ref().map(|_| result.clone());
            #[cfg(feature = "lsp")]
            if let Some(lsp) = &self.lsp
                && formatted
                    .as_ref()
                    .is_none_or(|formatted| formatted.notify_lsp)
            {
                let baseline = lsp.notify_changed_relative(relative).await;
                if let Some(block) = lsp
                    .diagnostics_block_for_relative_edit(relative, baseline)
                    .await
                    && (formatted.is_none()
                        || super::mutation_diff::preimage_allowed(
                            &self.permission,
                            &expanded,
                            true,
                        ))
                {
                    result.push_str(&block);
                }
            }
            if formatted.is_some()
                && (!super::mutation_diff::preimage_allowed(&self.permission, &expanded, true)
                    || crate::agent::runner::current_work_scope_is_cancelled()
                    || crate::question::required().is_some())
            {
                result = private_fallback.expect("configured formatting retains a static fallback");
                diff = None;
            }
            if let Some(notice) = crate::session::file_checkpoints::take_uncaptured_notice() {
                result.push_str(&notice);
            }
            return Ok(MutationOutcome {
                output: result,
                diff,
            });
        }

        let resolved = resolve_write_path(&requested).await?;
        let path = resolved.as_path();
        // Check the path atomic_write will modify, not a symlink that points to it.
        // Only the resolved path precedes the prompt; the target is opened and
        // read after the exclusive lane is re-acquired and the path is
        // re-resolved below ("Path changed after permission check").
        let (coaching, _released) = super::concurrency::prompt_unlocked(check_perm_path(
            &self.permission,
            &self.ask_tx,
            "write",
            &path.to_string_lossy(),
        ))
        .await;
        let coaching = coaching?;

        if let Some(instructions) = &self.read_tracker.instructions {
            instructions.before_mutation(path, extensions).await?;
        }
        let mut preview_before = None;
        let mut keep_bom = false;
        let existing = if path.exists() {
            tracing::debug!("tool write file exists: path={}", expanded);
            if !args.overwrite {
                return Err(create_error(
                    &expanded,
                    std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                ));
            }
            let mut file = crate::fs::open_stable_file(path).await?;
            let metadata = file.metadata().await?;
            if metadata.len() > self.max_text_file_size {
                return Err(overwrite_not_authorized(&expanded));
            }
            let current = read_bounded_overwrite_async(
                &mut file,
                metadata.len(),
                self.max_text_file_size,
                &expanded,
            )
            .await?;
            let (text_bytes, had_bom) = super::text_encoding::strip_utf8_bom(&current);
            keep_bom = had_bom && !args.content.starts_with('\u{feff}');
            let (crc, lines) = full_text_fingerprint(text_bytes)?;
            if !self.read_tracker.permits_full_overwrite(
                &path.to_string_lossy(),
                &metadata,
                crc,
                lines,
            ) {
                return Err(overwrite_not_authorized_sized(
                    &expanded,
                    current.len(),
                    lines,
                ));
            }
            if self.diff_options.max_lines > 0
                && current.len() <= super::mutation_diff::MAX_INPUT_BYTES
                && super::mutation_diff::preimage_allowed(
                    &self.permission,
                    &path.to_string_lossy(),
                    false,
                )
            {
                preview_before = Some(
                    std::str::from_utf8(text_bytes)
                        .expect("full text fingerprint validates UTF-8")
                        .to_owned(),
                );
            }
            Some(crate::fs::ContentDigest::of(&current))
        } else {
            None
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let current = resolve_write_path(&requested).await?;
        if current != resolved {
            return Err(ToolError::Msg(format!(
                "Path changed after permission check: {}",
                expanded
            )));
        }
        let approved_parent = crate::fs::stable_path_metadata(path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write target has no parent directory",
            )
        })?)
        .await?;
        let created = existing.is_none();
        crate::session::file_checkpoints::skipped();
        if let Some(base) = existing {
            let published;
            let content: &str = if keep_bom {
                published = format!("\u{feff}{}", args.content);
                &published
            } else {
                &args.content
            };
            crate::fs::atomic_write_resolved_expecting(path, content, approved_parent, base)
                .await?;
        } else {
            crate::fs::atomic_create_resolved_checked(path, &args.content, approved_parent)
                .await
                .map_err(|error| create_error(&expanded, error))?;
        }
        crate::session::file_checkpoints::published();
        self.read_tracker.untrack_read_path(&path.to_string_lossy());
        let formatted = match &self.formatter {
            Some(formatter) if !keep_bom => {
                crate::startup::heap_future(|| {
                    formatter.run(
                        path,
                        None,
                        &args.content,
                        &self.permission,
                        &self.ask_tx,
                        self.max_text_file_size,
                    )
                })
                .await
            }
            _ => None,
        };
        let final_content = match &formatted {
            Some(formatted) => formatted.content.as_deref(),
            None => Some(args.content.as_str()),
        };
        let mut diff = if created {
            final_content.and_then(|content| {
                MutationDiff::replacement(&path.to_string_lossy(), None, content, self.diff_options)
            })
        } else {
            preview_before
                .as_deref()
                .zip(final_content)
                .and_then(|(before, content)| {
                    MutationDiff::replacement(
                        &path.to_string_lossy(),
                        Some(before),
                        content,
                        self.diff_options,
                    )
                })
        };
        tracing::debug!("tool write done: path={}, bytes={}", expanded, bytes);
        let mut result = format!("Written {} bytes to {}", bytes, expanded);
        if let Some(msg) = coaching {
            result = format!("{}\n\n{}", msg, result);
        }

        if let Some(formatted) = &formatted {
            result.push('\n');
            result.push_str(&formatted.status);
        }
        let private_fallback = formatted.as_ref().map(|_| result.clone());
        #[cfg(feature = "lsp")]
        if let Some(lsp) = &self.lsp
            && formatted
                .as_ref()
                .is_none_or(|formatted| formatted.notify_lsp)
        {
            let baseline = lsp.notify_changed(path).await;
            if let Some(block) = lsp.diagnostics_block_for_edit(path, baseline).await
                && (formatted.is_none()
                    || super::mutation_diff::preimage_allowed(
                        &self.permission,
                        &path.to_string_lossy(),
                        false,
                    ))
            {
                result.push_str(&block);
            }
        }

        if formatted.is_some()
            && (!super::mutation_diff::preimage_allowed(
                &self.permission,
                &path.to_string_lossy(),
                false,
            ) || crate::agent::runner::current_work_scope_is_cancelled()
                || crate::question::required().is_some())
        {
            result = private_fallback.expect("configured formatting retains a static fallback");
            diff = None;
        }
        if let Some(notice) = crate::session::file_checkpoints::take_uncaptured_notice() {
            result.push_str(&notice);
        }
        Ok(MutationOutcome {
            output: result,
            diff,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::agent::tools::{ReadArgs, ReadTool};
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{PermissionConfig, PermissionConfigs, SecurityMode};

    use crate::tests::support::temp::TempDir;

    fn plan_write_tool(workspace: &Path) -> WriteTool {
        let checker = PermissionChecker::new(
            &PermissionConfigs::from(PermissionConfig::default()),
            SecurityMode::PlanWrite,
            Some(workspace.to_path_buf()),
            Some(vec!["planwrite".to_string()]),
        )
        .expect("valid PlanWrite permission fixture");
        WriteTool::new(Some(Arc::new(Mutex::new(checker))), None, None)
            .with_workspace(workspace.to_path_buf())
    }

    #[tokio::test]
    async fn overwrite_reads_recheck_the_limit_after_the_recorded_size() {
        let payload = b"0123456789";
        let mut sync_reader = std::io::Cursor::new(payload);
        let sync_error = read_bounded_overwrite(&mut sync_reader, 4, 5, "growing.txt")
            .unwrap_err()
            .to_string();
        assert!(sync_error.contains("complete current read"));

        let mut async_reader = std::io::Cursor::new(payload);
        let async_error = read_bounded_overwrite_async(&mut async_reader, 4, 5, "growing.txt")
            .await
            .unwrap_err()
            .to_string();
        assert!(async_error.contains("complete current read"));
    }

    #[tokio::test]
    async fn workspace_write_reports_existing_files_as_an_actionable_tool_error() {
        let temp = TempDir::new("write-permission");
        std::fs::write(temp.path().join("existing.txt"), "original").unwrap();
        let error = WriteTool::new(None, None, None)
            .with_workspace(temp.path())
            .call(WriteArgs {
                path: "existing.txt".into(),
                content: "replacement".into(),
                overwrite: false,
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("already exists"), "{error}");
        assert!(error.contains("edit tool"), "{error}");
        assert!(error.contains("write tool with overwrite=true"), "{error}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("existing.txt")).unwrap(),
            "original"
        );
    }

    #[tokio::test]
    async fn overwrite_requires_and_consumes_a_complete_current_read() {
        let temp = TempDir::new("write-permission");
        let target = temp.path().join("existing.txt");
        std::fs::write(&target, "original\n").unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(temp.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());

        let denied = write
            .call(WriteArgs {
                path: "existing.txt".into(),
                content: "replacement\n".into(),
                overwrite: true,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(denied.contains("complete current read"), "{denied}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");

        read.call(ReadArgs {
            pages: None,
            raw_notebook: false,
            character_offset: None,
            character_limit: None,
            path: "existing.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
        write
            .call(WriteArgs {
                path: "existing.txt".into(),
                content: "replacement\n".into(),
                overwrite: true,
            })
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(target).unwrap(), "replacement\n");
    }

    fn rd(path: &str) -> ReadArgs {
        ReadArgs {
            pages: None,
            raw_notebook: false,
            character_offset: None,
            character_limit: None,
            path: path.into(),
            offset: None,
            limit: None,
        }
    }

    fn wr(path: &str, content: &str) -> WriteArgs {
        WriteArgs {
            path: path.into(),
            content: content.into(),
            overwrite: true,
        }
    }

    // mini-agent-nvnkx.24
    #[tokio::test]
    async fn a_file_the_agent_just_wrote_can_be_overwritten_without_a_read() {
        let temp = TempDir::new("write-own-overwrite");
        let tracker = ReadTracker::new(true);
        let write = WriteTool::new_with_tracker(None, None, None, tracker.clone())
            .with_workspace(temp.path());
        let create = WriteArgs {
            path: "own.txt".into(),
            content: "one\ntwo\n".into(),
            overwrite: false,
        };
        write.call(create).await.unwrap();
        write.call(wr("own.txt", "three\n")).await.unwrap();
        write.call(wr("own.txt", "four\nfive\n")).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("own.txt")).unwrap(),
            "four\nfive\n"
        );
    }

    #[tokio::test]
    async fn an_external_change_after_own_write_still_requires_a_read() {
        let temp = TempDir::new("write-own-external");
        let tracker = ReadTracker::new(true);
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());
        write
            .call(WriteArgs {
                path: "own.txt".into(),
                content: "one\n".into(),
                overwrite: false,
            })
            .await
            .unwrap();
        // Same length, so only the content CRC can tell.
        std::fs::write(temp.path().join("own.txt"), "two\n").unwrap();
        let error = write
            .call(wr("own.txt", "three\n"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("complete current read"), "{error}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("own.txt")).unwrap(),
            "two\n"
        );
    }

    #[tokio::test]
    async fn a_formatter_rewrite_keeps_the_read_requirement_after_own_write() {
        let temp = TempDir::new("write-own-formatter");
        let formatter = crate::agent::tools::format_after_edit::FormatAfterEdit::new(
            &std::collections::BTreeMap::from([("txt".into(), "true \"{file}\"".into())]),
            crate::sandbox::Sandbox::new(false, "bwrap"),
        );
        let write = WriteTool::new_with_tracker(None, None, None, ReadTracker::new(true))
            .with_workspace(temp.path())
            .with_formatter(formatter);
        write
            .call(WriteArgs {
                path: "own.txt".into(),
                content: "one\n".into(),
                overwrite: false,
            })
            .await
            .unwrap();
        let error = write
            .call(wr("own.txt", "two\n"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("complete current read"), "{error}");
    }

    // mini-agent-nvnkx.193
    #[tokio::test]
    async fn overwrite_of_a_hard_linked_file_says_the_other_names_keep_old_content() {
        let temp = TempDir::new("write-hardlink");
        let target = temp.path().join("a.txt");
        std::fs::write(&target, "old\n").unwrap();
        std::fs::hard_link(&target, temp.path().join("b.txt")).unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(temp.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());
        read.call(rd("a.txt")).await.unwrap();
        let output = write.call(wr("a.txt", "new\n")).await.unwrap();
        assert!(output.contains("2 hard links"), "{output}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
            "old\n"
        );
    }

    // mini-agent-nvnkx.23
    #[tokio::test]
    async fn overwrite_keeps_a_utf8_bom_and_never_adds_one() {
        let temp = TempDir::new("write-bom");
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(temp.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());
        let with_bom = temp.path().join("bom.cs");
        std::fs::write(&with_bom, b"\xEF\xBB\xBFclass A {}\n").unwrap();
        let shown = read.call(rd("bom.cs")).await.unwrap();
        assert!(!shown.contains('\u{feff}'), "{shown}");
        assert!(shown.contains("UTF-8 BOM"), "{shown}");
        write.call(wr("bom.cs", "class B {}\n")).await.unwrap();
        assert_eq!(
            std::fs::read(&with_bom).unwrap(),
            b"\xEF\xBB\xBFclass B {}\n"
        );

        let plain = temp.path().join("plain.cs");
        std::fs::write(&plain, "class A {}\n").unwrap();
        let shown = read.call(rd("plain.cs")).await.unwrap();
        assert!(!shown.contains("UTF-8 BOM"), "{shown}");
        write.call(wr("plain.cs", "class B {}\n")).await.unwrap();
        assert_eq!(std::fs::read(&plain).unwrap(), b"class B {}\n");
    }

    #[tokio::test]
    async fn overwrite_of_a_file_beyond_one_read_result_explains_the_ceiling() {
        let temp = TempDir::new("write-big-overwrite");
        let target = temp.path().join("big.txt");
        let body: String = (0..6_000)
            .map(|i| format!("line {i:05} {}\n", "x".repeat(40)))
            .collect();
        std::fs::write(&target, &body).unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 2000, tracker.clone())
            .with_workspace(temp.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());
        for start in [1usize, 1501, 3001, 4501] {
            read.call(ReadArgs {
                pages: None,
                raw_notebook: false,
                character_offset: None,
                character_limit: None,
                path: "big.txt".into(),
                offset: Some(start),
                limit: Some(1500),
            })
            .await
            .unwrap();
        }
        let error = write
            .call(WriteArgs {
                path: "big.txt".into(),
                content: "new\n".into(),
                overwrite: true,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("larger than a single read result"),
            "{error}"
        );
        assert!(error.contains("edit tool"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), body);
    }

    // mini-agent-sskdu.89: many short lines fit one result; an explicit read
    // limit authorizes the overwrite and the refusal says so.
    #[tokio::test]
    async fn overwrite_of_a_long_but_small_file_needs_only_an_explicit_read_limit() {
        let temp = TempDir::new("write-long-overwrite");
        let target = temp.path().join("long.txt");
        std::fs::write(&target, "a\n".repeat(2_500)).unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 2000, tracker.clone())
            .with_workspace(temp.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());
        let read_args = |limit: Option<usize>| ReadArgs {
            pages: None,
            raw_notebook: false,
            character_offset: None,
            character_limit: None,
            path: "long.txt".into(),
            offset: None,
            limit,
        };
        let write_args = || WriteArgs {
            path: "long.txt".into(),
            content: "new\n".into(),
            overwrite: true,
        };
        read.call(read_args(None)).await.unwrap();
        let error = write.call(write_args()).await.unwrap_err().to_string();
        assert!(error.contains("limit >= 2500"), "{error}");
        assert!(
            !error.contains("larger than a single read result"),
            "{error}"
        );
        read.call(read_args(Some(2_500))).await.unwrap();
        write.call(write_args()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn overwrite_rejects_an_intervening_same_length_change() {
        let temp = TempDir::new("write-permission");
        let target = temp.path().join("existing.txt");
        std::fs::write(&target, "before\n").unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(temp.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());

        read.call(ReadArgs {
            pages: None,
            raw_notebook: false,
            character_offset: None,
            character_limit: None,
            path: "existing.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
        let original_times = std::fs::metadata(&target).unwrap();
        std::fs::write(&target, "after!\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_accessed(original_times.accessed().unwrap())
                    .set_modified(original_times.modified().unwrap()),
            )
            .unwrap();

        let error = write
            .call(WriteArgs {
                path: "existing.txt".into(),
                content: "unsafe\n".into(),
                overwrite: true,
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("complete current read"), "{error}");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "after!\n");
    }

    #[tokio::test]
    async fn plan_write_external_lookalike_is_denied_without_mutation() {
        let temp = TempDir::new("write-permission");
        let workspace = temp.path().join("workspace");
        let external = temp.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let target = external.join("PLAN-private.md");
        let tool = plan_write_tool(&workspace);

        let error = tool
            .call(WriteArgs {
                path: target.to_string_lossy().into_owned(),
                content: "must not be written".to_string(),
                overwrite: false,
            })
            .await
            .expect_err("basename alone must not grant PlanWrite authority");

        assert!(error.to_string().contains("Permission denied"));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn plan_write_relative_symlink_parent_cannot_escape_workspace() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("write-permission");
        let workspace = temp.path().join("workspace");
        let external = temp.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let sentinel = external.join("sentinel.txt");
        std::fs::write(&sentinel, "unchanged").unwrap();
        symlink(&external, workspace.join("plans")).unwrap();
        let escaped = external.join("PLAN.md");
        let tool = plan_write_tool(&workspace);

        tool.call(WriteArgs {
            path: "plans/PLAN.md".to_string(),
            content: "must not escape".to_string(),
            overwrite: false,
        })
        .await
        .expect_err("workspace capability must reject a symlinked parent");

        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
        assert!(!escaped.exists());
    }

    #[tokio::test]
    async fn checks_permission_on_broken_symlink_target_before_write() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("write-permission");
        let allowed_dir = temp.path().join("allowed");
        let restricted_dir = temp.path().join("restricted");
        std::fs::create_dir_all(&allowed_dir).unwrap();
        std::fs::create_dir_all(&restricted_dir).unwrap();

        let restricted_target = restricted_dir.join("created-through-link.txt");
        let allowed_link = allowed_dir.join("safe-link.txt");
        symlink(&restricted_target, &allowed_link).unwrap();

        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(allowed_dir),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration");
        let tool = WriteTool::new(Some(Arc::new(Mutex::new(checker))), None, None);

        let error = tool
            .call(WriteArgs {
                path: allowed_link.to_string_lossy().into_owned(),
                content: "must not be written".to_string(),
                overwrite: false,
            })
            .await
            .expect_err("the resolved external target must require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected error: {error}"
        );
        assert!(
            !restricted_target.exists(),
            "permission denial must happen before the symlink target is written"
        );
    }

    #[tokio::test]
    async fn checks_permission_on_symlinked_parent_before_write() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("write-permission");
        let allowed_dir = temp.path().join("allowed");
        let restricted_dir = temp.path().join("restricted");
        std::fs::create_dir_all(&allowed_dir).unwrap();
        std::fs::create_dir_all(&restricted_dir).unwrap();

        let allowed_link = allowed_dir.join("linked-directory");
        symlink(&restricted_dir, &allowed_link).unwrap();
        let restricted_target = restricted_dir.join("created-through-parent-link.txt");

        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(allowed_dir),
            Some(vec!["standard".to_string()]),
        )
        .expect("valid permission test configuration");
        let tool = WriteTool::new(Some(Arc::new(Mutex::new(checker))), None, None);

        let error = tool
            .call(WriteArgs {
                path: allowed_link
                    .join("created-through-parent-link.txt")
                    .to_string_lossy()
                    .into_owned(),
                content: "must not be written".to_string(),
                overwrite: false,
            })
            .await
            .expect_err("the resolved external parent must require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected error: {error}"
        );
        assert!(
            !restricted_target.exists(),
            "permission denial must happen before the external target is written"
        );
    }

    #[tokio::test]
    async fn symlink_swap_after_permission_check_is_rejected() {
        use std::os::unix::fs::symlink;

        use crate::permission::ask::UserDecision;

        let temp = TempDir::new("write-permission");
        let checked_target = temp.path().join("checked.txt");
        let swapped_target = temp.path().join("swapped.txt");
        let link = temp.path().join("input.txt");
        symlink(&checked_target, &link).unwrap();

        let checker = PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Guarded,
            Some(temp.path().to_path_buf()),
            Some(vec!["guarded".to_string()]),
        )
        .expect("valid permission test configuration");
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = WriteTool::new(Some(Arc::new(Mutex::new(checker))), Some(ask_tx), None);

        let call = tool.call(WriteArgs {
            path: link.to_string_lossy().into_owned(),
            content: "checked contents".to_string(),
            overwrite: false,
        });
        let swap = async {
            let request = ask_rx
                .recv()
                .await
                .expect("permission request")
                .into_permission();
            let expected = std::fs::canonicalize(checked_target.parent().unwrap())
                .unwrap()
                .join(checked_target.file_name().unwrap());
            assert_eq!(PathBuf::from(&request.input), expected);
            symlink(&swapped_target, &checked_target).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, swap);
        let error = result.expect_err("write must reject a swapped permission-checked target");
        assert!(error.to_string().contains("Path changed"));
        assert!(!swapped_target.exists());
    }
}
