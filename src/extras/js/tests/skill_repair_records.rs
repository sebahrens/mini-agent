use crate::extras::js::skills::privacy::Redactor;
use crate::extras::js::skills::repair::{
    ExpectedBehavior, RepairAttemptPolicy, RepairError, RepairInput, RepairProposalSink,
    create_record, persist_record, submit_repair_proposal,
};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};

fn input(secret: &str) -> RepairInput {
    RepairInput {
        failing_skill_id: "a".repeat(64),
        export_name: "run".into(),
        argument_shape: Some(format!(r#"{{"argc":1,"token":"{secret}"}}"#)),
        deterministic_fixture: None,
        fixture_human_approved: false,
        direct_outcome: "exception".into(),
        expected_behavior: ExpectedBehavior::Unknown,
        inherited_case_ids: vec!["case-b".into(), "case-a".into(), "case-a".into()],
        query_fingerprint: Some("v1:opaque".into()),
        retrieval_score: Some(0.8),
        index_generation: 7,
    }
}

#[test]
fn repair_record_persists_and_phase4_adapter_links_quarantined_predecessor() {
    let root = std::env::temp_dir().join(format!("repair-flow-{}", uuid::Uuid::new_v4()));
    let env = crate::paths::PathEnvironment {
        platform: if cfg!(target_os = "macos") {
            crate::paths::PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            crate::paths::PathPlatform::Windows
        } else {
            crate::paths::PathPlatform::Linux
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
    let paths = crate::paths::AppPaths::resolve(&env).unwrap();
    let mut store = crate::extras::js::skills::store::SkillStore::open_at(&paths).unwrap();
    let predecessor = SkillArtifact::new(
        "function run() { return 1; }".into(),
        "Failed predecessor".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => number".into(),
        }],
        vec!["run() === 1".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let candidate = SkillArtifact::new(
        "function run() { return 2; }".into(),
        "Immutable repair".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => number".into(),
        }],
        vec!["run() === 2".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    store.insert_verified(&predecessor).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions SET status = 'quarantined' WHERE id = ?",
            [&predecessor.id],
        )
        .unwrap();
    let mut repair_input = input("safe");
    repair_input.failing_skill_id = predecessor.id.clone();
    let record = create_record(repair_input, &Redactor::new(vec![], 1_024)).unwrap();
    persist_record(&mut store, &record, 10).unwrap();
    persist_record(&mut store, &record, 11).unwrap();
    submit_repair_proposal(&mut store, &predecessor, &candidate, &record).unwrap();
    let predecessor_link: String = store
        .conn()
        .query_row(
            "SELECT predecessor_id FROM skill_proposals WHERE skill_id = ?",
            [&candidate.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(predecessor_link, predecessor.id);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skill_repair_privacy_identity_is_deterministic_sorted_and_secret_free() {
    let secret = "SECRET-CANARY";
    let redactor = Redactor::new(vec![secret.into()], 1_024);
    let mut repair_input = input(secret);
    repair_input.direct_outcome = format!("exception token={secret}");
    let first = create_record(repair_input.clone(), &redactor).unwrap();
    let second = create_record(repair_input, &redactor).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.inherited_case_ids, vec!["case-a", "case-b"]);
    assert!(!serde_json::to_string(&first).unwrap().contains(secret));
}

#[test]
fn skill_repair_proposal_flow_requires_distinct_immutable_identity() {
    struct Sink(usize);
    impl RepairProposalSink for Sink {
        fn enqueue(
            &mut self,
            _artifact: &SkillArtifact,
            _supersedes_id: &str,
            _repair_id: &str,
        ) -> Result<(), String> {
            self.0 += 1;
            Ok(())
        }
    }
    let predecessor = SkillArtifact::new(
        "function run() { return 1; }".into(),
        "Predecessor".into(),
        vec![],
        vec![],
        vec!["true".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let candidate = SkillArtifact::new(
        "function run() { return 2; }".into(),
        "Repair".into(),
        vec![],
        vec![],
        vec!["true".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let mut repair_input = input("safe");
    repair_input.failing_skill_id = predecessor.id.clone();
    let record = create_record(repair_input, &Redactor::new(vec![], 1_024)).unwrap();
    let mut sink = Sink(0);
    submit_repair_proposal(&mut sink, &predecessor, &candidate, &record).unwrap();
    assert_eq!(sink.0, 1);
    assert!(matches!(
        submit_repair_proposal(&mut sink, &predecessor, &predecessor, &record),
        Err(RepairError::InvalidProposal)
    ));
}

#[test]
fn value_fixtures_require_human_approval() {
    let redactor = Redactor::new(vec![], 1_024);
    let mut value = input("safe");
    value.deterministic_fixture = Some(r#"{"input":"value"}"#.into());
    assert!(matches!(
        create_record(value, &redactor),
        Err(RepairError::FixtureApprovalRequired)
    ));
}

#[test]
fn repair_attempt_limits_are_independent_and_backoff_is_checked() {
    let policy = RepairAttemptPolicy {
        max_per_session: 2,
        max_per_lineage: 4,
        base_backoff_seconds: 10,
        max_backoff_seconds: 100,
    };
    assert_eq!(policy.next_allowed_at(1, 2, 1_000), Some(1_040));
    assert_eq!(policy.next_allowed_at(2, 1, 1_000), None);
    assert_eq!(policy.next_allowed_at(1, 4, 1_000), None);
    assert_eq!(policy.next_allowed_at(0, 0, -1), None);
}
