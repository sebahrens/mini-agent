
#[cfg(test)]
mod continuation_discovery_root_probe {
    use super::*;
    use std::sync::{Arc, Mutex};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};
    use crate::permission::checker::PermissionChecker;

    fn fixture_permission(root: &std::path::Path) -> crate::agent::tools::PermCheck {
        let config = PermissionConfigs::from(PermissionConfig {
            read: Some(ToolPerm::Granular([("private".to_owned(), Action::Deny)].into())),
            ..PermissionConfig::default()
        });
        Arc::new(Mutex::new(PermissionChecker::new(&config, SecurityMode::Standard, Some(root.to_path_buf()), None).unwrap()))
    }

    #[tokio::test]
    async fn continuation_explicit_directory_must_not_bypass_pruned_read_deny() {
        let root = crate::tests::OwnedTestDirectory::new("continuation-denied-root");
        std::fs::create_dir_all(root.path().join("private/sub")).unwrap();
        std::fs::write(root.path().join("private/sub/data.txt"), "fixture-needle\n").unwrap();
        let tool = GrepTool::new(Some(fixture_permission(root.path())), None, 100).with_workspace(root.path());
        let args = |path: &str| serde_json::from_value::<crate::agent::tools::GrepArgs>(serde_json::json!({"pattern":"fixture-needle", "path":path})).unwrap();
        let control = tool.call(args(".")).await.unwrap();
        assert!(!control.contains("data.txt:"), "root search must prune the read-denied directory: {control}");
        for path in ["private", "private/sub"] {
            match tool.call(args(path)).await {
                Err(_) => {},
                Ok(output) => assert!(!output.contains("data.txt:"), "explicit directory bypassed the same read deny: {output}")
            }
        }
    }
    #[tokio::test]
    async fn continuation_find_and_list_explicit_directory_obey_pruned_read_deny() {
        use crate::agent::tools::{FindFilesTool, ListDirTool, FindFilesArgs, ListDirArgs};
        let root = crate::tests::OwnedTestDirectory::new("continuation-denied-metadata-root");
        std::fs::create_dir_all(root.path().join("private/sub")).unwrap();
        std::fs::write(root.path().join("private/sub/data.txt"), "fixture-needle\n").unwrap();
        let permission = fixture_permission(root.path());
        let find = FindFilesTool::new(Some(permission.clone()), None, 100).with_workspace(root.path());
        let list = ListDirTool::new(Some(permission), None, Some(100)).with_workspace(root.path());
        let find_args = |path: &str| serde_json::from_value::<FindFilesArgs>(serde_json::json!({"pattern":"data.txt", "path":path})).unwrap();
        let list_args = |path: &str| serde_json::from_value::<ListDirArgs>(serde_json::json!({"path":path, "depth":3})).unwrap();
        let find_control = find.call(find_args(".")).await.unwrap();
        let list_control = list.call(list_args(".")).await.unwrap();
        assert!(!find_control.contains("data.txt"), "{find_control}");
        assert!(!list_control.contains("data.txt"), "{list_control}");
        for path in ["private", "private/sub"] {
            if let Ok(output) = find.call(find_args(path)).await {
                assert!(!output.contains("data.txt"), "find_files explicit-root bypass: {output}");
            }
            if let Ok(output) = list.call(list_args(path)).await {
                assert!(!output.contains("data.txt"), "list_dir explicit-root bypass: {output}");
            }
        }
    }

}
