use rig::tool::Tool;

use crate::agent::tools::{EditArgs, EditTool, ReadArgs, ReadTool, ReadTracker, is_skip_dir};
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
async fn repeated_read_is_allowed_after_an_external_file_change() {
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
    tokio::fs::write(&path, "after with a different length")
        .await
        .unwrap();
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
    let owner_edit = EditTool::new_with_tracker(None, None, owner_tracker);
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
    let edit = EditTool::new_with_tracker(None, None, tracker);
    let alias_args = || ReadArgs {
        path: alias.to_string_lossy().into_owned(),
        offset: None,
        limit: None,
    };

    assert!(read.call(alias_args()).await.is_ok());
    edit.call(EditArgs {
        path: target.to_string_lossy().into_owned(),
        block: Some("<<<<<<< SEARCH\nbefore\n=======\nafter\n>>>>>>> REPLACE".to_string()),
        file_crc: None,
        edits: None,
    })
    .await
    .unwrap();
    assert!(read.call(alias_args()).await.is_ok());

    let _ = tokio::fs::remove_dir_all(directory).await;
}
