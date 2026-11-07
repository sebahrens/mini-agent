//! The ACP client file seam: reads see the editor's live text, edits and
//! overwrites go back through the editor, and the seam never widens what the
//! tools were already allowed to touch.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rig::tool::Tool;

use super::client_fs::fake::FakeClient;
use super::client_fs::{ClientFs, MAX_CLIENT_TRANSFER_BYTES};
use super::{EditArgs, EditTool, ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool};

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mini-agent-client-fs-{}", uuid::Uuid::new_v4()));
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        Self {
            base,
            workspace,
            outside,
        }
    }

    fn disk(&self, name: &str, text: &str) -> PathBuf {
        let path = self.workspace.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn tracker(client: &Arc<FakeClient>) -> ReadTracker {
        ReadTracker::new(true).with_client_fs(Some(Arc::clone(client) as ClientFs))
    }

    fn read(&self, tracker: &ReadTracker, path: &str) -> ReadTool {
        let _ = path;
        ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
            .with_workspace(self.workspace.clone())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn read_args(path: &str) -> ReadArgs {
    ReadArgs {
        pages: None,
        raw_notebook: false,
        character_offset: None,
        character_limit: None,
        path: path.into(),
        offset: None,
        limit: None,
    }
}

fn edit_args(path: &str, from: &str, to: &str) -> EditArgs {
    EditArgs {
        fuzzy: false,
        path: path.into(),
        replace_all: false,
        block: Some(format!(
            "<<<<<<< SEARCH\n{from}\n=======\n{to}\n>>>>>>> REPLACE"
        )),
        file_crc: None,
        edits: None,
    }
}

#[tokio::test]
async fn read_serves_the_unsaved_editor_text_not_the_disk_text() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "on disk\n");
    let client = FakeClient::with(path, "unsaved in editor\n");
    let tracker = Fixture::tracker(&client);

    let out = fixture
        .read(&tracker, "a.txt")
        .call(read_args("a.txt"))
        .await
        .unwrap();

    assert!(out.contains("unsaved in editor"), "{out}");
    assert!(!out.contains("on disk"), "{out}");
}

#[tokio::test]
async fn closed_by_default_the_tools_use_the_disk_and_never_consult_a_client() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "on disk\n");
    let client = FakeClient::with(path.clone(), "unsaved in editor\n");
    // A client exists in the world, but no handle was attached to the tracker.
    let tracker = ReadTracker::new(true);

    let out = fixture
        .read(&tracker, "a.txt")
        .call(read_args("a.txt"))
        .await
        .unwrap();
    assert!(out.contains("on disk"), "{out}");

    EditTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(edit_args("a.txt", "on disk", "edited on disk"))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "edited on disk\n");
    assert_eq!(client.calls(), 0);
}

#[tokio::test]
async fn edit_computes_against_the_buffer_and_writes_through_the_editor() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "one\n");
    let client = FakeClient::with(path.clone(), "one\ntwo\n");
    let tracker = Fixture::tracker(&client);

    EditTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(edit_args("a.txt", "two", "three"))
        .await
        .unwrap();

    assert_eq!(client.files.lock().unwrap()[&path], "one\nthree\n");
    assert_eq!(client.writes.lock().unwrap().len(), 1);
    // The editor owns persistence; the tool did not touch the disk itself.
    assert_eq!(std::fs::read_to_string(path).unwrap(), "one\n");
}

#[tokio::test]
async fn edit_refuses_when_the_buffer_changed_after_it_was_read() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "one\n");
    let client = FakeClient::with(path.clone(), "one\ntwo\n");
    *client.typed_meanwhile.lock().unwrap() = Some("one\ntwo\nuser typed this\n".into());
    let tracker = Fixture::tracker(&client);

    let error = EditTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(edit_args("a.txt", "two", "three"))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("changed in the editor"), "{error}");
    assert!(client.writes.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "one\n");
}

#[tokio::test]
async fn overwrite_refuses_when_the_buffer_changes_after_the_read_and_before_the_write_lands() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "disk\n");
    let client = FakeClient::with(path.clone(), "buffer\n");
    let tracker = Fixture::tracker(&client);
    fixture
        .read(&tracker, "a.txt")
        .call(read_args("a.txt"))
        .await
        .unwrap();
    // The user types after the agent's last read; the write request then
    // arrives carrying the old preimage.
    *client.typed_meanwhile.lock().unwrap() = Some("buffer\nuser typed this\n".into());
    let error = WriteTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(WriteArgs {
            path: "a.txt".into(),
            content: "agent replacement\n".into(),
            overwrite: true,
        })
        .await
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(error.contains("changed in the editor"), "{error}");
    assert_eq!(
        client.files.lock().unwrap()[&path],
        "buffer\nuser typed this\n",
        "the user's text must survive"
    );
    assert!(client.writes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_client_without_the_conditional_write_extension_never_receives_a_write() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "one\n");
    let client = Arc::new(FakeClient {
        unconditional_only: true,
        ..FakeClient::default()
    });
    client
        .files
        .lock()
        .unwrap()
        .insert(path.clone(), "one\ntwo\n".into());
    let tracker = Fixture::tracker(&client);
    let _ = EditTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(edit_args("a.txt", "one", "uno"))
        .await;
    assert!(
        client.writes.lock().unwrap().is_empty(),
        "an unconditional editor write could clobber text typed after the read"
    );
    assert_eq!(client.files.lock().unwrap()[&path], "one\ntwo\n");
}

#[tokio::test]
async fn a_refused_editor_write_is_reported_and_changes_nothing() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "one\n");
    let client = FakeClient::with(path.clone(), "one\n");
    client.fail_writes.store(true, Ordering::SeqCst);
    let tracker = Fixture::tracker(&client);

    let error = EditTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(edit_args("a.txt", "one", "two"))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("not applied"), "{error}");
    assert_eq!(client.files.lock().unwrap()[&path], "one\n");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "one\n");
}

#[tokio::test]
async fn overwrite_after_a_read_publishes_through_the_editor() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "disk\n");
    let client = FakeClient::with(path.clone(), "buffer\n");
    let tracker = Fixture::tracker(&client);

    fixture
        .read(&tracker, "a.txt")
        .call(read_args("a.txt"))
        .await
        .unwrap();
    WriteTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(WriteArgs {
            path: "a.txt".into(),
            content: "replacement\n".into(),
            overwrite: true,
        })
        .await
        .unwrap();

    assert_eq!(client.files.lock().unwrap()[&path], "replacement\n");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "disk\n");
}

#[tokio::test]
async fn overwrite_without_a_read_of_the_editor_text_is_still_refused() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "disk\n");
    let client = FakeClient::with(path.clone(), "buffer\n");
    let tracker = Fixture::tracker(&client);

    let error = WriteTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(WriteArgs {
            path: "a.txt".into(),
            content: "blind\n".into(),
            overwrite: true,
        })
        .await
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("complete prior read") || error.contains("overwrite"),
        "{error}"
    );
    assert!(client.writes.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "disk\n");
}

#[tokio::test]
async fn a_client_cannot_widen_authority_outside_or_through_links() {
    let fixture = Fixture::new();
    let secret = fixture.outside.join("secret.txt");
    std::fs::write(&secret, "outside original\n").unwrap();
    let client = FakeClient::with(secret.clone(), "client claims this file\n");
    // The client would happily serve anything, including the escape target.
    client.files.lock().unwrap().insert(
        fixture.workspace.join("../outside/secret.txt"),
        "claimed\n".into(),
    );
    let tracker = Fixture::tracker(&client);

    let read = fixture
        .read(&tracker, "x")
        .call(read_args("../outside/secret.txt"))
        .await;
    let edit = EditTool::new_with_tracker(None, None, None, tracker.clone())
        .with_workspace(fixture.workspace.clone())
        .call(edit_args(
            "../outside/secret.txt",
            "outside original",
            "pwned",
        ))
        .await;
    let write = WriteTool::new_with_tracker(None, None, None, tracker.clone())
        .with_workspace(fixture.workspace.clone())
        .call(WriteArgs {
            path: "../outside/secret.txt".into(),
            content: "pwned\n".into(),
            overwrite: true,
        })
        .await;

    for (name, result) in [("read", read), ("edit", edit), ("write", write)] {
        if let Ok(output) = &result {
            assert!(!output.contains("client claims"), "{name}: {output}");
        }
    }
    assert_eq!(
        client.calls(),
        0,
        "the client is consulted only after authority checks"
    );
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "outside original\n"
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&secret, fixture.workspace.join("link.txt")).unwrap();
        let result = fixture
            .read(&tracker, "link.txt")
            .call(read_args("link.txt"))
            .await;
        if let Ok(output) = &result {
            assert!(!output.contains("client claims"), "{output}");
        }
        assert_eq!(client.calls(), 0);
    }
}

#[tokio::test]
async fn permission_denial_stops_before_the_client_is_asked() {
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "disk\n");
    let client = FakeClient::with(path, "buffer\n");
    let tracker = Fixture::tracker(&client);
    let checker = std::sync::Arc::new(std::sync::Mutex::new(
        crate::permission::checker::PermissionChecker::new(
            &PermissionConfigs::from(PermissionConfig {
                default: Some(Action::Allow),
                read: Some(ToolPerm::Simple(Action::Deny)),
                ..Default::default()
            }),
            SecurityMode::Standard,
            Some(fixture.workspace.clone()),
            None,
        )
        .unwrap(),
    ));

    let result = ReadTool::new_with_tracker(Some(checker), None, None, 100, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(read_args("a.txt"))
        .await;

    assert!(result.is_err());
    assert_eq!(client.calls(), 0);
}

#[tokio::test]
async fn an_oversized_or_failing_client_degrades_to_the_disk_file() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "on disk\n");
    let client = FakeClient::with(path.clone(), &"x".repeat(MAX_CLIENT_TRANSFER_BYTES + 1));
    // Repeated-read blocking would hide what each read served.
    let tracker = ReadTracker::new(false).with_client_fs(Some(Arc::clone(&client) as ClientFs));
    let out = fixture
        .read(&tracker, "a.txt")
        .call(read_args("a.txt"))
        .await
        .unwrap();
    assert!(out.contains("on disk"), "{out}");

    client.fail_reads.store(true, Ordering::SeqCst);
    let out = fixture
        .read(&tracker, "a.txt")
        .call(read_args("a.txt"))
        .await
        .unwrap();
    assert!(out.contains("on disk"), "{out}");
}

#[tokio::test]
async fn a_read_only_client_never_receives_a_write_and_edit_stays_on_disk() {
    let fixture = Fixture::new();
    let path = fixture.disk("a.txt", "one\n");
    let client = {
        let client = FakeClient {
            read_only: true,
            ..Default::default()
        };
        client
            .files
            .lock()
            .unwrap()
            .insert(path.clone(), "one\n".into());
        Arc::new(client)
    };
    let tracker = Fixture::tracker(&client);

    EditTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(edit_args("a.txt", "one", "two"))
        .await
        .unwrap();

    assert!(client.writes.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "two\n");
}
