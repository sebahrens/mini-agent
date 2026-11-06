use std::path::{Component, Path};
use std::sync::Arc;

use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{ToolError, check_perm, check_perm_bound_path};
use crate::git::runner::{GitRunner, LOCAL_MUTATION_LIMITS, QUERY_LIMITS, command_result};
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;
use crate::sandbox::{CommandOutput, CommandStatus, Sandbox};

const TEXT_LIMITS: crate::sandbox::CommandLimits = crate::sandbox::CommandLimits {
    timeout: std::time::Duration::from_secs(10),
    stdout_bytes: 192 * 1024,
    stderr_bytes: 64 * 1024,
    combined_bytes: 224 * 1024,
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
}

impl GitOperation {
    // The native /diff and /commit actors (git-worktree) report the name.
    #[cfg_attr(not(feature = "git-worktree"), allow(dead_code))]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Diff => "diff",
            Self::Log => "log",
            Self::Show => "show",
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
}

pub(crate) struct GitTool {
    runner: GitRunner,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    exact_files: bool,
    filter_drivers: Arc<FilterDriverCache>,
    #[cfg(test)]
    test_uncontained: bool,
}

/// Memoized result of the filter-driver config probe. The key is the full
/// content of the repository-controlled config files, so any edit (including
/// an in-place same-size rewrite) misses the cache.
#[derive(Default)]
struct FilterDriverCache {
    entry: std::sync::Mutex<Option<(Vec<u8>, Vec<String>)>>,
    #[cfg(test)]
    probes: std::sync::atomic::AtomicUsize,
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
            filter_drivers: Arc::default(),
            #[cfg(test)]
            test_uncontained: false,
        })
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
    ) -> Result<CommandOutput, ToolError> {
        #[cfg(test)]
        if self.test_uncontained {
            let result = if allow_nonzero_or_truncated {
                self.runner
                    .run_allow_exit(
                        self.workspace.root(),
                        operation,
                        hardened_args(args),
                        limits,
                    )
                    .await
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
            return result.map_err(ToolError::Msg);
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
            .map_err(ToolError::Msg)
    }

    async fn permission(
        &self,
        verb: &'static str,
        identity: &str,
    ) -> Result<Option<String>, ToolError> {
        check_perm(&self.permission, &self.ask_tx, verb, identity).await
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
                .map_err(ToolError::Msg);
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
            .map_err(ToolError::Msg)
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
                .map_err(ToolError::Msg);
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
            .map_err(ToolError::Msg)
    }

    async fn validate_revision(&self, revision: &str) -> Result<(), ToolError> {
        if revision.is_empty()
            || revision.starts_with('-')
            || revision.contains('\0')
            || revision.len() > 1024
        {
            return Err(ToolError::Msg("invalid Git revision".to_string()));
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
        .map_err(|_| ToolError::Msg("invalid Git revision".to_string()))
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
    async fn status_snapshot(&self, guard: &[String]) -> Result<serde_json::Value, ToolError> {
        let output = self
            .run(
                "status",
                guarded(
                    guard,
                    vec![
                        "status".into(),
                        "--porcelain=v2".into(),
                        "-z".into(),
                        "--branch".into(),
                        "--untracked-files=all".into(),
                        "--ignore-submodules=all".into(),
                    ],
                ),
                QUERY_LIMITS,
                false,
            )
            .await?;
        let records = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .map(|record| String::from_utf8_lossy(record).into_owned())
            .collect::<Vec<_>>();
        Ok(serde_json::json!({ "records": records }))
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
    async fn cached_filter_drivers(&self) -> Result<Vec<String>, ToolError> {
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
    async fn configured_filter_drivers(&self) -> Result<Vec<String>, ToolError> {
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
            Some(0) => parse_filter_driver_names(&output.stdout),
            // `git config --get-regexp` exits 1 when no key matches.
            Some(1) => Ok(Vec::new()),
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
        let drivers = self.cached_filter_drivers().await?;
        if drivers.is_empty() {
            return Ok(Vec::new());
        }
        let guard = neutralizing_args(&drivers);
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
            return Ok(guard);
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
            .map_err(ToolError::Msg)?;
        let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
        // Every record is `path NUL attribute NUL value NUL`, so the split
        // leaves one empty trailing field.
        if fields.len() % 3 != 1 || fields.last().is_some_and(|field| !field.is_empty()) {
            return Err(ToolError::Msg(
                "git check-attr output had unexpected field count".to_string(),
            ));
        }
        for record in fields.chunks_exact(3) {
            let value = String::from_utf8_lossy(record[2]);
            if let Some(driver) = drivers.iter().find(|driver| driver.as_str() == value) {
                return Err(ToolError::Msg(format!(
                    "Git {operation} refused: a tracked path has a `filter` attribute bound to \
                     the repository-configured filter driver `{driver}`, and refreshing the index \
                     would execute that driver's command. Remove the `filter.{driver}` \
                     configuration or the attribute to use this operation."
                )));
            }
        }
        Ok(guard)
    }

    async fn ensure_no_external_filters(&self, paths: &[String]) -> Result<(), ToolError> {
        let mut args = vec![
            "check-attr".into(),
            "-z".into(),
            "--all".into(),
            "--".into(),
        ];
        args.extend(paths.iter().cloned());
        let output = self
            .run("check-attributes", args, QUERY_LIMITS, false)
            .await?;
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
            .run("expand-stage-paths", args, QUERY_LIMITS, false)
            .await?;
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
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
        let guard = self.filter_guard("stage").await?;
        let before = self.status_snapshot(&guard).await?;
        self.reject_recursive_paths(&paths)?;
        let expanded_paths = self.expand_stage_paths(&paths).await?;
        if self.exact_files && expanded_paths.iter().any(|path| !paths.contains(path)) {
            return Err(ToolError::Msg(
                "Git selection expanded beyond frozen file operands".into(),
            ));
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
        Ok(render_mutation_result("stage", None, before, after, output))
    }

    async fn unstage(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        require_paths(&args.paths, "unstage")?;
        let paths = self
            .validate_paths(&args.paths, Some("git/unstage"))
            .await?;
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
        let guard = self.filter_guard("unstage").await?;
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
        Ok(render_mutation_result(
            "unstage", None, before, after, output,
        ))
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
        let coaching = self.permission("git/commit", message).await?;
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
        // `git commit` itself refreshes the index before writing the tree.
        let guard = self.filter_guard("commit").await?;
        let before = self.status_snapshot(&guard).await?;
        self.reject_recursive_paths(&paths)?;
        let mut command = vec![
            "commit".into(),
            "--file=-".into(),
            "--cleanup=verbatim".into(),
        ];
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
        let commit_id = if output.status == CommandStatus::Completed
            && output.exit_status.is_some_and(|status| status.success())
        {
            self.head_id().await
        } else {
            None
        };
        let after = self.status_snapshot(&guard).await?;
        let mut result = render_mutation_result("commit", coaching, before, after, output);
        result["commit_id"] = serde_json::json!(commit_id);
        Ok(result)
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
        reject_irrelevant_fields(&args, false, false, false)?;
        if !args.paths.is_empty() {
            return Err(ToolError::Msg("branches takes no paths".to_string()));
        }
        let coaching = self.permission("git/status", "branches").await?;
        let output = self
            .run(
                "branches",
                vec![
                    "for-each-ref".into(),
                    "--format=%(HEAD)%00%(refname:short)%00%(objectname)%00%(upstream:short)%00%(contents:subject)".into(),
                    "refs/heads".into(),
                ],
                QUERY_LIMITS,
                false,
            )
            .await?;
        let branches = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split('\0').collect();
                (fields.len() == 5).then(|| {
                    serde_json::json!({
                        "current": fields[0] == "*",
                        "name": fields[1],
                        "id": fields[2],
                        "upstream": fields[3],
                        "subject": fields[4],
                    })
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "operation": "branches",
            "branches": branches,
            "coaching": coaching,
        }))
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
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
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
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
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
        Ok(render_mutation_result(
            "switch", coaching, before, after, output,
        ))
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
                    QUERY_LIMITS,
                    false,
                )
                .await?;
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
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
        let guard = self.filter_guard(label).await?;
        let before = self.status_snapshot(&guard).await?;
        let mut command = vec!["stash".into()];
        if push {
            command.push("push".into());
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
        Ok(render_mutation_result(
            label, coaching, before, after, output,
        ))
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
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
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
        Ok(render_ref_result("tag", coaching, output))
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
        let _mutation = self
            .runner
            .acquire_mutation(self.workspace.root())
            .await
            .map_err(ToolError::Msg)?;
        let guard = self.filter_guard("restore").await?;
        let before = self.status_snapshot(&guard).await?;
        self.reject_recursive_paths(&paths)?;
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
        Ok(render_mutation_result(
            "restore", coaching, before, after, output,
        ))
    }

    async fn read_operation(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        let pages = matches!(args.operation, GitOperation::Diff | GitOperation::Show);
        if !pages && (args.format.is_some() || args.offset.is_some()) {
            return Err(ToolError::Msg(
                "format and offset are supported only by diff and show".to_string(),
            ));
        }
        let named = matches!(
            args.operation,
            GitOperation::BranchCreate | GitOperation::Switch | GitOperation::Tag
        );
        if !named && args.name.is_some() {
            return Err(ToolError::Msg(
                "name is supported only by branch_create, switch and tag".to_string(),
            ));
        }
        match args.operation {
            GitOperation::Status => {
                reject_irrelevant_fields(&args, false, false, false)?;
                if !args.paths.is_empty() {
                    return Err(ToolError::Msg(
                        "paths are not supported by Git status".to_string(),
                    ));
                }
                let coaching = self.permission("git/status", "workspace").await?;
                let guard = self.filter_guard("status").await?;
                let mut value = self.status_snapshot(&guard).await?;
                value["operation"] = serde_json::json!("status");
                value["coaching"] = serde_json::json!(coaching);
                Ok(value)
            }
            GitOperation::Diff => {
                reject_irrelevant_fields(&args, false, true, false)?;
                let paths = self.validate_paths(&args.paths, None).await?;
                let identity = serde_json::to_string(&serde_json::json!({
                    "revision": args.revision,
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
                if let Some(revision) = args.revision.as_deref() {
                    self.validate_revision(revision).await?;
                    command.push(revision.to_string());
                }
                command.push("--".into());
                command.extend(paths);
                // Without `--cached` every diff reads the working tree.
                let guard = self.filter_guard("diff").await?;
                render_paged_text_result(
                    "diff",
                    coaching,
                    self.run("diff", guarded(&guard, command), PAGED_TEXT_LIMITS, true)
                        .await?,
                    args.offset,
                )
            }
            GitOperation::Log => {
                reject_irrelevant_fields(&args, false, true, true)?;
                let paths = self.validate_paths(&args.paths, None).await?;
                let count = args.max_count.unwrap_or(20).clamp(1, 100);
                if let Some(revision) = args.revision.as_deref() {
                    self.validate_revision(revision).await?;
                }
                let identity = serde_json::to_string(&serde_json::json!({
                    "revision": args.revision,
                    "paths": paths,
                    "max_count": count,
                }))?;
                let coaching = self.permission("git/log", &identity).await?;
                let mut command = vec![
                    "log".into(),
                    format!("--max-count={count}"),
                    "--date=iso-strict".into(),
                    LOG_FORMAT.into(),
                ];
                if let Some(revision) = args.revision {
                    command.push(revision);
                }
                command.push("--".into());
                command.extend(paths);
                let output = self.run("log", command, TEXT_LIMITS, true).await?;
                let commits = parse_log_records(&output.stdout)?;
                Ok(serde_json::json!({
                    "operation": "log",
                    "commits": commits,
                    "truncated": matches!(output.status, CommandStatus::OutputLimitExceeded(_)),
                    "coaching": coaching,
                }))
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
                render_paged_text_result(
                    "show",
                    coaching,
                    self.run("show", command, PAGED_TEXT_LIMITS, true).await?,
                    args.offset,
                )
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
        }
    }
}

/// `git log` pretty format: NUL-separated fields, each record ended by an ASCII
/// record separator. Git's tformat also appends a newline after every record,
/// so parsing splits records first and then fields, keeping empty fields (a
/// root commit has an empty `%P`).
const LOG_FORMAT: &str = "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x1e";
const LOG_FIELD_COUNT: usize = 6;

fn parse_log_records(stdout: &[u8]) -> Result<Vec<serde_json::Value>, ToolError> {
    let mut records = stdout.split(|byte| *byte == 0x1e).collect::<Vec<_>>();
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
            Ok(serde_json::json!({
                "id": fields[0], "parents": fields[1], "author": fields[2],
                "email": fields[3], "authored_at": fields[4], "subject": fields[5],
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
        "Inspect and update the bound Git repository through fixed structured operations: status, diff/show (format patch|stat|name_status; at most 192 KiB per call, page with offset/next_offset or narrow with paths), log, stage, unstage, commit, branches, branch_create (name, optional start revision), switch (name of an existing branch), stash_push (optional message/paths), stash_pop, stash_list, tag (name, optional revision; message makes it annotated) and restore (discard working-tree changes of paths, from the index or revision). Every mutation has its own git/<operation> permission key. No shell, raw argv, remotes, or network access is available.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["status", "diff", "log", "show", "stage", "unstage", "commit", "branches", "branch_create", "switch", "stash_push", "stash_pop", "stash_list", "tag", "restore"]
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
                "revision": { "type": "string", "description": "Revision for diff, log, show, branch_create (start point), tag (target) or restore (source)" },
                "message": { "type": "string", "maxLength": 16384, "description": "Commit message (required for commit), stash_push message, or annotated tag message" },
                "max_count": { "type": "integer", "minimum": 1, "maximum": 100 }
            },
            "required": ["operation"],
            "additionalProperties": false
        })
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
    let mut names = std::collections::BTreeSet::new();
    for record in stdout.split(|byte| *byte == 0) {
        let key = record.split(|byte| *byte == b'\n').next().unwrap_or(record);
        let Some(rest) = key.strip_prefix(b"filter.") else {
            continue;
        };
        // Section and variable names contain no dots; the subsection may.
        let Some(dot) = rest.iter().rposition(|byte| *byte == b'.') else {
            continue;
        };
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
    Ok(names.into_iter().collect())
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
        return Err(ToolError::Msg(
            "message is supported only by commit, stash_push and tag".to_string(),
        ));
    }
    if !allow_revision && args.revision.is_some() {
        return Err(ToolError::Msg(
            "revision is not supported by this Git operation".to_string(),
        ));
    }
    if !allow_max_count && args.max_count.is_some() {
        return Err(ToolError::Msg(
            "max_count is supported only by log".to_string(),
        ));
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
        "before": before,
        "after": after,
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
        CommandStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
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
                filter_drivers: Arc::default(),
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
        assert!(committed["before"]["records"].is_array());
        assert!(committed["after"]["records"].is_array());
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

    #[test]
    fn log_parser_returns_only_terminated_records() {
        let complete = b"a\0\0n\0e\0t\0s1\x1e\nb\0a\0n\0e\0t\0s2\x1e\n";
        let partial = b"a\0\0n\0e\0t\0s1\x1e\nb\0a\0n";
        let parsed = super::parse_log_records(complete).expect("complete output");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1]["id"], "b");
        assert_eq!(parsed[1]["parents"], "a");
        let parsed = super::parse_log_records(partial).expect("truncated output");
        assert_eq!(parsed.len(), 1);
        assert!(super::parse_log_records(b"a\0b\x1e\n").is_err());
        assert!(
            super::parse_log_records(b"")
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
