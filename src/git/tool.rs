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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GitOperation {
    Status,
    Diff,
    Log,
    Show,
    Stage,
    Unstage,
    Commit,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GitArgs {
    pub operation: GitOperation,
    #[serde(default)]
    pub paths: Vec<String>,
    pub revision: Option<String>,
    pub message: Option<String>,
    pub max_count: Option<u16>,
}

pub(crate) struct GitTool {
    runner: GitRunner,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    #[cfg(test)]
    test_uncontained: bool,
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
            #[cfg(test)]
            test_uncontained: false,
        })
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
        let drivers = self.configured_filter_drivers().await?;
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
        let expanded_paths = self.expand_stage_paths(&paths).await?;
        self.ensure_no_external_filters(&expanded_paths).await?;
        let mut command = vec!["add".into(), "--".into()];
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
        if !args.paths.is_empty() {
            return Err(ToolError::Msg(
                "commit operates on the existing index and does not accept paths".to_string(),
            ));
        }
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
        let output = self
            .run_with_input(
                "commit",
                guarded(
                    &guard,
                    vec![
                        "commit".into(),
                        "--file=-".into(),
                        "--cleanup=verbatim".into(),
                    ],
                ),
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

    async fn read_operation(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
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
                if let Some(revision) = args.revision.as_deref() {
                    self.validate_revision(revision).await?;
                    command.push(revision.to_string());
                }
                command.push("--".into());
                command.extend(paths);
                // Without `--cached` every diff reads the working tree.
                let guard = self.filter_guard("diff").await?;
                render_text_result(
                    "diff",
                    coaching,
                    self.run("diff", guarded(&guard, command), TEXT_LIMITS, true)
                        .await?,
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
                    revision.to_string(),
                    "--".into(),
                ];
                command.extend(paths);
                render_text_result(
                    "show",
                    coaching,
                    self.run("show", command, TEXT_LIMITS, true).await?,
                )
            }
            GitOperation::Stage => self.stage(args).await,
            GitOperation::Unstage => self.unstage(args).await,
            GitOperation::Commit => self.commit(args).await,
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
        "Inspect and update the bound Git repository through fixed structured operations. No shell, raw argv, remotes, or network access is available.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["status", "diff", "log", "show", "stage", "unstage", "commit"]
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 128,
                    "description": "Literal repository-relative paths; never options or globs"
                },
                "revision": { "type": "string", "description": "Revision for diff, log, or show" },
                "message": { "type": "string", "maxLength": 16384, "description": "Required commit message for commit only" },
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
                format!("filter.{driver}.required=false"),
            ]
        })
        .collect()
}

fn render_text_result(
    operation: &str,
    coaching: Option<String>,
    output: CommandOutput,
) -> Result<serde_json::Value, ToolError> {
    Ok(serde_json::json!({
        "operation": operation,
        "text": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "truncated": matches!(output.status, CommandStatus::OutputLimitExceeded(_)),
        "exit_code": output.exit_status.and_then(|status| status.code()),
        "coaching": coaching,
    }))
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
            "message is supported only by commit".to_string(),
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
