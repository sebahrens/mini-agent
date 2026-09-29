//! Regression tests for mini-agent-zkyt1: `$HOME/...` and `~/...` tool paths
//! are decided bound-vs-ambient from the *expanded* path, so a bound
//! workspace never gains a literal `$HOME` directory and the reported path is
//! the path actually touched on disk.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rig::tool::Tool;

use super::{
    EditArgs, EditTool, FindFilesArgs, FindFilesTool, GrepArgs, GrepTool, ListDirArgs, ListDirTool,
    ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool, resolve_tool_target,
};
use crate::permission::checker::PermissionChecker;
use crate::permission::{PermissionConfigs, SecurityMode};

struct Fixture {
    base: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
    _home: crate::paths::ScopedTestHome,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mini-agent-home-path-{}", uuid::Uuid::new_v4()));
        let workspace = base.join("workspace");
        let home = base.join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        // A thread-local override rather than a process-wide `HOME` change,
        // so concurrently running tests that expand `~` are unaffected.
        let home_override = crate::paths::ScopedTestHome::set(&home);
        Self {
            base,
            workspace,
            home,
            _home: home_override,
        }
    }

    fn assert_no_literal_home_in_workspace(&self) {
        assert!(
            !self.workspace.join("$HOME").exists(),
            "a literal $HOME directory must never be created inside the workspace"
        );
        assert!(!self.workspace.join("~").exists());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[test]
fn resolve_tool_target_decides_from_the_expanded_path() {
    let fixture = Fixture::new();
    let workspace = super::capture_workspace_binding(fixture.workspace.clone());

    let home = resolve_tool_target(Some(&workspace), "$HOME/.config/tool.toml");
    assert_eq!(home.requested, fixture.home.join(".config/tool.toml"));
    assert_eq!(home.bound_relative, None);

    let tilde = resolve_tool_target(Some(&workspace), "~/notes.txt");
    assert_eq!(tilde.requested, fixture.home.join("notes.txt"));
    assert_eq!(tilde.bound_relative, None);

    // An unexpanded `$` component stays a literal workspace-relative path and
    // is reported as its on-disk location inside the workspace.
    let literal = resolve_tool_target(Some(&workspace), "$FOO/x.txt");
    assert_eq!(literal.requested, fixture.workspace.join("$FOO/x.txt"));
    assert_eq!(literal.bound_relative, Some(PathBuf::from("$FOO/x.txt")));

    let relative = resolve_tool_target(Some(&workspace), "./src/lib.rs");
    assert_eq!(relative.requested, fixture.workspace.join("src/lib.rs"));
    assert_eq!(relative.bound_relative, Some(PathBuf::from("./src/lib.rs")));

    let unbound = resolve_tool_target(None, "src/lib.rs");
    assert_eq!(unbound.requested, PathBuf::from("src/lib.rs"));
    assert_eq!(unbound.bound_relative, None);
}

#[tokio::test]
async fn write_home_path_goes_to_home_and_reports_the_written_path() {
    let fixture = Fixture::new();
    let result = WriteTool::new(None, None, None)
        .with_workspace(fixture.workspace.clone())
        .call(WriteArgs {
            path: "$HOME/.config/tool/config.toml".into(),
            content: "key = 1\n".into(),
            overwrite: false,
        })
        .await
        .unwrap();

    let written = fixture.home.join(".config/tool/config.toml");
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "key = 1\n");
    assert!(
        result.ends_with(&format!("to {}", written.display())),
        "reported path must equal the on-disk path: {result}"
    );
    fixture.assert_no_literal_home_in_workspace();
}

#[tokio::test]
async fn write_home_path_is_checked_against_ambient_policy() {
    let fixture = Fixture::new();
    let checker = PermissionChecker::new(
        &PermissionConfigs::default(),
        SecurityMode::Standard,
        Some(fixture.workspace.clone()),
        Some(vec!["standard".to_string()]),
    )
    .expect("valid permission fixture");
    let error = WriteTool::new(Some(Arc::new(Mutex::new(checker))), None, None)
        .with_workspace(fixture.workspace.clone())
        .call(WriteArgs {
            path: "$HOME/.config/tool/config.toml".into(),
            content: "key = 1\n".into(),
            overwrite: false,
        })
        .await
        .expect_err("an out-of-workspace home write must not be silently allowed")
        .to_string();

    assert!(error.contains("Permission denied"), "{error}");
    assert!(!fixture.home.join(".config/tool/config.toml").exists());
    fixture.assert_no_literal_home_in_workspace();
}

#[tokio::test]
async fn write_unexpanded_dollar_component_is_reported_inside_the_workspace() {
    let fixture = Fixture::new();
    let result = WriteTool::new(None, None, None)
        .with_workspace(fixture.workspace.clone())
        .call(WriteArgs {
            path: "$NOT_A_HOME/x.txt".into(),
            content: "literal\n".into(),
            overwrite: false,
        })
        .await
        .unwrap();

    let written = fixture.workspace.join("$NOT_A_HOME/x.txt");
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "literal\n");
    assert!(
        result.ends_with(&format!("to {}", written.display())),
        "{result}"
    );
}

#[tokio::test]
async fn read_and_edit_home_paths_resolve_through_the_ambient_path() {
    let fixture = Fixture::new();
    let target = fixture.home.join("notes.txt");
    std::fs::write(&target, "original contents\n").unwrap();
    let tracker = ReadTracker::new(true);

    let read = ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
        .with_workspace(fixture.workspace.clone())
        .call(ReadArgs {
            path: "$HOME/notes.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
    assert!(read.contains("original contents"), "{read}");

    let edited = EditTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(fixture.workspace.clone())
        .call(EditArgs {
            path: "$HOME/notes.txt".into(),
            replace_all: false,
            block: Some(
                "<<<<<<< SEARCH\noriginal contents\n=======\nmodified contents\n>>>>>>> REPLACE"
                    .to_string(),
            ),
            file_crc: None,
            edits: None,
        })
        .await
        .unwrap();
    assert!(
        edited.contains(&format!("to {}", target.display())),
        "reported path must equal the on-disk path: {edited}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "modified contents\n"
    );
    fixture.assert_no_literal_home_in_workspace();
}

#[tokio::test]
async fn directory_tools_resolve_home_paths_through_the_ambient_path() {
    let fixture = Fixture::new();
    let project = fixture.home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("needle.txt"), "home needle\n").unwrap();
    // A decoy inside the workspace proves the tools did not search a literal
    // workspace-relative `$HOME` tree.
    std::fs::create_dir_all(fixture.workspace.join("project")).unwrap();
    std::fs::write(
        fixture.workspace.join("project/decoy.txt"),
        "workspace needle\n",
    )
    .unwrap();

    let listing = ListDirTool::new(None, None, None)
        .with_workspace(fixture.workspace.clone())
        .call(ListDirArgs {
            path: Some("$HOME/project".into()),
        })
        .await
        .unwrap();
    assert!(listing.contains("needle.txt"), "{listing}");
    assert!(!listing.contains("decoy.txt"), "{listing}");

    let grep = GrepTool::new(None, None, 100)
        .with_workspace(fixture.workspace.clone())
        .call(GrepArgs {
            pattern: "needle".into(),
            path: Some("$HOME/project".into()),
            include: None,
            context_lines: None,
            case_insensitive: false,
            files_only: false,
            count: false,
        })
        .await
        .unwrap();
    assert!(grep.contains("home needle"), "{grep}");
    assert!(!grep.contains("workspace needle"), "{grep}");

    let found = FindFilesTool::new(None, None, 100)
        .with_workspace(fixture.workspace.clone())
        .call(FindFilesArgs {
            pattern: "*.txt".into(),
            path: Some("~/project".into()),
        })
        .await
        .unwrap();
    assert!(found.contains("needle.txt"), "{found}");
    assert!(!found.contains("decoy.txt"), "{found}");

    let found_dollar = FindFilesTool::new(None, None, 100)
        .with_workspace(fixture.workspace.clone())
        .call(FindFilesArgs {
            pattern: "*.txt".into(),
            path: Some("$HOME/project".into()),
        })
        .await
        .unwrap();
    assert_eq!(found_dollar, found);
    fixture.assert_no_literal_home_in_workspace();
}
