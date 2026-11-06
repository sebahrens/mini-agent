use crate::agent::tools::list_dir::format_size;
use crate::agent::tools::{ListDirArgs, ListDirTool};
use rig::tool::Tool;

#[test]
fn format_size_preserves_unit_boundaries_and_fractional_units() {
    for (bytes, expected) in [
        (0, "0 B"),
        (1, "1 B"),
        (512, "512 B"),
        (1023, "1023 B"),
        (1024, "1.0 KB"),
        (1536, "1.5 KB"),
        (2048, "2.0 KB"),
        (2560, "2.5 KB"),
        (1_047_552, "1023.0 KB"),
        (1_048_576, "1.0 MB"),
        (1_073_741_824, "1.0 GB"),
        (2_199_023_255_552, "2048.0 GB"),
    ] {
        assert_eq!(format_size(bytes), expected, "bytes={bytes}");
    }
}

use crate::tests::support::temp::TempDir;

#[tokio::test]
async fn list_dir_reports_actual_child_counts_and_empty_directories() {
    let root = TempDir::new("list");
    std::fs::create_dir(root.path().join("empty")).unwrap();
    std::fs::create_dir_all(root.path().join("populated/nested")).unwrap();
    std::fs::write(root.path().join("populated/file.txt"), "contents").unwrap();
    let tool = ListDirTool::new(None, None, None).with_workspace(root.path());

    let listing = tool
        .call(ListDirArgs {
            path: None,
            ..Default::default()
        })
        .await
        .unwrap();
    let rows: Vec<Vec<_>> = listing
        .lines()
        .skip(1)
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(
        rows,
        vec![vec!["[dir(0)]", "empty"], vec!["[dir(2)]", "populated"]]
    );

    let listing = tool
        .call(ListDirArgs {
            path: Some("empty".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(listing.ends_with("\n(empty directory)"), "{listing}");
}

#[tokio::test]
async fn list_dir_rejects_missing_directory_instead_of_reporting_empty() {
    let root = TempDir::new("list");
    let result = ListDirTool::new(None, None, None)
        .with_workspace(root.path())
        .call(ListDirArgs {
            path: Some("missing".into()),
            ..Default::default()
        })
        .await;
    assert!(result.is_err(), "missing directory must fail: {result:?}");
}

// mini-agent-06im2: list_dir hides entries denied to `read` or `list_dir`.
#[tokio::test]
async fn list_dir_omits_denied_entries() {
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
    use std::sync::{Arc, Mutex};

    let root = TempDir::new("list");
    let workspace = root.path().canonicalize().unwrap();
    std::fs::create_dir_all(workspace.join("config/secrets")).unwrap();
    std::fs::write(workspace.join("config/.env"), "TOKEN=x").unwrap();
    std::fs::write(workspace.join("config/app.toml"), "").unwrap();
    std::fs::write(workspace.join("config/hidden.log"), "").unwrap();
    let config = PermissionConfig {
        read: Some(ToolPerm::Granular(
            [
                ("config/.env".to_string(), Action::Deny),
                ("config/secrets".to_string(), Action::Deny),
            ]
            .into(),
        )),
        list_dir: Some(ToolPerm::Granular(
            [("**/*.log".to_string(), Action::Deny)].into(),
        )),
        ..PermissionConfig::default()
    };
    let permission = Arc::new(Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(workspace.clone()),
            None,
        )
        .unwrap(),
    ));
    let tool = ListDirTool::new(Some(permission), None, None).with_workspace(&workspace);
    let absolute = workspace.join("config").to_string_lossy().into_owned();
    for path in ["config".to_string(), absolute] {
        let listing = tool
            .call(ListDirArgs {
                path: Some(path),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(listing.contains("app.toml"), "{listing}");
        assert!(!listing.contains(".env"), "{listing}");
        assert!(!listing.contains("secrets"), "{listing}");
        assert!(!listing.contains("hidden.log"), "{listing}");
    }
}

#[tokio::test]
async fn list_dir_sizes_name_padding_by_characters_instead_of_utf8_bytes() {
    let root = TempDir::new("list");
    std::fs::write(root.path().join("ééé"), "").unwrap();
    std::fs::write(root.path().join("x"), "").unwrap();
    let listing = ListDirTool::new(None, None, None)
        .with_workspace(root.path())
        .call(ListDirArgs {
            path: None,
            ..Default::default()
        })
        .await
        .unwrap();
    let rows: Vec<_> = listing.lines().skip(1).collect();
    assert!(
        rows.iter().any(|row| row.ends_with("ééé  0 B")),
        "{listing}"
    );
    assert!(
        rows.iter().any(|row| row.ends_with("x    0 B")),
        "{listing}"
    );
}

/// mini-agent-yyfbq: offset/limit page past the entry cap and depth renders
/// a bounded tree under the same cap.
#[tokio::test]
async fn list_dir_pages_with_offset_and_renders_a_bounded_depth_tree() {
    let root = TempDir::new("list-pages");
    for n in 0..160 {
        std::fs::write(root.path().join(format!("f{n:03}.txt")), "x").unwrap();
    }
    let tool = ListDirTool::new(None, None, Some(150)).with_workspace(root.path());
    let first = tool.call(ListDirArgs::default()).await.unwrap();
    assert!(first.contains("[file]  f149.txt"), "{first}");
    assert!(!first.contains("f150.txt"), "{first}");
    assert!(
        first.contains("10 more; continue with offset=150,"),
        "{first}"
    );
    let second = tool
        .call(ListDirArgs {
            offset: Some(150),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(second.contains("(entries 151-160 of 160)"), "{second}");
    let rows: Vec<&str> = second
        .lines()
        .filter(|line| line.contains("[file]"))
        .collect();
    assert_eq!(rows.len(), 10, "{second}");
    assert!(rows[0].contains("f150.txt"), "{second}");
    assert!(!second.contains("truncated"), "{second}");
    let limited = tool
        .call(ListDirArgs {
            limit: Some(5),
            offset: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        limited.contains("f003.txt") && limited.contains("f007.txt"),
        "{limited}"
    );
    assert!(limited.contains("continue with offset=8,"), "{limited}");

    let tree = TempDir::new("list-tree");
    std::fs::create_dir_all(tree.path().join("src/nested/deeper/deepest")).unwrap();
    std::fs::write(tree.path().join("src/lib.rs"), "x").unwrap();
    std::fs::write(tree.path().join("src/nested/mod.rs"), "x").unwrap();
    std::fs::write(tree.path().join("README.md"), "x").unwrap();
    let tool = ListDirTool::new(None, None, Some(4)).with_workspace(tree.path());
    let listing = tool
        .call(ListDirArgs {
            depth: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(listing.contains("(depth 3; entries 1-4 of 6)"), "{listing}");
    assert!(listing.contains("  [dir(2)]  src"), "{listing}");
    assert!(listing.contains("    [dir(2)]  src/nested"), "{listing}");
    assert!(
        listing.contains("      [dir(1)]  src/nested/deeper"),
        "{listing}"
    );
    assert!(!listing.contains("deepest"), "{listing}");
    assert!(listing.contains("continue with offset=4,"), "{listing}");
    for depth in [0, 4] {
        assert!(
            tool.call(ListDirArgs {
                depth: Some(depth),
                ..Default::default()
            })
            .await
            .is_err()
        );
    }
}
