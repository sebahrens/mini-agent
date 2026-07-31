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
        decision: &'static str,
    }
    match serde_json::to_string(&Snapshot {
        schema_version: 1,
        policy,
        evidence,
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
        let canonical_snapshot = match evaluate(policy, evidence) {
            QuarantineDecision::Quarantine { canonical_snapshot } => canonical_snapshot,
            QuarantineDecision::Hold(reason) => return Err(QuarantineExecutionError::Held(reason)),
        };
        if !matches!(
            from_status,
            LifecycleStatus::Canary | LifecycleStatus::Active
        ) {
            return Err(QuarantineExecutionError::Held("ineligible_status"));
        }
        let evidence_id = format!("{:x}", Sha256::digest(canonical_snapshot.as_bytes()));
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
        let request = TransitionRequest {
            idempotency_key: format!("quarantine:{evidence_id}"),
            skill_id: evidence.skill_id.clone(),
            from_status,
            to_status: LifecycleStatus::Quarantined,
            expected_row_version,
            reason: format!("{:?}", evidence.reason).to_ascii_lowercase(),
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
                    store.connection_mut().execute(
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
                    )?;
                    let outcome = LifecycleService::new(store).transition(&request, created_at)?;
                    let generation = outcome.desired_generation as u64;
                    Ok((outcome, generation))
                },
            )
            .map_err(Into::into)
    }
}
