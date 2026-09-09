use crate::extras::js::skills::CapabilityTier;
use crate::extras::js::skills::policy::{
    DirectOutcome, InvocationEvidence, PromotionContext, PromotionDecision, PromotionPolicy,
    TaskOutcomeEvidence, TaskOutcomeSource, evaluate_promotion,
    evaluate_promotion_with_task_outcomes, nearest_rank_percentile, wilson_upper,
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
    let inputs: serde_json::Value = serde_json::from_str(&result.canonical_inputs).unwrap();
    assert_eq!(inputs["policy"], serde_json::to_value(&policy).unwrap());
    assert_eq!(inputs["context"], serde_json::to_value(context()).unwrap());
    assert_eq!(
        inputs["candidate"],
        serde_json::to_value(&result.candidate).unwrap()
    );
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

#[test]
fn configured_task_outcome_gate_counts_only_qualified_distinct_turns() {
    let mut policy = PromotionPolicy::conservative("v1", 50, 200);
    policy.min_verified_task_passes = Some(1);
    let candidate_id = "a".repeat(64);
    let unrelated_id = "b".repeat(64);
    let mut promotion_context = context();
    promotion_context.candidate_id = candidate_id.clone();
    let candidate = calls(&candidate_id, 100, 0, 100);
    let predecessor = calls("predecessor", 100, 0, 100);

    let without_outcome =
        evaluate_promotion(&policy, &promotion_context, &candidate, &predecessor).unwrap();
    assert_eq!(without_outcome.decision, PromotionDecision::Hold);
    assert!(
        without_outcome
            .reasons
            .iter()
            .any(|reason| reason == "insufficient_verified_task_passes")
    );

    for (source, carries_verdict) in [
        (
            TaskOutcomeSource::VerifyCommand("0123456789abcdef".into()),
            true,
        ),
        (TaskOutcomeSource::Oracle("task-oracle".into()), true),
        (TaskOutcomeSource::NoVerifyCommand, false),
        (TaskOutcomeSource::GateSkipped, false),
    ] {
        for (production, evidence_complete, created_at, skill_id, in_scope) in [
            (true, true, 100, &candidate_id, true),
            (false, true, 100, &candidate_id, false),
            (true, false, 100, &candidate_id, false),
            (true, true, 49, &candidate_id, false),
            (true, true, 201, &candidate_id, false),
            (true, true, 100, &unrelated_id, false),
        ] {
            let passed = TaskOutcomeEvidence {
                turn_id: "candidate-turn-0".into(),
                skill_ids: vec![skill_id.clone()],
                verify_passed: true,
                attempt: 1,
                source: source.clone(),
                production,
                evidence_complete,
                created_at,
            };
            let failed = TaskOutcomeEvidence {
                turn_id: "candidate-turn-1".into(),
                verify_passed: false,
                ..passed.clone()
            };
            let result = evaluate_promotion_with_task_outcomes(
                &policy,
                &promotion_context,
                &candidate,
                &predecessor,
                &[passed.clone(), failed.clone(), passed, failed],
            )
            .unwrap();
            let qualifies = carries_verdict && in_scope;
            let count = usize::from(qualifies);
            assert_eq!(
                (result.verified_task_passes, result.verified_task_failures),
                (count, count),
                "{source:?}, production={production}, complete={evidence_complete}, time={created_at}, skill={skill_id}"
            );
            assert_eq!(
                result.decision,
                if qualifies {
                    PromotionDecision::Promote
                } else {
                    PromotionDecision::Hold
                }
            );
            if !qualifies {
                assert!(
                    result
                        .reasons
                        .iter()
                        .any(|reason| reason == "insufficient_verified_task_passes")
                );
            }
        }
    }
}

#[test]
fn turn_latency_keeps_the_slowest_call_even_when_another_call_fails() {
    let mut policy = PromotionPolicy::conservative("v1", 0, 200);
    policy.absolute_p95_latency_us = 500;
    let mut candidate = calls("candidate", 100, 0, 100);
    for event in &mut candidate[..6] {
        event.latency_us = 1_000;
    }
    let mut failed = candidate[0].clone();
    failed.invocation_id = "fast-failure".into();
    failed.outcome = DirectOutcome::Throw;
    failed.latency_us = 1;
    candidate.push(failed);
    let predecessor = calls("predecessor", 100, 0, 100);
    let result = evaluate_promotion(&policy, &context(), &candidate, &predecessor).unwrap();
    assert_eq!(result.candidate.distinct_turns, 100);
    assert_eq!(result.candidate.failures, 1);
    assert_eq!(result.candidate.p95_latency_us, 1_000);
    assert_eq!(result.decision, PromotionDecision::Hold);
    for reason in [
        "absolute_latency_budget_exceeded",
        "relative_latency_budget_exceeded",
    ] {
        assert!(result.reasons.iter().any(|actual| actual == reason));
    }
    candidate.reverse();
    candidate.extend(candidate.clone());
    let replay = evaluate_promotion(&policy, &context(), &candidate, &predecessor).unwrap();
    assert_eq!(replay.canonical_inputs, result.canonical_inputs);
}

#[test]
fn task_outcome_source_tags_preserve_verifier_and_skip_identity() {
    for (source, expected) in [
        (
            TaskOutcomeSource::VerifyCommand("hash".into()),
            serde_json::json!({"kind":"verify_command","id":"hash"}),
        ),
        (
            TaskOutcomeSource::Oracle("oracle".into()),
            serde_json::json!({"kind":"oracle","id":"oracle"}),
        ),
        (
            TaskOutcomeSource::NoVerifyCommand,
            serde_json::json!({"kind":"no_verify_command"}),
        ),
        (
            TaskOutcomeSource::GateSkipped,
            serde_json::json!({"kind":"gate_skipped"}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&source).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<TaskOutcomeSource>(expected).unwrap(),
            source
        );
    }
}
