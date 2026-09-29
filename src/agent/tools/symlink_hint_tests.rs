//! Regression tests for mini-agent-93b91: a workspace-relative read, edit or
//! write through an in-workspace symbolic link is still refused (bound opens
//! never follow links), but the error names the link and points the model at
//! the absolute target path instead of a raw "Too many levels of symbolic
//! links" / "Not a directory".

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use rig::tool::Tool;

use super::{EditArgs, EditTool, ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool};

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mini-agent-symlink-hint-{}", uuid::Uuid::new_v4()));
        let workspace = base.join("workspace");
        std::fs::create_dir_all(workspace.join("shared/x")).unwrap();
        std::fs::create_dir_all(workspace.join("packages")).unwrap();
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "agents\n").unwrap();
        std::fs::write(workspace.join("shared/x/lib.ts"), "export {}\n").unwrap();
        // docs/CLAUDE.md -> ../AGENTS.md and packages/x -> ../shared/x: the
        // common layouts that used to surface a raw ELOOP/ENOTDIR. A link
        // whose target stays in its own directory may be resolved inside the
        // capability on some platforms, so the fixtures use links that leave
        // their directory, which every platform refuses.
        symlink("../AGENTS.md", workspace.join("docs/CLAUDE.md")).unwrap();
        symlink("../shared/x", workspace.join("packages/x")).unwrap();
        Self { base, workspace }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn assert_symlink_hint(error: &str, link: &Path, target: &Path) {
    assert!(
        error.contains(&format!("'{}'", link.display())),
        "error must name the symlink: {error}"
    );
    assert!(error.contains("symbolic link"), "{error}");
    assert!(
        error.contains("absolute path of the target"),
        "error must suggest the absolute path: {error}"
    );
    assert!(
        error.contains(&format!("'{}'", target.display())),
        "error must name the resolved absolute target: {error}"
    );
    assert!(!error.contains("os error"), "raw errno leaked: {error}");
}

#[tokio::test]
async fn read_through_in_workspace_symlink_names_the_link() {
    let fixture = Fixture::new();
    let tool = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(true))
        .with_workspace(fixture.workspace.clone());

    let error = tool
        .call(ReadArgs {
            path: "docs/CLAUDE.md".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect_err("bound read must not follow a symlinked file")
        .to_string();
    assert_symlink_hint(
        &error,
        &fixture.workspace.join("docs/CLAUDE.md"),
        &fixture.workspace.join("AGENTS.md"),
    );

    let error = tool
        .call(ReadArgs {
            path: "packages/x/lib.ts".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect_err("bound read must not follow a symlinked directory")
        .to_string();
    assert_symlink_hint(
        &error,
        &fixture.workspace.join("packages/x"),
        &fixture.workspace.join("shared/x/lib.ts"),
    );

    // The suggested absolute path is readable through the ambient route.
    let ok = tool
        .call(ReadArgs {
            path: fixture
                .workspace
                .join("AGENTS.md")
                .to_string_lossy()
                .into_owned(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
    assert!(ok.contains("agents"), "{ok}");
}

#[tokio::test]
async fn edit_through_in_workspace_symlink_names_the_link() {
    let fixture = Fixture::new();
    let error = EditTool::new_with_tracker(None, None, None, ReadTracker::new(true))
        .with_workspace(fixture.workspace.clone())
        .call(EditArgs {
            path: "packages/x/lib.ts".into(),
            replace_all: false,
            block: Some("<<<<<<< SEARCH\nexport {}\n=======\nexport {};\n>>>>>>> REPLACE".into()),
            file_crc: None,
            edits: None,
        })
        .await
        .expect_err("bound edit must not follow a symlinked directory")
        .to_string();
    assert_symlink_hint(
        &error,
        &fixture.workspace.join("packages/x"),
        &fixture.workspace.join("shared/x/lib.ts"),
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("shared/x/lib.ts")).unwrap(),
        "export {}\n"
    );
}

#[tokio::test]
async fn write_through_in_workspace_symlink_names_the_link() {
    let fixture = Fixture::new();
    let tool = WriteTool::new(None, None, None).with_workspace(fixture.workspace.clone());

    // Existing symlinked file.
    let error = tool
        .call(WriteArgs {
            path: "docs/CLAUDE.md".into(),
            content: "replaced\n".into(),
            overwrite: true,
        })
        .await
        .expect_err("bound write must not follow a symlinked file")
        .to_string();
    assert_symlink_hint(
        &error,
        &fixture.workspace.join("docs/CLAUDE.md"),
        &fixture.workspace.join("AGENTS.md"),
    );

    // New file below a symlinked directory: the create path reports the link too.
    let error = tool
        .call(WriteArgs {
            path: "packages/x/new.ts".into(),
            content: "new\n".into(),
            overwrite: false,
        })
        .await
        .expect_err("bound write must not create through a symlinked directory")
        .to_string();
    assert!(
        error.contains(&format!(
            "'{}'",
            fixture.workspace.join("packages/x").display()
        )),
        "{error}"
    );
    assert!(error.contains("absolute path of the target"), "{error}");
    assert!(!fixture.workspace.join("shared/x/new.ts").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("AGENTS.md")).unwrap(),
        "agents\n"
    );
}

#[tokio::test]
async fn non_symlink_errors_pass_through_unchanged() {
    let fixture = Fixture::new();
    // `AGENTS.md/child` hits ENOTDIR without any link in the path.
    let error = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(true))
        .with_workspace(fixture.workspace.clone())
        .call(ReadArgs {
            path: "AGENTS.md/child".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect_err("a file used as a directory must fail")
        .to_string();
    assert!(!error.contains("symbolic link"), "{error}");
}

#[tokio::test]
async fn escaping_symlink_target_is_not_disclosed() {
    let fixture = Fixture::new();
    let outside = fixture.base.join("outside-secret.txt");
    std::fs::write(&outside, "secret\n").unwrap();
    symlink(&outside, fixture.workspace.join("docs/escape.md")).unwrap();

    let error = ReadTool::new_with_tracker(None, None, None, 100, ReadTracker::new(true))
        .with_workspace(fixture.workspace.clone())
        .call(ReadArgs {
            path: "docs/escape.md".into(),
            offset: None,
            limit: None,
        })
        .await
        .expect_err("bound read must not follow an escaping symlink")
        .to_string();
    let link = fixture.workspace.join("docs/escape.md");
    assert!(error.contains(&format!("'{}'", link.display())), "{error}");
    assert!(error.contains("absolute path of the target"), "{error}");
    assert!(!error.contains("outside-secret"), "{error}");
}
