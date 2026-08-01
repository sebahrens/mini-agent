use crate::extras::js::skills::CapabilityTier;
use crate::extras::js::skills::lifecycle::LifecycleStatus;
use crate::extras::js::skills::quarantine::{
    QuarantineDecision, QuarantineEvidence, QuarantinePolicy, QuarantineReason, evaluate,
};
use crate::extras::js::skills::router::{CanaryCandidate, RouteKind, RouteRequest, route};
use crate::extras::js::skills::telemetry::{
    EventBatch, SkillEvent, SkillEventKind, TelemetryError, stable_invocation_id,
};

fn event(invocation_id: &str, kind: SkillEventKind) -> SkillEvent {
    SkillEvent {
        invocation_id: Some(invocation_id.to_string()),
        skill_id: "a".repeat(64),
        turn_id: "failure-turn".into(),
        tool_call_id: Some("failure-tool".into()),
        kind,
        export_name: Some("run".into()),
        outcome: Some("failure".into()),
        latency_us: Some(1),
        retrieval_score: None,
        retrieval_rank: None,
        query_fingerprint: None,
        index_generation: 1,
        evidence_complete: true,
        production: true,
        argument_shape: None,
        created_at: 1,
    }
}

#[test]
fn self_learning_failure_matrix_fails_closed_on_stale_incomplete_or_conflicting_evidence() {
    let route_request = RouteRequest {
        active_id: "active".into(),
        active_lineage_root_id: "root".into(),
        turn_id: "turn".into(),
        policy_version: "phase5-v1".into(),
        canary_share_basis_points: 1_000,
        retrieval_score: 0.8,
        retrieval_rank: 0,
        index_generation: 1,
        candidate: Some(CanaryCandidate {
            candidate_id: "candidate".into(),
            lineage_root_id: "root".into(),
            status: LifecycleStatus::Quarantined,
            model_compatible: true,
            identity_valid: true,
            capability_tier: CapabilityTier::Pure,
            explicitly_idempotent: true,
        }),
    };
    assert_eq!(
        route(b"failure-key", &route_request).unwrap().route_kind,
        RouteKind::Active
    );

    let quarantine = evaluate(
        &QuarantinePolicy::conservative("phase5-v1"),
        &QuarantineEvidence {
            skill_id: "a".repeat(64),
            reason: QuarantineReason::CapabilityPolicyFault,
            qualified_invocations: 1,
            direct_failures: 1,
            evidence_complete: false,
            authenticated_feedback: false,
            feedback_marked_severe: false,
            row_version_current: true,
            generation_current: true,
        },
    );
    assert!(matches!(
        quarantine,
        QuarantineDecision::Hold("incomplete_evidence")
    ));

    let invocation =
        stable_invocation_id("failure-turn", "failure-tool", &"a".repeat(64), "run", 0);
    assert!(matches!(
        EventBatch::new(vec![
            event(&invocation, SkillEventKind::Threw),
            event(&invocation, SkillEventKind::TimedOut),
        ]),
        Err(TelemetryError::MultipleTerminalOutcomes)
    ));
}
