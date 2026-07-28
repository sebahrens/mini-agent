use std::io::Read;

use regex::Regex;
use rig::tool::Tool;

use super::find_files::BoundDirectory;
use crate::agent::tools::{
    AskSender, GrepArgs, PermCheck, ToolError, check_perm, check_perm_path,
};

pub struct GrepTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_results: u64,
}

impl GrepTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, max_results: u64) -> Self {
        GrepTool {
            permission,
            ask_tx,
            max_results,
        }
    }

    pub(crate) fn glob_to_regex(glob: &str) -> String {
        let mut re = String::with_capacity(glob.len() * 2);
        for c in glob.chars() {
            match c {
                '.' => re.push_str("\\."),
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '{' => re.push_str("(?:"),
                '}' => re.push(')'),
                ',' => re.push('|'),
                _ => re.push(c),
            }
        }
        re
    }

    pub(crate) fn is_binary(data: &[u8]) -> bool {
        data.iter().take(8192).any(|&b| b == 0)
    }
}

impl Tool for GrepTool {
    const NAME: &'static str = "grep";

    type Error = ToolError;
    type Args = GrepArgs;
    type Output = String;

    fn description(&self) -> String {
        "Search file contents using a regex pattern (Rust regex syntax). Respects .gitignore. Skips binary files, node_modules, and target.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for (supports Rust regex syntax)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (defaults to current working directory)"
                },
                "include": {
                    "type": "string",
                    "description": "Optional file glob pattern to filter (e.g. '*.rs', '*.{ts,tsx}')"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match (like grep -C)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: GrepArgs) -> Result<String, ToolError> {
        tracing::debug!(
            "tool grep start: pattern={}, path={}, include={:?}",
            args.pattern,
            args.path.as_deref().unwrap_or("."),
            args.include,
        );
        let coaching = check_perm(&self.permission, &self.ask_tx, "grep", &args.pattern).await?;

        let re = Regex::new(&args.pattern)
            .map_err(|e| ToolError::Msg(format!("Invalid regex pattern: {}", e)))?;

        let requested_path = args.path.as_deref().unwrap_or(".");
        if requested_path.is_empty() {
            return Err(ToolError::Msg("Search path cannot be empty".to_string()));
        }
        let search_path = crate::fs::expand_tilde(requested_path);
        let traversal_root = tokio::fs::canonicalize(&search_path).await?;
        let authorized_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        let bound_directory = BoundDirectory::open(&traversal_root, &authorized_metadata)?;
        let permission_path = traversal_root.to_string_lossy();
        let _ = check_perm_path(&self.permission, &self.ask_tx, "grep", &permission_path).await?;
        let traversal_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        crate::fs::ensure_same_file(&traversal_root, &authorized_metadata, &traversal_metadata)?;
        let context = args.context_lines.unwrap_or(0);

        let include_re = args.include.as_ref().map(|g| {
            let pattern = format!("^(?:{})$", Self::glob_to_regex(g));
            Regex::new(&pattern).unwrap_or_else(|_| Regex::new(".*").unwrap())
        });

        let walker = bound_directory.walker()?;

        let max_results = self.max_results as usize;
        let mut file_count = 0;
        let mut files_with_matches: usize = 0;
        let mut all_results: Vec<String> = Vec::with_capacity(max_results.min(64));
        let mut limit_hit = false;

        for entry in walker {
            if all_results.len() >= max_results {
                limit_hit = true;
                break;
            }

            if let Some(ref re_include) = include_re {
                let fname = entry.file_name.to_string_lossy();
                if !re_include.is_match(&fname) {
                    continue;
                }
            }

            if entry.metadata.len() > 10 * 1024 * 1024 {
                continue;
            }

            let path_str = entry.path.to_string_lossy().to_string();
            let capacity = entry.metadata.len() as usize;
            let mut file = entry.file;
            let read_result = tokio::task::spawn_blocking(move || {
                let mut data = Vec::with_capacity(capacity);
                file.read_to_end(&mut data).map(|_| data)
            })
            .await
            .map_err(|error| ToolError::Msg(format!("grep file reader failed: {error}")))?;

            match read_result {
                Ok(data) => {
                    if Self::is_binary(&data) {
                        continue;
                    }
                    file_count += 1;
                    let content = String::from_utf8_lossy(&data);
                    let lines: Vec<&str> = content.lines().collect();
                    let total = lines.len();

                    let match_lines: Vec<usize> = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| re.is_match(l))
                        .map(|(i, _)| i)
                        .collect();

                    if match_lines.is_empty() {
                        continue;
                    }
                    files_with_matches += 1;

                    if context == 0 {
                        for (match_index, &ml) in match_lines.iter().enumerate() {
                            all_results.push(format!("{}:{}:{}", path_str, ml + 1, lines[ml]));
                            if all_results.len() >= max_results {
                                limit_hit = match_index + 1 < match_lines.len();
                                break;
                            }
                        }
                    } else {
                        let mut shown = vec![false; total];
                        for &ml in &match_lines {
                            let start = ml.saturating_sub(context);
                            let end = (ml + 1 + context).min(total);
                            for s in &mut shown[start..end] {
                                *s = true;
                            }
                        }

                        let mut i = 0;
                        while i < total && all_results.len() < max_results {
                            if !shown[i] {
                                i += 1;
                                continue;
                            }

                            if !all_results.is_empty() {
                                all_results.push("--".to_string());
                            }

                            while i < total && shown[i] && all_results.len() < max_results {
                                let is_match = match_lines.binary_search(&i).is_ok();
                                let sep = if is_match { ':' } else { '-' };
                                all_results.push(format!(
                                    "{}-{}{} {}",
                                    path_str,
                                    i + 1,
                                    sep,
                                    lines[i]
                                ));
                                i += 1;
                            }
                        }

                        if all_results.len() >= max_results
                            && i < total
                            && shown[i..].iter().any(|&is_shown| is_shown)
                        {
                            limit_hit = true;
                        }
                    }
                }
                Err(_) => continue,
            }

            if limit_hit {
                break;
            }
        }
        let current_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        crate::fs::ensure_same_file(&traversal_root, &authorized_metadata, &current_metadata)?;

        if all_results.is_empty() {
            let msg = "No matches found.".to_string();
            return Ok(match coaching {
                Some(c) => format!("{}\n\n{}", c, msg),
                None => msg,
            });
        }

        let total = all_results.len();
        let truncated = limit_hit;
        let result = if truncated {
            format!(
                "{} results (showing first {}, searched {} files):\n{}\n\n[truncated after {} matches — unknown number of additional matches; narrow the pattern or restrict to a path]",
                total,
                max_results,
                file_count,
                all_results.join("\n"),
                max_results
            )
        } else {
            format!(
                "{} results (searched {} files):\n{}",
                total,
                file_count,
                all_results.join("\n")
            )
        };

        // Add a "consider task" hint when results span multiple files and the
        // count is non-trivial. The agent sees this at the moment it decides
        // its next action, which is the highest-leverage point in the loop.
        // Suppressed when truncated, since the truncation hint already steers
        // the agent toward narrowing or task.
        let result = if !truncated && total >= 10 && files_with_matches >= 2 {
            format!(
                "{}\n\n[{} matches across {} files; for cross-file enumeration or synthesis, `task` returns a verified summary in one call]",
                result, total, files_with_matches,
            )
        } else {
            result
        };

        tracing::debug!(
            "tool grep done: files_searched={}, files_with_matches={}, total_matches={}, truncated={}",
            file_count,
            files_with_matches,
            total,
            truncated,
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
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "zerostack-grep-test-{}-{}-{sequence}",
                std::process::id(),
                tag,
            ));
            std::fs::create_dir_all(&path).expect("failed to create grep test directory");
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

    fn restrictive_permission_allowing_pattern() -> PermCheck {
        let config = PermissionConfig {
            grep: Some(ToolPerm::Granular(
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
            Some(vec!["standard".to_string()]),
        )))
    }

    async fn call_answering_path_permission(
        permission: PermCheck,
        args: GrepArgs,
        expected_path: &Path,
        decision: UserDecision,
    ) -> Result<String, ToolError> {
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = GrepTool::new(Some(permission), Some(ask_tx), 10);
        let call = tool.call(args);
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("grep did not request path permission")
                .expect("grep permission channel closed");
            assert_eq!(request.tool.as_str(), "grep");
            assert_eq!(
                PathBuf::from(request.input.as_str()),
                expected_path.to_path_buf()
            );
            request
                .reply
                .send(decision)
                .expect("grep dropped the permission reply");
        };

        let (result, ()) = tokio::join!(call, respond);
        result
    }

    #[tokio::test]
    async fn grep_external_path_permission_prompts_before_traversal() {
        let external = TempDir::new("restrictive-external");
        let canonical_external = std::fs::canonicalize(external.path()).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = GrepTool::new(
            Some(restrictive_permission_allowing_pattern()),
            Some(ask_tx),
            10,
        );

        let call = tool.call(GrepArgs {
            pattern: "needle".to_string(),
            path: Some(external.path().to_string_lossy().into_owned()),
            include: None,
            context_lines: None,
        });
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("grep did not request path permission")
                .expect("grep permission channel closed");
            assert_eq!(request.tool.as_str(), "grep");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_external);
            request
                .reply
                .send(UserDecision::Deny)
                .expect("grep dropped the permission reply");
        };

        let (result, ()) = tokio::join!(call, respond);
        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission denied by user"
        ));
    }

    #[tokio::test]
    async fn grep_external_path_permission_keeps_local_relative_searches() {
        let cwd = std::env::current_dir().unwrap();
        let dir = TempDir::new_in(&cwd, "local-relative");
        let marker = "grep_local_relative_marker";
        std::fs::write(dir.path().join("marker.txt"), marker).unwrap();
        let relative_root = dir.path().strip_prefix(&cwd).unwrap();

        let output = GrepTool::new(Some(standard_permission(&cwd)), None, 10)
            .call(GrepArgs {
                pattern: marker.to_string(),
                path: Some(relative_root.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            })
            .await
            .unwrap();

        assert!(output.contains(marker));
    }

    #[tokio::test]
    async fn grep_external_path_permission_uses_canonical_absolute_root() {
        let container = TempDir::new("absolute-external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            GrepArgs {
                pattern: "needle".to_string(),
                path: Some(external.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
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
    async fn grep_external_path_permission_policy_deny_prevents_traversal() {
        let container = TempDir::new("policy-deny");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "policy_deny_must_not_be_returned";
        std::fs::write(external.join("secret.txt"), marker).unwrap();
        let canonical_external = std::fs::canonicalize(&external).unwrap();
        let config = PermissionConfig {
            grep: Some(ToolPerm::Granular(
                [(
                    canonical_external.to_string_lossy().into_owned(),
                    Action::Deny,
                )]
                .into(),
            )),
            ..PermissionConfig::default()
        };
        let permission = Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(workspace),
            Some(vec!["standard".to_string()]),
        )));

        let result = GrepTool::new(Some(permission), None, 10)
            .call(GrepArgs {
                pattern: marker.to_string(),
                path: Some(external.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            })
            .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission denied: Blocked by deny rule"
        ));
    }

    #[tokio::test]
    async fn grep_external_path_permission_resolves_traversal_before_asking() {
        let container = TempDir::new("traversal-external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let requested = workspace.join("..").join("external");
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            GrepArgs {
                pattern: "needle".to_string(),
                path: Some(requested.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn grep_external_path_permission_expands_tilde_before_asking() {
        let home = PathBuf::from(crate::fs::expand_tilde("~"));
        assert_ne!(home, PathBuf::from("~"), "test requires a home directory");
        let workspace = TempDir::new("tilde-workspace");
        let canonical_home = std::fs::canonicalize(&home).unwrap();

        let result = call_answering_path_permission(
            standard_permission(workspace.path()),
            GrepArgs {
                pattern: "needle".to_string(),
                path: Some("~".to_string()),
                include: None,
                context_lines: None,
            },
            &canonical_home,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_external_path_permission_resolves_symlink_escape_before_asking() {
        let container = TempDir::new("symlink-external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        let link = workspace.join("escaped");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::os::unix::fs::symlink(&external, &link).unwrap();
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            GrepArgs {
                pattern: "needle".to_string(),
                path: Some(link.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_external_path_permission_binds_walker_to_authorized_symlink_target() {
        let container = TempDir::new("symlink-binding");
        let workspace = container.path().join("workspace");
        let authorized = container.path().join("authorized");
        let swapped = container.path().join("swapped");
        let link = workspace.join("root");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&swapped).unwrap();
        std::fs::write(
            authorized.join("authorized.txt"),
            "authorized_binding_marker",
        )
        .unwrap();
        std::fs::write(swapped.join("swapped.txt"), "swapped_binding_marker").unwrap();
        std::os::unix::fs::symlink(&authorized, &link).unwrap();
        let canonical_authorized = std::fs::canonicalize(&authorized).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = GrepTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let call = tool.call(GrepArgs {
            pattern: "binding_marker".to_string(),
            path: Some(link.to_string_lossy().into_owned()),
            include: None,
            context_lines: None,
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
        assert!(output.contains("authorized_binding_marker"));
        assert!(!output.contains("swapped_binding_marker"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_external_path_permission_rejects_authorized_root_replacement() {
        let container = TempDir::new("root-replacement");
        let workspace = container.path().join("workspace");
        let authorized = container.path().join("authorized");
        let moved = container.path().join("moved");
        let swapped = container.path().join("swapped");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&swapped).unwrap();
        std::fs::write(swapped.join("secret.txt"), "must_not_be_returned").unwrap();
        let canonical_authorized = std::fs::canonicalize(&authorized).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = GrepTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let call = tool.call(GrepArgs {
            pattern: "must_not_be_returned".to_string(),
            path: Some(authorized.to_string_lossy().into_owned()),
            include: None,
            context_lines: None,
        });
        let replace = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_authorized);
            std::fs::rename(&authorized, &moved).unwrap();
            std::os::unix::fs::symlink(&swapped, &authorized).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, replace);
        let error = result.expect_err("grep must reject a replaced traversal root");
        assert!(error.to_string().contains("Path changed"));
        assert!(!error.to_string().contains("must_not_be_returned"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn bound_file_reads_never_observe_an_aba_root_replacement() {
        let container = TempDir::new("aba-root-replacement");
        let authorized = container.path().join("authorized");
        let moved = container.path().join("moved");
        let replacement = container.path().join("replacement");
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        std::fs::write(authorized.join("one.txt"), "approved marker one").unwrap();
        std::fs::write(authorized.join("two.txt"), "approved marker two").unwrap();
        let secret = "aba_unique_secret_marker";
        std::fs::write(replacement.join("secret.txt"), secret).unwrap();

        let approved_metadata = std::fs::symlink_metadata(&authorized).unwrap();
        let bound = BoundDirectory::open(&authorized, &approved_metadata).unwrap();
        std::fs::rename(&authorized, &moved).unwrap();
        std::fs::rename(&replacement, &authorized).unwrap();

        let mut walker = bound.walker().unwrap();
        let mut first = walker.next().expect("approved directory has two files");
        let mut contents = String::new();
        first.file.read_to_string(&mut contents).unwrap();

        std::fs::rename(&authorized, &replacement).unwrap();
        std::fs::rename(&moved, &authorized).unwrap();
        for mut entry in walker {
            entry.file.read_to_string(&mut contents).unwrap();
        }

        assert!(contents.contains("approved marker one"));
        assert!(contents.contains("approved marker two"));
        assert!(!contents.contains(secret));
    }

    #[tokio::test]
    async fn grep_external_path_permission_pattern_cannot_widen_root() {
        let container = TempDir::new("pattern-root");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "pattern_must_not_escape_marker";
        std::fs::write(external.join("secret.txt"), marker).unwrap();

        let output = GrepTool::new(Some(standard_permission(&workspace)), None, 10)
            .call(GrepArgs {
                pattern: marker.to_string(),
                path: Some(workspace.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            })
            .await
            .unwrap();

        assert_eq!(output, "No matches found.");
    }

    #[tokio::test]
    async fn grep_external_path_permission_omitted_root_searches_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let dir = TempDir::new_in(&cwd, "omitted-root");
        let marker = "grep_omitted_root_marker";
        std::fs::write(dir.path().join("marker.txt"), marker).unwrap();

        let output = GrepTool::new(Some(standard_permission(&cwd)), None, 10)
            .call(GrepArgs {
                pattern: marker.to_string(),
                path: None,
                include: None,
                context_lines: None,
            })
            .await
            .unwrap();

        assert!(output.contains(marker));
    }

    #[tokio::test]
    async fn grep_external_path_permission_rejects_empty_root_before_asking() {
        let cwd = std::env::current_dir().unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = GrepTool::new(Some(standard_permission(&cwd)), Some(ask_tx), 10);

        let result = tool
            .call(GrepArgs {
                pattern: "needle".to_string(),
                path: Some(String::new()),
                include: None,
                context_lines: None,
            })
            .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Search path cannot be empty"
        ));
        assert!(ask_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn grep_external_path_permission_fails_closed_on_permission_channel_failure() {
        let container = TempDir::new("closed-permission-channel");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "closed_permission_channel_marker";
        std::fs::write(external.join("secret.txt"), marker).unwrap();
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
        drop(ask_rx);
        let tool = GrepTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let result = tool
            .call(GrepArgs {
                pattern: marker.to_string(),
                path: Some(external.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            })
            .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission system unavailable"
        ));
    }

    #[tokio::test]
    async fn reports_unknown_additional_matches_when_limit_is_hit() {
        let dir = TempDir::new("truncated");
        std::fs::write(dir.path().join("matches.txt"), "needle\nneedle\nneedle\n")
            .expect("failed to write grep test file");
        let tool = GrepTool::new(None, None, 2);

        let output = tool
            .call(GrepArgs {
                pattern: "needle".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            })
            .await
            .expect("grep failed");

        assert!(output.contains("unknown number of additional matches"));
        assert!(!output.contains("0 more matches"));
    }

    #[tokio::test]
    async fn does_not_report_truncation_when_walker_is_exhausted_at_limit() {
        let dir = TempDir::new("exact-limit");
        std::fs::write(dir.path().join("matches.txt"), "needle\nneedle\n")
            .expect("failed to write grep test file");
        let tool = GrepTool::new(None, None, 2);

        let output = tool
            .call(GrepArgs {
                pattern: "needle".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
            })
            .await
            .expect("grep failed");

        assert!(!output.contains("[truncated after"));
        assert!(output.starts_with("2 results (searched 1 files):"));
    }
}
