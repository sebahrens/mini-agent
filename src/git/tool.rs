use std::path::{Component, Path};
use std::sync::Arc;

use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{ToolError, check_perm, check_perm_bound_path};
use crate::git::runner::{GitRunner, QUERY_LIMITS};
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
        })
    }

    async fn run(
        &self,
        operation: &'static str,
        args: Vec<String>,
        limits: crate::sandbox::CommandLimits,
        allow_nonzero_or_truncated: bool,
    ) -> Result<CommandOutput, ToolError> {
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
                    "enum": ["status", "diff", "log", "show"]
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 128,
                    "description": "Literal repository-relative paths; never options or globs"
                },
                "revision": { "type": "string", "description": "Revision for diff, log, or show" },
                "message": { "type": "string", "description": "Commit message for commit only" },
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

trait Pipe: Sized {
    fn pipe<T>(self, apply: impl FnOnce(Self) -> T) -> T {
        apply(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::hardened_args;

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
