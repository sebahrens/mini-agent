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

#[test]
fn telemetry_dispatch_retries_busy_writer_without_dropping_batch() {
    let root = std::env::temp_dir().join(format!("telemetry-busy-{}", uuid::Uuid::new_v4()));
    let paths = paths(&root);
    let mut store = SkillStore::open_at(&paths).unwrap();
    let skill = SkillArtifact::new(
        "function run() { return true; }".into(),
        "Telemetry contention fixture".into(),
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

    let journal_mode: String = store
        .conn()
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    let busy_timeout_ms: i64 = store
        .conn()
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");
    assert_eq!(busy_timeout_ms, 5_000);

    let dispatcher =
        TelemetryDispatcher::spawn_with_busy_timeout_for_test(&paths, Duration::from_millis(1))
            .unwrap();
    let blocker = store
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let batch = EventBatch::new(vec![SkillEvent {
        invocation_id: None,
        skill_id: skill.id.clone(),
        turn_id: "busy-turn".into(),
        tool_call_id: Some("busy-tool".into()),
        kind: SkillEventKind::Selected,
        export_name: None,
        outcome: None,
        latency_us: None,
        retrieval_score: Some(1.0),
        retrieval_rank: Some(0),
        query_fingerprint: Some("busy-query".into()),
        index_generation: 0,
        evidence_complete: true,
        production: true,
        argument_shape: None,
        created_at: 2_000_000_000,
    }])
    .unwrap();
    dispatcher.try_dispatch(batch).unwrap();

    let retry_deadline = Instant::now() + Duration::from_secs(2);
    while dispatcher.busy_retries_for_test() == 0 && Instant::now() < retry_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        dispatcher.busy_retries_for_test() > 0,
        "the worker never observed the forced writer contention"
    );
    blocker.rollback().unwrap();

    let persistence_deadline = Instant::now() + Duration::from_secs(3);
    let persisted = loop {
        let count: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_events
                 WHERE skill_id = ? AND turn_id = 'busy-turn' AND event_kind = 'selected'",
                [&skill.id],
                |row| row.get(0),
            )
            .unwrap();
        if count == 1 || Instant::now() >= persistence_deadline {
            break count;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(persisted, 1, "the retained busy batch was not persisted");
    assert_eq!(dispatcher.observability_lost_for_test(), 0);

    drop(dispatcher);
    drop(store);
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
