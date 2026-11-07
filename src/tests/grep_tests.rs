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
    let mut buf = Vec::new();
    let result = GrepTool::read_non_binary(&mut reader, &mut buf, 32 * 1024).unwrap();
    assert!(!result);
    assert_eq!(reader.emitted, 8192);
}

#[test]
fn text_reader_preserves_the_sniffed_prefix_and_remaining_bytes() {
    let expected = vec![b'x'; 10 * 1024];
    let mut reader = std::io::Cursor::new(expected.clone());
    let mut result = Vec::new();
    assert!(GrepTool::read_non_binary(&mut reader, &mut result, expected.len()).unwrap());
    assert_eq!(result, expected);
}

#[test]
fn growing_text_reader_is_bounded_by_the_search_file_limit() {
    let mut reader = std::io::repeat(b'x');
    let mut buf = Vec::new();
    assert!(!GrepTool::read_non_binary(&mut reader, &mut buf, 0).unwrap());
}

#[test]
fn shared_read_buffer_is_not_polluted_by_a_previous_file() {
    let mut buf = Vec::new();
    let mut binary = std::io::Cursor::new(b"abc\0def".to_vec());
    assert!(!GrepTool::read_non_binary(&mut binary, &mut buf, 8).unwrap());
    let mut text = std::io::Cursor::new(b"hello\n".to_vec());
    assert!(GrepTool::read_non_binary(&mut text, &mut buf, 6).unwrap());
    assert_eq!(buf, b"hello\n");
    let mut shorter = std::io::Cursor::new(b"hi".to_vec());
    assert!(GrepTool::read_non_binary(&mut shorter, &mut buf, 2).unwrap());
    assert_eq!(buf, b"hi");
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
    // mini-agent-5efke: the notice states the configured budgets.
    assert!(
        output.contains("(100000 directory entries, depth 256, 100 MiB read bytes ([tool_limits] search_max_entries/search_max_depth/search_max_bytes)"),
        "{output}"
    );
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
    let mut buf = Vec::new();
    assert!(!GrepTool::read_non_binary(&mut reader, &mut buf, 1).unwrap());
    let mut exact = std::io::repeat(b'x').take(10 * 1024 * 1024);
    use std::io::Read;
    assert!(GrepTool::read_non_binary(&mut exact, &mut buf, 1).unwrap());
    assert_eq!(buf.len(), 10 * 1024 * 1024);
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
    // The file at which the byte budget ran out is the first unsearched path.
    let stop = output
        .split("first unsearched path: ")
        .nth(1)
        .and_then(|rest| rest.split(" (search it").next())
        .expect("first unsearched path");
    assert!(
        stop.ends_with(".txt") && std::path::Path::new(stop).exists(),
        "{output}"
    );
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

/// mini-agent-o4iie: `offset` pages through results without overlap, and the
/// truncation notice names the exact offset that continues.
#[tokio::test]
async fn grep_offset_pages_lines_files_only_and_count_without_overlap() {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("grep-offset-pages");
    let body: String = (1..=400).map(|n| format!("needle {n}\n")).collect();
    std::fs::write(root.path().join("hits.txt"), body).unwrap();
    let tool = GrepTool::new(None, None, 150);
    assert_eq!(tool.parameters()["properties"]["offset"]["type"], "integer");

    let first = tool
        .call(search_args(root.path(), serde_json::json!({})))
        .await
        .unwrap();
    assert!(first.contains("continue with offset=150 "), "{first}");
    assert!(first.contains("hits.txt:150:needle 150\n"), "{first}");
    let second = tool
        .call(search_args(root.path(), serde_json::json!({"offset":150})))
        .await
        .unwrap();
    assert!(
        second.starts_with("150 results (151-300 after offset 150, "),
        "{second}"
    );
    assert!(second.contains("continue with offset=300 "), "{second}");
    assert!(!second.contains("hits.txt:150:"), "{second}");
    assert!(second.contains("hits.txt:151:needle 151\n"), "{second}");
    assert!(second.contains("hits.txt:300:needle 300\n"), "{second}");
    assert!(!second.contains("hits.txt:301:"), "{second}");
    let last = tool
        .call(search_args(root.path(), serde_json::json!({"offset":300})))
        .await
        .unwrap();
    assert!(
        last.starts_with("100 results (301-400 after offset 300, "),
        "{last}"
    );
    assert!(!last.contains("truncated"), "{last}");
    let past = tool
        .call(search_args(root.path(), serde_json::json!({"offset":400})))
        .await
        .unwrap();
    assert!(
        past.starts_with("No results at offset 400: this search produced 400 results"),
        "{past}"
    );

    // Context paging never starts a page with a dangling separator.
    let context = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"offset":2,"max_results":2,"context_lines":1}),
        ))
        .await
        .unwrap();
    assert!(context.contains("hits.txt:3: needle 3"), "{context}");
    assert!(!context.contains("\n--\n"), "{context}");

    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.path().join(name), "needle\n").unwrap();
    }
    for mode in ["files_only", "count"] {
        let mut page_one = serde_json::json!({"max_results":2});
        page_one[mode] = true.into();
        let page_one = tool.call(search_args(root.path(), page_one)).await.unwrap();
        assert!(
            page_one.contains("continue with offset=2 "),
            "{mode}: {page_one}"
        );
        let mut page_two = serde_json::json!({"max_results":2,"offset":2});
        page_two[mode] = true.into();
        let page_two = tool.call(search_args(root.path(), page_two)).await.unwrap();
        let files = |output: &str| -> Vec<String> {
            output
                .lines()
                .filter(|line| line.contains(".txt"))
                .map(|line| line.split(".txt").next().unwrap().to_owned())
                .collect()
        };
        let (one, two) = (files(&page_one), files(&page_two));
        assert_eq!(one.len(), 2, "{mode}: {page_one}");
        assert_eq!(two.len(), 2, "{mode}: {page_two}");
        assert!(
            one.iter().all(|file| !two.contains(file)),
            "{mode}: {one:?} {two:?}"
        );
    }
}

/// mini-agent-nzs2j: a source directory named `target` is searched; build
/// output (`target` beside Cargo.toml) and nested `node_modules` are skipped
/// and named in a notice.
#[tokio::test]
async fn search_skips_only_build_target_and_node_modules_and_names_them() {
    use crate::agent::tools::{FindFilesArgs, FindFilesTool};
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("skip-dirs");
    let write = |relative: &str| {
        let path = root.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "needle\n").unwrap();
    };
    write("src/target/lib.rs");
    write("crate/Cargo.toml");
    write("crate/target/debug/build.rs");
    write("web/pkg/package.json");
    write("web/pkg/node_modules/dep/index.rs");

    let grep = GrepTool::new(None, None, 50)
        .call(search_args(root.path(), serde_json::json!({})))
        .await
        .unwrap();
    assert!(grep.contains("src/target/lib.rs:1:needle"), "{grep}");
    assert!(!grep.contains("crate/target/debug"), "{grep}");
    assert!(!grep.contains("node_modules/dep"), "{grep}");
    assert!(
        grep.contains("[skipped dependency/build directories: "),
        "{grep}"
    );
    assert!(grep.contains("crate/target"), "{grep}");
    assert!(grep.contains("web/pkg/node_modules"), "{grep}");

    let args: FindFilesArgs = serde_json::from_value(serde_json::json!({
        "pattern": "*.rs",
        "path": root.path().to_string_lossy(),
    }))
    .unwrap();
    let found = FindFilesTool::new(None, None, 50).call(args).await.unwrap();
    assert!(found.contains("src/target/lib.rs"), "{found}");
    assert!(!found.contains("debug/build.rs"), "{found}");
    assert!(!found.contains("dep/index.rs"), "{found}");
    assert!(found.contains("crate/target"), "{found}");

    // Naming a skipped directory explicitly still searches it.
    let explicit = GrepTool::new(None, None, 50)
        .call(search_args(
            &root.path().join("crate/target"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert!(explicit.contains("debug/build.rs:1:needle"), "{explicit}");
}

fn utf16_bytes(text: &str, little: bool) -> Vec<u8> {
    let mut out = if little {
        vec![0xFF, 0xFE]
    } else {
        vec![0xFE, 0xFF]
    };
    for unit in text.encode_utf16() {
        out.extend_from_slice(&if little {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        });
    }
    out
}

#[test]
fn read_non_binary_decodes_bom_utf16_and_still_rejects_invalid_or_bomless_nul() {
    let mut buf = Vec::new();
    for little in [true, false] {
        let mut file = std::io::Cursor::new(utf16_bytes("alpha\nneedle é\n", little));
        assert!(GrepTool::read_non_binary(&mut file, &mut buf, 64).unwrap());
        assert_eq!(buf, "alpha\nneedle é\n".as_bytes());
    }
    // Unpaired surrogate after a BOM is not text.
    let mut bad = vec![0xFF, 0xFE, 0x00, 0xD8, 0x41, 0x00];
    bad.extend_from_slice(&[0, 0]);
    let mut file = std::io::Cursor::new(bad);
    assert!(!GrepTool::read_non_binary(&mut file, &mut buf, 64).unwrap());
    // NULs without a BOM remain binary.
    let mut file = std::io::Cursor::new(b"n\0e\0e\0d\0l\0e\0".to_vec());
    assert!(!GrepTool::read_non_binary(&mut file, &mut buf, 64).unwrap());
}

#[tokio::test]
async fn grep_finds_needle_in_bom_utf16_files_by_directory_and_explicit_path() {
    use rig::tool::Tool;
    let root = crate::tests::OwnedTestDirectory::new("grep-utf16");
    std::fs::write(
        root.path().join("app.rc"),
        utf16_bytes("first\nIDS_needle here\n", true),
    )
    .unwrap();
    std::fs::write(
        root.path().join("be.reg"),
        utf16_bytes("x\nneedle in be\n", false),
    )
    .unwrap();
    std::fs::write(root.path().join("blob.bin"), b"nee\0dle\0\0").unwrap();
    let tool = GrepTool::new(None, None, 20);
    let dir = tool
        .call(search_args(
            root.path(),
            serde_json::json!({"pattern":"needle"}),
        ))
        .await
        .unwrap();
    assert!(dir.contains("app.rc:2:IDS_needle here"), "{dir}");
    assert!(dir.contains("be.reg:2:needle in be"), "{dir}");
    assert!(dir.contains("1 binary"), "{dir}");
    let explicit = tool
        .call(search_args(
            &root.path().join("app.rc"),
            serde_json::json!({"pattern":"needle"}),
        ))
        .await
        .unwrap();
    assert!(explicit.contains("IDS_needle here"), "{explicit}");
    let blob = tool
        .call(search_args(
            &root.path().join("blob.bin"),
            serde_json::json!({"pattern":"needle"}),
        ))
        .await;
    assert!(blob.is_err());
}
