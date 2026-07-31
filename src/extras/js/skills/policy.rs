//! Versioned, reproducible evidence qualification and promotion policy.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::CapabilityTier;

const ONE_SIDED_95_Z: f64 = 1.644_853_626_951_472_2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectOutcome {
    Success,
    Throw,
    Timeout,
    Oom,
    CapabilityDenied,
}

impl DirectOutcome {
    fn severity(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Throw => 1,
            Self::Timeout => 2,
            Self::Oom => 3,
            Self::CapabilityDenied => 4,
        }
    }

    pub fn is_error(self) -> bool {
        self != Self::Success
    }

    pub fn is_severe(self) -> bool {
        matches!(self, Self::Timeout | Self::Oom | Self::CapabilityDenied)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationEvidence {
    pub invocation_id: String,
    pub skill_id: String,
    pub turn_id: String,
    pub outcome: DirectOutcome,
    pub latency_us: u64,
    pub production: bool,
    pub observability_complete: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionPolicy {
    pub version: String,
    pub min_distinct_turns: usize,
    pub max_observed_error_rate: f64,
    pub non_inferiority_margin: f64,
    pub max_candidate_latency_ratio: f64,
    pub absolute_p95_latency_us: u64,
    pub window_start: i64,
    pub window_end: i64,
}

impl PromotionPolicy {
    pub fn conservative(version: impl Into<String>, window_start: i64, window_end: i64) -> Self {
        Self {
            version: version.into(),
            min_distinct_turns: 25,
            max_observed_error_rate: 0.05,
            non_inferiority_margin: 0.05,
            max_candidate_latency_ratio: 1.25,
            absolute_p95_latency_us: 5_000_000,
            window_start,
            window_end,
        }
    }

    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.version.is_empty()
            || self.min_distinct_turns == 0
            || !(0.0..1.0).contains(&self.max_observed_error_rate)
            || !(0.0..=1.0).contains(&self.non_inferiority_margin)
            || !self.max_candidate_latency_ratio.is_finite()
            || self.max_candidate_latency_ratio < 1.0
            || self.absolute_p95_latency_us == 0
            || self.window_start > self.window_end
        {
            return Err(PolicyError::InvalidConfiguration);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionContext {
    pub candidate_id: String,
    pub predecessor_id: Option<String>,
    pub capability_tier: CapabilityTier,
    pub capability_increased: bool,
    pub inherited_tests_passed: bool,
    pub held_out_tests_passed: bool,
    pub unresolved_negative_feedback: bool,
    pub identity_valid: bool,
    pub row_version_current: bool,
    pub generation_current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionDecision {
    Promote,
    Hold,
    HumanReview,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualifiedStatistics {
    pub distinct_turns: usize,
    pub successes: usize,
    pub failures: usize,
    pub observed_error_rate: f64,
    pub wilson_upper: f64,
    pub p95_latency_us: u64,
    pub severe_faults: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionEvaluation {
    pub policy_version: String,
    pub candidate_id: String,
    pub predecessor_id: Option<String>,
    pub candidate: QualifiedStatistics,
    pub predecessor: Option<QualifiedStatistics>,
    pub decision: PromotionDecision,
    pub reasons: Vec<String>,
    pub canonical_inputs: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("invalid promotion policy configuration")]
    InvalidConfiguration,
    #[error("evidence contains an invocation ID conflict")]
    InvocationConflict,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub fn evaluate_promotion(
    policy: &PromotionPolicy,
    context: &PromotionContext,
    candidate_events: &[InvocationEvidence],
    predecessor_events: &[InvocationEvidence],
) -> Result<PromotionEvaluation, PolicyError> {
    policy.validate()?;
    let candidate = qualify(policy, &context.candidate_id, candidate_events)?;
    let predecessor = context
        .predecessor_id
        .as_deref()
        .map(|id| qualify(policy, id, predecessor_events))
        .transpose()?;

    let mut reasons = Vec::new();
    let mut decision = PromotionDecision::Promote;
    let lineage_root = context.predecessor_id.is_none();
    if lineage_root {
        decision = PromotionDecision::HumanReview;
        reasons.push("lineage_root_requires_second_human_activation".to_string());
    }
    if context.capability_tier == CapabilityTier::SideEffecting || context.capability_increased {
        decision = PromotionDecision::HumanReview;
        reasons.push("capability_requires_human_review".to_string());
    }
    if !context.identity_valid || !context.row_version_current || !context.generation_current {
        decision = PromotionDecision::Hold;
        reasons.push("stale_or_invalid_artifact".to_string());
    }
    if !context.inherited_tests_passed || !context.held_out_tests_passed {
        decision = PromotionDecision::Hold;
        reasons.push("regression_gate_failed".to_string());
    }
    if context.unresolved_negative_feedback {
        decision = PromotionDecision::Hold;
        reasons.push("unresolved_targeted_negative_feedback".to_string());
    }
    // Root activation has no production predecessor and cannot accumulate
    // prompt-time canary evidence. Its separate two-human gate revalidates the
    // artifact and evaluation report without fabricating replacement metrics.
    if !lineage_root {
        if candidate.distinct_turns < policy.min_distinct_turns {
            decision = PromotionDecision::Hold;
            reasons.push("insufficient_distinct_turns".to_string());
        }
        if candidate.observed_error_rate >= policy.max_observed_error_rate {
            decision = PromotionDecision::Hold;
            reasons.push("observed_error_rate_too_high".to_string());
        }
        if candidate.severe_faults > 0 {
            decision = PromotionDecision::Hold;
            reasons.push("severe_fault".to_string());
        }
        if candidate.p95_latency_us > policy.absolute_p95_latency_us {
            decision = PromotionDecision::Hold;
            reasons.push("absolute_latency_budget_exceeded".to_string());
        }
        match &predecessor {
            Some(previous) if previous.distinct_turns >= policy.min_distinct_turns => {
                if candidate.wilson_upper > previous.wilson_upper + policy.non_inferiority_margin {
                    decision = PromotionDecision::Hold;
                    reasons.push("non_inferiority_confidence_failed".to_string());
                }
                let allowed_latency =
                    previous.p95_latency_us as f64 * policy.max_candidate_latency_ratio;
                if candidate.p95_latency_us as f64 > allowed_latency {
                    decision = PromotionDecision::Hold;
                    reasons.push("relative_latency_budget_exceeded".to_string());
                }
            }
            Some(_) => {
                decision = PromotionDecision::Hold;
                reasons.push("sparse_predecessor_evidence".to_string());
            }
            None => {}
        }
    }

    let canonical_inputs = canonical_inputs(policy, context, &candidate, predecessor.as_ref())?;
    Ok(PromotionEvaluation {
        policy_version: policy.version.clone(),
        candidate_id: context.candidate_id.clone(),
        predecessor_id: context.predecessor_id.clone(),
        candidate,
        predecessor,
        decision,
        reasons,
        canonical_inputs,
    })
}

pub fn qualify(
    policy: &PromotionPolicy,
    skill_id: &str,
    events: &[InvocationEvidence],
) -> Result<QualifiedStatistics, PolicyError> {
    let mut by_invocation: BTreeMap<&str, &InvocationEvidence> = BTreeMap::new();
    for event in events.iter().filter(|event| {
        event.skill_id == skill_id
            && event.production
            && event.observability_complete
            && event.created_at >= policy.window_start
            && event.created_at <= policy.window_end
    }) {
        match by_invocation.get(event.invocation_id.as_str()) {
            None => {
                by_invocation.insert(&event.invocation_id, event);
            }
            Some(existing) if *existing == event => {}
            Some(_) => return Err(PolicyError::InvocationConflict),
        }
    }

    // One conservative evidence unit per skill revision per user turn. If a
    // model loops, retain the worst direct outcome and highest latency.
    let mut by_turn: BTreeMap<&str, &InvocationEvidence> = BTreeMap::new();
    for event in by_invocation.values().copied() {
        by_turn
            .entry(&event.turn_id)
            .and_modify(|current| {
                if event.outcome.severity() > current.outcome.severity()
                    || (event.outcome == current.outcome && event.latency_us > current.latency_us)
                {
                    *current = event;
                }
            })
            .or_insert(event);
    }

    let failures = by_turn
        .values()
        .filter(|event| event.outcome.is_error())
        .count();
    let successes = by_turn.len() - failures;
    let severe_faults = by_turn
        .values()
        .filter(|event| event.outcome.is_severe())
        .count();
    let mut latencies: Vec<u64> = by_turn.values().map(|event| event.latency_us).collect();
    latencies.sort_unstable();
    let p95_latency_us = nearest_rank_percentile(&latencies, 95).unwrap_or(0);
    let observed_error_rate = if by_turn.is_empty() {
        1.0
    } else {
        failures as f64 / by_turn.len() as f64
    };
    Ok(QualifiedStatistics {
        distinct_turns: by_turn.len(),
        successes,
        failures,
        observed_error_rate,
        wilson_upper: wilson_upper(failures, by_turn.len()),
        p95_latency_us,
        severe_faults,
    })
}

pub fn nearest_rank_percentile(sorted_values: &[u64], percentile: u32) -> Option<u64> {
    if sorted_values.is_empty() || percentile == 0 || percentile > 100 {
        return None;
    }
    let rank = ((percentile as usize * sorted_values.len()).saturating_add(99) / 100).max(1);
    sorted_values.get(rank - 1).copied()
}

pub fn wilson_upper(failures: usize, total: usize) -> f64 {
    if total == 0 {
        return 1.0;
    }
    let n = total as f64;
    let p = failures as f64 / n;
    let z2 = ONE_SIDED_95_Z * ONE_SIDED_95_Z;
    let center = p + z2 / (2.0 * n);
    let radius = ONE_SIDED_95_Z * ((p * (1.0 - p) / n) + z2 / (4.0 * n * n)).sqrt();
    ((center + radius) / (1.0 + z2 / n)).clamp(0.0, 1.0)
}

fn canonical_inputs(
    policy: &PromotionPolicy,
    context: &PromotionContext,
    candidate: &QualifiedStatistics,
    predecessor: Option<&QualifiedStatistics>,
) -> Result<String, PolicyError> {
    #[derive(Serialize)]
    struct Inputs<'a> {
        schema_version: u32,
        policy: &'a PromotionPolicy,
        context: &'a PromotionContext,
        candidate: &'a QualifiedStatistics,
        predecessor: Option<&'a QualifiedStatistics>,
        gates: BTreeSet<&'static str>,
    }
    let gates = BTreeSet::from([
        "capability",
        "direct_attribution",
        "distinct_turns",
        "held_out",
        "inherited",
        "latency",
        "non_inferiority",
    ]);
    Ok(serde_json::to_string(&Inputs {
        schema_version: 1,
        policy,
        context,
        candidate,
        predecessor,
        gates,
    })?)
}
