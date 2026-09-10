use crate::extras::js::skills::scheduler::{
    DecisionLease, PolicyScheduler, RetryOutcome, SchedulerError,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

/// A store with one retained skill and one registered policy version.
fn scheduler_fixture(directory: &super::TestTempDir) -> (AppPaths, SkillStore, String) {
    let root = directory.path().to_path_buf();
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
    let paths = AppPaths::resolve(&env).unwrap();
    let mut store = SkillStore::open_at(&paths).unwrap();
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
    (paths, store, skill.id.clone())
}

/// `run_one` is the orchestration entry point the scheduler will use once the
/// quarantine path enqueues its held decisions: it leases one due decision,
/// dispatches it, and completes or reschedules it. The other methods are
/// covered below; this pins the composition, including that a dispatch failure
/// leaves the decision retryable rather than completed.
#[test]
fn run_one_leases_dispatches_and_settles_exactly_one_due_decision() {
    let directory = super::TestTempDir::new("scheduler-run-one");
    let (paths, mut store, skill_id) = scheduler_fixture(&directory);
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

    for fails in [false, true] {
        let id = if fails {
            "raced-retry"
        } else {
            "raced-complete"
        };
        scheduler.enqueue(id, &skill_id, "v1", 200).unwrap();
        let mut reclaimed = None;
        let result = scheduler.run_one("reused-worker", 200, 5, 1, 60, |_| {
            let mut other_store = SkillStore::open_at(&paths).unwrap();
            reclaimed = PolicyScheduler::new(&mut other_store)
                .lease_due("reused-worker", 205, 5)
                .unwrap();
            if fails { Err("transient") } else { Ok(()) }
        });
        assert!(matches!(result, Err(SchedulerError::StaleLease)));
        let lease = reclaimed.expect("second connection reclaimed the expired lease");
        assert_eq!(lease.decision_id, id);
        assert_eq!(lease.attempts, 2);
        scheduler.complete(&lease, "reused-worker", 206).unwrap();
    }

    drop(store);
}

fn decision_state(store: &SkillStore, id: &str) -> Vec<rusqlite::types::Value> {
    store
        .conn()
        .query_row(
            "SELECT due_at, lease_owner, lease_expires_at, attempts, last_error_code, completed_at
         FROM skill_decision_jobs WHERE decision_id = ?",
            [id],
            |row| (0..6).map(|column| row.get(column)).collect(),
        )
        .unwrap()
}

fn assert_stale_settlement(store: &mut SkillStore, lease: &DecisionLease, owner: &str, now: i64) {
    let before = decision_state(store, &lease.decision_id);
    assert!(matches!(
        PolicyScheduler::new(store).complete(lease, owner, now),
        Err(SchedulerError::StaleLease)
    ));
    assert_eq!(decision_state(store, &lease.decision_id), before);
    assert!(matches!(
        PolicyScheduler::new(store).retry(lease, owner, now, 1, 60, "stale"),
        Err(SchedulerError::StaleLease)
    ));
    assert_eq!(decision_state(store, &lease.decision_id), before);
}

#[test]
fn decision_leases_are_restart_safe_and_stale_workers_cannot_settle() {
    for owner in ["worker-a", "worker-b"] {
        for retry in [false, true] {
            let directory = super::TestTempDir::new("scheduler-leases");
            let (paths, mut store, skill_id) = scheduler_fixture(&directory);
            let mut scheduler = PolicyScheduler::new(&mut store);
            scheduler.enqueue("decision", &skill_id, "v1", 10).unwrap();
            scheduler.enqueue("decision", &skill_id, "v1", 10).unwrap();
            assert!(matches!(
                scheduler.enqueue("decision", &skill_id, "v1", 11),
                Err(SchedulerError::InvalidLease)
            ));
            let first = scheduler.lease_due("worker-a", 10, 5).unwrap().unwrap();
            assert_eq!(first.decision_id, "decision");
            assert_eq!(first.skill_id, skill_id);
            assert_eq!(first.policy_version, "v1");
            assert_eq!((first.attempts, first.lease_expires_at), (1, 15));
            drop(store);

            let mut store = SkillStore::open_at(&paths).unwrap();
            let mut scheduler = PolicyScheduler::new(&mut store);
            assert!(scheduler.lease_due(owner, 12, 5).unwrap().is_none());
            let second = scheduler.lease_due(owner, 15, 5).unwrap().unwrap();
            assert_eq!((second.attempts, second.lease_expires_at), (2, 20));
            assert_stale_settlement(&mut store, &first, "worker-a", 16);
            assert_stale_settlement(&mut store, &second, "wrong-worker", 16);
            let mut wrong_expiry = second.clone();
            wrong_expiry.lease_expires_at += 1;
            assert_stale_settlement(&mut store, &wrong_expiry, owner, 16);
            let (attempts, completed_at) = if retry {
                assert_eq!(
                    PolicyScheduler::new(&mut store)
                        .retry(&second, owner, 16, 1, 60, "transient")
                        .unwrap(),
                    RetryOutcome::Scheduled(18)
                );
                drop(store);
                store = SkillStore::open_at(&paths).unwrap();
                let mut scheduler = PolicyScheduler::new(&mut store);
                assert!(scheduler.lease_due(owner, 17, 2).unwrap().is_none());
                let third = scheduler.lease_due(owner, 18, 2).unwrap().unwrap();
                // Same owner and expiry as the old token: only the attempt
                // generation distinguishes these two live-looking leases.
                assert_eq!(
                    (third.attempts, third.lease_expires_at),
                    (3, second.lease_expires_at)
                );
                assert_stale_settlement(&mut store, &second, owner, 19);
                PolicyScheduler::new(&mut store)
                    .complete(&third, owner, 19)
                    .unwrap();
                (3, 19)
            } else {
                PolicyScheduler::new(&mut store)
                    .complete(&second, owner, 16)
                    .unwrap();
                (2, 16)
            };
            drop(store);

            let mut store = SkillStore::open_at(&paths).unwrap();
            let completion: (i64, Option<i64>, Option<String>, Option<i64>) = store
                .conn()
                .query_row(
                    "SELECT attempts, completed_at, lease_owner, lease_expires_at
                     FROM skill_decision_jobs WHERE decision_id = 'decision'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(completion, (attempts, Some(completed_at), None, None));
            assert!(
                PolicyScheduler::new(&mut store)
                    .lease_due("worker-c", 30, 5)
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn invalid_attempt_counters_do_not_commit_a_lease() {
    for attempts in [i64::from(u32::MAX), -1, i64::MAX] {
        let directory = super::TestTempDir::new("scheduler-counter");
        let (_paths, mut store, skill_id) = scheduler_fixture(&directory);
        PolicyScheduler::new(&mut store)
            .enqueue("decision", &skill_id, "v1", 10)
            .unwrap();
        store
            .conn()
            .execute("UPDATE skill_decision_jobs SET attempts = ?", [attempts])
            .unwrap();
        let before = decision_state(&store, "decision");
        assert!(matches!(
            PolicyScheduler::new(&mut store).lease_due("owner", 10, 5),
            Err(SchedulerError::InvalidLease)
        ));
        assert_eq!(decision_state(&store, "decision"), before);
    }
    let directory = super::TestTempDir::new("scheduler-counter-boundary");
    let (_paths, mut store, skill_id) = scheduler_fixture(&directory);
    PolicyScheduler::new(&mut store)
        .enqueue("decision", &skill_id, "v1", 10)
        .unwrap();
    store
        .conn()
        .execute(
            "UPDATE skill_decision_jobs SET attempts = ?",
            [i64::from(u32::MAX) - 2],
        )
        .unwrap();
    let previous = PolicyScheduler::new(&mut store)
        .lease_due("owner", 10, 5)
        .unwrap()
        .unwrap();
    let lease = PolicyScheduler::new(&mut store)
        .lease_due("owner", 15, 5)
        .unwrap()
        .unwrap();
    assert_eq!((lease.attempts, lease.lease_expires_at), (u32::MAX, 20));
    // Both tokens select retry's dead-letter branch, which must fence stale
    // generations just like normal retry and successful completion.
    assert_stale_settlement(&mut store, &previous, "owner", 16);
    let before = decision_state(&store, "decision");
    let mut zero_attempt = lease.clone();
    zero_attempt.attempts = 0;
    assert!(matches!(
        PolicyScheduler::new(&mut store).complete(&zero_attempt, "owner", 16),
        Err(SchedulerError::InvalidLease)
    ));
    assert!(matches!(
        PolicyScheduler::new(&mut store).retry(&zero_attempt, "owner", 16, 1, 60, "invalid"),
        Err(SchedulerError::InvalidLease)
    ));
    assert_eq!(decision_state(&store, "decision"), before);
    PolicyScheduler::new(&mut store)
        .complete(&lease, "owner", 16)
        .unwrap();
}

#[test]
fn permanently_failing_decision_is_dead_lettered_after_eight_attempts() {
    let directory = super::TestTempDir::new("scheduler-dead-letter");
    let (_paths, mut store, skill_id) = scheduler_fixture(&directory);
    {
        let mut scheduler = PolicyScheduler::new(&mut store);
        scheduler.enqueue("dead", &skill_id, "v1", 10).unwrap();

        let mut now = 10;
        for attempt in 1..=8 {
            let lease = scheduler.lease_due("worker", now, 5).unwrap().unwrap();
            assert_eq!(lease.attempts, attempt);
            if attempt == 8 {
                assert!(matches!(
                    scheduler.retry(&lease, "wrong-worker", now, 1, 60, "stale"),
                    Err(SchedulerError::StaleLease)
                ));
            }
            match scheduler
                .retry(&lease, "worker", now, 1, 60, "backend_down")
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
}
