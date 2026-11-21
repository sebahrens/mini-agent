//! `write_file` obeys the native `write` tool's overwrite guard, checkpoints
//! the preimage for `/rewind`, and publishes the mutation diff.

use std::sync::Arc;

use rig::tool::{Tool, ToolCallExtensions};

use super::TestTempDir;
use crate::agent::tools::mutation_diff::{DiffOptions, MutationDiff};
use crate::agent::tools::{ReadArgs, ReadTracker};
use crate::extras::js::tool::{JsArgs, JsTool, JsWriteGuardConfig};
use crate::session::file_checkpoints::{FileCheckpoints, scope};

fn guarded_tool(
    workspace: &Arc<crate::paths::WorkspaceBinding>,
    audits: &TestTempDir,
    tracker: &ReadTracker,
) -> JsTool {
    let root = audits.path().join(format!("tool-{}", uuid::Uuid::new_v4()));
    let paths = crate::paths::AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        local_data_dir: root.join("local"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        credentials_dir: root.join("credentials"),
        project_dir: None,
    };
    let audit = crate::extras::js::audit::EffectAudit::open(paths.effect_audit()).unwrap();
    JsTool::new_with_runtime_for_test(
        crate::sandbox::Sandbox::new(false, "bwrap")
            .with_workspace_binding(workspace.clone())
            .with_complete_process_tree_for_test(),
        None,
        None,
        crate::extras::js::host::AllowConfig::unrestricted(workspace.root())
            .with_workspace_binding(workspace.clone()),
        Arc::new(
            crate::extras::js::supervisor::JsWorkerSupervisor::with_launcher_for_test(
                crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
            ),
        ),
        Arc::new(std::sync::Mutex::new(audit)),
    )
    .with_write_guard(JsWriteGuardConfig {
        tracker: tracker.clone(),
        diff_options: DiffOptions::default(),
        permission: None,
        max_text_file_size: 1024 * 1024,
    })
}

async fn read_whole(
    workspace: &Arc<crate::paths::WorkspaceBinding>,
    tracker: &ReadTracker,
    path: &str,
) {
    crate::agent::tools::read::ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
        .with_workspace(workspace.root())
        .call(ReadArgs {
            pages: None,
            raw_notebook: false,
            character_offset: None,
            character_limit: None,
            path: path.into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
}

fn code(path: &str, content: &str) -> JsArgs {
    JsArgs {
        code: format!(
            "try {{ write_file({path:?}, {content:?}); 'ok' }} catch (e) {{ 'ERR ' + e.message }}"
        ),
    }
}

#[tokio::test]
async fn overwriting_an_unread_file_is_refused_and_leaves_it_intact() {
    let audits = TestTempDir::new("js-guard-unread");
    let root = crate::tests::OwnedTestDirectory::new("js-guard-unread-ws");
    std::fs::write(root.path().join("a.txt"), "precious\n").unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tracker = ReadTracker::new(true);
    let tool = guarded_tool(&workspace, &audits, &tracker);

    let out = tool.call(code("a.txt", "clobbered\n")).await.unwrap();
    assert!(out.contains("not fully read"), "{out}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "precious\n"
    );

    // A partial read does not authorize a full replacement either.
    std::fs::write(root.path().join("b.txt"), "1\n2\n3\n").unwrap();
    crate::agent::tools::read::ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
        .with_workspace(workspace.root())
        .call(ReadArgs {
            pages: None,
            raw_notebook: false,
            character_offset: None,
            character_limit: None,
            path: "b.txt".into(),
            offset: None,
            limit: Some(1),
        })
        .await
        .unwrap();
    let out = tool.call(code("b.txt", "x\n")).await.unwrap();
    assert!(out.contains("not fully read"), "{out}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
        "1\n2\n3\n"
    );
}

#[tokio::test]
async fn a_file_changed_after_the_read_is_refused() {
    let audits = TestTempDir::new("js-guard-stale");
    let root = crate::tests::OwnedTestDirectory::new("js-guard-stale-ws");
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tracker = ReadTracker::new(true);
    let tool = guarded_tool(&workspace, &audits, &tracker);
    read_whole(&workspace, &tracker, "a.txt").await;
    std::fs::write(root.path().join("a.txt"), "two (edited elsewhere)\n").unwrap();

    let out = tool.call(code("a.txt", "mine\n")).await.unwrap();
    assert!(out.contains("not fully read"), "{out}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "two (edited elsewhere)\n"
    );
}

#[tokio::test]
async fn overwrite_after_a_full_read_is_rewindable_and_publishes_a_diff() {
    let audits = TestTempDir::new("js-guard-rewind");
    let root = crate::tests::OwnedTestDirectory::new("js-guard-rewind-ws");
    std::fs::write(root.path().join("a.txt"), "original\n").unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tracker = ReadTracker::new(true);
    let tool = guarded_tool(&workspace, &audits, &tracker);
    read_whole(&workspace, &tracker, "a.txt").await;
    let store = FileCheckpoints::default();

    let result = scope(Some(store.recorder("session", 0, true)), async {
        let overwritten = tool
            .call_structured(code("a.txt", "replaced\n"), &ToolCallExtensions::new())
            .await
            .unwrap();
        let created = tool
            .call_structured(code("new.txt", "fresh\n"), &ToolCallExtensions::new())
            .await
            .unwrap();
        (overwritten, created)
    })
    .await;
    let (overwritten, created) = result;
    assert_eq!(overwritten.output, "ok");
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "replaced\n"
    );
    let diff = overwritten
        .extensions
        .get::<MutationDiff>()
        .expect("overwrite diff");
    assert!(diff.rows.iter().any(|row| row.text.contains("replaced")));
    assert!(created.extensions.get::<MutationDiff>().is_some());

    // The model must read again before a second replacement.
    let again = tool.call(code("a.txt", "second\n")).await.unwrap();
    assert!(again.contains("not fully read"), "{again}");

    assert_eq!(store.restore("session", 0, &workspace).unwrap(), 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "original\n"
    );
    assert!(!root.path().join("new.txt").exists());
}

#[tokio::test]
async fn several_writes_in_one_call_name_the_single_shown_preview() {
    let audits = TestTempDir::new("js-guard-multi");
    let root = crate::tests::OwnedTestDirectory::new("js-guard-multi-ws");
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tracker = ReadTracker::new(true);
    let tool = guarded_tool(&workspace, &audits, &tracker);
    let result = tool
        .call_structured(
            JsArgs {
                code: "write_file('one.txt','1\\n'); write_file('two.txt','2\\n'); 'ok'".into(),
            },
            &ToolCallExtensions::new(),
        )
        .await
        .unwrap();
    assert!(
        result.output.contains("1 more file write"),
        "{}",
        result.output
    );
    assert!(result.extensions.get::<MutationDiff>().is_some());
}

#[tokio::test]
async fn a_complete_js_read_authorizes_the_overwrite_of_that_file() {
    let audits = TestTempDir::new("js-guard-jsread");
    let root = crate::tests::OwnedTestDirectory::new("js-guard-jsread-ws");
    std::fs::write(root.path().join("a.txt"), "alpha beta\n").unwrap();
    std::fs::write(root.path().join("b.txt"), "untouched\n").unwrap();
    std::fs::write(root.path().join("c.txt"), "changes\n").unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tracker = ReadTracker::new(true);
    let tool = guarded_tool(&workspace, &audits, &tracker);
    let store = FileCheckpoints::default();

    let result = scope(Some(store.recorder("session", 0, true)), async {
        tool.call_structured(
            JsArgs {
                code: "const t = read_file('a.txt'); write_file('a.txt', t.replace('beta', 'gamma')); \
                       const many = read_files(['c.txt']); write_file('c.txt', many[0] + 'more\\n'); 'ok'"
                    .into(),
            },
            &ToolCallExtensions::new(),
        )
        .await
        .unwrap()
    })
    .await;
    assert!(result.output.starts_with("ok"), "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "alpha gamma\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("c.txt")).unwrap(),
        "changes\nmore\n"
    );
    assert!(result.extensions.get::<MutationDiff>().is_some());
    assert_eq!(store.restore("session", 0, &workspace).unwrap(), 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "alpha beta\n"
    );

    // Without a read the guard still refuses, and a read does not outlive the write.
    let blind = tool.call(code("b.txt", "blind\n")).await.unwrap();
    assert!(blind.contains("not fully read"), "{blind}");
    let twice = tool
        .call(JsArgs {
            code: "read_file('a.txt'); write_file('a.txt', '1\\n'); \
                   try { write_file('a.txt', '2\\n'); 'second ok' } catch (e) { 'second refused' }"
                .into(),
        })
        .await
        .unwrap();
    assert_eq!(twice, "second refused");
}

#[tokio::test]
async fn a_js_read_followed_by_an_external_change_does_not_authorize_the_overwrite() {
    let audits = TestTempDir::new("js-guard-jsstale");
    let root = crate::tests::OwnedTestDirectory::new("js-guard-jsstale-ws");
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tracker = ReadTracker::new(true);
    let tool = guarded_tool(&workspace, &audits, &tracker);
    tool.call(JsArgs {
        code: "read_file('a.txt'); 1".into(),
    })
    .await
    .unwrap();
    std::fs::write(root.path().join("a.txt"), "two (edited elsewhere)\n").unwrap();
    let out = tool.call(code("a.txt", "mine\n")).await.unwrap();
    assert!(out.contains("not fully read"), "{out}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "two (edited elsewhere)\n"
    );
}

#[tokio::test]
async fn overwriting_a_non_text_file_says_so_instead_of_asking_for_a_read() {
    let audits = TestTempDir::new("js-guard-nontext");
    let root = crate::tests::OwnedTestDirectory::new("js-guard-nontext-ws");
    std::fs::write(root.path().join("l1.txt"), b"na\xefve").unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let tracker = ReadTracker::new(true);
    let tool = guarded_tool(&workspace, &audits, &tracker);
    let out = tool.call(code("l1.txt", "x")).await.unwrap();
    assert!(out.contains("not_text"), "{out}");
    assert!(!out.contains("not fully read"), "{out}");
}
