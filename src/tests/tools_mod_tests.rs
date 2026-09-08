use rig::tool::Tool;

use crate::agent::tools::{
    EditArgs, EditTool, FindFilesArgs, FindFilesTool, GrepArgs, GrepTool, ListDirArgs, ListDirTool,
    ReadArgs, ReadTool, ReadTracker, is_skip_dir,
};
use crate::session::Session;

#[test]
fn skip_node_modules() {
    assert!(is_skip_dir("node_modules"));
}

#[test]
fn skip_target() {
    assert!(is_skip_dir("target"));
}

#[test]
fn skip_common_vcs_metadata_directories() {
    for name in [".git", ".hg", ".svn", ".bzr"] {
        assert!(is_skip_dir(name), "{name} should be skipped");
    }
}

#[test]
fn skip_case_sensitive() {
    assert!(!is_skip_dir("Node_Modules"));
    assert!(!is_skip_dir("TARGET"));
}

#[test]
fn skip_other_dirs() {
    assert!(!is_skip_dir("src"));
    assert!(!is_skip_dir(""));
    assert!(!is_skip_dir("node_modules_extra"));
}

#[tokio::test]
async fn workspace_tools_skip_vcs_metadata_unless_it_is_the_explicit_root() {
    let root = std::env::temp_dir().join(format!(
        "mini-agent-skip-vcs-metadata-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".git/secret.txt"), "VCS_SENTINEL\n").unwrap();
    std::fs::write(root.join(".svn"), "VCS_FILE_SENTINEL\n").unwrap();
    std::fs::write(root.join("src/visible.txt"), "VISIBLE_SENTINEL\n").unwrap();

    {
        let listing = ListDirTool::new(None, None, None)
            .with_workspace(&root)
            .call(ListDirArgs { path: None })
            .await
            .unwrap();
        assert!(listing.contains("src"), "{listing}");
        assert!(!listing.contains(".git"), "{listing}");
        assert!(!listing.contains(".svn"), "{listing}");

        let grep = GrepTool::new(None, None, 100)
            .with_workspace(&root)
            .call(GrepArgs {
                pattern: "SENTINEL".into(),
                path: None,
                include: None,
                context_lines: None,
                case_insensitive: false,
                files_only: false,
                count: false,
            })
            .await
            .unwrap();
        assert!(grep.contains("visible.txt"), "{grep}");
        assert!(!grep.contains("secret.txt"), "{grep}");
        assert!(!grep.contains(".svn"), "{grep}");

        let found = FindFilesTool::new(None, None, 100)
            .with_workspace(&root)
            .call(FindFilesArgs {
                pattern: r".*\.txt".into(),
                path: None,
            })
            .await
            .unwrap();
        assert!(found.contains("visible.txt"), "{found}");
        assert!(!found.contains("secret.txt"), "{found}");

        let explicit_listing = ListDirTool::new(None, None, None)
            .with_workspace(&root)
            .call(ListDirArgs {
                path: Some(".git".into()),
            })
            .await
            .unwrap();
        assert!(
            explicit_listing.contains("secret.txt"),
            "{explicit_listing}"
        );

        let explicit_grep = GrepTool::new(None, None, 100)
            .with_workspace(&root)
            .call(GrepArgs {
                pattern: "VCS_SENTINEL".into(),
                path: Some(".git".into()),
                include: None,
                context_lines: None,
                case_insensitive: false,
                files_only: false,
                count: false,
            })
            .await
            .unwrap();
        assert!(explicit_grep.contains("secret.txt"), "{explicit_grep}");
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn permission_coaching_combines_pattern_and_path_messages() {
    assert_eq!(
        crate::agent::tools::combine_coaching(Some("pattern".into()), Some("path".into())),
        Some("pattern\n\npath".into())
    );
    assert_eq!(
        crate::agent::tools::combine_coaching(Some("same".into()), Some("same".into())),
        Some("same".into())
    );
}

#[test]
fn track_read_returns_none_when_deny_disabled() {
    let tracker = ReadTracker::new(false);
    let result = tracker.track_read("test_path", 0, 10);
    assert!(result.is_none());
}

#[test]
fn track_read_first_call_returns_none() {
    let tracker = ReadTracker::new(true);
    let result = tracker.track_read("test_path", 1, 100);
    assert!(result.is_none());
}

#[test]
fn track_read_duplicate_returns_blocking_message() {
    let tracker = ReadTracker::new(true);

    // First call
    let first = tracker.track_read("dup_path", 5, 50);
    assert!(first.is_none());

    // Second identical call
    let second = tracker.track_read("dup_path", 5, 50);
    assert!(second.is_some());
    let msg = second.unwrap();
    assert!(msg.contains("already read"));
    assert!(msg.contains("dup_path"));
}

#[test]
fn track_read_different_offset_not_duplicate() {
    let tracker = ReadTracker::new(true);

    let first = tracker.track_read("diff_path", 0, 100);
    assert!(first.is_none());

    let second = tracker.track_read("diff_path", 10, 100);
    assert!(second.is_none());
}

#[test]
fn track_read_different_limit_not_duplicate() {
    let tracker = ReadTracker::new(true);

    let first = tracker.track_read("diff_path2", 0, 100);
    assert!(first.is_none());

    let second = tracker.track_read("diff_path2", 0, 200);
    assert!(second.is_none());
}

#[test]
fn untrack_removes_matching_path() {
    let tracker = ReadTracker::new(true);

    tracker.track_read("remove_me", 0, 10);
    tracker.untrack_read_path("remove_me");

    // After untracking, first call should be fine again
    let result = tracker.track_read("remove_me", 0, 10);
    assert!(result.is_none());
}

#[test]
fn untrack_does_not_affect_other_paths() {
    let tracker = ReadTracker::new(true);

    tracker.track_read("keep_me", 0, 10);
    tracker.track_read("unrelated", 0, 10);

    tracker.untrack_read_path("unrelated");

    // keep_me should still be tracked
    let result = tracker.track_read("keep_me", 0, 10);
    assert!(result.is_some());
}

#[test]
fn separate_trackers_keep_settings_and_ranges_independent() {
    let denying = ReadTracker::new(true);
    let allowing = ReadTracker::new(false);

    assert!(denying.track_read("same_path", 0, 10).is_none());
    assert!(allowing.track_read("same_path", 0, 10).is_none());
    assert!(denying.track_read("same_path", 0, 10).is_some());
    assert!(allowing.track_read("same_path", 0, 10).is_none());
}

#[tokio::test]
async fn concurrent_read_tools_with_different_settings_do_not_share_history() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-concurrent-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, "same contents").await.unwrap();
    let path = path.to_string_lossy().into_owned();
    let denying = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(true));
    let allowing = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(false));
    let args = || ReadArgs {
        path: path.clone(),
        offset: None,
        limit: None,
    };

    let (first_denying, first_allowing) = tokio::join!(denying.call(args()), allowing.call(args()));
    assert!(first_denying.is_ok());
    assert!(first_allowing.is_ok());
    let (second_denying, second_allowing) =
        tokio::join!(denying.call(args()), allowing.call(args()));
    assert!(
        second_denying
            .unwrap_err()
            .to_string()
            .contains("already read")
    );
    assert!(second_allowing.is_ok());

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn repeated_read_is_allowed_after_same_length_mtime_preserving_change() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-external-change-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, "before").await.unwrap();
    let tool = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(true));
    let args = || ReadArgs {
        path: path.to_string_lossy().into_owned(),
        offset: None,
        limit: None,
    };

    assert!(tool.call(args()).await.is_ok());
    assert!(tool.call(args()).await.is_err());
    let original_metadata = std::fs::metadata(&path).unwrap();
    let original_modified = original_metadata.modified().unwrap();
    tokio::fs::write(&path, "after!").await.unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    let changed_metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(changed_metadata.len(), original_metadata.len());
    assert_eq!(changed_metadata.modified().unwrap(), original_modified);
    assert!(tool.call(args()).await.is_ok());

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn failed_read_is_not_recorded_as_successful() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-failed-read-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, "too large").await.unwrap();
    let tool = ReadTool::new_with_tracker(None, None, Some(1), 100, ReadTracker::new(true));
    let args = || ReadArgs {
        path: path.to_string_lossy().into_owned(),
        offset: None,
        limit: None,
    };

    for _ in 0..2 {
        let error = tool.call(args()).await.unwrap_err().to_string();
        assert!(
            error.contains("File too large"),
            "unexpected error: {error}"
        );
        assert!(!error.contains("already read"));
    }

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn rebuilt_read_tool_keeps_its_logical_session_history() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-rebuild-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, "contents").await.unwrap();
    let path = path.to_string_lossy().into_owned();
    let session_tracker = ReadTracker::new(true);
    let first_build = ReadTool::new_with_tracker(None, None, None, 100, session_tracker.clone());
    let args = || ReadArgs {
        path: path.clone(),
        offset: None,
        limit: None,
    };
    assert!(first_build.call(args()).await.is_ok());
    drop(first_build);

    let rebuilt = ReadTool::new_with_tracker(None, None, None, 100, session_tracker);
    assert!(
        rebuilt
            .call(args())
            .await
            .unwrap_err()
            .to_string()
            .contains("already read")
    );
    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn loading_another_session_starts_fresh_read_history() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-session-load-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, "contents").await.unwrap();
    let path = path.to_string_lossy().into_owned();
    let args = || ReadArgs {
        path: path.clone(),
        offset: None,
        limit: None,
    };

    let mut session_a = Session::new("test", "test", 1_000, "A");
    session_a.initialize_read_tracker(true);
    let agent_a = ReadTool::new_with_tracker(None, None, None, 100, session_a.read_tracker.clone());
    assert!(agent_a.call(args()).await.is_ok());
    assert!(agent_a.call(args()).await.is_err());

    let mut loaded_session_b = Session::new("test", "test", 1_000, "B");
    loaded_session_b.initialize_read_tracker(true);
    let rebuilt_agent =
        ReadTool::new_with_tracker(None, None, None, 100, loaded_session_b.read_tracker.clone());
    assert!(rebuilt_agent.call(args()).await.is_ok());

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn imported_session_uses_active_allow_repeated_reads_setting() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-session-import-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, "contents").await.unwrap();
    let path = path.to_string_lossy().into_owned();
    let args = || ReadArgs {
        path: path.clone(),
        offset: None,
        limit: None,
    };

    let serialized =
        serde_json::to_string(&Session::new("test", "test", 1_000, "imported")).unwrap();
    let mut imported: Session = serde_json::from_str(&serialized).unwrap();
    imported.initialize_read_tracker(false);
    let rebuilt_agent =
        ReadTool::new_with_tracker(None, None, None, 100, imported.read_tracker.clone());
    assert!(rebuilt_agent.call(args()).await.is_ok());
    assert!(rebuilt_agent.call(args()).await.is_ok());

    let _ = tokio::fs::remove_file(path).await;
}

#[tokio::test]
async fn canonical_path_key_blocks_dot_alias_of_same_file() {
    let directory = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-alias-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let canonical_spelling = directory.join("file.txt");
    tokio::fs::write(&canonical_spelling, "contents")
        .await
        .unwrap();
    let dot_spelling = directory.join(".").join("file.txt");
    let tracker = ReadTracker::new(true);
    let tool = ReadTool::new_with_tracker(None, None, None, 100, tracker);

    assert!(
        tool.call(ReadArgs {
            path: dot_spelling.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
        })
        .await
        .is_ok()
    );
    assert!(
        tool.call(ReadArgs {
            path: canonical_spelling.to_string_lossy().into_owned(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("already read")
    );
    let _ = tokio::fs::remove_dir_all(directory).await;
}

#[tokio::test]
async fn edit_file_version_change_invalidates_every_session_tracker() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-write-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&path, "before").await.unwrap();
    let path = path.to_string_lossy().into_owned();
    let owner_tracker = ReadTracker::new(true);
    let other_tracker = ReadTracker::new(true);
    let owner_read = ReadTool::new_with_tracker(None, None, None, 100, owner_tracker.clone());
    let other_read = ReadTool::new_with_tracker(None, None, None, 100, other_tracker);
    let owner_edit = EditTool::new_with_tracker(None, None, None, owner_tracker);
    let read_args = || ReadArgs {
        path: path.clone(),
        offset: None,
        limit: None,
    };

    assert!(owner_read.call(read_args()).await.is_ok());
    assert!(other_read.call(read_args()).await.is_ok());
    owner_edit
        .call(EditArgs {
            path: path.clone(),
            replace_all: false,
            block: Some("<<<<<<< SEARCH\nbefore\n=======\nafter\n>>>>>>> REPLACE".to_string()),
            file_crc: None,
            edits: None,
        })
        .await
        .unwrap();

    assert!(owner_read.call(read_args()).await.is_ok());
    assert!(other_read.call(read_args()).await.is_ok());

    let _ = tokio::fs::remove_file(path).await;
}

#[cfg(unix)]
#[tokio::test]
async fn edit_of_canonical_target_invalidates_read_through_symlink_alias() {
    use std::os::unix::fs::symlink;

    let directory = std::env::temp_dir().join(format!(
        "mini-agent-read-tracker-symlink-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let target = directory.join("target.txt");
    let alias = directory.join("alias.txt");
    tokio::fs::write(&target, "before").await.unwrap();
    symlink(&target, &alias).unwrap();
    let tracker = ReadTracker::new(true);
    let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone());
    let edit = EditTool::new_with_tracker(None, None, None, tracker);
    let alias_args = || ReadArgs {
        path: alias.to_string_lossy().into_owned(),
        offset: None,
        limit: None,
    };

    assert!(read.call(alias_args()).await.is_ok());
    edit.call(EditArgs {
        path: target.to_string_lossy().into_owned(),
        replace_all: false,
        block: Some("<<<<<<< SEARCH\nbefore\n=======\nafter\n>>>>>>> REPLACE".to_string()),
        file_crc: None,
        edits: None,
    })
    .await
    .unwrap();
    assert!(read.call(alias_args()).await.is_ok());

    let _ = tokio::fs::remove_dir_all(directory).await;
}

// ── Bounded line reading ───────────────────────────────────────────────

fn read_temp_root(label: &str) -> std::path::PathBuf {
    let root =
        std::env::temp_dir().join(format!("mini-agent-read-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::canonicalize(&root).unwrap()
}

fn bounded_read_tool(root: &std::path::Path, cap: u64) -> ReadTool {
    ReadTool::new_with_tracker(None, None, Some(cap), 100, ReadTracker::new(false))
        .with_workspace(root.to_path_buf())
}

fn read_args(path: &str, offset: Option<usize>, limit: Option<usize>) -> ReadArgs {
    ReadArgs {
        path: path.into(),
        offset,
        limit,
    }
}

#[tokio::test]
async fn a_single_line_far_larger_than_the_cap_is_rejected_not_buffered() {
    let root = read_temp_root("huge-line");
    let cap = 64 * 1024;
    // One line two orders of magnitude past the output cap.
    std::fs::write(
        root.join("huge.txt"),
        format!("{}\nsecond\n", "x".repeat(8 * 1024 * 1024)),
    )
    .unwrap();

    let tool = bounded_read_tool(&root, cap);
    let error = tool
        .call(read_args("huge.txt", Some(1), Some(1)))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("read output cap"),
        "the byte budget must stop the line, not the excerpt check afterwards: {error}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_line_past_the_requested_window_is_skipped_without_being_retained() {
    let root = read_temp_root("skip-huge-line");
    let cap = 64 * 1024;
    std::fs::write(
        root.join("huge.txt"),
        format!("{}\nsecond line\n", "x".repeat(8 * 1024 * 1024)),
    )
    .unwrap();

    let tool = bounded_read_tool(&root, cap);
    let output = tool
        .call(read_args("huge.txt", Some(2), Some(1)))
        .await
        .unwrap();
    assert!(output.contains("second line"), "{output}");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn crlf_terminators_survive_chunked_reading() {
    let root = read_temp_root("crlf-chunks");
    // Long enough that lines and their terminators straddle read chunks.
    let long = "a".repeat(20_000);
    std::fs::write(
        root.join("crlf.txt"),
        format!("{long}\r\nsecond\r\ntrailing\r"),
    )
    .unwrap();

    let tool = bounded_read_tool(&root, 1024 * 1024);
    let output = tool.call(read_args("crlf.txt", None, None)).await.unwrap();
    assert!(output.contains("3 lines total"), "{output}");
    assert!(output.contains("second"), "{output}");
    // A carriage return without a following newline is ordinary content.
    assert!(output.contains("trailing\r"), "{output:?}");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn multibyte_characters_split_across_chunks_stay_valid() {
    let root = read_temp_root("utf8-chunks");
    // Each character is three bytes, so sequences straddle the reader's chunks.
    let line = "\u{4f60}\u{597d}".repeat(20_000);
    std::fs::write(root.join("utf8.txt"), format!("{line}\n")).unwrap();

    let tool = bounded_read_tool(&root, 4 * 1024 * 1024);
    let output = tool.call(read_args("utf8.txt", None, None)).await.unwrap();
    assert!(output.contains("1 lines total"), "{output}");
    assert!(
        output.contains("\u{4f60}\u{597d}\u{4f60}"),
        "chunked UTF-8 was corrupted"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn invalid_utf8_late_in_a_long_line_is_still_rejected() {
    let root = read_temp_root("utf8-invalid");
    let mut bytes = vec![b'a'; 40_000];
    bytes.push(0xFF);
    bytes.push(b'\n');
    std::fs::write(root.join("binary.txt"), bytes).unwrap();

    let tool = bounded_read_tool(&root, 1024 * 1024);
    let error = tool
        .call(read_args("binary.txt", None, None))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not valid UTF-8"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_truncated_multibyte_sequence_at_eof_is_rejected() {
    let root = read_temp_root("utf8-truncated");
    let mut bytes = "ok\n".as_bytes().to_vec();
    // Leading byte of a three-byte sequence with no continuation bytes.
    bytes.push(0xE4);
    std::fs::write(root.join("truncated.txt"), bytes).unwrap();

    let tool = bounded_read_tool(&root, 1024 * 1024);
    let error = tool
        .call(read_args("truncated.txt", None, None))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not valid UTF-8"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn an_explicit_window_still_reads_a_file_above_the_cap() {
    let root = read_temp_root("explicit-window");
    let cap = 16 * 1024;
    let body = (0..4_000)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.len() as u64 > cap);
    std::fs::write(root.join("big.txt"), body).unwrap();

    let tool = bounded_read_tool(&root, cap);
    let output = tool
        .call(read_args("big.txt", Some(10), Some(2)))
        .await
        .unwrap();
    assert!(output.contains("line 9"), "{output}");
    assert!(output.contains("line 10"), "{output}");
    assert!(!output.contains("line 12"), "{output}");
    std::fs::remove_dir_all(root).unwrap();
}
