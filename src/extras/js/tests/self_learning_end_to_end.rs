use std::collections::BTreeMap;
use std::sync::Arc;

use crate::extras::js::skills::coordinator::IndexCoordinator;
use crate::extras::js::skills::embed::Embedder;
use crate::extras::js::skills::lifecycle::{
    CoordinatedLifecycle, EvidenceSnapshot, HumanApproval, LifecycleService,
    ReplacementTransitionRequest,
};
use crate::extras::js::skills::privacy::Redactor;
use crate::extras::js::skills::repair::{
    ExpectedBehavior, RepairInput, create_record, persist_record, submit_repair_proposal,
};
use crate::extras::js::skills::router::{RouteKind, RouteRequest, route};
use crate::extras::js::skills::store::SkillStore;
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn fixture() -> (std::path::PathBuf, AppPaths) {
    let root = std::env::temp_dir().join(format!("self-learning-{}", uuid::Uuid::new_v4()));
    let paths = AppPaths::resolve(&PathEnvironment {
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
    })
    .unwrap();
    (root, paths)
}

fn artifact(value: i32, description: &str) -> SkillArtifact {
    SkillArtifact::new(
        format!("function run() {{ return {value}; }}"),
        description.into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => number".into(),
        }],
        vec![format!("run() === {value}")],
        CapabilityManifest::pure(),
    )
    .unwrap()
}

#[test]
fn self_learning_end_to_end_root_route_promote_repair_and_rollback() {
    let (root, paths) = fixture();
    let root_artifact = artifact(1, "Lineage root");
    let mut store = SkillStore::open_at(&paths).unwrap();
    store.insert_verified(&root_artifact).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions
             SET status = 'canary', evaluation_report_id = 'root-report', lineage_root_id = id
             WHERE id = ?",
            [&root_artifact.id],
        )
        .unwrap();
    let first_approval =
        HumanApproval::verified("phase4-root-approval", "owner", "root-report", 1).unwrap();
    let mut lifecycle = LifecycleService::new(&mut store);
    lifecycle.register_policy("phase5-v1", "{}", 0).unwrap();
    lifecycle
        .record_root_canary_approval(&root_artifact.id, &first_approval, 1)
        .unwrap();
    let second_approval =
        HumanApproval::verified("phase5-root-activation", "owner", "root-report", 1).unwrap();
    let root_authorization = lifecycle
        .authorize_root_for_test(&root_artifact.id, &second_approval, 1)
        .unwrap();
    drop(store);

    let embedder = Arc::new(Embedder::new().unwrap());
    let coordinator = IndexCoordinator::open(&paths, Arc::clone(&embedder)).unwrap();
    assert!(coordinator.lease().unwrap().is_empty());
    let generation = SkillStore::open_at(&paths)
        .unwrap()
        .generation_state()
        .unwrap()
        .desired_generation;
    let activation_snapshot = EvidenceSnapshot::new(
        root_artifact.id.clone(),
        None,
        "phase5-v1",
        vec![],
        BTreeMap::new(),
        1,
        None,
        generation as i64,
    )
    .unwrap();
    CoordinatedLifecycle::new(&coordinator)
        .activate_root(
            "activate-root",
            &root_artifact.id,
            &second_approval,
            &root_authorization,
            &activation_snapshot,
            2,
        )
        .unwrap();
    assert!(coordinator.lease().unwrap().contains_id(&root_artifact.id));

    let candidate = artifact(2, "Replacement candidate");
    let mut store = SkillStore::open_at(&paths).unwrap();
    store.insert_verified(&candidate).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions
             SET status = 'canary', supersedes_id = ?, lineage_root_id = ?
             WHERE id = ?",
            rusqlite::params![root_artifact.id, root_artifact.id, candidate.id],
        )
        .unwrap();
    let vector = embedder
        .embed_documents(&[candidate.description.clone()])
        .unwrap()
        .pop()
        .unwrap();
    let bytes = vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    let model = embedder.model_metadata();
    store
        .store_embedding(
            &candidate.id,
            &model.model_id,
            &model.model_revision,
            model.dimensions as u32,
            model.normalized,
            &bytes,
        )
        .unwrap();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_evidence
             (evidence_id, skill_id, evidence_kind, payload_json, policy_version, created_at)
             VALUES ('promotion-evidence', ?, 'qualified', '{}', 'phase5-v1', 3)",
            [&candidate.id],
        )
        .unwrap();
    let generation = store.generation_state().unwrap().desired_generation;
    drop(store);

    let (_, candidate_metadata) = coordinator
        .replacement_candidate(&root_artifact.id, generation)
        .unwrap()
        .expect("eligible replacement canary");
    let key = coordinator.routing_key().unwrap();
    assert!((0..100).any(|turn| {
        route(
            &key,
            &RouteRequest {
                active_id: root_artifact.id.clone(),
                active_lineage_root_id: root_artifact.id.clone(),
                turn_id: format!("turn-{turn}"),
                policy_version: "phase5-v1".into(),
                canary_share_basis_points: 1_000,
                retrieval_score: 0.9,
                retrieval_rank: 0,
                index_generation: generation,
                candidate: Some(candidate_metadata.clone()),
            },
        )
        .unwrap()
        .route_kind
            == RouteKind::Canary
    }));

    let promotion = ReplacementTransitionRequest {
        idempotency_key: "promote-replacement".into(),
        candidate_id: candidate.id.clone(),
        predecessor_id: root_artifact.id.clone(),
        candidate_row_version: 1,
        predecessor_row_version: 2,
        reason: "qualified_evidence".into(),
        snapshot: EvidenceSnapshot::new(
            candidate.id.clone(),
            Some(root_artifact.id.clone()),
            "phase5-v1",
            vec!["promotion-evidence".into()],
            BTreeMap::from([("decision".into(), serde_json::json!("promote"))]),
            1,
            Some(2),
            generation as i64,
        )
        .unwrap(),
    };
    CoordinatedLifecycle::new(&coordinator)
        .promote_replacement(&promotion, 4)
        .unwrap();
    let lease = coordinator.lease().unwrap();
    assert!(lease.contains_id(&candidate.id));
    assert!(!lease.contains_id(&root_artifact.id));

    let mut store = SkillStore::open_at(&paths).unwrap();
    store
        .conn_mut()
        .execute(
            "INSERT INTO skill_evidence
             (evidence_id, skill_id, evidence_kind, payload_json, policy_version, created_at)
             VALUES ('rollback-evidence', ?, 'regression', '{}', 'phase5-v1', 5)",
            [&candidate.id],
        )
        .unwrap();
    let generation = store.generation_state().unwrap().desired_generation;
    drop(store);
    let rollback = ReplacementTransitionRequest {
        idempotency_key: "rollback-replacement".into(),
        candidate_row_version: 2,
        predecessor_row_version: 3,
        reason: "direct_regression".into(),
        snapshot: EvidenceSnapshot::new(
            candidate.id.clone(),
            Some(root_artifact.id.clone()),
            "phase5-v1",
            vec!["rollback-evidence".into()],
            BTreeMap::from([("decision".into(), serde_json::json!("rollback"))]),
            2,
            Some(3),
            generation as i64,
        )
        .unwrap(),
        ..promotion
    };
    CoordinatedLifecycle::new(&coordinator)
        .rollback_replacement(&rollback, 6)
        .unwrap();
    let lease = coordinator.lease().unwrap();
    assert!(lease.contains_id(&root_artifact.id));
    assert!(!lease.contains_id(&candidate.id));

    let repair = artifact(3, "Repair proposal");
    let record = create_record(
        RepairInput {
            failing_skill_id: candidate.id.clone(),
            export_name: "run".into(),
            argument_shape: Some(r#"{"argc":0,"types":[]}"#.into()),
            deterministic_fixture: None,
            fixture_human_approved: false,
            direct_outcome: "regression".into(),
            expected_behavior: ExpectedBehavior::Unknown,
            inherited_case_ids: vec!["rollback-evidence".into()],
            query_fingerprint: None,
            retrieval_score: Some(0.9),
            index_generation: generation,
        },
        &Redactor::new(vec![], 1_024),
    )
    .unwrap();
    let mut store = SkillStore::open_at(&paths).unwrap();
    persist_record(&mut store, &record, 7).unwrap();
    submit_repair_proposal(&mut store, &candidate, &repair, &record).unwrap();
    assert_eq!(store.count_proposals().unwrap(), 1);
    let _ = std::fs::remove_dir_all(root);
}
