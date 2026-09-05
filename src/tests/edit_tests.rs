// These tests deliberately hold EDIT_SYSTEM_GUARD across .await points to keep
// the process-global edit system fixed for the whole async test (see the guard
// doc below), so the lint does not apply here.
#![allow(clippy::await_holding_lock)]

use crate::agent::tools::crc::crc32_hex;
use crate::agent::tools::set_edit_system;
use crate::agent::tools::{EditArgs, EditOp, edit};
use crate::config::types::EditSystem;
use rig::tool::Tool;

/// The edit system is a process-global, and `cargo test` runs tests in parallel,
/// so a `Similarity` test could otherwise have the global flipped to `Hashedit`
/// by a concurrent test mid-run. Serialize every test that touches it: lock this
/// shared mutex (held for the test's lifetime via the returned guard) and set
/// the system atomically.
static EDIT_SYSTEM_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialize_edit_system(es: EditSystem) -> std::sync::MutexGuard<'static, ()> {
    let guard = EDIT_SYSTEM_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    set_edit_system(es);
    guard
}

struct TempFile(String);

impl TempFile {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("zerostack_test_{}", name))
            .to_string_lossy()
            .to_string();
        TempFile(path)
    }

    fn path(&self) -> &str {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ── Similarity (V1) tests ──────────────────────────────────────────────

#[tokio::test]
async fn test_sim_rejects_no_blocks() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("noblocks.txt");
    std::fs::write(tmp.path(), "hello world\n").unwrap();
    let tool = edit::EditTool::new(None, None);
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: Some("no blocks here".into()),
            file_crc: None,
            edits: None,
        })
        .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("No SEARCH/REPLACE blocks found"));
}

#[tokio::test]
async fn test_sim_rejects_empty_search() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("emptysearch.txt");
    std::fs::write(tmp.path(), "hello world\n").unwrap();
    let tool = edit::EditTool::new(None, None);
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: Some("<<<<<<< SEARCH\n=======\nreplacement\n>>>>>>> REPLACE".into()),
            file_crc: None,
            edits: None,
        })
        .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("has empty search text"));
}

#[tokio::test]
async fn test_sim_search_not_found() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("notfound2.txt");
    std::fs::write(tmp.path(), "hello world\n").unwrap();
    let tool = edit::EditTool::new(None, None);
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: Some(
                "<<<<<<< SEARCH\nthis does not exist in file\n=======\nreplacement\n>>>>>>> REPLACE"
                    .into(),
            ),
            file_crc: None,
            edits: None,
        })
        .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("not found"));
}

#[tokio::test]
async fn test_sim_single_block_replacement() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("single2.txt");
    std::fs::write(tmp.path(), "before after done\n").unwrap();
    let tool = edit::EditTool::new(None, None);
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: Some("<<<<<<< SEARCH\nafter\n=======\nmiddle\n>>>>>>> REPLACE".into()),
            file_crc: None,
            edits: None,
        })
        .await
        .unwrap();
    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(content, "before middle done\n");
    assert!(result.contains("Applied 1 edit(s)"));
}

#[tokio::test]
async fn test_sim_multi_block_atomic() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("multiblock.txt");
    std::fs::write(tmp.path(), "aaa\nbbb\nccc\n").unwrap();
    let tool = edit::EditTool::new(None, None);
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: Some(
                "\
<<<<<<< SEARCH
aaa
=======
AAA
>>>>>>> REPLACE

<<<<<<< SEARCH
ccc
=======
CCC
>>>>>>> REPLACE"
                    .into(),
            ),
            file_crc: None,
            edits: None,
        })
        .await
        .unwrap();
    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(content, "AAA\nbbb\nCCC\n");
    assert!(result.contains("Applied 2 edit(s)"));
}

#[tokio::test]
async fn test_sim_multi_match_returns_error() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("multi2.txt");
    std::fs::write(tmp.path(), "hello world, hello there\n").unwrap();
    let tool = edit::EditTool::new(None, None);
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: Some("<<<<<<< SEARCH\nhello\n=======\nbye\n>>>>>>> REPLACE".into()),
            file_crc: None,
            edits: None,
        })
        .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("matched 2 times"));
}

#[tokio::test]
async fn test_sim_preserves_crlf_line_endings() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("crlf2.txt");
    std::fs::write(tmp.path(), "line1\r\nline2\r\nline3\r\n").unwrap();
    let tool = edit::EditTool::new(None, None);
    tool.call(EditArgs {
        path: tmp.path().into(),
        block: Some("<<<<<<< SEARCH\nline2\n=======\nmodified\n>>>>>>> REPLACE".into()),
        file_crc: None,
        edits: None,
    })
    .await
    .unwrap();
    let raw = std::fs::read(tmp.path()).unwrap();
    assert!(
        raw.windows(2).any(|w| w == b"\r\n"),
        "CRLF should be preserved"
    );
}

#[tokio::test]
async fn test_sim_crlf_multiline_search_is_exact_and_replacement_uses_crlf() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_crlf_multiline.txt");
    std::fs::write(tmp.path(), b"alpha\r\nbeta\r\ngamma\r\n").unwrap();

    let result = sim_edit(&tmp, "alpha\nbeta", "first\nsecond")
        .await
        .expect("LF-authored block should match CRLF file exactly after adaptation");

    assert!(!result.contains("whitespace normalization"), "{result}");
    assert_eq!(
        std::fs::read(tmp.path()).unwrap(),
        b"first\r\nsecond\r\ngamma\r\n"
    );
}

// ── Hashedit (V2) tests ─────────────────────────────────────────────────

fn make_tagged_line(line_num: usize, content: &str) -> String {
    let tag = crc32_hex(content.as_bytes());
    format!("   {}|{} {}", line_num, tag, content)
}

#[tokio::test]
async fn test_hash_single_line_edit() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_single.txt");
    let original = "use std::io;\nuse std::fs;\n\nfn main() {\n    println!(\"hi\");\n}\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let tagged = make_tagged_line(4, "fn main() {");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![EditOp {
                line: Some(tagged),
                lines: None,
                text: "fn run() {".into(),
            }]),
        })
        .await
        .unwrap();

    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert!(
        content.contains("fn run() {"),
        "expected 'fn run() {{', got: {content}"
    );
    assert!(!content.contains("fn main() {"));
    assert!(result.contains("Applied 1 edit(s)"));
}

#[tokio::test]
async fn test_hash_range_edit() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_range.txt");
    let original = "line1\nline2\nline3\nline4\nline5\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let l2 = make_tagged_line(2, "line2");
    let l3 = make_tagged_line(3, "line3");
    let l4 = make_tagged_line(4, "line4");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![EditOp {
                line: None,
                lines: Some(format!("{}\n{}\n{}", l2, l3, l4)),
                text: "CHANGED_A\nCHANGED_B".into(),
            }]),
        })
        .await
        .unwrap();

    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(content, "line1\nCHANGED_A\nCHANGED_B\nline5\n");
    assert!(result.contains("Applied 1 edit(s)"));
}

#[tokio::test]
async fn test_hash_delete_via_empty_text() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_delete.txt");
    let original = "keep me\nremove me\nkeep me too\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let tagged = make_tagged_line(2, "remove me");
    tool.call(EditArgs {
        path: tmp.path().into(),
        block: None,
        file_crc: Some(file_crc),
        edits: Some(vec![EditOp {
            line: Some(tagged),
            lines: None,
            text: String::new(),
        }]),
    })
    .await
    .unwrap();

    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(content, "keep me\nkeep me too\n");
}

#[tokio::test]
async fn test_hash_file_crc_mismatch() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_badcrc.txt");
    std::fs::write(tmp.path(), "hello world\n").unwrap();

    let tool = edit::EditTool::new(None, None);
    let tagged = make_tagged_line(1, "hello world");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some("deadbeef".into()),
            edits: Some(vec![EditOp {
                line: Some(tagged),
                lines: None,
                text: "bye".into(),
            }]),
        })
        .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("CRC mismatch"));
}

#[tokio::test]
async fn test_hash_tag_mismatch() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_badtag.txt");
    let original = "hello world\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    // Tag is for "different content" not for "hello world"
    let bad_tag = crc32_hex(b"different content");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![EditOp {
                line: Some(format!("   1|{} hello world", bad_tag)),
                lines: None,
                text: "bye".into(),
            }]),
        })
        .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("Tag mismatch"));
}

#[tokio::test]
async fn test_hash_invalid_tag_format() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_badfmt.txt");
    let original = "hello world\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![EditOp {
                line: Some("not a valid tagged line".into()),
                lines: None,
                text: "bye".into(),
            }]),
        })
        .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("invalid tagged line"));
}

#[tokio::test]
async fn test_hash_crlf_preserved() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_crlf.txt");
    let original = "line1\r\nline2\r\nline3\r\n";
    std::fs::write(tmp.path(), original).unwrap();
    // CRC must be computed on LF-normalized content, same as edit tool normalizes
    let normalized = original.replace("\r\n", "\n");
    let file_crc = crc32_hex(normalized.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let tagged = make_tagged_line(2, "line2");
    tool.call(EditArgs {
        path: tmp.path().into(),
        block: None,
        file_crc: Some(file_crc),
        edits: Some(vec![EditOp {
            line: Some(tagged),
            lines: None,
            text: "modified".into(),
        }]),
    })
    .await
    .unwrap();

    let raw = std::fs::read(tmp.path()).unwrap();
    assert!(
        raw.windows(2).any(|w| w == b"\r\n"),
        "CRLF should be preserved"
    );
}

#[tokio::test]
async fn test_hash_multi_edit_atomic() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_multi.txt");
    let original = "aaa\nbbb\nccc\nddd\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let l1 = make_tagged_line(1, "aaa");
    let l4 = make_tagged_line(4, "ddd");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![
                EditOp {
                    line: Some(l1),
                    lines: None,
                    text: "AAA".into(),
                },
                EditOp {
                    line: Some(l4),
                    lines: None,
                    text: "DDD".into(),
                },
            ]),
        })
        .await
        .unwrap();

    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert_eq!(content, "AAA\nbbb\nccc\nDDD\n");
    assert!(result.contains("Applied 2 edit(s)"));
}

// ── Similarity: whitespace-normalized match must map to exact bytes ─────

async fn sim_edit(tmp: &TempFile, search: &str, replace: &str) -> Result<String, String> {
    let tool = edit::EditTool::new(None, None);
    tool.call(EditArgs {
        path: tmp.path().into(),
        block: Some(format!(
            "<<<<<<< SEARCH\n{search}\n=======\n{replace}\n>>>>>>> REPLACE"
        )),
        file_crc: None,
        edits: None,
    })
    .await
    .map_err(|e| e.to_string())
}

#[tokio::test]
async fn test_sim_normalized_match_after_trailing_whitespace() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_norm_trailing.txt");
    std::fs::write(tmp.path(), "foo   \n    bar\n").unwrap();
    let result = sim_edit(&tmp, "\tbar", "    baz").await.unwrap();
    assert!(result.contains("whitespace normalization"), "{result}");
    assert!(result.contains("replaced region:\n    bar"), "{result}");
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        "foo   \n    baz\n"
    );
}

#[tokio::test]
async fn test_sim_normalized_match_must_be_unique() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_norm_ambiguous.txt");
    let original = "fn a() {\n\tvalue = 1;\n}\nfn b() {\n\tvalue = 1;\n}\n";
    std::fs::write(tmp.path(), original).unwrap();

    let message = sim_edit(&tmp, "    value = 1;", "    value = 2;")
        .await
        .expect_err("ambiguous normalized matches must be rejected");

    assert!(message.contains("matched more than once"), "{message}");
    assert!(message.contains("lines 2 and 5"), "{message}");
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);
}

#[tokio::test]
async fn test_sim_fuzzy_match_must_be_unique() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_fuzzy_ambiguous.txt");
    let original = "fn a() {\n    value = 100;\n}\nfn b() {\n    value = 100;\n}\n";
    std::fs::write(tmp.path(), original).unwrap();

    let message = sim_edit(&tmp, "    value = 101;", "    value = 200;")
        .await
        .expect_err("ambiguous fuzzy matches must be rejected");

    assert!(message.contains("multiple fuzzy matches"), "{message}");
    assert!(message.contains("line 2"), "{message}");
    assert!(message.contains("line 5"), "{message}");
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);
}

#[tokio::test]
async fn test_sim_single_fuzzy_match_echoes_replaced_region() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_fuzzy_echo.txt");
    std::fs::write(tmp.path(), "keep\n    value = 100;\nend\n").unwrap();

    let result = sim_edit(&tmp, "    value = 101;", "    value = 200;")
        .await
        .expect("one fuzzy candidate should apply");

    assert!(result.contains("fuzzy match"), "{result}");
    assert!(
        result.contains("replaced region:\n    value = 100;"),
        "{result}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        "keep\n    value = 200;\nend\n"
    );
}

#[tokio::test]
async fn test_sim_normalized_match_after_collapsed_blank_lines() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_norm_blank.txt");
    std::fs::write(tmp.path(), "foo\n\n\n\n    bar\nbaz\n").unwrap();
    let result = sim_edit(&tmp, "\tbar", "    qux").await.unwrap();
    assert!(result.contains("whitespace normalization"), "{result}");
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        "foo\n\n\n\n    qux\nbaz\n"
    );
}

#[tokio::test]
async fn test_sim_normalized_match_tabs_vs_spaces_inside_match() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_norm_tabs.txt");
    std::fs::write(tmp.path(), "fn a() {\n\tx = 1;\n\ty = 2;\n}\n").unwrap();
    let result = sim_edit(&tmp, "    x = 1;\n    y = 2;", "    z = 3;")
        .await
        .unwrap();
    assert!(result.contains("whitespace normalization"), "{result}");
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        "fn a() {\n    z = 3;\n}\n"
    );
}

#[tokio::test]
async fn test_sim_normalized_match_in_crlf_file() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_norm_crlf.txt");
    std::fs::write(tmp.path(), "foo   \r\n    bar\r\nend\r\n").unwrap();
    let result = sim_edit(&tmp, "\tbar", "    baz").await.unwrap();
    assert!(result.contains("whitespace normalization"), "{result}");
    assert_eq!(
        std::fs::read(tmp.path()).unwrap(),
        b"foo   \r\n    baz\r\nend\r\n"
    );
}

#[tokio::test]
async fn test_sim_normalized_match_at_end_of_file() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);

    let tmp = TempFile::new("sim_norm_eof_nonl.txt");
    std::fs::write(tmp.path(), "keep   \n\tlast").unwrap();
    sim_edit(&tmp, "    last", "LAST").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        "keep   \nLAST"
    );

    let tmp = TempFile::new("sim_norm_eof_nl.txt");
    std::fs::write(tmp.path(), "keep   \n\tlast\n").unwrap();
    sim_edit(&tmp, "    last", "LAST").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        "keep   \nLAST\n"
    );
}

#[tokio::test]
async fn test_sim_normalized_match_replaces_exactly_the_matched_lines() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("sim_norm_exact_lines.txt");
    std::fs::write(tmp.path(), "a\nb   \n\tc\nd\n").unwrap();
    sim_edit(&tmp, "b\n    c", "X").await.unwrap();
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), "a\nX\nd\n");
}

// ── Hashedit: line-range validation ─────────────────────────────────────

#[tokio::test]
async fn test_hash_range_rejects_descending_lines() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_desc.txt");
    let original = "line1\nline2\nline3\nline4\nline5\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let l4 = make_tagged_line(4, "line4");
    let l2 = make_tagged_line(2, "line2");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![EditOp {
                line: None,
                lines: Some(format!("{}\n{}", l4, l2)),
                text: "CHANGED".into(),
            }]),
        })
        .await;
    let msg = result
        .expect_err("descending range must be rejected")
        .to_string();
    assert!(msg.contains("ascending"), "unexpected error: {msg}");
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);
}

#[tokio::test]
async fn test_hash_range_rejects_non_contiguous_lines() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_gap.txt");
    let original = "line1\nline2\nline3\nline4\nline5\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let l2 = make_tagged_line(2, "line2");
    let l4 = make_tagged_line(4, "line4");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![EditOp {
                line: None,
                lines: Some(format!("{}\n{}", l2, l4)),
                text: "CHANGED".into(),
            }]),
        })
        .await;
    let msg = result
        .expect_err("non-contiguous range must be rejected")
        .to_string();
    assert!(msg.contains("contiguous"), "unexpected error: {msg}");
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);
}

#[tokio::test]
async fn test_hash_rejects_line_zero() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_zero.txt");
    let original = "line1\nline2\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    // Tag is valid for line 1 but the line number is 0.
    let l0 = make_tagged_line(0, "line1");
    for op in [
        EditOp {
            line: Some(l0.clone()),
            lines: None,
            text: "INSERTED".into(),
        },
        EditOp {
            line: None,
            lines: Some(l0.clone()),
            text: "INSERTED".into(),
        },
    ] {
        let result = tool
            .call(EditArgs {
                path: tmp.path().into(),
                block: None,
                file_crc: Some(file_crc.clone()),
                edits: Some(vec![op]),
            })
            .await;
        let msg = result.expect_err("line 0 must be rejected").to_string();
        assert!(msg.contains("Line 0"), "unexpected error: {msg}");
    }
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);
}

#[tokio::test]
async fn test_hash_rejects_overlapping_edits() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_overlap.txt");
    let original = "line1\nline2\nline3\nline4\nline5\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let l2 = make_tagged_line(2, "line2");
    let l3 = make_tagged_line(3, "line3");
    let l4 = make_tagged_line(4, "line4");
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc.clone()),
            edits: Some(vec![
                EditOp {
                    line: None,
                    lines: Some(format!("{}\n{}", l2, l3)),
                    text: "A".into(),
                },
                EditOp {
                    line: None,
                    lines: Some(format!("{}\n{}", l3.clone(), l4)),
                    text: "B".into(),
                },
            ]),
        })
        .await;
    let msg = result
        .expect_err("overlapping edits must be rejected")
        .to_string();
    assert!(msg.contains("overlap"), "unexpected error: {msg}");
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);

    // Two edits targeting the same single line overlap as well.
    let result = tool
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![
                EditOp {
                    line: Some(l3.clone()),
                    lines: None,
                    text: "A".into(),
                },
                EditOp {
                    line: Some(l3),
                    lines: None,
                    text: "B".into(),
                },
            ]),
        })
        .await;
    let msg = result
        .expect_err("duplicate edits must be rejected")
        .to_string();
    assert!(msg.contains("overlap"), "unexpected error: {msg}");
    assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), original);
}

#[tokio::test]
async fn test_hash_adjacent_edits_are_allowed() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_adjacent.txt");
    let original = "line1\nline2\nline3\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.as_bytes());

    let tool = edit::EditTool::new(None, None);
    let l1 = make_tagged_line(1, "line1");
    let l2 = make_tagged_line(2, "line2");
    tool.call(EditArgs {
        path: tmp.path().into(),
        block: None,
        file_crc: Some(file_crc),
        edits: Some(vec![
            EditOp {
                line: Some(l1),
                lines: None,
                text: "A".into(),
            },
            EditOp {
                line: Some(l2),
                lines: None,
                text: "B".into(),
            },
        ]),
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path()).unwrap(),
        "A\nB\nline3\n"
    );
}

#[tokio::test]
async fn test_hash_multiline_replacement_uses_dominant_crlf() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_crlf_multiline.txt");
    let original = "line1\r\nline2\r\nline3\r\n";
    std::fs::write(tmp.path(), original).unwrap();
    let file_crc = crc32_hex(original.replace("\r\n", "\n").as_bytes());
    let tool = edit::EditTool::new(None, None);

    tool.call(EditArgs {
        path: tmp.path().into(),
        block: None,
        file_crc: Some(file_crc),
        edits: Some(vec![EditOp {
            line: Some(make_tagged_line(2, "line2")),
            lines: None,
            text: "second-a\nsecond-b".into(),
        }]),
    })
    .await
    .unwrap();

    assert_eq!(
        std::fs::read(tmp.path()).unwrap(),
        b"line1\r\nsecond-a\r\nsecond-b\r\nline3\r\n"
    );
}

#[tokio::test]
async fn test_similarity_edit_preserves_mixed_line_endings() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("mixed_endings.txt");
    std::fs::write(tmp.path(), b"one\r\ntwo\nthree\r\n").unwrap();

    sim_edit(&tmp, "two", "changed").await.unwrap();

    assert_eq!(
        std::fs::read(tmp.path()).unwrap(),
        b"one\r\nchanged\nthree\r\n"
    );
}

#[tokio::test]
async fn test_hash_empty_replacement_deletes_line_without_requiring_tag_space() {
    let _edit_guard = serialize_edit_system(EditSystem::Hashedit);
    let tmp = TempFile::new("hash_delete_line.txt");
    let original = "one\r\ntwo\nthree\r\n";
    std::fs::write(tmp.path(), original).unwrap();
    let normalized = original.replace("\r\n", "\n");
    let file_crc = crc32_hex(normalized.as_bytes());
    let tag_without_trailing_space = format!("2|{}", crc32_hex(b"two"));

    edit::EditTool::new(None, None)
        .call(EditArgs {
            path: tmp.path().into(),
            block: None,
            file_crc: Some(file_crc),
            edits: Some(vec![EditOp {
                line: Some(tag_without_trailing_space),
                lines: None,
                text: String::new(),
            }]),
        })
        .await
        .unwrap();

    assert_eq!(std::fs::read(tmp.path()).unwrap(), b"one\r\nthree\r\n");
}

#[tokio::test]
async fn test_similarity_fuzzy_fallback_has_a_hard_work_bound() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("bounded_fuzzy.txt");
    let content = (0..20_000)
        .map(|index| format!("content line {index:05} with enough padding to be expensive"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(tmp.path(), content).unwrap();
    let search = (0..40)
        .map(|index| format!("missing line {index:05} with enough padding to be expensive"))
        .collect::<Vec<_>>()
        .join("\n");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sim_edit(&tmp, &search, "replacement"),
    )
    .await
    .expect("fuzzy fallback exceeded its work budget");
    assert!(result.unwrap_err().contains("not found"));
}

// ── Non-UTF-8 files must fail closed ────────────────────────────────────

#[tokio::test]
async fn test_edit_rejects_non_utf8_file() {
    let _edit_guard = serialize_edit_system(EditSystem::Similarity);
    let tmp = TempFile::new("latin1.txt");
    // Latin-1 "café" followed by an ASCII line the edit targets.
    let original: &[u8] = b"caf\xe9 au lait\nline2\n";
    std::fs::write(tmp.path(), original).unwrap();

    let result = sim_edit(&tmp, "line2", "modified").await;
    let msg = result.expect_err("non-UTF-8 file must be rejected");
    assert!(msg.contains("UTF-8"), "unexpected error: {msg}");
    assert_eq!(
        std::fs::read(tmp.path()).unwrap(),
        original,
        "file must be left untouched"
    );
}
