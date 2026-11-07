    #[cfg(feature = "git-worktree")]
    #[tokio::test]
    async fn continuation_native_staged_guard_accepts_actual_git_tool_rename_records() {
        let repo = TestRepo::new();
        repo.write("old.txt", "some stable content long enough for rename detection\n");
        repo.git(["add", "."]);
        repo.git(["commit", "--quiet", "-m", "base"]);
        repo.git(["mv", "old.txt", "new.txt"]);
        let status = repo.tool().call(args(GitOperation::Status, &[], None)).await.unwrap();
        let selected = vec!["old.txt".to_string(), "new.txt".to_string()];
        let verdict = crate::ui::slash::git::check_staged_paths(&status, &selected);
        assert!(verdict.is_ok(), "actual GitTool rename record was rejected despite selecting both paths: {verdict:?}; {status}");
    }
