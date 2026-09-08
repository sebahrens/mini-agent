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

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mini-agent-list-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn list_dir_reports_actual_child_counts_and_empty_directories() {
    let root = TestDirectory::new();
    std::fs::create_dir(root.0.join("empty")).unwrap();
    std::fs::create_dir_all(root.0.join("populated/nested")).unwrap();
    std::fs::write(root.0.join("populated/file.txt"), "contents").unwrap();
    let tool = ListDirTool::new(None, None, None).with_workspace(&root.0);

    let listing = tool.call(ListDirArgs { path: None }).await.unwrap();
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
        })
        .await
        .unwrap();
    assert!(listing.ends_with("\n(empty directory)"), "{listing}");
}

#[tokio::test]
async fn list_dir_rejects_missing_directory_instead_of_reporting_empty() {
    let root = TestDirectory::new();
    let result = ListDirTool::new(None, None, None)
        .with_workspace(&root.0)
        .call(ListDirArgs {
            path: Some("missing".into()),
        })
        .await;
    assert!(result.is_err(), "missing directory must fail: {result:?}");
}
