use crate::extras::js::skills::scheduler::{PolicyScheduler, RetryOutcome, SchedulerError};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

#[test]
fn decision_leases_are_restart_safe_and_stale_workers_cannot_complete() {
    let root = std::env::temp_dir().join(format!("scheduler-{}", uuid::Uuid::new_v4()));
    let env = PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.clone()),
        data_base: Some(root.clone()),
        local_data_base: Some(root.clone()),
        state_base: Some(root.clone()),
        cache_base: Some(root.clone()),
        workspace_root: None,
        overrides: Default::default(),
    };
    let mut store = SkillStore::open_at(&AppPaths::resolve(&env).unwrap()).unwrap();
    let skill = SkillArtifact::new(
        "function run() { return true; }".into(),
        "Scheduler fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => bool".into(),
        }],
        vec!["run()".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&skill).unwrap();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_policy_versions VALUES ('v1', '{}', 0)",
            [],
        )
        .unwrap();
    let mut scheduler = PolicyScheduler::new(&mut store);
    scheduler.enqueue("decision", &skill.id, "v1", 10).unwrap();
    scheduler.enqueue("decision", &skill.id, "v1", 10).unwrap();
    assert!(matches!(
        scheduler.enqueue("decision", &skill.id, "v1", 11),
        Err(SchedulerError::InvalidLease)
    ));
    let first = scheduler.lease_due("worker-a", 10, 5).unwrap().unwrap();
    assert_eq!(first.attempts, 1);
    assert!(scheduler.lease_due("worker-b", 12, 5).unwrap().is_none());
    let second = scheduler.lease_due("worker-b", 15, 5).unwrap().unwrap();
    assert_eq!(second.attempts, 2);
    assert!(matches!(
        scheduler.complete("decision", "worker-a", 16),
        Err(SchedulerError::StaleLease)
    ));
    scheduler.complete("decision", "worker-b", 16).unwrap();
    assert!(scheduler.lease_due("worker-c", 30, 5).unwrap().is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn permanently_failing_decision_is_dead_lettered_after_eight_attempts() {
    let root = std::env::temp_dir().join(format!("scheduler-dead-{}", uuid::Uuid::new_v4()));
    let env = PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.clone()),
        data_base: Some(root.clone()),
        local_data_base: Some(root.clone()),
        state_base: Some(root.clone()),
        cache_base: Some(root.clone()),
        workspace_root: None,
        overrides: Default::default(),
    };
    let mut store = SkillStore::open_at(&AppPaths::resolve(&env).unwrap()).unwrap();
    let skill = SkillArtifact::new(
        "function run() { return true; }".into(),
        "Scheduler dead-letter fixture".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => bool".into(),
        }],
        vec!["run()".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&skill).unwrap();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_policy_versions VALUES ('v1', '{}', 0)",
            [],
        )
        .unwrap();
    {
        let mut scheduler = PolicyScheduler::new(&mut store);
        scheduler.enqueue("dead", &skill.id, "v1", 10).unwrap();

        let mut now = 10;
        for attempt in 1..=8 {
            let lease = scheduler.lease_due("worker", now, 5).unwrap().unwrap();
            assert_eq!(lease.attempts, attempt);
            match scheduler
                .retry("dead", "worker", now, 1, 60, "backend_down")
                .unwrap()
            {
                RetryOutcome::Scheduled(due) if attempt < 8 => now = due,
                RetryOutcome::DeadLettered if attempt == 8 => {}
                outcome => panic!("unexpected retry outcome at attempt {attempt}: {outcome:?}"),
            }
        }
        assert!(
            scheduler
                .lease_due("worker", now + 1_000, 5)
                .unwrap()
                .is_none()
        );
    }
    let (completed_at, error): (Option<i64>, Option<String>) = store
        .conn()
        .query_row(
            "SELECT completed_at, last_error_code FROM skill_decision_jobs WHERE decision_id = 'dead'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(completed_at.is_some());
    assert_eq!(error.as_deref(), Some("backend_down"));
    std::fs::remove_dir_all(root).unwrap();
}
