use std::collections::BTreeSet;

use crate::extras::js::skills::feedback::{
    ActorKind, AuthenticatedActor, FeedbackCommand, FeedbackError, FeedbackKind, FeedbackService,
    FeedbackState,
};
use crate::extras::js::skills::privacy::Redactor;
use crate::extras::js::skills::telemetry::{
    EventBatch, SkillEvent, SkillEventKind, TelemetryIngestor, stable_invocation_id,
};
use crate::extras::js::skills::{
    CapabilityManifest, SkillArtifact, SkillExport, store::SkillStore,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

fn fixture() -> (std::path::PathBuf, SkillStore, SkillArtifact, String) {
    let root = std::env::temp_dir().join(format!("feedback-{}", uuid::Uuid::new_v4()));
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
        "Feedback fixture".into(),
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
    let invocation = stable_invocation_id("turn", "tool", &skill.id, "run", 0);
    let event = SkillEvent {
        invocation_id: Some(invocation.clone()),
        skill_id: skill.id.clone(),
        turn_id: "turn".into(),
        tool_call_id: Some("tool".into()),
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
        created_at: 1,
    };
    TelemetryIngestor::new(&mut store)
        .ingest(&EventBatch::new(vec![event]).unwrap())
        .unwrap();
    (root, store, skill, invocation)
}

#[test]
fn skill_feedback_authorization_is_idempotent_redacted_and_audited() {
    let (root, mut store, skill, invocation) = fixture();
    let actor = AuthenticatedActor {
        actor_id: "owner".into(),
        kind: ActorKind::Owner,
        allowed_skill_ids: Some(BTreeSet::from([skill.id.clone()])),
    };
    let command = FeedbackCommand {
        idempotency_key: "feedback-1".into(),
        skill_id: skill.id.clone(),
        invocation_id: Some(invocation),
        kind: FeedbackKind::Negative,
        reason_code: "incorrect_result".into(),
        reason_text: Some("token=SECRET-CANARY".into()),
    };
    let mut service =
        FeedbackService::new(&mut store, Redactor::new(vec!["SECRET-CANARY".into()], 512));
    let id = service.submit(&actor, &command, 2).unwrap();
    assert_eq!(id, service.submit(&actor, &command, 3).unwrap());
    let mut changed_payload = command.clone();
    changed_payload.reason_text = Some("different explanation".into());
    assert!(matches!(
        service.submit(&actor, &changed_payload, 3),
        Err(FeedbackError::IdempotencyConflict)
    ));
    service
        .change_state(&actor, &id, 1, FeedbackState::Resolved, "fixed", 4)
        .unwrap();
    let payload: String = store
        .conn()
        .query_row(
            "SELECT COALESCE(reason_text, '') FROM skill_feedback WHERE feedback_id = ?",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!payload.contains("SECRET-CANARY"));
    let audit: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM skill_feedback_audit WHERE feedback_id = ?",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit, 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn model_and_unknown_targets_have_no_effect() {
    let (_root, mut store, skill, _invocation) = fixture();
    let model = AuthenticatedActor {
        actor_id: "model".into(),
        kind: ActorKind::Model,
        allowed_skill_ids: None,
    };
    let command = FeedbackCommand {
        idempotency_key: "forged".into(),
        skill_id: skill.id,
        invocation_id: Some("unknown".into()),
        kind: FeedbackKind::Severe,
        reason_code: "unsafe_effect".into(),
        reason_text: None,
    };
    let result =
        FeedbackService::new(&mut store, Redactor::new(vec![], 512)).submit(&model, &command, 2);
    assert!(matches!(result, Err(FeedbackError::Unauthorized)));
}

#[test]
fn skill_feedback_privacy_redacts_configured_secret_values() {
    let secret = "FEEDBACK-SECRET-CANARY";
    let redacted =
        Redactor::new(vec![secret.into()], 512).redact(&format!("incorrect token={secret}"));
    assert!(!redacted.contains(secret));
    assert!(redacted.contains("[REDACTED]"));
}

#[test]
fn severe_feedback_requires_an_enumerated_safety_reason() {
    let (_root, mut store, skill, invocation) = fixture();
    let actor = AuthenticatedActor {
        actor_id: "reviewer".into(),
        kind: ActorKind::Reviewer,
        allowed_skill_ids: Some(BTreeSet::from([skill.id.clone()])),
    };
    let command = FeedbackCommand {
        idempotency_key: "severe-invalid".into(),
        skill_id: skill.id,
        invocation_id: Some(invocation),
        kind: FeedbackKind::Severe,
        reason_code: "incorrect_result".into(),
        reason_text: None,
    };
    assert!(matches!(
        FeedbackService::new(&mut store, Redactor::new(vec![], 512)).submit(&actor, &command, 2),
        Err(FeedbackError::InvalidFeedback)
    ));
}
