use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::extras::js::skills::lifecycle::{
    EvidenceSnapshot, LifecycleError, LifecycleService, LifecycleStatus, TransitionRequest,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn paths() -> (PathBuf, AppPaths) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "phase5-lifecycle-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
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
    let resolved = AppPaths::resolve(&env).expect("test paths");
    (root, resolved)
}

fn artifact(label: &str) -> SkillArtifact {
    SkillArtifact::new(
        format!("function run() {{ return {label:?}; }}"),
        format!("Lifecycle fixture {label}"),
        vec!["lifecycle".to_string()],
        vec![SkillExport {
            name: "run".to_string(),
            signature: "() => string".to_string(),
        }],
        vec!["run() !== ''".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("valid artifact")
}

fn snapshot(id: &str, row_version: i64, generation: i64) -> EvidenceSnapshot {
    EvidenceSnapshot::new(
        id,
        None,
        "phase5-test-v1",
        vec!["evidence-b".into(), "evidence-a".into()],
        BTreeMap::from([
            ("threshold".to_string(), serde_json::json!(25)),
            ("verified".to_string(), serde_json::json!(true)),
        ]),
        row_version,
        None,
        generation,
    )
    .expect("valid snapshot")
}

#[test]
fn phase5_schema_is_restartable_and_complete() {
    let (root, app_paths) = paths();
    {
        let store = SkillStore::open_at(&app_paths).expect("create schema");
        assert_eq!(
            store
                .conn()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            6
        );
        for table in [
            "skill_events",
            "skill_evidence",
            "skill_transitions",
            "skill_stats",
            "skill_daily_stats",
            "skill_compaction_watermarks",
            "skill_repair_records",
            "skill_feedback",
            "skill_feedback_audit",
            "skill_policy_versions",
            "skill_generations",
            "skill_tombstones",
            "skill_decision_jobs",
            "skill_approval_authorizations",
        ] {
            let exists: i64 = store
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = ?",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing Phase 5 table {table}");
        }
    }
    SkillStore::open_at(&app_paths).expect("migration replay");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn canonical_snapshot_is_order_independent_and_versioned() {
    let first = snapshot("abc", 1, 0);
    let second = EvidenceSnapshot::new(
        "abc",
        None,
        "phase5-test-v1",
        vec!["evidence-a".into(), "evidence-b".into()],
        BTreeMap::from([
            ("verified".to_string(), serde_json::json!(true)),
            ("threshold".to_string(), serde_json::json!(25)),
        ]),
        1,
        None,
        0,
    )
    .unwrap();
    assert_eq!(
        first.canonical_json().unwrap(),
        second.canonical_json().unwrap()
    );
    assert!(
        first
            .canonical_json()
            .unwrap()
            .contains("\"schema_version\":1")
    );
}

#[test]
fn skill_lifecycle_transitions_are_atomic_optimistic_and_idempotent() {
    let (root, app_paths) = paths();
    let mut store = SkillStore::open_at(&app_paths).unwrap();
    let artifact = artifact("atomic");
    store.insert_verified(&artifact).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions SET status = 'pending' WHERE id = ?",
            [&artifact.id],
        )
        .unwrap();

    let mut service = LifecycleService::new(&mut store);
    service
        .register_policy("phase5-test-v1", r#"{"min":25,"rate":0.05}"#, 1)
        .unwrap();
    drop(service);
    for evidence_id in ["evidence-a", "evidence-b"] {
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_evidence (
                    evidence_id, skill_id, evidence_kind, payload_json,
                    policy_version, created_at
                 ) VALUES (?, ?, 'verification', '{}', 'phase5-test-v1', 1)",
                rusqlite::params![evidence_id, artifact.id],
            )
            .unwrap();
    }
    let mut service = LifecycleService::new(&mut store);
    let request = TransitionRequest {
        idempotency_key: "transition-1".into(),
        skill_id: artifact.id.clone(),
        from_status: LifecycleStatus::Pending,
        to_status: LifecycleStatus::Verified,
        expected_row_version: 1,
        reason: "verified".into(),
        snapshot: snapshot(&artifact.id, 1, 0),
    };
    let first = service.transition(&request, 2).unwrap();
    let replay = service.transition(&request, 3).unwrap();
    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(first.transition_id, replay.transition_id);
    assert_eq!(first.desired_generation, 1);
    assert_eq!(service.revision(&artifact.id).unwrap().row_version, 2);
    assert_eq!(service.index_generations().unwrap(), (1, 0));

    let mut stale = request.clone();
    stale.idempotency_key = "transition-2".into();
    stale.from_status = LifecycleStatus::Verified;
    stale.to_status = LifecycleStatus::Canary;
    assert!(matches!(
        service.transition(&stale, 4),
        Err(LifecycleError::StaleRowVersion { .. })
            | Err(LifecycleError::EvidenceMismatch)
            | Err(LifecycleError::StaleGeneration { .. })
    ));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skill_lineage_properties_rejected_is_terminal_and_illegal_edges_fail_closed() {
    assert!(LifecycleStatus::Pending.may_transition_to(LifecycleStatus::Rejected));
    assert!(LifecycleStatus::Verified.may_transition_to(LifecycleStatus::Rejected));
    for destination in LifecycleStatus::ALL {
        assert!(!LifecycleStatus::Rejected.may_transition_to(destination));
    }
    assert!(!LifecycleStatus::Pending.may_transition_to(LifecycleStatus::Active));
    assert!(!LifecycleStatus::Quarantined.may_transition_to(LifecycleStatus::Active));
}

#[test]
fn skill_lineage_properties_identity_columns_are_storage_immutable() {
    let (root, app_paths) = paths();
    let mut store = SkillStore::open_at(&app_paths).unwrap();
    let artifact = artifact("immutable");
    store.insert_verified(&artifact).unwrap();
    let mutation = store.conn_mut().execute(
        "UPDATE skill_revisions SET source = 'function run() { return 0; }'
         WHERE id = ?",
        [&artifact.id],
    );
    assert!(mutation.is_err());
    assert_eq!(store.get(&artifact.id).unwrap(), Some(artifact));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn skill_lifecycle_recovery_future_schema_fails_without_mutating_database() {
    let (root, app_paths) = paths();
    {
        let store = SkillStore::open_at(&app_paths).unwrap();
        store
            .conn()
            .execute_batch("PRAGMA user_version = 999;")
            .unwrap();
    }
    let error = SkillStore::open_at(&app_paths)
        .err()
        .expect("future schema");
    assert!(matches!(
        error,
        crate::extras::js::skills::store::StoreError::UnsupportedSchemaVersion(999)
    ));
    std::fs::remove_dir_all(root).unwrap();
}
