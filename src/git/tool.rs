use std::path::{Component, Path};
use std::sync::Arc;

use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{ToolError, check_perm, check_perm_bound_path};
use crate::git::runner::{
    GitRunError, GitRunner, LOCAL_MUTATION_LIMITS, QUERY_LIMITS, command_result,
};
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::{CommandOutput, CommandStatus, Sandbox};

const TEXT_LIMITS: crate::sandbox::CommandLimits = crate::sandbox::CommandLimits {
    timeout: std::time::Duration::from_secs(10),
    stdout_bytes: 192 * 1024,
    stderr_bytes: 64 * 1024,
    combined_bytes: 224 * 1024,
};

/// Bounds for `git status`: a larger time budget than other queries (huge
/// monorepos), with output beyond the cap reported as truncated.
const STATUS_LIMITS: crate::sandbox::CommandLimits = crate::sandbox::CommandLimits {
    timeout: std::time::Duration::from_secs(30),
    stdout_bytes: 256 * 1024,
    stderr_bytes: 64 * 1024,
    combined_bytes: 320 * 1024,
};

/// Bounds for the filter-attribute probe, which lists and classifies every
/// index path. Exceeding them fails the operation closed.
const FILTER_PROBE_LIMITS: crate::sandbox::CommandLimits = crate::sandbox::CommandLimits {
    timeout: std::time::Duration::from_secs(30),
    stdout_bytes: 16 * 1024 * 1024,
    stderr_bytes: 64 * 1024,
    combined_bytes: 16 * 1024 * 1024 + 64 * 1024,
};

/// Bounds for one diff/show run whose output is then paged. Larger output
/// is reported as truncated at this ceiling.
const PAGED_TEXT_LIMITS: crate::sandbox::CommandLimits = crate::sandbox::CommandLimits {
    timeout: std::time::Duration::from_secs(30),
    stdout_bytes: 16 * 1024 * 1024,
    stderr_bytes: 64 * 1024,
    combined_bytes: 16 * 1024 * 1024 + 64 * 1024,
};
/// Bytes of diff/show text one call returns; `offset` pages through the rest.
/// Most commits one `log` call returns; larger requests are rejected.
const LOG_MAX_COUNT: u16 = 100;
const TEXT_PAGE_BYTES: usize = 192 * 1024;

/// Page size actually used: the diff/show page is delivered inline by the
/// self-bounded tool-result path, so it must stay under the
/// `[tool_limits] max_result_chars` backstop (with room for JSON escaping).
fn text_page_bytes() -> usize {
    TEXT_PAGE_BYTES.min((crate::agent::tools::limits::current().max_result_chars / 4) * 3)
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GitOperation {
    #[default]
    Status,
    Diff,
    Log,
    Show,
    /// Line-by-line authorship of one file.
    Blame,
    Stage,
    Unstage,
    Commit,
    /// List local branches.
    Branches,
    /// Create a branch at `revision` (default HEAD) without switching.
    BranchCreate,
    /// Switch the working tree to an existing local branch.
    Switch,
    /// Save and clean local changes (`git stash push`).
    StashPush,
    /// Re-apply and drop the newest stash.
    StashPop,
    StashList,
    /// Create a lightweight tag, or an annotated one with `message`.
    Tag,
    /// Discard working-tree changes of `paths` (from the index, or `revision`).
    Restore,
    /// Merge a local revision into the current branch (`merge --no-edit`).
    Merge,
    /// Abort an in-progress merge (`merge --abort`).
    MergeAbort,
    /// Apply one commit onto the current branch (`cherry-pick --no-edit`).
    CherryPick,
    /// Abort an in-progress cherry-pick.
    CherryPickAbort,
    /// Record a commit that undoes `revision` (`revert --no-edit`).
    Revert,
    /// Abort an in-progress revert.
    RevertAbort,
    /// Move HEAD (and with `mixed` the index) to a validated local revision.
    Reset,
    /// Delete a merged, non-current local branch (`branch -d`).
    BranchDelete,
    /// Rename a non-current local branch.
    BranchRename,
    /// Re-apply stash `index` (default newest) and keep it.
    StashApply,
    /// Drop stash `index` (default newest).
    StashDrop,
}

impl GitOperation {
    /// Operations that only read repository state.
    pub(crate) fn is_inspection(self) -> bool {
        matches!(
            self,
            Self::Status
                | Self::Diff
                | Self::Log
                | Self::Show
                | Self::Blame
                | Self::Branches
                | Self::StashList
        )
    }

    // The native /diff and /commit actors (git-worktree) report the name.
    #[cfg_attr(not(feature = "git-worktree"), allow(dead_code))]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Diff => "diff",
            Self::Log => "log",
            Self::Show => "show",
            Self::Blame => "blame",
            Self::Stage => "stage",
            Self::Unstage => "unstage",
            Self::Commit => "commit",
            Self::Branches => "branches",
            Self::BranchCreate => "branch_create",
            Self::Switch => "switch",
            Self::StashPush => "stash_push",
            Self::StashPop => "stash_pop",
            Self::StashList => "stash_list",
            Self::Tag => "tag",
            Self::Restore => "restore",
            Self::Merge => "merge",
            Self::MergeAbort => "merge_abort",
            Self::CherryPick => "cherry_pick",
            Self::CherryPickAbort => "cherry_pick_abort",
            Self::Revert => "revert",
            Self::RevertAbort => "revert_abort",
            Self::Reset => "reset",
            Self::BranchDelete => "branch_delete",
            Self::BranchRename => "branch_rename",
            Self::StashApply => "stash_apply",
            Self::StashDrop => "stash_drop",
        }
    }
}

/// Diff/show rendering.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffFormat {
    #[default]
    Patch,
    Stat,
    NameStatus,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitArgs {
    pub operation: GitOperation,
    #[serde(default)]
    pub paths: Vec<String>,
    pub revision: Option<String>,
    pub message: Option<String>,
    pub max_count: Option<u16>,
    /// Branch or tag name for branch_create, switch and tag.
    #[serde(default)]
    pub name: Option<String>,
    /// diff/show output format.
    #[serde(default)]
    pub format: Option<DiffFormat>,
    /// Byte offset of the diff/show page to return.
    #[serde(default)]
    pub offset: Option<u64>,
    /// diff: second revision (`diff <revision> <revision_to>`).
    #[serde(default)]
    pub revision_to: Option<String>,
    /// diff: compare the index (`--cached`) instead of the working tree.
    #[serde(default)]
    pub staged: Option<bool>,
    /// log: number of commits to skip (page past `max_count` with `next_skip`).
    #[serde(default)]
    pub skip: Option<u32>,
    /// log: only commits whose author matches this fixed string.
    #[serde(default)]
    pub author: Option<String>,
    /// log: only commits whose message contains this fixed string.
    #[serde(default)]
    pub grep: Option<String>,
    /// blame: first line (1-based) of the range.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// blame: last line (inclusive).
    #[serde(default)]
    pub end_line: Option<u32>,
    /// commit: replace the unpushed HEAD commit (guarded; needs `git/amend`).
    #[serde(default)]
    pub amend: Option<bool>,
    /// commit: append a `Signed-off-by` trailer for the resolved identity.
    #[serde(default)]
    pub signoff: Option<bool>,
    /// commit: allow a commit that records no change.
    #[serde(default)]
    pub allow_empty: Option<bool>,
    /// branches: which refs to list (default `local`; no network access).
    #[serde(default)]
    pub scope: Option<RefScope>,
    /// log, blame, branches, stash_list: time budget in seconds for this call
    /// (default 30, at most 120). Retry with a larger value after a timeout.
    #[serde(default)]
    pub timeout_secs: Option<u16>,
    /// commit: allow committing while a rebase or cherry-pick is in progress.
    #[serde(default)]
    pub allow_in_progress: Option<bool>,
    /// reset: `soft` (HEAD only) or `mixed` (HEAD and index; the default).
    #[serde(default)]
    pub mode: Option<ResetMode>,
    /// branch_rename: the new branch name (`name` is the current one).
    #[serde(default)]
    pub new_name: Option<String>,
    /// stash_apply, stash_drop: stash number (`stash@{index}`; default 0).
    #[serde(default)]
    pub index: Option<u32>,
    /// stash_push: also stash untracked files.
    #[serde(default)]
    pub include_untracked: Option<bool>,
}

/// How far `reset` moves.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResetMode {
    /// Move HEAD only; the index and working tree keep their content.
    Soft,
    /// Move HEAD and reset the index; the working tree is untouched.
    #[default]
    Mixed,
}

/// Which refs `branches` lists.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefScope {
    /// `refs/heads`.
    #[default]
    Local,
    /// Remote-tracking branches (`refs/remotes`); nothing is fetched.
    Remote,
    /// Local and remote-tracking branches.
    All,
    /// Tags (`refs/tags`).
    Tags,
}

/// Default time budget of log, blame, branches and stash_list.
const QUERY_TIMEOUT_DEFAULT_SECS: u16 = 30;
/// Largest `timeout_secs` a call may request.
const QUERY_TIMEOUT_MAX_SECS: u16 = 120;
/// Most refs one `branches` call returns, and its default page size.
const REF_PAGE_MAX: u16 = 1000;
const REF_PAGE_DEFAULT: u16 = 100;

/// Bounds for ref listings: generous output (thousands of refs), caller-chosen
/// time budget. Output beyond the cap is reported as truncated.
fn ref_limits(timeout: std::time::Duration) -> crate::sandbox::CommandLimits {
    crate::sandbox::CommandLimits {
        timeout,
        stdout_bytes: 8 * 1024 * 1024,
        stderr_bytes: 64 * 1024,
        combined_bytes: 8 * 1024 * 1024 + 64 * 1024,
    }
}

/// The per-call query deadline from `timeout_secs`.
fn query_timeout(args: &GitArgs) -> Result<std::time::Duration, ToolError> {
    match args.timeout_secs {
        None => Ok(std::time::Duration::from_secs(
            QUERY_TIMEOUT_DEFAULT_SECS.into(),
        )),
        Some(secs) if (1..=QUERY_TIMEOUT_MAX_SECS).contains(&secs) => {
            Ok(std::time::Duration::from_secs(secs.into()))
        }
        Some(_) => Err(ToolError::Msg(format!(
            "timeout_secs must be between 1 and {QUERY_TIMEOUT_MAX_SECS}"
        ))),
    }
}

fn with_timeout(
    mut limits: crate::sandbox::CommandLimits,
    timeout: std::time::Duration,
) -> crate::sandbox::CommandLimits {
    limits.timeout = timeout;
    limits
}

/// A revision quoted for an error message: bounded, and shown with its
/// quotes escaped so it cannot be mistaken for the surrounding text.
fn shown_revision(revision: &str) -> String {
    let mut shown: String = revision.chars().take(80).collect();
    if shown.len() < revision.len() {
        shown.push('…');
    }
    format!("{shown:?}")
}

/// Says which side of a `a..b` range a revision error belongs to.
fn side_of_range(error: ToolError, side: &str) -> ToolError {
    match error {
        ToolError::Msg(message) => {
            ToolError::Msg(format!("{message} (the {side} side of the range)"))
        }
        other => other,
    }
}

/// Adds the retry guidance to a timeout error.
fn timeout_guidance(error: GitRunError) -> ToolError {
    match error {
        GitRunError::TimedOut { .. } => ToolError::Msg(format!(
            "{error}; retry with a larger timeout_secs (at most {QUERY_TIMEOUT_MAX_SECS}) or narrow the query (revision range, paths, skip)"
        )),
        other => other.into(),
    }
}

impl From<GitRunError> for ToolError {
    fn from(error: GitRunError) -> Self {
        ToolError::Msg(error.to_string())
    }
}

/// Disclosure attached to commit and tag results: the tool hardens every Git
/// call, so repository hooks and signing never run.
const HOOKS_SIGNING_NOTICE: &str = "hooks and signing were not run: this tool disables repository hooks (pre-commit, commit-msg, ...) and commit/tag signing, so run required checks yourself and sign outside the tool if the repository requires it";

fn mark_hooks_and_signing_skipped(result: &mut serde_json::Value) {
    result["hooks_run"] = serde_json::json!(false);
    result["signed"] = serde_json::json!(false);
    result["notice"] = serde_json::json!(HOOKS_SIGNING_NOTICE);
}

/// Disclosure attached to status/diff/show results when the repository holds
/// gitlinks: Git is never run inside a (model-writable) nested repository, so
/// submodule changes are excluded from these results.
const SUBMODULES_NOTICE: &str = "submodules_ignored: submodule (gitlink) changes are not reported by status, diff or show because Git is never run inside nested repositories; a modified, advanced or dirty submodule can exist even when this result shows no change. Check the listed submodule_paths yourself if they matter";

/// Gitlink paths recorded in an index listing (`ls-files --stage -z`); mode
/// 160000 entries only.
fn gitlink_paths(stage_listing: &[u8]) -> Vec<String> {
    stage_listing
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            let record = std::str::from_utf8(record).ok()?;
            let (meta, path) = record.split_once('\t')?;
            meta.starts_with("160000 ").then(|| path.to_string())
        })
        .collect()
}

pub(crate) struct GitTool {
    runner: GitRunner,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    exact_files: bool,
    /// Subagent mode: only the inspection operations run; every mutation is
    /// refused before any permission prompt or process.
    read_only: bool,
    filter_drivers: Arc<FilterDriverCache>,
    /// Canonical common Git directory keying the per-repository mutation lock,
    /// resolved once: the tool is bound to one immutable workspace.
    mutation_key: Arc<std::sync::OnceLock<std::path::PathBuf>>,
    #[cfg(test)]
    test_uncontained: bool,
}

/// Memoized result of the filter-driver config probe. The key is the full
/// content of the repository-controlled config files, so any edit (including
/// an in-place same-size rewrite) misses the cache.
#[derive(Default)]
struct FilterDriverCache {
    entry: std::sync::Mutex<Option<(Vec<u8>, FilterDrivers)>>,
    #[cfg(test)]
    probes: std::sync::atomic::AtomicUsize,
}

/// The filter drivers the contained Git sees, and whether the `lfs` driver (if
/// any) is the stock Git LFS one: only the canonical `git-lfs` commands (or an
/// emptied, non-executing value) under the four keys LFS installs. The runner
/// already replaces repository-defined commands with trusted or empty values
/// and every driver is neutralised again for the call, so nothing named `lfs`
/// ever executes; this only decides whether paths bound to it are reported as
/// unresolved LFS pointers instead of refusing the whole operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FilterDrivers {
    names: Vec<String>,
    lfs_canonical: bool,
}

/// Most files a `restore` checkpoints; a larger selection is reported as
/// uncaptured instead.
const RESTORE_CHECKPOINT_MAX_FILES: usize = 256;
/// Largest file whose preimage a `restore` checkpoints.
const RESTORE_CHECKPOINT_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Preimages read before `restore`, completed after it.
#[derive(Default)]
struct RestoreCapture {
    entries: Vec<(
        String,
        Option<crate::fs::ContentDigest>,
        crate::session::file_checkpoints::Pending,
    )>,
}

/// The current content of a workspace file, read through the binding.
enum WorkspaceFile {
    Bytes(Vec<u8>),
    Missing,
    Unreadable,
}

fn read_workspace_file(workspace: &crate::paths::WorkspaceBinding, path: &Path) -> WorkspaceFile {
    use std::io::Read;
    match workspace.open_regular_relative(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            match file
                .take(RESTORE_CHECKPOINT_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
            {
                Ok(_) if bytes.len() as u64 <= RESTORE_CHECKPOINT_FILE_BYTES => {
                    WorkspaceFile::Bytes(bytes)
                }
                _ => WorkspaceFile::Unreadable,
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WorkspaceFile::Missing,
        Err(_) => WorkspaceFile::Unreadable,
    }
}

/// Appends the one-time "not captured" checkpoint notice, when one is due.
fn attach_checkpoint_notice(result: &mut serde_json::Value) {
    if let Some(notice) = crate::session::file_checkpoints::take_uncaptured_notice() {
        result["checkpoint_notice"] = serde_json::json!(notice.trim());
    }
}

/// Whether a mutation's before/after status snapshots differ in anything
/// besides the branch header lines, or HEAD moved.
fn worktree_may_have_changed(before: &serde_json::Value, after: &serde_json::Value) -> bool {
    let records = |snapshot: &serde_json::Value| -> Vec<String> {
        snapshot
            .get("records")
            .and_then(serde_json::Value::as_array)
            .map(|records| {
                records
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter(|record| !record.starts_with("# "))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    head_from_snapshot(before) != head_from_snapshot(after) || records(before) != records(after)
}

/// Paths in the unmerged (`u`) state of a status snapshot.
fn conflicted_paths(snapshot: &serde_json::Value) -> Vec<String> {
    snapshot
        .get("path_records")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| entry.get("kind").and_then(serde_json::Value::as_str) == Some("u"))
        .filter_map(|entry| entry.get("path").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// What a caller can do about an in-progress history operation.
fn in_progress_note(operation: &str) -> String {
    match operation {
        "merge" => "a merge is in progress: resolve the conflicted paths, stage them and commit, or call merge_abort".to_string(),
        "cherry-pick" => "a cherry-pick is in progress: resolve the conflicted paths, stage them and commit with allow_in_progress=true, or call cherry_pick_abort; if it came from a multi-commit sequence use the shell to continue or abort".to_string(),
        "revert" => "a revert is in progress: resolve the conflicted paths, stage them and commit, or call revert_abort".to_string(),
        other => format!("a {other} is in progress. This tool cannot continue or abort it: use the shell (git {other} --continue or --abort) and do not commit until it is finished"),
    }
}

/// Most LFS-bound paths kept per operation.
const LFS_PATH_CAP: usize = 1000;

/// Shown with every result that was computed in a Git LFS repository.
const LFS_NOTICE: &str = "lfs_pointers_not_resolved: this repository stores some files with Git LFS and this tool never runs the LFS filters, so LFS-tracked paths are compared as raw working-tree content against pointer files and can be reported as modified when they are not. Staging or committing an LFS-tracked path is refused; use the shell for those paths";

/// The refusal for an operation that would rewrite LFS-tracked files.
fn lfs_refusal(operation: &str, path: &str) -> ToolError {
    ToolError::Msg(format!(
        "Git {operation} refused: this repository uses Git LFS (for example `{path}` is bound to the lfs filter) and this tool never runs LFS filters, so {operation} could replace LFS-tracked files with pointer text. Use the shell for this operation."
    ))
}

/// Adds the LFS disclosure to a result when the operation saw LFS-bound paths.
fn disclose_lfs(result: &mut serde_json::Value, lfs_paths: &[String]) {
    if lfs_paths.is_empty() {
        return;
    }
    result["lfs_pointers_not_resolved"] = serde_json::json!(true);
    result["lfs_paths"] = serde_json::json!(lfs_paths.iter().take(50).collect::<Vec<_>>());
    if lfs_paths.len() > 50 {
        result["lfs_paths_truncated"] = serde_json::json!(true);
    }
    result["lfs_notice"] = serde_json::json!(LFS_NOTICE);
}

/// Largest repository config file that is fingerprinted by content.
const CONFIG_FINGERPRINT_MAX_BYTES: u64 = 64 * 1024;

/// Content fingerprint of every repository-controlled config file Git reads
/// for this checkout. `None` when the answer cannot be keyed safely: unusual
/// layouts, oversized files, or any `include`/`includeIf` (included files are
/// not fingerprinted).
fn config_fingerprint(root: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let dot_git = root.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    let (git_dir, common_dir) = if meta.is_dir() {
        (dot_git.clone(), dot_git)
    } else if meta.is_file() {
        let mut text = String::new();
        std::fs::File::open(&dot_git)
            .ok()?
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        let git_dir = root.join(text.trim().strip_prefix("gitdir:")?.trim());
        let common = match std::fs::read_to_string(git_dir.join("commondir")) {
            Ok(common) => git_dir.join(common.trim()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
            Err(_) => return None,
        };
        (git_dir, common)
    } else {
        return None;
    };
    let mut files = vec![
        git_dir.join("config"),
        git_dir.join("config.worktree"),
        common_dir.join("config"),
        common_dir.join("config.worktree"),
    ];
    files.dedup();
    let mut seen = Vec::new();
    let mut key = Vec::new();
    for path in files {
        if seen.contains(&path) {
            continue;
        }
        key.extend_from_slice(path.as_os_str().as_encoded_bytes());
        key.push(0);
        match std::fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(CONFIG_FINGERPRINT_MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                if bytes.len() as u64 > CONFIG_FINGERPRINT_MAX_BYTES
                    || bytes
                        .windows(7)
                        .any(|window| window.eq_ignore_ascii_case(b"include"))
                {
                    return None;
                }
                key.push(1);
                key.extend_from_slice(&bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => key.push(2),
            Err(_) => return None,
        }
        key.push(0);
        seen.push(path);
    }
    Some(key)
}

impl GitTool {
    #[cfg(feature = "git-worktree")]
    pub(crate) fn capture(
        workspace: Arc<crate::paths::WorkspaceBinding>,
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Result<Self, String> {
        let runner = GitRunner::discover()?;
        runner.verify_contained(&workspace, &sandbox)?;
        Ok(Self {
            runner,
            workspace,
            sandbox,
            permission,
            ask_tx,
            exact_files: false,
            read_only: false,
            filter_drivers: Arc::default(),
            mutation_key: Arc::default(),
            #[cfg(test)]
            test_uncontained: false,
        })
    }

    /// Restrict the tool to the inspection operations (status, diff, log,
    /// show, blame, branches, stash_list), for subagents.
    #[cfg(feature = "git-worktree")]
    pub(crate) fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    #[cfg(feature = "git-worktree")]
    pub(crate) async fn verify_runtime(
        workspace: Arc<crate::paths::WorkspaceBinding>,
        sandbox: Sandbox,
    ) -> Result<serde_json::Value, String> {
        let tool = Self::capture(workspace, sandbox, None, None)?;
        tool.call(GitArgs {
            operation: GitOperation::Status,
            paths: Vec::new(),
            revision: None,
            message: None,
            max_count: None,
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())
    }

    #[cfg(feature = "git-worktree")]
    pub(crate) fn selected_files_only(mut self) -> Self {
        self.exact_files = true;
        self
    }

    fn reject_recursive_paths(&self, paths: &[String]) -> Result<(), ToolError> {
        if self.exact_files
            && paths.iter().any(|path| {
                std::fs::symlink_metadata(self.workspace.root().join(path))
                    .is_ok_and(|meta| meta.is_dir() || meta.file_type().is_symlink())
            })
        {
            return Err(ToolError::Msg(
                "selected-file Git mutations refuse directories and symbolic links".into(),
            ));
        }
        Ok(())
    }

    async fn run(
        &self,
        operation: &'static str,
        args: Vec<String>,
        limits: crate::sandbox::CommandLimits,
        allow_nonzero_or_truncated: bool,
    ) -> Result<CommandOutput, GitRunError> {
        #[cfg(test)]
        if self.test_uncontained {
            let result = if allow_nonzero_or_truncated {
                // Mirrors the contained path: truncated output is observed,
                // not an error.
                match self
                    .runner
                    .run_observed(
                        self.workspace.root(),
                        operation,
                        hardened_args(args),
                        limits,
                    )
                    .await
                {
                    Ok(output)
                        if matches!(output.status, CommandStatus::OutputLimitExceeded(_))
                            || (output.status == CommandStatus::Completed
                                && output.exit_status.is_some()) =>
                    {
                        Ok(output)
                    }
                    Ok(output) => command_result(operation, limits, output),
                    Err(error) => Err(error),
                }
            } else {
                self.runner
                    .run(
                        self.workspace.root(),
                        operation,
                        hardened_args(args),
                        limits,
                    )
                    .await
            };
            return result;
        }
        self.runner
            .run_contained(
                &self.workspace,
                &self.sandbox,
                operation,
                hardened_args(args),
                limits,
                allow_nonzero_or_truncated,
            )
            .await
    }

    async fn permission(
        &self,
        verb: &'static str,
        identity: &str,
    ) -> Result<Option<String>, ToolError> {
        check_perm(&self.permission, &self.ask_tx, verb, identity).await
    }

    /// Runs a read-only query and returns every observed outcome, including a
    /// timeout with the output captured so far. Callers decide how to report
    /// `TimedOut`/`OutputLimitExceeded`; other failures are errors.
    async fn run_query(
        &self,
        operation: &'static str,
        args: Vec<String>,
        limits: crate::sandbox::CommandLimits,
    ) -> Result<CommandOutput, ToolError> {
        let args = hardened_args(args);
        #[cfg(test)]
        let output = if self.test_uncontained {
            self.runner
                .run_observed(self.workspace.root(), operation, args, limits)
                .await
        } else {
            self.runner
                .run_contained_observed(&self.workspace, &self.sandbox, operation, args, limits)
                .await
        };
        #[cfg(not(test))]
        let output = self
            .runner
            .run_contained_observed(&self.workspace, &self.sandbox, operation, args, limits)
            .await;
        let output = output.map_err(ToolError::from)?;
        match output.status {
            CommandStatus::TimedOut | CommandStatus::OutputLimitExceeded(_) => Ok(output),
            CommandStatus::Completed
                if output.exit_status.is_some_and(|status| status.success())
                    || !output.stdout.is_empty() =>
            {
                Ok(output)
            }
            _ => {
                command_result(operation, limits, output).map_err(ToolError::from)?;
                Err(ToolError::Msg(format!("git {operation} failed")))
            }
        }
    }

    async fn run_mutation(
        &self,
        operation: &'static str,
        args: Vec<String>,
    ) -> Result<CommandOutput, ToolError> {
        #[cfg(test)]
        if self.test_uncontained {
            return self
                .runner
                .run_observed(
                    self.workspace.root(),
                    operation,
                    hardened_args(args),
                    LOCAL_MUTATION_LIMITS,
                )
                .await
                .map_err(ToolError::from);
        }
        self.runner
            .run_contained_observed(
                &self.workspace,
                &self.sandbox,
                operation,
                hardened_args(args),
                LOCAL_MUTATION_LIMITS,
            )
            .await
            .map_err(ToolError::from)
    }

    async fn run_with_input(
        &self,
        operation: &'static str,
        args: Vec<String>,
        input: Vec<u8>,
        limits: crate::sandbox::CommandLimits,
    ) -> Result<CommandOutput, ToolError> {
        #[cfg(test)]
        if self.test_uncontained {
            return self
                .runner
                .run_with_input_observed(
                    self.workspace.root(),
                    operation,
                    hardened_args(args),
                    input,
                    limits,
                )
                .await
                .map_err(ToolError::from);
        }
        self.runner
            .run_contained_with_input_observed(
                &self.workspace,
                &self.sandbox,
                operation,
                hardened_args(args),
                input,
                limits,
            )
            .await
            .map_err(ToolError::from)
    }

    async fn validate_revision(&self, revision: &str) -> Result<(), ToolError> {
        if revision.is_empty()
            || revision.starts_with('-')
            || revision.contains('\0')
            || revision.len() > 1024
        {
            return Err(ToolError::Msg(format!(
                "invalid Git revision {}: it must be non-empty, at most 1024 bytes, free of NUL, and must not start with '-'",
                shown_revision(revision)
            )));
        }
        self.run(
            "validate-revision",
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                "--end-of-options".into(),
                format!("{revision}^{{object}}"),
            ],
            QUERY_LIMITS,
            false,
        )
        .await
        .map(|_| ())
        .map_err(|error| match error {
            // `rev-parse --verify --quiet` exits non-zero without a message
            // only for a name that does not resolve; any other failure
            // (not a repository, timeout, launch failure) is passed through.
            GitRunError::Failed { operation, stderr }
                if operation == "validate-revision" && stderr.is_empty() =>
            {
                ToolError::Msg(format!(
                    "unknown Git revision {}: it does not name a commit, tag, branch or other object here",
                    shown_revision(revision)
                ))
            }
            other => other.into(),
        })
    }

    /// `a..b` or `a...b`: both sides must be valid revisions (an unvalidated
    /// side could smuggle an option or a pathspec-like operand).
    async fn validate_range(&self, range: &str) -> Result<(), ToolError> {
        let (left, right) = range
            .split_once("...")
            .or_else(|| range.split_once(".."))
            .ok_or_else(|| ToolError::Msg("invalid Git revision range".to_string()))?;
        if left.is_empty() || right.is_empty() {
            return Err(ToolError::Msg(
                "invalid Git revision range: both sides are required".to_string(),
            ));
        }
        self.validate_revision(left)
            .await
            .map_err(|error| side_of_range(error, "left"))?;
        self.validate_revision(right)
            .await
            .map_err(|error| side_of_range(error, "right"))
    }

    /// A single revision, or a `..`/`...` range.
    async fn validate_revision_or_range(&self, revision: &str) -> Result<bool, ToolError> {
        if revision.contains("..") {
            self.validate_range(revision).await?;
            Ok(true)
        } else {
            self.validate_revision(revision).await?;
            Ok(false)
        }
    }

    async fn validate_paths(
        &self,
        paths: &[String],
        permission_verb: Option<&'static str>,
    ) -> Result<Vec<String>, ToolError> {
        if paths.len() > 128 {
            return Err(ToolError::Msg(
                "too many Git paths (maximum 128)".to_string(),
            ));
        }
        let mut validated = Vec::with_capacity(paths.len());
        for value in paths {
            if value.is_empty() || value.starts_with('-') || value.contains('\0') {
                return Err(ToolError::Msg(
                    "invalid repository-relative Git path".to_string(),
                ));
            }
            let path = Path::new(value);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
            {
                return Err(ToolError::Msg(
                    "Git paths must remain relative to the bound workspace".to_string(),
                ));
            }
            if let Some(verb) = permission_verb {
                check_perm_bound_path(&self.permission, &self.ask_tx, verb, &self.workspace, path)
                    .await?;
                if std::fs::symlink_metadata(self.workspace.root().join(path))
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(ToolError::Msg(
                        "Git mutations reject symbolic-link path operands".to_string(),
                    ));
                }
            }
            validated.push(value.clone());
        }
        Ok(validated)
    }

    /// `git status` refreshes the index, so callers pass the argument prefix
    /// returned by [`Self::filter_guard`] for the current operation.
    ///
    /// Untracked directories are collapsed (`dir/`) so an un-ignored
    /// `node_modules` costs one record, and output beyond the cap is reported
    /// as `truncated: true` with the complete records that fit instead of
    /// failing the operation (and every mutation that snapshots around it).
    async fn status_snapshot(&self, guard: &[String]) -> Result<serde_json::Value, ToolError> {
        self.status_snapshot_for(guard, &[]).await
    }

    async fn status_snapshot_for(
        &self,
        guard: &[String],
        paths: &[String],
    ) -> Result<serde_json::Value, ToolError> {
        // Narrowing to paths expands untracked directories inside them.
        let untracked = if paths.is_empty() {
            "--untracked-files=normal"
        } else {
            "--untracked-files=all"
        };
        let mut command = vec![
            "status".into(),
            "--porcelain=v2".into(),
            "-z".into(),
            "--branch".into(),
            untracked.into(),
            "--ignore-submodules=all".into(),
        ];
        if !paths.is_empty() {
            command.push("--".into());
            command.extend(paths.iter().cloned());
        }
        let output = self
            .run("status", guarded(guard, command), STATUS_LIMITS, true)
            .await?;
        let truncated = matches!(output.status, CommandStatus::OutputLimitExceeded(_));
        let output = if truncated {
            output
        } else {
            command_result("status", STATUS_LIMITS, output).map_err(ToolError::from)?
        };
        let (records, changed_count, untracked_count) =
            parse_status_records(&output.stdout, truncated);
        let mut value = serde_json::json!({
            "records": records,
            "path_records": parse_status_paths(&output.stdout, truncated),
            "truncated": truncated,
            "counts": { "changed": changed_count, "untracked": untracked_count },
        });
        if truncated {
            value["truncation_note"] = serde_json::json!(
                "status output exceeded the cap; only the first records are listed. \
                 Pass `paths` to narrow status to a subdirectory (untracked files are \
                 then listed individually)."
            );
        }
        Ok(value)
    }

    /// Adds `submodules_ignored` plus a notice when the index holds gitlinks.
    /// Uses `ls-files --stage`, which only reads the index and never enters a
    /// nested repository. If the listing cannot be read completely the result
    /// is marked conservatively.
    async fn disclose_submodules(&self, result: &mut serde_json::Value) {
        let listing = self
            .run(
                "list-gitlinks",
                vec!["ls-files".into(), "--stage".into(), "-z".into()],
                FILTER_PROBE_LIMITS,
                true,
            )
            .await;
        let (paths, complete) = match &listing {
            Ok(output) if output.status == CommandStatus::Completed => {
                (gitlink_paths(&output.stdout), true)
            }
            Ok(output) => (gitlink_paths(&output.stdout), false),
            Err(_) => (Vec::new(), false),
        };
        if paths.is_empty() && complete {
            return;
        }
        result["submodules_ignored"] = serde_json::json!(true);
        result["submodule_paths"] = serde_json::json!(paths.iter().take(50).collect::<Vec<_>>());
        if paths.len() > 50 {
            result["submodule_paths_truncated"] = serde_json::json!(true);
        }
        result["submodules_notice"] = serde_json::json!(if complete {
            SUBMODULES_NOTICE.to_string()
        } else {
            format!(
                "{SUBMODULES_NOTICE} (the index could not be fully listed, so submodule presence is unknown)"
            )
        });
    }

    /// The per-repository mutation lock; the common-dir identity query runs
    /// once per tool instance.
    async fn acquire_mutation(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, ToolError> {
        if let Some(key) = self.mutation_key.get() {
            return Ok(self.runner.lock_repository(key).await);
        }
        let key = self
            .runner
            .repository_key(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
        let _ = self.mutation_key.set(key.clone());
        Ok(self.runner.lock_repository(&key).await)
    }

    /// The history operation Git is in the middle of, from the existence of
    /// its state files (no content is read): rebase, merge, cherry-pick,
    /// revert or bisect.
    async fn in_progress(&self) -> Result<Option<&'static str>, ToolError> {
        const STATE: [(&str, &str); 7] = [
            ("rebase-merge", "rebase"),
            ("rebase-apply", "rebase"),
            ("MERGE_HEAD", "merge"),
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
            ("BISECT_LOG", "bisect"),
            ("sequencer", "cherry-pick"),
        ];
        let mut command = vec!["rev-parse".to_string()];
        for (name, _) in STATE {
            command.push("--git-path".into());
            command.push(name.into());
        }
        let output = self
            .run("in-progress-state", command, QUERY_LIMITS, false)
            .await?;
        let lines = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = lines.lines().collect();
        if lines.len() != STATE.len() {
            return Err(ToolError::Msg(
                "git rev-parse --git-path returned an unexpected answer".to_string(),
            ));
        }
        for ((_, operation), line) in STATE.iter().zip(lines) {
            if std::fs::symlink_metadata(self.workspace.root().join(line)).is_ok() {
                return Ok(Some(*operation));
            }
        }
        Ok(None)
    }

    /// Adds `in_progress` (and how to leave it) to a status result.
    async fn disclose_in_progress(&self, result: &mut serde_json::Value) {
        let Ok(Some(operation)) = self.in_progress().await else {
            return;
        };
        result["in_progress"] = serde_json::json!(operation);
        result["in_progress_note"] = serde_json::json!(in_progress_note(operation));
    }

    async fn head_id(&self) -> Option<String> {
        let output = self
            .run(
                "resolve-resulting-head",
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    "HEAD".into(),
                ],
                QUERY_LIMITS,
                false,
            )
            .await
            .ok()?;
        let id = String::from_utf8(output.stdout).ok()?;
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_string())
    }

    /// [`Self::configured_filter_drivers`], skipped while the repository config
    /// files are byte-identical to those of the last successful probe. The
    /// production runner's global/system configuration is a frozen snapshot;
    /// unguarded runners (tests) re-probe nothing extra because their globals
    /// are fixed for the test's duration.
    async fn cached_filter_drivers(&self) -> Result<FilterDrivers, ToolError> {
        let root = self.workspace.root().to_path_buf();
        let key = crate::agent::runner::spawn_blocking_scoped(move || config_fingerprint(&root))
            .await
            .ok()
            .flatten();
        if let Some(key) = &key
            && let Some((cached_key, drivers)) = self
                .filter_drivers
                .entry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            && cached_key == key
        {
            return Ok(drivers.clone());
        }
        #[cfg(test)]
        self.filter_drivers
            .probes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let drivers = self.configured_filter_drivers().await?;
        *self
            .filter_drivers
            .entry
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = key.map(|key| (key, drivers.clone()));
        Ok(drivers)
    }

    /// Names of the filter drivers the contained Git would see configured.
    async fn configured_filter_drivers(&self) -> Result<FilterDrivers, ToolError> {
        let output = self
            .run(
                "list-filter-drivers",
                vec![
                    "config".into(),
                    "-z".into(),
                    "--get-regexp".into(),
                    r"^filter\.".into(),
                ],
                QUERY_LIMITS,
                true,
            )
            .await?;
        if output.status != CommandStatus::Completed {
            return Err(ToolError::Msg(
                "could not enumerate Git filter drivers".to_string(),
            ));
        }
        match output.exit_status.and_then(|status| status.code()) {
            Some(0) => parse_filter_drivers(&output.stdout),
            // `git config --get-regexp` exits 1 when no key matches.
            Some(1) => Ok(FilterDrivers::default()),
            _ => Err(ToolError::Msg(
                "could not enumerate Git filter drivers".to_string(),
            )),
        }
    }

    /// Keeps workspace-defined clean filters from running during commands
    /// that read working-tree content through Git's conversion layer
    /// (`status`, worktree `diff`, and the `stage`/`unstage`/`commit`
    /// commands and their before/after snapshots, which refresh the index).
    ///
    /// A `filter.<driver>.clean` or `.process` command in repository config
    /// runs for every stat-dirty index path whose `filter` attribute names
    /// that driver, and the model can write both the config and the
    /// attributes. When any driver is configured, the operation is refused
    /// with a closed error if an index path is currently bound to one; the
    /// returned `-c` prefix, which callers prepend to every Git command of
    /// the operation, additionally empties each configured driver so an
    /// attribute written after this probe still executes nothing.
    async fn filter_guard(&self, operation: &'static str) -> Result<Vec<String>, ToolError> {
        self.filter_guard_lfs(operation, false)
            .await
            .map(|(guard, _)| guard)
    }

    /// [`Self::filter_guard`] for operations that can tolerate Git LFS: when the
    /// only bound driver is the stock `lfs` one, the operation proceeds with
    /// every driver neutralised and the LFS-bound index paths are returned (the
    /// caller discloses them and refuses to write them). Operations that
    /// rewrite working-tree content pass `allow_lfs = false`: neutralised
    /// smudge would replace real files with pointer text.
    async fn filter_guard_lfs(
        &self,
        operation: &'static str,
        allow_lfs: bool,
    ) -> Result<(Vec<String>, Vec<String>), ToolError> {
        let drivers = self.cached_filter_drivers().await?;
        if drivers.names.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let guard = neutralizing_args(&drivers.names);
        let listed = self
            .run(
                "list-index-paths",
                guarded(
                    &guard,
                    vec!["ls-files".into(), "-z".into(), "--cached".into()],
                ),
                FILTER_PROBE_LIMITS,
                false,
            )
            .await?;
        if listed.stdout.is_empty() {
            return Ok((guard, Vec::new()));
        }
        let output = self
            .run_with_input(
                "check-filter-attributes",
                guarded(
                    &guard,
                    vec![
                        "check-attr".into(),
                        "--stdin".into(),
                        "-z".into(),
                        "filter".into(),
                    ],
                ),
                listed.stdout,
                FILTER_PROBE_LIMITS,
            )
            .await?;
        let output = command_result("check-filter-attributes", FILTER_PROBE_LIMITS, output)
            .map_err(ToolError::from)?;
        let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        // Every record is `path NUL attribute NUL value NUL`, so the split
        // leaves one empty trailing field.
        if fields.len() % 3 != 1 || fields.last().is_some_and(|field| !field.is_empty()) {
            return Err(ToolError::Msg(
                "git check-attr output had unexpected field count".to_string(),
            ));
        }
        let mut lfs_paths = Vec::new();
        for record in fields.chunks_exact(3) {
            let value = String::from_utf8_lossy(record[2]);
            if let Some(driver) = drivers.names.iter().find(|driver| driver.as_str() == value) {
                if driver == "lfs" && drivers.lfs_canonical {
                    if !allow_lfs {
                        return Err(lfs_refusal(operation, &String::from_utf8_lossy(record[0])));
                    }
                    if lfs_paths.len() < LFS_PATH_CAP {
                        lfs_paths.push(String::from_utf8_lossy(record[0]).into_owned());
                    }
                    continue;
                }
                return Err(ToolError::Msg(format!(
                    "Git {operation} refused: a tracked path has a `filter` attribute bound to \
                     the repository-configured filter driver `{driver}`, and refreshing the index \
                     would execute that driver's command. Remove the `filter.{driver}` \
                     configuration or the attribute to use this operation."
                )));
            }
        }
        Ok((guard, lfs_paths))
    }

    /// The members of `paths` whose `filter` attribute names `lfs`, whether or
    /// not a driver is configured: staging or committing such a path through
    /// this tool would store the raw file where Git LFS keeps a pointer.
    async fn lfs_bound_paths(&self, paths: &[String]) -> Result<Vec<String>, ToolError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        let output = self
            .run_with_input(
                "check-lfs-attributes",
                vec![
                    "check-attr".into(),
                    "-z".into(),
                    "filter".into(),
                    "--stdin".into(),
                ],
                input,
                PAGED_TEXT_LIMITS,
            )
            .await?;
        let output = command_result("check-lfs-attributes", PAGED_TEXT_LIMITS, output)
            .map_err(ToolError::from)?;
        let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        Ok(fields
            .chunks_exact(3)
            .filter(|record| record[2] == b"lfs")
            .map(|record| String::from_utf8_lossy(record[0]).into_owned())
            .collect())
    }

    async fn ensure_no_external_filters(&self, paths: &[String]) -> Result<(), ToolError> {
        // Paths travel on stdin: a large directory expands to more operands
        // than argv can carry.
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        let output = self
            .run_with_input(
                "check-attributes",
                vec![
                    "check-attr".into(),
                    "-z".into(),
                    "--all".into(),
                    "--stdin".into(),
                ],
                input,
                PAGED_TEXT_LIMITS,
            )
            .await?;
        let output = command_result("check-attributes", PAGED_TEXT_LIMITS, output)
            .map_err(ToolError::from)?;
        let fields = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| String::from_utf8_lossy(field).into_owned())
            .collect::<Vec<_>>();
        if fields.len() % 3 != 0 {
            return Err(ToolError::Msg(
                "git check-attr output had unexpected field count".to_string(),
            ));
        }
        for triple in fields.chunks_exact(3) {
            let attribute = triple[1].as_str();
            let value = triple[2].as_str();
            if matches!(attribute, "filter" | "working-tree-encoding")
                && !matches!(value, "unspecified" | "unset")
            {
                return Err(ToolError::Msg(
                    "Git staging rejected a path with an external transform attribute".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn expand_stage_paths(&self, paths: &[String]) -> Result<Vec<String>, ToolError> {
        let mut args = vec![
            "ls-files".into(),
            "-z".into(),
            "--cached".into(),
            "--others".into(),
            "--exclude-standard".into(),
            "--".into(),
        ];
        args.extend(paths.iter().cloned());
        let output = self
            .run("expand-stage-paths", args, PAGED_TEXT_LIMITS, false)
            .await
            .map_err(|error| match error {
                // Only an output overflow is fixed by staging less at once; a
                // timeout, cancellation or repository error is not.
                GitRunError::OutputLimit { .. } => ToolError::Msg(format!(
                    "{error}; the selection is too large to stage at once. Stage a narrower \
                         set of subdirectories in separate calls."
                )),
                other => other.into(),
            })?;
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect())
    }

    async fn stage(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        require_paths(&args.paths, "stage")?;
        let paths = self.validate_paths(&args.paths, Some("git/stage")).await?;
        let _mutation = self.acquire_mutation().await?;
        let (guard, lfs_paths) = self.filter_guard_lfs("stage", true).await?;
        let before = self.status_snapshot(&guard).await?;
        self.reject_recursive_paths(&paths)?;
        let expanded_paths = self.expand_stage_paths(&paths).await?;
        if self.exact_files && expanded_paths.iter().any(|path| !paths.contains(path)) {
            return Err(ToolError::Msg(
                "Git selection expanded beyond frozen file operands".into(),
            ));
        }
        if let Some(path) = self.lfs_bound_paths(&expanded_paths).await?.first() {
            return Err(ToolError::Msg(format!(
                "Git stage refused: `{path}` is tracked by Git LFS and this tool never runs the LFS filters, so staging it would store the raw file instead of an LFS pointer. Stage LFS-tracked paths from the shell (`git add`), and stage the other paths here."
            )));
        }
        self.ensure_no_external_filters(&expanded_paths).await?;
        let mut command = if self.exact_files {
            vec![
                "update-index".into(),
                "--add".into(),
                "--remove".into(),
                "--".into(),
            ]
        } else {
            vec!["add".into(), "--".into()]
        };
        command.extend(paths);
        let output = self.run_mutation("stage", guarded(&guard, command)).await?;
        let after = self.status_snapshot(&guard).await?;
        let mut result = render_mutation_result("stage", None, before, after, output);
        disclose_lfs(&mut result, &lfs_paths);
        Ok(result)
    }

    async fn unstage(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        require_paths(&args.paths, "unstage")?;
        let paths = self
            .validate_paths(&args.paths, Some("git/unstage"))
            .await?;
        let _mutation = self.acquire_mutation().await?;
        let (guard, lfs_paths) = self.filter_guard_lfs("unstage", true).await?;
        let before = self.status_snapshot(&guard).await?;
        let head = self
            .run(
                "resolve-head",
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    "HEAD".into(),
                ],
                QUERY_LIMITS,
                true,
            )
            .await?;
        let mut command = if head.exit_status.is_some_and(|status| status.success()) {
            vec!["restore".into(), "--staged".into(), "--".into()]
        } else {
            vec![
                "rm".into(),
                "--cached".into(),
                "-r".into(),
                "--ignore-unmatch".into(),
                "--".into(),
            ]
        };
        command.extend(paths);
        let output = self
            .run_mutation("unstage", guarded(&guard, command))
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let mut result = render_mutation_result("unstage", None, before, after, output);
        disclose_lfs(&mut result, &lfs_paths);
        Ok(result)
    }

    async fn commit(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, true, false, false)?;
        // An explicit selection commits only those literal operands, even if
        // another writer stages an unrelated path after a caller's preview.
        // The ordinary empty-path operation still commits the existing index.
        let paths = self.validate_paths(&args.paths, Some("write")).await?;
        let message = args
            .message
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ToolError::Msg("commit requires a non-empty message".to_string()))?;
        if message.len() > 16 * 1024 || message.contains('\0') {
            return Err(ToolError::Msg(
                "commit message exceeds 16 KiB or contains NUL".to_string(),
            ));
        }
        let amend = args.amend.unwrap_or(false);
        let mut coaching = self.permission("git/commit", message).await?;
        if amend {
            // Rewriting history is its own capability with its own key.
            let amend_coaching = self.permission("git/amend", message).await?;
            coaching = coaching.or(amend_coaching);
        }
        let _mutation = self.acquire_mutation().await?;
        // `git commit` itself refreshes the index before writing the tree.
        let (guard, lfs_paths) = self.filter_guard_lfs("commit", true).await?;
        if let Some(operation) = self.in_progress().await?
            && matches!(operation, "rebase" | "cherry-pick")
            && !args.allow_in_progress.unwrap_or(false)
        {
            return Err(ToolError::Msg(format!(
                "commit refused: a {operation} is in progress, and a commit made now would land in the middle of it. This tool cannot continue or abort a {operation}; finish it in the shell, or pass allow_in_progress=true to commit anyway."
            )));
        }
        let before = self.status_snapshot(&guard).await?;
        self.reject_recursive_paths(&paths)?;
        if !paths.is_empty() {
            let expanded = self.expand_stage_paths(&paths).await?;
            if let Some(path) = self.lfs_bound_paths(&expanded).await?.first() {
                return Err(ToolError::Msg(format!(
                    "Git commit refused: `{path}` is tracked by Git LFS and this tool never runs the LFS filters, so committing it by path would store the raw file instead of an LFS pointer. Stage and commit LFS-tracked paths from the shell."
                )));
            }
        }
        if amend {
            self.ensure_amendable().await?;
        }
        let mut command = vec![
            "commit".into(),
            "--file=-".into(),
            "--cleanup=verbatim".into(),
        ];
        if amend {
            command.push("--amend".into());
        }
        if args.signoff.unwrap_or(false) {
            command.push("--signoff".into());
        }
        if args.allow_empty.unwrap_or(false) {
            command.push("--allow-empty".into());
        }
        if !paths.is_empty() {
            command.push("--only".into());
            command.push("--".into());
            command.extend(paths);
        }
        let output = self
            .run_with_input(
                "commit",
                guarded(&guard, command),
                message.as_bytes().to_vec(),
                LOCAL_MUTATION_LIMITS,
            )
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let commit_id = if output.status == CommandStatus::Completed
            && output.exit_status.is_some_and(|status| status.success())
        {
            // The status header already names the resulting HEAD; only fall
            // back to a separate rev-parse when it does not.
            match head_from_snapshot(&after) {
                Some(id) => Some(id),
                None => self.head_id().await,
            }
        } else {
            None
        };
        let mut result = render_mutation_result("commit", coaching, before, after, output);
        result["commit_id"] = serde_json::json!(commit_id);
        result["amended"] = serde_json::json!(amend);
        disclose_lfs(&mut result, &lfs_paths);
        mark_hooks_and_signing_skipped(&mut result);
        Ok(result)
    }

    /// Amend is allowed only for a local-only HEAD commit (see
    /// [`Self::ensure_head_unpublished`]), so shared history is never rewritten.
    async fn ensure_amendable(&self) -> Result<(), ToolError> {
        self.ensure_head_unpublished("amend", "amending", "Create a new commit instead.")
            .await
    }

    /// History-rewriting operations (`amend`, `reset`) apply only to a local-only
    /// HEAD: one that exists and is not reachable from any remote-tracking ref.
    async fn ensure_head_unpublished(
        &self,
        action: &str,
        gerund: &str,
        advice: &str,
    ) -> Result<(), ToolError> {
        let head = self
            .run(
                "resolve-rewrite-head",
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    "HEAD".into(),
                ],
                QUERY_LIMITS,
                true,
            )
            .await?;
        if !head.exit_status.is_some_and(|status| status.success()) {
            return Err(ToolError::Msg(format!(
                "{action} refused: there is no commit to {action}"
            )));
        }
        let remote = self
            .run(
                "check-head-pushed",
                vec![
                    "for-each-ref".into(),
                    "--contains=HEAD".into(),
                    "--count=1".into(),
                    "--format=%(refname)".into(),
                    "refs/remotes".into(),
                ],
                QUERY_LIMITS,
                false,
            )
            .await?;
        let published = String::from_utf8_lossy(&remote.stdout).trim().to_string();
        if !published.is_empty() {
            return Err(ToolError::Msg(format!(
                "{action} refused: HEAD is already contained in the remote-tracking ref {published}; \
                 {gerund} would rewrite published history. {advice}"
            )));
        }
        Ok(())
    }

    /// Validate a new or existing branch/tag name with Git's own rules.
    async fn validate_ref_name(&self, name: &str, kind: &'static str) -> Result<(), ToolError> {
        if name.is_empty() || name.starts_with('-') || name.contains('\0') || name.len() > 255 {
            return Err(ToolError::Msg(format!("invalid Git {kind} name")));
        }
        let candidate = match kind {
            "tag" => format!("refs/tags/{name}"),
            _ => format!("refs/heads/{name}"),
        };
        self.run(
            "check-ref-format",
            vec!["check-ref-format".into(), candidate],
            QUERY_LIMITS,
            false,
        )
        .await
        .map(|_| ())
        .map_err(|_| ToolError::Msg(format!("invalid Git {kind} name")))
    }

    fn required_name<'a>(args: &'a GitArgs, operation: &str) -> Result<&'a str, ToolError> {
        args.name
            .as_deref()
            .ok_or_else(|| ToolError::Msg(format!("{operation} requires a name")))
    }

    async fn branches(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, true)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg("branches takes no paths".to_string()));
        }
        let scope = args.scope.unwrap_or_default();
        let timeout = query_timeout(&args)?;
        let max = args
            .max_count
            .unwrap_or(REF_PAGE_DEFAULT)
            .clamp(1, REF_PAGE_MAX) as usize;
        let skip = args.skip.unwrap_or(0) as usize;
        let identity = match scope {
            RefScope::Local => "branches".to_string(),
            RefScope::Remote => "branches remote".to_string(),
            RefScope::All => "branches all".to_string(),
            RefScope::Tags => "branches tags".to_string(),
        };
        let coaching = self.permission("git/status", &identity).await?;
        let namespaces: &[&str] = match scope {
            RefScope::Local => &["refs/heads"],
            RefScope::Remote => &["refs/remotes"],
            RefScope::All => &["refs/heads", "refs/remotes"],
            RefScope::Tags => &["refs/tags"],
        };
        let mut command = vec![
            "for-each-ref".into(),
            // One more than the page tells whether anything follows it.
            format!("--count={}", skip.saturating_add(max).saturating_add(1)),
            "--format=%(HEAD)%00%(refname)%00%(objectname)%00%(upstream:short)%00%(contents:subject)%00%(objecttype)%00%(*objectname)%00%(symref)".into(),
        ];
        command.extend(namespaces.iter().map(|ns| (*ns).to_string()));
        let output = self
            .run_query("branches", command, ref_limits(timeout))
            .await?;
        let timed_out = output.status == CommandStatus::TimedOut;
        let limit_hit = matches!(output.status, CommandStatus::OutputLimitExceeded(_));
        let mut stdout: &[u8] = &output.stdout;
        if timed_out || limit_hit {
            // Keep only complete lines.
            stdout = &stdout[..stdout
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |i| i + 1)];
        }
        let entries = String::from_utf8_lossy(stdout)
            .lines()
            .filter_map(|line| parse_ref_line(line, scope))
            .collect::<Vec<_>>();
        let more = entries.len() > skip.saturating_add(max);
        let page: Vec<serde_json::Value> = entries.into_iter().skip(skip).take(max).collect();
        let truncated = more || timed_out || limit_hit;
        let next_skip = (more || ((timed_out || limit_hit) && !page.is_empty()))
            .then(|| (skip + page.len()) as u64);
        let key = if scope == RefScope::Tags {
            "tags"
        } else {
            "branches"
        };
        let mut value = serde_json::json!({
            "operation": "branches",
            "scope": match scope {
                RefScope::Local => "local",
                RefScope::Remote => "remote",
                RefScope::All => "all",
                RefScope::Tags => "tags",
            },
            key: page,
            "skip": skip,
            "next_skip": next_skip,
            "truncated": truncated,
            "coaching": coaching,
        });
        if timed_out {
            value["timed_out"] = serde_json::json!(true);
            value["note"] = serde_json::json!(format!(
                "git branches timed out after {}s; the refs listed so far are shown. Retry with a larger timeout_secs (at most {QUERY_TIMEOUT_MAX_SECS}) or continue with skip.",
                timeout.as_secs()
            ));
        } else if limit_hit {
            value["note"] = serde_json::json!(
                "git branches output reached its size cap; continue with skip (next_skip)."
            );
        } else if more {
            value["note"] = serde_json::json!(
                "more refs follow; pass skip=next_skip (and optionally max_count up to 1000) to continue."
            );
        }
        Ok(value)
    }

    async fn branch_create(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, true, false)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg("branch_create takes no paths".to_string()));
        }
        let name = Self::required_name(&args, "branch_create")?;
        self.validate_ref_name(name, "branch").await?;
        if let Some(revision) = args.revision.as_deref() {
            self.validate_revision(revision).await?;
        }
        let identity = match args.revision.as_deref() {
            Some(revision) => format!("{name} {revision}"),
            None => name.to_string(),
        };
        let coaching = self.permission("git/branch", &identity).await?;
        let _mutation = self.acquire_mutation().await?;
        let mut command = vec!["branch".into(), "--no-track".into(), name.to_string()];
        if let Some(revision) = args.revision {
            command.push(revision);
        }
        let output = self.run_mutation("branch", command).await?;
        Ok(render_ref_result("branch_create", coaching, output))
    }

    async fn switch(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg("switch takes no paths".to_string()));
        }
        let name = Self::required_name(&args, "switch")?;
        self.validate_ref_name(name, "branch").await?;
        let coaching = self.permission("git/switch", name).await?;
        let _mutation = self.acquire_mutation().await?;
        // Updating the working tree runs checkout conversion (smudge).
        let guard = self.filter_guard("switch").await?;
        let before = self.status_snapshot(&guard).await?;
        let output = self
            .run_mutation(
                "switch",
                guarded(
                    &guard,
                    vec![
                        "switch".into(),
                        "--no-guess".into(),
                        "--no-recurse-submodules".into(),
                        name.to_string(),
                    ],
                ),
            )
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let (changed_before, changed_after) = (before.clone(), after.clone());
        let mut result = render_mutation_result("switch", coaching, before, after, output);
        Self::note_uncaptured_worktree_change(&changed_before, &changed_after, &mut result);
        Ok(result)
    }

    async fn stash(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        let operation = args.operation;
        let push = operation == GitOperation::StashPush;
        reject_irrelevant_fields(&args, push, false, false)?;
        if operation == GitOperation::StashList {
            if !args.paths.is_empty() {
                return Err(ToolError::Msg("stash_list takes no paths".to_string()));
            }
            let coaching = self.permission("git/status", "stash_list").await?;
            let output = self
                .run(
                    "stash-list",
                    vec![
                        "stash".into(),
                        "list".into(),
                        "--format=%gd%x00%H%x00%gs".into(),
                    ],
                    with_timeout(QUERY_LIMITS, query_timeout(&args)?),
                    false,
                )
                .await
                .map_err(timeout_guidance)?;
            let entries = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    let fields: Vec<&str> = line.split('\0').collect();
                    (fields.len() == 3).then(|| {
                        serde_json::json!({"ref": fields[0], "id": fields[1], "subject": fields[2]})
                    })
                })
                .collect::<Vec<_>>();
            return Ok(serde_json::json!({
                "operation": "stash_list",
                "stashes": entries,
                "coaching": coaching,
            }));
        }
        let paths = if push {
            self.validate_paths(&args.paths, Some("write")).await?
        } else if args.paths.is_empty() {
            Vec::new()
        } else {
            return Err(ToolError::Msg("stash_pop takes no paths".to_string()));
        };
        let message = args.message.as_deref().unwrap_or("");
        if message.len() > 16 * 1024 || message.contains('\0') {
            return Err(ToolError::Msg(
                "stash message exceeds 16 KiB or contains NUL".to_string(),
            ));
        }
        let (label, identity) = if push {
            ("stash_push", format!("push {message}"))
        } else {
            ("stash_pop", "pop".to_string())
        };
        let coaching = self.permission("git/stash", &identity).await?;
        let _mutation = self.acquire_mutation().await?;
        let guard = self.filter_guard(label).await?;
        let before = self.status_snapshot(&guard).await?;
        let mut command = vec!["stash".into()];
        if push {
            if args.include_untracked.unwrap_or(false) {
                if !paths.is_empty() {
                    return Err(ToolError::Msg(
                        "include_untracked cannot be combined with paths".to_string(),
                    ));
                }
                // Git's untracked-file cleanup breaks under literal pathspecs.
                command.insert(0, "--no-literal-pathspecs".into());
            }
            command.push("push".into());
            if args.include_untracked.unwrap_or(false) {
                command.push("--include-untracked".into());
            }
            if !message.is_empty() {
                command.push("--message".into());
                command.push(message.to_string());
            }
            if !paths.is_empty() {
                command.push("--".into());
                command.extend(paths);
            }
        } else {
            command.push("pop".into());
        }
        // A stash is a commit: it needs the host-resolved identity.
        let output = self
            .run_with_input(
                "stash",
                guarded(&guard, command),
                Vec::new(),
                LOCAL_MUTATION_LIMITS,
            )
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let (changed_before, changed_after) = (before.clone(), after.clone());
        let mut result = render_mutation_result(label, coaching, before, after, output);
        Self::note_uncaptured_worktree_change(&changed_before, &changed_after, &mut result);
        Ok(result)
    }

    async fn tag(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, true, true, false)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg("tag takes no paths".to_string()));
        }
        let name = Self::required_name(&args, "tag")?;
        self.validate_ref_name(name, "tag").await?;
        if let Some(revision) = args.revision.as_deref() {
            self.validate_revision(revision).await?;
        }
        let message = args
            .message
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        if message.is_some_and(|message| message.len() > 16 * 1024 || message.contains('\0')) {
            return Err(ToolError::Msg(
                "tag message exceeds 16 KiB or contains NUL".to_string(),
            ));
        }
        let identity = format!(
            "{name}{}",
            args.revision
                .as_deref()
                .map(|revision| format!(" {revision}"))
                .unwrap_or_default()
        );
        let coaching = self.permission("git/tag", &identity).await?;
        let _mutation = self.acquire_mutation().await?;
        let mut command = vec!["tag".into()];
        if message.is_some() {
            command.push("--annotate".into());
            command.push("--file=-".into());
            command.push("--cleanup=verbatim".into());
        }
        command.push(name.to_string());
        if let Some(revision) = args.revision.clone() {
            command.push(revision);
        }
        let output = match message {
            Some(message) => {
                self.run_with_input(
                    "tag",
                    command,
                    message.as_bytes().to_vec(),
                    LOCAL_MUTATION_LIMITS,
                )
                .await?
            }
            None => self.run_mutation("tag", command).await?,
        };
        let mut result = render_ref_result("tag", coaching, output);
        mark_hooks_and_signing_skipped(&mut result);
        Ok(result)
    }

    async fn restore(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, true, false)?;
        require_paths(&args.paths, "restore")?;
        let paths = self.validate_paths(&args.paths, Some("write")).await?;
        if let Some(revision) = args.revision.as_deref() {
            self.validate_revision(revision).await?;
        }
        let identity = serde_json::to_string(&serde_json::json!({
            "revision": args.revision,
            "paths": paths,
        }))?;
        let coaching = self.permission("git/restore", &identity).await?;
        let _mutation = self.acquire_mutation().await?;
        let guard = self.filter_guard("restore").await?;
        let before = self.status_snapshot(&guard).await?;
        self.reject_recursive_paths(&paths)?;
        let captured = self
            .capture_restore_preimages(&paths, args.revision.as_deref())
            .await?;
        let mut command = vec!["restore".into(), "--worktree".into()];
        if let Some(revision) = args.revision {
            command.push(format!("--source={revision}"));
        }
        command.push("--".into());
        command.extend(paths);
        let output = self
            .run_mutation("restore", guarded(&guard, command))
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let mut result = render_mutation_result("restore", coaching, before, after, output);
        self.commit_restore_checkpoints(captured, &mut result).await;
        Ok(result)
    }

    /// Reads the current bytes of every file `restore` may rewrite and records
    /// them as file-checkpoint preimages, so `/rewind` can bring back
    /// uncommitted content the restore discards. Returns `None` entries only
    /// when the selection is too large to checkpoint.
    async fn capture_restore_preimages(
        &self,
        paths: &[String],
        revision: Option<&str>,
    ) -> Result<RestoreCapture, ToolError> {
        if crate::session::file_checkpoints::current().is_none() {
            return Ok(RestoreCapture::default());
        }
        let mut files = std::collections::BTreeSet::new();
        let mut listings = vec![{
            let mut command = vec![
                "ls-files".to_string(),
                "-z".into(),
                "--cached".into(),
                "--".into(),
            ];
            command.extend(paths.iter().cloned());
            command
        }];
        if let Some(revision) = revision {
            let mut command = vec![
                "ls-tree".to_string(),
                "-r".into(),
                "-z".into(),
                "--name-only".into(),
                revision.to_string(),
                "--".into(),
            ];
            command.extend(paths.iter().cloned());
            listings.push(command);
        }
        for command in listings {
            let output = self
                .run("list-restore-targets", command, PAGED_TEXT_LIMITS, false)
                .await?;
            files.extend(
                output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| String::from_utf8_lossy(path).into_owned()),
            );
        }
        if files.len() > RESTORE_CHECKPOINT_MAX_FILES {
            crate::session::file_checkpoints::skipped_git();
            return Ok(RestoreCapture::default());
        }
        let workspace = Arc::clone(&self.workspace);
        let listed: Vec<String> = files.into_iter().collect();
        let read = crate::agent::runner::spawn_blocking_scoped(move || {
            listed
                .into_iter()
                .map(|file| {
                    let current = read_workspace_file(&workspace, Path::new(&file));
                    (file, current)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|_| ToolError::Msg("restore checkpoint capture failed".to_string()))?;
        let mut entries = Vec::new();
        for (file, current) in read {
            let before = match current {
                WorkspaceFile::Bytes(bytes) => Some(bytes),
                WorkspaceFile::Missing => None,
                WorkspaceFile::Unreadable => {
                    crate::session::file_checkpoints::skipped_git();
                    continue;
                }
            };
            let allowed =
                crate::agent::tools::mutation_diff::preimage_allowed(&self.permission, &file, true);
            let digest = before
                .as_ref()
                .map(|bytes| crate::fs::ContentDigest::of(bytes));
            let preimage = match (&before, &digest) {
                (Some(bytes), Some(digest)) => {
                    Some(crate::session::file_checkpoints::Preimage::new(
                        Arc::<[u8]>::from(bytes.as_slice()),
                        digest,
                    ))
                }
                _ => None,
            };
            let pending = crate::session::file_checkpoints::prepare(
                &self.workspace,
                Path::new(&file),
                preimage,
                allowed,
            )
            .await;
            if let Some(pending) = pending {
                entries.push((file, digest, pending));
            }
        }
        Ok(RestoreCapture { entries })
    }

    /// Completes the captures taken before `restore`: each file whose bytes
    /// changed gets its postimage recorded.
    async fn commit_restore_checkpoints(
        &self,
        captured: RestoreCapture,
        result: &mut serde_json::Value,
    ) {
        if captured.entries.is_empty() {
            return;
        }
        let workspace = Arc::clone(&self.workspace);
        let committed = crate::agent::runner::spawn_blocking_scoped(move || {
            captured
                .entries
                .into_iter()
                .map(|(file, before, pending)| {
                    let after = read_workspace_file(&workspace, Path::new(&file));
                    (before, after, pending)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let Ok(committed) = committed else {
            crate::session::file_checkpoints::skipped_git();
            return;
        };
        crate::session::file_checkpoints::published();
        for (before, after, mut pending) in committed {
            match after {
                WorkspaceFile::Bytes(bytes) => {
                    let digest = crate::fs::ContentDigest::of(&bytes);
                    if before.as_ref() != Some(&digest) {
                        pending.committed(&digest, bytes.len());
                    }
                }
                // The restore removed the file or it cannot be read back.
                _ => {
                    if before.is_some() {
                        crate::session::file_checkpoints::skipped_git();
                    }
                }
            }
        }
        attach_checkpoint_notice(result);
    }

    /// Marks a working-tree-rewriting operation whose files cannot be
    /// individually checkpointed, when the before/after snapshots show it
    /// changed anything.
    fn note_uncaptured_worktree_change(
        before: &serde_json::Value,
        after: &serde_json::Value,
        result: &mut serde_json::Value,
    ) {
        if !worktree_may_have_changed(before, after) {
            return;
        }
        crate::session::file_checkpoints::skipped_git();
        crate::session::file_checkpoints::external_mutation();
        attach_checkpoint_notice(result);
    }

    async fn history_operation(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        let operation = args.operation;
        let label = operation.name();
        let merge = matches!(operation, GitOperation::Merge | GitOperation::MergeAbort);
        let is_abort = matches!(
            operation,
            GitOperation::MergeAbort | GitOperation::CherryPickAbort | GitOperation::RevertAbort
        );
        if !args.paths.is_empty() {
            return Err(ToolError::Msg(format!("{label} takes no paths")));
        }
        reject_irrelevant_fields(&args, operation == GitOperation::Merge, !is_abort, false)?;
        let (key, family) = match operation {
            GitOperation::Merge | GitOperation::MergeAbort => ("git/merge", "merge"),
            GitOperation::CherryPick | GitOperation::CherryPickAbort => {
                ("git/cherry_pick", "cherry-pick")
            }
            _ => ("git/revert", "revert"),
        };
        let mut command = vec![family.to_string()];
        let identity;
        if is_abort {
            command.push("--abort".into());
            identity = "abort".to_string();
        } else {
            let revision = args
                .revision
                .as_deref()
                .ok_or_else(|| ToolError::Msg(format!("{label} requires a revision")))?;
            if !merge && revision.contains("..") {
                return Err(ToolError::Msg(format!(
                    "{label} takes a single commit, not a range"
                )));
            }
            self.validate_revision(revision).await?;
            command.push("--no-edit".into());
            if merge && let Some(message) = args.message.as_deref() {
                if message.trim().is_empty() || message.len() > 16 * 1024 || message.contains('\0')
                {
                    return Err(ToolError::Msg(
                        "merge message must be non-empty, at most 16 KiB and free of NUL"
                            .to_string(),
                    ));
                }
                command.push("-m".into());
                command.push(message.to_string());
            }
            if merge {
                command.push("--no-stat".into());
            }
            command.push(revision.to_string());
            identity = revision.to_string();
        }
        let coaching = self.permission(key, &identity).await?;
        let _mutation = self.acquire_mutation().await?;
        let guard = self.filter_guard(label).await?;
        let state = self.in_progress().await?;
        if is_abort {
            if state != Some(family) {
                return Err(ToolError::Msg(format!(
                    "{label} refused: no {family} is in progress"
                )));
            }
        } else if let Some(state) = state {
            return Err(ToolError::Msg(format!(
                "{label} refused: a {state} is already in progress; finish or abort it first"
            )));
        }
        let before = self.status_snapshot(&guard).await?;
        // The result is a commit (merge, cherry-pick, revert), which needs the
        // host-resolved identity.
        let output = self
            .run_with_input(
                label,
                guarded(&guard, command),
                Vec::new(),
                LOCAL_MUTATION_LIMITS,
            )
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let conflicted = conflicted_paths(&after);
        let mut result =
            render_mutation_result(label, coaching, before.clone(), after.clone(), output);
        self.disclose_history_state(&mut result, &conflicted).await;
        Self::note_uncaptured_worktree_change(&before, &after, &mut result);
        mark_hooks_and_signing_skipped(&mut result);
        Ok(result)
    }

    /// Adds the structured state of a history operation: the operation still in
    /// progress (a conflict stopped it) and the conflicted paths.
    async fn disclose_history_state(&self, result: &mut serde_json::Value, conflicted: &[String]) {
        result["conflicted_paths"] =
            serde_json::json!(conflicted.iter().take(100).collect::<Vec<_>>());
        if conflicted.len() > 100 {
            result["conflicted_paths_truncated"] = serde_json::json!(true);
        }
        match self.in_progress().await {
            Ok(Some(operation)) => {
                result["in_progress"] = serde_json::json!(operation);
                result["in_progress_note"] = serde_json::json!(in_progress_note(operation));
            }
            _ => {
                result["in_progress"] = serde_json::Value::Null;
            }
        }
    }

    async fn reset(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, true, false)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg("reset takes no paths".to_string()));
        }
        let revision = args
            .revision
            .as_deref()
            .ok_or_else(|| ToolError::Msg("reset requires a revision".to_string()))?;
        self.validate_revision(revision).await?;
        let mode = args.mode.unwrap_or_default();
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
        };
        let coaching = self
            .permission("git/reset", &format!("{flag} {revision}"))
            .await?;
        let _mutation = self.acquire_mutation().await?;
        let (guard, lfs_paths) = self.filter_guard_lfs("reset", true).await?;
        if let Some(state) = self.in_progress().await? {
            return Err(ToolError::Msg(format!(
                "reset refused: a {state} is in progress; finish or abort it first"
            )));
        }
        let resolved = self
            .run(
                "resolve-reset-target",
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--end-of-options".into(),
                    format!("{revision}^{{commit}}"),
                ],
                QUERY_LIMITS,
                false,
            )
            .await
            .map_err(|_| {
                ToolError::Msg(format!(
                    "reset refused: {} does not name a commit",
                    shown_revision(revision)
                ))
            })?;
        let target = String::from_utf8_lossy(&resolved.stdout).trim().to_string();
        if self.head_id().await.as_deref() != Some(target.as_str()) {
            self.ensure_head_unpublished(
                "reset",
                "resetting",
                "Use revert to undo published commits.",
            )
            .await?;
        }
        let before = self.status_snapshot(&guard).await?;
        let output = self
            .run_mutation(
                "reset",
                guarded(&guard, vec!["reset".into(), flag.into(), target]),
            )
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let mut result = render_mutation_result("reset", coaching, before, after, output);
        result["mode"] = serde_json::json!(match mode {
            ResetMode::Soft => "soft",
            ResetMode::Mixed => "mixed",
        });
        disclose_lfs(&mut result, &lfs_paths);
        Ok(result)
    }

    /// The name of the checked-out branch, or `None` when HEAD is detached.
    async fn current_branch(&self) -> Result<Option<String>, ToolError> {
        let output = self
            .run(
                "current-branch",
                vec![
                    "symbolic-ref".into(),
                    "--quiet".into(),
                    "--short".into(),
                    "HEAD".into(),
                ],
                QUERY_LIMITS,
                true,
            )
            .await?;
        if !output.exit_status.is_some_and(|status| status.success()) {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!name.is_empty()).then_some(name))
    }

    async fn branch_manage(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg(format!(
                "{} takes no paths",
                args.operation.name()
            )));
        }
        let rename = args.operation == GitOperation::BranchRename;
        let label = args.operation.name();
        let name = Self::required_name(&args, label)?;
        self.validate_ref_name(name, "branch").await?;
        let new_name = if rename {
            let new_name = args
                .new_name
                .as_deref()
                .ok_or_else(|| ToolError::Msg("branch_rename requires new_name".to_string()))?;
            self.validate_ref_name(new_name, "branch").await?;
            Some(new_name)
        } else {
            if args.new_name.is_some() {
                return Err(ToolError::Msg(
                    "new_name is supported only by branch_rename".to_string(),
                ));
            }
            None
        };
        let identity = match new_name {
            Some(new_name) => format!("rename {name} {new_name}"),
            None => format!("delete {name}"),
        };
        let coaching = self.permission("git/branch_delete", &identity).await?;
        let _mutation = self.acquire_mutation().await?;
        if self.current_branch().await?.as_deref() == Some(name) {
            return Err(ToolError::Msg(format!(
                "{label} refused: `{name}` is the checked-out branch; switch to another branch first"
            )));
        }
        let exists = self
            .run(
                "check-branch-exists",
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    format!("refs/heads/{name}"),
                ],
                QUERY_LIMITS,
                true,
            )
            .await?;
        if !exists.exit_status.is_some_and(|status| status.success()) {
            return Err(ToolError::Msg(format!(
                "{label} refused: there is no local branch `{name}`"
            )));
        }
        let command = match new_name {
            // `-d` refuses a branch whose commits are not merged anywhere else.
            None => vec!["branch".to_string(), "-d".into(), name.to_string()],
            Some(new_name) => vec![
                "branch".to_string(),
                "-m".into(),
                name.to_string(),
                new_name.to_string(),
            ],
        };
        let output = self.run_mutation("branch", command).await?;
        Ok(render_ref_result(label, coaching, output))
    }

    async fn stash_ref(&self, args: &GitArgs) -> Result<String, ToolError> {
        let index = args.index.unwrap_or(0);
        if index > 1000 {
            return Err(ToolError::Msg(
                "stash index must be at most 1000".to_string(),
            ));
        }
        let listed = self
            .run(
                "stash-list",
                vec!["stash".into(), "list".into(), "--format=%gd".into()],
                QUERY_LIMITS,
                false,
            )
            .await?;
        let count = String::from_utf8_lossy(&listed.stdout).lines().count();
        if (index as usize) >= count {
            return Err(ToolError::Msg(format!(
                "stash {index} does not exist ({count} stash{} saved)",
                if count == 1 { "" } else { "es" }
            )));
        }
        Ok(format!("stash@{{{index}}}"))
    }

    async fn stash_by_index(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg(format!(
                "{} takes no paths",
                args.operation.name()
            )));
        }
        let apply = args.operation == GitOperation::StashApply;
        let label = args.operation.name();
        let index = args.index.unwrap_or(0);
        let identity = format!("{} {index}", if apply { "apply" } else { "drop" });
        let coaching = self.permission("git/stash", &identity).await?;
        let _mutation = self.acquire_mutation().await?;
        let stash = self.stash_ref(&args).await?;
        if !apply {
            let output = self
                .run_mutation("stash", vec!["stash".into(), "drop".into(), stash])
                .await?;
            return Ok(render_ref_result(label, coaching, output));
        }
        let guard = self.filter_guard(label).await?;
        let before = self.status_snapshot(&guard).await?;
        let output = self
            .run_with_input(
                "stash",
                guarded(&guard, vec!["stash".into(), "apply".into(), stash]),
                Vec::new(),
                LOCAL_MUTATION_LIMITS,
            )
            .await?;
        let after = self.status_snapshot(&guard).await?;
        let conflicted = conflicted_paths(&after);
        let mut result =
            render_mutation_result(label, coaching, before.clone(), after.clone(), output);
        result["conflicted_paths"] =
            serde_json::json!(conflicted.iter().take(100).collect::<Vec<_>>());
        Self::note_uncaptured_worktree_change(&before, &after, &mut result);
        Ok(result)
    }

    async fn read_operation(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        if self.read_only && !args.operation.is_inspection() {
            return Err(ToolError::Msg(format!(
                "git {} is not available to this tool: it is read-only (status, diff, log, show, blame, branches, stash_list)",
                args.operation.name()
            )));
        }
        let pages = matches!(
            args.operation,
            GitOperation::Diff | GitOperation::Show | GitOperation::Blame
        );
        if !pages && (args.format.is_some() || args.offset.is_some()) {
            return Err(ToolError::Msg(
                "format and offset are supported only by diff, show and blame".to_string(),
            ));
        }
        if args.operation != GitOperation::Commit
            && (args.amend.is_some() || args.signoff.is_some() || args.allow_empty.is_some())
        {
            return Err(ToolError::Msg(
                "amend, signoff and allow_empty are supported only by commit".to_string(),
            ));
        }
        let is_diff = args.operation == GitOperation::Diff;
        let is_log = args.operation == GitOperation::Log;
        let is_blame = args.operation == GitOperation::Blame;
        if !is_diff && (args.revision_to.is_some() || args.staged.is_some()) {
            return Err(ToolError::Msg(
                "revision_to and staged are supported only by diff".to_string(),
            ));
        }
        let is_branches = args.operation == GitOperation::Branches;
        if !(is_log || is_branches) && args.skip.is_some() {
            return Err(ToolError::Msg(
                "skip is supported only by log and branches".to_string(),
            ));
        }
        if !is_log && (args.author.is_some() || args.grep.is_some()) {
            return Err(ToolError::Msg(
                "author and grep are supported only by log".to_string(),
            ));
        }
        if !is_branches && args.scope.is_some() {
            return Err(ToolError::Msg(
                "scope is supported only by branches".to_string(),
            ));
        }
        if args.timeout_secs.is_some()
            && !matches!(
                args.operation,
                GitOperation::Log
                    | GitOperation::Blame
                    | GitOperation::Branches
                    | GitOperation::StashList
            )
        {
            return Err(ToolError::Msg(
                "timeout_secs is supported only by log, blame, branches and stash_list".to_string(),
            ));
        }
        if !is_blame && (args.start_line.is_some() || args.end_line.is_some()) {
            return Err(ToolError::Msg(
                "start_line and end_line are supported only by blame".to_string(),
            ));
        }
        let named = matches!(
            args.operation,
            GitOperation::BranchCreate
                | GitOperation::Switch
                | GitOperation::Tag
                | GitOperation::BranchDelete
                | GitOperation::BranchRename
        );
        if !named && args.name.is_some() {
            return Err(ToolError::Msg(
                "name is supported only by branch_create, switch, tag, branch_delete and branch_rename"
                    .to_string(),
            ));
        }
        if args.allow_in_progress.is_some() && args.operation != GitOperation::Commit {
            return Err(ToolError::Msg(
                "allow_in_progress is supported only by commit".to_string(),
            ));
        }
        if args.mode.is_some() && args.operation != GitOperation::Reset {
            return Err(ToolError::Msg(
                "mode is supported only by reset".to_string(),
            ));
        }
        if args.new_name.is_some() && args.operation != GitOperation::BranchRename {
            return Err(ToolError::Msg(
                "new_name is supported only by branch_rename".to_string(),
            ));
        }
        if args.index.is_some()
            && !matches!(
                args.operation,
                GitOperation::StashApply | GitOperation::StashDrop
            )
        {
            return Err(ToolError::Msg(
                "index is supported only by stash_apply and stash_drop".to_string(),
            ));
        }
        if args.include_untracked.is_some() && args.operation != GitOperation::StashPush {
            return Err(ToolError::Msg(
                "include_untracked is supported only by stash_push".to_string(),
            ));
        }
        match args.operation {
            GitOperation::Status => {
                reject_irrelevant_fields(&args, false, false, false)?;
                let paths = self.validate_paths(&args.paths, None).await?;
                let identity = if paths.is_empty() {
                    "workspace".to_string()
                } else {
                    serde_json::to_string(&serde_json::json!({ "paths": paths }))?
                };
                let coaching = self.permission("git/status", &identity).await?;
                let (guard, lfs_paths) = self.filter_guard_lfs("status", true).await?;
                let mut value = self.status_snapshot_for(&guard, &paths).await?;
                disclose_lfs(&mut value, &lfs_paths);
                self.disclose_in_progress(&mut value).await;
                if !self.exact_files {
                    // Lossless path records serve the host-side `/commit`
                    // flow (`selected_files_only`), not the model.
                    if let Some(object) = value.as_object_mut() {
                        object.remove("path_records");
                    }
                }
                self.disclose_submodules(&mut value).await;
                value["operation"] = serde_json::json!("status");
                value["coaching"] = serde_json::json!(coaching);
                Ok(value)
            }
            GitOperation::Diff => {
                reject_irrelevant_fields(&args, false, true, false)?;
                let paths = self.validate_paths(&args.paths, None).await?;
                let staged = args.staged.unwrap_or(false);
                let identity = serde_json::to_string(&serde_json::json!({
                    "revision": args.revision,
                    "revision_to": args.revision_to,
                    "staged": staged,
                    "paths": paths,
                }))?;
                let coaching = self.permission("git/diff", &identity).await?;
                let mut command = vec![
                    "diff".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--ignore-submodules=all".into(),
                ];
                command.extend(format_args(args.format));
                if staged {
                    command.push("--cached".into());
                }
                let mut is_range = false;
                if let Some(revision) = args.revision.as_deref() {
                    is_range = self.validate_revision_or_range(revision).await?;
                    command.push(revision.to_string());
                }
                if let Some(second) = args.revision_to.as_deref() {
                    if args.revision.is_none() || is_range || staged {
                        return Err(ToolError::Msg(
                            "revision_to needs a single `revision` and cannot be combined with a range or staged".to_string(),
                        ));
                    }
                    self.validate_revision(second).await?;
                    command.push(second.to_string());
                }
                if is_range && staged {
                    return Err(ToolError::Msg(
                        "a revision range cannot be combined with staged".to_string(),
                    ));
                }
                command.push("--".into());
                command.extend(paths);
                // Without `--cached` every diff reads the working tree.
                let (guard, lfs_paths) = self.filter_guard_lfs("diff", true).await?;
                let mut result = render_paged_text_result(
                    "diff",
                    coaching,
                    self.run("diff", guarded(&guard, command), PAGED_TEXT_LIMITS, true)
                        .await?,
                    args.offset,
                )?;
                self.disclose_submodules(&mut result).await;
                disclose_lfs(&mut result, &lfs_paths);
                Ok(result)
            }
            GitOperation::Log => {
                reject_irrelevant_fields(&args, false, true, true)?;
                let paths = self.validate_paths(&args.paths, None).await?;
                let count = args.max_count.unwrap_or(20);
                if !(1..=LOG_MAX_COUNT).contains(&count) {
                    return Err(ToolError::Msg(format!(
                        "log max_count must be between 1 and {LOG_MAX_COUNT} (got {count}); page past {LOG_MAX_COUNT} commits with skip/next_skip. branches accepts up to 1000."
                    )));
                }
                let timeout = query_timeout(&args)?;
                if let Some(revision) = args.revision.as_deref() {
                    self.validate_revision_or_range(revision).await?;
                }
                let skip = args.skip.unwrap_or(0);
                for value in [&args.author, &args.grep].into_iter().flatten() {
                    if value.is_empty() || value.contains('\0') || value.len() > 512 {
                        return Err(ToolError::Msg(
                            "author and grep must be 1-512 bytes without NUL".to_string(),
                        ));
                    }
                }
                let identity = serde_json::to_string(&serde_json::json!({
                    "revision": args.revision,
                    "paths": paths,
                    "max_count": count,
                    "skip": skip,
                    "author": args.author,
                    "grep": args.grep,
                }))?;
                let coaching = self.permission("git/log", &identity).await?;
                // Unpredictable record terminator: a commit message cannot forge or split
                // records because it cannot know this value.
                let log_terminator = new_log_terminator();
                let mut command = vec![
                    "log".into(),
                    format!("--max-count={count}"),
                    "--date=iso-strict".into(),
                    log_format(&log_terminator),
                ];
                if skip > 0 {
                    command.push(format!("--skip={skip}"));
                }
                if args.author.is_some() || args.grep.is_some() {
                    command.push("--fixed-strings".into());
                }
                if let Some(author) = args.author.as_deref() {
                    command.push(format!("--author={author}"));
                }
                if let Some(grep) = args.grep.as_deref() {
                    command.push(format!("--grep={grep}"));
                }
                if let Some(revision) = args.revision {
                    command.push(revision);
                }
                command.push("--".into());
                command.extend(paths);
                let output = self
                    .run_query("log", command, with_timeout(TEXT_LIMITS, timeout))
                    .await?;
                render_log_result(&output, skip, count, timeout, coaching, &log_terminator)
            }
            GitOperation::Show => {
                reject_irrelevant_fields(&args, false, true, false)?;
                let revision = args
                    .revision
                    .as_deref()
                    .ok_or_else(|| ToolError::Msg("show requires a revision".to_string()))?;
                self.validate_revision(revision).await?;
                let paths = self.validate_paths(&args.paths, None).await?;
                let identity = serde_json::to_string(&serde_json::json!({
                    "revision": revision,
                    "paths": paths,
                }))?;
                let coaching = self.permission("git/show", &identity).await?;
                let mut command = vec![
                    "show".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--ignore-submodules=all".into(),
                    "--format=fuller".into(),
                ];
                command.extend(format_args(args.format));
                command.push(revision.to_string());
                command.push("--".into());
                command.extend(paths);
                let mut result = render_paged_text_result(
                    "show",
                    coaching,
                    self.run("show", command, PAGED_TEXT_LIMITS, true).await?,
                    args.offset,
                )?;
                self.disclose_submodules(&mut result).await;
                Ok(result)
            }
            GitOperation::Blame => {
                let timeout = query_timeout(&args)?;
                reject_irrelevant_fields(&args, false, true, false)?;
                if args.paths.len() != 1 {
                    return Err(ToolError::Msg(
                        "blame requires exactly one repository-relative path".to_string(),
                    ));
                }
                let paths = self.validate_paths(&args.paths, None).await?;
                if let Some(revision) = args.revision.as_deref() {
                    self.validate_revision(revision).await?;
                }
                let range = match (args.start_line, args.end_line) {
                    (None, None) => None,
                    (Some(start), end) if start >= 1 && end.is_none_or(|end| end >= start) => {
                        Some(format!("-L{start},{}", end.unwrap_or(start)))
                    }
                    _ => {
                        return Err(ToolError::Msg(
                            "blame needs start_line >= 1 and end_line >= start_line".to_string(),
                        ));
                    }
                };
                let identity = serde_json::to_string(&serde_json::json!({
                    "revision": args.revision,
                    "paths": paths,
                    "range": range,
                }))?;
                let coaching = self.permission("git/show", &identity).await?;
                let mut command = vec![
                    "blame".into(),
                    "--no-textconv".into(),
                    "--date=short".into(),
                ];
                command.extend(range);
                if let Some(revision) = args.revision {
                    command.push(revision);
                }
                command.push("--".into());
                command.extend(paths);
                let (guard, lfs_paths) = self.filter_guard_lfs("blame", true).await?;
                let mut result = render_paged_text_result(
                    "blame",
                    coaching,
                    self.run(
                        "blame",
                        guarded(&guard, command),
                        with_timeout(PAGED_TEXT_LIMITS, timeout),
                        true,
                    )
                    .await
                    .map_err(timeout_guidance)?,
                    args.offset,
                )?;
                disclose_lfs(&mut result, &lfs_paths);
                Ok(result)
            }
            GitOperation::Stage => self.stage(args).await,
            GitOperation::Unstage => self.unstage(args).await,
            GitOperation::Commit => self.commit(args).await,
            GitOperation::Branches => self.branches(args).await,
            GitOperation::BranchCreate => self.branch_create(args).await,
            GitOperation::Switch => self.switch(args).await,
            GitOperation::StashPush | GitOperation::StashPop | GitOperation::StashList => {
                self.stash(args).await
            }
            GitOperation::Tag => self.tag(args).await,
            GitOperation::Restore => self.restore(args).await,
            GitOperation::Merge
            | GitOperation::MergeAbort
            | GitOperation::CherryPick
            | GitOperation::CherryPickAbort
            | GitOperation::Revert
            | GitOperation::RevertAbort => self.history_operation(args).await,
            GitOperation::Reset => self.reset(args).await,
            GitOperation::BranchDelete | GitOperation::BranchRename => {
                self.branch_manage(args).await
            }
            GitOperation::StashApply | GitOperation::StashDrop => self.stash_by_index(args).await,
        }
    }
}

/// `git log` pretty format: NUL-separated fields (git forbids NUL in messages),
/// each record ended by a per-call unpredictable terminator. Git's tformat also
/// appends a newline after every record, so parsing splits records first and
/// then fields, keeping empty fields (a root commit has an empty `%P`).
fn log_format(terminator: &[u8]) -> String {
    format!(
        "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x00%b%x1e{}",
        String::from_utf8_lossy(&terminator[1..])
    )
}

fn new_log_terminator() -> Vec<u8> {
    let mut terminator = b"\x1e".to_vec();
    terminator.extend_from_slice(uuid::Uuid::new_v4().simple().to_string().as_bytes());
    terminator
}

const LOG_FIELD_COUNT: usize = 7;
/// Bytes of commit-message body kept per commit; `body_truncated` flags the rest
/// (read the whole message with `show`).
const LOG_BODY_MAX_BYTES: usize = 2048;

fn split_on<'a>(haystack: &'a [u8], needle: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle {
            parts.push(&haystack[start..index]);
            index += needle.len();
            start = index;
        } else {
            index += 1;
        }
    }
    parts.push(&haystack[start..]);
    parts
}

/// Bounded body text: trailing whitespace trimmed, cut at a char boundary.
fn bounded_log_body(body: &str) -> (String, bool) {
    let body = body.trim_end();
    if body.len() <= LOG_BODY_MAX_BYTES {
        return (body.to_string(), false);
    }
    let mut end = LOG_BODY_MAX_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    (body[..end].to_string(), true)
}

fn parse_log_records(
    stdout: &[u8],
    terminator: &[u8],
) -> Result<Vec<serde_json::Value>, ToolError> {
    let mut records = split_on(stdout, terminator);
    // What follows the final terminator is only the trailing newline for
    // complete output, or a partial record when the output limit cut it off
    // (reported separately as `truncated`). Only terminated records count.
    records.pop();
    records
        .into_iter()
        .map(|record| {
            let record = record.strip_prefix(b"\n").unwrap_or(record);
            let fields = record
                .split(|byte| *byte == 0)
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .collect::<Vec<_>>();
            if fields.len() != LOG_FIELD_COUNT {
                return Err(ToolError::Msg(
                    "git log output had unexpected field count".to_string(),
                ));
            }
            let (body, body_truncated) = bounded_log_body(&fields[6]);
            Ok(serde_json::json!({
                "id": fields[0], "parents": fields[1], "author": fields[2],
                "email": fields[3], "authored_at": fields[4], "subject": fields[5],
                "body": body, "body_truncated": body_truncated,
            }))
        })
        .collect()
}

impl Tool for GitTool {
    const NAME: &'static str = "git";
    type Error = ToolError;
    type Args = GitArgs;
    type Output = serde_json::Value;

    fn description(&self) -> String {
        if self.read_only {
            return "Inspect the bound Git repository through fixed read-only operations: status (collapsed untracked dirs; optional paths narrow it; truncated is flagged; submodule changes are never reported, results carry submodules_ignored when the repository has submodules), diff (revision may be a range a..b or a...b; revision_to for a two-commit diff; staged for the index), show (format patch|stat|name_status; at most 192 KiB per call, page with offset/next_offset or narrow with paths), log (revision may be a range; skip/next_skip pages past 100 commits; each commit carries subject plus a body of at most 2 KiB with body_truncated, use show for the full message; author/grep are fixed strings matched against the whole message), blame (one path, optional start_line/end_line/revision, paged like diff), branches (scope local|remote|all|tags; page with skip/max_count and next_skip) and stash_list. Nothing can be changed and nothing is fetched. No shell, raw argv, remotes, or network access is available.".to_string();
        }
        "Inspect and update the bound Git repository through fixed structured operations: status (collapsed untracked dirs; optional paths narrow it; truncated is flagged; submodule changes are never reported, results carry submodules_ignored when the repository has submodules), diff (revision may be a range a..b or a...b; revision_to for a two-commit diff; staged for the index) and show (format patch|stat|name_status; at most 192 KiB per call, page with offset/next_offset or narrow with paths), log (revision may be a range; skip/next_skip pages past 100 commits; each commit carries subject plus a body of at most 2 KiB with body_truncated, use show for the full message; author/grep are fixed strings matched against the whole message; a query that exceeds its 30 s budget returns the commits found so far, and log, blame, branches and stash_list accept timeout_secs up to 120), blame (one path, optional start_line/end_line/revision, paged like diff), stage, unstage, commit (optional signoff, allow_empty and a guarded amend of an unpushed HEAD; hooks and signing are never run and the result says so), branches (scope local|remote|all|tags lists remote-tracking branches or tags without network access; page with skip/max_count and next_skip), branch_create (name, optional start revision), switch (name of an existing branch), stash_push (optional message/paths), stash_pop, stash_list, tag (name, optional revision; message makes it annotated), restore (discard working-tree changes of paths, from the index or revision; checkpointed for /rewind), merge, cherry_pick and revert (revision; each has an *_abort; a conflict leaves it in progress with conflicted_paths), reset (revision, mode soft|mixed; refused for a pushed HEAD), branch_delete and branch_rename (name, new_name; never the current branch), stash_apply and stash_drop (index), and stash_push include_untracked. status reports in_progress; commit refuses inside a rebase or cherry-pick unless allow_in_progress, and rebase or bisect must be continued in the shell. In a Git LFS repository results carry lfs_pointers_not_resolved and LFS-tracked paths cannot be staged or committed. Every mutation has its own git/<operation> permission key. No shell, raw argv, remotes, or network access is available; rebase, fetch, pull and push are not offered.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        let operations: &[&str] = if self.read_only {
            &[
                "status",
                "diff",
                "log",
                "show",
                "blame",
                "branches",
                "stash_list",
            ]
        } else {
            &[
                "status",
                "diff",
                "log",
                "show",
                "blame",
                "stage",
                "unstage",
                "commit",
                "branches",
                "branch_create",
                "switch",
                "stash_push",
                "stash_pop",
                "stash_list",
                "tag",
                "restore",
                "merge",
                "merge_abort",
                "cherry_pick",
                "cherry_pick_abort",
                "revert",
                "revert_abort",
                "reset",
                "branch_delete",
                "branch_rename",
                "stash_apply",
                "stash_drop",
            ]
        };
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": operations
                },
                "name": { "type": "string", "maxLength": 255, "description": "Branch name for branch_create/switch, tag name for tag" },
                "format": { "type": "string", "enum": ["patch", "stat", "name_status"], "description": "diff/show output format (default patch)" },
                "offset": { "type": "integer", "minimum": 0, "description": "diff/show byte offset; pass the previous next_offset to fetch the next page" },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 128,
                    "description": "Literal repository-relative paths; never options or globs. Commit with paths commits only that selection; with no paths it commits the existing index."
                },
                "revision": { "type": "string", "description": "Revision for diff, log, show, blame, branch_create (start point), tag (target) or restore (source); diff and log also accept a range a..b or a...b" },
                "revision_to": { "type": "string", "description": "diff only: second revision, comparing revision with revision_to" },
                "staged": { "type": "boolean", "description": "diff only: compare the index (git diff --cached) instead of the working tree" },
                "skip": { "type": "integer", "minimum": 0, "description": "log, branches: entries to skip; pass the previous next_skip for the next page" },
                "author": { "type": "string", "maxLength": 512, "description": "log only: fixed-string author filter" },
                "grep": { "type": "string", "maxLength": 512, "description": "log only: fixed-string message filter" },
                "start_line": { "type": "integer", "minimum": 1, "description": "blame only: first line" },
                "end_line": { "type": "integer", "minimum": 1, "description": "blame only: last line (default start_line)" },
                "message": { "type": "string", "maxLength": 16384, "description": "Commit message (required for commit), stash_push message, or annotated tag message" },
                "max_count": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "log: commits per page (1-100; larger values are rejected, page with skip); branches: refs per page (default 100, at most 1000)" },
                "scope": { "type": "string", "enum": ["local", "remote", "all", "tags"], "description": "branches only: local branches (default), remote-tracking branches, both, or tags; page with skip/next_skip; nothing is fetched" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 120, "description": "log, blame, branches, stash_list: time budget for this call (default 30); retry with a larger value after a timeout" },
                "amend": { "type": "boolean", "description": "commit only: amend the HEAD commit if it is not in any remote-tracking ref (separate git/amend permission)" },
                "signoff": { "type": "boolean", "description": "commit only: add a Signed-off-by trailer" },
                "allow_empty": { "type": "boolean", "description": "commit only: allow a commit with no changes" },
                "allow_in_progress": { "type": "boolean", "description": "commit only: allow it inside a rebase or cherry-pick" },
                "mode": { "type": "string", "enum": ["soft", "mixed"], "description": "reset only (default mixed)" },
                "new_name": { "type": "string", "maxLength": 255, "description": "branch_rename only" },
                "index": { "type": "integer", "minimum": 0, "maximum": 1000, "description": "stash_apply, stash_drop: stash number (default 0)" },
                "include_untracked": { "type": "boolean", "description": "stash_push only" }
            },
            "required": ["operation"],
            "additionalProperties": false
        });
        if self.read_only
            && let Some(properties) = schema["properties"].as_object_mut()
        {
            // The inspection-only variant does not advertise mutation fields.
            for key in [
                "name",
                "message",
                "amend",
                "signoff",
                "allow_empty",
                "allow_in_progress",
                "mode",
                "new_name",
                "index",
                "include_untracked",
            ] {
                properties.remove(key);
            }
            properties.insert(
                "paths".to_owned(),
                serde_json::json!({
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 128,
                    "description": "Literal repository-relative paths; never options or globs."
                }),
            );
            properties.insert(
                "revision".to_owned(),
                serde_json::json!({ "type": "string", "description": "Revision for diff, log, show or blame; diff and log also accept a range a..b or a...b" }),
            );
        }
        schema
    }

    async fn call(&self, args: GitArgs) -> Result<Self::Output, Self::Error> {
        self.read_operation(args).await
    }
}

fn hardened_args(mut args: Vec<String>) -> Vec<String> {
    let mut hardened = vec![
        "--no-optional-locks".into(),
        "--literal-pathspecs".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.untrackedCache=false".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        // A repository-local `core.attributesFile` could point at any
        // workspace file; only in-tree and `info/attributes` remain, and the
        // filter guard inspects those.
        "-c".into(),
        "core.attributesFile=/dev/null".into(),
        "-c".into(),
        "commit.gpgSign=false".into(),
        "-c".into(),
        "tag.gpgSign=false".into(),
        "-c".into(),
        "diff.external=".into(),
        "-c".into(),
        "diff.trustExitCode=false".into(),
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        "core.askPass=".into(),
        "-c".into(),
        "submodule.recurse=false".into(),
        "-c".into(),
        "fetch.recurseSubmodules=false".into(),
        "-c".into(),
        "protocol.file.allow=never".into(),
        "-c".into(),
        "protocol.ext.allow=never".into(),
    ];
    hardened.append(&mut args);
    hardened
}

/// Prepends an operation's filter-neutralising `-c` prefix (see
/// [`GitTool::filter_guard`]) to a Git subcommand and its operands.
fn guarded(guard: &[String], args: Vec<String>) -> Vec<String> {
    let mut command = guard.to_vec();
    command.extend(args);
    command
}

/// Parses `git config -z --get-regexp '^filter\.'` output (`key LF value NUL`
/// or `key NUL`) into the sorted, distinct driver names. A name that cannot
/// be expressed as a `-c filter.<name>.<var>=` override fails closed.
fn parse_filter_driver_names(stdout: &[u8]) -> Result<Vec<String>, ToolError> {
    parse_filter_drivers(stdout).map(|drivers| drivers.names)
}

/// Like [`parse_filter_driver_names`], also classifying the `lfs` driver.
fn parse_filter_drivers(stdout: &[u8]) -> Result<FilterDrivers, ToolError> {
    let mut names = std::collections::BTreeSet::new();
    let mut lfs_seen = false;
    let mut lfs_canonical = true;
    for record in stdout.split(|byte| *byte == 0) {
        let mut parts = record.splitn(2, |byte| *byte == b'\n');
        let key = parts.next().unwrap_or(record);
        let value = parts.next();
        let Some(rest) = key.strip_prefix(b"filter.") else {
            continue;
        };
        // Section and variable names contain no dots; the subsection may.
        let Some(dot) = rest.iter().rposition(|byte| *byte == b'.') else {
            continue;
        };
        if &rest[..dot] == b"lfs" {
            lfs_seen = true;
            let variable = &rest[dot + 1..];
            let value = value.unwrap_or(b"\0");
            let canonical = match variable.to_ascii_lowercase().as_slice() {
                b"clean" => matches!(value, b"" | b"git-lfs clean -- %f"),
                b"smudge" => matches!(value, b"" | b"git-lfs smudge -- %f"),
                b"process" => matches!(value, b"" | b"git-lfs filter-process"),
                b"required" => matches!(value, b"" | b"true" | b"false"),
                _ => false,
            };
            lfs_canonical &= canonical;
        }
        let name = std::str::from_utf8(&rest[..dot])
            .ok()
            .filter(|name| !name.is_empty() && !name.contains(['=', '\0', '\n']))
            .ok_or_else(|| {
                ToolError::Msg(
                    "Git operation refused: repository config defines a filter driver whose \
                     name cannot be safely neutralised"
                        .to_string(),
                )
            })?;
        names.insert(name.to_string());
    }
    Ok(FilterDrivers {
        names: names.into_iter().collect(),
        lfs_canonical: lfs_seen && lfs_canonical,
    })
}

/// `-c` overrides that empty each driver's `clean` and `process` commands
/// (Git runs neither when empty) and clear `required`, so a matching path is
/// read unfiltered instead of executing a command.
fn neutralizing_args(drivers: &[String]) -> Vec<String> {
    drivers
        .iter()
        .flat_map(|driver| {
            [
                "-c".to_string(),
                format!("filter.{driver}.clean="),
                "-c".to_string(),
                format!("filter.{driver}.process="),
                "-c".to_string(),
                format!("filter.{driver}.smudge="),
                "-c".to_string(),
                format!("filter.{driver}.required=false"),
            ]
        })
        .collect()
}

fn format_args(format: Option<DiffFormat>) -> Vec<String> {
    match format.unwrap_or_default() {
        DiffFormat::Patch => Vec::new(),
        DiffFormat::Stat => vec!["--stat=200".into(), "--summary".into()],
        DiffFormat::NameStatus => vec!["--name-status".into()],
    }
}

/// One page of diff/show text: at most [`TEXT_PAGE_BYTES`] from `offset`,
/// ending on a line (or UTF-8) boundary, with the offset of the next page.
fn render_paged_text_result(
    operation: &str,
    coaching: Option<String>,
    output: CommandOutput,
    offset: Option<u64>,
) -> Result<serde_json::Value, ToolError> {
    render_paged_text_result_with(operation, coaching, output, offset, text_page_bytes())
}

fn render_paged_text_result_with(
    operation: &str,
    coaching: Option<String>,
    output: CommandOutput,
    offset: Option<u64>,
    page_bytes: usize,
) -> Result<serde_json::Value, ToolError> {
    let CommandOutput {
        exit_status,
        stdout,
        stderr,
        status: command_status,
        ..
    } = output;
    let total = stdout.len();
    let start = usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX);
    if start > total {
        return Err(ToolError::Msg(format!(
            "{operation} offset {start} is beyond the {total}-byte output"
        )));
    }
    let mut end = total.min(start.saturating_add(page_bytes));
    if end < total {
        if let Some(newline) = stdout[start..end].iter().rposition(|byte| *byte == b'\n') {
            end = start + newline + 1;
        }
        while end > start && end < total && (stdout[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
    }
    let next_offset = (end < total).then_some(end);
    let page = String::from_utf8_lossy(&stdout[start..end]);
    let stderr = String::from_utf8_lossy(&stderr);
    // Output beyond the 16 MiB run ceiling was not captured at all.
    let truncated = matches!(command_status, CommandStatus::OutputLimitExceeded(_));
    let exit_code = exit_status.and_then(|exit| exit.code());
    Ok(serde_json::json!({
        "operation": operation,
        "text": page,
        "stderr": stderr,
        "offset": start,
        "next_offset": next_offset,
        "total_bytes": total,
        "truncated": truncated,
        "exit_code": exit_code,
        "coaching": coaching,
    }))
}

fn render_ref_result(
    operation: &str,
    coaching: Option<String>,
    output: CommandOutput,
) -> serde_json::Value {
    let CommandOutput {
        exit_status,
        stdout,
        stderr,
        status: command_status,
        ..
    } = output;
    serde_json::json!({
        "operation": operation,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "status": mutation_status(command_status, exit_status),
        "exit_code": exit_status.and_then(|exit| exit.code()),
        "coaching": coaching,
    })
}

fn require_paths(paths: &[String], operation: &str) -> Result<(), ToolError> {
    if paths.is_empty() {
        Err(ToolError::Msg(format!(
            "{operation} requires at least one repository-relative path"
        )))
    } else {
        Ok(())
    }
}

fn reject_irrelevant_fields(
    args: &GitArgs,
    allow_message: bool,
    allow_revision: bool,
    allow_max_count: bool,
) -> Result<(), ToolError> {
    if !allow_message && args.message.is_some() {
        return Err(ToolError::Msg(format!(
            "message is not supported by {}; it is accepted by commit, stash_push and tag",
            args.operation.name()
        )));
    }
    if !allow_revision && args.revision.is_some() {
        return Err(ToolError::Msg(format!(
            "revision is not supported by {}; it is accepted by diff, log, show, blame, branch_create, tag and restore",
            args.operation.name()
        )));
    }
    if !allow_max_count && args.max_count.is_some() {
        return Err(ToolError::Msg(format!(
            "max_count is not supported by {}; it is accepted by log and branches",
            args.operation.name()
        )));
    }
    Ok(())
}

fn render_mutation_result(
    operation: &str,
    coaching: Option<String>,
    before: serde_json::Value,
    after: serde_json::Value,
    output: CommandOutput,
) -> serde_json::Value {
    let CommandOutput {
        exit_status,
        stdout,
        stderr,
        status: command_status,
        ..
    } = output;
    serde_json::json!({
        "operation": operation,
        "before": mutation_before_view(before),
        "after": mutation_after_view(after),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "status": mutation_status(command_status, exit_status),
        "truncated": matches!(command_status, CommandStatus::OutputLimitExceeded(_)),
        "exit_code": exit_status.and_then(|exit| exit.code()),
        "coaching": coaching,
    })
}

fn mutation_status(
    status: CommandStatus,
    exit_status: Option<std::process::ExitStatus>,
) -> &'static str {
    match status {
        CommandStatus::Completed if exit_status.is_some_and(|value| value.success()) => "success",
        CommandStatus::Completed => "nonzero",
        CommandStatus::TimedOut => "timed_out",
        CommandStatus::Cancelled => "cancelled",
        CommandStatus::OutputLimitExceeded(_) => "output_limit_exceeded",
        CommandStatus::Failed | CommandStatus::NotStarted => "failed",
    }
}

/// Renders a `log` run. A timed-out query returns the commits found so far
/// with a continuation (`next_skip`) and a narrowing note.
fn render_log_result(
    output: &CommandOutput,
    skip: u32,
    count: u16,
    timeout: std::time::Duration,
    coaching: Option<String>,
    terminator: &[u8],
) -> Result<serde_json::Value, ToolError> {
    let timed_out = output.status == CommandStatus::TimedOut;
    let output_limited = matches!(output.status, CommandStatus::OutputLimitExceeded(_));
    let commits = parse_log_records(&output.stdout, terminator)?;
    // A full page may have more behind it: say how to continue. A timed-out
    // query always offers a continuation.
    let next_skip = (commits.len() == usize::from(count)
        || ((timed_out || output_limited) && !commits.is_empty()))
    .then(|| u64::from(skip) + commits.len() as u64);
    let mut value = serde_json::json!({
        "operation": "log",
        "skip": skip,
        "next_skip": next_skip,
        "commits": commits,
        "truncated": timed_out || output_limited,
        "coaching": coaching,
    });
    if timed_out {
        value["timed_out"] = serde_json::json!(true);
        value["note"] = serde_json::json!(format!(
            "git log timed out after {}s; the commits found so far are shown. Narrow with a revision range, paths or skip (next_skip), or retry with a larger timeout_secs (at most {QUERY_TIMEOUT_MAX_SECS}).",
            timeout.as_secs()
        ));
    }
    Ok(value)
}

/// One `for-each-ref` line of the `branches` listing as a JSON entry.
fn parse_ref_line(line: &str, scope: RefScope) -> Option<serde_json::Value> {
    let fields: Vec<&str> = line.split('\0').collect();
    if fields.len() != 8 {
        return None;
    }
    let refname = fields[1];
    let (kind, name) = if let Some(name) = refname.strip_prefix("refs/heads/") {
        ("local", name)
    } else if let Some(name) = refname.strip_prefix("refs/remotes/") {
        ("remote", name)
    } else if let Some(name) = refname.strip_prefix("refs/tags/") {
        ("tag", name)
    } else {
        return None;
    };
    if scope == RefScope::Tags {
        let annotated = fields[5] == "tag";
        return Some(serde_json::json!({
            "name": name,
            "id": fields[2],
            "annotated": annotated,
            // The commit an annotated tag points at; the id itself otherwise.
            "target": if annotated && !fields[6].is_empty() { fields[6] } else { fields[2] },
            "subject": fields[4],
        }));
    }
    let mut entry = serde_json::json!({
        "current": fields[0] == "*",
        "name": name,
        "kind": kind,
        "id": fields[2],
        "upstream": fields[3],
        "subject": fields[4],
    });
    if !fields[7].is_empty() {
        entry["symref"] = serde_json::json!(fields[7]);
    }
    Some(entry)
}

/// Split `git status --porcelain=v2 -z` output into entries. A rename/copy
/// (`2 ...`) record is followed by its origin path as a separate NUL field
/// with no type prefix; it is attached to the record as `\torig: <path>`
/// rather than counted or listed as an entry of its own. When `truncated`,
/// output is cut only at entry boundaries: an incomplete trailing field, or a
/// rename record whose origin field is missing, is dropped.
/// Returns `(entries, changed_count, untracked_count)`.
fn parse_status_records(stdout: &[u8], truncated: bool) -> (Vec<String>, usize, usize) {
    let mut fields: Vec<&[u8]> = stdout.split(|byte| *byte == 0).collect();
    // The final element is what follows the last NUL: empty for complete
    // output, a partial field when the output was cut.
    let tail = fields.pop().unwrap_or(&[]);
    if !truncated && !tail.is_empty() {
        fields.push(tail);
    }
    let mut records = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        if field.is_empty() {
            continue;
        }
        let mut record = compact_status_record(field);
        if field.starts_with(b"2 ") {
            match fields.get(index) {
                Some(origin) => {
                    record.push_str("\torig: ");
                    record.push_str(&String::from_utf8_lossy(origin));
                    index += 1;
                }
                // Origin field lost to truncation: drop the incomplete entry.
                None if truncated => break,
                None => {}
            }
        }
        records.push(record);
    }
    let untracked = records.iter().filter(|r| r.starts_with("? ")).count();
    let changed = records
        .iter()
        .filter(|r| matches!(r.as_bytes().first(), Some(b'1' | b'2' | b'u')))
        .count();
    (records, changed, untracked)
}

/// The commit id in a status snapshot's `# branch.oid` header, if it names one
/// (an unborn branch reports `(initial)`).
fn head_from_snapshot(snapshot: &serde_json::Value) -> Option<String> {
    let id = snapshot
        .get("records")?
        .as_array()?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .find_map(|record| record.strip_prefix("# branch.oid "))?;
    (id.len() >= 40 && id.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| id.to_string())
}

/// A porcelain-v2 changed entry reduced to `<kind> <XY> <path>`: the two modes,
/// three submodule flags and two object ids (about 115 bytes an entry) are
/// never needed by the model, which can ask for a diff or `status` details.
/// Anything that does not parse is kept verbatim.
fn compact_status_record(field: &[u8]) -> String {
    let count = match field.first() {
        Some(b'1') => 9,
        Some(b'2') => 10,
        Some(b'u') => 11,
        _ => return String::from_utf8_lossy(field).into_owned(),
    };
    let parts: Vec<&[u8]> = field.splitn(count, |byte| *byte == b' ').collect();
    if parts.len() != count || parts[1].len() != 2 {
        return String::from_utf8_lossy(field).into_owned();
    }
    format!(
        "{} {} {}",
        String::from_utf8_lossy(parts[0]),
        String::from_utf8_lossy(parts[1]),
        String::from_utf8_lossy(parts[count - 1])
    )
}

/// Records kept in the `after` snapshot of a mutation result; the model can
/// call `status` for the rest.
const MUTATION_RECORD_CAP: usize = 50;

/// The model-facing `before` snapshot of a mutation: counts only.
fn mutation_before_view(before: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "counts": before.get("counts").cloned().unwrap_or(serde_json::Value::Null),
        "truncated": before.get("truncated").cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// The model-facing `after` snapshot of a mutation: no structured
/// `path_records` (host-side `/commit` reads those from its own status
/// call), and at most [`MUTATION_RECORD_CAP`] records.
fn mutation_after_view(mut after: serde_json::Value) -> serde_json::Value {
    let Some(object) = after.as_object_mut() else {
        return after;
    };
    object.remove("path_records");
    if let Some(records) = object
        .get_mut("records")
        .and_then(serde_json::Value::as_array_mut)
        && records.len() > MUTATION_RECORD_CAP
    {
        let omitted = records.len() - MUTATION_RECORD_CAP;
        records.truncate(MUTATION_RECORD_CAP);
        object.insert("records_omitted".into(), serde_json::json!(omitted));
    }
    after
}

/// Lossless path operands for consumers that authorize effects. Display records
/// deliberately join rename fields and therefore cannot serve as path operands.
fn parse_status_paths(stdout: &[u8], truncated: bool) -> Vec<serde_json::Value> {
    let mut fields: Vec<&[u8]> = stdout.split(|byte| *byte == 0).collect();
    let tail = fields.pop().unwrap_or_default();
    if !truncated && !tail.is_empty() {
        fields.push(tail);
    }
    let mut entries = Vec::new();
    let mut fields = fields.into_iter();
    while let Some(field) = fields.next() {
        let Some(kind @ (b'1' | b'2' | b'u')) = field.first().copied() else {
            continue;
        };
        let count = if kind == b'2' {
            10
        } else if kind == b'u' {
            11
        } else {
            9
        };
        let parts: Vec<&[u8]> = field.splitn(count, |byte| *byte == b' ').collect();
        let origin = if kind == b'2' { fields.next() } else { None };
        if truncated && kind == b'2' && origin.is_none() {
            break;
        }
        let valid = parts.len() == count && parts[1].len() == 2;
        let path = valid
            .then(|| std::str::from_utf8(parts[count - 1]).ok())
            .flatten();
        let origin = origin.and_then(|path| std::str::from_utf8(path).ok());
        entries.push(serde_json::json!({
            "kind": (kind as char).to_string(),
            "xy": valid.then(|| std::str::from_utf8(parts[1]).ok()).flatten(),
            "path": path,
            "origin": origin,
        }));
    }
    entries
}

#[cfg(test)]
mod tests {
    /// The structured-Git ADR lists every model-visible operation. The
    /// exhaustive match makes adding a variant a compile error here until the
    /// array below, and then the ADR, are updated.
    #[test]
    fn adr_lists_every_git_operation() {
        use super::GitOperation as Op;
        fn covered(operation: Op) {
            match operation {
                Op::Status
                | Op::Diff
                | Op::Log
                | Op::Show
                | Op::Blame
                | Op::Stage
                | Op::Unstage
                | Op::Commit
                | Op::Branches
                | Op::BranchCreate
                | Op::Switch
                | Op::StashPush
                | Op::StashPop
                | Op::StashList
                | Op::Tag
                | Op::Restore
                | Op::Merge
                | Op::MergeAbort
                | Op::CherryPick
                | Op::CherryPickAbort
                | Op::Revert
                | Op::RevertAbort
                | Op::Reset
                | Op::BranchDelete
                | Op::BranchRename
                | Op::StashApply
                | Op::StashDrop => {}
            }
        }
        let all = [
            Op::Status,
            Op::Diff,
            Op::Log,
            Op::Show,
            Op::Blame,
            Op::Stage,
            Op::Unstage,
            Op::Commit,
            Op::Branches,
            Op::BranchCreate,
            Op::Switch,
            Op::StashPush,
            Op::StashPop,
            Op::StashList,
            Op::Tag,
            Op::Restore,
            Op::Merge,
            Op::MergeAbort,
            Op::CherryPick,
            Op::CherryPickAbort,
            Op::Revert,
            Op::RevertAbort,
            Op::Reset,
            Op::BranchDelete,
            Op::BranchRename,
            Op::StashApply,
            Op::StashDrop,
        ];
        let adr = include_str!("../../docs/specs/structured-git-mutations.md");
        for operation in all {
            covered(operation);
            assert!(
                adr.contains(&format!("`{}`", operation.name())),
                "docs/specs/structured-git-mutations.md must list `{}`",
                operation.name()
            );
        }
    }

    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use rig::tool::Tool;

    use super::{
        GitArgs, GitOperation, GitTool, hardened_args, mutation_status, render_mutation_result,
    };
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    struct TestRepo {
        root: std::path::PathBuf,
    }

    impl TestRepo {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("mini-agent-git-tool-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).expect("create test repository");
            let repo = Self { root };
            repo.git(["init", "--quiet"]);
            repo.git(["config", "user.name", "Mini Agent Test"]);
            repo.git(["config", "user.email", "mini-agent@example.invalid"]);
            repo
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn git<const N: usize>(&self, args: [&str; N]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(args)
                .output()
                .expect("run git fixture command");
            let std::process::Output {
                status: exit,
                stdout,
                stderr,
            } = output;
            assert!(
                exit.success(),
                "git fixture command failed: {}",
                String::from_utf8_lossy(&stderr)
            );
            String::from_utf8(stdout).expect("git fixture output is UTF-8")
        }

        fn write(&self, path: &str, contents: &str) {
            std::fs::write(self.root.join(path), contents).expect("write test repository file");
        }

        fn tool(&self) -> GitTool {
            let workspace = Arc::new(
                crate::paths::WorkspaceBinding::capture(&self.root)
                    .expect("capture test repository"),
            );
            GitTool {
                runner: crate::git::runner::GitRunner::discover().expect("discover Git"),
                workspace: workspace.clone(),
                sandbox: crate::sandbox::Sandbox::new(false, "bwrap")
                    .with_workspace_binding(workspace),
                permission: None,
                ask_tx: None,
                exact_files: false,
                read_only: false,
                filter_drivers: Arc::default(),
                mutation_key: Arc::default(),
                test_uncontained: true,
            }
        }

        fn tool_with_permission(&self, config: PermissionConfig) -> GitTool {
            let mut tool = self.tool();
            let checker = PermissionChecker::new(
                &PermissionConfigs::from(config),
                SecurityMode::Standard,
                Some(self.root.clone()),
                Some(vec!["standard".to_string()]),
            )
            .expect("create permission checker");
            tool.permission = Some(Arc::new(Mutex::new(checker)));
            tool
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn args(operation: GitOperation, paths: &[&str], message: Option<&str>) -> GitArgs {
        GitArgs {
            operation,
            paths: paths.iter().map(|path| (*path).to_string()).collect(),
            revision: None,
            message: message.map(str::to_string),
            max_count: None,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn read_only_tool_refuses_every_mutation_and_offers_only_inspection() {
        let repo = TestRepo::new();
        repo.write("tracked.txt", "first\n");
        let mut tool = repo.tool();
        tool.read_only = true;
        let schema = tool.parameters();
        let operations = schema["properties"]["operation"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(operations.contains(&"status".to_string()));
        for write in [
            "stage",
            "unstage",
            "commit",
            "branch_create",
            "switch",
            "stash_push",
            "stash_pop",
            "tag",
            "restore",
            "merge",
            "merge_abort",
            "cherry_pick",
            "cherry_pick_abort",
            "revert",
            "revert_abort",
            "reset",
            "branch_delete",
            "branch_rename",
            "stash_apply",
            "stash_drop",
        ] {
            assert!(!operations.contains(&write.to_string()), "{write} offered");
        }
        for field in ["name", "message", "amend", "signoff", "allow_empty"] {
            assert!(schema["properties"].get(field).is_none(), "{field} offered");
        }
        for operation in [
            GitOperation::Stage,
            GitOperation::Unstage,
            GitOperation::Commit,
            GitOperation::BranchCreate,
            GitOperation::Switch,
            GitOperation::StashPush,
            GitOperation::StashPop,
            GitOperation::Tag,
            GitOperation::Restore,
            GitOperation::Merge,
            GitOperation::MergeAbort,
            GitOperation::CherryPick,
            GitOperation::CherryPickAbort,
            GitOperation::Revert,
            GitOperation::RevertAbort,
            GitOperation::Reset,
            GitOperation::BranchDelete,
            GitOperation::BranchRename,
            GitOperation::StashApply,
            GitOperation::StashDrop,
        ] {
            let error = tool
                .call(args(operation, &["tracked.txt"], Some("m")))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("read-only"), "{error}");
        }
        // Nothing was staged or committed, and inspection still works.
        assert_eq!(repo.git(["diff", "--cached", "--name-only"]).trim(), "");
        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert!(status.to_string().contains("tracked.txt"), "{status}");
    }

    #[tokio::test]
    async fn stage_and_unstage_mutate_only_the_index() {
        let repo = TestRepo::new();
        repo.write("tracked.txt", "first\n");
        let tool = repo.tool();

        let staged = tool
            .call(args(GitOperation::Stage, &["tracked.txt"], None))
            .await
            .expect("stage path");
        assert_eq!(staged["operation"], "stage");
        assert_eq!(staged["exit_code"], 0);
        assert_eq!(
            repo.git(["diff", "--cached", "--name-only"]),
            "tracked.txt\n"
        );

        let unstaged = tool
            .call(args(GitOperation::Unstage, &["tracked.txt"], None))
            .await
            .expect("unstage path");
        assert_eq!(unstaged["operation"], "unstage");
        assert_eq!(unstaged["exit_code"], 0);
        assert!(repo.git(["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "first\n"
        );
    }

    #[tokio::test]
    async fn commit_reads_a_bounded_message_from_stdin() {
        let repo = TestRepo::new();
        repo.write("committed.txt", "content\n");
        let tool = repo.tool();
        tool.call(args(GitOperation::Stage, &["committed.txt"], None))
            .await
            .expect("stage path");

        let committed = tool
            .call(args(
                GitOperation::Commit,
                &[],
                Some("subject from stdin\n\nbody remains intact"),
            ))
            .await
            .expect("commit index");
        assert_eq!(committed["operation"], "commit");
        assert_eq!(committed["status"], "success");
        assert_eq!(committed["exit_code"], 0);
        assert_eq!(
            committed["commit_id"],
            repo.git(["rev-parse", "HEAD"]).trim()
        );
        assert_eq!(
            repo.git(["log", "-1", "--format=%B"]),
            "subject from stdin\n\nbody remains intact\n"
        );
    }

    #[tokio::test]
    async fn selected_commit_keeps_an_unrelated_staged_file_out_of_the_commit() {
        let repo = TestRepo::new();
        repo.write("selected.txt", "selected before\n");
        repo.write("other.txt", "other before\n");
        let tool = repo.tool();
        tool.call(args(
            GitOperation::Stage,
            &["selected.txt", "other.txt"],
            None,
        ))
        .await
        .expect("stage initial files");
        tool.call(args(GitOperation::Commit, &[], Some("initial")))
            .await
            .expect("commit initial index");
        repo.write("selected.txt", "selected after\n");
        repo.write("other.txt", "other after\n");
        // Another writer can stage this after the UI's final status check.
        // The commit operation itself must enforce the frozen selection.
        tool.call(args(GitOperation::Stage, &["other.txt"], None))
            .await
            .expect("stage unrelated change");
        let message = "literal $(command); 'quote'\n\nbody";
        let result = tool
            .call(args(GitOperation::Commit, &["selected.txt"], Some(message)))
            .await
            .expect("commit only selected path");
        assert_eq!(result["status"], "success");
        assert_eq!(repo.git(["show", "HEAD:selected.txt"]), "selected after\n");
        assert_eq!(repo.git(["show", "HEAD:other.txt"]), "other before\n");
        assert_eq!(repo.git(["diff", "--cached", "--name-only"]), "other.txt\n");
        assert_eq!(
            repo.git(["log", "-1", "--format=%B"]),
            format!("{message}\n")
        );
    }

    #[tokio::test]
    async fn failed_commit_still_reports_the_post_operation_snapshot() {
        let repo = TestRepo::new();
        let committed = repo
            .tool()
            .call(args(GitOperation::Commit, &[], Some("empty index")))
            .await
            .expect("a completed non-zero mutation remains an observed result");

        assert_eq!(committed["status"], "nonzero");
        assert!(
            committed["exit_code"]
                .as_i64()
                .is_some_and(|code| code != 0)
        );
        assert!(committed["before"]["counts"].is_object());
        assert!(committed["after"]["records"].is_array());
    }

    #[cfg(feature = "git-worktree")]
    #[tokio::test]
    async fn native_commit_guard_accepts_real_rename_operands_losslessly() {
        let repo = TestRepo::new();
        let old = if cfg!(windows) {
            "old 雪.txt"
        } else {
            "old\torig: name\n雪.txt"
        };
        let new = if cfg!(windows) {
            "new 雪.txt"
        } else {
            "new\torig: name\n雪.txt"
        };
        repo.write(old, "tracked\n");
        repo.git(["add", "--", old]);
        repo.git(["commit", "--quiet", "-m", "initial"]);
        repo.git(["mv", "--", old, new]);
        repo.write("other.txt", "other\n");
        repo.git(["add", "--", "other.txt"]);
        // Lossless path records belong to the host-side `/commit` flow.
        let mut tool = repo.tool();
        tool.exact_files = true;
        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        let guard = crate::ui::slash::git::check_staged_paths;
        assert!(
            guard(&status, &[old.into(), new.into(), "other.txt".into()]).is_ok(),
            "{status}"
        );
        assert!(guard(&status, &[old.into(), new.into()]).is_err());
        assert!(guard(&status, &[new.into(), "other.txt".into()]).is_err());
        assert!(guard(&status, &[old.into(), "other.txt".into()]).is_err());
    }

    #[test]
    fn mutation_results_stay_small_for_a_large_dirty_tree() {
        let record = |index: usize| {
            format!(
                "1 .M N... 100644 100644 100644 {0} {0} src/module_{index:03}/file.rs",
                "a".repeat(40)
            )
        };
        let raw: Vec<String> = (0..200).map(record).collect();
        let mut stdout = Vec::new();
        for entry in &raw {
            stdout.extend_from_slice(entry.as_bytes());
            stdout.push(0);
        }
        let (compact, changed, _) = super::parse_status_records(&stdout, false);
        assert_eq!(changed, 200);
        assert_eq!(compact[7], "1 .M src/module_007/file.rs");
        let snapshot = serde_json::json!({
            "records": compact,
            "path_records": super::parse_status_paths(&stdout, false),
            "truncated": false,
            "counts": { "changed": 200, "untracked": 0 },
        });
        let full = serde_json::to_string(&serde_json::json!({
            "before": snapshot.clone(), "after": snapshot.clone()
        }))
        .unwrap();
        let rendered = render_mutation_result(
            "stage",
            None,
            snapshot.clone(),
            snapshot,
            crate::sandbox::CommandOutput {
                exit_status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: crate::sandbox::CommandStatus::Completed,
                descendants_escaped: false,
            },
        );
        let size = rendered.to_string().len();
        assert!(size < 4096, "{size} bytes: {rendered}");
        assert!(size * 10 < full.len(), "{size} vs {}", full.len());
        assert!(rendered["after"].get("path_records").is_none());
        assert_eq!(rendered["after"]["records_omitted"], 150);
        assert_eq!(rendered["before"]["counts"]["changed"], 200);
        assert!(rendered["before"].get("records").is_none());
    }

    #[test]
    fn resulting_head_is_read_from_the_status_header() {
        let oid = "a".repeat(40);
        let snapshot =
            serde_json::json!({"records": [format!("# branch.oid {oid}"), "# branch.head main"]});
        assert_eq!(super::head_from_snapshot(&snapshot), Some(oid));
        let unborn = serde_json::json!({"records": ["# branch.oid (initial)"]});
        assert_eq!(super::head_from_snapshot(&unborn), None);
    }

    #[tokio::test]
    async fn path_records_are_host_only() {
        let repo = TestRepo::new();
        repo.write("a.txt", "a\n");
        let model = repo
            .tool()
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert!(model.get("path_records").is_none(), "{model}");
        let mut host = repo.tool();
        host.exact_files = true;
        let host = host
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert!(host["path_records"].is_array(), "{host}");
    }

    #[test]
    fn interrupted_mutation_result_preserves_the_post_operation_snapshot() {
        let after = serde_json::json!({"records": ["? changed.txt"]});
        let rendered = render_mutation_result(
            "stage",
            None,
            serde_json::json!({"records": []}),
            after.clone(),
            crate::sandbox::CommandOutput {
                exit_status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: crate::sandbox::CommandStatus::TimedOut,
                descendants_escaped: false,
            },
        );

        assert_eq!(rendered["status"], "timed_out");
        assert_eq!(rendered["after"], after);
        assert_eq!(
            mutation_status(crate::sandbox::CommandStatus::Cancelled, None),
            "cancelled"
        );
    }

    fn log_args(max_count: Option<u16>) -> GitArgs {
        GitArgs {
            max_count,
            ..args(GitOperation::Log, &[], None)
        }
    }

    #[tokio::test]
    async fn log_rejects_max_count_above_its_cap_instead_of_clamping() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        let tool = repo.tool();
        for bad in [0u16, 101, 500, 1000] {
            let error = tool
                .call(log_args(Some(bad)))
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("between 1 and 100"), "{bad}: {error}");
        }
        tool.call(log_args(Some(100))).await.unwrap();
    }

    #[tokio::test]
    async fn log_reports_a_single_root_commit_with_empty_parents() {
        let repo = TestRepo::new();
        repo.write("root.txt", "root\n");
        repo.git(["add", "root.txt"]);
        repo.git(["commit", "--quiet", "-m", "root subject"]);
        let head = repo.git(["rev-parse", "HEAD"]).trim().to_string();

        let log = repo
            .tool()
            .call(log_args(Some(1)))
            .await
            .expect("log a single root commit");
        let commits = log["commits"].as_array().expect("commits array");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0]["id"], head);
        assert_eq!(commits[0]["parents"], "");
        assert_eq!(commits[0]["author"], "Mini Agent Test");
        assert_eq!(commits[0]["email"], "mini-agent@example.invalid");
        assert_eq!(commits[0]["subject"], "root subject");
        assert_eq!(log["truncated"], false);
    }

    #[tokio::test]
    async fn log_keeps_fields_aligned_across_commits_reaching_the_root() {
        let repo = TestRepo::new();
        repo.write("file.txt", "one\n");
        repo.git(["add", "file.txt"]);
        repo.git(["commit", "--quiet", "-m", "first"]);
        let first = repo.git(["rev-parse", "HEAD"]).trim().to_string();
        repo.write("file.txt", "two\n");
        repo.git(["commit", "--quiet", "-am", "second"]);
        let second = repo.git(["rev-parse", "HEAD"]).trim().to_string();

        let log = repo.tool().call(log_args(None)).await.expect("log history");
        let commits = log["commits"].as_array().expect("commits array");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["id"], second);
        assert_eq!(commits[0]["parents"], first);
        assert_eq!(commits[0]["subject"], "second");
        assert_eq!(commits[1]["id"], first);
        assert_eq!(commits[1]["parents"], "");
        assert_eq!(commits[1]["subject"], "first");
    }

    #[tokio::test]
    async fn log_returns_bounded_multi_paragraph_body_and_trailers() {
        let repo = TestRepo::new();
        repo.write("a.txt", "a\n");
        repo.git(["add", "a.txt"]);
        repo.git([
            "commit",
            "--quiet",
            "-m",
            "subject line",
            "-m",
            "first paragraph\n\nsecond \x1e paragraph",
            "-m",
            "Fixes: #123\nCo-authored-by: A <a@example.invalid>",
        ]);
        repo.write("b.txt", "b\n");
        repo.git(["add", "b.txt"]);
        let long = "x".repeat(5000);
        repo.git(["commit", "--quiet", "-m", "long body", "-m", long.as_str()]);
        repo.write("c.txt", "c\n");
        repo.git(["add", "c.txt"]);
        repo.git(["commit", "--quiet", "-m", "no body"]);

        let log = repo.tool().call(log_args(None)).await.expect("log");
        let commits = log["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0]["body"], "");
        assert_eq!(commits[0]["body_truncated"], false);
        assert_eq!(commits[1]["subject"], "long body");
        assert_eq!(commits[1]["body"].as_str().unwrap().len(), 2048);
        assert_eq!(commits[1]["body_truncated"], true);
        assert_eq!(commits[2]["subject"], "subject line");
        let body = commits[2]["body"].as_str().unwrap();
        assert!(
            body.contains("first paragraph\n\nsecond \u{1e} paragraph"),
            "{body}"
        );
        assert!(body.contains("Fixes: #123"), "{body}");
        assert!(body.contains("Co-authored-by: A"), "{body}");
        assert_eq!(commits[2]["body_truncated"], false);
    }

    #[test]
    fn log_body_cut_respects_char_boundaries() {
        let text = "é".repeat(2000);
        let (body, truncated) = super::bounded_log_body(&text);
        assert!(truncated);
        assert!(body.len() <= 2048 && body.chars().all(|c| c == 'é'));
    }

    #[test]
    fn log_parser_returns_only_terminated_records() {
        const T: &[u8] = b"\x1eNONCE";
        let complete = b"a\0\0n\0e\0t\0s1\0\x1eNONCE\nb\0a\0n\0e\0t\0s2\0\x1eNONCE\n";
        let partial = b"a\0\0n\0e\0t\0s1\0\x1eNONCE\nb\0a\0n";
        let parsed = super::parse_log_records(complete, T).expect("complete output");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1]["id"], "b");
        assert_eq!(parsed[1]["parents"], "a");
        let parsed = super::parse_log_records(partial, T).expect("truncated output");
        assert_eq!(parsed.len(), 1);
        assert!(super::parse_log_records(b"a\0b\x1eNONCE\n", T).is_err());
        assert!(
            super::parse_log_records(b"", T)
                .expect("empty history")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn mutations_reject_missing_operands() {
        let repo = TestRepo::new();
        let tool = repo.tool();
        let stage_error = tool
            .call(args(GitOperation::Stage, &[], None))
            .await
            .expect_err("stage must require paths");
        assert!(stage_error.to_string().contains("requires at least one"));

        let commit_error = tool
            .call(args(GitOperation::Commit, &[], Some("   ")))
            .await
            .expect_err("commit must require a non-empty message");
        assert!(commit_error.to_string().contains("non-empty message"));

        let option_error = tool
            .call(args(GitOperation::Stage, &["-option"], None))
            .await
            .expect_err("stage must reject option-like paths");
        assert!(
            option_error
                .to_string()
                .contains("invalid repository-relative")
        );
    }

    #[tokio::test]
    async fn pathspec_metacharacters_are_treated_as_literal_file_names() {
        let repo = TestRepo::new();
        repo.write("literal[1].txt", "selected\n");
        repo.write("literal1.txt", "must remain untracked\n");

        repo.tool()
            .call(args(GitOperation::Stage, &["literal[1].txt"], None))
            .await
            .expect("stage literal metacharacter path");

        assert_eq!(
            repo.git(["diff", "--cached", "--name-only"]),
            "literal[1].txt\n"
        );
    }

    #[tokio::test]
    async fn diff_reports_binary_changes_without_emitting_binary_patch_payloads() {
        let repo = TestRepo::new();
        std::fs::write(repo.path().join("asset.bin"), b"before\0bytes").unwrap();
        repo.git(["add", "asset.bin"]);
        repo.git(["commit", "--quiet", "-m", "base"]);
        std::fs::write(repo.path().join("asset.bin"), b"after\0bytes").unwrap();

        let diff = repo
            .tool()
            .call(args(GitOperation::Diff, &["asset.bin"], None))
            .await
            .expect("render binary diff");
        let text = diff["text"].as_str().unwrap();

        assert!(text.contains("Binary files"), "{text}");
        assert!(!text.contains("GIT binary patch"), "{text}");
    }

    #[test]
    fn status_parser_attaches_rename_origin_and_cuts_at_entry_boundaries() {
        use super::parse_status_records;
        let out = b"1 .M N... 100644 100644 100644 a b x.txt\x002 R. N... 100644 100644 100644 a b R100 new.txt\0u.rs\0? untracked\0";
        let (records, changed, untracked) = parse_status_records(out, false);
        assert_eq!(records.len(), 3, "{records:?}");
        assert!(records[1].ends_with("new.txt\torig: u.rs"), "{records:?}");
        assert_eq!((changed, untracked), (2, 1));
        // Cut between a rename record and its origin: the entry is dropped.
        let cut = b"1 .M N... 100644 100644 100644 a b x.txt\x002 R. N... 100644 100644 100644 a b R100 new.txt\0";
        let (records, changed, _) = parse_status_records(cut, true);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(changed, 1);
        // Cut inside a field: the partial field is dropped.
        let (records, _, _) = parse_status_records(b"? a.txt\0? b.t", true);
        assert_eq!(records, vec!["? a.txt".to_string()]);
    }

    #[tokio::test]
    async fn status_does_not_count_rename_origin_paths_as_entries() {
        for old in ["1.txt", "2024-report.md", "utils.rs"] {
            let repo = TestRepo::new();
            repo.write(old, "some content that is long enough to detect a rename\n");
            repo.git(["add", "."]);
            repo.git(["commit", "--quiet", "-m", "base"]);
            repo.git(["mv", old, "renamed.txt"]);
            let status = repo
                .tool()
                .call(args(GitOperation::Status, &[], None))
                .await
                .expect("status");
            let records = status["records"].as_array().unwrap();
            // Two `# branch.*` headers plus exactly one entry.
            assert_eq!(records.len(), 3, "{old}: {status}");
            assert_eq!(status["counts"]["changed"], 1, "{old}: {status}");
            let record = records[2].as_str().unwrap();
            assert!(record.starts_with("2 "), "{record}");
            assert!(
                record.ends_with(&format!("renamed.txt\torig: {old}")),
                "{record}"
            );
        }
    }

    #[tokio::test]
    async fn status_diff_show_disclose_ignored_submodules() {
        let repo = TestRepo::new();
        repo.write("a.txt", "a\n");
        repo.git(["add", "."]);
        repo.git(["commit", "--quiet", "-m", "base"]);
        let plain = repo
            .tool()
            .call(args(GitOperation::Status, &[], None))
            .await
            .expect("status");
        assert!(plain.get("submodules_ignored").is_none(), "{plain}");
        // A gitlink (mode 160000) recorded without any nested repository.
        let head = repo.git(["rev-parse", "HEAD"]);
        let cacheinfo = format!("160000,{},vendor/lib", head.trim());
        repo.git(["update-index", "--add", "--cacheinfo", &cacheinfo]);
        repo.git(["commit", "--quiet", "-m", "add submodule pointer"]);
        let status = repo
            .tool()
            .call(args(GitOperation::Status, &[], None))
            .await
            .expect("status");
        assert_eq!(status["submodules_ignored"], true, "{status}");
        assert_eq!(status["submodule_paths"][0], "vendor/lib", "{status}");
        assert!(
            status["submodules_notice"]
                .as_str()
                .unwrap()
                .contains("submodule"),
            "{status}"
        );
        let mut show = args(GitOperation::Show, &[], None);
        show.revision = Some("HEAD".to_string());
        let show = repo.tool().call(show).await.expect("show");
        assert_eq!(show["submodules_ignored"], true, "{show}");
        let diff = repo
            .tool()
            .call(args(GitOperation::Diff, &[], None))
            .await
            .expect("diff");
        assert_eq!(diff["submodules_ignored"], true, "{diff}");
        let description = repo.tool().description();
        assert!(description.contains("submodules_ignored"), "{description}");
    }

    #[tokio::test]
    async fn status_collapses_untracked_directories_and_flags_truncation() {
        let repo = TestRepo::new();
        std::fs::create_dir(repo.path().join("generated")).unwrap();
        for i in 0..6000 {
            repo.write(
                &format!("generated/file-with-a-long-generated-name-{i:06}.txt"),
                "x",
            );
        }
        let tool = repo.tool();
        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .expect("status of a huge untracked tree must succeed");
        assert_eq!(status["truncated"], false, "{status}");
        assert_eq!(status["counts"]["untracked"], 1);
        assert!(
            status["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r == "? generated/"),
            "{status}"
        );

        // Narrowing to a path lists files individually; the cap flags truncation.
        let narrowed = tool
            .call(args(GitOperation::Status, &["generated"], None))
            .await
            .expect("narrowed status");
        assert_eq!(narrowed["truncated"], true, "{narrowed}");
        assert!(narrowed["truncation_note"].is_string());
        let records = narrowed["records"].as_array().unwrap();
        assert!(!records.is_empty());
        assert!(
            records
                .iter()
                .filter(|r| r.as_str().unwrap().starts_with("? "))
                .all(|r| r.as_str().unwrap().ends_with(".txt")),
            "only complete records survive truncation"
        );

        // Mutations snapshot around the tool and keep working.
        let staged = tool
            .call(args(GitOperation::Stage, &["generated"], None))
            .await
            .expect("stage a large directory");
        assert_eq!(staged["exit_code"], 0, "{staged}");
        assert_eq!(repo.git(["ls-files"]).lines().count(), 6000);
    }

    fn commit_args(message: &str) -> GitArgs {
        args(GitOperation::Commit, &[], Some(message))
    }

    #[tokio::test]
    async fn commit_and_tag_disclose_that_hooks_and_signing_were_not_run() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        // A failing pre-commit hook must not run and must not block.
        let hooks = repo.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
        let tool = repo.tool();
        tool.call(args(GitOperation::Stage, &["a.txt"], None))
            .await
            .unwrap();
        let commit = tool.call(commit_args("first")).await.unwrap();
        assert_eq!(commit["status"], "success", "{commit}");
        assert_eq!(commit["hooks_run"], false);
        assert_eq!(commit["signed"], false);
        assert!(
            commit["notice"]
                .as_str()
                .unwrap()
                .contains("hooks and signing were not run")
        );
        let tag = tool.call(named(GitOperation::Tag, "v1")).await.unwrap();
        assert_eq!(tag["hooks_run"], false);
    }

    #[tokio::test]
    async fn commit_supports_signoff_allow_empty_and_guarded_amend() {
        let repo = TestRepo::new();
        repo.write("a.txt", "one\n");
        let tool = repo.tool();
        tool.call(args(GitOperation::Stage, &["a.txt"], None))
            .await
            .unwrap();
        tool.call(commit_args("first")).await.unwrap();

        // Nothing staged: refused without allow_empty, accepted with it.
        let refused = tool.call(commit_args("empty")).await.unwrap();
        assert_ne!(refused["status"], "success", "{refused}");
        let empty = tool
            .call(GitArgs {
                allow_empty: Some(true),
                signoff: Some(true),
                ..commit_args("empty")
            })
            .await
            .unwrap();
        assert_eq!(empty["status"], "success", "{empty}");
        assert!(
            repo.git(["log", "-1", "--format=%B"])
                .contains("Signed-off-by:")
        );

        // Amend replaces the unpushed HEAD instead of adding a commit.
        let before = repo.git(["rev-list", "--count", "HEAD"]);
        let amended = tool
            .call(GitArgs {
                amend: Some(true),
                allow_empty: Some(true),
                ..commit_args("reworded")
            })
            .await
            .unwrap();
        assert_eq!(amended["status"], "success", "{amended}");
        assert_eq!(amended["amended"], true);
        assert_eq!(repo.git(["rev-list", "--count", "HEAD"]), before);
        assert_eq!(repo.git(["log", "-1", "--format=%s"]).trim(), "reworded");

        // Once a remote-tracking ref contains HEAD, amend is refused.
        repo.git(["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let head = repo.git(["rev-parse", "HEAD"]);
        let error = tool
            .call(GitArgs {
                amend: Some(true),
                ..commit_args("rewrite published")
            })
            .await
            .expect_err("amend of published history must be refused");
        assert!(error.to_string().contains("remote-tracking"), "{error}");
        assert_eq!(repo.git(["rev-parse", "HEAD"]), head);
    }

    #[tokio::test]
    async fn amend_needs_a_commit_and_its_own_permission() {
        let repo = TestRepo::new();
        let tool = repo.tool();
        let error = tool
            .call(GitArgs {
                amend: Some(true),
                ..commit_args("x")
            })
            .await
            .expect_err("amend without a commit");
        assert!(error.to_string().contains("no commit to amend"), "{error}");

        repo.write("a.txt", "one\n");
        tool.call(args(GitOperation::Stage, &["a.txt"], None))
            .await
            .unwrap();
        tool.call(commit_args("first")).await.unwrap();
        let head = repo.git(["rev-parse", "HEAD"]);
        let denied = repo.tool_with_permission(PermissionConfig {
            git_commit: Some(ToolPerm::Simple(Action::Allow)),
            git_amend: Some(ToolPerm::Simple(Action::Deny)),
            ..PermissionConfig::default()
        });
        let error = denied
            .call(GitArgs {
                amend: Some(true),
                ..commit_args("again")
            })
            .await
            .expect_err("git/amend deny must block amend");
        assert!(error.to_string().contains("Permission denied"), "{error}");
        assert_eq!(repo.git(["rev-parse", "HEAD"]), head);
        // The commit flags are commit-only.
        let error = tool
            .call(GitArgs {
                amend: Some(true),
                ..args(GitOperation::Status, &[], None)
            })
            .await
            .expect_err("amend on status");
        assert!(error.to_string().contains("only by commit"));
    }

    fn read_args(operation: GitOperation) -> GitArgs {
        args(operation, &[], None)
    }

    #[tokio::test]
    async fn diff_and_log_accept_ranges_two_revisions_staged_and_skip() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        repo.git(["branch", "base"]);
        for i in 0..5 {
            commit_file(&repo, "a.txt", &format!("v{i}\n"), &format!("change {i}"));
        }
        let tool = repo.tool();

        // Range diff and log.
        let range = tool
            .call(GitArgs {
                revision: Some("base..HEAD".into()),
                format: Some(super::DiffFormat::NameStatus),
                ..read_args(GitOperation::Diff)
            })
            .await
            .expect("range diff");
        assert!(range["text"].as_str().unwrap().contains("a.txt"), "{range}");
        let triple = tool
            .call(GitArgs {
                revision: Some("base...HEAD".into()),
                ..read_args(GitOperation::Diff)
            })
            .await
            .expect("merge-base diff");
        assert!(triple["text"].as_str().unwrap().contains("+v4"), "{triple}");
        let log = tool
            .call(GitArgs {
                revision: Some("base..HEAD".into()),
                ..read_args(GitOperation::Log)
            })
            .await
            .expect("range log");
        assert_eq!(log["commits"].as_array().unwrap().len(), 5, "{log}");

        // Two-commit diff.
        let two = tool
            .call(GitArgs {
                revision: Some("HEAD~2".into()),
                revision_to: Some("HEAD".into()),
                ..read_args(GitOperation::Diff)
            })
            .await
            .expect("two revision diff");
        assert!(two["text"].as_str().unwrap().contains("-v2"), "{two}");
        assert!(two["text"].as_str().unwrap().contains("+v4"), "{two}");

        // Staged-only diff ignores unstaged edits.
        repo.write("a.txt", "staged\n");
        repo.git(["add", "a.txt"]);
        repo.write("a.txt", "unstaged\n");
        let staged = tool
            .call(GitArgs {
                staged: Some(true),
                ..read_args(GitOperation::Diff)
            })
            .await
            .expect("staged diff");
        let text = staged["text"].as_str().unwrap();
        assert!(
            text.contains("+staged") && !text.contains("unstaged"),
            "{text}"
        );

        // Log pages past max_count with skip / next_skip.
        let page1 = tool
            .call(GitArgs {
                max_count: Some(2),
                ..read_args(GitOperation::Log)
            })
            .await
            .unwrap();
        assert_eq!(page1["next_skip"], 2);
        let page2 = tool
            .call(GitArgs {
                max_count: Some(2),
                skip: page1["next_skip"].as_u64().map(|v| v as u32),
                ..read_args(GitOperation::Log)
            })
            .await
            .unwrap();
        let ids = |v: &serde_json::Value| -> Vec<String> {
            v["commits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["id"].as_str().unwrap().into())
                .collect()
        };
        assert!(ids(&page1).iter().all(|id| !ids(&page2).contains(id)));
        assert_eq!(page2["commits"][0]["subject"], "change 2");
        let filtered = tool
            .call(GitArgs {
                grep: Some("change 3".into()),
                ..read_args(GitOperation::Log)
            })
            .await
            .unwrap();
        assert_eq!(filtered["commits"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revision_errors_name_the_revision_and_the_failing_side() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        let tool = repo.tool();
        for (revision, expected) in [
            ("nope", "unknown Git revision \"nope\""),
            ("HEAD..nope", "unknown Git revision \"nope\""),
            ("nope..HEAD", "left side of the range"),
            ("HEAD..nope", "right side of the range"),
        ] {
            let error = tool
                .call(GitArgs {
                    revision: Some(revision.into()),
                    ..read_args(GitOperation::Log)
                })
                .await
                .expect_err(revision)
                .to_string();
            assert!(error.contains(expected), "{revision}: {error}");
        }
        let error = tool
            .call(GitArgs {
                revision: Some("HEAD".into()),
                ..read_args(GitOperation::Status)
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not supported by status"), "{error}");
        assert!(error.contains("accepted by diff, log, show"), "{error}");
    }

    // ---- mini-agent-nvnkx.50 / .316 / .317 / .49 -------------------------

    impl TestRepo {
        /// Runs a Git fixture command that may legitimately fail (a conflict).
        fn try_git(&self, args: &[&str]) -> bool {
            Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(args)
                .output()
                .expect("run git fixture command")
                .status
                .success()
        }

        fn commit_file(&self, path: &str, contents: &str, message: &str) {
            self.write(path, contents);
            self.git(["add", "--", path]);
            self.git(["commit", "--quiet", "-m", message]);
        }

        fn branch_name(&self) -> String {
            self.git(["branch", "--show-current"]).trim().to_string()
        }

        /// `f.txt` conflicts between the current branch and `topic`.
        fn conflicting_topic(&self) -> String {
            self.commit_file("f.txt", "base\n", "base");
            let base = self.branch_name();
            self.git(["checkout", "--quiet", "-b", "topic"]);
            self.commit_file("f.txt", "topic\n", "topic change");
            self.git(["checkout", "--quiet", base.as_str()]);
            self.commit_file("f.txt", "main\n", "main change");
            base
        }
    }

    fn rev_op(operation: GitOperation, revision: Option<&str>) -> GitArgs {
        GitArgs {
            operation,
            revision: revision.map(str::to_string),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn nvnkx316_status_reports_merge_cherry_pick_and_rebase_in_progress() {
        let repo = TestRepo::new();
        repo.conflicting_topic();
        let tool = repo.tool();
        let clean = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert!(clean.get("in_progress").is_none(), "{clean}");

        assert!(!repo.try_git(&["merge", "topic"]));
        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert_eq!(status["in_progress"], "merge", "{status}");
        assert!(
            status["in_progress_note"]
                .as_str()
                .unwrap()
                .contains("merge_abort")
        );
        repo.git(["merge", "--abort"]);

        assert!(!repo.try_git(&["cherry-pick", "topic"]));
        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert_eq!(status["in_progress"], "cherry-pick", "{status}");
        let refused = tool
            .call(args(GitOperation::Commit, &[], Some("mid cherry-pick")))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("cherry-pick is in progress"), "{refused}");
        repo.git(["cherry-pick", "--abort"]);

        assert!(!repo.try_git(&["rebase", "topic"]));
        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert_eq!(status["in_progress"], "rebase", "{status}");
        assert!(
            status["in_progress_note"]
                .as_str()
                .unwrap()
                .contains("shell")
        );
        let refused = tool
            .call(args(GitOperation::Commit, &[], Some("mid rebase")))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("rebase is in progress"), "{refused}");
        repo.git(["rebase", "--abort"]);
        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert!(status.get("in_progress").is_none(), "{status}");
    }

    #[tokio::test]
    async fn nvnkx50_merge_conflict_is_reported_and_aborted_without_running_hooks() {
        let repo = TestRepo::new();
        let base = repo.conflicting_topic();
        // Hostile hooks must never run.
        let marker = repo.path().join("hook-ran");
        let install_hooks = || {
            #[cfg(unix)]
            for hook in [
                "pre-merge-commit",
                "post-merge",
                "commit-msg",
                "pre-commit",
                "post-commit",
            ] {
                use std::os::unix::fs::PermissionsExt;
                let path = repo.path().join(".git/hooks").join(hook);
                std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))
                    .unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        };
        install_hooks();
        let tool = repo.tool();
        let result = tool
            .call(rev_op(GitOperation::Merge, Some("topic")))
            .await
            .unwrap();
        assert_eq!(result["status"], "nonzero", "{result}");
        assert_eq!(result["conflicted_paths"][0], "f.txt", "{result}");
        assert_eq!(result["in_progress"], "merge", "{result}");
        let again = tool
            .call(rev_op(GitOperation::Merge, Some("topic")))
            .await
            .unwrap_err()
            .to_string();
        assert!(again.contains("already in progress"), "{again}");
        let aborted = tool
            .call(rev_op(GitOperation::MergeAbort, None))
            .await
            .unwrap();
        assert_eq!(aborted["status"], "success", "{aborted}");
        assert!(aborted["in_progress"].is_null(), "{aborted}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
            "main\n"
        );
        let none = tool
            .call(rev_op(GitOperation::MergeAbort, None))
            .await
            .unwrap_err()
            .to_string();
        assert!(none.contains("no merge is in progress"), "{none}");

        assert!(!marker.exists(), "no hook may run");
        // The fixture's own commits would run the hooks; the tool must not.
        let _ = std::fs::remove_dir_all(repo.path().join(".git/hooks"));
        std::fs::create_dir(repo.path().join(".git/hooks")).unwrap();
        // A clean merge makes a merge commit and moves HEAD.
        repo.git(["checkout", "--quiet", "-b", "side", "topic~1"]);
        repo.commit_file("side.txt", "side\n", "side");
        repo.git(["checkout", "--quiet", base.as_str()]);
        install_hooks();
        let merged = tool
            .call(rev_op(GitOperation::Merge, Some("side")))
            .await
            .unwrap();
        assert_eq!(merged["status"], "success", "{merged}");
        assert!(repo.path().join("side.txt").exists());
        assert!(!marker.exists(), "no hook may run");
    }

    #[tokio::test]
    async fn nvnkx50_cherry_pick_and_revert_make_commits_and_report_conflicts() {
        let repo = TestRepo::new();
        repo.conflicting_topic();
        let tool = repo.tool();
        let picked = tool
            .call(rev_op(GitOperation::CherryPick, Some("topic")))
            .await
            .unwrap();
        assert_eq!(picked["status"], "nonzero", "{picked}");
        assert_eq!(picked["in_progress"], "cherry-pick", "{picked}");
        assert_eq!(picked["conflicted_paths"][0], "f.txt");
        let aborted = tool
            .call(rev_op(GitOperation::CherryPickAbort, None))
            .await
            .unwrap();
        assert_eq!(aborted["status"], "success", "{aborted}");

        let before = repo.git(["rev-parse", "HEAD"]);
        let reverted = tool
            .call(rev_op(GitOperation::Revert, Some("HEAD")))
            .await
            .unwrap();
        assert_eq!(reverted["status"], "success", "{reverted}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("f.txt")).unwrap(),
            "base\n"
        );
        assert_eq!(repo.git(["rev-parse", "HEAD~1"]), before);
        let range = tool
            .call(rev_op(GitOperation::Revert, Some("HEAD~2..HEAD")))
            .await
            .unwrap_err()
            .to_string();
        assert!(range.contains("single commit"), "{range}");
    }

    #[tokio::test]
    async fn nvnkx50_reset_moves_head_but_refuses_pushed_commits() {
        let repo = TestRepo::new();
        repo.commit_file("a.txt", "a\n", "one");
        repo.commit_file("a.txt", "b\n", "two");
        let tool = repo.tool();
        let mut args = rev_op(GitOperation::Reset, Some("HEAD~1"));
        args.mode = Some(super::ResetMode::Soft);
        let soft = tool.call(args).await.unwrap();
        assert_eq!(soft["status"], "success", "{soft}");
        assert_eq!(soft["mode"], "soft");
        assert_eq!(repo.git(["log", "--format=%s"]).trim(), "one");
        assert_eq!(
            repo.git(["diff", "--cached", "--name-only"]).trim(),
            "a.txt",
            "soft keeps the change staged"
        );
        repo.git(["commit", "--quiet", "-m", "two again"]);
        let mixed = tool
            .call(rev_op(GitOperation::Reset, Some("HEAD~1")))
            .await
            .unwrap();
        assert_eq!(mixed["status"], "success", "{mixed}");
        assert_eq!(repo.git(["diff", "--cached", "--name-only"]).trim(), "");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "b\n"
        );
        repo.git(["add", "a.txt"]);
        repo.git(["commit", "--quiet", "-m", "two once more"]);

        repo.git(["update-ref", "refs/remotes/origin/pushed", "HEAD"]);
        let head = repo.git(["rev-parse", "HEAD"]);
        let refused = tool
            .call(rev_op(GitOperation::Reset, Some("HEAD~1")))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("published history"), "{refused}");
        assert_eq!(repo.git(["rev-parse", "HEAD"]), head);
        let missing = tool
            .call(rev_op(GitOperation::Reset, Some("no-such-revision")))
            .await
            .unwrap_err()
            .to_string();
        assert!(missing.contains("unknown Git revision"), "{missing}");
    }

    #[tokio::test]
    async fn nvnkx50_branch_delete_rename_and_indexed_stash_operations() {
        let repo = TestRepo::new();
        repo.commit_file("a.txt", "a\n", "one");
        let base = repo.branch_name();
        repo.git(["branch", "gone"]);
        repo.git(["branch", "old-name"]);
        let tool = repo.tool();
        let named_op = |operation, name: &str| GitArgs {
            operation,
            name: Some(name.to_string()),
            ..Default::default()
        };
        let deleted = tool
            .call(named_op(GitOperation::BranchDelete, "gone"))
            .await
            .unwrap();
        assert_eq!(deleted["status"], "success", "{deleted}");
        assert!(!repo.git(["branch", "--list"]).contains("gone"));
        let current = tool
            .call(named_op(GitOperation::BranchDelete, &base))
            .await
            .unwrap_err()
            .to_string();
        assert!(current.contains("checked-out branch"), "{current}");
        let mut rename = named_op(GitOperation::BranchRename, "old-name");
        rename.new_name = Some("new-name".into());
        let renamed = tool.call(rename).await.unwrap();
        assert_eq!(renamed["status"], "success", "{renamed}");
        assert!(repo.git(["branch", "--list"]).contains("new-name"));

        // Two stashes: apply the older one by index and keep it, then drop.
        repo.write("a.txt", "first\n");
        repo.git(["stash", "push", "--quiet"]);
        repo.write("a.txt", "second\n");
        repo.write("untracked.txt", "u\n");
        let mut push = args(GitOperation::StashPush, &[], Some("with untracked"));
        push.include_untracked = Some(true);
        let pushed = tool.call(push).await.unwrap();
        assert_eq!(pushed["status"], "success", "{pushed}");
        assert!(!repo.path().join("untracked.txt").exists(), "{pushed}");
        let mut apply = rev_op(GitOperation::StashApply, None);
        apply.index = Some(1);
        let applied = tool.call(apply).await.unwrap();
        assert_eq!(applied["status"], "success", "{applied}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "first\n"
        );
        assert_eq!(
            repo.git(["stash", "list"]).lines().count(),
            2,
            "apply keeps the stash"
        );
        let mut drop_args = rev_op(GitOperation::StashDrop, None);
        drop_args.index = Some(0);
        let dropped = tool.call(drop_args).await.unwrap();
        assert_eq!(dropped["status"], "success", "{dropped}");
        assert_eq!(repo.git(["stash", "list"]).lines().count(), 1);
        let mut missing = rev_op(GitOperation::StashDrop, None);
        missing.index = Some(9);
        let error = tool.call(missing).await.unwrap_err().to_string();
        assert!(error.contains("does not exist"), "{error}");
    }

    #[tokio::test]
    async fn nvnkx50_new_fields_are_rejected_on_other_operations() {
        let repo = TestRepo::new();
        let tool = repo.tool();
        for (field, args) in [
            (
                "mode",
                GitArgs {
                    mode: Some(super::ResetMode::Soft),
                    ..args(GitOperation::Status, &[], None)
                },
            ),
            (
                "new_name",
                GitArgs {
                    new_name: Some("x".into()),
                    ..args(GitOperation::Status, &[], None)
                },
            ),
            (
                "index",
                GitArgs {
                    index: Some(0),
                    ..args(GitOperation::Status, &[], None)
                },
            ),
            (
                "include_untracked",
                GitArgs {
                    include_untracked: Some(true),
                    ..args(GitOperation::Status, &[], None)
                },
            ),
            (
                "allow_in_progress",
                GitArgs {
                    allow_in_progress: Some(true),
                    ..args(GitOperation::Status, &[], None)
                },
            ),
        ] {
            let error = tool.call(args).await.unwrap_err().to_string();
            assert!(error.contains(field), "{field}: {error}");
        }
    }

    #[tokio::test]
    async fn nvnkx317_lfs_repositories_report_unresolved_pointers_and_refuse_lfs_paths() {
        let repo = TestRepo::new();
        repo.commit_file("a.bin", "pretend-binary\n", "binary");
        repo.commit_file("plain.txt", "plain\n", "plain");
        // Installed after the commits so no real LFS program is involved.
        repo.write(
            ".gitattributes",
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        repo.git(["config", "filter.lfs.clean", "git-lfs clean -- %f"]);
        repo.git(["config", "filter.lfs.smudge", "git-lfs smudge -- %f"]);
        repo.git(["config", "filter.lfs.process", "git-lfs filter-process"]);
        repo.git(["config", "filter.lfs.required", "true"]);
        repo.write("plain.txt", "plain changed\n");
        repo.write("other.bin", "new binary\n");
        repo.write("other.txt", "other\n");
        let tool = repo.tool();

        let status = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .unwrap();
        assert_eq!(status["lfs_pointers_not_resolved"], true, "{status}");
        assert_eq!(status["lfs_paths"][0], "a.bin", "{status}");
        assert!(status["records"].to_string().contains("plain.txt"));
        let diff = tool
            .call(args(GitOperation::Diff, &["plain.txt"], None))
            .await
            .unwrap();
        assert!(
            diff["text"].as_str().unwrap().contains("+plain changed"),
            "{diff}"
        );
        assert_eq!(diff["lfs_pointers_not_resolved"], true);

        let refused = tool
            .call(args(GitOperation::Stage, &["other.bin"], None))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("tracked by Git LFS"), "{refused}");
        assert!(refused.contains("shell"), "{refused}");
        assert_eq!(repo.git(["diff", "--cached", "--name-only"]).trim(), "");
        let staged = tool
            .call(args(GitOperation::Stage, &["other.txt"], None))
            .await
            .unwrap();
        assert_eq!(staged["status"], "success", "{staged}");
        assert_eq!(staged["lfs_pointers_not_resolved"], true);
        let by_path = tool
            .call(args(GitOperation::Commit, &["other.bin"], Some("bin")))
            .await
            .unwrap_err()
            .to_string();
        assert!(by_path.contains("tracked by Git LFS"), "{by_path}");
        // Operations that would rewrite the working tree keep refusing.
        let switch = tool
            .call(GitArgs {
                operation: GitOperation::Restore,
                paths: vec!["plain.txt".into()],
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(switch.contains("uses Git LFS"), "{switch}");
    }

    #[tokio::test]
    async fn nvnkx317_a_non_canonical_lfs_driver_is_still_refused() {
        let repo = TestRepo::new();
        repo.commit_file("a.bin", "x\n", "binary");
        repo.write(".gitattributes", "*.bin filter=lfs\n");
        repo.git(["config", "filter.lfs.clean", "sh -c evil"]);
        let error = repo
            .tool()
            .call(args(GitOperation::Status, &[], None))
            .await;
        // The runner empties repository-defined commands; either the value is
        // seen as non-canonical (refused) or neutralised (reported as LFS) but
        // nothing may execute.
        if let Err(error) = error {
            assert!(error.to_string().contains("filter"), "{error}");
        }
    }

    #[tokio::test]
    async fn nvnkx49_restore_checkpoints_the_discarded_content_for_rewind() {
        let repo = TestRepo::new();
        repo.commit_file("tracked.txt", "committed\n", "base");
        repo.write("tracked.txt", "hand edited, never committed\n");
        let tool = repo.tool();
        let workspace = Arc::clone(&tool.workspace);
        let store = crate::session::file_checkpoints::FileCheckpoints::default();
        let recorder = store.recorder("session", 3, true);
        let result = crate::session::file_checkpoints::scope(Some(recorder), async {
            tool.call(args(GitOperation::Restore, &["tracked.txt"], None))
                .await
        })
        .await
        .unwrap();
        assert_eq!(result["status"], "success", "{result}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "committed\n"
        );
        let restored = store.restore("session", 3, &workspace).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
            "hand edited, never committed\n"
        );
    }

    #[tokio::test]
    async fn nvnkx49_switch_flags_the_rewind_range_as_partly_unrestorable() {
        let repo = TestRepo::new();
        repo.commit_file("a.txt", "a\n", "one");
        let base = repo.branch_name();
        repo.git(["checkout", "--quiet", "-b", "other"]);
        repo.commit_file("b.txt", "b\n", "two");
        repo.git(["checkout", "--quiet", base.as_str()]);
        let tool = repo.tool();
        let store = crate::session::file_checkpoints::FileCheckpoints::default();
        let recorder = store.recorder("session", 2, true);
        let result = crate::session::file_checkpoints::scope(Some(recorder), async {
            tool.call(GitArgs {
                operation: GitOperation::Switch,
                name: Some("other".into()),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert!(
            result["checkpoint_notice"]
                .as_str()
                .unwrap()
                .contains("git operation"),
            "{result}"
        );
        let summary = store.summary_from("session", 2);
        assert!(summary.contains("1 edit not restorable"), "{summary}");
        assert!(summary.contains("shell or git commands"), "{summary}");
    }

    /// A definition is paid for in every request (mini-agent-sskdu.174).
    #[test]
    fn the_git_definition_stays_within_its_size_budget() {
        let repo = TestRepo::new();
        let tool = repo.tool();
        let size = tool.description().len() + tool.parameters().to_string().len();
        assert!(size <= 5_900, "git definition is {size} bytes");
    }

    #[test]
    fn only_timeouts_get_the_retry_guidance() {
        use super::{GitRunError, timeout_guidance};
        let timeout = GitRunError::TimedOut {
            operation: "log".into(),
            millis: 5,
        };
        let guided = timeout_guidance(timeout).to_string();
        assert!(guided.contains("timeout_secs"), "{guided}");
        let overflow = GitRunError::OutputLimit {
            operation: "log".into(),
            limit: "Stdout".into(),
        };
        let plain = overflow.to_string();
        assert_eq!(timeout_guidance(overflow).to_string(), plain);
        // Git's own stderr that mentions a timeout is not a timed-out command.
        let refused = GitRunError::Failed {
            operation: "log".into(),
            stderr: "fatal: timed out after waiting".into(),
        };
        let plain = refused.to_string();
        assert_eq!(timeout_guidance(refused).to_string(), plain);
    }

    #[tokio::test]
    async fn revision_ranges_reject_option_and_malformed_operands() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        let tool = repo.tool();
        for bad in [
            "--output=/tmp/pwn..HEAD",
            "HEAD..--output=/tmp/pwn",
            "..HEAD",
            "HEAD..",
            "HEAD..nope",
            "HEAD...nope",
            "a..b..c",
        ] {
            for op in [GitOperation::Diff, GitOperation::Log] {
                let error = tool
                    .call(GitArgs {
                        revision: Some(bad.into()),
                        ..read_args(op)
                    })
                    .await
                    .expect_err(bad);
                assert!(error.to_string().contains("Git revision"), "{bad}: {error}");
            }
        }
        for combo in [
            GitArgs {
                revision: Some("HEAD..HEAD".into()),
                revision_to: Some("HEAD".into()),
                ..read_args(GitOperation::Diff)
            },
            GitArgs {
                revision: Some("HEAD..HEAD".into()),
                staged: Some(true),
                ..read_args(GitOperation::Diff)
            },
            GitArgs {
                revision_to: Some("HEAD".into()),
                ..read_args(GitOperation::Diff)
            },
            GitArgs {
                revision_to: Some("--output=x".into()),
                revision: Some("HEAD".into()),
                ..read_args(GitOperation::Diff)
            },
            GitArgs {
                staged: Some(true),
                ..read_args(GitOperation::Log)
            },
            GitArgs {
                skip: Some(1),
                ..read_args(GitOperation::Diff)
            },
        ] {
            assert!(tool.call(combo).await.is_err());
        }
        // Option-looking author/grep values stay values (single `--author=` argv).
        let log = tool
            .call(GitArgs {
                author: Some("--output=/tmp/pwn".into()),
                ..read_args(GitOperation::Log)
            })
            .await
            .unwrap();
        assert!(log["commits"].as_array().unwrap().is_empty());
        assert!(!Path::new("/tmp/pwn").exists());
    }

    #[tokio::test]
    async fn blame_reports_line_authorship_for_one_path() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\ntwo\nthree\n", "first");
        let tool = repo.tool();
        let blame = tool
            .call(GitArgs {
                paths: vec!["a.txt".into()],
                start_line: Some(2),
                end_line: Some(3),
                ..read_args(GitOperation::Blame)
            })
            .await
            .expect("blame range");
        let text = blame["text"].as_str().unwrap();
        assert!(
            text.contains("two") && text.contains("three") && !text.contains("one"),
            "{text}"
        );
        assert!(tool.call(read_args(GitOperation::Blame)).await.is_err());
        assert!(
            tool.call(GitArgs {
                paths: vec!["../x".into()],
                ..read_args(GitOperation::Blame)
            })
            .await
            .is_err()
        );
        assert!(
            tool.call(GitArgs {
                paths: vec!["a.txt".into()],
                start_line: Some(3),
                end_line: Some(1),
                ..read_args(GitOperation::Blame)
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn stage_denial_has_no_index_effect() {
        let repo = TestRepo::new();
        repo.write("denied.txt", "content\n");
        let tool = repo.tool_with_permission(PermissionConfig {
            git_stage: Some(ToolPerm::Simple(Action::Deny)),
            ..PermissionConfig::default()
        });

        let error = tool
            .call(args(GitOperation::Stage, &["denied.txt"], None))
            .await
            .expect_err("stage permission must deny the mutation");

        assert!(error.to_string().contains("Permission denied"));
        assert!(repo.git(["diff", "--cached", "--name-only"]).is_empty());
    }

    #[tokio::test]
    async fn stage_rejects_paths_with_external_filters() {
        let repo = TestRepo::new();
        repo.write(".gitattributes", "filtered.txt filter=external\n");
        repo.write("filtered.txt", "content\n");

        let error = repo
            .tool()
            .call(args(GitOperation::Stage, &["filtered.txt"], None))
            .await
            .expect_err("stage must reject external transforms");

        assert!(error.to_string().contains("external transform"));
        assert!(repo.git(["diff", "--cached", "--name-only"]).is_empty());
    }

    #[tokio::test]
    async fn stage_expands_directory_operands_before_checking_attributes() {
        let repo = TestRepo::new();
        std::fs::create_dir(repo.path().join("nested")).unwrap();
        repo.write(".gitattributes", "nested/filtered.txt filter=external\n");
        repo.write("nested/filtered.txt", "content\n");

        let error = repo
            .tool()
            .call(args(GitOperation::Stage, &["nested"], None))
            .await
            .expect_err("directory staging must inspect each affected file");

        assert!(error.to_string().contains("external transform"));
        assert!(repo.git(["diff", "--cached", "--name-only"]).is_empty());
    }

    /// Recreates `tracked.txt` with identical content so its cached stat data
    /// no longer matches and any index refresh must hash (and so clean-filter)
    /// the working-tree file.
    #[cfg(unix)]
    fn make_stat_dirty(repo: &TestRepo) {
        std::fs::remove_file(repo.path().join("tracked.txt")).expect("remove tracked file");
        repo.write("tracked.txt", "content\n");
    }

    /// Sets `filter.probe.clean` in the repository-local config to a command
    /// that leaves a marker file whenever Git executes it.
    #[cfg(unix)]
    fn configure_marker_filter(repo: &TestRepo) -> std::path::PathBuf {
        let marker = repo.path().join(".git").join("filter-ran");
        let command = format!("touch '{}'; cat", marker.display());
        repo.git(["config", "filter.probe.clean", command.as_str()]);
        marker
    }

    /// A committed `tracked.txt` bound by `.gitattributes` to a
    /// repository-configured clean filter, left stat-dirty. The fixture first
    /// proves that plain `git status` really executes the filter here.
    #[cfg(unix)]
    fn clean_filter_fixture() -> (TestRepo, std::path::PathBuf) {
        let repo = TestRepo::new();
        repo.write("tracked.txt", "content\n");
        repo.write(".gitattributes", "tracked.txt filter=probe\n");
        repo.git(["add", "tracked.txt", ".gitattributes"]);
        repo.git(["commit", "--quiet", "-m", "base"]);
        let marker = configure_marker_filter(&repo);
        make_stat_dirty(&repo);
        repo.git(["status", "--porcelain"]);
        assert!(
            marker.exists(),
            "fixture must demonstrate that an index refresh runs the clean filter"
        );
        std::fs::remove_file(&marker).expect("reset filter marker");
        make_stat_dirty(&repo);
        (repo, marker)
    }

    #[cfg(unix)]
    fn assert_filter_refusal(error: &super::ToolError, marker: &Path) {
        let error = error.to_string();
        assert!(
            error.contains("`filter`") && error.contains("probe"),
            "refusal must name the attribute and driver: {error}"
        );
        assert!(!marker.exists(), "the workspace clean filter must not run");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn filter_driver_probe_is_reused_until_repository_config_changes() {
        let repo = TestRepo::new();
        repo.write("tracked.txt", "content\n");
        repo.git(["add", "tracked.txt"]);
        repo.git(["commit", "--quiet", "-m", "base"]);
        let tool = repo.tool();
        let probes = || {
            tool.filter_drivers
                .probes
                .load(std::sync::atomic::Ordering::SeqCst)
        };
        for _ in 0..3 {
            tool.call(args(GitOperation::Status, &[], None))
                .await
                .expect("status");
        }
        assert_eq!(probes(), 1, "unchanged config must not re-probe");

        // A same-length in-place config edit still invalidates the cache.
        repo.write("tracked.txt", "changed\n");
        repo.write(".gitattributes", "tracked.txt filter=probe\n");
        repo.git(["add", ".gitattributes"]);
        let marker = configure_marker_filter(&repo);
        let after = tool.call(args(GitOperation::Status, &[], None)).await;
        assert_eq!(probes(), 2, "changed config must re-probe");
        assert!(!marker.exists(), "the workspace clean filter must not run");
        drop(after);

        // `include` makes the config unkeyable: always re-probe.
        repo.git(["config", "include.path", "extra.cfg"]);
        let before = probes();
        for _ in 0..2 {
            let _ = tool.call(args(GitOperation::Status, &[], None)).await;
        }
        assert_eq!(probes(), before + 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_status_refuses_or_ignores_workspace_clean_filter() {
        let (repo, marker) = clean_filter_fixture();

        let error = repo
            .tool()
            .call(args(GitOperation::Status, &[], None))
            .await
            .expect_err("status must refuse a configured clean filter");

        assert_filter_refusal(&error, &marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_diff_refuses_or_ignores_workspace_clean_filter() {
        let (repo, marker) = clean_filter_fixture();
        let tool = repo.tool();

        let error = tool
            .call(args(GitOperation::Diff, &[], None))
            .await
            .expect_err("worktree diff must refuse a configured clean filter");
        assert_filter_refusal(&error, &marker);

        let mut against_head = args(GitOperation::Diff, &["tracked.txt"], None);
        against_head.revision = Some("HEAD".into());
        let error = tool
            .call(against_head)
            .await
            .expect_err("revision-to-worktree diff must refuse a configured clean filter");
        assert_filter_refusal(&error, &marker);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_stage_snapshot_does_not_run_filters() {
        let (repo, marker) = clean_filter_fixture();
        repo.write("unfiltered.txt", "plain\n");

        let error = repo
            .tool()
            .call(args(GitOperation::Stage, &["unfiltered.txt"], None))
            .await
            .expect_err("stage snapshots must not refresh a filtered index entry");

        assert_filter_refusal(&error, &marker);
        assert!(repo.git(["diff", "--cached", "--name-only"]).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_unstage_and_commit_do_not_run_filters() {
        let (repo, marker) = clean_filter_fixture();
        let tool = repo.tool();

        let error = tool
            .call(args(GitOperation::Unstage, &["tracked.txt"], None))
            .await
            .expect_err("unstage must refuse a configured clean filter");
        assert_filter_refusal(&error, &marker);

        let head = repo.git(["rev-parse", "HEAD"]);
        let error = tool
            .call(args(
                GitOperation::Commit,
                &[],
                Some("refresh would filter"),
            ))
            .await
            .expect_err("commit must refuse a configured clean filter");
        assert_filter_refusal(&error, &marker);
        assert_eq!(repo.git(["rev-parse", "HEAD"]), head);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn filter_neutralization_covers_attributes_added_after_the_probe() {
        let repo = TestRepo::new();
        repo.write("tracked.txt", "content\n");
        repo.git(["add", "tracked.txt"]);
        repo.git(["commit", "--quiet", "-m", "base"]);
        let marker = configure_marker_filter(&repo);
        let tool = repo.tool();

        let guard = tool
            .filter_guard("status")
            .await
            .expect("no attribute binds the driver yet");
        assert!(guard.iter().any(|arg| arg == "filter.probe.clean="));

        // A concurrent writer binds the driver after the probe has passed.
        repo.write(".gitattributes", "tracked.txt filter=probe\n");
        make_stat_dirty(&repo);
        tool.status_snapshot(&guard)
            .await
            .expect("status runs with every configured driver neutralised");

        assert!(!marker.exists(), "a neutralised driver must not run");
    }

    #[tokio::test]
    async fn filter_attributes_without_a_configured_driver_do_not_block_reads() {
        let repo = TestRepo::new();
        repo.write(
            ".gitattributes",
            "tracked.txt filter=mini-agent-unconfigured\n",
        );
        repo.write("tracked.txt", "content\n");
        repo.git(["add", "tracked.txt", ".gitattributes"]);
        repo.git(["commit", "--quiet", "-m", "base"]);
        repo.write("tracked.txt", "changed\n");
        let tool = repo.tool();

        let snapshot = tool
            .call(args(GitOperation::Status, &[], None))
            .await
            .expect("an attribute naming no configured driver executes nothing");
        assert!(snapshot["records"].to_string().contains("tracked.txt"));
        let diff = tool
            .call(args(GitOperation::Diff, &[], None))
            .await
            .expect("diff an unconfigured filter attribute");
        assert!(diff["text"].as_str().unwrap().contains("+changed"));
    }

    #[test]
    fn filter_driver_names_are_parsed_from_nul_terminated_config() {
        let names = super::parse_filter_driver_names(
            b"filter.lfs.required\ntrue\0filter.lfs.clean\ngit-lfs clean -- %f\0\
              filter.dotted.name.process\ncmd\0filter.flag\0filter.bare.smudge\0",
        )
        .expect("parse driver names");
        assert_eq!(names, vec!["bare", "dotted.name", "lfs"]);
        assert_eq!(
            super::neutralizing_args(&names[2..]),
            vec![
                "-c",
                "filter.lfs.clean=",
                "-c",
                "filter.lfs.process=",
                "-c",
                "filter.lfs.smudge=",
                "-c",
                "filter.lfs.required=false",
            ]
        );

        let error = super::parse_filter_driver_names(b"filter.a=b.clean\ncmd\0")
            .expect_err("a driver name that -c cannot express must fail closed");
        assert!(error.to_string().contains("filter driver"));
        assert!(super::parse_filter_driver_names(b"filter.\xff.clean\ncmd\0").is_err());
    }

    #[tokio::test]
    async fn read_operations_reject_fields_they_do_not_use() {
        let repo = TestRepo::new();
        let mut status = args(GitOperation::Status, &["ignored.txt"], None);
        status.revision = Some("HEAD".into());
        let error = repo.tool().call(status).await.unwrap_err().to_string();
        assert!(error.contains("revision") || error.contains("paths"));

        let mut diff = args(GitOperation::Diff, &[], Some("ignored"));
        diff.max_count = Some(2);
        assert!(repo.tool().call(diff).await.is_err());

        let mut show = args(GitOperation::Show, &[], None);
        show.revision = Some("HEAD".into());
        show.max_count = Some(2);
        assert!(repo.tool().call(show).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stage_rejects_symbolic_link_operands() {
        use std::os::unix::fs::symlink;

        let repo = TestRepo::new();
        repo.write("target.txt", "target\n");
        symlink("target.txt", repo.path().join("link.txt")).expect("create symlink fixture");
        let error = repo
            .tool()
            .call(args(GitOperation::Stage, &["link.txt"], None))
            .await
            .expect_err("stage must reject symlink operands");
        assert!(error.to_string().contains("symbolic-link"));
        assert!(repo.git(["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn hardened_args_enforces_policy_before_preserving_literal_operands() {
        use std::collections::{BTreeMap, BTreeSet};

        let expected_config = BTreeMap::from([
            ("core.fsmonitor", "false"),
            ("core.untrackedCache", "false"),
            ("core.hooksPath", "/dev/null"),
            ("core.attributesFile", "/dev/null"),
            ("commit.gpgSign", "false"),
            ("tag.gpgSign", "false"),
            ("diff.external", ""),
            ("diff.trustExitCode", "false"),
            ("credential.helper", ""),
            ("core.askPass", ""),
            ("submodule.recurse", "false"),
            ("fetch.recurseSubmodules", "false"),
            ("protocol.file.allow", "never"),
            ("protocol.ext.allow", "never"),
        ]);
        for caller in [
            vec![],
            vec!["log", "--oneline"],
            vec!["commit", "--file=-"],
            vec!["show", "--", "-c", "file name", "unicodé", "*.txt"],
        ] {
            let caller = caller.into_iter().map(String::from).collect::<Vec<_>>();
            let result = hardened_args(caller.clone());
            assert!(result.len() >= caller.len());
            let (policy, operands) = result.split_at(result.len() - caller.len());
            assert_eq!(operands, caller, "caller operands changed");
            let mut flags = BTreeSet::new();
            let mut config = BTreeMap::new();
            let mut arguments = policy.iter();
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--no-optional-locks" | "--literal-pathspecs" => {
                        assert!(flags.insert(argument.as_str()), "duplicate global flag");
                    }
                    "-c" => {
                        let (key, value) = arguments
                            .next()
                            .expect("configuration flag requires a value")
                            .split_once('=')
                            .expect("configuration requires key=value");
                        assert!(
                            config.insert(key, value).is_none(),
                            "duplicate policy key {key}"
                        );
                    }
                    other => panic!("unexpected argument before the command: {other}"),
                }
            }
            assert_eq!(
                flags,
                BTreeSet::from(["--no-optional-locks", "--literal-pathspecs"])
            );
            assert_eq!(config, expected_config);
        }
    }

    fn isolated_host_config() -> Vec<(String, std::ffi::OsString)> {
        // Point the host-side `git config` lookup at a global config that does
        // not exist so the test never observes the developer's real identity.
        let missing = std::env::temp_dir().join(format!(
            "mini-agent-git-no-global-config-{}",
            uuid::Uuid::new_v4()
        ));
        vec![("GIT_CONFIG_GLOBAL".to_string(), missing.into_os_string())]
    }

    fn named(operation: GitOperation, name: &str) -> GitArgs {
        GitArgs {
            operation,
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    fn commit_file(repo: &TestRepo, path: &str, contents: &str, message: &str) {
        repo.write(path, contents);
        repo.git(["add", "--", path]);
        repo.git(["commit", "--quiet", "-m", message]);
    }

    fn branches_args(scope: super::RefScope, skip: Option<u32>, max: Option<u16>) -> GitArgs {
        GitArgs {
            operation: GitOperation::Branches,
            scope: Some(scope),
            skip,
            max_count: max,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn branches_lists_remote_tracking_refs_and_tags_without_network() {
        use super::RefScope;
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        repo.git(["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git(["update-ref", "refs/remotes/origin/topic", "HEAD"]);
        repo.git([
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        repo.git(["tag", "v1-light"]);
        repo.git(["tag", "-a", "v2-annotated", "-m", "release two"]);
        let tool = repo.tool();
        let names = |value: &serde_json::Value, key: &str| -> Vec<String> {
            value[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["name"].as_str().unwrap().to_string())
                .collect()
        };
        let local = tool
            .call(branches_args(RefScope::Local, None, None))
            .await
            .unwrap();
        assert!(
            !names(&local, "branches")
                .iter()
                .any(|n| n.starts_with("origin/")),
            "{local}"
        );
        let remote = tool
            .call(branches_args(RefScope::Remote, None, None))
            .await
            .unwrap();
        let remote_names = names(&remote, "branches");
        assert!(
            remote_names.contains(&"origin/main".to_string()),
            "{remote}"
        );
        assert!(
            remote_names.contains(&"origin/topic".to_string()),
            "{remote}"
        );
        let head = remote["branches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == "origin/HEAD")
            .unwrap();
        assert_eq!(head["symref"], "refs/remotes/origin/main", "{remote}");
        assert_eq!(head["kind"], "remote");
        let all = tool
            .call(branches_args(RefScope::All, None, None))
            .await
            .unwrap();
        let all_names = names(&all, "branches");
        assert!(
            all_names.contains(&"origin/main".to_string()) && all_names.len() > remote_names.len(),
            "{all}"
        );
        let tags = tool
            .call(branches_args(RefScope::Tags, None, None))
            .await
            .unwrap();
        let listed = tags["tags"].as_array().unwrap();
        assert_eq!(listed.len(), 2, "{tags}");
        let light = listed.iter().find(|t| t["name"] == "v1-light").unwrap();
        let annotated = listed.iter().find(|t| t["name"] == "v2-annotated").unwrap();
        assert_eq!(light["annotated"], false);
        assert_eq!(annotated["annotated"], true);
        let head_id = repo.git(["rev-parse", "HEAD"]).trim().to_string();
        assert_eq!(annotated["target"], head_id.as_str(), "{tags}");
        assert_eq!(annotated["subject"], "release two");
        assert_eq!(tags["truncated"], false);
        // Scope and timeout belong to specific operations.
        let bad = GitArgs {
            operation: GitOperation::Status,
            scope: Some(RefScope::Tags),
            ..Default::default()
        };
        assert!(tool.call(bad).await.is_err());
        let bad = GitArgs {
            operation: GitOperation::Status,
            timeout_secs: Some(60),
            ..Default::default()
        };
        assert!(tool.call(bad).await.is_err());
        let bad = GitArgs {
            operation: GitOperation::Log,
            timeout_secs: Some(500),
            ..Default::default()
        };
        assert!(tool.call(bad).await.is_err());
    }

    #[tokio::test]
    async fn branches_pages_through_thousands_of_refs() {
        use super::RefScope;
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        let id = repo.git(["rev-parse", "HEAD"]).trim().to_string();
        let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
        for i in 0..3000 {
            packed.push_str(&format!("{id} refs/heads/bulk/b{i:05}\n"));
        }
        std::fs::write(repo.path().join(".git/packed-refs"), packed).unwrap();
        let tool = repo.tool();
        let mut seen = std::collections::BTreeSet::new();
        let mut skip = 0u32;
        let mut pages = 0;
        loop {
            let page = tool
                .call(branches_args(RefScope::Local, Some(skip), Some(1000)))
                .await
                .unwrap();
            pages += 1;
            for entry in page["branches"].as_array().unwrap() {
                assert!(seen.insert(entry["name"].as_str().unwrap().to_string()));
            }
            if page["truncated"] == false {
                assert!(page["next_skip"].is_null(), "{page}");
                break;
            }
            assert!(page["note"].as_str().unwrap().contains("skip"), "{page}");
            skip = page["next_skip"].as_u64().unwrap() as u32;
            assert!(pages < 10);
        }
        // 3000 bulk branches plus the current one.
        assert_eq!(seen.len(), 3001);
        assert_eq!(pages, 4);
    }

    #[test]
    fn timed_out_log_returns_partial_commits_with_a_continuation() {
        let record = |n: u32| {
            format!(
                "{n:040x}\0\0A\0a@example.invalid\02026-01-01T00:00:00+00:00\0subject {n}\0\x1eNONCE\n"
            )
        };
        let mut stdout = record(1).into_bytes();
        stdout.extend_from_slice(record(2).as_bytes());
        stdout.extend_from_slice(b"0000000000000000000000000000000000000003\0par");
        let output = crate::sandbox::CommandOutput {
            exit_status: None,
            stdout,
            stderr: vec![],
            status: crate::sandbox::CommandStatus::TimedOut,
            descendants_escaped: false,
        };
        let value = super::render_log_result(
            &output,
            10,
            20,
            std::time::Duration::from_secs(30),
            None,
            b"\x1eNONCE",
        )
        .unwrap();
        assert_eq!(value["commits"].as_array().unwrap().len(), 2, "{value}");
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["next_skip"], 12);
        let note = value["note"].as_str().unwrap();
        assert!(
            note.contains("timed out") && note.contains("timeout_secs"),
            "{note}"
        );
    }

    #[tokio::test]
    async fn branch_create_list_and_switch_move_head_between_branches() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        let tool = repo.tool();
        let created = tool
            .call(named(GitOperation::BranchCreate, "feature/x"))
            .await
            .unwrap();
        assert_eq!(created["status"], "success", "{created}");
        let listed = tool
            .call(GitArgs {
                operation: GitOperation::Branches,
                ..Default::default()
            })
            .await
            .unwrap();
        let names: Vec<_> = listed["branches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|branch| branch["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"feature/x".to_string()), "{listed}");
        let switched = tool
            .call(named(GitOperation::Switch, "feature/x"))
            .await
            .unwrap();
        assert_eq!(switched["status"], "success", "{switched}");
        assert_eq!(repo.git(["branch", "--show-current"]), "feature/x\n");
        // Git's own ref rules reject bad names before anything runs.
        for bad in ["-x", "a..b", "with space", "end.lock"] {
            assert!(
                tool.call(named(GitOperation::BranchCreate, bad))
                    .await
                    .is_err(),
                "{bad}"
            );
        }
        let missing = tool
            .call(named(GitOperation::Switch, "nope"))
            .await
            .unwrap();
        assert_eq!(missing["status"], "nonzero", "{missing}");
    }

    #[tokio::test]
    async fn stash_push_list_pop_round_trips_working_tree_changes() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        repo.write("a.txt", "changed\n");
        let tool = repo.tool();
        let pushed = tool
            .call(GitArgs {
                operation: GitOperation::StashPush,
                message: Some("wip change".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(pushed["status"], "success", "{pushed}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "one\n"
        );
        let listed = tool
            .call(GitArgs {
                operation: GitOperation::StashList,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(listed["stashes"][0]["ref"], "stash@{0}", "{listed}");
        assert!(
            listed["stashes"][0]["subject"]
                .as_str()
                .unwrap()
                .contains("wip change")
        );
        let popped = tool
            .call(GitArgs {
                operation: GitOperation::StashPop,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(popped["status"], "success", "{popped}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "changed\n"
        );
    }

    #[tokio::test]
    async fn tag_creates_lightweight_and_annotated_tags() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        let tool = repo.tool();
        let light = tool.call(named(GitOperation::Tag, "v1")).await.unwrap();
        assert_eq!(light["status"], "success", "{light}");
        let annotated = tool
            .call(GitArgs {
                message: Some("release two".into()),
                ..named(GitOperation::Tag, "v2")
            })
            .await
            .unwrap();
        assert_eq!(annotated["status"], "success", "{annotated}");
        assert_eq!(repo.git(["cat-file", "-t", "v1"]), "commit\n");
        assert_eq!(repo.git(["cat-file", "-t", "v2"]), "tag\n");
        assert!(repo.git(["tag", "-n1", "v2"]).contains("release two"));
    }

    #[tokio::test]
    async fn restore_discards_working_tree_changes_of_selected_paths_only() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        commit_file(&repo, "b.txt", "two\n", "second");
        repo.write("a.txt", "dirty a\n");
        repo.write("b.txt", "dirty b\n");
        let tool = repo.tool();
        let restored = tool
            .call(args(GitOperation::Restore, &["a.txt"], None))
            .await
            .unwrap();
        assert_eq!(restored["status"], "success", "{restored}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("b.txt")).unwrap(),
            "dirty b\n"
        );
        assert!(
            tool.call(args(GitOperation::Restore, &[], None))
                .await
                .is_err()
        );
        assert!(
            tool.call(args(GitOperation::Restore, &["../x"], None))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn large_diff_is_paged_by_offset_and_has_stat_and_name_status_forms() {
        let repo = TestRepo::new();
        commit_file(&repo, "big.txt", "seed\n", "first");
        commit_file(&repo, "small.txt", "seed\n", "second");
        let big: String = (0..12_000)
            .map(|line| format!("changed line {line:06} with padding\n"))
            .collect();
        repo.write("big.txt", &big);
        repo.write("small.txt", "edited\n");
        let tool = repo.tool();
        let mut offset = None;
        let mut pages = 0;
        let mut text = String::new();
        loop {
            let page = tool
                .call(GitArgs {
                    operation: GitOperation::Diff,
                    offset,
                    ..Default::default()
                })
                .await
                .unwrap();
            pages += 1;
            assert_eq!(page["truncated"], false);
            let chunk = page["text"].as_str().unwrap();
            assert!(chunk.len() <= super::text_page_bytes());
            text.push_str(chunk);
            match page["next_offset"].as_u64() {
                Some(next) => offset = Some(next),
                None => {
                    assert_eq!(page["total_bytes"].as_u64().unwrap() as usize, text.len());
                    break;
                }
            }
        }
        assert!(pages >= 3, "a >192 KiB diff spans several pages: {pages}");
        assert!(text.contains("+changed line 011999 with padding"));
        assert!(text.contains("+edited"));
        let stat = tool
            .call(GitArgs {
                operation: GitOperation::Diff,
                format: Some(super::DiffFormat::Stat),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(stat["text"].as_str().unwrap().contains("big.txt"), "{stat}");
        assert!(
            stat["text"].as_str().unwrap().contains("2 files changed"),
            "{stat}"
        );
        let names = tool
            .call(GitArgs {
                operation: GitOperation::Diff,
                format: Some(super::DiffFormat::NameStatus),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(names["text"], "M\tbig.txt\nM\tsmall.txt\n");
        let beyond = tool
            .call(GitArgs {
                operation: GitOperation::Diff,
                offset: Some(u64::MAX),
                ..Default::default()
            })
            .await;
        assert!(beyond.is_err());
        let misplaced = tool
            .call(GitArgs {
                operation: GitOperation::Log,
                offset: Some(1),
                ..Default::default()
            })
            .await;
        assert!(misplaced.is_err());
    }

    /// The new mutations under the real Seatbelt `.git` protection model:
    /// refs, HEAD, objects and the index stay writable while config, hooks
    /// and info remain protected.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn new_git_mutations_work_inside_the_contained_git_service() {
        let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
        if !crate::sandbox::seatbelt_exists() {
            crate::sandbox::skip_unusable_real_sandbox_test("Seatbelt is unavailable");
            return;
        }
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        let mut tool = repo.tool();
        tool.sandbox = crate::sandbox::Sandbox::new(true, "seatbelt")
            .with_workspace_binding(tool.workspace.clone());
        tool.test_uncontained = false;
        for call in [
            named(GitOperation::BranchCreate, "contained"),
            named(GitOperation::Switch, "contained"),
            named(GitOperation::Tag, "contained-tag"),
        ] {
            let operation = call.operation;
            let result = tool.call(call).await.unwrap();
            assert_eq!(result["status"], "success", "{operation:?}: {result}");
        }
        assert_eq!(repo.git(["branch", "--show-current"]), "contained\n");
        repo.write("a.txt", "dirty\n");
        let pushed = tool
            .call(GitArgs {
                operation: GitOperation::StashPush,
                message: Some("contained stash".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(pushed["status"], "success", "{pushed}");
        let popped = tool
            .call(GitArgs {
                operation: GitOperation::StashPop,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(popped["status"], "success", "{popped}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "dirty\n"
        );
    }

    #[tokio::test]
    async fn new_git_mutations_use_their_own_permission_keys() {
        let repo = TestRepo::new();
        commit_file(&repo, "a.txt", "one\n", "first");
        repo.write("a.txt", "dirty\n");
        let tool = repo.tool_with_permission(PermissionConfig {
            git_branch: Some(ToolPerm::Simple(Action::Deny)),
            git_switch: Some(ToolPerm::Simple(Action::Deny)),
            git_stash: Some(ToolPerm::Simple(Action::Deny)),
            git_tag: Some(ToolPerm::Simple(Action::Deny)),
            git_restore: Some(ToolPerm::Simple(Action::Deny)),
            write: Some(ToolPerm::Simple(Action::Allow)),
            ..PermissionConfig::default()
        });
        for call in [
            named(GitOperation::BranchCreate, "denied"),
            named(GitOperation::Switch, "master"),
            GitArgs {
                operation: GitOperation::StashPush,
                ..Default::default()
            },
            named(GitOperation::Tag, "denied"),
            args(GitOperation::Restore, &["a.txt"], None),
        ] {
            let operation = call.operation;
            assert!(
                tool.call(call).await.is_err(),
                "{operation:?} was not denied"
            );
        }
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "dirty\n"
        );
        assert!(repo.git(["tag"]).is_empty());
        assert!(!repo.git(["branch"]).contains("denied"));
    }

    #[tokio::test]
    async fn contained_commit_environment_carries_host_identity_without_home() {
        use crate::git::runner::{GitRunner, contained_commit_environment};

        let repo = TestRepo::new();
        let runner = GitRunner::discover().expect("discover Git");
        let identity = runner
            .resolve_commit_identity_with(repo.path(), |_| None, &isolated_host_config())
            .await
            .expect("repository-local identity resolves on the host");
        assert_eq!(identity.author_name, "Mini Agent Test");
        assert_eq!(identity.author_email, "mini-agent@example.invalid");
        assert_eq!(identity.committer_name, "Mini Agent Test");
        assert_eq!(identity.committer_email, "mini-agent@example.invalid");

        let env = contained_commit_environment(&identity);
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(
            lookup("GIT_AUTHOR_NAME").as_deref(),
            Some("Mini Agent Test")
        );
        assert_eq!(
            lookup("GIT_AUTHOR_EMAIL").as_deref(),
            Some("mini-agent@example.invalid")
        );
        assert_eq!(
            lookup("GIT_COMMITTER_NAME").as_deref(),
            Some("Mini Agent Test")
        );
        assert_eq!(
            lookup("GIT_COMMITTER_EMAIL").as_deref(),
            Some("mini-agent@example.invalid")
        );
        assert_eq!(lookup("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        for forbidden in [
            "HOME",
            "XDG_CONFIG_HOME",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG",
            "GIT_ASKPASS",
            "SSH_AUTH_SOCK",
        ] {
            assert!(
                lookup(forbidden).is_none(),
                "contained commit environment must not carry {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn commit_identity_honours_git_author_and_committer_overrides() {
        use crate::git::runner::GitRunner;

        let repo = TestRepo::new();
        let runner = GitRunner::discover().expect("discover Git");
        let identity = runner
            .resolve_commit_identity_with(
                repo.path(),
                |name| match name {
                    "GIT_AUTHOR_NAME" => Some("Override Author".into()),
                    "GIT_COMMITTER_EMAIL" => Some("committer@example.invalid".into()),
                    _ => None,
                },
                &isolated_host_config(),
            )
            .await
            .expect("overrides combine with repository-local identity");
        assert_eq!(identity.author_name, "Override Author");
        assert_eq!(identity.author_email, "mini-agent@example.invalid");
        assert_eq!(identity.committer_name, "Mini Agent Test");
        assert_eq!(identity.committer_email, "committer@example.invalid");
    }

    #[test]
    fn a_large_diff_page_is_delivered_whole_with_a_working_next_offset() {
        let stdout: Vec<u8> = (0..8_000)
            .flat_map(|line| format!("+changed line {line:06} padding\n").into_bytes())
            .collect();
        let output = || crate::sandbox::CommandOutput {
            exit_status: None,
            stdout: stdout.clone(),
            stderr: Vec::new(),
            status: crate::sandbox::CommandStatus::Completed,
            descendants_escaped: false,
        };
        let page =
            super::render_paged_text_result_with("diff", None, output(), None, 150 * 1024).unwrap();
        let text = page["text"].as_str().unwrap();
        assert!(text.len() > 140 * 1024 && text.len() <= 150 * 1024);
        let next = page["next_offset"].as_u64().unwrap();
        assert_eq!(next as usize, text.len());
        let rest =
            super::render_paged_text_result_with("diff", None, output(), Some(next), 150 * 1024)
                .unwrap();
        assert!(rest["next_offset"].is_null());
        assert_eq!(
            text.len() + rest["text"].as_str().unwrap().len(),
            stdout.len()
        );
    }

    #[test]
    fn page_size_stays_below_the_default_result_backstop() {
        let bytes = super::text_page_bytes();
        assert!(bytes <= crate::agent::tools::limits::DEFAULT_MAX_RESULT_CHARS * 3 / 4);
        assert!(crate::session::tool_result_is_self_bounded("git"));
    }

    #[tokio::test]
    async fn commit_without_resolvable_identity_fails_explicitly() {
        use crate::git::runner::GitRunner;

        let repo = TestRepo::new();
        repo.git(["config", "--unset", "user.name"]);
        repo.git(["config", "--unset", "user.email"]);
        let runner = GitRunner::discover().expect("discover Git");

        let error = runner
            .resolve_commit_identity_with(repo.path(), |_| None, &isolated_host_config())
            .await
            .expect_err("missing identity must be rejected");
        assert!(
            error.contains("author identity"),
            "error must name the missing identity: {error}"
        );
        assert!(
            error.contains("user.name") && error.contains("GIT_AUTHOR_NAME"),
            "error must explain how to fix it: {error}"
        );

        // Partial identities (name without email) are rejected too.
        repo.git(["config", "user.name", "Only Name"]);
        let error = runner
            .resolve_commit_identity_with(repo.path(), |_| None, &isolated_host_config())
            .await
            .expect_err("name without email must be rejected");
        assert!(error.contains("author identity"), "{error}");
    }
}
