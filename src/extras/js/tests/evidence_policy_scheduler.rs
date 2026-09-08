use crate::extras::js::skills::scheduler::{PolicyScheduler, RetryOutcome, SchedulerError};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

/// A store with one retained skill and one registered policy version.
fn scheduler_fixture(label: &str) -> (std::path::PathBuf, SkillStore, String) {
    let root = std::env::temp_dir().join(format!("scheduler-{label}-{}", uuid::Uuid::new_v4()));
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
    (root, store, skill.id.clone())
}

/// `run_one` is the orchestration entry point the scheduler will use once the
/// quarantine path enqueues its held decisions: it leases one due decision,
/// dispatches it, and completes or reschedules it. The other methods are
/// covered above; this pins the composition, including that a dispatch failure
/// leaves the decision retryable rather than completed.
#[test]
fn run_one_leases_dispatches_and_settles_exactly_one_due_decision() {
    let (root, mut store, skill_id) = scheduler_fixture("run-one");
    let mut scheduler = PolicyScheduler::new(&mut store);
    scheduler
        .enqueue("decision-a", &skill_id, "v1", 10)
        .unwrap();

    // Nothing is due yet.
    let dispatched = std::cell::RefCell::new(Vec::new());
    assert!(
        !scheduler
            .run_one("worker-a", 5, 5, 1, 60, |lease| {
                dispatched.borrow_mut().push(lease.decision_id.clone());
                Ok(())
            })
            .unwrap(),
        "a decision that is not due must not be leased"
    );
    assert!(dispatched.borrow().is_empty());

    // A failing dispatch reschedules instead of completing.
    assert!(
        scheduler
            .run_one("worker-a", 10, 5, 1, 60, |lease| {
                dispatched.borrow_mut().push(lease.decision_id.clone());
                Err("transient")
            })
            .unwrap()
    );
    assert_eq!(dispatched.borrow().as_slice(), ["decision-a"]);
    assert!(
        scheduler.lease_due("worker-b", 10, 5).unwrap().is_none(),
        "a rescheduled decision must not be immediately leasable again"
    );

    // Once the backoff elapses it is dispatched again, and a successful
    // dispatch completes it for good.
    assert!(
        scheduler
            .run_one("worker-b", 100, 5, 1, 60, |lease| {
                dispatched.borrow_mut().push(lease.decision_id.clone());
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(dispatched.borrow().len(), 2);
    assert!(
        !scheduler
            .run_one("worker-c", 200, 5, 1, 60, |_| Ok(()))
            .unwrap(),
        "a completed decision must never be dispatched again"
    );

    drop(store);
    let _ = std::fs::remove_dir_all(root);
}

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
