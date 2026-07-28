use ignore::WalkBuilder;
use regex::Regex;
use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, FindFilesArgs, PermCheck, ToolError, check_perm, check_perm_path, is_skip_dir,
};

pub struct FindFilesTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_results: u64,
}

impl FindFilesTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, max_results: u64) -> Self {
        FindFilesTool {
            permission,
            ask_tx,
            max_results,
        }
    }
}

impl Tool for FindFilesTool {
    const NAME: &'static str = "find_files";

    type Error = ToolError;
    type Args = FindFilesArgs;
    type Output = String;

    fn description(&self) -> String {
        "Recursively find files matching a regex pattern in their filename. Respects .gitignore. Skips node_modules and target.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to match file names against"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (defaults to current working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: FindFilesArgs) -> Result<String, ToolError> {
        tracing::debug!(
            "tool find_files start: pattern={}, path={}",
            args.pattern,
            args.path.as_deref().unwrap_or("."),
        );
        let coaching =
            check_perm(&self.permission, &self.ask_tx, "find_files", &args.pattern).await?;

        let re = Regex::new(&args.pattern)
            .map_err(|e| ToolError::Msg(format!("Invalid regex: {}", e)))?;

        let search_path = args.path.as_deref().unwrap_or(".");
        let _ = check_perm_path(&self.permission, &self.ask_tx, "find_files", search_path).await?;

        let walker = WalkBuilder::new(search_path)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .hidden(false)
            .filter_entry(|entry| {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    !is_skip_dir(entry.file_name().to_str().unwrap_or(""))
                } else {
                    true
                }
            })
            .build();

        let max_results = self.max_results as usize;
        let mut results: Vec<String> =
            Vec::with_capacity(max_results.saturating_add(1).min(64));
        let mut limit_hit = false;

        for entry in walker
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            let fname = entry.file_name().to_string_lossy();
            if re.is_match(&fname) {
                results.push(entry.path().to_string_lossy().to_string());
                if results.len() > max_results {
                    limit_hit = true;
                    break;
                }
            }
        }

        if results.is_empty() {
            let msg = "No files found matching the pattern.".to_string();
            return Ok(match coaching {
                Some(c) => format!("{}\n\n{}", c, msg),
                None => msg,
            });
        }

        if limit_hit {
            results.truncate(max_results);
        }
        results.sort();

        let total = results.len();
        let result = if limit_hit {
            format!(
                "{} files found (showing first {}):\n{}\n\n[truncated after {} entries — unknown number of additional entries; narrow the pattern or path]",
                total,
                max_results,
                results[..max_results].join("\n"),
                max_results
            )
        } else {
            format!("{} files found:\n{}", total, results.join("\n"))
        };

        tracing::debug!(
            "tool find_files done: results={}, truncated={}",
            total,
            limit_hit,
        );
        Ok(match coaching {
            Some(c) => format!("{}\n\n{}", c, result),
            None => result,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::permission::ask::UserDecision;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "zerostack_find_files_test_{}_{}_{}",
                tag,
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
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

    fn restrictive_permission_allowing_pattern() -> PermCheck {
        let config = PermissionConfig {
            find_files: Some(ToolPerm::Granular(
                [("needle".to_string(), Action::Allow)].into(),
            )),
            ..PermissionConfig::default()
        };
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Restrictive,
            Some(std::path::PathBuf::from("/workspace")),
            Some(vec!["restrictive".to_string()]),
        )))
    }

    #[tokio::test]
    async fn prompts_before_searching_external_path() {
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(
            Some(restrictive_permission_allowing_pattern()),
            Some(ask_tx),
            10,
        );

        let call = tool.call(FindFilesArgs {
            pattern: "needle".to_string(),
            path: Some("/etc".to_string()),
        });
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("find_files did not request path permission")
                .expect("find_files permission channel closed");
            assert_eq!(request.tool.as_str(), "find_files");
            assert_eq!(request.input, "/etc");
            request
                .reply
                .send(UserDecision::Deny)
                .expect("find_files dropped the permission reply");
        };

        let (result, ()) = tokio::join!(call, respond);
        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission denied by user"
        ));
    }

    #[tokio::test]
    async fn reports_unknown_remaining_count_when_result_limit_is_hit() {
        let dir = TempDir::new("truncation");
        for index in 0..101 {
            std::fs::write(dir.path().join(format!("match_{index:03}.txt")), "").unwrap();
        }

        let output = FindFilesTool::new(None, None, 100)
            .call(FindFilesArgs {
                pattern: r"^match_\d+\.txt$".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(output.contains("truncated after 100 entries"));
        assert!(output.contains("unknown number of additional entries"));
        assert!(!output.contains("0 more"));
    }

    #[tokio::test]
    async fn does_not_report_truncation_when_walker_is_exhausted_at_result_limit() {
        let dir = TempDir::new("exact_limit");
        for index in 0..100 {
            std::fs::write(dir.path().join(format!("match_{index:03}.txt")), "").unwrap();
        }

        let output = FindFilesTool::new(None, None, 100)
            .call(FindFilesArgs {
                pattern: r"^match_\d+\.txt$".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(output.starts_with("100 files found:\n"));
        assert!(!output.contains("[truncated"));
    }
}
