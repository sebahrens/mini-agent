use ignore::WalkBuilder;
use regex::Regex;
use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, GrepArgs, PermCheck, ToolError, check_perm, check_perm_path, is_skip_dir,
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

        let search_path = crate::fs::expand_tilde(args.path.as_deref().unwrap_or("."));
        let _ = check_perm_path(&self.permission, &self.ask_tx, "grep", &search_path).await?;
        let context = args.context_lines.unwrap_or(0);

        let include_re = args.include.as_ref().map(|g| {
            let pattern = format!("^(?:{})$", Self::glob_to_regex(g));
            Regex::new(&pattern).unwrap_or_else(|_| Regex::new(".*").unwrap())
        });

        let walker = WalkBuilder::new(&search_path)
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
        let mut file_count = 0;
        let mut files_with_matches: usize = 0;
        let mut all_results: Vec<String> = Vec::with_capacity(max_results.min(64));
        let mut limit_hit = false;

        for entry in walker
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            if all_results.len() >= max_results {
                limit_hit = true;
                break;
            }

            if let Some(ref re_include) = include_re {
                let fname = entry.file_name().to_string_lossy();
                if !re_include.is_match(&fname) {
                    continue;
                }
            }

            if let Ok(meta) = entry.metadata()
                && meta.len() > 10 * 1024 * 1024
            {
                continue;
            }

            let path_str = entry.path().to_string_lossy().to_string();

            match tokio::fs::read(entry.path()).await {
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
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::permission::ask::UserDecision;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is before Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "zerostack-grep-test-{}-{}-{unique}",
                std::process::id(),
                name
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

    #[tokio::test]
    async fn prompts_before_searching_external_path() {
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = GrepTool::new(
            Some(restrictive_permission_allowing_pattern()),
            Some(ask_tx),
            10,
        );

        let call = tool.call(GrepArgs {
            pattern: "needle".to_string(),
            path: Some("/etc".to_string()),
            include: None,
            context_lines: None,
        });
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("grep did not request path permission")
                .expect("grep permission channel closed");
            assert_eq!(request.tool.as_str(), "grep");
            assert_eq!(request.input, "/etc");
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
