use std::path::{Path, PathBuf};

use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, PermCheck, ToolError, WriteArgs, check_perm_bound_path, check_perm_path,
};
#[cfg(feature = "lsp")]
use crate::extras::lsp::LspManager;

const DEFAULT_MAX_TEXT_SIZE: u64 = 1024 * 1024;

pub struct WriteTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_text_file_size: u64,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
    /// When `Some`, written files are synced to their language server and
    /// fresh diagnostics are appended to the tool result.
    #[cfg(feature = "lsp")]
    pub lsp: Option<LspManager>,
}

impl WriteTool {
    pub fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        max_text_file_size: Option<u64>,
    ) -> Self {
        WriteTool {
            permission,
            ask_tx,
            max_text_file_size: max_text_file_size.unwrap_or(DEFAULT_MAX_TEXT_SIZE),
            workspace: None,
            #[cfg(feature = "lsp")]
            lsp: None,
        }
    }

    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace = Some(crate::agent::tools::capture_workspace_binding(root));
        self
    }

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
        "Create a new file with the given content. Fails if the file already exists — use edit for existing files. Automatically creates parent directories.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
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
            workspace.create_relative_atomic(relative, args.content.as_bytes())?;
            crate::agent::tools::untrack_read_path(&expanded);
            let mut result = format!("Written {} bytes to {}", bytes, expanded);
            if let Some(msg) = coaching {
                result = format!("{}\n\n{}", msg, result);
            }
            #[cfg(feature = "lsp")]
            if let Some(lsp) = &self.lsp {
                lsp.notify_changed_relative(relative).await;
                if let Some(block) = lsp.diagnostics_block_for_relative_edit(relative).await {
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

        if path.exists() {
            tracing::warn!("tool write file exists: path={}", expanded);
            return Err(ToolError::Msg(format!(
                "File '{}' already exists. Use edit for targeted changes, or delete and recreate if a full rewrite is needed.",
                expanded
            )));
        }
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
        crate::fs::atomic_create_resolved_checked(path, &args.content, approved_parent).await?;
        crate::agent::tools::untrack_read_path(&expanded);
        tracing::debug!("tool write done: path={}, bytes={}", expanded, bytes);
        let mut result = format!("Written {} bytes to {}", bytes, expanded);
        if let Some(msg) = coaching {
            result = format!("{}\n\n{}", msg, result);
        }

        #[cfg(feature = "lsp")]
        if let Some(lsp) = &self.lsp {
            lsp.notify_changed(path).await;
            if let Some(block) = lsp.diagnostics_block_for_edit(path).await {
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
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{PermissionConfigs, SecurityMode};

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
        );
        let tool = WriteTool::new(Some(Arc::new(Mutex::new(checker))), None, None);

        let error = tool
            .call(WriteArgs {
                path: allowed_link.to_string_lossy().into_owned(),
                content: "must not be written".to_string(),
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
        );
        let tool = WriteTool::new(Some(Arc::new(Mutex::new(checker))), None, None);

        let error = tool
            .call(WriteArgs {
                path: allowed_link
                    .join("created-through-parent-link.txt")
                    .to_string_lossy()
                    .into_owned(),
                content: "must not be written".to_string(),
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
        );
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = WriteTool::new(Some(Arc::new(Mutex::new(checker))), Some(ask_tx), None);

        let call = tool.call(WriteArgs {
            path: link.to_string_lossy().into_owned(),
            content: "checked contents".to_string(),
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
