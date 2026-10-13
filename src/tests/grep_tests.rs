use crate::agent::tools::GrepTool;

struct SniffOnlyBinaryReader {
    emitted: usize,
}

impl std::io::Read for SniffOnlyBinaryReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        assert!(
            self.emitted < 8192,
            "binary detection must not read beyond the sniff window"
        );
        let count = buffer.len().min(8192 - self.emitted);
        buffer[..count].fill(b'a');
        if self.emitted == 0 && count > 0 {
            buffer[0] = 0;
        }
        self.emitted += count;
        Ok(count)
    }
}

#[test]
fn glob_literal_chars() {
    assert_eq!(GrepTool::glob_to_regex("hello"), "hello");
}

#[test]
fn glob_dot() {
    assert_eq!(GrepTool::glob_to_regex("file.txt"), "file\\.txt");
}

#[test]
fn glob_star() {
    assert_eq!(GrepTool::glob_to_regex("*.rs"), "[^/]*\\.rs");
}

#[test]
fn glob_question_mark() {
    assert_eq!(GrepTool::glob_to_regex("file.?"), "file\\.[^/]");
}

#[test]
fn glob_brace_alternation() {
    assert_eq!(GrepTool::glob_to_regex("*.{ts,tsx}"), "[^/]*\\.(?:ts|tsx)");
}

#[test]
fn glob_complex_pattern() {
    assert_eq!(
        GrepTool::glob_to_regex("src/**/test_*.{rs,toml}"),
        "src/(?:.*/)?test_[^/]*\\.(?:rs|toml)"
    );
}

#[test]
fn glob_empty() {
    assert_eq!(GrepTool::glob_to_regex(""), "");
}

#[test]
fn is_binary_null_byte_in_first_8k() {
    let mut data = vec![b'a'; 100];
    data[50] = 0;
    assert!(GrepTool::is_binary(&data));
}

#[test]
fn is_binary_no_null_byte() {
    let data = vec![b'a'; 100];
    assert!(!GrepTool::is_binary(&data));
}

#[test]
fn is_binary_empty() {
    assert!(!GrepTool::is_binary(&[]));
}

#[test]
fn is_binary_null_at_start() {
    let data = vec![0, b'a', b'b'];
    assert!(GrepTool::is_binary(&data));
}

#[test]
fn is_binary_null_at_end() {
    let mut data = vec![b'a'; 8192];
    data[8191] = 0;
    assert!(GrepTool::is_binary(&data));
}

#[test]
fn is_binary_all_text() {
    let data = b"hello world\nline 2\nline 3\n";
    assert!(!GrepTool::is_binary(data));
}

#[test]
fn is_binary_non_utf8_no_null() {
    let data = vec![0xFF, 0xFE, 0xFD];
    assert!(!GrepTool::is_binary(&data));
}

#[test]
fn binary_reader_stops_after_the_sniff_window() {
    let mut reader = SniffOnlyBinaryReader { emitted: 0 };
    let result = GrepTool::read_non_binary(&mut reader, 32 * 1024).unwrap();
    assert!(result.is_none());
    assert_eq!(reader.emitted, 8192);
}

#[test]
fn text_reader_preserves_the_sniffed_prefix_and_remaining_bytes() {
    let expected = vec![b'x'; 10 * 1024];
    let mut reader = std::io::Cursor::new(expected.clone());
    let result = GrepTool::read_non_binary(&mut reader, expected.len())
        .unwrap()
        .expect("text input");
    assert_eq!(result, expected);
}

fn search_args(
    path: &std::path::Path,
    options: serde_json::Value,
) -> crate::agent::tools::GrepArgs {
    let mut value = serde_json::json!({"pattern":"needle", "path":path.to_string_lossy()});
    value
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn grep_per_call_limit_restricts_source_context_and_compact_results() {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("grep-per-call-cap");
    std::fs::write(
        root.path().join("hits.txt"),
        "before\nneedle\nneedle\nafter\n",
    )
    .unwrap();
    let tool = GrepTool::new(None, None, 5);
    assert_eq!(tool.parameters()["properties"]["max_results"]["maximum"], 5);
    let default = tool
        .call(search_args(root.path(), serde_json::json!({})))
        .await
        .unwrap();
    assert!(default.starts_with("2 results"), "{default}");
    let limited = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"max_results":1}),
        ))
        .await
        .unwrap();
    assert!(
        limited.starts_with("1 results (showing first 1"),
        "{limited}"
    );
    assert!(limited.contains("hits.txt:2:needle"), "{limited}");
    assert!(!limited.contains("hits.txt:3:"), "{limited}");
    let context = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"max_results":2,"context_lines":usize::MAX}),
        ))
        .await
        .unwrap();
    assert!(context.starts_with("2 results"), "{context}");
    assert!(context.contains("hits.txt:1- before"), "{context}");
    assert!(!context.contains("hits.txt:3"), "{context}");
    for cap in [0, 6, u64::MAX] {
        let error = tool
            .call(search_args(
                root.path(),
                serde_json::json!({"max_results":cap}),
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("configured cap 5"), "{error}");
    }
    std::fs::write(root.path().join("other.txt"), "needle").unwrap();
    for mode in ["files_only", "count"] {
        let mut options = serde_json::json!({"max_results":1});
        options[mode] = true.into();
        let output = tool.call(search_args(root.path(), options)).await.unwrap();
        assert!(output.starts_with("1 matching files"), "{output}");
        assert_eq!(
            output.lines().filter(|line| line.contains(".txt")).count(),
            1,
            "{output}"
        );
    }
}

#[tokio::test]
async fn grep_multiline_matches_whole_files_and_projects_numbered_unicode_crlf_lines() {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("grep-multiline");
    std::fs::write(
        root.path().join("hits.txt"),
        "before\r\nα begin\r\nend β\r\nafter\r\nα begin\nend β\n",
    )
    .unwrap();
    let tool = GrepTool::new(None, None, 20);
    let plain = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"begin.*end"}),
        ))
        .await
        .unwrap();
    assert_eq!(plain, "No matches found.");
    let multiline = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"begin.*?end", "multiline":true}),
        ))
        .await
        .unwrap();
    assert!(multiline.starts_with("4 results"), "{multiline}");
    for expected in [
        "hits.txt:2:α begin",
        "hits.txt:3:end β",
        "hits.txt:5:α begin",
        "hits.txt:6:end β",
    ] {
        assert!(multiline.contains(expected), "{multiline}");
    }
    let count = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"begin.*?end","multiline":true,"count":true}),
        ))
        .await
        .unwrap();
    assert!(
        count.lines().any(|line| line.ends_with("hits.txt:2")),
        "{count}"
    );
    let files = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"begin.*?end","multiline":true,"files_only":true}),
        ))
        .await
        .unwrap();
    assert_eq!(files.matches("hits.txt").count(), 1, "{files}");
    let capped = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"begin.*?end","multiline":true,"max_results":1}),
        ))
        .await
        .unwrap();
    assert!(capped.contains("hits.txt:2:α begin"), "{capped}");
    assert!(!capped.contains("hits.txt:3"), "{capped}");
    assert!(capped.contains("truncated after 1 results"), "{capped}");
    let disabled_dot = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"(?-s:begin.*end)","multiline":true}),
        ))
        .await
        .unwrap();
    assert_eq!(disabled_dot, "No matches found.");
    let context = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"begin.*?end","multiline":true,"context_lines":1}),
        ))
        .await
        .unwrap();
    assert!(context.starts_with("6 results"), "{context}");
    assert!(context.contains("hits.txt:1- before"), "{context}");
    assert!(context.contains("hits.txt:3: end β"), "{context}");
}

#[tokio::test]
async fn grep_multiline_zero_width_and_match_budget_are_bounded() {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("grep-multiline-budget");
    std::fs::write(root.path().join("hits.txt"), "α\nβ\n").unwrap();
    let tool = GrepTool::new(None, None, 5);
    let output = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"(?m)^|\\z","multiline":true}),
        ))
        .await
        .unwrap();
    assert!(output.starts_with("2 results"), "{output}");
    assert!(
        output.contains("hits.txt:1:α") && output.contains("hits.txt:2:β"),
        "{output}"
    );
    std::fs::write(root.path().join("hits.txt"), "x\n".repeat(100_001)).unwrap();
    let output = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"(?m)^","multiline":true,"count":true}),
        ))
        .await
        .unwrap();
    assert!(
        output.lines().any(|line| line.ends_with("hits.txt:100000")),
        "{output}"
    );
    assert!(output.contains("search work limit reached"), "{output}");
}

#[tokio::test]
async fn grep_multiline_and_per_call_cap_preserve_permission_denial() {
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
    use rig::tool::Tool;
    use std::sync::{Arc, Mutex};
    let root = crate::tests::OwnedTestDirectory::new("grep-multiline-deny");
    std::fs::write(root.path().join("secret.txt"), "PRIVATE_A\nPRIVATE_B").unwrap();
    let config = PermissionConfig {
        grep: Some(ToolPerm::Simple(Action::Deny)),
        ..Default::default()
    };
    let permission = Arc::new(Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(root.path().to_path_buf()),
            None,
        )
        .unwrap(),
    ));
    let output = GrepTool::new(Some(permission), None, 5)
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"PRIVATE_A.*PRIVATE_B","multiline":true,"max_results":1}),
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(output.contains("denied"), "{output}");
    assert!(!output.contains("secret.txt"), "{output}");
}

#[test]
fn text_reader_limits_actual_bytes_even_when_metadata_underestimates_growth() {
    let mut reader = std::io::repeat(b'x');
    assert!(GrepTool::read_non_binary(&mut reader, 1).unwrap().is_none());
    let mut exact = std::io::repeat(b'x').take(10 * 1024 * 1024);
    use std::io::Read;
    assert_eq!(
        GrepTool::read_non_binary(&mut exact, 1)
            .unwrap()
            .unwrap()
            .len(),
        10 * 1024 * 1024
    );
}

#[tokio::test]
async fn grep_aggregate_read_budget_reports_incomplete_search_instead_of_no_match_claim() {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("grep-aggregate-byte-budget");
    let payload = root.path().join("payload.txt");
    std::fs::write(&payload, vec![b'x'; 10 * 1024 * 1024]).unwrap();
    // Distinct directory entries reuse the same inert content and keep fixture
    // disk use to 10 MiB; traversal still opens and reads each entry normally.
    for number in 0..10 {
        std::fs::hard_link(&payload, root.path().join(format!("alias-{number}.txt"))).unwrap();
    }
    let output = GrepTool::new(None, None, 5)
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"absent-marker","multiline":true}),
        ))
        .await
        .unwrap();
    assert!(output.starts_with("No matches found."), "{output}");
    assert!(output.contains("search work limit reached"), "{output}");
    assert!(output.contains("100 MiB read bytes"), "{output}");
}

#[tokio::test]
async fn grep_one_multiline_match_cannot_collect_unbounded_source_line_ranges() {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("grep-spanning-line-budget");
    std::fs::write(
        root.path().join("hits.txt"),
        format!("start\n{}end", "x\n".repeat(100_001)),
    )
    .unwrap();
    let output = GrepTool::new(None, None, 2)
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"start.*end", "multiline":true}),
        ))
        .await
        .unwrap();
    assert!(output.starts_with("2 results"), "{output}");
    assert!(output.contains("hits.txt:1:start"), "{output}");
    assert!(output.contains("hits.txt:2:x"), "{output}");
    assert!(output.contains("search work limit reached"), "{output}");
    assert!(output.contains("matches/touched lines"), "{output}");
}
