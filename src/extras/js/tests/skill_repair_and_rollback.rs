use std::collections::BTreeMap;

use crate::extras::js::skills::coordinator::IndexCoordinator;
use crate::extras::js::skills::embed::Embedder;
use crate::extras::js::skills::lifecycle::{
    EvidenceSnapshot, HumanApproval, LifecycleError, LifecycleService, LifecycleStatus,
    ReplacementTransitionRequest,
};
use crate::extras::js::skills::policy::{PromotionPolicy, TaskOutcomeEvidence, TaskOutcomeSource};
use crate::extras::js::skills::telemetry::TelemetryDispatcher;
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn fixture(directory: &super::TestTempDir) -> (AppPaths, SkillStore, SkillArtifact, SkillArtifact) {
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
    let make = |value: &str| {
        SkillArtifact::new(
            format!("function run() {{ return {value:?}; }}"),
            format!("Replacement {value}"),
            vec![],
            vec![SkillExport {
                name: "run".into(),
                signature: "() => string".into(),
            }],
            vec![format!("run() === {value:?}")],
            CapabilityManifest::pure(),
        )
        .unwrap()
    };
    let predecessor = make("old");
    let candidate = make("new");
    store.insert_verified(&predecessor).unwrap();
    store.insert_verified(&candidate).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions
             SET status = 'canary', supersedes_id = ?, lineage_root_id = ?
             WHERE id = ?",
            rusqlite::params![predecessor.id, predecessor.id, candidate.id],
        )
        .unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions SET lineage_root_id = ? WHERE id = ?",
            rusqlite::params![predecessor.id, predecessor.id],
        )
        .unwrap();
    (paths, store, predecessor, candidate)
}

fn request(predecessor: &SkillArtifact, candidate: &SkillArtifact) -> ReplacementTransitionRequest {
    ReplacementTransitionRequest {
        idempotency_key: "replace-1".into(),
        candidate_id: candidate.id.clone(),
        predecessor_id: predecessor.id.clone(),
        candidate_row_version: 1,
        predecessor_row_version: 1,
        reason: "qualified_evidence".into(),
        snapshot: EvidenceSnapshot::new(
            candidate.id.clone(),
            Some(predecessor.id.clone()),
            "v1",
            vec!["promotion-evidence".into()],
            BTreeMap::from([("decision".into(), serde_json::json!("promote"))]),
            1,
            Some(1),
            0,
        )
        .unwrap(),
    }
}

fn insert_successful_invocations(store: &mut SkillStore, skill_id: &str, prefix: char) {
    for index in 0..25 {
        let invocation_id = format!("{prefix}{index:063x}");
        let turn_id = format!("promotion-{prefix}-{index}");
        for (event_kind, latency) in [("invoked", None), ("returned", Some(100i64))] {
            store
                .conn_mut()
                .execute(
                    "INSERT INTO skill_events (
                         invocation_id, skill_id, turn_id, event_kind, export_name,
                         latency_us, index_generation, evidence_complete, production, created_at
                     ) VALUES (?, ?, ?, ?, 'run', ?, 0, 1, 1, 0)",
                    rusqlite::params![invocation_id, skill_id, turn_id, event_kind, latency],
                )
                .unwrap();
        }
    }
}

#[test]
fn promotion_and_exact_rollback_are_atomic_and_idempotent() {
    let directory = super::TestTempDir::new("replacement");
    let (paths, mut store, predecessor, candidate) = fixture(&directory);
    let mut policy = PromotionPolicy::conservative("v1", 0, 100);
    policy.min_verified_task_passes = Some(1);
    {
        let mut service = LifecycleService::new(&mut store);
        service
            .register_policy("v1", &serde_json::to_string(&policy).unwrap(), 0)
            .unwrap();
    }
    for (evidence_id, evidence_kind) in [
        ("promotion-evidence", "qualified"),
        ("rollback-evidence", "regression"),
    ] {
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_evidence (
                    evidence_id, skill_id, evidence_kind, payload_json,
                    policy_version, created_at
                 ) VALUES (?, ?, ?, '{}', 'v1', 0)",
                rusqlite::params![evidence_id, candidate.id, evidence_kind],
            )
            .unwrap();
    }
    insert_successful_invocations(&mut store, &candidate.id, 'a');
    insert_successful_invocations(&mut store, &predecessor.id, 'b');
    let record_outcome = |turn: &str, evidence_complete| {
        let dispatcher = TelemetryDispatcher::spawn(&paths).unwrap();
        dispatcher
            .record_task_outcome(TaskOutcomeEvidence {
                turn_id: turn.into(),
                skill_ids: vec![candidate.id.clone()],
                verify_passed: true,
                attempt: 1,
                source: TaskOutcomeSource::VerifyCommand("1".repeat(64)),
                production: true,
                evidence_complete,
                created_at: 0,
            })
            .unwrap();
        drop(dispatcher);
    };
    record_outcome("promotion-a-0", false);
    let complete: bool = store
        .conn()
        .query_row(
            "SELECT outcome.evidence_complete FROM skill_task_outcomes AS outcome
             JOIN skill_task_outcome_links AS link USING (evidence_id)
             WHERE link.skill_id = ? AND outcome.turn_id = 'promotion-a-0'",
            [&candidate.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !complete,
        "the fixture must retain a linked incomplete outcome"
    );
    let pair_state = |store: &mut SkillStore| {
        let candidate = LifecycleService::new(store)
            .revision(&candidate.id)
            .unwrap();
        let predecessor = LifecycleService::new(store)
            .revision(&predecessor.id)
            .unwrap();
        let generations = LifecycleService::new(store).index_generations().unwrap();
        let transitions: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM skill_transitions", [], |row| {
                row.get(0)
            })
            .unwrap();
        (candidate, predecessor, generations, transitions)
    };
    // The second audit row is written after both revisions, the generation,
    // and the first audit row. Failing here must roll all of them back.
    const FAIL_SECOND_TRANSITION: &str =
        "CREATE TRIGGER fail_pair_transition BEFORE INSERT ON skill_transitions
         WHEN NEW.idempotency_key LIKE '%:predecessor'
         BEGIN SELECT RAISE(ABORT, 'injected pair transaction failure'); END;";
    let promote_request = request(&predecessor, &candidate);
    let mut forged_request = promote_request.clone();
    forged_request.idempotency_key = "forged-promotion".into();
    forged_request.snapshot.evidence_ids = vec!["rollback-evidence".into()];
    assert!(matches!(
        LifecycleService::new(&mut store).promote_replacement(&forged_request, 1),
        Err(LifecycleError::UnknownEvidence)
    ));
    let incomplete = LifecycleService::new(&mut store).promote_replacement(&promote_request, 1);
    assert!(
        matches!(&incomplete, Err(LifecycleError::PromotionHeld(reason))
            if reason.contains("insufficient_verified_task_passes")),
        "{incomplete:?}"
    );
    record_outcome("promotion-a-1", true);
    let before = pair_state(&mut store);
    store.conn().execute_batch(FAIL_SECOND_TRANSITION).unwrap();
    assert!(matches!(
        LifecycleService::new(&mut store).promote_replacement(&promote_request, 1),
        Err(LifecycleError::Sqlite(_))
    ));
    assert_eq!(pair_state(&mut store), before);
    store
        .conn()
        .execute_batch("DROP TRIGGER fail_pair_transition")
        .unwrap();

    // A separate reader holds the old snapshot while the writer commits the
    // pair. It must see both old revisions until ending that read transaction.
    let mut reader = SkillStore::open_at(&paths).unwrap();
    reader.conn().execute_batch("BEGIN DEFERRED").unwrap();
    assert_eq!(pair_state(&mut reader), before);
    let promoted = LifecycleService::new(&mut store)
        .promote_replacement(&promote_request, 1)
        .unwrap();
    assert_eq!(promoted.candidate_status, LifecycleStatus::Active);
    assert_eq!(promoted.predecessor_status, LifecycleStatus::Superseded);
    let after_promotion = pair_state(&mut store);
    assert_eq!(pair_state(&mut reader), before);
    reader.conn().execute_batch("COMMIT").unwrap();
    assert_eq!(pair_state(&mut reader), after_promotion);
    assert!(
        LifecycleService::new(&mut store)
            .promote_replacement(&promote_request, 2)
            .unwrap()
            .replayed
    );
    let mut conflicting_replay = promote_request.clone();
    conflicting_replay.reason = "different-decision".into();
    assert!(matches!(
        LifecycleService::new(&mut store).promote_replacement(&conflicting_replay, 2),
        Err(LifecycleError::IdempotencyConflict)
    ));
    assert_eq!(pair_state(&mut store), after_promotion);

    let rollback = ReplacementTransitionRequest {
        idempotency_key: "rollback-1".into(),
        candidate_row_version: 2,
        predecessor_row_version: 2,
        reason: "regression".into(),
        snapshot: EvidenceSnapshot::new(
            candidate.id.clone(),
            Some(predecessor.id.clone()),
            "v1",
            vec!["rollback-evidence".into()],
            BTreeMap::from([("decision".into(), serde_json::json!("rollback"))]),
            2,
            Some(2),
            1,
        )
        .unwrap(),
        ..promote_request
    };
    store.conn().execute_batch(FAIL_SECOND_TRANSITION).unwrap();
    assert!(matches!(
        LifecycleService::new(&mut store).rollback_replacement(&rollback, 3),
        Err(LifecycleError::Sqlite(_))
    ));
    assert_eq!(pair_state(&mut store), after_promotion);
    store
        .conn()
        .execute_batch("DROP TRIGGER fail_pair_transition")
        .unwrap();
    reader.conn().execute_batch("BEGIN DEFERRED").unwrap();
    assert_eq!(pair_state(&mut reader), after_promotion);
    let rolled_back = LifecycleService::new(&mut store)
        .rollback_replacement(&rollback, 3)
        .unwrap();
    assert_eq!(rolled_back.candidate_status, LifecycleStatus::Quarantined);
    assert_eq!(rolled_back.predecessor_status, LifecycleStatus::Active);
    assert_eq!(rolled_back.desired_generation, 2);
    let after_rollback = pair_state(&mut store);
    assert_eq!(pair_state(&mut reader), after_promotion);
    reader.conn().execute_batch("COMMIT").unwrap();
    assert_eq!(pair_state(&mut reader), after_rollback);
    drop(reader);
    let replay = LifecycleService::new(&mut store)
        .rollback_replacement(&rollback, 4)
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.desired_generation, rolled_back.desired_generation);
    let mut conflicting_rollback = rollback;
    conflicting_rollback.reason = "another-regression".into();
    assert!(matches!(
        LifecycleService::new(&mut store).rollback_replacement(&conflicting_rollback, 4),
        Err(LifecycleError::IdempotencyConflict)
    ));
    assert_eq!(pair_state(&mut store), after_rollback);
    let predecessor_successor: Option<String> = store
        .conn()
        .query_row(
            "SELECT superseded_by_id FROM skill_revisions WHERE id = ?1",
            [&predecessor.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(predecessor_successor, None);
    drop(store);
}

#[test]
fn invocation_promotion_excludes_a_turn_after_async_ingestion_failure() {
    use crate::extras::js::skills::telemetry::{
        EventBatch, SkillEvent, SkillEventKind, TelemetryIngestor,
        behavioral_window_counts_for_test,
    };
    let directory = super::TestTempDir::new("replacement");
    let (paths, mut store, predecessor, candidate) = fixture(&directory);
    let now = 2_000_000_000;
    let policy = PromotionPolicy::conservative("v1", now, now + 100);
    LifecycleService::new(&mut store)
        .register_policy("v1", &serde_json::to_string(&policy).unwrap(), now)
        .unwrap();
    store
        .conn()
        .execute(
            "INSERT INTO skill_evidence (evidence_id, skill_id, evidence_kind, payload_json,
         policy_version, created_at) VALUES ('promotion-evidence', ?, 'qualified', '{}', 'v1', ?)",
            rusqlite::params![candidate.id, now],
        )
        .unwrap();
    insert_successful_invocations(&mut store, &candidate.id, 'a');
    insert_successful_invocations(&mut store, &predecessor.id, 'b');
    store
        .conn()
        .execute("UPDATE skill_events SET created_at = ?", [now])
        .unwrap();
    store
        .conn()
        .execute_batch(
            "CREATE TRIGGER reject_selected BEFORE INSERT ON skill_events
         WHEN NEW.event_kind = 'selected'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .unwrap();
    let original = SkillEvent {
        invocation_id: Some(format!("a{:063x}", 0)),
        skill_id: candidate.id.clone(),
        turn_id: "promotion-a-0".into(),
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
        created_at: now,
    };
    let dispatcher = TelemetryDispatcher::spawn(&paths).unwrap();
    let failed = SkillEvent {
        kind: SkillEventKind::Selected,
        ..original.clone()
    };
    dispatcher
        .try_dispatch(EventBatch::new(vec![failed]).unwrap())
        .unwrap();
    let probe = dispatcher.shutdown_probe_for_test();
    drop(dispatcher);
    assert_eq!(probe().1, 1);
    // Reopening must preserve the loss without changing the original event's
    // idempotent replay or requiring a task outcome to have been recorded.
    drop(store);
    let mut store = SkillStore::open_at(&paths).unwrap();
    let replay = TelemetryIngestor::new(&mut store)
        .ingest(&EventBatch::new(vec![original.clone()]).unwrap())
        .unwrap();
    assert_eq!((replay.inserted, replay.replayed), (0, 1));
    let counts = behavioral_window_counts_for_test(&store, &candidate.id, now + 1).unwrap();
    let held = LifecycleService::new(&mut store)
        .promote_replacement(&request(&predecessor, &candidate), now + 1);
    assert!(
        matches!(&held, Err(LifecycleError::PromotionHeld(reason))
        if reason.contains("insufficient_distinct_turns")),
        "{held:?}; behavioral counts {counts:?}"
    );
    assert_eq!(counts, (24, 0));
    let fresh = SkillEvent {
        invocation_id: Some(format!("a{:063x}", 25)),
        turn_id: "promotion-a-25".into(),
        ..original
    };
    let returned = SkillEvent {
        kind: SkillEventKind::Returned,
        latency_us: Some(100),
        ..fresh.clone()
    };
    TelemetryIngestor::new(&mut store)
        .ingest(&EventBatch::new(vec![fresh, returned]).unwrap())
        .unwrap();
    assert_eq!(
        behavioral_window_counts_for_test(&store, &candidate.id, now + 1).unwrap(),
        (25, 0)
    );
    LifecycleService::new(&mut store)
        .promote_replacement(&request(&predecessor, &candidate), now + 1)
        .unwrap();
    drop(store);
}

#[test]
fn skill_transition_failure_injection_excludes_removals_from_new_turns() {
    let directory = super::TestTempDir::new("replacement");
    let (paths, store, predecessor, candidate) = fixture(&directory);
    let embedder = std::sync::Arc::new(Embedder::new().unwrap());
    drop(store);
    let coordinator = IndexCoordinator::open(&paths, embedder).unwrap();
    coordinator.rebuild_and_publish().unwrap();
    assert!(coordinator.lease().unwrap().contains_id(&predecessor.id));
    // A malformed row is now treated as missing and repaired. Keep this test's
    // publication-failure contract by corrupting the candidate after the initial
    // generation has been built, then making the repair write itself fail.
    let mut failure_store = SkillStore::open_at(&paths).unwrap();
    failure_store
        .conn_mut()
        .execute(
            "UPDATE skill_embeddings SET embedding = x'00' WHERE skill_id = ?",
            [&candidate.id],
        )
        .unwrap();
    failure_store
        .conn_mut()
        .execute_batch(
            "CREATE TRIGGER fail_embedding_repair
             BEFORE INSERT ON skill_embeddings
             BEGIN
                 SELECT RAISE(ABORT, 'injected embedding repair failure');
             END;",
        )
        .unwrap();
    drop(failure_store);
    let report = coordinator
        .coordinate_mutation(
            std::collections::HashSet::from([predecessor.id.clone()]),
            |store| {
                let tx = store.conn_mut().transaction().unwrap();
                tx.execute(
                    "UPDATE skill_revisions SET status = 'quarantined' WHERE id = ?",
                    [&predecessor.id],
                )
                .unwrap();
                tx.execute(
                    "UPDATE skill_revisions SET status = 'active' WHERE id = ?",
                    [&candidate.id],
                )
                .unwrap();
                let generation: i64 = tx
                    .query_row(
                        "SELECT desired_generation + 1 FROM skill_generations WHERE singleton = 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                tx.execute(
                    "UPDATE skill_generations SET desired_generation = ? WHERE singleton = 1",
                    [generation],
                )
                .unwrap();
                tx.commit().unwrap();
                Ok::<_, std::convert::Infallible>(((), generation as u64))
            },
        )
        .unwrap();
    assert!(report.1.removal_only);
    let frozen = coordinator.lease().unwrap();
    assert!(!frozen.contains_id(&candidate.id));
    assert!(!frozen.contains_id(&predecessor.id));
    drop(coordinator);
}

#[test]
fn skill_root_activation_requires_two_authenticated_human_actions() {
    let directory = super::TestTempDir::new("replacement");
    let (_paths, mut store, predecessor, _candidate) = fixture(&directory);
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions
             SET status = 'canary', evaluation_report_id = 'report-1'
             WHERE id = ?",
            [&predecessor.id],
        )
        .unwrap();
    let mut service = LifecycleService::new(&mut store);
    service
        .register_policy("v1", r#"{"root":"human"}"#, 0)
        .unwrap();
    let first = HumanApproval::verified("approval-phase4", "owner", "report-1", 1).unwrap();
    service
        .record_root_canary_approval(&predecessor.id, &first, 1)
        .unwrap();
    let snapshot = EvidenceSnapshot::new(
        predecessor.id.clone(),
        None,
        "v1",
        vec![],
        BTreeMap::new(),
        1,
        None,
        0,
    )
    .unwrap();
    assert!(matches!(
        HumanApproval::verified("approval-forged", "", "report-1", 1),
        Err(LifecycleError::InvalidHumanApproval)
    ));
    let second = HumanApproval::verified("approval-phase5", "owner", "report-1", 1).unwrap();
    let authorization = service
        .authorize_root_for_test(&predecessor.id, &second, 2)
        .unwrap();
    let other = HumanApproval::verified("approval-phase5-other", "other", "report-1", 1).unwrap();
    let other_authorization = service
        .authorize_root_for_test(&predecessor.id, &other, 2)
        .unwrap();
    let activated = service
        .activate_root(
            "root-activation",
            &predecessor.id,
            &second,
            &authorization,
            &snapshot,
            3,
        )
        .unwrap();
    assert_eq!(activated.status, LifecycleStatus::Active);
    assert!(matches!(
        service.activate_root(
            "root-activation",
            &predecessor.id,
            &other,
            &other_authorization,
            &snapshot,
            4,
        ),
        Err(LifecycleError::IdempotencyConflict)
    ));
    assert!(
        service
            .activate_root(
                "root-activation",
                &predecessor.id,
                &second,
                &authorization,
                &snapshot,
                4,
            )
            .unwrap()
            .replayed
    );
    drop(store);
}
