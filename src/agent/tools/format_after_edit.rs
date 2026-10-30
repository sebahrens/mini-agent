//! Optional post-commit formatting is an ordinary authorized shell effect.
//! It retains the original workspace/config lookup and never rolls back an edit.

use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{AskSender, PermCheck, check_perm};
use crate::fs::{CheckedMetadata, ContentDigest};
use crate::paths::WorkspaceBinding;
use crate::sandbox::{CommandLimits, CommandStatus, Sandbox, ShellDialect};

const MAX_COMMAND_BYTES: usize = 8192;
const MAX_ARGS: usize = 64;
const MAX_OBSERVATION_BYTES: u64 = 8 * 1024 * 1024;
const LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(10),
    stdout_bytes: 1024 * 1024,
    stderr_bytes: 1024 * 1024,
    combined_bytes: 1024 * 1024,
};

pub(crate) struct FormatAfterEdit {
    commands: BTreeMap<String, String>,
    invalid_map: bool,
    sandbox: Sandbox,
}

pub(crate) struct FormatResult {
    pub(crate) status: &'static str,
    /// A fresh, bounded, authorized observation, never an overwrite proof.
    pub(crate) content: Option<String>,
    #[cfg(feature = "lsp")]
    pub(crate) notify_lsp: bool,
}

impl FormatAfterEdit {
    pub(crate) fn new(commands: &BTreeMap<String, String>, sandbox: Sandbox) -> Option<Arc<Self>> {
        (!commands.is_empty()).then(|| {
            let invalid_map = commands.len() > 64
                || commands
                    .values()
                    .any(|template| template.len() > MAX_COMMAND_BYTES)
                || commands.keys().any(|extension| {
                    extension.is_empty()
                        || extension.len() > 16
                        || !extension
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                });
            Arc::new(Self {
                commands: if invalid_map {
                    BTreeMap::new()
                } else {
                    commands.clone()
                },
                invalid_map,
                sandbox,
            })
        })
    }

    pub(crate) async fn run(
        &self,
        path: &Path,
        bound: Option<(&Arc<WorkspaceBinding>, &Path)>,
        committed: &str,
        permission: &Option<PermCheck>,
        ask: &Option<AskSender>,
        limit: u64,
    ) -> Option<FormatResult> {
        let unavailable = |status| FormatResult {
            status,
            content: None,
            #[cfg(feature = "lsp")]
            notify_lsp: false,
        };
        if self.invalid_map {
            return Some(unavailable(
                "Formatter skipped: invalid configuration; edit remains committed.",
            ));
        }
        let extension = path.extension()?.to_str()?;
        let template = self.commands.get(extension)?;
        let Some(operand) = path.to_str() else {
            return Some(unavailable(
                "Formatter skipped: path is not UTF-8; edit remains committed.",
            ));
        };
        let Some(shell) = self.sandbox.shell_capability() else {
            return Some(unavailable(
                "Formatter skipped: shell unavailable; edit remains committed.",
            ));
        };
        let Some(command) = command(template, operand, shell.dialect()) else {
            return Some(unavailable(
                "Formatter skipped: invalid command template; edit remains committed.",
            ));
        };
        if blocked() {
            return Some(unavailable(
                "Formatter skipped: work stopped; edit remains committed.",
            ));
        }
        let owned_path = path.to_path_buf();
        let owned_bound =
            bound.map(|(workspace, relative)| (Arc::clone(workspace), relative.to_path_buf()));
        let target = match crate::agent::runner::spawn_blocking_scoped(move || {
            Target::capture(
                &owned_path,
                owned_bound
                    .as_ref()
                    .map(|(workspace, relative)| (workspace, relative.as_path())),
            )
        })
        .await
        {
            Ok(Ok(target)) => Arc::new(target),
            _ => {
                return Some(unavailable(
                    "Formatter skipped: target unavailable; edit remains committed.",
                ));
            }
        };
        let expected = ContentDigest::of(committed.as_bytes());
        let initial = match snapshot(Arc::clone(&target), limit).await {
            Some(snapshot) if snapshot_matches(&snapshot, committed.len(), &expected) => snapshot,
            _ => {
                return Some(unavailable(
                    "Formatter skipped: target changed; edit remains committed.",
                ));
            }
        };
        let status = if blocked() {
            "Formatter skipped: work stopped; edit remains committed."
        } else if check_perm(permission, ask, "shell", &command)
            .await
            .is_err()
        {
            "Formatter not authorized; edit remains committed."
        } else if crate::spend::run::admit_current_effect().await.is_err() || blocked() {
            "Formatter skipped: work stopped; edit remains committed."
        } else if !unchanged_since_capture(
            &target,
            path,
            &initial,
            committed.len(),
            &expected,
            limit,
        )
        .await
        {
            "Formatter skipped: target changed after approval; edit remains committed."
        } else if blocked() {
            "Formatter skipped: work stopped; edit remains committed."
        } else if permission.as_ref().is_some_and(|permission| {
            !permission
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retains_shell_approval(&command)
        }) {
            "Formatter approval was revoked; edit remains committed."
        } else {
            // The current tool's captured scope owns this existing process worker
            // through actual family retirement, including a dropped receiver.
            match self
                .sandbox
                .output_command_with_limits(&command, LIMITS)
                .await
            {
                Ok(output) if output.descendants_escaped => {
                    return Some(unavailable(
                        "Formatter family retirement could not be proved; edit remains committed.",
                    ));
                }
                Ok(output)
                    if output.status == CommandStatus::Completed
                        && output.exit_status.is_some_and(|status| status.success())
                        && !output.descendants_escaped =>
                {
                    "Formatted."
                }
                Ok(_) => "Formatter failed or stopped; edit remains committed.",
                Err(_) => "Formatter could not start; edit remains committed.",
            }
        };
        // Never return formatter stdout/stderr: even an error can quote private
        // source. Permission is checked afresh, without Ask or read tracking.
        let allowed = super::mutation_diff::preimage_allowed(permission, operand, bound.is_some());
        let current = if allowed && !blocked() {
            snapshot(Arc::clone(&target), limit).await
        } else {
            None
        };
        let content = current.and_then(|current| {
            super::mutation_diff::preimage_allowed(permission, operand, bound.is_some())
                .then_some(current.text)
        });
        #[cfg(feature = "lsp")]
        let notify_lsp = content.is_some();
        let status = if allowed && content.is_none() && !blocked() {
            if status == "Formatted." {
                "Formatter completed; final file text unavailable."
            } else {
                "Formatter failed or was skipped; final file text unavailable; edit remains committed."
            }
        } else {
            status
        };
        Some(FormatResult {
            status,
            content,
            #[cfg(feature = "lsp")]
            notify_lsp,
        })
    }
}

fn blocked() -> bool {
    crate::agent::runner::current_work_scope_is_cancelled() || crate::question::required().is_some()
}

struct Target {
    workspace: Arc<WorkspaceBinding>,
    relative: PathBuf,
    path: PathBuf,
    parent: CheckedMetadata,
}

impl Target {
    fn capture(
        path: &Path,
        bound: Option<(&Arc<WorkspaceBinding>, &Path)>,
    ) -> std::io::Result<Self> {
        let (workspace, relative) = match bound {
            Some((workspace, relative)) => (Arc::clone(workspace), relative.to_path_buf()),
            None => {
                let parent = path
                    .parent()
                    .ok_or_else(|| std::io::Error::other("missing parent"))?;
                let name = path
                    .file_name()
                    .ok_or_else(|| std::io::Error::other("missing name"))?;
                (
                    Arc::new(WorkspaceBinding::capture(parent)?),
                    PathBuf::from(name),
                )
            }
        };
        let parent_file =
            workspace.open_relative_directory_file(relative.parent().unwrap_or(Path::new("")))?;
        let parent = crate::fs::checked_file_metadata(&parent_file)?;
        Ok(Self {
            workspace,
            relative,
            path: path.to_path_buf(),
            parent,
        })
    }

    /// Metadata of the target as it is now, without reading its bytes.
    fn probe(&self) -> std::io::Result<CheckedMetadata> {
        self.workspace.validate().map_err(std::io::Error::other)?;
        let parent = self
            .workspace
            .open_relative_directory_file(self.relative.parent().unwrap_or(Path::new("")))?;
        crate::fs::ensure_same_file(
            &self.path,
            &self.parent,
            &crate::fs::checked_file_metadata(&parent)?,
        )?;
        let file = self.workspace.open_regular_relative(&self.relative)?;
        crate::fs::checked_file_metadata(&file)
    }

    fn read(&self, limit: u64) -> std::io::Result<Snapshot> {
        #[cfg(test)]
        {
            *test_reads()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .entry(self.path.clone())
                .or_insert(0) += 1;
        }
        let limit = limit.min(MAX_OBSERVATION_BYTES);
        self.workspace.validate().map_err(std::io::Error::other)?;
        let parent = self
            .workspace
            .open_relative_directory_file(self.relative.parent().unwrap_or(Path::new("")))?;
        crate::fs::ensure_same_file(
            &self.path,
            &self.parent,
            &crate::fs::checked_file_metadata(&parent)?,
        )?;
        let mut file = self.workspace.open_regular_relative(&self.relative)?;
        let identity = crate::fs::checked_file_metadata(&file)?;
        if identity.len() > limit {
            return Err(std::io::Error::other("text limit"));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(std::io::Error::other("text limit"));
        }
        // An in-place writer can preserve inode identity. Check the bounded
        // digest again, then verify this name still denotes the same file.
        file.rewind()?;
        if !ContentDigest::of(&bytes).matches_reader(&mut file)? {
            return Err(std::io::Error::other("file changed during observation"));
        }
        let current = self.workspace.open_regular_relative(&self.relative)?;
        crate::fs::ensure_same_file(
            &self.path,
            &identity,
            &crate::fs::checked_file_metadata(&current)?,
        )?;
        let parent = self
            .workspace
            .open_relative_directory_file(self.relative.parent().unwrap_or(Path::new("")))?;
        crate::fs::ensure_same_file(
            &self.path,
            &self.parent,
            &crate::fs::checked_file_metadata(&parent)?,
        )?;
        self.workspace.validate().map_err(std::io::Error::other)?;
        let text = String::from_utf8(bytes).map_err(std::io::Error::other)?;
        Ok(Snapshot { identity, text })
    }
}

/// Per-path count of full target observations, for tests.
#[cfg(test)]
fn test_reads() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, usize>> {
    static READS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, usize>>> =
        std::sync::OnceLock::new();
    READS.get_or_init(Default::default)
}

struct Snapshot {
    identity: CheckedMetadata,
    text: String,
}

/// Cheap length check first, so a changed file skips the SHA-256 pass.
fn snapshot_matches(snapshot: &Snapshot, expected_len: usize, expected: &ContentDigest) -> bool {
    snapshot.text.len() == expected_len && ContentDigest::of(snapshot.text.as_bytes()) == *expected
}

/// Whether the file still is the one captured in `initial`. When the same
/// file (physical identity, length and modification time) is still there, the
/// bytes are taken as unchanged without another read and digest; any
/// difference falls back to a full observation.
async fn unchanged_since_capture(
    target: &Arc<Target>,
    path: &Path,
    initial: &Snapshot,
    expected_len: usize,
    expected: &ContentDigest,
    limit: u64,
) -> bool {
    if blocked() {
        return false;
    }
    let probe = {
        let target = Arc::clone(target);
        crate::agent::runner::spawn_blocking_scoped(move || target.probe())
            .await
            .ok()
            .and_then(Result::ok)
    };
    if let Some(current) = &probe
        && crate::fs::ensure_same_file(path, &initial.identity, current).is_ok()
        && current.len() == initial.identity.len()
        && let (Ok(before), Ok(after)) = (initial.identity.modified(), current.modified())
        && before == after
    {
        return true;
    }
    matches!(snapshot(Arc::clone(target), limit).await, Some(current)
        if crate::fs::ensure_same_file(path, &initial.identity, &current.identity).is_ok()
            && snapshot_matches(&current, expected_len, expected))
}

async fn snapshot(target: Arc<Target>, limit: u64) -> Option<Snapshot> {
    if blocked() {
        return None;
    }
    crate::agent::runner::spawn_blocking_scoped(move || target.read(limit))
        .await
        .ok()?
        .ok()
}

/// A literal argv grammar, not a shell interpreter. Tokenization happens
/// before substitution; quoted placeholders cannot change quoting authority.
fn command(template: &str, operand: &str, dialect: ShellDialect) -> Option<String> {
    if template.len() > MAX_COMMAND_BYTES
        || operand.len() > 4096
        || operand
            .chars()
            .any(|ch| ch == '\0' || ch == '\n' || ch == '\r')
    {
        return None;
    }
    let mut args = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch.is_control() && ch != '\t' {
            return None;
        }
        match (quote, ch) {
            (Some(q), ch) if ch == q => quote = None,
            (Some('\''), ch) => word.push(ch),
            (_, '\\') => {
                word.push(chars.next()?);
                started = true;
            }
            (Some(_), ch) => word.push(ch),
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (None, ' ' | '\t') => {
                if started {
                    args.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (
                None,
                ';' | '|' | '&' | '<' | '>' | '$' | '`' | '*' | '?' | '[' | ']' | '(' | ')' | '#',
            ) => return None,
            (None, ch) => {
                word.push(ch);
                started = true;
            }
        }
        if args.len() > MAX_ARGS {
            return None;
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(word);
    }
    if args.len() > MAX_ARGS {
        return None;
    }
    let executable = args.first()?;
    if executable.is_empty()
        || executable.contains("{file}")
        || executable.contains('=')
        || matches!(
            executable.as_str(),
            "." | "source"
                | "eval"
                | "if"
                | "for"
                | "while"
                | "until"
                | "case"
                | "function"
                | "select"
                | "then"
                | "else"
                | "fi"
                | "do"
                | "done"
        )
    {
        return None;
    }
    let mut placeholders = 0;
    for arg in &mut args {
        if arg.contains("{file}") {
            placeholders += arg.matches("{file}").count();
            if arg != "{file}" {
                let flag = arg.strip_suffix("={file}")?.strip_prefix("--")?;
                if flag.is_empty()
                    || !flag
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
                {
                    return None;
                }
            }
            *arg = arg.replace("{file}", operand);
        }
        if arg.chars().any(|ch| ch == '\0' || ch == '\n' || ch == '\r') {
            return None;
        }
    }
    if placeholders > 1 {
        return None;
    }
    let mut rendered = match dialect {
        ShellDialect::Posix => String::new(),
        ShellDialect::PowerShell => "& ".into(),
        // cmd.exe has no quoting that keeps arbitrary paths literal.
        ShellDialect::Cmd => return None,
    };
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            rendered.push(' ');
        }
        if dialect == ShellDialect::Posix
            && !arg.is_empty()
            && arg
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || b"_-. /:+=".contains(&ch))
            && !arg.contains(' ')
        {
            rendered.push_str(arg);
        } else {
            rendered.push('\'');
            rendered.push_str(&arg.replace(
                '\'',
                match dialect {
                    ShellDialect::Posix => "'\"'\"'",
                    ShellDialect::PowerShell | ShellDialect::Cmd => "''",
                },
            ));
            rendered.push('\'');
        }
    }
    (rendered.len() <= MAX_COMMAND_BYTES + 5 * 4096).then_some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format45_template_substitutes_literal_argv_after_quoted_tokenization() {
        let file = "/tmp/space ' $(touch marker) `id` *.rs";
        let quoted = "'/tmp/space '\"'\"' $(touch marker) `id` *.rs'";
        for template in ["rustfmt {file}", "rustfmt '{file}'", "rustfmt \"{file}\""] {
            assert_eq!(
                command(template, file, ShellDialect::Posix).unwrap(),
                format!("rustfmt {quoted}")
            );
        }
        assert_eq!(
            command("prettier --write --file={file}", file, ShellDialect::Posix).unwrap(),
            format!("prettier --write '--file=/tmp/space '\"'\"' $(touch marker) `id` *.rs'")
        );
        assert_eq!(
            command("cargo fmt", file, ShellDialect::Posix).unwrap(),
            "cargo fmt"
        );
        assert_eq!(
            command(
                "formatter 'literal $HOME' {file}",
                "a.rs",
                ShellDialect::Posix
            )
            .unwrap(),
            "formatter 'literal $HOME' a.rs"
        );
    }

    #[test]
    fn format45_template_rejects_ambiguous_syntax_before_permission() {
        for template in [
            "",
            "fmt \"{file}",
            "fmt '\\",
            "fmt {file} {file}",
            "{file}",
            "fmt prefix{file}",
            "fmt --={file}",
            "fmt --bad flag={file}",
            "fmt {file}; touch marker",
            "fmt {file} && next",
            "fmt > out",
            "fmt $(id)",
            "KEY=x fmt {file}",
            "eval {file}",
            "source {file}",
            "fmt\n{file}",
        ] {
            assert!(
                command(template, "a.rs", ShellDialect::Posix).is_none(),
                "{template:?}"
            );
        }
        assert!(
            command(
                &"x".repeat(MAX_COMMAND_BYTES + 1),
                "a.rs",
                ShellDialect::Posix
            )
            .is_none()
        );
        assert!(command("fmt {file}", "a\n.rs", ShellDialect::Posix).is_none());
        assert!(command("fmt {file}", &"x".repeat(4097), ShellDialect::Posix).is_none());
        let at_limit = std::iter::repeat_n("word", MAX_ARGS)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(command(&at_limit, "a.rs", ShellDialect::Posix).is_some());
        assert!(command(&format!("{at_limit} "), "a.rs", ShellDialect::Posix).is_some());
        assert!(command(&format!("{at_limit} word"), "a.rs", ShellDialect::Posix).is_none());
    }

    #[test]
    fn format45_template_powershell_uses_literal_call_arguments() {
        assert_eq!(
            command(
                "prettier --write \"{file}\"",
                "C:\\space ' $name\\x.js",
                ShellDialect::PowerShell
            )
            .unwrap(),
            "& 'prettier' '--write' 'C:\\space '' $name\\x.js'"
        );
    }

    #[cfg(unix)]
    fn fixture(
        exit: u8,
    ) -> (
        crate::tests::OwnedTestDirectory,
        Arc<WorkspaceBinding>,
        Arc<FormatAfterEdit>,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::tests::OwnedTestDirectory::new("format45-native");
        std::fs::write(
            root.path().join(".formatter-config"),
            "actually formatted\n",
        )
        .unwrap();
        let executable = root.path().join("formatter.sh");
        std::fs::write(&executable, format!("#!/bin/sh\nprintf invoked > formatter.marker\ncat .formatter-config > \"$1\"\nprintf 'PRIVATE_FORMATTER_OUTPUT\\n' >&2\nexit {exit}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let workspace = Arc::new(WorkspaceBinding::capture(root.path()).unwrap());
        let capability =
            crate::sandbox::ShellCapability::resolve("/bin/bash", workspace.root(), None).unwrap();
        let sandbox = Sandbox::new(false, "bwrap")
            .with_bound_resolved_shell(Some(capability), &workspace)
            .with_workspace_binding(Arc::clone(&workspace));
        let formatter = FormatAfterEdit::new(
            &BTreeMap::from([("rs".into(), "./formatter.sh \"{file}\"".into())]),
            sandbox,
        )
        .unwrap();
        (root, workspace, formatter)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_actual_write_failure_preserves_commit_and_reports_observed_final_diff() {
        use super::super::{WriteArgs, WriteTool};
        use rig::tool::{Tool, ToolCallExtensions};
        let (root, workspace, formatter) = fixture(3);
        let tool = WriteTool::new(None, None, None)
            .with_workspace_binding(workspace)
            .with_formatter(Some(formatter));
        let scope = crate::agent::runner::AgentWorkScope::new();
        let result = scope
            .run(tool.call_structured(
                WriteArgs {
                    path: "space ' $(touch injection).rs".into(),
                    content: "authored\n".into(),
                    overwrite: false,
                },
                &ToolCallExtensions::new(),
            ))
            .await
            .unwrap();
        scope.wait_idle().await;
        assert_eq!(scope.active_children(), 0);
        assert_eq!(
            std::fs::read_to_string(root.path().join("space ' $(touch injection).rs")).unwrap(),
            "actually formatted\n"
        );
        assert!(!root.path().join("injection").exists());
        assert!(result.output.contains("Written 9 bytes"));
        assert!(result.output.contains("Formatter failed"));
        assert!(!result.output.contains("PRIVATE_FORMATTER_OUTPUT"));
        let diff = result
            .extensions
            .get::<super::super::mutation_diff::MutationDiff>()
            .unwrap();
        assert_eq!(diff.after_bytes, "actually formatted\n".len());
        assert!(
            diff.rows
                .iter()
                .any(|row| row.text.contains("actually formatted"))
        );
        assert!(!diff.rows.iter().any(|row| row.text == "authored"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_formatted_write_observes_the_target_twice_not_three_times() {
        use super::super::{WriteArgs, WriteTool};
        use rig::tool::Tool;
        let (root, workspace, formatter) = fixture(0);
        let tool = WriteTool::new(None, None, None)
            .with_workspace_binding(workspace)
            .with_formatter(Some(formatter));
        let scope = crate::agent::runner::AgentWorkScope::new();
        let output = scope
            .run(tool.call(WriteArgs {
                path: "counted.rs".into(),
                content: "authored\n".into(),
                overwrite: false,
            }))
            .await
            .unwrap();
        scope.wait_idle().await;
        assert!(output.contains("Formatted."), "{output}");
        let reads = test_reads()
            .lock()
            .unwrap()
            .get(&root.path().join("counted.rs"))
            .copied();
        assert_eq!(reads, Some(2), "capture and final observation only");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_actual_edit_uses_final_excerpt_and_does_not_authorize_overwrite() {
        use super::super::{
            EditArgs, EditTool, ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool,
        };
        use rig::tool::{Tool, ToolCallExtensions};
        let (root, workspace, formatter) = fixture(0);
        std::fs::write(root.path().join("source.rs"), "old\n").unwrap();
        let tracker = ReadTracker::new(true);
        ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace_binding(Arc::clone(&workspace))
            .call(ReadArgs {
                raw_notebook: false,
                path: "source.rs".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let tool = EditTool::new_with_tracker(None, None, None, tracker.clone())
            .with_workspace_binding(Arc::clone(&workspace))
            .with_formatter(Some(formatter));
        let scope = crate::agent::runner::AgentWorkScope::new();
        let result = scope
            .run(tool.call_structured(
                EditArgs {
                    path: "source.rs".into(),
                    block: Some("<<<<<<< SEARCH\nold\n=======\nrequested\n>>>>>>> REPLACE".into()),
                    file_crc: None,
                    edits: None,
                    replace_all: false,
                    fuzzy: false,
                },
                &ToolCallExtensions::new(),
            ))
            .await
            .unwrap();
        scope.wait_idle().await;
        assert!(result.output.contains("actually formatted"));
        assert!(!result.output.contains("1| requested"));
        assert!(result.output.contains("Formatted."));
        assert_eq!(
            std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
            "actually formatted\n"
        );
        assert!(
            WriteTool::new_with_tracker(None, None, None, tracker)
                .with_workspace_binding(workspace)
                .call(WriteArgs {
                    path: "source.rs".into(),
                    content: "bad\n".into(),
                    overwrite: true
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("complete current read")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_actual_read_denied_source_and_formatter_output_stay_private() {
        use super::super::{EditArgs, EditTool};
        use crate::permission::{
            Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm,
        };
        use rig::tool::{Tool, ToolCallExtensions};
        let (root, workspace, formatter) = fixture(3);
        std::fs::write(root.path().join("source.rs"), "private neighbor\nold\n").unwrap();
        let permission = Arc::new(std::sync::Mutex::new(
            crate::permission::checker::PermissionChecker::new(
                &PermissionConfigs::from(PermissionConfig {
                    default: Some(Action::Allow),
                    read: Some(ToolPerm::Simple(Action::Deny)),
                    ..Default::default()
                }),
                SecurityMode::Standard,
                Some(root.path().to_path_buf()),
                None,
            )
            .unwrap(),
        ));
        let script = command(
            "./formatter.sh \"{file}\"",
            root.path().join("source.rs").to_str().unwrap(),
            ShellDialect::Posix,
        )
        .unwrap();
        permission
            .lock()
            .unwrap()
            .add_session_allowlist("shell".into(), &script);
        let tool = EditTool::new(Some(permission), None)
            .with_workspace_binding(workspace)
            .with_formatter(Some(formatter));
        let scope = crate::agent::runner::AgentWorkScope::new();
        let result = scope
            .run(tool.call_structured(
                EditArgs {
                    path: "source.rs".into(),
                    block: Some("<<<<<<< SEARCH\nold\n=======\nrequested\n>>>>>>> REPLACE".into()),
                    file_crc: None,
                    edits: None,
                    replace_all: false,
                    fuzzy: false,
                },
                &ToolCallExtensions::new(),
            ))
            .await
            .unwrap();
        scope.wait_idle().await;
        assert!(result.output.contains("requested"));
        for private in [
            "private neighbor",
            "actually formatted",
            "PRIVATE_FORMATTER_OUTPUT",
        ] {
            assert!(!result.output.contains(private), "{private}");
        }
        assert!(
            result
                .extensions
                .get::<super::super::mutation_diff::MutationDiff>()
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
            "actually formatted\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_actual_unknown_command_asks_once_and_revalidates_after_reply() {
        use super::super::{WriteArgs, WriteTool};
        use crate::permission::ask::UserDecision;
        use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode};
        use rig::tool::Tool;
        for case in [
            "allow",
            "deny",
            "replace",
            "inplace",
            "root",
            "readonly",
            "rule-deny",
            "cancel",
        ] {
            let (root, workspace, formatter) = fixture(0);
            let permission = Arc::new(std::sync::Mutex::new(
                crate::permission::checker::PermissionChecker::new(
                    &PermissionConfigs::from(PermissionConfig {
                        default: Some(Action::Allow),
                        ..Default::default()
                    }),
                    SecurityMode::Standard,
                    Some(root.path().to_path_buf()),
                    None,
                )
                .unwrap(),
            ));
            let (tx, mut rx) = tokio::sync::mpsc::channel(2);
            let tool = WriteTool::new(Some(Arc::clone(&permission)), Some(tx), None)
                .with_workspace_binding(workspace)
                .with_formatter(Some(formatter));
            let scope = crate::agent::runner::AgentWorkScope::new();
            let mut work = Box::pin(scope.run(tool.call(WriteArgs {
                path: "source.rs".into(),
                content: "committed\n".into(),
                overwrite: false,
            })));
            let request = tokio::select! {
                result = &mut work => panic!("formatter did not ask: {result:?}"),
                request = rx.recv() => request.unwrap().into_permission(),
                _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("formatter approval missing"),
            };
            assert_eq!(request.tool, "shell");
            assert_eq!(
                std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
                "committed\n",
                "edit precedes formatter approval"
            );
            assert!(!root.path().join("formatter.marker").exists());
            let moved = crate::tests::OwnedTestDirectory::new("format45-moved-root");
            match case {
                "replace" => {
                    std::fs::rename(root.path().join("source.rs"), root.path().join("old.rs"))
                        .unwrap();
                    std::fs::write(root.path().join("source.rs"), "committed\n").unwrap();
                }
                "inplace" => {
                    std::fs::write(root.path().join("source.rs"), "concurrent editor\n").unwrap()
                }
                "root" => {
                    std::fs::rename(root.path(), moved.path().join("original")).unwrap();
                    std::fs::create_dir(root.path()).unwrap();
                    std::fs::write(root.path().join("source.rs"), "new namespace\n").unwrap();
                }
                "readonly" => {
                    permission.lock().unwrap().set_mode(SecurityMode::ReadOnly);
                }
                "rule-deny" => {
                    *permission.lock().unwrap() =
                        crate::permission::checker::PermissionChecker::new(
                            &PermissionConfigs::from(PermissionConfig {
                                default: Some(Action::Allow),
                                bash: Some(crate::permission::ToolPerm::Simple(Action::Deny)),
                                ..Default::default()
                            }),
                            SecurityMode::Standard,
                            Some(root.path().to_path_buf()),
                            None,
                        )
                        .unwrap();
                }
                "cancel" => scope.cancellation_handle().cancel(),
                _ => {}
            }
            let decision = if case == "deny" {
                UserDecision::Deny
            } else {
                UserDecision::AllowOnce
            };
            request.reply.send(decision).unwrap();
            let output = tokio::time::timeout(Duration::from_secs(3), work)
                .await
                .unwrap()
                .unwrap();
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
            assert!(output.contains("Written 10 bytes"));
            assert!(rx.try_recv().is_err(), "only one formatter approval");
            assert_eq!(
                root.path().join("formatter.marker").exists(),
                case == "allow",
                "{case}: {output}"
            );
            if matches!(case, "replace" | "inplace") {
                assert!(
                    output.contains("Formatter skipped: target changed after approval"),
                    "{case}: {output}"
                );
            }
            if case == "allow" {
                assert_eq!(
                    std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
                    "actually formatted\n"
                );
            } else {
                assert!(!output.contains("Formatted."), "{case}");
            }
        }
    }

    #[cfg(unix)]
    struct Cleanup {
        scope: Arc<crate::agent::runner::AgentWorkScope>,
        sandbox: Sandbox,
    }
    #[cfg(unix)]
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.scope.cancellation_handle().cancel();
            self.sandbox.kill_active();
        }
    }
    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn alive(pid: i32) -> bool {
        assert!(pid > 1);
        // SAFETY: signal zero is a read-only existence probe. This helper never
        // authorizes a kill by PID; cleanup uses the retained Sandbox owner.
        unsafe {
            libc::kill(pid, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }
    #[cfg(unix)]
    fn pid(workspace: &WorkspaceBinding, name: &str) -> Option<i32> {
        let mut content = String::new();
        workspace
            .open_regular_relative(Path::new(name))
            .ok()?
            .take(64)
            .read_to_string(&mut content)
            .ok()?;
        content.trim().parse().ok()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_native_timeout_output_cap_and_cancel_retire_the_owned_family() {
        use super::super::{WriteArgs, WriteTool};
        use rig::tool::Tool;
        for mode in ["timeout", "output", "cancel"] {
            let (root, workspace, formatter) = fixture(0);
            let script = if mode == "output" {
                "#!/bin/sh\necho $$ > formatter.pid\ni=0\nwhile [ \"$i\" -lt 20000 ]; do printf 'XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\\n'; i=$((i+1)); done\nprintf late > \"$1\"\n"
            } else {
                "#!/bin/sh\necho $$ > formatter.pid\nsleep 30 &\necho $! > descendant.pid\nwait\nprintf late > \"$1\"\n"
            };
            std::fs::write(root.path().join("formatter.sh"), script).unwrap();
            let scope = crate::agent::runner::AgentWorkScope::new();
            let _cleanup = Cleanup {
                scope: Arc::clone(&scope),
                sandbox: formatter.sandbox.clone(),
            };
            let tool = WriteTool::new(None, None, None)
                .with_workspace_binding(Arc::clone(&workspace))
                .with_formatter(Some(formatter));
            let mut work = Box::pin(scope.run(tool.call(WriteArgs {
                path: "source.rs".into(),
                content: "committed\n".into(),
                overwrite: false,
            })));
            if mode == "cancel" {
                tokio::select! {
                    result = &mut work => panic!("held formatter finished: {result:?}"),
                    _ = async {
                        tokio::time::timeout(Duration::from_secs(3), async {
                            while pid(&workspace, "descendant.pid").is_none() { tokio::time::sleep(Duration::from_millis(5)).await; }
                        }).await.unwrap();
                    } => {}
                }
                scope.cancellation_handle().cancel();
                drop(work);
            } else {
                let result = tokio::time::timeout(Duration::from_secs(15), work)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(result.contains("Written 10 bytes"));
                assert!(
                    result.contains("Formatter failed or stopped"),
                    "{mode}: {result}"
                );
                assert!(!result.contains("XXXXX"));
            }
            tokio::time::timeout(Duration::from_secs(3), scope.wait_idle())
                .await
                .unwrap();
            assert_eq!(scope.active_children(), 0);
            assert_eq!(
                std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
                "committed\n"
            );
            for name in ["formatter.pid", "descendant.pid"] {
                if let Some(pid) = pid(&workspace, name) {
                    tokio::time::timeout(Duration::from_secs(3), async {
                        while alive(pid) {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .unwrap();
                } else {
                    assert_eq!(mode, "output", "the held family must be actually observed");
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_actual_empty_map_keeps_output_and_atomic_formatter_replacement_is_observed() {
        use super::super::{ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool};
        use rig::tool::{Tool, ToolCallExtensions};
        let (root, workspace, formatter) = fixture(0);
        let tracker = ReadTracker::new(true);
        let empty = FormatAfterEdit::new(&BTreeMap::new(), formatter.sandbox.clone());
        assert!(empty.is_none());
        let write = WriteTool::new_with_tracker(None, None, None, tracker.clone())
            .with_workspace_binding(Arc::clone(&workspace))
            .with_formatter(empty);
        let first = write
            .call(WriteArgs {
                path: "same.rs".into(),
                content: "same\n".into(),
                overwrite: false,
            })
            .await
            .unwrap();
        ReadTool::new_with_tracker(None, None, None, 100, tracker)
            .with_workspace_binding(Arc::clone(&workspace))
            .call(ReadArgs {
                raw_notebook: false,
                path: "same.rs".into(),
                offset: None,
                limit: None,
            })
            .await
            .unwrap();
        let second = write
            .call(WriteArgs {
                path: "same.rs".into(),
                content: "same\n".into(),
                overwrite: true,
            })
            .await
            .unwrap();
        assert_eq!(first, second);
        assert!(!root.path().join("formatter.marker").exists());
        std::fs::write(
            root.path().join("formatter.sh"),
            "#!/bin/sh\ncat .formatter-config > formatted.tmp\nmv formatted.tmp \"$1\"\n",
        )
        .unwrap();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let result = scope
            .run(
                WriteTool::new(None, None, None)
                    .with_workspace_binding(workspace)
                    .with_formatter(Some(formatter))
                    .call_structured(
                        WriteArgs {
                            path: "atomic.rs".into(),
                            content: "authored\n".into(),
                            overwrite: false,
                        },
                        &ToolCallExtensions::new(),
                    ),
            )
            .await
            .unwrap();
        scope.wait_idle().await;
        assert!(result.output.contains("Formatted."));
        assert_eq!(
            result
                .extensions
                .get::<super::super::mutation_diff::MutationDiff>()
                .unwrap()
                .after_bytes,
            "actually formatted\n".len()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("atomic.rs")).unwrap(),
            "actually formatted\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn format45_non_utf8_workspace_is_refused_before_write_or_formatter() {
        use super::super::{WriteArgs, WriteTool};
        use rig::tool::Tool;
        use std::os::unix::ffi::OsStringExt;
        let (root, _, formatter) = fixture(0);
        let actual = root
            .path()
            .join(std::ffi::OsString::from_vec(b"workspace_\xff".to_vec()));
        std::fs::create_dir(&actual).unwrap();
        let workspace = Arc::new(WorkspaceBinding::capture(&actual).unwrap());
        let scope = crate::agent::runner::AgentWorkScope::new();
        let result = scope
            .run(
                WriteTool::new(None, None, None)
                    .with_workspace_binding(workspace)
                    .with_formatter(Some(formatter))
                    .call(WriteArgs {
                        path: "source.rs".into(),
                        content: "committed\n".into(),
                        overwrite: false,
                    }),
            )
            .await
            .unwrap_err();
        scope.wait_idle().await;
        assert_eq!(
            result.to_string(),
            "bound workspace path is not valid UTF-8"
        );
        assert!(!actual.join("source.rs").exists());
        assert_eq!(scope.active_children(), 0);
        assert!(!root.path().join("workspace_�").exists());
        assert!(!root.path().join("formatter.marker").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_non_utf8_operand_refuses_before_lookup_or_permission_on_all_unix_hosts() {
        use std::os::unix::ffi::OsStringExt;
        let (root, workspace, formatter) = fixture(0);
        // APFS rejects invalid UTF-8 filenames. This portable control supplies
        // an exact OsString operand without inventing a lossy filesystem name;
        // Linux additionally proves the existing tool boundary refuses a real
        // non-UTF-8 workspace before mutation or formatter dispatch above.
        let path = root
            .path()
            .join(std::ffi::OsString::from_vec(b"source_\xff.rs".to_vec()));
        let scope = crate::agent::runner::AgentWorkScope::new();
        let result = scope
            .run(formatter.run(
                &path,
                Some((&workspace, Path::new("source.rs"))),
                "committed\n",
                &None,
                &None,
                1024,
            ))
            .await
            .unwrap();
        scope.wait_idle().await;
        assert!(result.status.contains("path is not UTF-8"));
        assert!(result.content.is_none());
        assert_eq!(scope.active_children(), 0);
        assert!(!root.path().join("source_�.rs").exists());
        assert!(!root.path().join("formatter.marker").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_actual_nonregular_invalid_utf8_and_oversized_final_observations_omit_stale_metadata()
     {
        use super::super::{WriteArgs, WriteTool};
        use rig::tool::{Tool, ToolCallExtensions};
        for mode in ["fifo", "utf8", "size"] {
            let (root, workspace, formatter) = fixture(0);
            let script = match mode {
                "fifo" => "#!/bin/sh\nrm \"$1\"\nmkfifo \"$1\"\n",
                "utf8" => "#!/bin/sh\nprintf '\\377' > \"$1\"\n",
                _ => {
                    "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 100 ]; do printf 'XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX\\n'; i=$((i+1)); done > \"$1\"\n"
                }
            };
            std::fs::write(root.path().join("formatter.sh"), script).unwrap();
            let scope = crate::agent::runner::AgentWorkScope::new();
            let _cleanup = Cleanup {
                scope: Arc::clone(&scope),
                sandbox: formatter.sandbox.clone(),
            };
            let tool = WriteTool::new(None, None, Some(1024))
                .with_workspace_binding(workspace)
                .with_formatter(Some(formatter));
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                scope.run(tool.call_structured(
                    WriteArgs {
                        path: "source.rs".into(),
                        content: "committed\n".into(),
                        overwrite: false,
                    },
                    &ToolCallExtensions::new(),
                )),
            )
            .await
            .unwrap()
            .unwrap();
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
            assert!(result.output.contains("Written 10 bytes"));
            assert!(
                result.output.contains("final file text unavailable"),
                "{mode}: {}",
                result.output
            );
            assert!(
                result
                    .extensions
                    .get::<super::super::mutation_diff::MutationDiff>()
                    .is_none()
            );
        }
    }

    #[test]
    fn format45_observation_caps_max_config_before_reading_sparse_final_file() {
        let root = crate::tests::OwnedTestDirectory::new("format45-observation-cap");
        let workspace = Arc::new(WorkspaceBinding::capture(root.path()).unwrap());
        let path = workspace.root().join("source.rs");
        std::fs::write(&path, "small\n").unwrap();
        let target = Target::capture(&path, Some((&workspace, Path::new("source.rs")))).unwrap();
        assert_eq!(target.read(u64::MAX).unwrap().text, "small\n");
        assert!(
            target.read(5).is_err(),
            "configured lower limit still applies"
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_OBSERVATION_BYTES + 1)
            .unwrap();
        assert!(
            target.read(u64::MAX).is_err(),
            "large configured limit cannot admit an unbounded observation"
        );
    }
    #[cfg(all(unix, feature = "lsp"))]
    #[tokio::test]
    async fn format45_late_read_revocation_during_real_lsp_wait_scrubs_write_and_edit_results() {
        use super::super::{EditArgs, EditTool, WriteArgs, WriteTool};
        use crate::config::types::{LspConfig, LspServerConfig};
        use crate::extras::lsp::LspManager;
        use crate::permission::{
            Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm,
        };
        use rig::tool::{Tool, ToolCallExtensions};
        struct ResumeClock;
        impl Drop for ResumeClock {
            fn drop(&mut self) {
                tokio::time::resume();
            }
        }
        for edit in [false, true] {
            for revoke in [false, true] {
                let (root, workspace, formatter) = fixture(0);
                let path = workspace.root().join("source.rs");
                if edit {
                    std::fs::write(&path, "old\n").unwrap();
                }
                let script = command(
                    "./formatter.sh {file}",
                    path.to_str().unwrap(),
                    ShellDialect::Posix,
                )
                .unwrap();
                let checker = |read| {
                    let mut checker = crate::permission::checker::PermissionChecker::new(
                        &PermissionConfigs::from(PermissionConfig {
                            default: Some(Action::Allow),
                            read: Some(ToolPerm::Simple(read)),
                            bash: Some(ToolPerm::Simple(Action::Allow)),
                            ..Default::default()
                        }),
                        SecurityMode::Standard,
                        Some(root.path().to_path_buf()),
                        Some(vec!["standard".into()]),
                    )
                    .unwrap();
                    checker.add_session_allowlist("shell".into(), &script);
                    checker
                };
                let permission = Arc::new(std::sync::Mutex::new(checker(Action::Allow)));
                let mut cfg = LspConfig {
                    enabled: true.into(),
                    ..Default::default()
                };
                cfg.servers.insert(
                    "rust".into(),
                    LspServerConfig {
                        command: workspace
                            .root()
                            .join("missing-owned-lsp")
                            .to_str()
                            .unwrap()
                            .into(),
                        extensions: vec![".rs".into()],
                        ..Default::default()
                    },
                );
                let lsp = LspManager::new(&cfg, Arc::clone(&workspace));
                let formatter_sandbox = formatter.sandbox.clone();
                let write = WriteTool::new(Some(Arc::clone(&permission)), None, None)
                    .with_workspace_binding(Arc::clone(&workspace))
                    .with_formatter(Some(Arc::clone(&formatter)))
                    .with_lsp(Some(lsp.clone()));
                let edit_tool = EditTool::new(Some(Arc::clone(&permission)), None)
                    .with_workspace_binding(Arc::clone(&workspace))
                    .with_formatter(Some(formatter))
                    .with_lsp(Some(lsp.clone()));
                let scope = crate::agent::runner::AgentWorkScope::new();
                let _cleanup = Cleanup {
                    scope: Arc::clone(&scope),
                    sandbox: formatter_sandbox,
                };
                let work = scope.run(async {
                    let extensions = ToolCallExtensions::new();
                    if edit {
                        edit_tool
                            .call_structured(
                                EditArgs {
                                    path: "source.rs".into(),
                                    block: Some(
                                        "<<<<<<< SEARCH\nold\n=======\nrequested\n>>>>>>> REPLACE"
                                            .into(),
                                    ),
                                    file_crc: None,
                                    edits: None,
                                    replace_all: false,
                                    fuzzy: false,
                                },
                                &extensions,
                            )
                            .await
                    } else {
                        write
                            .call_structured(
                                WriteArgs {
                                    path: "source.rs".into(),
                                    content: "requested\n".into(),
                                    overwrite: false,
                                },
                                &extensions,
                            )
                            .await
                    }
                });
                tokio::pin!(work);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                loop {
                    tokio::select! {
                        value = &mut work => panic!("tool settled before diagnostic publication: {value:?}"),
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                    }
                    if lsp.diagnostic_snapshot().failure_count == Some(1) {
                        break;
                    }
                    assert!(tokio::time::Instant::now() < deadline);
                }
                assert_eq!(
                    scope.active_children(),
                    0,
                    "native formatter/service work retired before pausing time"
                );
                tokio::time::pause();
                let clock = ResumeClock;
                // Poll through the callback and prove it remains pending. The
                // paired positive must then complete via publish, well before
                // the existing 1000ms diagnostic timeout, and return its text.
                tokio::select! {
                    value = &mut work => panic!("tool was not waiting for diagnostics: {value:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(30)) => {},
                }
                if revoke {
                    *permission.lock().unwrap() = checker(Action::Deny);
                }
                let uri = crate::extras::lsp::client::file_uri(&path).unwrap();
                let mut diagnostic = lsp_types::Diagnostic::new_simple(
                    lsp_types::Range::new(
                        lsp_types::Position::new(0, 0),
                        lsp_types::Position::new(0, 1),
                    ),
                    "PRIVATE_LSP45".into(),
                );
                diagnostic.severity = Some(lsp_types::DiagnosticSeverity::ERROR);
                assert!(lsp.publish_diagnostics_for_test(&uri, "rust", vec![diagnostic]));
                let result = tokio::time::timeout(Duration::from_millis(250), &mut work)
                    .await
                    .expect("publication must wake the actual wait, not its 1000ms timeout")
                    .unwrap();
                drop(clock); // Resume before native/owned cleanup and the next case.
                let has_diff = result
                    .extensions
                    .get::<super::super::mutation_diff::MutationDiff>()
                    .is_some();
                let rendered = result.output;
                assert_eq!(has_diff, !revoke);
                assert_eq!(rendered.contains("PRIVATE_LSP45"), !revoke);
                if edit {
                    assert_eq!(rendered.contains("actually formatted"), !revoke);
                }
                assert!(
                    rendered.contains("Formatted."),
                    "edit={edit} revoke={revoke}: {rendered}"
                );
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    "actually formatted\n"
                );
                lsp.shutdown().await;
                scope.wait_idle().await;
                assert_eq!(scope.active_children(), 0);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn format45_held_ask_required_question_or_exhausted_spend_blocks_formatter_dispatch() {
        use super::super::{WriteArgs, WriteTool};
        use crate::permission::ask::UserDecision;
        use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode};
        use rig::tool::Tool;
        for question in [false, true] {
            let (root, workspace, formatter) = fixture(0);
            let permission = Arc::new(std::sync::Mutex::new(
                crate::permission::checker::PermissionChecker::new(
                    &PermissionConfigs::from(PermissionConfig {
                        default: Some(Action::Allow),
                        ..Default::default()
                    }),
                    SecurityMode::Standard,
                    Some(root.path().to_path_buf()),
                    None,
                )
                .unwrap(),
            ));
            let (tx, mut rx) = tokio::sync::mpsc::channel(2);
            let tool = WriteTool::new(Some(permission), Some(tx), None)
                .with_workspace_binding(workspace)
                .with_formatter(Some(formatter));
            let account =
                crate::spend::Account::new("formatter45-admission", Some(1.0), 0.0).unwrap();
            let run = crate::spend::run::Run::fixture(
                account.clone(),
                Arc::new(crate::spend::Ledger::new(&root.path().join("state"))),
                "formatter45-admission",
                crate::spend::PriceSnapshot::capture(
                    Some((1_000_000.0, 1_000_000.0)),
                    Default::default(),
                    false,
                ),
            );
            let scope = crate::agent::runner::AgentWorkScope::new();
            scope.run(run.prepare()).await.unwrap();
            let turn = crate::question::QuestionTurn::default();
            let work = crate::spend::run::scope(
                Some(run.clone()),
                crate::question::scope(
                    Some(turn.clone()),
                    scope.run(tool.call(WriteArgs {
                        path: "source.rs".into(),
                        content: "committed\n".into(),
                        overwrite: false,
                    })),
                ),
            );
            tokio::pin!(work);
            let request = tokio::select! {
                value = &mut work => panic!("formatter did not ask: {value:?}"),
                request = rx.recv() => request.unwrap().into_permission(),
                _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("missing formatter Ask"),
            };
            assert_eq!(request.tool, "shell");
            assert_eq!(
                std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
                "committed\n"
            );
            if question {
                turn.require(crate::question::QuestionBatch(vec![
                    crate::question::Question {
                        question: "Choose the next step".into(),
                        options: Vec::new(),
                        allow_free_text: true,
                    },
                ]));
                assert!(turn.required().is_some());
            } else {
                run.observe(crate::event::UsageDelta {
                    input_tokens: 2,
                    ..Default::default()
                });
                scope.run(run.flush()).await;
                assert!(account.snapshot().exhausted());
            }
            request.reply.send(UserDecision::AllowOnce).unwrap();
            let rendered = tokio::time::timeout(Duration::from_secs(3), &mut work)
                .await
                .unwrap()
                .unwrap();
            assert!(rendered.contains("Written 10 bytes"));
            assert!(!rendered.contains("Formatted."));
            assert!(!root.path().join("formatter.marker").exists());
            assert_eq!(
                std::fs::read_to_string(root.path().join("source.rs")).unwrap(),
                "committed\n"
            );
            assert!(rx.try_recv().is_err());
            scope.wait_idle().await;
            assert_eq!(scope.active_children(), 0);
        }
    }
}
