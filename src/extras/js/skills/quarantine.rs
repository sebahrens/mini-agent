//! Asymmetric, versioned automatic quarantine decisions.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::coordinator::{
    CoordinatedMutationError, CoordinatorError, IndexCoordinator, PublicationReport,
};
use super::lifecycle::{
    EvidenceSnapshot, LifecycleError, LifecycleStatus, TransitionOutcome, TransitionRequest,
    register_policy_in_tx, transition_in_tx,
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
    if !evidence.row_version_current {
        return QuarantineDecision::Hold("stale_state");
    }
    // A safety-immediate reason must not wait for a pending index generation to
    // be applied: quarantine publishes a removal-only snapshot, which takes
    // effect regardless of publication lag. Leaving a known-unsafe revision
    // available until the next rebuild lands — or until another invocation
    // fails — would contradict the immediate-quarantine contract. A
    // threshold-based decision still holds, because its inputs are only
    // meaningful against the applied generation.
    if !evidence.generation_current && !evidence.reason.is_immediate() {
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
                    // The policy row, the evidence row, and the decision are
                    // one transaction. Writing evidence as an autocommit
                    // statement before the transition opened its own
                    // `BEGIN IMMEDIATE` left `skill_evidence` implying a
                    // decision that a crash or a stale row version never
                    // applied.
                    let policy_json = serde_json::to_string(policy)?;
                    let tx = store
                        .connection_mut()
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    register_policy_in_tx(&tx, &policy.version, &policy_json, created_at)?;
                    tx.execute(
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
                    let outcome = transition_in_tx(&tx, &request, created_at)?;
                    tx.commit()?;
                    let generation = outcome.desired_generation as u64;
                    Ok::<(TransitionOutcome, u64), QuarantineExecutionError>((outcome, generation))
                },
            )
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::skills::embed::Embedder;
    use crate::extras::js::skills::store::SkillStore;
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::paths::AppPaths;
    use std::sync::Arc;

    fn temp_paths() -> (std::path::PathBuf, AppPaths) {
        let root =
            std::env::temp_dir().join(format!("mini-agent-quarantine-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local-data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: None,
        };
        (root, paths)
    }

    fn quarantine_artifact() -> SkillArtifact {
        SkillArtifact::new(
            "function quarantineTarget(_cap, value) { return value; }".to_string(),
            "A revision that the quarantine executor acts on.".to_string(),
            vec!["quarantine".to_string()],
            vec![SkillExport {
                name: "quarantineTarget".to_string(),
                signature: "quarantineTarget(value: string): string".to_string(),
            }],
            vec!["quarantineTarget('x') === 'x'".to_string()],
            CapabilityManifest::pure(),
        )
        .expect("test artifact")
    }

    fn immediate_evidence(skill_id: &str) -> QuarantineEvidence {
        QuarantineEvidence {
            skill_id: skill_id.to_string(),
            reason: QuarantineReason::IdentityMismatch,
            qualified_invocations: 0,
            direct_failures: 0,
            evidence_complete: true,
            authenticated_feedback: false,
            feedback_marked_severe: false,
            row_version_current: true,
            generation_current: true,
        }
    }

    /// `(policy rows, evidence rows)` durably visible to a fresh connection.
    fn durable_rows(paths: &AppPaths, policy_version: &str, evidence_id: &str) -> (i64, i64) {
        let store = SkillStore::open_at(paths).expect("store");
        let policies = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_policy_versions WHERE policy_version = ?",
                [policy_version],
                |row| row.get(0),
            )
            .expect("count policy rows");
        let evidence = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM skill_evidence
                  WHERE evidence_id = ? AND evidence_kind = 'quarantine'",
                [evidence_id],
                |row| row.get(0),
            )
            .expect("count evidence rows");
        (policies, evidence)
    }

    #[test]
    fn a_rejected_quarantine_commits_neither_its_policy_nor_its_evidence() {
        let (root, paths) = temp_paths();
        let artifact = quarantine_artifact();
        {
            let mut store = SkillStore::open_at(&paths).expect("store");
            store.insert_verified(&artifact).expect("insert revision");
            store
                .connection_mut()
                .execute(
                    "UPDATE skill_revisions SET status = 'canary' WHERE id = ?",
                    [&artifact.id],
                )
                .expect("mark the revision as a canary");
        }

        let coordinator = IndexCoordinator::open(
            &paths,
            Arc::new(Embedder::from_config(None).expect("deterministic embedder")),
        )
        .expect("index coordinator");
        let executor = QuarantineExecutor::new(&coordinator);
        let policy = QuarantinePolicy::conservative("quarantine-atomicity-v1");
        let evidence = immediate_evidence(&artifact.id);
        let canonical_snapshot = match evaluate(&policy, &evidence) {
            QuarantineDecision::Quarantine { canonical_snapshot } => canonical_snapshot,
            QuarantineDecision::Hold(reason) => {
                panic!("the fixture must quarantine, it held for {reason}")
            }
        };
        let evidence_id = crate::hex::encode_lower(Sha256::digest(canonical_snapshot.as_bytes()));

        let (generation, row_version) = {
            let store = SkillStore::open_at(&paths).expect("store");
            let state = store.generation_state().expect("generation state");
            let metadata = store
                .metadata(&artifact.id)
                .expect("metadata")
                .expect("revision row");
            (state.desired_generation as i64, metadata.row_version as i64)
        };

        // A stale row version rejects the transition after the policy and the
        // evidence row have already been written inside the same transaction.
        let rejected = executor.apply(
            &policy,
            &evidence,
            LifecycleStatus::Canary,
            row_version + 1,
            generation,
            100,
        );
        assert!(
            rejected.is_err(),
            "a stale row version must reject the quarantine"
        );
        assert_eq!(
            durable_rows(&paths, &policy.version, &evidence_id),
            (0, 0),
            "a rejected quarantine must leave neither a policy row nor evidence for a \
             decision that was never applied"
        );

        // The same decision on the current row version commits all three writes.
        let (outcome, _report) = executor
            .apply(
                &policy,
                &evidence,
                LifecycleStatus::Canary,
                row_version,
                generation,
                101,
            )
            .expect("an applicable quarantine must commit");
        assert_eq!(outcome.status, LifecycleStatus::Quarantined);
        assert_eq!(
            durable_rows(&paths, &policy.version, &evidence_id),
            (1, 1),
            "an applied quarantine must persist its policy and its evidence"
        );

        drop(coordinator);
        let _ = std::fs::remove_dir_all(root);
    }
}
