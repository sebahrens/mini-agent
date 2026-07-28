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

        let requested_path = args.path.as_deref().unwrap_or(".");
        if requested_path.is_empty() {
            return Err(ToolError::Msg("Search path cannot be empty".to_string()));
        }
        let search_path = crate::fs::expand_tilde(requested_path);
        let traversal_root = tokio::fs::canonicalize(&search_path).await?;
        let authorized_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        let permission_path = traversal_root.to_string_lossy();
        let _ = check_perm_path(
            &self.permission,
            &self.ask_tx,
            "find_files",
            &permission_path,
        )
        .await?;
        let traversal_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        crate::fs::ensure_same_file(&traversal_root, &authorized_metadata, &traversal_metadata)?;

        let walker = WalkBuilder::new(&traversal_root)
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
        let mut results: Vec<String> = Vec::with_capacity(max_results.saturating_add(1).min(64));
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
        let current_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        crate::fs::ensure_same_file(&traversal_root, &authorized_metadata, &current_metadata)?;

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
            Self::new_in(&std::env::temp_dir(), tag)
        }

        fn new_in(parent: &Path, tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = parent.join(format!(
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

    fn standard_permission(working_dir: &Path) -> PermCheck {
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(working_dir.to_path_buf()),
            None,
        )))
    }

    async fn call_answering_path_permission(
        permission: PermCheck,
        args: FindFilesArgs,
        expected_path: &Path,
        decision: UserDecision,
    ) -> Result<String, ToolError> {
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(permission), Some(ask_tx), 10);
        let call = tool.call(args);
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("find_files did not request path permission")
                .expect("find_files permission channel closed");
            assert_eq!(request.tool.as_str(), "find_files");
            assert_eq!(
                PathBuf::from(request.input.as_str()),
                expected_path.to_path_buf()
            );
            request
                .reply
                .send(decision)
                .expect("find_files dropped the permission reply");
        };

        let (result, ()) = tokio::join!(call, respond);
        result
    }

    #[tokio::test]
    async fn prompts_before_searching_external_path() {
        let external = TempDir::new("restrictive_external");
        let canonical_external = std::fs::canonicalize(external.path()).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(
            Some(restrictive_permission_allowing_pattern()),
            Some(ask_tx),
            10,
        );

        let call = tool.call(FindFilesArgs {
            pattern: "needle".to_string(),
            path: Some(external.path().to_string_lossy().into_owned()),
        });
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("find_files did not request path permission")
                .expect("find_files permission channel closed");
            assert_eq!(request.tool.as_str(), "find_files");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_external);
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
    async fn find_files_external_path_permission_keeps_local_relative_searches() {
        let cwd = std::env::current_dir().unwrap();
        let dir = TempDir::new_in(&cwd, "local_relative");
        let marker = "find_files_local_relative_marker.txt";
        std::fs::write(dir.path().join(marker), "").unwrap();
        let relative_root = dir.path().strip_prefix(&cwd).unwrap();

        let output = FindFilesTool::new(Some(standard_permission(&cwd)), None, 10)
            .call(FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: Some(relative_root.to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(output.contains(marker));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_uses_canonical_absolute_root() {
        let container = TempDir::new("absolute_external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "absolute_external_marker.txt";
        std::fs::write(external.join(marker), "").unwrap();
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: Some(external.to_string_lossy().into_owned()),
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission denied by user"
        ));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_resolves_traversal_before_asking() {
        let container = TempDir::new("traversal_external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let requested = workspace.join("..").join("external");
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some(requested.to_string_lossy().into_owned()),
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn find_files_external_path_permission_expands_tilde_before_asking() {
        let home = PathBuf::from(crate::fs::expand_tilde("~"));
        assert_ne!(home, PathBuf::from("~"), "test requires a home directory");
        let workspace = TempDir::new("tilde_workspace");
        let canonical_home = std::fs::canonicalize(&home).unwrap();

        let result = call_answering_path_permission(
            standard_permission(workspace.path()),
            FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some("~".to_string()),
            },
            &canonical_home,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_files_external_path_permission_resolves_symlink_escape_before_asking() {
        let container = TempDir::new("symlink_external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        let link = workspace.join("escaped");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::os::unix::fs::symlink(&external, &link).unwrap();
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some(link.to_string_lossy().into_owned()),
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_files_external_path_permission_binds_walker_to_authorized_symlink_target() {
        let container = TempDir::new("symlink_binding");
        let workspace = container.path().join("workspace");
        let authorized = container.path().join("authorized");
        let swapped = container.path().join("swapped");
        let link = workspace.join("root");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&swapped).unwrap();
        std::fs::write(authorized.join("authorized_marker.txt"), "").unwrap();
        std::fs::write(swapped.join("swapped_marker.txt"), "").unwrap();
        std::os::unix::fs::symlink(&authorized, &link).unwrap();
        let canonical_authorized = std::fs::canonicalize(&authorized).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let call = tool.call(FindFilesArgs {
            pattern: "marker".to_string(),
            path: Some(link.to_string_lossy().into_owned()),
        });
        let swap = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_authorized);
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&swapped, &link).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, swap);
        let output = result.unwrap();
        assert!(output.contains("authorized_marker.txt"));
        assert!(!output.contains("swapped_marker.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_files_external_path_permission_rejects_authorized_root_replacement() {
        let container = TempDir::new("root_replacement");
        let workspace = container.path().join("workspace");
        let authorized = container.path().join("authorized");
        let moved = container.path().join("moved");
        let swapped = container.path().join("swapped");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&swapped).unwrap();
        std::fs::write(swapped.join("must_not_be_returned.txt"), "").unwrap();
        let canonical_authorized = std::fs::canonicalize(&authorized).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let call = tool.call(FindFilesArgs {
            pattern: "must_not_be_returned".to_string(),
            path: Some(authorized.to_string_lossy().into_owned()),
        });
        let replace = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_authorized);
            std::fs::rename(&authorized, &moved).unwrap();
            std::os::unix::fs::symlink(&swapped, &authorized).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, replace);
        let error = result.expect_err("find_files must reject a replaced traversal root");
        assert!(error.to_string().contains("Path changed"));
        assert!(!error.to_string().contains("must_not_be_returned.txt"));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_pattern_cannot_widen_root() {
        let container = TempDir::new("pattern_root");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "pattern_must_not_escape_marker.txt";
        std::fs::write(external.join(marker), "").unwrap();

        let output = FindFilesTool::new(Some(standard_permission(&workspace)), None, 10)
            .call(FindFilesArgs {
                pattern: format!(".*{marker}$"),
                path: Some(workspace.to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert_eq!(output, "No files found matching the pattern.");
    }

    #[tokio::test]
    async fn find_files_external_path_permission_omitted_root_searches_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let dir = TempDir::new_in(&cwd, "omitted_root");
        let marker = "find_files_omitted_root_marker.txt";
        std::fs::write(dir.path().join(marker), "").unwrap();

        let output = FindFilesTool::new(Some(standard_permission(&cwd)), None, 10)
            .call(FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: None,
            })
            .await
            .unwrap();

        assert!(output.contains(marker));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_rejects_empty_root_before_asking() {
        let cwd = std::env::current_dir().unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(standard_permission(&cwd)), Some(ask_tx), 10);

        let result = tool
            .call(FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some(String::new()),
            })
            .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Search path cannot be empty"
        ));
        assert!(ask_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn find_files_external_path_permission_fails_closed_on_permission_channel_failure() {
        let container = TempDir::new("closed_permission_channel");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "closed_permission_channel_marker.txt";
        std::fs::write(external.join(marker), "").unwrap();
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
        drop(ask_rx);
        let tool = FindFilesTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let result = tool
            .call(FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: Some(external.to_string_lossy().into_owned()),
            })
            .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission system unavailable"
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
