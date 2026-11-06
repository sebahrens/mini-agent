//! `no_ignore` on grep/find_files/list_dir: gitignored files stay hidden by
//! default (and the result says so), and become visible only on request.

use rig::tool::Tool;

use crate::agent::tools::{
    FindFilesArgs, FindFilesMode, FindFilesSort, GrepArgs, GrepTool, ListDirArgs, ListDirTool,
};
use crate::tests::support::temp::TempDir;

fn workspace() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new("ignored-discovery");
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "dist/\n.env.local\n").unwrap();
    std::fs::write(root.join("kept.txt"), "needle kept\n").unwrap();
    std::fs::write(root.join(".env.local"), "needle secret-ish\n").unwrap();
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("dist/bundle.js"), "needle bundled\n").unwrap();
    std::fs::create_dir_all(root.join("node_modules/dep")).unwrap();
    std::fs::write(root.join("node_modules/dep/index.js"), "needle dep\n").unwrap();
    (dir, root)
}

fn grep_args(no_ignore: bool) -> GrepArgs {
    GrepArgs {
        pattern: "needle".into(),
        path: None,
        include: None,
        context_lines: None,
        case_insensitive: false,
        files_only: false,
        count: false,
        max_results: None,
        multiline: false,
        offset: None,
        no_ignore,
    }
}

fn find_args(pattern: &str, no_ignore: bool) -> FindFilesArgs {
    FindFilesArgs {
        pattern: pattern.into(),
        mode: FindFilesMode::Glob,
        sort: FindFilesSort::Name,
        no_ignore,
        ..FindFilesArgs::default()
    }
}

#[tokio::test]
async fn grep_hides_ignored_files_by_default_and_says_so() {
    let (_dir, root) = workspace();
    let tool = GrepTool::new(None, None, 50).with_workspace(&root);
    let default = tool.call(grep_args(false)).await.unwrap();
    assert!(default.contains("kept.txt"), "{default}");
    assert!(!default.contains("bundled"), "{default}");
    assert!(!default.contains("secret-ish"), "{default}");
    assert!(
        default.contains("2 gitignored entries were not searched"),
        "{default}"
    );
    assert!(default.contains("no_ignore=true"), "{default}");

    let included = tool.call(grep_args(true)).await.unwrap();
    assert!(
        included.contains("dist/bundle.js:1:needle bundled"),
        "{included}"
    );
    assert!(
        included.contains(".env.local:1:needle secret-ish"),
        "{included}"
    );
    assert!(!included.contains("gitignored"), "{included}");
    // Dependency directories keep their own rule.
    assert!(!included.contains("node_modules/dep"), "{included}");
    assert!(
        included.contains("skipped dependency/build directories"),
        "{included}"
    );
}

#[tokio::test]
async fn find_files_and_list_dir_follow_the_same_contract() {
    let (_dir, root) = workspace();
    let find = crate::agent::tools::FindFilesTool::new(None, None, 50).with_workspace(&root);
    let default = find.call(find_args("**/*.js", false)).await.unwrap();
    assert!(default.contains("No files found"), "{default}");
    assert!(
        default.contains("2 gitignored entries were not searched (.env.local, dist)"),
        "{default}"
    );
    let included = find.call(find_args("**/*.js", true)).await.unwrap();
    assert!(included.contains("dist/bundle.js"), "{included}");
    assert!(!included.contains("node_modules/dep"), "{included}");
    assert!(!included.contains("gitignored"), "{included}");

    let list = ListDirTool::new(None, None, None).with_workspace(&root);
    let hidden = list.call(ListDirArgs::default()).await.unwrap();
    let rows = hidden.split("\n\n[").next().unwrap();
    assert!(!rows.contains("dist"), "{hidden}");
    assert!(!rows.contains(".env.local"), "{hidden}");
    assert!(
        hidden.contains("2 gitignored entries were not searched"),
        "{hidden}"
    );
    let shown = list
        .call(ListDirArgs {
            no_ignore: true,
            depth: Some(2),
            ..ListDirArgs::default()
        })
        .await
        .unwrap();
    assert!(shown.contains("dist"), "{shown}");
    assert!(shown.contains("bundle.js"), "{shown}");
    assert!(shown.contains(".env.local"), "{shown}");
    assert!(!shown.contains("gitignored"), "{shown}");
}

#[test]
fn no_ignore_defaults_closed_in_arguments_and_schemas() {
    let args: GrepArgs = serde_json::from_value(serde_json::json!({"pattern": "x"})).unwrap();
    assert!(!args.no_ignore);
    let args: FindFilesArgs = serde_json::from_value(serde_json::json!({"pattern": "x"})).unwrap();
    assert!(!args.no_ignore);
    let args: ListDirArgs = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(!args.no_ignore);
    for schema in [
        GrepTool::new(None, None, 50).parameters(),
        crate::agent::tools::FindFilesTool::new(None, None, 50).parameters(),
        ListDirTool::new(None, None, None).parameters(),
    ] {
        assert_eq!(
            schema["properties"]["no_ignore"]["default"], false,
            "{schema}"
        );
    }
}
