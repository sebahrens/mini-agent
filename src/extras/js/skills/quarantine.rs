//! Asymmetric, versioned automatic quarantine decisions.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::coordinator::{
    CoordinatedMutationError, CoordinatorError, IndexCoordinator, PublicationReport,
};
use super::lifecycle::{
    EvidenceSnapshot, LifecycleError, LifecycleService, LifecycleStatus, TransitionOutcome,
    TransitionRequest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    IdentityMismatch,
    CapabilityPolicyFault,
    SandboxPolicyFault,
    HeldOutRegression,
    CanaryTimeout,
    CanaryOom,
    UnsafeEmbeddingMetadata,
    AuthenticatedCanarySafetyFeedback,
    AuthenticatedActiveIntegrityFeedback,
    BehavioralFailureRate,
}

impl QuarantineReason {
    pub fn is_immediate(self) -> bool {
        self != Self::BehavioralFailureRate
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineEvidence {
    pub skill_id: String,
    pub reason: QuarantineReason,
    pub qualified_invocations: usize,
    pub direct_failures: usize,
    pub evidence_complete: bool,
    pub authenticated_feedback: bool,
    pub feedback_marked_severe: bool,
    pub row_version_current: bool,
    pub generation_current: bool,
}

/// The feedback row a quarantine decision was derived from.
///
/// [`QuarantineReason`] is chosen from the target's lifecycle status, so it
/// cannot say *what* was reported. Carrying the submitted reason code and the
/// feedback id makes the evidence snapshot traceable back to the exact
/// `skill_feedback` row, and puts the reported code in the transition reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackAttribution {
    pub feedback_id: String,
    pub reason_code: String,
}

impl FeedbackAttribution {
    pub fn new(feedback_id: impl Into<String>, reason_code: impl Into<String>) -> Self {
        Self {
            feedback_id: feedback_id.into(),
            reason_code: reason_code.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantinePolicy {
    pub version: String,
    pub min_behavioral_invocations: usize,
    pub min_behavioral_failures: usize,
}

impl QuarantinePolicy {
    pub fn conservative(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            min_behavioral_invocations: 20,
            min_behavioral_failures: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineDecision {
    Quarantine { canonical_snapshot: String },
    Hold(&'static str),
}

pub fn evaluate(policy: &QuarantinePolicy, evidence: &QuarantineEvidence) -> QuarantineDecision {
    evaluate_with_attribution(policy, evidence, None)
}

/// Evaluate the same policy, recording which feedback row drove the decision.
///
/// The attribution is serialized into the canonical snapshot, so the persisted
/// `skill_evidence` payload names the feedback id and the submitted reason code.
/// It is omitted entirely when absent, so snapshots taken by non-feedback paths
/// keep their existing bytes and evidence ids.
pub fn evaluate_with_attribution(
    policy: &QuarantinePolicy,
    evidence: &QuarantineEvidence,
    attribution: Option<&FeedbackAttribution>,
) -> QuarantineDecision {
    if policy.version.is_empty()
        || policy.min_behavioral_invocations == 0
        || policy.min_behavioral_failures == 0
    {
        return QuarantineDecision::Hold("invalid_policy");
    }
    if !evidence.row_version_current || !evidence.generation_current {
        return QuarantineDecision::Hold("stale_state");
    }
    if !evidence.evidence_complete {
        return QuarantineDecision::Hold("incomplete_evidence");
    }
    let eligible = match evidence.reason {
        QuarantineReason::AuthenticatedCanarySafetyFeedback
        | QuarantineReason::AuthenticatedActiveIntegrityFeedback => {
            evidence.authenticated_feedback && evidence.feedback_marked_severe
        }
        QuarantineReason::BehavioralFailureRate => {
            evidence.qualified_invocations >= policy.min_behavioral_invocations
                && evidence.direct_failures >= policy.min_behavioral_failures
        }
        immediate => immediate.is_immediate(),
    };
    if !eligible {
        return QuarantineDecision::Hold("threshold_not_met");
    }
    #[derive(Serialize)]
    struct Snapshot<'a> {
        schema_version: u32,
        policy: &'a QuarantinePolicy,
        evidence: &'a QuarantineEvidence,
        #[serde(skip_serializing_if = "Option::is_none")]
        feedback: Option<&'a FeedbackAttribution>,
        decision: &'static str,
    }
    match serde_json::to_string(&Snapshot {
        schema_version: 1,
        policy,
        evidence,
        feedback: attribution,
        decision: "quarantine",
    }) {
        Ok(canonical_snapshot) => QuarantineDecision::Quarantine { canonical_snapshot },
        Err(_) => QuarantineDecision::Hold("serialization_failed"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QuarantineExecutionError {
    #[error("quarantine policy held the revision: {0}")]
    Held(&'static str),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error(transparent)]
    Publication(#[from] CoordinatorError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<CoordinatedMutationError<QuarantineExecutionError>> for QuarantineExecutionError {
    fn from(error: CoordinatedMutationError<QuarantineExecutionError>) -> Self {
        match error {
            CoordinatedMutationError::Mutation(error) => error,
            CoordinatedMutationError::Publication(error) => Self::Publication(error),
        }
    }
}

pub struct QuarantineExecutor<'a> {
    coordinator: &'a IndexCoordinator,
}

impl<'a> QuarantineExecutor<'a> {
    pub fn new(coordinator: &'a IndexCoordinator) -> Self {
        Self { coordinator }
    }

    pub fn apply(
        &self,
        policy: &QuarantinePolicy,
        evidence: &QuarantineEvidence,
        from_status: LifecycleStatus,
        expected_row_version: i64,
        expected_generation: i64,
        created_at: i64,
    ) -> Result<(TransitionOutcome, PublicationReport), QuarantineExecutionError> {
        self.apply_with_attribution(
            policy,
            evidence,
            None,
            from_status,
            expected_row_version,
            expected_generation,
            created_at,
        )
    }

    /// Apply a quarantine that a specific feedback row caused.
    ///
    /// The attribution reaches both the canonical evidence snapshot and the
    /// recorded transition reason, so `permission_violation` is no longer
    /// flattened into `authenticatedactiveintegrityfeedback`.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_with_attribution(
        &self,
        policy: &QuarantinePolicy,
        evidence: &QuarantineEvidence,
        attribution: Option<&FeedbackAttribution>,
        from_status: LifecycleStatus,
        expected_row_version: i64,
        expected_generation: i64,
        created_at: i64,
    ) -> Result<(TransitionOutcome, PublicationReport), QuarantineExecutionError> {
        let canonical_snapshot = match evaluate_with_attribution(policy, evidence, attribution) {
            QuarantineDecision::Quarantine { canonical_snapshot } => canonical_snapshot,
            QuarantineDecision::Hold(reason) => return Err(QuarantineExecutionError::Held(reason)),
        };
        if !matches!(
            from_status,
            LifecycleStatus::Canary | LifecycleStatus::Active
        ) {
            return Err(QuarantineExecutionError::Held("ineligible_status"));
        }
        let evidence_id = crate::hex::encode_lower(Sha256::digest(canonical_snapshot.as_bytes()));
        let policy_inputs: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::from([(
                "quarantine".to_string(),
                serde_json::from_str(&canonical_snapshot)?,
            )]);
        let snapshot = EvidenceSnapshot::new(
            evidence.skill_id.clone(),
            None,
            policy.version.clone(),
            vec![evidence_id.clone()],
            policy_inputs,
            expected_row_version,
            None,
            expected_generation,
        )?;
        let mut reason = format!("{:?}", evidence.reason).to_ascii_lowercase();
        if let Some(attribution) = attribution {
            reason.push(':');
            reason.push_str(&attribution.reason_code);
        }
        let request = TransitionRequest {
            idempotency_key: format!("quarantine:{evidence_id}"),
            skill_id: evidence.skill_id.clone(),
            from_status,
            to_status: LifecycleStatus::Quarantined,
            expected_row_version,
            reason,
            snapshot,
        };
        self.coordinator
            .coordinate_removal(
                std::collections::HashSet::from([evidence.skill_id.clone()]),
                |store| {
                    LifecycleService::new(store).register_policy(
                        &policy.version,
                        &serde_json::to_string(policy)?,
                        created_at,
                    )?;
                    // `transition` opens its own `BEGIN IMMEDIATE`, so the
                    // evidence row below is already committed by the time the
                    // transition is evaluated. Remember whether this call is
                    // the one that created it, so a rejected transition does
                    // not leave evidence for a decision that never happened.
                    let inserted_evidence = store.connection_mut().execute(
                        "INSERT OR IGNORE INTO skill_evidence (
                            evidence_id, skill_id, evidence_kind, payload_json,
                            policy_version, created_at
                         ) VALUES (?, ?, 'quarantine', ?, ?, ?)",
                        rusqlite::params![
                            evidence_id,
                            evidence.skill_id,
                            canonical_snapshot,
                            policy.version,
                            created_at,
                        ],
                    )? == 1;
                    match LifecycleService::new(store).transition(&request, created_at) {
                        Ok(outcome) => {
                            let generation = outcome.desired_generation as u64;
                            Ok((outcome, generation))
                        }
                        Err(error) => {
                            if inserted_evidence {
                                discard_unused_evidence(store, &evidence_id);
                            }
                            Err(QuarantineExecutionError::from(error))
                        }
                    }
                },
            )
            .map_err(Into::into)
    }
}

/// Compensate the pre-transition evidence insert when the transition is
/// rejected. Full atomicity needs a `transition` entry point that accepts a
/// caller-owned transaction; until then this keeps `skill_evidence` from
/// implying a decision that was never applied.
fn discard_unused_evidence(store: &mut super::store::SkillStore, evidence_id: &str) {
    match store.connection_mut().execute(
        "DELETE FROM skill_evidence WHERE evidence_id = ? AND evidence_kind = 'quarantine'",
        rusqlite::params![evidence_id],
    ) {
        Ok(_) => {}
        Err(error) => tracing::warn!(
            error = %error,
            "failed to discard quarantine evidence after a rejected transition"
        ),
    }
}
