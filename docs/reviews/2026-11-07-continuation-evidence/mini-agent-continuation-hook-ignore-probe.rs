    #[test]
    fn continuation_agent_reads_do_not_unignore_children_of_an_excluded_directory() {
        let root = std::env::temp_dir().join(format!("hook-ignore-parent-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(root.join("build/.gitignore"), "!config.json\n").unwrap();
        std::fs::write(root.join("build/config.json"), "fixture-private-data").unwrap();
        let root = root.canonicalize().unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&root).unwrap();
        let ignored = is_vcs_ignored(&root, std::path::Path::new("build/config.json"));
        let result = read_hook_file(&binding, "build/config.json", None);
        let _ = std::fs::remove_dir_all(&root);
        assert!(ignored && result.is_err(), "root-excluded directory was re-opened by its own .gitignore: ignored={ignored}, result={result:?}");
    }
