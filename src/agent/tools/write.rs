use std::io::Read;
use std::path::{Path, PathBuf};

use rig::tool::Tool;
use tokio::io::AsyncReadExt;

use crate::agent::tools::crc::Crc32;
use crate::agent::tools::{
    AskSender, PermCheck, ReadTracker, ToolError, WriteArgs, check_perm_bound_path, check_perm_path,
};
#[cfg(feature = "lsp")]
use crate::extras::lsp::LspManager;

const DEFAULT_MAX_TEXT_SIZE: u64 = 1024 * 1024;

fn full_text_fingerprint(bytes: &[u8]) -> Result<(u32, usize), ToolError> {
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
        "Cannot overwrite '{path}': overwrite=true requires a complete current read of the file. Read it from offset 1 through EOF, then retry without any intervening change."
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

pub struct WriteTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_text_file_size: u64,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    read_tracker: ReadTracker,
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
        let workspace_root =
            crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let requested =
            crate::agent::tools::resolve_tool_path(workspace_root.as_deref(), &args.path);
        let expanded = requested.to_string_lossy().into_owned();
        tracing::debug!(
            "tool write start: path={}, content_len={}",
            expanded,
            args.content.len(),
        );
        let bytes = args.content.len();
        if bytes as u64 > self.max_text_file_size {
            tracing::warn!(
                "tool write file too large: path={}, size={}, max={}",
                expanded,
                bytes,
                self.max_text_file_size,
            );
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes). Maximum allowed file size is {} bytes.",
                bytes, self.max_text_file_size
            )));
        }
        let relative = Path::new(&args.path);
        if !relative.is_absolute()
            && !args.path.starts_with('~')
            && let Some(workspace) = &self.workspace
        {
            let coaching =
                check_perm_bound_path(&self.permission, &self.ask_tx, "write", workspace, relative)
                    .await?;
            match workspace.open_relative(relative) {
                Ok(mut existing) => {
                    if !args.overwrite {
                        return Err(create_error(
                            &expanded,
                            std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                        ));
                    }
                    let metadata = existing.metadata()?;
                    if metadata.len() > self.max_text_file_size {
                        return Err(overwrite_not_authorized(&expanded));
                    }
                    let expected = crate::fs::checked_file_metadata(&existing)?;
                    let mut current = Vec::with_capacity(metadata.len() as usize);
                    existing.read_to_end(&mut current)?;
                    let (crc, lines) = full_text_fingerprint(&current)?;
                    if !self
                        .read_tracker
                        .permits_full_overwrite(&expanded, &metadata, crc, lines)
                    {
                        return Err(overwrite_not_authorized(&expanded));
                    }
                    workspace.replace_relative_atomic_expecting(
                        relative,
                        args.content.as_bytes(),
                        &expected,
                        &crate::fs::ContentDigest::of(&current),
                    )?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => workspace
                    .create_relative_atomic(relative, args.content.as_bytes())
                    .map_err(|error| create_error(&expanded, error))?,
                Err(error) => return Err(error.into()),
            }
            self.read_tracker.untrack_read_path(&expanded);
            let mut result = format!("Written {} bytes to {}", bytes, expanded);
            if let Some(msg) = coaching {
                result = format!("{}\n\n{}", msg, result);
            }
            #[cfg(feature = "lsp")]
            if let Some(lsp) = &self.lsp {
                let baseline = lsp.notify_changed_relative(relative).await;
                if let Some(block) = lsp
                    .diagnostics_block_for_relative_edit(relative, baseline)
                    .await
                {
                    result.push_str(&block);
                }
            }
            return Ok(result);
        }

        let resolved = resolve_write_path(&requested).await?;
        let path = resolved.as_path();
        // Check the path atomic_write will modify, not a symlink that points to it.
        let coaching = check_perm_path(
            &self.permission,
            &self.ask_tx,
            "write",
            &path.to_string_lossy(),
        )
        .await?;

        let existing = if path.exists() {
            tracing::warn!("tool write file exists: path={}", expanded);
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
            let mut current = Vec::with_capacity(metadata.len() as usize);
            file.read_to_end(&mut current).await?;
            let (crc, lines) = full_text_fingerprint(&current)?;
            if !self.read_tracker.permits_full_overwrite(
                &path.to_string_lossy(),
                &metadata,
                crc,
                lines,
            ) {
                return Err(overwrite_not_authorized(&expanded));
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
        if let Some(base) = existing {
            crate::fs::atomic_write_resolved_expecting(path, &args.content, approved_parent, base)
                .await?;
        } else {
            crate::fs::atomic_create_resolved_checked(path, &args.content, approved_parent)
                .await
                .map_err(|error| create_error(&expanded, error))?;
        }
        self.read_tracker.untrack_read_path(&path.to_string_lossy());
        tracing::debug!("tool write done: path={}, bytes={}", expanded, bytes);
        let mut result = format!("Written {} bytes to {}", bytes, expanded);
        if let Some(msg) = coaching {
            result = format!("{}\n\n{}", msg, result);
        }

        #[cfg(feature = "lsp")]
        if let Some(lsp) = &self.lsp {
            let baseline = lsp.notify_changed(path).await;
            if let Some(block) = lsp.diagnostics_block_for_edit(path, baseline).await {
                result.push_str(&block);
            }
        }

        Ok(result)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::agent::tools::{ReadArgs, ReadTool};
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{PermissionConfig, PermissionConfigs, SecurityMode};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "zerostack_write_permission_test_{}_{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
    async fn workspace_write_reports_existing_files_as_an_actionable_tool_error() {
        let temp = TempDir::new();
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
        let temp = TempDir::new();
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

    #[tokio::test]
    async fn overwrite_rejects_an_intervening_same_length_change() {
        let temp = TempDir::new();
        let target = temp.path().join("existing.txt");
        std::fs::write(&target, "before\n").unwrap();
        let tracker = ReadTracker::new(true);
        let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(temp.path());
        let write =
            WriteTool::new_with_tracker(None, None, None, tracker).with_workspace(temp.path());

        read.call(ReadArgs {
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
        let temp = TempDir::new();
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

        let temp = TempDir::new();
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

        let temp = TempDir::new();
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

        let temp = TempDir::new();
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

        let temp = TempDir::new();
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
            let request = ask_rx.recv().await.expect("permission request");
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
