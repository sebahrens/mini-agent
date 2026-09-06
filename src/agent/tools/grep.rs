use std::io::Read;
use std::ops::Range;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use rig::tool::Tool;

use super::find_files::BoundDirectory;
use crate::agent::tools::{
    AskSender, GrepArgs, PermCheck, ToolError, check_perm, check_perm_bound_path, check_perm_path,
    combine_coaching,
};
const MAX_OUTPUT_LINE_CHARS: usize = 500;
const MAX_SEARCH_FILE_BYTES: u64 = 10 * 1024 * 1024;
const BINARY_SNIFF_BYTES: u64 = 8 * 1024;

struct GrepSearchResult {
    file_count: usize,
    files_with_matches: usize,
    results: Vec<String>,
    emitted_results: usize,
    limit_hit: bool,
}

#[derive(Clone, Copy)]
struct GrepSearchOptions {
    max_results: usize,
    context: usize,
    files_only: bool,
    count: bool,
}

pub(crate) fn truncate_output_line(line: &str, match_range: Option<Range<usize>>) -> String {
    let total_chars = line.chars().count();
    if total_chars <= MAX_OUTPUT_LINE_CHARS {
        return line.to_string();
    }

    let (mut start_char, mut end_char) = if let Some(range) = match_range {
        let match_start = line[..range.start].chars().count();
        let match_end = match_start + line[range].chars().count();
        let match_chars = match_end.saturating_sub(match_start);

        if match_chars >= MAX_OUTPUT_LINE_CHARS {
            (match_start, match_start + MAX_OUTPUT_LINE_CHARS)
        } else {
            let surrounding = MAX_OUTPUT_LINE_CHARS - match_chars;
            let start = match_start.saturating_sub(surrounding / 2);
            let end =
                (match_end + surrounding.saturating_sub(match_start - start)).min(total_chars);
            (start, end)
        }
    } else {
        (0, MAX_OUTPUT_LINE_CHARS)
    };

    if end_char - start_char < MAX_OUTPUT_LINE_CHARS {
        start_char = start_char.saturating_sub(MAX_OUTPUT_LINE_CHARS - (end_char - start_char));
    }
    end_char = (start_char + MAX_OUTPUT_LINE_CHARS).min(total_chars);

    let start_byte = line
        .char_indices()
        .nth(start_char)
        .map_or(line.len(), |(index, _)| index);
    let end_byte = line
        .char_indices()
        .nth(end_char)
        .map_or(line.len(), |(index, _)| index);
    let mut output = String::with_capacity(end_byte - start_byte + 6);
    if start_char > 0 {
        output.push('…');
    }
    output.push_str(&line[start_byte..end_byte]);
    if end_char < total_chars {
        output.push('…');
    }
    output
}

pub struct GrepTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_results: u64,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
}

impl GrepTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, max_results: u64) -> Self {
        GrepTool {
            permission,
            ask_tx,
            max_results,
            workspace: None,
        }
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub(crate) fn with_workspace(self, root: impl Into<std::path::PathBuf>) -> Self {
        self.with_workspace_binding(crate::agent::tools::capture_workspace_binding(root.into()))
    }

    pub(crate) fn glob_to_regex(glob: &str) -> String {
        let mut re = String::with_capacity(glob.len() * 2);
        let chars: Vec<char> = glob.chars().collect();
        let mut index = 0;
        let mut brace_depth = 0_usize;
        while index < chars.len() {
            let c = chars[index];
            match c {
                '.' => re.push_str("\\."),
                '*' if chars.get(index + 1) == Some(&'*') => {
                    if chars.get(index + 2) == Some(&'/') {
                        re.push_str("(?:.*/)?");
                        index += 2;
                    } else {
                        re.push_str(".*");
                        index += 1;
                    }
                }
                '*' => re.push_str("[^/]*"),
                '?' => re.push_str("[^/]"),
                '{' => {
                    brace_depth += 1;
                    re.push_str("(?:");
                }
                '}' => {
                    brace_depth = brace_depth.saturating_sub(1);
                    re.push(')');
                }
                ',' if brace_depth > 0 => re.push('|'),
                '(' | ')' | '[' | ']' | '+' | '^' | '$' | '|' | '\\' => {
                    re.push('\\');
                    re.push(c);
                }
                _ => re.push(c),
            }
            index += 1;
        }
        re
    }

    pub(crate) fn compile_include_glob(glob: &str) -> Result<(Regex, bool), ToolError> {
        let mut depth = 0_usize;
        for character in glob.chars() {
            match character {
                '{' => depth += 1,
                '}' if depth == 0 => {
                    return Err(ToolError::Msg(format!(
                        "Invalid include glob '{glob}': unmatched closing brace"
                    )));
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth != 0 {
            return Err(ToolError::Msg(format!(
                "Invalid include glob '{glob}': unclosed brace"
            )));
        }
        let pattern = format!("^(?:{})$", Self::glob_to_regex(glob));
        let regex = Regex::new(&pattern)
            .map_err(|error| ToolError::Msg(format!("Invalid include glob '{glob}': {error}")))?;
        Ok((regex, glob.contains('/')))
    }

    pub(crate) fn is_binary(data: &[u8]) -> bool {
        data.iter().take(8192).any(|&b| b == 0)
    }

    pub(crate) fn read_non_binary<R: Read>(
        reader: &mut R,
        capacity: usize,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let mut prefix = Vec::with_capacity(capacity.min(BINARY_SNIFF_BYTES as usize));
        reader
            .by_ref()
            .take(BINARY_SNIFF_BYTES)
            .read_to_end(&mut prefix)?;
        if Self::is_binary(&prefix) {
            return Ok(None);
        }

        let mut data = Vec::with_capacity(capacity);
        data.extend_from_slice(&prefix);
        reader.read_to_end(&mut data)?;
        Ok(Some(data))
    }
}

fn search_bound_directory(
    bound_directory: BoundDirectory,
    re: Regex,
    include_re: Option<(Regex, bool)>,
    options: GrepSearchOptions,
) -> Result<GrepSearchResult, ToolError> {
    let include_root = bound_directory.approved_root().to_path_buf();
    let walker = bound_directory.walker()?;
    let mut file_count = 0;
    let mut files_with_matches = 0;
    let mut results = Vec::with_capacity(options.max_results.min(64));
    let mut emitted_results = 0_usize;
    let mut limit_hit = false;

    for entry in walker {
        if emitted_results >= options.max_results {
            limit_hit = true;
            break;
        }

        if let Some((re_include, path_aware)) = &include_re {
            let candidate = if *path_aware {
                entry
                    .path
                    .strip_prefix(&include_root)
                    .unwrap_or(&entry.path)
                    .to_string_lossy()
                    .replace('\\', "/")
            } else {
                entry.file_name.to_string_lossy().into_owned()
            };
            if !re_include.is_match(&candidate) {
                continue;
            }
        }

        if entry.metadata.len() > MAX_SEARCH_FILE_BYTES {
            continue;
        }

        let path_str = entry.path.to_string_lossy().to_string();
        let capacity = entry.metadata.len() as usize;
        let mut file = entry.file;
        let Some(data) = GrepTool::read_non_binary(&mut file, capacity).unwrap_or_default() else {
            continue;
        };
        file_count += 1;
        let content = String::from_utf8_lossy(&data);
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let match_lines: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| re.is_match(line))
            .map(|(index, _)| index)
            .collect();

        if match_lines.is_empty() {
            continue;
        }
        files_with_matches += 1;

        if options.files_only {
            results.push(path_str);
            emitted_results += 1;
            continue;
        }
        if options.count {
            results.push(format!("{}:{}", path_str, match_lines.len()));
            emitted_results += 1;
            continue;
        }

        if options.context == 0 {
            for (match_index, &matched_line) in match_lines.iter().enumerate() {
                let matched = re.find(lines[matched_line]).map(|found| found.range());
                let displayed = truncate_output_line(lines[matched_line], matched);
                results.push(format!("{}:{}:{}", path_str, matched_line + 1, displayed));
                emitted_results += 1;
                if emitted_results >= options.max_results {
                    limit_hit = match_index + 1 < match_lines.len();
                    break;
                }
            }
        } else {
            let mut shown = vec![false; total];
            for &matched_line in &match_lines {
                let start = matched_line.saturating_sub(options.context);
                let end = (matched_line + 1 + options.context).min(total);
                for shown_line in &mut shown[start..end] {
                    *shown_line = true;
                }
            }

            let mut index = 0;
            while index < total && emitted_results < options.max_results {
                if !shown[index] {
                    index += 1;
                    continue;
                }

                if !results.is_empty() {
                    results.push("--".to_string());
                }

                while index < total && shown[index] && emitted_results < options.max_results {
                    let is_match = match_lines.binary_search(&index).is_ok();
                    let separator = if is_match { ':' } else { '-' };
                    let matched = is_match
                        .then(|| re.find(lines[index]))
                        .flatten()
                        .map(|found| found.range());
                    let displayed = truncate_output_line(lines[index], matched);
                    results.push(format!(
                        "{}:{}{} {}",
                        path_str,
                        index + 1,
                        separator,
                        displayed
                    ));
                    emitted_results += 1;
                    index += 1;
                }
            }

            if emitted_results >= options.max_results
                && index < total
                && shown[index..].iter().any(|&is_shown| is_shown)
            {
                limit_hit = true;
            }
        }

        if limit_hit {
            break;
        }
    }

    Ok(GrepSearchResult {
        file_count,
        files_with_matches,
        results,
        emitted_results,
        limit_hit,
    })
}

impl Tool for GrepTool {
    const NAME: &'static str = "grep";

    type Error = ToolError;
    type Args = GrepArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "Search file contents using a regex pattern (Rust regex syntax). Returns at most {} result lines and truncates each displayed source line to {} characters around its match. Files larger than 10 MiB and binary files are skipped. Respects .gitignore. Skips dependency/build directories and VCS metadata unless that directory is requested explicitly.",
            self.max_results, MAX_OUTPUT_LINE_CHARS
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for (Rust regex syntax; inline flags such as (?i) are supported)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (defaults to current working directory)"
                },
                "include": {
                    "type": "string",
                    "description": "Optional path-aware file glob (e.g. '*.rs', 'src/*.rs', '**/*.{ts,tsx}')"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match (like grep -C)"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Match the pattern without regard to case (default false)"
                },
                "files_only": {
                    "type": "boolean",
                    "description": "Return only paths of files containing a match (default false)"
                },
                "count": {
                    "type": "boolean",
                    "description": "Return each matching path and its match count (default false)"
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

        if args.files_only && args.count {
            return Err(ToolError::Msg(
                "grep files_only and count modes are mutually exclusive".to_string(),
            ));
        }
        let re = RegexBuilder::new(&args.pattern)
            .case_insensitive(args.case_insensitive)
            .build()
            .map_err(|e| ToolError::Msg(format!("Invalid regex pattern: {}", e)))?;

        let requested_path = args.path.as_deref().unwrap_or(".");
        if requested_path.is_empty() {
            return Err(ToolError::Msg("Search path cannot be empty".to_string()));
        }
        let workspace_root =
            crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let search_path =
            crate::agent::tools::resolve_tool_path(workspace_root.as_deref(), requested_path);
        let relative = Path::new(requested_path);
        let (bound_directory, path_coaching) = if !relative.is_absolute()
            && !requested_path.starts_with('~')
            && let Some(workspace) = &self.workspace
        {
            let logical = workspace.logical_relative_path(relative)?;
            let directory = workspace.open_relative_directory_file(relative)?;
            let bound = BoundDirectory::from_file(&logical, directory)?;
            let coaching =
                check_perm_bound_path(&self.permission, &self.ask_tx, "grep", workspace, relative)
                    .await?;
            (bound, coaching)
        } else {
            let traversal_root = tokio::fs::canonicalize(&search_path).await?;
            let authorized_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
            let bound = BoundDirectory::open(&traversal_root, &authorized_metadata)?;
            let coaching = check_perm_path(
                &self.permission,
                &self.ask_tx,
                "grep",
                &traversal_root.to_string_lossy(),
            )
            .await?;
            (bound, coaching)
        };
        let coaching = combine_coaching(coaching, path_coaching);
        let context = args.context_lines.unwrap_or(0);

        let include_re = args
            .include
            .as_deref()
            .map(Self::compile_include_glob)
            .transpose()?;

        let max_results = self.max_results as usize;
        let files_only = args.files_only;
        let count = args.count;
        let search = crate::agent::runner::spawn_blocking_scoped(move || {
            search_bound_directory(
                bound_directory,
                re,
                include_re,
                GrepSearchOptions {
                    max_results,
                    context,
                    files_only,
                    count,
                },
            )
        })
        .await
        .map_err(|error| ToolError::Msg(format!("grep directory walker failed: {error}")))??;
        let GrepSearchResult {
            file_count,
            files_with_matches,
            results: all_results,
            emitted_results,
            limit_hit,
        } = search;
        if all_results.is_empty() {
            let msg = "No matches found.".to_string();
            return Ok(match coaching {
                Some(c) => format!("{}\n\n{}", c, msg),
                None => msg,
            });
        }

        let total = emitted_results;
        let truncated = limit_hit;
        let result_label = if args.files_only || args.count {
            "matching files"
        } else {
            "results"
        };
        let truncation_detail = if args.files_only || args.count {
            "additional matching files may exist"
        } else {
            "unknown number of additional matches"
        };
        let result = if truncated {
            format!(
                "{} {} (showing first {}, searched {} files):\n{}\n\n[truncated after {} {} — {}; narrow the pattern or restrict to a path]",
                total,
                result_label,
                max_results,
                file_count,
                all_results.join("\n"),
                max_results,
                result_label,
                truncation_detail,
            )
        } else {
            format!(
                "{} {} (searched {} files):\n{}",
                total,
                result_label,
                file_count,
                all_results.join("\n")
            )
        };

        // Add a "consider task" hint when results span multiple files and the
        // count is non-trivial. The agent sees this at the moment it decides
        // its next action, which is the highest-leverage point in the loop.
        // Suppressed when truncated, since the truncation hint already steers
        // the agent toward narrowing or task.
        let result = if !args.files_only
            && !args.count
            && !truncated
            && total >= 10
            && files_with_matches >= 2
        {
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
        Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(config),
                SecurityMode::Restrictive,
                Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
                Some(vec!["restrictive".to_string()]),
            )
            .expect("valid permission test configuration"),
        ))
    }

    fn standard_permission(working_dir: &Path) -> PermCheck {
        Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::default(),
                SecurityMode::Standard,
                Some(working_dir.to_path_buf()),
                Some(vec!["standard".to_string()]),
            )
            .expect("valid permission test configuration"),
        ))
    }

    #[tokio::test]
    async fn cancelled_turn_keeps_real_grep_read_owned_until_it_finishes() {
        let directory = TempDir::new("owned-read");
        std::fs::write(directory.path().join("match.txt"), "needle\n")
            .expect("failed to create grep fixture");
        let path = directory.path().to_string_lossy().into_owned();
        let (scope, read_started, release_read) =
            crate::agent::runner::AgentWorkScope::new_with_blocking_test_gate();
        let cancellation = scope.cancellation_handle();

        let call = tokio::spawn({
            let scope = Arc::clone(&scope);
            async move {
                scope
                    .run(async move {
                        GrepTool::new(None, None, 10)
                            .call(GrepArgs {
                                pattern: "needle".to_owned(),
                                path: Some(path),
                                include: None,
                                context_lines: None,
                                case_insensitive: false,
                                files_only: false,
                                count: false,
                            })
                            .await
                    })
                    .await
            }
        });

        tokio::task::spawn_blocking(move || {
            read_started
                .recv_timeout(Duration::from_secs(1))
                .expect("real GrepTool file read should enter the scoped blocking pool");
        })
        .await
        .unwrap();
        cancellation.cancel();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        assert_eq!(
            scope.active_children(),
            1,
            "turn settlement must retain the in-flight GrepTool file read"
        );

        release_read.release();
        tokio::time::timeout(Duration::from_secs(1), scope.wait_idle())
            .await
            .expect("GrepTool read should release its turn ownership after completion");
        assert_eq!(scope.active_children(), 0);
    }

    #[test]
    fn glob_to_regex_escapes_literal_regex_metacharacters() {
        for (glob, literal_match, regex_only_match) in [
            ("test(1).rs", "test(1).rs", "test1.rs"),
            ("file[1].js", "file[1].js", "file1.js"),
            ("prefix+.js", "prefix+.js", "prefixx.js"),
            ("cash^$|\\.txt", "cash^$|\\.txt", "cash.txt"),
        ] {
            let pattern = format!("^(?:{})$", GrepTool::glob_to_regex(glob));
            let regex = Regex::new(&pattern).expect("glob must produce a valid regex");

            assert!(
                regex.is_match(literal_match),
                "{glob:?} must match literally"
            );
            assert!(
                !regex.is_match(regex_only_match),
                "{glob:?} must not treat literal characters as regex syntax"
            );
        }
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
            case_insensitive: false,
            files_only: false,
            count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
        let permission = Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(config),
                SecurityMode::Standard,
                Some(workspace),
                Some(vec!["standard".to_string()]),
            )
            .expect("valid permission test configuration"),
        ));

        let result = GrepTool::new(Some(permission), None, 10)
            .call(GrepArgs {
                pattern: marker.to_string(),
                path: Some(external.to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
            case_insensitive: false,
            files_only: false,
            count: false,
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
    async fn grep_external_path_permission_retains_authorized_root_on_replacement() {
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
            case_insensitive: false,
            files_only: false,
            count: false,
        });
        let replace = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_authorized);
            std::fs::rename(&authorized, &moved).unwrap();
            std::os::unix::fs::symlink(&swapped, &authorized).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, replace);
        let output = result.expect("descriptor-bound grep must retain the authorized root");
        assert!(output.contains("No matches found"));
        assert!(!output.contains("must_not_be_returned"));
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

        let approved_metadata = crate::fs::checked_path_metadata(&authorized).unwrap();
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
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
                case_insensitive: false,
                files_only: false,
                count: false,
            })
            .await
            .expect("grep failed");

        assert!(!output.contains("[truncated after"));
        assert!(output.starts_with("2 results (searched 1 files):"));
    }

    #[tokio::test]
    async fn truncates_long_match_lines_around_the_match_on_character_boundaries() {
        let dir = TempDir::new("long-match-line");
        let long_line = format!("{}needle{}", "α".repeat(800), "β".repeat(800));
        std::fs::write(dir.path().join("minified.js"), &long_line)
            .expect("failed to write grep test file");

        let output = GrepTool::new(None, None, 10)
            .call(GrepArgs {
                pattern: "needle".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
                include: None,
                context_lines: None,
                case_insensitive: false,
                files_only: false,
                count: false,
            })
            .await
            .expect("grep failed");

        let result_line = output
            .lines()
            .find(|line| line.contains("minified.js:"))
            .expect("grep result line");
        let displayed = result_line
            .rsplit_once(":1:")
            .map(|(_, content)| content)
            .expect("path:line:content format");
        assert!(displayed.contains("needle"), "{displayed}");
        assert!(displayed.starts_with('…'), "{displayed}");
        assert!(displayed.ends_with('…'), "{displayed}");
        assert_eq!(displayed.chars().count(), MAX_OUTPUT_LINE_CHARS + 2);
        assert!(!output.contains(&long_line));
    }

    #[tokio::test]
    async fn truncates_long_context_lines_as_well_as_matches() {
        let dir = TempDir::new("long-context-line");
        let context_line = "x".repeat(1_000);
        std::fs::write(
            dir.path().join("context.txt"),
            format!("{context_line}\nneedle\n"),
        )
        .expect("failed to write grep test file");

        let output = GrepTool::new(None, None, 10)
            .call(GrepArgs {
                pattern: "needle".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
                include: None,
                context_lines: Some(1),
                case_insensitive: false,
                files_only: false,
                count: false,
            })
            .await
            .expect("grep failed");

        assert!(!output.contains(&context_line));
        let context_result = output
            .lines()
            .find(|line| line.contains("context.txt:1-"))
            .expect("context result line");
        assert!(context_result.ends_with('…'), "{context_result}");
    }

    #[test]
    fn description_discloses_result_and_file_size_limits() {
        let description = GrepTool::new(None, None, 37).description();
        assert!(description.contains("at most 37 result lines"));
        assert!(description.contains("500 characters"));
        assert!(description.contains("10 MiB"));
    }

    fn ergonomic_args(path: &Path, include: Option<&str>) -> GrepArgs {
        GrepArgs {
            pattern: "needle".to_string(),
            path: Some(path.to_string_lossy().into_owned()),
            include: include.map(str::to_string),
            context_lines: None,
            case_insensitive: false,
            files_only: false,
            count: false,
        }
    }

    #[tokio::test]
    async fn path_aware_include_globs_match_root_and_nested_files() {
        let dir = TempDir::new("path-aware-include");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("root.rs"), "Needle\n").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "Needle\n").unwrap();
        std::fs::write(dir.path().join("src/lib.txt"), "Needle\n").unwrap();

        let mut args = ergonomic_args(dir.path(), Some("**/*.rs"));
        args.case_insensitive = true;
        let output = GrepTool::new(None, None, 10)
            .call(args)
            .await
            .unwrap()
            .replace('\\', "/");
        assert!(output.contains("root.rs"), "{output}");
        assert!(output.contains("src/lib.rs"), "{output}");
        assert!(!output.contains("lib.txt"), "{output}");

        let mut args = ergonomic_args(dir.path(), Some("src/*.rs"));
        args.case_insensitive = true;
        let output = GrepTool::new(None, None, 10)
            .call(args)
            .await
            .unwrap()
            .replace('\\', "/");
        assert!(!output.contains("root.rs"), "{output}");
        assert!(output.contains("src/lib.rs"), "{output}");
    }

    #[tokio::test]
    async fn invalid_include_glob_is_an_explicit_error() {
        let dir = TempDir::new("invalid-include");
        let error = GrepTool::new(None, None, 10)
            .call(ergonomic_args(dir.path(), Some("*.{rs,txt")))
            .await
            .expect_err("malformed glob must not become match-all")
            .to_string();
        assert!(error.contains("Invalid include glob"), "{error}");
        assert!(error.contains("unclosed brace"), "{error}");
    }

    #[tokio::test]
    async fn files_only_and_count_modes_return_compact_file_results() {
        let dir = TempDir::new("compact-modes");
        std::fs::write(dir.path().join("two.txt"), "needle\nneedle again\n").unwrap();

        let mut files_only = ergonomic_args(dir.path(), None);
        files_only.files_only = true;
        let output = GrepTool::new(None, None, 10)
            .call(files_only)
            .await
            .unwrap();
        assert_eq!(output.matches("two.txt").count(), 1, "{output}");
        assert!(!output.contains("needle again"), "{output}");

        let mut count = ergonomic_args(dir.path(), None);
        count.count = true;
        let output = GrepTool::new(None, None, 10).call(count).await.unwrap();
        assert!(
            output.lines().any(|line| line.ends_with("two.txt:2")),
            "{output}"
        );
    }

    #[tokio::test]
    async fn context_output_uses_path_colon_line_and_separators_do_not_consume_limit() {
        let dir = TempDir::new("context-format");
        std::fs::write(dir.path().join("context.txt"), "before\nneedle\nafter\n").unwrap();
        let mut args = ergonomic_args(dir.path(), None);
        args.context_lines = Some(1);

        let output = GrepTool::new(None, None, 3).call(args).await.unwrap();
        assert!(
            output
                .lines()
                .any(|line| line.contains("context.txt:1- before")),
            "{output}"
        );
        assert!(
            output
                .lines()
                .any(|line| line.contains("context.txt:2: needle")),
            "{output}"
        );
        assert!(
            output
                .lines()
                .any(|line| line.contains("context.txt:3- after")),
            "{output}"
        );
        assert!(output.starts_with("3 results"), "{output}");
    }
}
