//! Opt-in read-only shell for read-mode subagents.
//!
//! With `[subagents] allow_read_only_shell = true`, a read-mode persona whose
//! `tools` names `shell` gets the parent's shell rebound to a fresh private
//! scratch directory: that directory is the command's only writable
//! workspace, and the parent workspace is added as a read root. The shell is
//! installed only when the rebound sandbox provably cannot write the parent
//! workspace (an uncontained sandbox, the host-full profile, or an extra write
//! root overlapping the workspace refuses it). The scratch directory is
//! removed when the child ends. Permission rules and approvals for `shell`
//! are the parent's, exactly as for a write child.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::builder::{ReadOnlyShell, WriteChildTools};

/// The child's private scratch directory; removed on drop.
pub(crate) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create the scratch directory and the read-only shell bound to it.
pub(crate) fn prepare(
    parent: &WriteChildTools,
    parent_root: &Path,
) -> Result<(Scratch, ReadOnlyShell), String> {
    if !parent.shell_enabled {
        return Err("the parent has no eligible shell tool".into());
    }
    // Canonical: private-directory creation refuses symlinked components
    // such as macOS's `/var -> /private/var`.
    let base = std::env::temp_dir()
        .canonicalize()
        .map_err(|error| format!("cannot resolve the temporary directory: {error}"))?;
    let path = base.join(format!("mini-agent-subagent-{}", uuid::Uuid::new_v4()));
    crate::paths::ensure_private_directory(&path)
        .map_err(|error| format!("cannot create the scratch directory: {error}"))?;
    let scratch = Scratch { path };
    let binding = Arc::new(
        crate::paths::WorkspaceBinding::capture(scratch.path())
            .map_err(|error| format!("cannot bind the scratch directory: {error}"))?,
    );
    let sandbox = parent
        .sandbox
        .clone()
        .with_extra_read_root(parent_root.to_path_buf())
        .rebind_workspace_binding(binding.clone())?;
    if sandbox.may_write_under(binding.root(), parent_root) {
        return Err(
            "the parent's sandbox could let a command write the workspace (sandbox off, host-full profile, or an overlapping write root)"
                .into(),
        );
    }
    Ok((
        scratch,
        ReadOnlyShell {
            sandbox,
            max_output_lines: parent.max_shell_output_lines,
            timeout_secs: parent.shell_timeout_secs,
        },
    ))
}

/// Host note prepended to a read-only-shell child's prompt.
pub(crate) fn prompt_note(scratch: &Path, workspace: &Path) -> String {
    format!(
        "[host note: your shell is read-only. Commands start in the scratch directory {} (the only writable location, deleted when you finish). The workspace {} is readable but not writable: use absolute paths or cd into it, and point build outputs at the scratch directory (for example CARGO_TARGET_DIR).]",
        scratch.display(),
        workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::agents::AgentTool;
    use crate::extras::subagents::builder::SubagentAuthorization;

    fn parent_tools(sandbox: crate::sandbox::Sandbox, root: &Path) -> WriteChildTools {
        let binding = Arc::new(crate::paths::WorkspaceBinding::capture(root).unwrap());
        WriteChildTools {
            sandbox: sandbox.with_workspace_binding(binding),
            write_enabled: true,
            edit_enabled: true,
            shell_enabled: true,
            max_text_file_size: Some(1024 * 1024),
            max_shell_output_lines: Some(100),
            shell_timeout_secs: 10,
            diff_options: Default::default(),
            formatter: None,
            #[cfg(feature = "lsp")]
            lsp: None,
        }
    }

    fn read_child(root: &Path) -> SubagentAuthorization {
        SubagentAuthorization::new(None, None, false).with_workspace_binding(Some(Arc::new(
            crate::paths::WorkspaceBinding::capture(root).unwrap(),
        )))
    }

    #[test]
    fn read_children_get_no_shell_unless_planned_and_named() {
        let root = crate::tests::OwnedTestDirectory::new("ro-shell-default");
        let named = [AgentTool::Read, AgentTool::Shell];
        // Default: no opt-in, so even a persona that names shell has none.
        assert!(
            !read_child(root.path())
                .filesystem_tool_kinds(Some(&named))
                .contains(&AgentTool::Shell)
        );
        let planned = read_child(root.path()).with_read_only_shell_planned();
        assert!(
            planned
                .filesystem_tool_kinds(Some(&named))
                .contains(&AgentTool::Shell)
        );
        assert!(
            !planned
                .filesystem_tool_kinds(None)
                .contains(&AgentTool::Shell),
            "the default read tool set never includes the shell"
        );
        // Planned is a preflight state only: it never instantiates a tool.
        let tools = planned.filesystem_tools(1024, 10, 10, 10, Some(10), Some(&named));
        assert!(tools.iter().all(|tool| tool.name() != "shell"));
    }

    #[test]
    fn an_uncontained_parent_sandbox_gets_no_read_only_shell() {
        let root = crate::tests::OwnedTestDirectory::new("ro-shell-off");
        let parent = parent_tools(crate::sandbox::Sandbox::new(false, "auto"), root.path());
        let error = prepare(&parent, root.path()).err().unwrap();
        assert!(error.contains("could let a command write"), "{error}");
        let mut disabled = parent.clone();
        disabled.shell_enabled = false;
        assert!(prepare(&disabled, root.path()).is_err());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn read_only_shell_reads_the_workspace_and_writes_only_its_scratch_directory() {
        let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
        let root = crate::tests::OwnedTestDirectory::new("ro-shell-real");
        let workspace = root.path().canonicalize().unwrap();
        std::fs::write(workspace.join("visible.txt"), "WORKSPACE_CONTENT").unwrap();
        #[cfg(target_os = "macos")]
        let backend = "seatbelt";
        #[cfg(target_os = "linux")]
        let backend = "bwrap";
        let parent = parent_tools(crate::sandbox::Sandbox::new(true, backend), &workspace);
        let (scratch, shell) = match prepare(&parent, &workspace) {
            Ok(prepared) => prepared,
            Err(error) => {
                eprintln!("skipping: the native sandbox is unusable on this host: {error}");
                return;
            }
        };
        let scratch_path = scratch.path().to_path_buf();
        let authorization = read_child(&workspace).with_read_only_shell(Some(shell));
        let tools = authorization.filesystem_tools(
            1024,
            10,
            10,
            10,
            Some(10),
            Some(&[AgentTool::Read, AgentTool::Shell]),
        );
        let shell = tools.iter().find(|tool| tool.name() == "shell").unwrap();
        let pwned = workspace.join("pwned.txt");
        let script = format!(
            "cat '{}'; echo scratch > inside.txt; touch '{}' 2>/dev/null; echo done",
            workspace.join("visible.txt").display(),
            pwned.display()
        );
        let result = shell
            .call(serde_json::json!({ "command": script }).to_string())
            .await;
        let inside = scratch_path.join("inside.txt");
        match result {
            Ok(output) if inside.exists() => {
                assert!(output.contains("WORKSPACE_CONTENT"), "{output}");
                assert!(!pwned.exists(), "the read-only shell wrote the workspace");
            }
            other => eprintln!("skipping: the native sandbox is unusable on this host: {other:?}"),
        }
        drop(tools);
        drop(scratch);
        assert!(!scratch_path.exists(), "the scratch directory is removed");
    }
}
