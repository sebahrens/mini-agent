use crate::extras::js::skills::retention::RetentionService;
use crate::extras::js::skills::telemetry::{
    EventBatch, SkillEvent, SkillEventKind, TelemetryIngestor, stable_invocation_id,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn fixture() -> (std::path::PathBuf, SkillStore, SkillArtifact) {
    let root = std::env::temp_dir().join(format!("retention-{}", uuid::Uuid::new_v4()));
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
