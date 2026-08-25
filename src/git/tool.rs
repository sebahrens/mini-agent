use std::path::{Component, Path};
use std::sync::Arc;

use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{ToolError, check_perm, check_perm_bound_path};
use crate::git::runner::{
    GitRunner, LOCAL_MUTATION_LIMITS, QUERY_LIMITS, acquire_process_git_mutation,
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

    async fn status_snapshot(&self) -> Result<serde_json::Value, ToolError> {
        let output = self
            .run(
                "status",
                vec![
                    "status".into(),
                    "--porcelain=v2".into(),
                    "-z".into(),
                    "--branch".into(),
                    "--untracked-files=all".into(),
                    "--ignore-submodules=all".into(),
                ],
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

    async fn stage(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        require_paths(&args.paths, "stage")?;
        let paths = self.validate_paths(&args.paths, Some("git/stage")).await?;
        let _mutation = acquire_process_git_mutation().await;
        let before = self.status_snapshot().await?;
        self.ensure_no_external_filters(&paths).await?;
        let mut command = vec!["add".into(), "--".into()];
        command.extend(paths);
        let output = self.run_mutation("stage", command).await?;
        let after = self.status_snapshot().await?;
        Ok(render_mutation_result("stage", None, before, after, output))
    }

    async fn unstage(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        reject_irrelevant_fields(&args, false, false, false)?;
        require_paths(&args.paths, "unstage")?;
        let paths = self
            .validate_paths(&args.paths, Some("git/unstage"))
            .await?;
        let _mutation = acquire_process_git_mutation().await;
        let before = self.status_snapshot().await?;
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
        let output = self.run_mutation("unstage", command).await?;
        let after = self.status_snapshot().await?;
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
        let _mutation = acquire_process_git_mutation().await;
        let before = self.status_snapshot().await?;
        let output = self
            .run_with_input(
                "commit",
                vec![
                    "commit".into(),
                    "--file=-".into(),
                    "--cleanup=verbatim".into(),
                ],
                message.as_bytes().to_vec(),
                LOCAL_MUTATION_LIMITS,
            )
            .await?;
        let after = self.status_snapshot().await?;
        Ok(render_mutation_result(
            "commit", coaching, before, after, output,
        ))
    }

    async fn read_operation(&self, args: GitArgs) -> Result<serde_json::Value, ToolError> {
        match args.operation {
            GitOperation::Status => {
                let coaching = self.permission("git/status", "workspace").await?;
                let mut value = self.status_snapshot().await?;
                value["operation"] = serde_json::json!("status");
                value["coaching"] = serde_json::json!(coaching);
                Ok(value)
            }
            GitOperation::Diff => {
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
                    "--binary".into(),
                ];
                if let Some(revision) = args.revision.as_deref() {
                    self.validate_revision(revision).await?;
                    command.push(revision.to_string());
                }
                command.push("--".into());
                command.extend(paths);
                render_text_result(
                    "diff",
                    coaching,
                    self.run("diff", command, TEXT_LIMITS, true).await?,
                )
            }
            GitOperation::Log => {
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
                    "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x00".into(),
                ];
                if let Some(revision) = args.revision {
                    command.push(revision);
                }
                command.push("--".into());
                command.extend(paths);
                let output = self.run("log", command, TEXT_LIMITS, true).await?;
                let fields = output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|field| !field.is_empty())
                    .map(|field| String::from_utf8_lossy(field).trim_end().to_string())
                    .collect::<Vec<_>>();
                if fields.len() % 6 != 0 {
                    return Err(ToolError::Msg(
                        "git log output had unexpected field count".to_string(),
                    ));
                }
                let commits = fields
                    .chunks_exact(6)
                    .map(|field| {
                        serde_json::json!({
                            "id": field[0], "parents": field[1], "author": field[2],
                            "email": field[3], "authored_at": field[4], "subject": field[5],
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({
                    "operation": "log",
                    "commits": commits,
                    "truncated": matches!(output.status, CommandStatus::OutputLimitExceeded(_)),
                    "coaching": coaching,
                }))
            }
            GitOperation::Show => {
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
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.untrackedCache=false".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
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
            },
        );

        assert_eq!(rendered["status"], "timed_out");
        assert_eq!(rendered["after"], after);
        assert_eq!(
            mutation_status(crate::sandbox::CommandStatus::Cancelled, None),
            "cancelled"
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
    fn hardened_args_starts_with_no_optional_locks() {
        let result = hardened_args(vec!["log".into()]);
        assert_eq!(result[0], "--no-optional-locks");
    }

    #[test]
    fn hardened_args_appends_caller_args_at_end() {
        let result = hardened_args(vec!["log".into(), "--oneline".into()]);
        // Caller args must appear and be in order relative to each other
        let log_pos = result.iter().position(|s| s == "log").expect("log missing");
        let oneline_pos = result
            .iter()
            .position(|s| s == "--oneline")
            .expect("--oneline missing");
        assert!(log_pos < oneline_pos, "caller arg order must be preserved");
        // Safety flags precede caller args
        let safety_end = result
            .iter()
            .position(|s| s == "--no-optional-locks")
            .unwrap();
        assert!(
            safety_end < log_pos,
            "safety flags must precede caller args"
        );
    }

    #[test]
    fn hardened_args_disables_hooks() {
        let result = hardened_args(vec!["commit".into()]);
        assert!(
            result
                .windows(2)
                .any(|w| w[0] == "-c" && w[1] == "core.hooksPath=/dev/null"),
            "expected core.hooksPath=/dev/null in {result:?}"
        );
    }

    #[test]
    fn hardened_args_clears_credential_helper() {
        let result = hardened_args(vec!["fetch".into()]);
        assert!(
            result
                .windows(2)
                .any(|w| w[0] == "-c" && w[1] == "credential.helper="),
            "expected credential.helper= in {result:?}"
        );
    }

    #[test]
    fn hardened_args_blocks_file_protocol() {
        let result = hardened_args(vec!["fetch".into()]);
        assert!(
            result
                .windows(2)
                .any(|w| w[0] == "-c" && w[1] == "protocol.file.allow=never"),
            "expected protocol.file.allow=never in {result:?}"
        );
    }

    #[test]
    fn hardened_args_blocks_ext_protocol() {
        let result = hardened_args(vec!["fetch".into()]);
        assert!(
            result
                .windows(2)
                .any(|w| w[0] == "-c" && w[1] == "protocol.ext.allow=never"),
            "expected protocol.ext.allow=never in {result:?}"
        );
    }

    #[test]
    fn hardened_args_disables_fsmonitor() {
        let result = hardened_args(vec![]);
        assert!(
            result
                .windows(2)
                .any(|w| w[0] == "-c" && w[1] == "core.fsmonitor=false"),
            "expected core.fsmonitor=false in {result:?}"
        );
    }

    #[test]
    fn hardened_args_empty_caller_args_still_has_safety_flags() {
        let result = hardened_args(vec![]);
        assert!(!result.is_empty());
        assert_eq!(result[0], "--no-optional-locks");
    }
}
