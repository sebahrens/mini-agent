use crate::extras::js::skills::policy::{TaskOutcomeEvidence, TaskOutcomeSource};
use crate::extras::js::skills::retention::RetentionService;
use crate::extras::js::skills::telemetry::{
    EventBatch, SkillEvent, SkillEventKind, TelemetryDispatcher, TelemetryIngestor,
    stable_invocation_id,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use rusqlite::TransactionBehavior;
use std::time::{Duration, Instant};

fn paths(root: &std::path::Path) -> AppPaths {
    let env = PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(root.to_path_buf()),
        data_base: Some(root.to_path_buf()),
        local_data_base: Some(root.to_path_buf()),
        state_base: Some(root.to_path_buf()),
        cache_base: Some(root.to_path_buf()),
        workspace_root: None,
        overrides: Default::default(),
    };
    AppPaths::resolve(&env).unwrap()
}

fn fixture() -> (std::path::PathBuf, SkillStore, SkillArtifact) {
    let root = std::env::temp_dir().join(format!("retention-{}", uuid::Uuid::new_v4()));
    let mut store = SkillStore::open_at(&paths(&root)).unwrap();
    let skill = SkillArtifact::new(
        "function run() { return true; }".into(),
        "Retention fixture".into(),
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
    (root, store, skill)
}

fn selected_batch(skill: &SkillArtifact, turn_id: &str) -> EventBatch {
    EventBatch::new(vec![SkillEvent {
        invocation_id: None,
        skill_id: skill.id.clone(),
        turn_id: turn_id.into(),
        tool_call_id: Some("tool".into()),
        kind: SkillEventKind::Selected,
        export_name: None,
        outcome: None,
        latency_us: None,
        retrieval_score: Some(1.0),
        retrieval_rank: Some(0),
        query_fingerprint: Some("query".into()),
        index_generation: 0,
        evidence_complete: true,
        production: true,
        argument_shape: None,
        created_at: 2_000_000_000,
    }])
    .unwrap()
}

fn dispatch_evidence(
    dispatcher: &TelemetryDispatcher,
    skill: &SkillArtifact,
    turn_id: &str,
    task_outcome: bool,
) {
    if task_outcome {
        dispatcher
            .record_task_outcome(TaskOutcomeEvidence {
                turn_id: turn_id.into(),
                skill_ids: vec![],
                verify_passed: true,
                attempt: 1,
                source: TaskOutcomeSource::Oracle("contention-test".into()),
                production: true,
                evidence_complete: true,
                created_at: 2_000_000_001,
            })
            .unwrap();
    } else {
        dispatcher
            .try_dispatch(selected_batch(skill, turn_id))
            .unwrap();
    }
}

fn evidence_counts(store: &SkillStore) -> (i64, i64) {
    store
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM skill_events),
                (SELECT COUNT(*) FROM skill_task_outcomes)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn wait_until(timeout: Duration, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

#[test]
fn telemetry_dispatch_retries_busy_writer_without_dropping_evidence() {
    for task_outcome in [false, true] {
        let (root, mut store, skill) = fixture();
        // Exercise the actual dispatcher configuration, not a test-only SQLite timeout.
        let dispatcher = TelemetryDispatcher::spawn(&paths(&root)).unwrap();
        let (journal_mode, busy_timeout): (String, i64) = store
            .conn()
            .query_row(
                "SELECT journal_mode, timeout FROM pragma_journal_mode, pragma_busy_timeout",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(journal_mode, "wal");
        assert_eq!(
            busy_timeout, 5_000,
            "ordinary store connections retain their timeout"
        );
        let blocker = store
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        dispatch_evidence(&dispatcher, &skill, "busy-turn", task_outcome);
        let observed_contention = wait_until(Duration::from_secs(7), || {
            dispatcher.busy_retries_for_test() > 0 || dispatcher.observability_lost_for_test() > 0
        });
        let retried = dispatcher.busy_retries_for_test() > 0;
        let lost = dispatcher.observability_lost_for_test();
        // Release the writer before asserting, so even a broken retry loop can join.
        blocker.rollback().unwrap();
        drop(dispatcher);
        let counts = evidence_counts(&store);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
        assert!(observed_contention, "worker did not encounter contention");
        assert_eq!(
            lost, 0,
            "task_outcome={task_outcome}: busy evidence was discarded"
        );
        assert!(retried, "task_outcome={task_outcome}: worker did not retry");
        assert_eq!(counts, if task_outcome { (0, 1) } else { (1, 0) });
    }
}

#[test]
fn telemetry_shutdown_does_not_wait_for_an_external_writer() {
    let (root, mut store, skill) = fixture();
    let mut dispatcher = TelemetryDispatcher::spawn(&paths(&root)).unwrap();
    dispatcher.set_shutdown_budget_for_test(Duration::from_millis(200));
    let probe = dispatcher.shutdown_probe_for_test();
    let blocker = store
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    // Fill the queue with both command types. A separate busy_timeout or flush
    // budget per command would multiply the shutdown delay across this tail.
    for n in 0..64 {
        dispatch_evidence(&dispatcher, &skill, &format!("shutdown-{n}"), n % 2 == 1);
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let join = std::thread::spawn(move || {
        drop(dispatcher);
        done_tx.send(()).unwrap();
    });
    let completed_while_locked = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    // A watchdog releases the writer even on regression: this test must fail,
    // not hang for five seconds per queued item (or forever).
    blocker.rollback().unwrap();
    join.join().unwrap();
    let (_, lost) = probe();
    let counts = evidence_counts(&store);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        completed_while_locked,
        "shutdown depended on releasing the writer"
    );
    assert_eq!(
        lost, 64,
        "every discarded queue command must be accounted for"
    );
    assert_eq!(counts, (0, 0));
}

#[test]
fn telemetry_shutdown_flushes_uncontended_and_transiently_busy_evidence() {
    for contended in [false, true] {
        let (root, mut store, skill) = fixture();
        let dispatcher = TelemetryDispatcher::spawn(&paths(&root)).unwrap();
        let probe = dispatcher.shutdown_probe_for_test();
        let blocker = contended.then(|| {
            store
                .conn_mut()
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap()
        });
        dispatch_evidence(&dispatcher, &skill, "flush-event", false);
        dispatch_evidence(&dispatcher, &skill, "flush-outcome", true);
        let retried = !contended
            || wait_until(Duration::from_secs(7), || {
                dispatcher.busy_retries_for_test() > 0
            });
        let join = std::thread::spawn(move || drop(dispatcher));
        let shutdown_started = wait_until(Duration::from_secs(1), || probe().0);
        blocker
            .map(|blocker| blocker.rollback())
            .transpose()
            .unwrap();
        join.join().unwrap();
        let (_, lost) = probe();
        let counts = evidence_counts(&store);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
        assert!(retried, "worker did not encounter the writer lock");
        assert!(
            shutdown_started,
            "writer must release after shutdown starts"
        );
        assert_eq!(lost, 0);
        assert_eq!(
            counts,
            (1, 1),
            "contended={contended}: shutdown lost evidence"
        );
    }
}

#[test]
fn task_outcome_is_ordered_after_invocations_and_ignores_uninvoked_skills() {
    let (root, store, skill) = fixture();
    let paths = paths(&root);
    let other = SkillArtifact::new(
        "function other() { return true; }".into(),
        "Uninvoked fixture".into(),
        vec![],
        vec![SkillExport {
            name: "other".into(),
            signature: "() => bool".into(),
        }],
        vec!["other()".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    // Keep the original connection open while adding the second candidate.
    other.verify_identity().unwrap();
    drop(store);
    let mut setup = SkillStore::open_at(&paths).unwrap();
    setup.insert_verified(&other).unwrap();
    drop(setup);

    let dispatcher = TelemetryDispatcher::spawn(&paths).unwrap();
    let invocation = stable_invocation_id("task-turn", "tool", &skill.id, "run", 0);
    let event = |kind, outcome, latency| SkillEvent {
        invocation_id: Some(invocation.clone()),
        skill_id: skill.id.clone(),
        turn_id: "task-turn".into(),
        tool_call_id: Some("tool".into()),
        kind,
        export_name: Some("run".into()),
        outcome,
        latency_us: latency,
        retrieval_score: Some(1.0),
        retrieval_rank: Some(0),
        query_fingerprint: Some("query".into()),
        index_generation: 0,
        evidence_complete: true,
        production: true,
        argument_shape: None,
        created_at: 2_000_000_000,
    };
    dispatcher
        .try_dispatch(
            EventBatch::new(vec![
                event(SkillEventKind::Invoked, None, None),
                event(SkillEventKind::Returned, Some("fulfilled".into()), Some(5)),
            ])
            .unwrap(),
        )
        .unwrap();
    dispatcher
        .record_task_outcome(TaskOutcomeEvidence {
            turn_id: "task-turn".into(),
            skill_ids: vec![skill.id.clone(), other.id.clone()],
            verify_passed: true,
            attempt: 1,
            source: TaskOutcomeSource::VerifyCommand("abc123".into()),
            production: true,
            evidence_complete: true,
            created_at: 2_000_000_001,
        })
        .unwrap();
    let check = SkillStore::open_at(&paths).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let rows: Vec<(String, i64, String)> = loop {
        let rows = {
            let mut statement = check
                .conn()
                .prepare(
                    "SELECT link.skill_id, outcome.verify_passed, outcome.source_kind
                     FROM skill_task_outcomes AS outcome
                     JOIN skill_task_outcome_links AS link
                       ON link.evidence_id = outcome.evidence_id
                     ORDER BY link.skill_id",
                )
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        if !rows.is_empty() || Instant::now() >= deadline {
            break rows;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(rows, vec![(skill.id, 1, "verify_command".into())]);
    drop(dispatcher);
    drop(check);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skill_retention_recovery_compaction_is_idempotent_and_preserves_aggregates() {
    let (root, mut store, skill) = fixture();
    let events = vec![
        SkillEventKind::Invoked,
        SkillEventKind::Returned,
        SkillEventKind::Invoked,
        SkillEventKind::Threw,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, kind)| SkillEvent {
        invocation_id: Some(stable_invocation_id(
            &format!("turn-{index}"),
            "retention-tool",
            &skill.id,
            "run",
            0,
        )),
        skill_id: skill.id.clone(),
        turn_id: format!("turn-{index}"),
        tool_call_id: None,
        kind,
        export_name: Some("run".into()),
        outcome: None,
        latency_us: kind.is_terminal().then_some(10),
        retrieval_score: None,
        retrieval_rank: None,
        query_fingerprint: None,
        index_generation: 0,
        evidence_complete: true,
        production: true,
        argument_shape: None,
        created_at: 100,
    })
    .collect();
    TelemetryIngestor::new(&mut store)
        .ingest(&EventBatch::new(events).unwrap())
        .unwrap();
    let mut retention = RetentionService::new(&mut store);
    let first = retention.compact_before(200, 1, 300).unwrap();
    let replay = retention.compact_before(200, 1, 300).unwrap();
    assert_eq!(first.compacted_events, 4);
    assert_eq!(replay.compacted_events, 0);
    let counts: (i64, i64, i64) = store
        .conn()
        .query_row(
            "SELECT invoked_count, direct_success_count, direct_failure_count
             FROM skill_daily_stats WHERE skill_id = ?",
            [&skill.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(counts, (2, 1, 1));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skill_privacy_purge_cascades_and_tombstone_blocks_resurrection() {
    let (root, mut store, skill) = fixture();
    let generation = RetentionService::new(&mut store)
        .privacy_purge(&skill.id, "user_request", 10)
        .unwrap();
    assert_eq!(generation, 1);
    assert!(store.get(&skill.id).unwrap().is_none());
    assert!(matches!(
        store.insert_verified(&skill),
        Err(crate::extras::js::skills::store::StoreError::Purged(_))
    ));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skill_privacy_purge_breaks_dependent_transition_foreign_keys() {
    let (root, mut store, predecessor) = fixture();
    let candidate = SkillArtifact::new(
        "function run() { return false; }".into(),
        "Retention successor".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => bool".into(),
        }],
        vec!["run() === false".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&candidate).unwrap();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_policy_versions
                (policy_version, policy_json, created_at)
             VALUES ('retention-v1', '{}', 0)",
            [],
        )
        .unwrap();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_transitions (
                idempotency_key, skill_id, predecessor_id, from_status,
                to_status, reason, evidence_snapshot, policy_version,
                row_version_from, row_version_to, desired_generation, created_at
             ) VALUES (
                'dependent-transition', ?, ?, 'pending', 'verified',
                'fixture', '{}', 'retention-v1', 1, 2, 0, 0
             )",
            rusqlite::params![candidate.id, predecessor.id],
        )
        .unwrap();

    RetentionService::new(&mut store)
        .privacy_purge(&predecessor.id, "user_request", 10)
        .unwrap();
    let dependent_transitions: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM skill_transitions
             WHERE idempotency_key = 'dependent-transition'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dependent_transitions, 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skill_retention_recovery_never_advances_past_an_ineligible_event() {
    let (root, mut store, skill) = fixture();
    let events = [300, 100]
        .into_iter()
        .enumerate()
        .map(|(index, created_at)| SkillEvent {
            invocation_id: Some(stable_invocation_id(
                &format!("ordered-turn-{index}"),
                "retention-tool",
                &skill.id,
                "run",
                0,
            )),
            skill_id: skill.id.clone(),
            turn_id: format!("ordered-turn-{index}"),
            tool_call_id: None,
            kind: SkillEventKind::Invoked,
            export_name: Some("run".into()),
            outcome: None,
            latency_us: None,
            retrieval_score: None,
            retrieval_rank: None,
            query_fingerprint: None,
            index_generation: 0,
            evidence_complete: true,
            production: true,
            argument_shape: None,
            created_at,
        })
        .collect();
    TelemetryIngestor::new(&mut store)
        .ingest(&EventBatch::new(events).unwrap())
        .unwrap();

    let first = RetentionService::new(&mut store)
        .compact_before(200, 1, 300)
        .unwrap();
    assert_eq!(first.compacted_events, 0);
    let second = RetentionService::new(&mut store)
        .compact_before(400, 1, 500)
        .unwrap();
    assert_eq!(second.compacted_events, 2);
    let invoked: i64 = store
        .conn()
        .query_row(
            "SELECT SUM(invoked_count) FROM skill_daily_stats WHERE skill_id = ?",
            [&skill.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(invoked, 2);
    std::fs::remove_dir_all(root).unwrap();
}
