//! Typed, transactional lifecycle boundary for immutable skill revisions.
//!
//! All Phase 5 status mutations flow through this module. Policy modules decide
//! *whether* a transition is justified; this service revalidates the exact row,
//! canonical evidence snapshot, policy version, and generation before applying
//! it atomically.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::coordinator::{
    CoordinatedMutationError, CoordinatorError, IndexCoordinator, PublicationReport,
};
#[cfg(test)]
use super::policy::{
    DirectOutcome, InvocationEvidence, PromotionContext, PromotionDecision, PromotionPolicy,
    TaskOutcomeEvidence, TaskOutcomeSource, evaluate_promotion_with_task_outcomes,
};
use super::store::{ApprovalAuthorizationRequest, approval_manifest_digest};
use super::store::{ApprovalTransition, SkillStore, StoreError, consume_approval_authorization};

const APPROVAL_AUTHORIZATION_LIFETIME_SECONDS: i64 = 300;

/// Version of the canonical lifecycle evidence encoding.
pub const EVIDENCE_SNAPSHOT_VERSION: u32 = 1;

/// Durable lifecycle values. Wire tokens are explicit because Debug output is
/// not a persistence contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Pending,
    Verified,
    Canary,
    Active,
    Quarantined,
    Superseded,
    Retired,
    Rejected,
}

impl LifecycleStatus {
    pub const ALL: [Self; 8] = [
        Self::Pending,
        Self::Verified,
        Self::Canary,
        Self::Active,
        Self::Quarantined,
        Self::Superseded,
        Self::Retired,
        Self::Rejected,
    ];

    pub fn as_token(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Canary => "canary",
            Self::Active => "active",
            Self::Quarantined => "quarantined",
            Self::Superseded => "superseded",
            Self::Retired => "retired",
            Self::Rejected => "rejected",
        }
    }

    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_token() == value)
    }

    /// Structural transition graph. Higher-level authorization remains the
    /// responsibility of the policy-specific service method.
    pub fn may_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Verified)
                | (Self::Pending, Self::Rejected)
                | (Self::Pending, Self::Quarantined)
                | (Self::Verified, Self::Canary)
                | (Self::Verified, Self::Rejected)
                | (Self::Verified, Self::Quarantined)
                | (Self::Canary, Self::Active)
                | (Self::Canary, Self::Rejected)
                | (Self::Canary, Self::Quarantined)
                | (Self::Active, Self::Superseded)
                | (Self::Active, Self::Quarantined)
                | (Self::Active, Self::Retired)
                | (Self::Superseded, Self::Active)
                | (Self::Superseded, Self::Quarantined)
                | (Self::Superseded, Self::Retired)
                | (Self::Quarantined, Self::Retired)
        )
    }
}

impl std::fmt::Display for LifecycleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_token())
    }
}

/// Exact inputs bound to a lifecycle decision.
///
/// `BTreeMap` and sorted evidence IDs make the serialized bytes deterministic
/// across processes and retries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceSnapshot {
    pub schema_version: u32,
    pub artifact_id: String,
    pub predecessor_id: Option<String>,
    pub policy_version: String,
    pub evidence_ids: Vec<String>,
    pub policy_inputs: BTreeMap<String, serde_json::Value>,
    pub artifact_row_version: i64,
    pub predecessor_row_version: Option<i64>,
    pub index_generation: i64,
}

impl EvidenceSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        artifact_id: impl Into<String>,
        predecessor_id: Option<String>,
        policy_version: impl Into<String>,
        evidence_ids: Vec<String>,
        policy_inputs: BTreeMap<String, serde_json::Value>,
        artifact_row_version: i64,
        predecessor_row_version: Option<i64>,
        index_generation: i64,
    ) -> Result<Self, LifecycleError> {
        let mut snapshot = Self {
            schema_version: EVIDENCE_SNAPSHOT_VERSION,
            artifact_id: artifact_id.into(),
            predecessor_id,
            policy_version: policy_version.into(),
            evidence_ids,
            policy_inputs,
            artifact_row_version,
            predecessor_row_version,
            index_generation,
        };
        snapshot.evidence_ids.sort();
        if snapshot.evidence_ids.windows(2).any(|ids| ids[0] == ids[1]) {
            return Err(LifecycleError::DuplicateEvidenceId);
        }
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), LifecycleError> {
        if self.schema_version != EVIDENCE_SNAPSHOT_VERSION {
            return Err(LifecycleError::UnsupportedEvidenceSnapshot(
                self.schema_version,
            ));
        }
        if self.artifact_id.is_empty()
            || self.policy_version.is_empty()
            || self.artifact_row_version < 1
            || self.index_generation < 0
        {
            return Err(LifecycleError::InvalidEvidenceSnapshot);
        }
        if self.predecessor_id.is_some() != self.predecessor_row_version.is_some() {
            return Err(LifecycleError::InvalidEvidenceSnapshot);
        }
        if self
            .predecessor_row_version
            .is_some_and(|version| version < 1)
        {
            return Err(LifecycleError::InvalidEvidenceSnapshot);
        }
        let unique: BTreeSet<&str> = self.evidence_ids.iter().map(String::as_str).collect();
        if unique.len() != self.evidence_ids.len() {
            return Err(LifecycleError::DuplicateEvidenceId);
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<String, LifecycleError> {
        self.validate()?;
        Ok(serde_json::to_string(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionState {
    pub id: String,
    pub status: LifecycleStatus,
    pub supersedes_id: Option<String>,
    pub superseded_by_id: Option<String>,
    pub lineage_root_id: String,
    pub row_version: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct TransitionRequest {
    pub idempotency_key: String,
    pub skill_id: String,
    pub from_status: LifecycleStatus,
    pub to_status: LifecycleStatus,
    pub expected_row_version: i64,
    pub reason: String,
    pub snapshot: EvidenceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionOutcome {
    pub transition_id: i64,
    pub skill_id: String,
    pub status: LifecycleStatus,
    pub row_version: i64,
    pub desired_generation: i64,
    pub replayed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ReplacementTransitionRequest {
    pub idempotency_key: String,
    pub candidate_id: String,
    pub predecessor_id: String,
    pub candidate_row_version: i64,
    pub predecessor_row_version: i64,
    pub reason: String,
    pub snapshot: EvidenceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementTransitionOutcome {
    pub candidate_status: LifecycleStatus,
    pub predecessor_status: LifecycleStatus,
    pub candidate_row_version: i64,
    pub predecessor_row_version: i64,
    pub desired_generation: i64,
    pub replayed: bool,
}

/// How a replacement promotion is authorized.
///
/// `EvidenceThreshold` revalidates the durable promotion policy against stored
/// invocation and task-outcome evidence inside the transaction. `LocalOwner` is
/// the explicit operator surface: it deliberately does not consult the
/// evidence-threshold policy and instead consumes a single-use approval
/// authority bound to a *second*, distinct local-owner action — exactly the
/// property lineage-root activation enforces.
#[derive(Debug, Clone, Copy)]
enum PromotionAuthority<'a> {
    #[cfg(test)]
    EvidenceThreshold,
    LocalOwner {
        approval: &'a HumanApproval,
        authorization: &'a super::store::ApprovalAuthorization,
    },
}

#[derive(Debug, Clone)]
pub struct HumanApproval {
    approval_id: String,
    actor_id: String,
    evaluation_report_id: String,
    expected_row_version: i64,
}

impl HumanApproval {
    /// Create the fixed local-owner approval used only by the explicit CLI
    /// lifecycle surface. Application-data directory ownership is the local
    /// authentication boundary.
    pub(crate) fn local_owner(
        evaluation_report_id: impl Into<String>,
        expected_row_version: i64,
    ) -> Result<Self, LifecycleError> {
        let approval = Self {
            approval_id: format!("local-owner-activation-{}", uuid::Uuid::new_v4()),
            actor_id: "local-owner".to_string(),
            evaluation_report_id: evaluation_report_id.into(),
            expected_row_version,
        };
        validate_human_approval(&approval)?;
        Ok(approval)
    }

    /// Test-only stand-in for an opaque approval produced by the parent authentication adapter.
    #[cfg(test)]
    pub(crate) fn verified(
        approval_id: impl Into<String>,
        actor_id: impl Into<String>,
        evaluation_report_id: impl Into<String>,
        expected_row_version: i64,
    ) -> Result<Self, LifecycleError> {
        let approval = Self {
            approval_id: approval_id.into(),
            actor_id: actor_id.into(),
            evaluation_report_id: evaluation_report_id.into(),
            expected_row_version,
        };
        validate_human_approval(&approval)?;
        Ok(approval)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("unknown lifecycle status in storage: {0}")]
    UnknownStatus(String),
    #[error("illegal lifecycle transition for the {role} {skill_id}: {from} -> {to}")]
    IllegalTransition {
        /// Which side of the transition was wrong: `revision`, `candidate` or
        /// `predecessor`. Collapsing the two sides of a replacement into one
        /// message left an operator unable to tell them apart.
        role: &'static str,
        skill_id: String,
        from: LifecycleStatus,
        to: LifecycleStatus,
    },
    #[error("stale row version for {skill_id}: expected {expected}, actual {actual}")]
    StaleRowVersion {
        skill_id: String,
        expected: i64,
        actual: i64,
    },
    #[error("stale index generation: expected {expected}, actual {actual}")]
    StaleGeneration { expected: i64, actual: i64 },
    #[error("evidence snapshot does not match the transition request")]
    EvidenceMismatch,
    #[error("duplicate evidence identifier")]
    DuplicateEvidenceId,
    #[error("unsupported evidence snapshot version: {0}")]
    UnsupportedEvidenceSnapshot(u32),
    #[error("invalid evidence snapshot")]
    InvalidEvidenceSnapshot,
    #[error("policy version is not registered: {0}")]
    UnknownPolicyVersion(String),
    #[error("evidence snapshot references missing or mismatched durable evidence")]
    UnknownEvidence,
    #[error("idempotency key was already used for a different transition")]
    IdempotencyConflict,
    #[error("lineage cycle detected")]
    LineageCycle,
    #[error("lineage fork detected")]
    LineageFork,
    #[error("revision is terminally rejected")]
    RejectedIsTerminal,
    #[error("authenticated human approval is missing, stale, or invalid")]
    InvalidHumanApproval,
    #[error(
        "authenticated human approval does not match {field} for {skill_id}: \
         expected {expected}, observed {observed}"
    )]
    ApprovalMismatch {
        skill_id: String,
        /// The precondition that failed: `status`, `row_version`,
        /// `evaluation_report_id` or `first_approval`.
        field: &'static str,
        expected: String,
        observed: String,
    },
    #[error("lineage-root activation was attempted on a replacement")]
    NotLineageRoot,
    #[error("privileged admission/activation/supersession requires its dedicated atomic service")]
    PrivilegedTransition,
    #[cfg(test)]
    #[error("stored evidence does not qualify this replacement for promotion: {0}")]
    PromotionHeld(String),
}

#[derive(Debug, thiserror::Error)]
pub enum LifecyclePublicationError {
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error(transparent)]
    Publication(#[from] CoordinatorError),
}

impl From<CoordinatedMutationError<LifecycleError>> for LifecyclePublicationError {
    fn from(error: CoordinatedMutationError<LifecycleError>) -> Self {
        match error {
            CoordinatedMutationError::Mutation(error) => Self::Lifecycle(error),
            CoordinatedMutationError::Publication(error) => Self::Publication(error),
        }
    }
}

/// Production lifecycle façade. It keeps lifecycle commit and immutable-index
/// publication behind the same new-turn gate.
pub struct CoordinatedLifecycle<'a> {
    coordinator: &'a IndexCoordinator,
}

impl<'a> CoordinatedLifecycle<'a> {
    pub fn new(coordinator: &'a IndexCoordinator) -> Self {
        Self { coordinator }
    }

    #[cfg(test)]
    pub(crate) fn promote_replacement(
        &self,
        request: &ReplacementTransitionRequest,
        created_at: i64,
    ) -> Result<(ReplacementTransitionOutcome, PublicationReport), LifecyclePublicationError> {
        let removed = HashSet::from([request.predecessor_id.clone()]);
        self.coordinator
            .coordinate_mutation(removed, |store| {
                let outcome =
                    LifecycleService::new(store).promote_replacement(request, created_at)?;
                let generation = outcome.desired_generation as u64;
                Ok((outcome, generation))
            })
            .map_err(Into::into)
    }

    /// Coordinated counterpart of
    /// [`LifecycleService::promote_replacement_by_local_owner`]. It runs behind
    /// the same new-turn gate and republishes the immutable index exactly as
    /// root activation does.
    pub(crate) fn promote_replacement_by_local_owner(
        &self,
        request: &ReplacementTransitionRequest,
        approval: &HumanApproval,
        authorization: &super::store::ApprovalAuthorization,
        predecessor_from: LifecycleStatus,
        created_at: i64,
    ) -> Result<(ReplacementTransitionOutcome, PublicationReport), LifecyclePublicationError> {
        let removed = HashSet::from([request.predecessor_id.clone()]);
        self.coordinator
            .coordinate_mutation(removed, |store| {
                let outcome = LifecycleService::new(store).promote_replacement_by_local_owner(
                    request,
                    approval,
                    authorization,
                    predecessor_from,
                    created_at,
                )?;
                let generation = outcome.desired_generation as u64;
                Ok((outcome, generation))
            })
            .map_err(Into::into)
    }

    #[cfg(test)]
    pub(crate) fn rollback_replacement(
        &self,
        request: &ReplacementTransitionRequest,
        created_at: i64,
    ) -> Result<(ReplacementTransitionOutcome, PublicationReport), LifecyclePublicationError> {
        let removed = HashSet::from([request.candidate_id.clone()]);
        self.coordinator
            .coordinate_mutation(removed, |store| {
                let outcome =
                    LifecycleService::new(store).rollback_replacement(request, created_at)?;
                let generation = outcome.desired_generation as u64;
                Ok((outcome, generation))
            })
            .map_err(Into::into)
    }

    pub(crate) fn activate_root(
        &self,
        idempotency_key: &str,
        skill_id: &str,
        approval: &HumanApproval,
        authorization: &super::store::ApprovalAuthorization,
        snapshot: &EvidenceSnapshot,
        created_at: i64,
    ) -> Result<(TransitionOutcome, PublicationReport), LifecyclePublicationError> {
        self.coordinator
            .coordinate_mutation(HashSet::new(), |store| {
                let outcome = LifecycleService::new(store).activate_root(
                    idempotency_key,
                    skill_id,
                    approval,
                    authorization,
                    snapshot,
                    created_at,
                )?;
                let generation = outcome.desired_generation as u64;
                Ok((outcome, generation))
            })
            .map_err(Into::into)
    }
}

/// Sole low-level service allowed to mutate lifecycle state.
pub struct LifecycleService<'a> {
    store: &'a mut SkillStore,
}

impl<'a> LifecycleService<'a> {
    /// Persist a short-lived authorization for the fixed local-owner CLI
    /// adapter. Keeping construction here preserves the opaque binding between
    /// approval, artifact, report, and transition.
    pub(crate) fn authorize_root_local_owner(
        &mut self,
        skill_id: &str,
        approval: &HumanApproval,
        issued_at: i64,
    ) -> Result<super::store::ApprovalAuthorization, LifecycleError> {
        self.authorize_local_owner(skill_id, approval, issued_at)
    }

    /// Persist the same short-lived, single-use local-owner authority for the
    /// explicit operator replacement-promotion surface. The store still pins
    /// the fixed local-owner principal and the canary -> active transition, and
    /// the authority is consumed inside the promotion transaction.
    pub(crate) fn authorize_replacement_local_owner(
        &mut self,
        skill_id: &str,
        approval: &HumanApproval,
        issued_at: i64,
    ) -> Result<super::store::ApprovalAuthorization, LifecycleError> {
        self.authorize_local_owner(skill_id, approval, issued_at)
    }

    fn authorize_local_owner(
        &mut self,
        skill_id: &str,
        approval: &HumanApproval,
        issued_at: i64,
    ) -> Result<super::store::ApprovalAuthorization, LifecycleError> {
        validate_human_approval(approval)?;
        let artifact = self
            .store
            .get(skill_id)?
            .ok_or_else(|| StoreError::NotFound(skill_id.to_string()))?;
        let expires_at = issued_at
            .checked_add(APPROVAL_AUTHORIZATION_LIFETIME_SECONDS)
            .ok_or(LifecycleError::InvalidHumanApproval)?;
        Ok(self
            .store
            .issue_root_approval_authorization(ApprovalAuthorizationRequest {
                authorization_id: approval.approval_id.clone(),
                principal: approval.actor_id.clone(),
                artifact_id: skill_id.to_string(),
                report_id: approval.evaluation_report_id.clone(),
                manifest_digest: approval_manifest_digest(&artifact)?,
                transition: ApprovalTransition::CanaryToActive,
                issued_at,
                expires_at,
            })?)
    }

    /// Test-only stand-in for the separate parent authentication interaction.
    #[cfg(test)]
    pub(crate) fn authorize_root_for_test(
        &mut self,
        skill_id: &str,
        approval: &HumanApproval,
        issued_at: i64,
    ) -> Result<super::store::ApprovalAuthorization, LifecycleError> {
        validate_human_approval(approval)?;
        let artifact = self
            .store
            .get(skill_id)?
            .ok_or_else(|| StoreError::NotFound(skill_id.to_string()))?;
        let expires_at = issued_at
            .checked_add(APPROVAL_AUTHORIZATION_LIFETIME_SECONDS)
            .ok_or(LifecycleError::InvalidHumanApproval)?;
        Ok(self
            .store
            .issue_approval_authorization_for_test(ApprovalAuthorizationRequest {
                authorization_id: approval.approval_id.clone(),
                principal: approval.actor_id.clone(),
                artifact_id: skill_id.to_string(),
                report_id: approval.evaluation_report_id.clone(),
                manifest_digest: approval_manifest_digest(&artifact)?,
                transition: ApprovalTransition::CanaryToActive,
                issued_at,
                expires_at,
            })?)
    }

    pub fn new(store: &'a mut SkillStore) -> Self {
        Self { store }
    }

    pub fn revision(&self, skill_id: &str) -> Result<RevisionState, LifecycleError> {
        read_revision(self.store.connection(), skill_id)
    }

    // Test-only: production reads generations through the index coordinator.
    #[cfg(test)]
    pub fn index_generations(&self) -> Result<(i64, i64), LifecycleError> {
        Ok(self.store.connection().query_row(
            "SELECT desired_generation, applied_generation
             FROM skill_generations WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    pub(crate) fn register_policy(
        &mut self,
        policy_version: &str,
        canonical_policy_json: &str,
        created_at: i64,
    ) -> Result<(), LifecycleError> {
        let parsed: serde_json::Value = serde_json::from_str(canonical_policy_json)?;
        let canonical = serde_json::to_string(&parsed)?;
        let changed = self.store.connection_mut().execute(
            "INSERT OR IGNORE INTO skill_policy_versions
                (policy_version, policy_json, created_at)
             VALUES (?, ?, ?)",
            params![policy_version, canonical, created_at],
        )?;
        if changed == 0 {
            let existing: String = self.store.connection().query_row(
                "SELECT policy_json FROM skill_policy_versions WHERE policy_version = ?",
                [policy_version],
                |row| row.get(0),
            )?;
            if existing != canonical {
                return Err(LifecycleError::IdempotencyConflict);
            }
        }
        Ok(())
    }

    /// Record the operator's own promotion evidence.
    ///
    /// The row is the durable trace of one explicit local-owner promotion
    /// action. It is stored under the dedicated `operator_promotion` kind so it
    /// can never be mistaken for — or substituted into — the `qualified`
    /// evidence that the evidence-threshold promotion policy consumes.
    pub(crate) fn record_operator_promotion_evidence(
        &mut self,
        evidence_id: &str,
        skill_id: &str,
        policy_version: &str,
        payload_json: &str,
        created_at: i64,
    ) -> Result<(), LifecycleError> {
        let parsed: serde_json::Value = serde_json::from_str(payload_json)?;
        let canonical = serde_json::to_string(&parsed)?;
        self.store.connection_mut().execute(
            "INSERT OR IGNORE INTO skill_evidence (
                evidence_id, skill_id, evidence_kind, payload_json,
                policy_version, created_at
             ) VALUES (?, ?, 'operator_promotion', ?, ?, ?)",
            params![evidence_id, skill_id, canonical, policy_version, created_at],
        )?;
        Ok(())
    }

    /// Exercise one lifecycle transition in its own `BEGIN IMMEDIATE` in tests.
    ///
    /// Callers that must write policy or evidence rows atomically with the
    /// decision own the transaction themselves and use
    /// [`register_policy_in_tx`] plus [`transition_in_tx`] instead.
    #[cfg(test)]
    pub(crate) fn transition(
        &mut self,
        request: &TransitionRequest,
        created_at: i64,
    ) -> Result<TransitionOutcome, LifecycleError> {
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = transition_in_tx(&tx, request, created_at)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Record Phase 4's first authenticated approval after the unchanged
    /// verified artifact has entered non-retrievable root canary.
    #[cfg(test)]
    pub(crate) fn record_root_canary_approval(
        &mut self,
        skill_id: &str,
        approval: &HumanApproval,
        created_at: i64,
    ) -> Result<(), LifecycleError> {
        validate_human_approval(approval)?;
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision = read_revision(&tx, skill_id)?;
        let report: Option<String> = tx
            .query_row(
                "SELECT evaluation_report_id FROM skill_revisions WHERE id = ?",
                [skill_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if revision.status != LifecycleStatus::Canary {
            return Err(approval_mismatch(
                skill_id,
                "status",
                LifecycleStatus::Canary.as_token(),
                revision.status.as_token(),
            ));
        }
        if revision.supersedes_id.is_some() {
            return Err(approval_mismatch(
                skill_id,
                "supersedes_id",
                "none",
                revision.supersedes_id.as_deref().unwrap_or("none"),
            ));
        }
        if revision.row_version != approval.expected_row_version {
            return Err(approval_mismatch(
                skill_id,
                "row_version",
                approval.expected_row_version,
                revision.row_version,
            ));
        }
        if report.as_deref() != Some(approval.evaluation_report_id.as_str()) {
            return Err(approval_mismatch(
                skill_id,
                "evaluation_report_id",
                &approval.evaluation_report_id,
                report.as_deref().unwrap_or("none"),
            ));
        }
        insert_approval(&tx, skill_id, "phase4_canary", approval, created_at)?;
        tx.commit()?;
        Ok(())
    }

    /// Activate a lineage-root canary only after a distinct second authenticated
    /// human action. No predecessor or non-inferiority evidence is fabricated.
    pub(crate) fn activate_root(
        &mut self,
        idempotency_key: &str,
        skill_id: &str,
        approval: &HumanApproval,
        authorization: &super::store::ApprovalAuthorization,
        snapshot: &EvidenceSnapshot,
        created_at: i64,
    ) -> Result<TransitionOutcome, LifecycleError> {
        validate_human_approval(approval)?;
        if !authorization.binds_approval(&approval.approval_id, &approval.actor_id) {
            return Err(StoreError::Unauthorized.into());
        }
        if idempotency_key.is_empty()
            || snapshot.artifact_id != skill_id
            || snapshot.predecessor_id.is_some()
            || snapshot.predecessor_row_version.is_some()
            || snapshot.artifact_row_version != approval.expected_row_version
        {
            return Err(LifecycleError::EvidenceMismatch);
        }
        snapshot.validate()?;
        let request = TransitionRequest {
            idempotency_key: idempotency_key.to_string(),
            skill_id: skill_id.to_string(),
            from_status: LifecycleStatus::Canary,
            to_status: LifecycleStatus::Active,
            expected_row_version: approval.expected_row_version,
            reason: "second_authenticated_root_activation".to_string(),
            snapshot: snapshot.clone(),
        };
        {
            let tx = self
                .store
                .connection_mut()
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(replayed) = read_idempotent_root_transition(&tx, &request, approval)? {
                tx.commit()?;
                return Ok(replayed);
            }
            tx.commit()?;
        }
        let artifact = self
            .store
            .get(skill_id)?
            .ok_or_else(|| LifecycleError::Store(StoreError::NotFound(skill_id.to_string())))?;
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(replayed) = read_idempotent_root_transition(&tx, &request, approval)? {
            tx.commit()?;
            return Ok(replayed);
        }
        ensure_policy_exists(&tx, &snapshot.policy_version)?;
        ensure_snapshot_evidence(&tx, snapshot, false, None)?;
        let revision = read_revision(&tx, skill_id)?;
        if revision.supersedes_id.is_some() || revision.lineage_root_id != revision.id {
            return Err(LifecycleError::NotLineageRoot);
        }
        if revision.status != LifecycleStatus::Canary {
            return Err(approval_mismatch(
                skill_id,
                "status",
                LifecycleStatus::Canary.as_token(),
                revision.status.as_token(),
            ));
        }
        if revision.row_version != approval.expected_row_version {
            return Err(approval_mismatch(
                skill_id,
                "row_version",
                approval.expected_row_version,
                revision.row_version,
            ));
        }
        let report: Option<String> = tx
            .query_row(
                "SELECT evaluation_report_id FROM skill_revisions WHERE id = ?",
                [skill_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if report.as_deref() != Some(approval.evaluation_report_id.as_str()) {
            return Err(approval_mismatch(
                skill_id,
                "evaluation_report_id",
                &approval.evaluation_report_id,
                report.as_deref().unwrap_or("none"),
            ));
        }
        let first_approval: Option<String> = tx
            .query_row(
                "SELECT approval_id FROM skill_lifecycle_approvals
                 WHERE skill_id = ? AND approval_kind = 'phase4_canary'",
                [skill_id],
                |row| row.get(0),
            )
            .optional()?;
        // Activation is the *second* authenticated action, so a missing first
        // approval and a replayed one are different failures.
        if first_approval
            .as_deref()
            .is_none_or(|first| first == approval.approval_id)
        {
            return Err(approval_mismatch(
                skill_id,
                "first_approval",
                "a distinct earlier phase4_canary approval",
                first_approval.as_deref().unwrap_or("none"),
            ));
        }
        consume_approval_authorization(
            &tx,
            authorization,
            &artifact,
            &approval.evaluation_report_id,
            ApprovalTransition::CanaryToActive,
            created_at,
        )?;
        insert_approval(
            &tx,
            skill_id,
            "phase5_root_activation",
            approval,
            created_at,
        )?;
        let desired: i64 = tx.query_row(
            "SELECT desired_generation FROM skill_generations WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if desired != snapshot.index_generation {
            return Err(LifecycleError::StaleGeneration {
                expected: snapshot.index_generation,
                actual: desired,
            });
        }
        let next_generation = desired + 1;
        let next_row_version = revision.row_version + 1;
        tx.execute(
            "UPDATE skill_revisions
             SET status = 'active', row_version = ?, updated_at = ?
             WHERE id = ? AND status = 'canary' AND row_version = ?",
            params![next_row_version, created_at, skill_id, revision.row_version],
        )?;
        tx.execute(
            "UPDATE skill_generations SET desired_generation = ?, updated_at = ?
             WHERE singleton = 1 AND desired_generation = ?",
            params![next_generation, created_at, desired],
        )?;
        tx.execute(
            "INSERT INTO skill_transitions (
                idempotency_key, skill_id, predecessor_id, from_status,
                to_status, reason, evidence_snapshot, policy_version,
                row_version_from, row_version_to, desired_generation, created_at
             ) VALUES (?, ?, NULL, 'canary', 'active', ?, ?, ?, ?, ?, ?, ?)",
            params![
                idempotency_key,
                skill_id,
                request.reason,
                snapshot.canonical_json()?,
                snapshot.policy_version,
                revision.row_version,
                next_row_version,
                next_generation,
                created_at,
            ],
        )?;
        let transition_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(TransitionOutcome {
            transition_id,
            skill_id: skill_id.to_string(),
            status: LifecycleStatus::Active,
            row_version: next_row_version,
            desired_generation: next_generation,
            replayed: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn promote_replacement(
        &mut self,
        request: &ReplacementTransitionRequest,
        created_at: i64,
    ) -> Result<ReplacementTransitionOutcome, LifecycleError> {
        self.replace_pair(
            request,
            LifecycleStatus::Canary,
            LifecycleStatus::Active,
            LifecycleStatus::Active,
            LifecycleStatus::Superseded,
            PromotionAuthority::EvidenceThreshold,
            created_at,
        )
    }

    /// Promote an approved replacement canary by explicit local-owner action.
    ///
    /// `predecessor_from` is restricted to `Active` (the ordinary replacement)
    /// or `Quarantined` (the emergency path, where the defective predecessor
    /// was quarantined before a replacement existed). Every other predecessor
    /// state is refused, and no other transition in this module is loosened.
    pub(crate) fn promote_replacement_by_local_owner(
        &mut self,
        request: &ReplacementTransitionRequest,
        approval: &HumanApproval,
        authorization: &super::store::ApprovalAuthorization,
        predecessor_from: LifecycleStatus,
        created_at: i64,
    ) -> Result<ReplacementTransitionOutcome, LifecycleError> {
        if !matches!(
            predecessor_from,
            LifecycleStatus::Active | LifecycleStatus::Quarantined
        ) {
            return Err(LifecycleError::IllegalTransition {
                role: "predecessor",
                skill_id: request.predecessor_id.clone(),
                from: predecessor_from,
                to: LifecycleStatus::Superseded,
            });
        }
        self.replace_pair(
            request,
            LifecycleStatus::Canary,
            LifecycleStatus::Active,
            predecessor_from,
            LifecycleStatus::Superseded,
            PromotionAuthority::LocalOwner {
                approval,
                authorization,
            },
            created_at,
        )
    }

    #[cfg(test)]
    pub(crate) fn rollback_replacement(
        &mut self,
        request: &ReplacementTransitionRequest,
        created_at: i64,
    ) -> Result<ReplacementTransitionOutcome, LifecycleError> {
        self.replace_pair(
            request,
            LifecycleStatus::Active,
            LifecycleStatus::Quarantined,
            LifecycleStatus::Superseded,
            LifecycleStatus::Active,
            PromotionAuthority::EvidenceThreshold,
            created_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn replace_pair(
        &mut self,
        request: &ReplacementTransitionRequest,
        candidate_from: LifecycleStatus,
        candidate_to: LifecycleStatus,
        predecessor_from: LifecycleStatus,
        predecessor_to: LifecycleStatus,
        authority: PromotionAuthority<'_>,
        created_at: i64,
    ) -> Result<ReplacementTransitionOutcome, LifecycleError> {
        if request.idempotency_key.is_empty()
            || request.reason.is_empty()
            || request.snapshot.evidence_ids.is_empty()
            || request.snapshot.policy_inputs.is_empty()
            || request.snapshot.artifact_id != request.candidate_id
            || request.snapshot.predecessor_id.as_deref() != Some(request.predecessor_id.as_str())
            || request.snapshot.artifact_row_version != request.candidate_row_version
            || request.snapshot.predecessor_row_version != Some(request.predecessor_row_version)
        {
            return Err(LifecycleError::EvidenceMismatch);
        }
        request.snapshot.validate()?;
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let replay_key = format!("{}:candidate", request.idempotency_key);
        if let Some(existing) = tx
            .query_row(
                "SELECT row_version_to, desired_generation, to_status
                 FROM skill_transitions WHERE idempotency_key = ?",
                [&replay_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        {
            let canonical = request.snapshot.canonical_json()?;
            let candidate_exact: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM skill_transitions
                    WHERE idempotency_key = ? AND skill_id = ?
                      AND predecessor_id = ? AND from_status = ? AND to_status = ?
                      AND reason = ? AND evidence_snapshot = ?
                      AND policy_version = ? AND row_version_from = ?
                      AND row_version_to = ? AND desired_generation = ?
                 )",
                params![
                    replay_key,
                    request.candidate_id,
                    request.predecessor_id,
                    candidate_from.as_token(),
                    candidate_to.as_token(),
                    request.reason,
                    canonical,
                    request.snapshot.policy_version,
                    request.candidate_row_version,
                    request.candidate_row_version + 1,
                    existing.1,
                ],
                |row| row.get(0),
            )?;
            let predecessor_version: Option<i64> = tx
                .query_row(
                    "SELECT row_version_to FROM skill_transitions
                     WHERE idempotency_key = ? AND skill_id = ?
                       AND predecessor_id = ? AND from_status = ? AND to_status = ?
                       AND reason = ? AND evidence_snapshot = ?
                       AND policy_version = ? AND row_version_from = ?
                       AND desired_generation = ?",
                    params![
                        format!("{}:predecessor", request.idempotency_key),
                        request.predecessor_id,
                        request.predecessor_id,
                        predecessor_from.as_token(),
                        predecessor_to.as_token(),
                        request.reason,
                        canonical,
                        request.snapshot.policy_version,
                        request.predecessor_row_version,
                        existing.1,
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.2 != candidate_to.as_token()
                || !candidate_exact
                || predecessor_version != Some(request.predecessor_row_version + 1)
            {
                return Err(LifecycleError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(ReplacementTransitionOutcome {
                candidate_status: candidate_to,
                predecessor_status: predecessor_to,
                candidate_row_version: existing.0,
                predecessor_row_version: request.predecessor_row_version + 1,
                desired_generation: existing.1,
                replayed: true,
            });
        }

        ensure_policy_exists(&tx, &request.snapshot.policy_version)?;
        let promoting = candidate_to == LifecycleStatus::Active
            && predecessor_to == LifecycleStatus::Superseded;
        // An explicit operator promotion carries its own dedicated evidence
        // kind; it must never satisfy — or be satisfied by — the `qualified`
        // evidence the evidence-threshold policy path requires.
        let required_evidence_kind = promoting.then_some(match authority {
            #[cfg(test)]
            PromotionAuthority::EvidenceThreshold => "qualified",
            PromotionAuthority::LocalOwner { .. } => "operator_promotion",
        });
        ensure_snapshot_evidence(&tx, &request.snapshot, true, required_evidence_kind)?;
        let candidate = read_revision(&tx, &request.candidate_id)?;
        let predecessor = read_revision(&tx, &request.predecessor_id)?;
        if candidate.status != candidate_from {
            return Err(LifecycleError::IllegalTransition {
                role: "candidate",
                skill_id: candidate.id.clone(),
                from: candidate.status,
                to: candidate_to,
            });
        }
        if predecessor.status != predecessor_from {
            return Err(LifecycleError::IllegalTransition {
                role: "predecessor",
                skill_id: predecessor.id.clone(),
                from: predecessor.status,
                to: predecessor_to,
            });
        }
        if candidate.row_version != request.candidate_row_version {
            return Err(LifecycleError::StaleRowVersion {
                skill_id: candidate.id,
                expected: request.candidate_row_version,
                actual: candidate.row_version,
            });
        }
        if predecessor.row_version != request.predecessor_row_version {
            return Err(LifecycleError::StaleRowVersion {
                skill_id: predecessor.id,
                expected: request.predecessor_row_version,
                actual: predecessor.row_version,
            });
        }
        if candidate.supersedes_id.as_deref() != Some(predecessor.id.as_str())
            || candidate.lineage_root_id != predecessor.lineage_root_id
        {
            return Err(LifecycleError::LineageFork);
        }
        validate_lineage(&tx, &candidate, candidate_to)?;
        let desired: i64 = tx.query_row(
            "SELECT desired_generation FROM skill_generations WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if desired != request.snapshot.index_generation {
            return Err(LifecycleError::StaleGeneration {
                expected: request.snapshot.index_generation,
                actual: desired,
            });
        }
        if promoting {
            match authority {
                #[cfg(test)]
                PromotionAuthority::EvidenceThreshold => {
                    revalidate_promotion(&tx, request, &candidate, &predecessor, desired)?;
                }
                PromotionAuthority::LocalOwner {
                    approval,
                    authorization,
                } => {
                    authorize_local_owner_promotion(
                        &tx,
                        request,
                        approval,
                        authorization,
                        &candidate,
                        created_at,
                    )?;
                }
            }
        }
        let next_generation = desired + 1;
        let candidate_next = candidate.row_version + 1;
        let predecessor_next = predecessor.row_version + 1;
        tx.execute(
            "UPDATE skill_revisions
             SET status = ?, row_version = ?, superseded_by_id = NULL, updated_at = ?
             WHERE id = ? AND status = ? AND row_version = ?",
            params![
                candidate_to.as_token(),
                candidate_next,
                created_at,
                candidate.id,
                candidate_from.as_token(),
                candidate.row_version,
            ],
        )?;
        let superseded_by_id =
            (predecessor_to == LifecycleStatus::Superseded).then_some(candidate.id.as_str());
        tx.execute(
            "UPDATE skill_revisions
             SET status = ?, row_version = ?, superseded_by_id = ?, updated_at = ?
             WHERE id = ? AND status = ? AND row_version = ?",
            params![
                predecessor_to.as_token(),
                predecessor_next,
                superseded_by_id,
                created_at,
                predecessor.id,
                predecessor_from.as_token(),
                predecessor.row_version,
            ],
        )?;
        tx.execute(
            "UPDATE skill_generations SET desired_generation = ?, updated_at = ?
             WHERE singleton = 1 AND desired_generation = ?",
            params![next_generation, created_at, desired],
        )?;
        let canonical = request.snapshot.canonical_json()?;
        for (suffix, revision, from, to, from_version, to_version) in [
            (
                "candidate",
                candidate.id.as_str(),
                candidate_from,
                candidate_to,
                candidate.row_version,
                candidate_next,
            ),
            (
                "predecessor",
                predecessor.id.as_str(),
                predecessor_from,
                predecessor_to,
                predecessor.row_version,
                predecessor_next,
            ),
        ] {
            tx.execute(
                "INSERT INTO skill_transitions (
                    idempotency_key, skill_id, predecessor_id, from_status,
                    to_status, reason, evidence_snapshot, policy_version,
                    row_version_from, row_version_to, desired_generation, created_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    format!("{}:{suffix}", request.idempotency_key),
                    revision,
                    predecessor.id,
                    from.as_token(),
                    to.as_token(),
                    request.reason,
                    canonical,
                    request.snapshot.policy_version,
                    from_version,
                    to_version,
                    next_generation,
                    created_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(ReplacementTransitionOutcome {
            candidate_status: candidate_to,
            predecessor_status: predecessor_to,
            candidate_row_version: candidate_next,
            predecessor_row_version: predecessor_next,
            desired_generation: next_generation,
            replayed: false,
        })
    }
}

/// Name the exact precondition an authenticated approval failed, and the value
/// observed instead. Six checks used to collapse into one payload-less error.
fn approval_mismatch(
    skill_id: &str,
    field: &'static str,
    expected: impl std::fmt::Display,
    observed: impl std::fmt::Display,
) -> LifecycleError {
    LifecycleError::ApprovalMismatch {
        skill_id: skill_id.to_string(),
        field,
        expected: expected.to_string(),
        observed: observed.to_string(),
    }
}

fn validate_human_approval(approval: &HumanApproval) -> Result<(), LifecycleError> {
    if approval.approval_id.is_empty()
        || approval.actor_id.is_empty()
        || approval.evaluation_report_id.is_empty()
        || approval.expected_row_version < 1
    {
        return Err(LifecycleError::InvalidHumanApproval);
    }
    Ok(())
}

fn insert_approval(
    tx: &Transaction<'_>,
    skill_id: &str,
    kind: &str,
    approval: &HumanApproval,
    created_at: i64,
) -> Result<(), LifecycleError> {
    let changed = tx.execute(
        "INSERT OR IGNORE INTO skill_lifecycle_approvals (
            approval_id, skill_id, approval_kind, actor_id,
            artifact_row_version, evaluation_report_id, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            approval.approval_id,
            skill_id,
            kind,
            approval.actor_id,
            approval.expected_row_version,
            approval.evaluation_report_id,
            created_at,
        ],
    )?;
    if changed == 0 {
        let existing: Option<(String, String, String, i64, String)> = tx
            .query_row(
                "SELECT skill_id, approval_kind, actor_id,
                        artifact_row_version, evaluation_report_id
                 FROM skill_lifecycle_approvals WHERE approval_id = ?",
                [&approval.approval_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let expected = (
            skill_id.to_string(),
            kind.to_string(),
            approval.actor_id.clone(),
            approval.expected_row_version,
            approval.evaluation_report_id.clone(),
        );
        if existing.as_ref() != Some(&expected) {
            return Err(approval_mismatch(
                skill_id,
                "approval_id_binding",
                format!("{expected:?}"),
                existing.map_or_else(|| "none".to_string(), |row| format!("{row:?}")),
            ));
        }
    }
    Ok(())
}

/// Apply one lifecycle transition inside a caller-owned transaction.
///
/// Callers that must write policy or evidence rows atomically with the
/// decision open the `BEGIN IMMEDIATE` themselves and pass it here, so a crash
/// between those writes and the decision can never leave evidence for a
/// transition that was never applied. Every idempotency, policy, evidence,
/// row-version, generation, and lineage guard is unchanged; only the commit
/// belongs to the caller.
pub(crate) fn transition_in_tx(
    tx: &Transaction<'_>,
    request: &TransitionRequest,
    created_at: i64,
) -> Result<TransitionOutcome, LifecycleError> {
    validate_request(request)?;
    if matches!(
        request.to_status,
        LifecycleStatus::Canary | LifecycleStatus::Active
    ) || (request.from_status == LifecycleStatus::Active
        && request.to_status == LifecycleStatus::Superseded)
    {
        return Err(LifecycleError::PrivilegedTransition);
    }

    if let Some(replayed) = read_idempotent_transition(tx, request)? {
        return Ok(replayed);
    }

    ensure_policy_exists(tx, &request.snapshot.policy_version)?;
    ensure_snapshot_evidence(tx, &request.snapshot, false, None)?;
    let current = read_revision(tx, &request.skill_id)?;
    if current.status == LifecycleStatus::Rejected {
        return Err(LifecycleError::RejectedIsTerminal);
    }
    if current.status != request.from_status {
        return Err(LifecycleError::IllegalTransition {
            role: "revision",
            skill_id: current.id.clone(),
            from: current.status,
            to: request.to_status,
        });
    }
    if current.row_version != request.expected_row_version {
        return Err(LifecycleError::StaleRowVersion {
            skill_id: current.id,
            expected: request.expected_row_version,
            actual: current.row_version,
        });
    }

    let (desired_generation, _): (i64, i64) = tx.query_row(
        "SELECT desired_generation, applied_generation
             FROM skill_generations WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if desired_generation != request.snapshot.index_generation {
        return Err(LifecycleError::StaleGeneration {
            expected: request.snapshot.index_generation,
            actual: desired_generation,
        });
    }

    validate_lineage(tx, &current, request.to_status)?;
    let next_generation = desired_generation + 1;
    let next_row_version = current.row_version + 1;
    let changed = tx.execute(
        "UPDATE skill_revisions
             SET status = ?, row_version = ?, updated_at = ?
             WHERE id = ? AND status = ? AND row_version = ?",
        params![
            request.to_status.as_token(),
            next_row_version,
            created_at,
            request.skill_id,
            request.from_status.as_token(),
            request.expected_row_version
        ],
    )?;
    if changed != 1 {
        return Err(LifecycleError::StaleRowVersion {
            skill_id: request.skill_id.clone(),
            expected: request.expected_row_version,
            actual: current.row_version,
        });
    }

    tx.execute(
        "UPDATE skill_generations
             SET desired_generation = ?, updated_at = ?
             WHERE singleton = 1 AND desired_generation = ?",
        params![next_generation, created_at, desired_generation],
    )?;

    let evidence_snapshot = request.snapshot.canonical_json()?;
    tx.execute(
        "INSERT INTO skill_transitions (
                idempotency_key, skill_id, predecessor_id, from_status,
                to_status, reason, evidence_snapshot, policy_version,
                row_version_from, row_version_to, desired_generation, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            request.idempotency_key,
            request.skill_id,
            request.snapshot.predecessor_id,
            request.from_status.as_token(),
            request.to_status.as_token(),
            request.reason,
            evidence_snapshot,
            request.snapshot.policy_version,
            current.row_version,
            next_row_version,
            next_generation,
            created_at,
        ],
    )?;
    let transition_id = tx.last_insert_rowid();

    Ok(TransitionOutcome {
        transition_id,
        skill_id: request.skill_id.clone(),
        status: request.to_status,
        row_version: next_row_version,
        desired_generation: next_generation,
        replayed: false,
    })
}

/// Register a policy version inside a caller-owned transaction.
///
/// The same idempotency contract as [`LifecycleService::register_policy`]: a
/// byte-identical re-registration is a no-op, a conflicting one is refused.
pub(crate) fn register_policy_in_tx(
    tx: &Transaction<'_>,
    policy_version: &str,
    canonical_policy_json: &str,
    created_at: i64,
) -> Result<(), LifecycleError> {
    let parsed: serde_json::Value = serde_json::from_str(canonical_policy_json)?;
    let canonical = serde_json::to_string(&parsed)?;
    let changed = tx.execute(
        "INSERT OR IGNORE INTO skill_policy_versions
            (policy_version, policy_json, created_at)
         VALUES (?, ?, ?)",
        params![policy_version, canonical, created_at],
    )?;
    if changed == 0 {
        let existing: String = tx.query_row(
            "SELECT policy_json FROM skill_policy_versions WHERE policy_version = ?",
            [policy_version],
            |row| row.get(0),
        )?;
        if existing != canonical {
            return Err(LifecycleError::IdempotencyConflict);
        }
    }
    Ok(())
}

fn validate_request(request: &TransitionRequest) -> Result<(), LifecycleError> {
    if !request.from_status.may_transition_to(request.to_status) {
        return Err(LifecycleError::IllegalTransition {
            role: "revision",
            skill_id: request.skill_id.clone(),
            from: request.from_status,
            to: request.to_status,
        });
    }
    request.snapshot.validate()?;
    if request.idempotency_key.is_empty()
        || request.reason.is_empty()
        || request.skill_id != request.snapshot.artifact_id
        || request.expected_row_version != request.snapshot.artifact_row_version
    {
        return Err(LifecycleError::EvidenceMismatch);
    }
    Ok(())
}

fn ensure_policy_exists(tx: &Transaction<'_>, version: &str) -> Result<(), LifecycleError> {
    let exists = tx
        .query_row(
            "SELECT 1 FROM skill_policy_versions WHERE policy_version = ?",
            [version],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(LifecycleError::UnknownPolicyVersion(version.to_string()))
    }
}

/// Revalidate the explicit local-owner promotion inside the promotion
/// transaction.
///
/// This is the replacement counterpart of `activate_root`'s gate: the approval
/// must be bound to the exact candidate row and its evaluation report, a first
/// and *distinct* local-owner approval must already exist for the canary, and
/// the single-use authorization is consumed here so one operator action can
/// promote at most once.
fn authorize_local_owner_promotion(
    tx: &Transaction<'_>,
    request: &ReplacementTransitionRequest,
    approval: &HumanApproval,
    authorization: &super::store::ApprovalAuthorization,
    candidate: &RevisionState,
    created_at: i64,
) -> Result<(), LifecycleError> {
    validate_human_approval(approval)?;
    if !authorization.binds_approval(&approval.approval_id, &approval.actor_id) {
        return Err(StoreError::Unauthorized.into());
    }
    if approval.expected_row_version != request.candidate_row_version {
        return Err(approval_mismatch(
            &request.candidate_id,
            "requested_row_version",
            request.candidate_row_version,
            approval.expected_row_version,
        ));
    }
    if candidate.row_version != approval.expected_row_version {
        return Err(approval_mismatch(
            &request.candidate_id,
            "row_version",
            approval.expected_row_version,
            candidate.row_version,
        ));
    }
    let report: Option<String> = tx
        .query_row(
            "SELECT evaluation_report_id FROM skill_revisions WHERE id = ?",
            [&request.candidate_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if report.as_deref() != Some(approval.evaluation_report_id.as_str()) {
        return Err(approval_mismatch(
            &request.candidate_id,
            "evaluation_report_id",
            &approval.evaluation_report_id,
            report.as_deref().unwrap_or("none"),
        ));
    }
    // Approval into canary recorded the first authenticated local-owner action.
    // Promotion must be a second action with a different approval identity.
    let first_approval: Option<String> = tx
        .query_row(
            "SELECT approval_id FROM skill_lifecycle_approvals
             WHERE skill_id = ? AND approval_kind = 'phase4_canary'",
            [&request.candidate_id],
            |row| row.get(0),
        )
        .optional()?;
    if first_approval
        .as_deref()
        .is_none_or(|first| first == approval.approval_id)
    {
        return Err(approval_mismatch(
            &request.candidate_id,
            "first_approval",
            "a distinct earlier phase4_canary approval",
            first_approval.as_deref().unwrap_or("none"),
        ));
    }
    let artifact = read_artifact_for_policy(tx, &request.candidate_id)?;
    consume_approval_authorization(
        tx,
        authorization,
        &artifact,
        &approval.evaluation_report_id,
        ApprovalTransition::CanaryToActive,
        created_at,
    )?;
    insert_approval(
        tx,
        &request.candidate_id,
        "phase5_operator_promotion",
        approval,
        created_at,
    )?;
    Ok(())
}

#[cfg(test)]
fn revalidate_promotion(
    tx: &Transaction<'_>,
    request: &ReplacementTransitionRequest,
    candidate_revision: &RevisionState,
    predecessor_revision: &RevisionState,
    desired_generation: i64,
) -> Result<(), LifecycleError> {
    let policy_json: String = tx.query_row(
        "SELECT policy_json FROM skill_policy_versions WHERE policy_version = ?",
        [&request.snapshot.policy_version],
        |row| row.get(0),
    )?;
    let policy: PromotionPolicy = serde_json::from_str(&policy_json).map_err(|error| {
        LifecycleError::PromotionHeld(format!("invalid durable policy: {error}"))
    })?;
    if policy.version != request.snapshot.policy_version {
        return Err(LifecycleError::PromotionHeld(
            "durable policy version mismatch".to_string(),
        ));
    }

    let candidate = read_artifact_for_policy(tx, &request.candidate_id)?;
    let predecessor = read_artifact_for_policy(tx, &request.predecessor_id)?;
    let capability_increased = !super::admission::capability_is_non_escalating(
        &candidate.capability,
        &predecessor.capability,
    );
    let unresolved_negative_feedback: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM skill_feedback
             WHERE skill_id = ? AND state = 'active'
               AND feedback_kind IN ('negative', 'severe')
         )",
        [&request.candidate_id],
        |row| row.get(0),
    )?;
    let context = PromotionContext {
        candidate_id: request.candidate_id.clone(),
        predecessor_id: Some(request.predecessor_id.clone()),
        capability_tier: candidate.capability.tier,
        capability_increased,
        // A canary can only be produced by the admission transaction after
        // inherited and held-out verification pass. The status is re-read in
        // this same transaction above, so callers cannot assert these gates.
        inherited_tests_passed: candidate_revision.status == LifecycleStatus::Canary,
        held_out_tests_passed: candidate_revision.status == LifecycleStatus::Canary,
        unresolved_negative_feedback,
        identity_valid: candidate.verify_identity().is_ok()
            && predecessor.verify_identity().is_ok(),
        row_version_current: candidate_revision.row_version == request.candidate_row_version
            && predecessor_revision.row_version == request.predecessor_row_version,
        generation_current: desired_generation == request.snapshot.index_generation,
    };
    let candidate_events = read_invocation_evidence(tx, &request.candidate_id, &policy)?;
    let predecessor_events = read_invocation_evidence(tx, &request.predecessor_id, &policy)?;
    let task_outcomes = read_task_outcomes(tx, &request.candidate_id, &policy)?;
    let evaluation = evaluate_promotion_with_task_outcomes(
        &policy,
        &context,
        &candidate_events,
        &predecessor_events,
        &task_outcomes,
    )
    .map_err(|error| LifecycleError::PromotionHeld(error.to_string()))?;
    if evaluation.decision != PromotionDecision::Promote {
        return Err(LifecycleError::PromotionHeld(evaluation.reasons.join(",")));
    }
    Ok(())
}

fn read_artifact_for_policy(
    tx: &Transaction<'_>,
    skill_id: &str,
) -> Result<super::SkillArtifact, LifecycleError> {
    let row = tx
        .query_row(
            "SELECT id, identity_version, source, description, tags_json,
                    exports_json, tests_json, capability_json, status
             FROM skill_revisions WHERE id = ?",
            [skill_id],
            super::store::read_artifact_row,
        )
        .optional()?;
    match row {
        Some(Ok(artifact)) => Ok(artifact),
        Some(Err(error)) => Err(error.into()),
        None => Err(StoreError::NotFound(skill_id.to_string()).into()),
    }
}

#[cfg(test)]
fn read_invocation_evidence(
    tx: &Transaction<'_>,
    skill_id: &str,
    policy: &PromotionPolicy,
) -> Result<Vec<InvocationEvidence>, LifecycleError> {
    let mut statement = tx.prepare(
        "SELECT terminal.invocation_id, terminal.turn_id, terminal.event_kind,
                COALESCE(terminal.latency_us, 0),
                invoked.production AND terminal.production,
                invoked.evidence_complete AND terminal.evidence_complete
                    AND NOT EXISTS (SELECT 1 FROM skill_turn_losses AS loss
                        WHERE loss.turn_id = terminal.turn_id
                          AND loss.production = terminal.production),
                terminal.created_at
         FROM skill_events AS terminal
         JOIN skill_events AS invoked
           ON invoked.invocation_id = terminal.invocation_id
          AND invoked.skill_id = terminal.skill_id
          AND invoked.event_kind = 'invoked'
         WHERE terminal.skill_id = ?
           AND terminal.event_kind IN (
               'returned', 'threw', 'timed_out', 'oom', 'capability_denied'
           )
           AND terminal.created_at BETWEEN ? AND ?
         ORDER BY terminal.event_id",
    )?;
    let mut rows = statement.query(params![skill_id, policy.window_start, policy.window_end])?;
    let mut evidence = Vec::new();
    while let Some(row) = rows.next()? {
        let event_kind: String = row.get(2)?;
        let outcome = match event_kind.as_str() {
            "returned" => DirectOutcome::Success,
            "threw" => DirectOutcome::Throw,
            "timed_out" => DirectOutcome::Timeout,
            "oom" => DirectOutcome::Oom,
            "capability_denied" => DirectOutcome::CapabilityDenied,
            _ => continue,
        };
        let latency_us: i64 = row.get(3)?;
        evidence.push(InvocationEvidence {
            invocation_id: row.get(0)?,
            skill_id: skill_id.to_string(),
            turn_id: row.get(1)?,
            outcome,
            latency_us: u64::try_from(latency_us).map_err(|_| {
                LifecycleError::PromotionHeld("negative durable latency".to_string())
            })?,
            production: row.get(4)?,
            observability_complete: row.get(5)?,
            created_at: row.get(6)?,
        });
    }
    Ok(evidence)
}

/// Inverse of `telemetry::task_outcome_source_columns`: decode one durable
/// `(source_kind, source_id)` pair.
///
/// `gate_skipped` — a configured verify command whose gate did not run because
/// the turn never touched the workspace — decodes to its own variant rather
/// than to `NoVerifyCommand`, so promotion and audit keep seeing the reason the
/// runner actually recorded. `None` means the row is undecodable and promotion
/// must be held rather than guessing a source.
#[cfg(all(test, feature = "goal"))]
pub(crate) fn task_outcome_source_from_columns_for_test(
    source_kind: &str,
    source_id: Option<String>,
) -> Option<TaskOutcomeSource> {
    task_outcome_source_from_columns(source_kind, source_id)
}

#[cfg(test)]
fn task_outcome_source_from_columns(
    source_kind: &str,
    source_id: Option<String>,
) -> Option<TaskOutcomeSource> {
    match (source_kind, source_id) {
        ("verify_command", Some(id)) => Some(TaskOutcomeSource::VerifyCommand(id)),
        ("oracle", Some(id)) => Some(TaskOutcomeSource::Oracle(id)),
        ("no_verify_command", None) => Some(TaskOutcomeSource::NoVerifyCommand),
        ("gate_skipped", None) => Some(TaskOutcomeSource::GateSkipped),
        #[cfg(feature = "goal")]
        ("goal", Some(id)) => {
            let (goal_id, verified) = id.split_once(':')?;
            Some(TaskOutcomeSource::Goal {
                goal_id: goal_id.to_string(),
                verified_by: verified
                    .split('+')
                    .filter(|kind| !kind.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
fn read_task_outcomes(
    tx: &Transaction<'_>,
    skill_id: &str,
    policy: &PromotionPolicy,
) -> Result<Vec<TaskOutcomeEvidence>, LifecycleError> {
    let mut statement = tx.prepare(
        "SELECT outcome.turn_id, outcome.verify_passed, outcome.attempt,
                outcome.source_kind, outcome.source_id, outcome.production,
                outcome.created_at, outcome.evidence_complete
         FROM skill_task_outcomes AS outcome
         JOIN skill_task_outcome_links AS link
           ON link.evidence_id = outcome.evidence_id
         WHERE link.skill_id = ? AND outcome.created_at BETWEEN ? AND ?
         ORDER BY outcome.created_at, outcome.evidence_id",
    )?;
    let mut rows = statement.query(params![skill_id, policy.window_start, policy.window_end])?;
    let mut outcomes = Vec::new();
    while let Some(row) = rows.next()? {
        let source_kind: String = row.get(3)?;
        let source_id: Option<String> = row.get(4)?;
        let Some(source) = task_outcome_source_from_columns(&source_kind, source_id) else {
            return Err(LifecycleError::PromotionHeld(
                "invalid durable task-outcome source".to_string(),
            ));
        };
        let attempt: i64 = row.get(2)?;
        outcomes.push(TaskOutcomeEvidence {
            turn_id: row.get(0)?,
            skill_ids: vec![skill_id.to_string()],
            verify_passed: row.get(1)?,
            attempt: u32::try_from(attempt).map_err(|_| {
                LifecycleError::PromotionHeld("invalid durable task-outcome attempt".to_string())
            })?,
            source,
            production: row.get(5)?,
            evidence_complete: row.get(7)?,
            created_at: row.get(6)?,
        });
    }
    Ok(outcomes)
}

fn ensure_snapshot_evidence(
    tx: &Transaction<'_>,
    snapshot: &EvidenceSnapshot,
    require_evidence: bool,
    required_kind: Option<&str>,
) -> Result<(), LifecycleError> {
    if require_evidence && snapshot.evidence_ids.is_empty() {
        return Err(LifecycleError::UnknownEvidence);
    }
    for evidence_id in &snapshot.evidence_ids {
        let evidence: Option<(String, String)> = tx
            .query_row(
                "SELECT skill_id, evidence_kind FROM skill_evidence
                 WHERE evidence_id = ? AND policy_version = ?",
                params![evidence_id, snapshot.policy_version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((evidence_skill, evidence_kind)) = evidence else {
            return Err(LifecycleError::UnknownEvidence);
        };
        if (evidence_skill != snapshot.artifact_id
            && Some(evidence_skill.as_str()) != snapshot.predecessor_id.as_deref())
            || required_kind.is_some_and(|kind| evidence_kind != kind)
        {
            return Err(LifecycleError::UnknownEvidence);
        }
    }
    Ok(())
}

fn read_revision(
    db: &rusqlite::Connection,
    skill_id: &str,
) -> Result<RevisionState, LifecycleError> {
    db.query_row(
        "SELECT id, status, supersedes_id, superseded_by_id,
                COALESCE(lineage_root_id, id), row_version
         FROM skill_revisions WHERE id = ?",
        [skill_id],
        |row| {
            let status: String = row.get(1)?;
            Ok((
                row.get::<_, String>(0)?,
                status,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        },
    )
    .map_err(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => {
            LifecycleError::Store(StoreError::NotFound(skill_id.to_string()))
        }
        other => LifecycleError::Sqlite(other),
    })
    .and_then(
        |(id, status, supersedes_id, superseded_by_id, lineage_root_id, row_version)| {
            let status = LifecycleStatus::from_token(&status)
                .ok_or_else(|| LifecycleError::UnknownStatus(status.clone()))?;
            Ok(RevisionState {
                id,
                status,
                supersedes_id,
                superseded_by_id,
                lineage_root_id,
                row_version,
            })
        },
    )
}

fn read_idempotent_transition(
    tx: &Transaction<'_>,
    request: &TransitionRequest,
) -> Result<Option<TransitionOutcome>, LifecycleError> {
    let existing = tx
        .query_row(
            "SELECT transition_id, skill_id, from_status, to_status,
                    evidence_snapshot, row_version_to, desired_generation
             FROM skill_transitions WHERE idempotency_key = ?",
            [&request.idempotency_key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;

    let Some((
        transition_id,
        skill_id,
        from_status,
        to_status,
        evidence_snapshot,
        row_version,
        desired_generation,
    )) = existing
    else {
        return Ok(None);
    };

    let expected_snapshot = request.snapshot.canonical_json()?;
    if skill_id != request.skill_id
        || from_status != request.from_status.as_token()
        || to_status != request.to_status.as_token()
        || evidence_snapshot != expected_snapshot
    {
        return Err(LifecycleError::IdempotencyConflict);
    }
    Ok(Some(TransitionOutcome {
        transition_id,
        skill_id,
        status: request.to_status,
        row_version,
        desired_generation,
        replayed: true,
    }))
}

fn read_idempotent_root_transition(
    tx: &Transaction<'_>,
    request: &TransitionRequest,
    approval: &HumanApproval,
) -> Result<Option<TransitionOutcome>, LifecycleError> {
    let Some(outcome) = read_idempotent_transition(tx, request)? else {
        return Ok(None);
    };
    let exact_approval_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM skill_lifecycle_approvals
          WHERE approval_id = ?1
            AND skill_id = ?2
            AND approval_kind = 'phase5_root_activation'
            AND actor_id = ?3
            AND artifact_row_version = ?4
            AND evaluation_report_id = ?5",
        params![
            approval.approval_id,
            request.skill_id,
            approval.actor_id,
            approval.expected_row_version,
            approval.evaluation_report_id,
        ],
        |row| row.get(0),
    )?;
    if exact_approval_count != 1 {
        return Err(LifecycleError::IdempotencyConflict);
    }
    Ok(Some(outcome))
}

fn validate_lineage(
    tx: &Transaction<'_>,
    revision: &RevisionState,
    next: LifecycleStatus,
) -> Result<(), LifecycleError> {
    if revision.supersedes_id.as_deref() == Some(revision.id.as_str())
        || revision.superseded_by_id.as_deref() == Some(revision.id.as_str())
    {
        return Err(LifecycleError::LineageCycle);
    }

    if revision.supersedes_id.is_none() && revision.lineage_root_id != revision.id {
        return Err(LifecycleError::LineageFork);
    }
    let mut seen = BTreeSet::new();
    let mut cursor = revision.supersedes_id.clone();
    while let Some(id) = cursor {
        if id == revision.id || !seen.insert(id.clone()) {
            return Err(LifecycleError::LineageCycle);
        }
        let ancestor = tx
            .query_row(
                "SELECT supersedes_id, COALESCE(lineage_root_id, id)
                 FROM skill_revisions WHERE id = ?",
                [&id],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or(LifecycleError::LineageFork)?;
        if ancestor.1 != revision.lineage_root_id {
            return Err(LifecycleError::LineageFork);
        }
        if ancestor.0.is_none() && id != revision.lineage_root_id {
            return Err(LifecycleError::LineageFork);
        }
        cursor = ancestor.0;
    }

    if next == LifecycleStatus::Active {
        let active_successors: i64 = tx.query_row(
            "SELECT COUNT(*) FROM skill_revisions
             WHERE supersedes_id = ? AND id <> ?
               AND status = 'active'",
            params![revision.supersedes_id, revision.id],
            |row| row.get(0),
        )?;
        if active_successors > 0 {
            return Err(LifecycleError::LineageFork);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v98t: a durable `gate_skipped` row must decode to its own variant. If
    /// the arm is dropped the row becomes undecodable and every promotion that
    /// reads the window is held, so this pins the decode, not just "not None".
    #[test]
    fn a_gate_skipped_row_decodes_to_its_own_source() {
        assert_eq!(
            task_outcome_source_from_columns("gate_skipped", None),
            Some(TaskOutcomeSource::GateSkipped)
        );
        assert_eq!(
            task_outcome_source_from_columns("no_verify_command", None),
            Some(TaskOutcomeSource::NoVerifyCommand)
        );
        // The two skip reasons must stay distinguishable after a round trip
        // through the durable columns.
        let skipped = TaskOutcomeSource::GateSkipped;
        let (kind, id) = super::super::telemetry::task_outcome_source_columns_for_test(&skipped);
        assert_eq!(kind, "gate_skipped");
        assert_eq!(id, None);
        assert_eq!(
            task_outcome_source_from_columns(kind, id.map(|id| id.to_string())),
            Some(TaskOutcomeSource::GateSkipped)
        );
        // A source id on a skip reason is not a valid row.
        assert_eq!(
            task_outcome_source_from_columns("gate_skipped", Some("x".to_string())),
            None
        );
    }

    #[test]
    fn approval_failures_name_the_field_and_the_observed_value() {
        let stale_row = approval_mismatch("skill-1", "row_version", 4, 7);
        let wrong_report =
            approval_mismatch("skill-1", "evaluation_report_id", "report-a", "report-b");
        let missing_first = approval_mismatch(
            "skill-1",
            "first_approval",
            "a distinct earlier approval",
            "none",
        );

        // Six preconditions used to produce one payload-less message.
        assert_ne!(stale_row.to_string(), wrong_report.to_string());
        assert_ne!(wrong_report.to_string(), missing_first.to_string());
        assert!(stale_row.to_string().contains("row_version"), "{stale_row}");
        assert!(stale_row.to_string().contains("expected 4"), "{stale_row}");
        assert!(stale_row.to_string().contains("observed 7"), "{stale_row}");
        assert!(
            wrong_report.to_string().contains("observed report-b"),
            "{wrong_report}"
        );
        assert!(matches!(
            stale_row,
            LifecycleError::ApprovalMismatch {
                field: "row_version",
                ..
            }
        ));
    }

    #[test]
    fn an_illegal_transition_names_which_side_of_the_pair_was_wrong() {
        let candidate = LifecycleError::IllegalTransition {
            role: "candidate",
            skill_id: "skill-candidate".to_string(),
            from: LifecycleStatus::Verified,
            to: LifecycleStatus::Active,
        };
        let predecessor = LifecycleError::IllegalTransition {
            role: "predecessor",
            skill_id: "skill-predecessor".to_string(),
            from: LifecycleStatus::Retired,
            to: LifecycleStatus::Superseded,
        };
        assert!(
            candidate.to_string().contains("candidate skill-candidate"),
            "{candidate}"
        );
        assert!(
            predecessor
                .to_string()
                .contains("predecessor skill-predecessor"),
            "{predecessor}"
        );
        assert_ne!(candidate.to_string(), predecessor.to_string());
    }

    #[test]
    fn a_structurally_impossible_transition_names_the_revision_it_refused() {
        let request = TransitionRequest {
            idempotency_key: "key".to_string(),
            skill_id: "skill-2".to_string(),
            from_status: LifecycleStatus::Retired,
            to_status: LifecycleStatus::Active,
            expected_row_version: 1,
            reason: "test".to_string(),
            snapshot: EvidenceSnapshot::new(
                "skill-2",
                None,
                "v1",
                Vec::new(),
                BTreeMap::new(),
                1,
                None,
                0,
            )
            .unwrap(),
        };
        let error = validate_request(&request).unwrap_err().to_string();
        assert!(error.contains("revision skill-2"), "{error}");
        assert!(error.contains("retired -> active"), "{error}");
    }
}
