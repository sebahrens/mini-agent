use crate::extras::js::skills::quarantine::{
    QuarantineDecision, QuarantineEvidence, QuarantineExecutor, QuarantinePolicy, QuarantineReason,
    evaluate,
};

fn evidence(reason: QuarantineReason) -> QuarantineEvidence {
    QuarantineEvidence {
        skill_id: "skill".into(),
        reason,
        qualified_invocations: 20,
        direct_failures: 5,
        evidence_complete: true,
        authenticated_feedback: true,
        feedback_marked_severe: true,
        row_version_current: true,
        generation_current: true,
    }
}

#[test]
fn every_enumerated_direct_safety_reason_quarantines_immediately() {
    let policy = QuarantinePolicy::conservative("v1");
    for reason in [
        QuarantineReason::IdentityMismatch,
        QuarantineReason::CapabilityPolicyFault,
        QuarantineReason::SandboxPolicyFault,
        QuarantineReason::HeldOutRegression,
        QuarantineReason::CanaryTimeout,
        QuarantineReason::CanaryOom,
        QuarantineReason::UnsafeEmbeddingMetadata,
        QuarantineReason::AuthenticatedCanarySafetyFeedback,
        QuarantineReason::AuthenticatedActiveIntegrityFeedback,
    ] {
        assert!(
            matches!(
                evaluate(&policy, &evidence(reason)),
                QuarantineDecision::Quarantine { .. }
            ),
            "{reason:?}"
        );
    }
}

#[test]
fn behavioral_and_feedback_boundaries_fail_closed() {
    let policy = QuarantinePolicy::conservative("v1");
    let mut behavioral = evidence(QuarantineReason::BehavioralFailureRate);
    behavioral.qualified_invocations = 19;
    assert!(matches!(
        evaluate(&policy, &behavioral),
        QuarantineDecision::Hold(_)
    ));
    behavioral.qualified_invocations = 20;
    behavioral.direct_failures = 4;
    assert!(matches!(
        evaluate(&policy, &behavioral),
        QuarantineDecision::Hold(_)
    ));
    behavioral.direct_failures = 5;
    assert!(matches!(
        evaluate(&policy, &behavioral),
        QuarantineDecision::Quarantine { .. }
    ));

    let mut feedback = evidence(QuarantineReason::AuthenticatedCanarySafetyFeedback);
    feedback.authenticated_feedback = false;
    assert!(matches!(
        evaluate(&policy, &feedback),
        QuarantineDecision::Hold(_)
    ));
}

#[test]
fn incomplete_or_stale_evidence_never_transitions() {
    let policy = QuarantinePolicy::conservative("v1");
    let mut value = evidence(QuarantineReason::IdentityMismatch);
    value.evidence_complete = false;
    assert!(matches!(
        evaluate(&policy, &value),
        QuarantineDecision::Hold("incomplete_evidence")
    ));
    value.evidence_complete = true;
    value.generation_current = false;
    assert!(matches!(
        evaluate(&policy, &value),
        QuarantineDecision::Hold("stale_state")
    ));
}

#[test]
fn skill_quarantine_generation_rejects_stale_generation() {
    let policy = QuarantinePolicy::conservative("v1");
    let mut value = evidence(QuarantineReason::CanaryOom);
    value.generation_current = false;
    assert!(matches!(
        evaluate(&policy, &value),
        QuarantineDecision::Hold("stale_state")
    ));
}

#[test]
fn skill_quarantine_concurrency_retries_produce_identical_snapshot() {
    let policy = QuarantinePolicy::conservative("v1");
    let value = evidence(QuarantineReason::CapabilityPolicyFault);
    assert_eq!(evaluate(&policy, &value), evaluate(&policy, &value));
}

#[test]
fn automatic_quarantine_commits_and_excludes_the_revision_from_new_leases() {
    use std::sync::Arc;

    use crate::extras::js::skills::coordinator::IndexCoordinator;
    use crate::extras::js::skills::embed::Embedder;
    use crate::extras::js::skills::store::SkillStore;
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

    let root = std::env::temp_dir().join(format!("quarantine-executor-{}", uuid::Uuid::new_v4()));
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
    let skill = SkillArtifact::new(
        "function run() { return 1; }".into(),
        "Quarantine executor".into(),
        vec![],
        vec![SkillExport {
            name: "run".into(),
            signature: "() => number".into(),
        }],
        vec!["run() === 1".into()],
        CapabilityManifest::pure(),
    )
    .unwrap();
    let mut store = SkillStore::open_at(&paths).unwrap();
    store.insert_verified(&skill).unwrap();
    drop(store);
    let coordinator = IndexCoordinator::open(&paths, Arc::new(Embedder::new().unwrap())).unwrap();
    let generation = coordinator.rebuild_and_publish().unwrap();
    assert!(coordinator.lease().unwrap().contains_id(&skill.id));
    let policy = QuarantinePolicy::conservative("phase5-quarantine-test");
    let evidence = QuarantineEvidence {
        skill_id: skill.id.clone(),
        reason: QuarantineReason::CapabilityPolicyFault,
        qualified_invocations: 1,
        direct_failures: 1,
        evidence_complete: true,
        authenticated_feedback: false,
        feedback_marked_severe: false,
        row_version_current: true,
        generation_current: true,
    };
    QuarantineExecutor::new(&coordinator)
        .apply(
            &policy,
            &evidence,
            crate::extras::js::skills::lifecycle::LifecycleStatus::Active,
            1,
            generation as i64,
            10,
        )
        .unwrap();
    assert!(!coordinator.lease().unwrap().contains_id(&skill.id));
    let status: String = SkillStore::open_at(&paths)
        .unwrap()
        .conn()
        .query_row(
            "SELECT status FROM skill_revisions WHERE id = ?",
            [&skill.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "quarantined");
    let _ = std::fs::remove_dir_all(root);
}
