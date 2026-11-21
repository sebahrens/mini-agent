use super::*;

#[test]
fn late_refresh_cannot_replace_status_after_a_workspace_switch() {
    let mut session = Session::new("openai", "model", 128_000, "");
    session.working_dir = "/current".into();
    session.git_status = Some(GitStatus {
        modified: 1,
        ..GitStatus::default()
    });
    let cached = session.git_status.clone();
    let refresh = |dir: &str, snapshot: Option<Option<GitStatus>>| GitStatusRefresh {
        working_dir: PathBuf::from(dir),
        branch: Some("main".into()),
        snapshot,
        fingerprint: None,
    };

    assert_eq!(
        apply_git_status_refresh(
            &mut session,
            Path::new("/current"),
            &refresh(
                "/previous",
                Some(Some(GitStatus {
                    untracked: 9,
                    ..GitStatus::default()
                }))
            ),
        ),
        None
    );
    assert_eq!(session.git_status, cached);
    assert_eq!(session.git_branch, None);

    let current = GitStatus {
        staged: 2,
        ..GitStatus::default()
    };
    let fresh = refresh("/current", Some(Some(current.clone())));
    assert_eq!(
        apply_git_status_refresh(&mut session, Path::new("/current"), &fresh),
        Some(true)
    );
    assert_eq!(session.git_status, Some(current.clone()));
    assert_eq!(session.git_branch.as_deref(), Some("main"));
    // Identical results change nothing, so no frame is painted for them.
    assert_eq!(
        apply_git_status_refresh(&mut session, Path::new("/current"), &fresh),
        Some(false)
    );
    // A skipped status spawn keeps the cached status.
    assert_eq!(
        apply_git_status_refresh(
            &mut session,
            Path::new("/current"),
            &refresh("/current", None)
        ),
        Some(false)
    );
    assert_eq!(session.git_status, Some(current));
}
