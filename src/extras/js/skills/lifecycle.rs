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

#[derive(Debug, Clone)]
pub struct HumanApproval {
    approval_id: String,
    actor_id: String,
    evaluation_report_id: String,
    expected_row_version: i64,
}

impl HumanApproval {
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
    #[error("illegal lifecycle transition: {from} -> {to}")]
    IllegalTransition {
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
    #[error("lineage-root activation was attempted on a replacement")]
    NotLineageRoot,
    #[error("privileged activation/supersession requires its dedicated atomic service")]
    PrivilegedTransition,
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

    pub(crate) fn transition(
        &self,
        request: &TransitionRequest,
        created_at: i64,
    ) -> Result<(TransitionOutcome, PublicationReport), LifecyclePublicationError> {
        let removed = (request.to_status != LifecycleStatus::Active)
            .then(|| request.skill_id.clone())
            .into_iter()
            .collect();
        self.coordinator
            .coordinate_mutation(removed, |store| {
                let outcome = LifecycleService::new(store).transition(request, created_at)?;
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

    pub(crate) fn transition(
        &mut self,
        request: &TransitionRequest,
        created_at: i64,
    ) -> Result<TransitionOutcome, LifecycleError> {
        validate_request(request)?;
        if request.to_status == LifecycleStatus::Active
            || (request.from_status == LifecycleStatus::Active
                && request.to_status == LifecycleStatus::Superseded)
        {
            return Err(LifecycleError::PrivilegedTransition);
        }
        let tx = self.store.connection_mut().transaction()?;

        if let Some(replayed) = read_idempotent_transition(&tx, request)? {
            tx.commit()?;
            return Ok(replayed);
        }

        ensure_policy_exists(&tx, &request.snapshot.policy_version)?;
        ensure_snapshot_evidence(&tx, &request.snapshot, false)?;
        let current = read_revision(&tx, &request.skill_id)?;
        if current.status == LifecycleStatus::Rejected {
            return Err(LifecycleError::RejectedIsTerminal);
        }
        if current.status != request.from_status {
            return Err(LifecycleError::IllegalTransition {
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

        validate_lineage(&tx, &current, request.to_status)?;
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
        tx.commit()?;

        Ok(TransitionOutcome {
            transition_id,
            skill_id: request.skill_id.clone(),
            status: request.to_status,
            row_version: next_row_version,
            desired_generation: next_generation,
            replayed: false,
        })
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
        let tx = self.store.connection_mut().transaction()?;
        let revision = read_revision(&tx, skill_id)?;
        let report: Option<String> = tx
            .query_row(
                "SELECT evaluation_report_id FROM skill_revisions WHERE id = ?",
                [skill_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if revision.status != LifecycleStatus::Canary
            || revision.supersedes_id.is_some()
            || revision.row_version != approval.expected_row_version
            || report.as_deref() != Some(approval.evaluation_report_id.as_str())
        {
            return Err(LifecycleError::InvalidHumanApproval);
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
            if let Some(replayed) = read_idempotent_transition(&tx, &request)? {
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
        if let Some(replayed) = read_idempotent_transition(&tx, &request)? {
            tx.commit()?;
            return Ok(replayed);
        }
        ensure_policy_exists(&tx, &snapshot.policy_version)?;
        ensure_snapshot_evidence(&tx, snapshot, false)?;
        let revision = read_revision(&tx, skill_id)?;
        if revision.supersedes_id.is_some() || revision.lineage_root_id != revision.id {
            return Err(LifecycleError::NotLineageRoot);
        }
        if revision.status != LifecycleStatus::Canary
            || revision.row_version != approval.expected_row_version
        {
            return Err(LifecycleError::InvalidHumanApproval);
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
            return Err(LifecycleError::InvalidHumanApproval);
        }
        let first_approval: Option<String> = tx
            .query_row(
                "SELECT approval_id FROM skill_lifecycle_approvals
                 WHERE skill_id = ? AND approval_kind = 'phase4_canary'",
                [skill_id],
                |row| row.get(0),
            )
            .optional()?;
        if first_approval
            .as_deref()
            .is_none_or(|first| first == approval.approval_id)
        {
            return Err(LifecycleError::InvalidHumanApproval);
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
            created_at,
        )
    }

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
            created_at,
        )
    }

    fn replace_pair(
        &mut self,
        request: &ReplacementTransitionRequest,
        candidate_from: LifecycleStatus,
        candidate_to: LifecycleStatus,
        predecessor_from: LifecycleStatus,
        predecessor_to: LifecycleStatus,
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
        let tx = self.store.connection_mut().transaction()?;
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
        ensure_snapshot_evidence(&tx, &request.snapshot, true)?;
        let candidate = read_revision(&tx, &request.candidate_id)?;
        let predecessor = read_revision(&tx, &request.predecessor_id)?;
        if candidate.status != candidate_from || predecessor.status != predecessor_from {
            return Err(LifecycleError::IllegalTransition {
                from: candidate.status,
                to: candidate_to,
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
        tx.execute(
            "UPDATE skill_revisions
             SET status = ?, row_version = ?, superseded_by_id = ?, updated_at = ?
             WHERE id = ? AND status = ? AND row_version = ?",
            params![
                predecessor_to.as_token(),
                predecessor_next,
                candidate.id,
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
        if existing.as_ref()
            != Some(&(
                skill_id.to_string(),
                kind.to_string(),
                approval.actor_id.clone(),
                approval.expected_row_version,
                approval.evaluation_report_id.clone(),
            ))
        {
            return Err(LifecycleError::InvalidHumanApproval);
        }
    }
    Ok(())
}

fn validate_request(request: &TransitionRequest) -> Result<(), LifecycleError> {
    if !request.from_status.may_transition_to(request.to_status) {
        return Err(LifecycleError::IllegalTransition {
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

fn ensure_snapshot_evidence(
    tx: &Transaction<'_>,
    snapshot: &EvidenceSnapshot,
    require_evidence: bool,
) -> Result<(), LifecycleError> {
    if require_evidence && snapshot.evidence_ids.is_empty() {
        return Err(LifecycleError::UnknownEvidence);
    }
    for evidence_id in &snapshot.evidence_ids {
        let evidence_skill: Option<String> = tx
            .query_row(
                "SELECT skill_id FROM skill_evidence
                 WHERE evidence_id = ? AND policy_version = ?",
                params![evidence_id, snapshot.policy_version],
                |row| row.get(0),
            )
            .optional()?;
        if evidence_skill.as_deref() != Some(snapshot.artifact_id.as_str())
            && evidence_skill.as_deref() != snapshot.predecessor_id.as_deref()
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
        let live_successors: i64 = tx.query_row(
            "SELECT COUNT(*) FROM skill_revisions
             WHERE supersedes_id = ? AND id <> ?
               AND status IN ('verified', 'canary', 'active')",
            params![revision.supersedes_id, revision.id],
            |row| row.get(0),
        )?;
        if live_successors > 0 {
            return Err(LifecycleError::LineageFork);
        }
    }
    Ok(())
}
