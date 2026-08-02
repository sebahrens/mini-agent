use std::collections::BTreeMap;

use crate::extras::js::skills::coordinator::IndexCoordinator;
use crate::extras::js::skills::embed::Embedder;
use crate::extras::js::skills::lifecycle::{
    EvidenceSnapshot, HumanApproval, LifecycleError, LifecycleService, LifecycleStatus,
    ReplacementTransitionRequest,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn fixture() -> (AppPaths, SkillStore, SkillArtifact, SkillArtifact) {
    let root = std::env::temp_dir().join(format!("replacement-{}", uuid::Uuid::new_v4()));
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
            vec!["run() !== ''".into()],
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

#[test]
fn promotion_and_exact_rollback_are_atomic_and_idempotent() {
    let (paths, mut store, predecessor, candidate) = fixture();
    let mut service = LifecycleService::new(&mut store);
    service.register_policy("v1", r#"{"min":25}"#, 0).unwrap();
    drop(service);
    for evidence_id in ["promotion-evidence", "rollback-evidence"] {
        store
            .conn_mut()
            .execute(
                "INSERT INTO skill_evidence (
                    evidence_id, skill_id, evidence_kind, payload_json,
                    policy_version, created_at
                 ) VALUES (?, ?, 'qualified', '{}', 'v1', 0)",
                rusqlite::params![evidence_id, candidate.id],
            )
            .unwrap();
    }
    let mut service = LifecycleService::new(&mut store);
    let promote_request = request(&predecessor, &candidate);
    let promoted = service.promote_replacement(&promote_request, 1).unwrap();
    assert_eq!(promoted.candidate_status, LifecycleStatus::Active);
    assert_eq!(promoted.predecessor_status, LifecycleStatus::Superseded);
    assert!(
        service
            .promote_replacement(&promote_request, 2)
            .unwrap()
            .replayed
    );
    let mut conflicting_replay = promote_request.clone();
    conflicting_replay.reason = "different-decision".into();
    assert!(matches!(
        service.promote_replacement(&conflicting_replay, 2),
        Err(LifecycleError::IdempotencyConflict)
    ));

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
    let rolled_back = service.rollback_replacement(&rollback, 3).unwrap();
    assert_eq!(rolled_back.candidate_status, LifecycleStatus::Quarantined);
    assert_eq!(rolled_back.predecessor_status, LifecycleStatus::Active);
    assert_eq!(rolled_back.desired_generation, 2);
    std::fs::remove_dir_all(paths.data_dir).unwrap();
}

#[test]
fn skill_transition_failure_injection_excludes_removals_from_new_turns() {
    let (paths, mut store, predecessor, candidate) = fixture();
    let embedder = std::sync::Arc::new(Embedder::new().unwrap());
    let model = embedder.model_metadata();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_embeddings (
                skill_id, model_id, model_revision, dimensions,
                normalized, embedding, created_at
             ) VALUES (?, ?, ?, ?, 1, x'00', 0)",
            rusqlite::params![
                candidate.id,
                model.model_id,
                model.model_revision,
                model.dimensions as i64,
            ],
        )
        .unwrap();
    drop(store);
    let coordinator = IndexCoordinator::open(&paths, embedder).unwrap();
    coordinator.rebuild_and_publish().unwrap();
    assert!(coordinator.lease().unwrap().contains_id(&predecessor.id));
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
    std::fs::remove_dir_all(paths.data_dir).unwrap();
}

#[test]
fn skill_root_activation_requires_two_authenticated_human_actions() {
    let (paths, mut store, predecessor, _candidate) = fixture();
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
    std::fs::remove_dir_all(paths.data_dir).unwrap();
}
