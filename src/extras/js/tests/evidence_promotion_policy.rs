use crate::extras::js::skills::CapabilityTier;
use crate::extras::js::skills::policy::{
    DirectOutcome, InvocationEvidence, PromotionContext, PromotionDecision, PromotionPolicy,
    evaluate_promotion, nearest_rank_percentile, wilson_upper,
};

fn calls(skill: &str, count: usize, failures: usize, latency: u64) -> Vec<InvocationEvidence> {
    (0..count)
        .map(|index| InvocationEvidence {
            invocation_id: format!("{skill}-invocation-{index}"),
            skill_id: skill.into(),
            turn_id: format!("{skill}-turn-{index}"),
            outcome: if index < failures {
                DirectOutcome::Throw
            } else {
                DirectOutcome::Success
            },
            latency_us: latency,
            production: true,
            observability_complete: true,
            created_at: 100,
        })
        .collect()
}

fn context() -> PromotionContext {
    PromotionContext {
        candidate_id: "candidate".into(),
        predecessor_id: Some("predecessor".into()),
        capability_tier: CapabilityTier::Pure,
        capability_increased: false,
        inherited_tests_passed: true,
        held_out_tests_passed: true,
        unresolved_negative_feedback: false,
        identity_valid: true,
        row_version_current: true,
        generation_current: true,
    }
}

#[test]
fn conservative_policy_promotes_only_qualified_non_inferior_replacement() {
    let policy = PromotionPolicy::conservative("v1", 0, 200);
    let result = evaluate_promotion(
        &policy,
        &context(),
        &calls("candidate", 100, 0, 100),
        &calls("predecessor", 100, 0, 100),
    )
    .unwrap();
    assert_eq!(result.decision, PromotionDecision::Promote);
    assert_eq!(result.candidate.distinct_turns, 100);
    assert_eq!(result.canonical_inputs, result.canonical_inputs.clone());
}

#[test]
fn evidence_qualification_loops_retries_and_incomplete_calls_cannot_inflate_evidence() {
    let policy = PromotionPolicy::conservative("v1", 0, 200);
    let mut candidate = calls("candidate", 25, 0, 100);
    for ordinal in 0..50 {
        candidate.push(InvocationEvidence {
            invocation_id: format!("loop-{ordinal}"),
            skill_id: "candidate".into(),
            turn_id: "candidate-turn-0".into(),
            outcome: DirectOutcome::Success,
            latency_us: 1,
            production: true,
            observability_complete: true,
            created_at: 100,
        });
    }
    candidate.push(InvocationEvidence {
        invocation_id: "incomplete".into(),
        skill_id: "candidate".into(),
        turn_id: "fake-extra-turn".into(),
        outcome: DirectOutcome::Success,
        latency_us: 1,
        production: true,
        observability_complete: false,
        created_at: 100,
    });
    let result = evaluate_promotion(
        &policy,
        &context(),
        &candidate,
        &calls("predecessor", 100, 0, 100),
    )
    .unwrap();
    assert_eq!(result.candidate.distinct_turns, 25);
}

#[test]
fn roots_and_side_effecting_replacements_require_humans() {
    let policy = PromotionPolicy::conservative("v1", 0, 200);
    let mut root = context();
    root.predecessor_id = None;
    let result = evaluate_promotion(&policy, &root, &[], &[]).unwrap();
    assert_eq!(result.decision, PromotionDecision::HumanReview);

    let mut tier_two = context();
    tier_two.capability_tier = CapabilityTier::SideEffecting;
    let result = evaluate_promotion(
        &policy,
        &tier_two,
        &calls("candidate", 100, 0, 100),
        &calls("predecessor", 100, 0, 100),
    )
    .unwrap();
    assert_eq!(result.decision, PromotionDecision::HumanReview);
}

#[test]
fn exact_boundaries_use_nearest_rank_and_wilson_confidence() {
    assert_eq!(nearest_rank_percentile(&[1, 2, 3, 4, 5], 95), Some(5));
    assert_eq!(wilson_upper(0, 0), 1.0);
    assert!(wilson_upper(0, 25) > 0.0);

    let policy = PromotionPolicy::conservative("v1", 0, 200);
    let sparse = evaluate_promotion(
        &policy,
        &context(),
        &calls("candidate", 24, 0, 100),
        &calls("predecessor", 100, 0, 100),
    )
    .unwrap();
    assert_eq!(sparse.decision, PromotionDecision::Hold);

    let severe = evaluate_promotion(
        &policy,
        &context(),
        &{
            let mut values = calls("candidate", 100, 0, 100);
            values[0].outcome = DirectOutcome::Timeout;
            values
        },
        &calls("predecessor", 100, 0, 100),
    )
    .unwrap();
    assert_eq!(severe.decision, PromotionDecision::Hold);
}
