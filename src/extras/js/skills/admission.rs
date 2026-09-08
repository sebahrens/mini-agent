//! Sole Phase 4 evaluation and authenticated human admission service.
//!
//! The service reloads persisted immutable bytes for every gate, classifies
//! deterministic versus retryable failures, creates model-versioned embeddings
//! off the JS request path, and delegates the only canary transition to the
//! private optimistic transaction module.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::admission_store::AdmissionStore;
use super::embed::{Embedder, SkillDocument};
use super::held_out::{HeldOutError, HeldOutEvaluationReport, evaluate};
use super::store::{
    AdminIdentity, CanaryApprovalResult, EVALUATION_ATTEMPTS_EXHAUSTED, EvaluationReportRecord,
    MAX_EVALUATION_ATTEMPTS, ProposalLease, ProposalStatus, SkillStore, StoreError,
};
#[cfg(test)]
use super::store::{
    ApprovalAuthorization, ApprovalAuthorizationRequest, ApprovalTransition,
    approval_manifest_digest,
};
use super::verify::VerificationError;
use super::{CapabilityManifest, CapabilityScope, SkillArtifact, SkillExport};

const LEASE_SECONDS: i64 = 30;
// One evaluation can run the 64-case held-out cap plus embedded and inherited
// verification at the 30-second worker deadline. Size the lease for that
// documented worst case with cleanup margin so another worker cannot reclaim it.
const EVALUATION_LEASE_SECONDS: i64 = 40 * 60;
const MAX_RETRY_BACKOFF_SECONDS: i64 = 300;
const MAX_AUTH_AGE_SECONDS: i64 = 300;
const WORKER_IDLE_POLL: Duration = Duration::from_millis(100);

pub(crate) struct AdmissionEvaluator {
    store: SkillStore,
    /// Shared with retrieval and the telemetry coordinator: one compatible
    /// embedding configuration initializes one backend per session.
    embedder: Arc<Embedder>,
    worker_id: String,
    #[cfg(test)]
    verification_failure: Option<VerificationError>,
    #[cfg(test)]
    review_gate_failure: Option<VerificationError>,
    /// Counts contained approval-gate re-runs so tests can pin the
    /// "exactly one contained gate per `review_and_admit`" cost property.
    #[cfg(test)]
    review_gate_runs: std::cell::Cell<usize>,
}

impl AdmissionEvaluator {
    pub(crate) fn new(
        store: SkillStore,
        embedder: Arc<Embedder>,
        worker_id: impl Into<String>,
    ) -> Result<Self, AdmissionError> {
        let worker_id = worker_id.into();
        if worker_id.trim().is_empty() || worker_id.len() > 256 {
            return Err(AdmissionError::InvalidWorker);
        }
        Ok(Self {
            store,
            embedder,
            worker_id,
            #[cfg(test)]
            verification_failure: None,
            #[cfg(test)]
            review_gate_failure: None,
            #[cfg(test)]
            review_gate_runs: std::cell::Cell::new(0),
        })
    }

    pub(crate) fn evaluate_next(
        &mut self,
        now: i64,
    ) -> Result<Option<EvaluationReportRecord>, AdmissionError> {
        // A row whose lease expired on its final attempt can no longer be
        // claimed (`claim_due_proposal` requires `attempt_count <
        // MAX_EVALUATION_ATTEMPTS`), so without this sweep it would stay
        // `evaluating` forever with no report and no reason code, invisible to
        // an authenticated reevaluation request. Park it as deferred instead:
        // the immutable identity is never rejected and stays recoverable.
        for proposal_id in self.store.sweep_exhausted_proposals(now)? {
            tracing::warn!(
                proposal_id = %proposal_id,
                reason_code = EVALUATION_ATTEMPTS_EXHAUSTED,
                "skill proposal exhausted its evaluation attempt budget and was deferred"
            );
        }
        let Some(mut lease) = self
            .store
            .claim_due_proposal(&self.worker_id, now, LEASE_SECONDS)?
        else {
            return Ok(None);
        };
        lease.row_version = self.store.renew_lease(
            &lease.proposal_id,
            &self.worker_id,
            now,
            EVALUATION_LEASE_SECONDS,
        )?;
        lease.lease_expires_at = now.saturating_add(EVALUATION_LEASE_SECONDS);
        match self.evaluate_lease(&lease, now) {
            Ok(report) => Ok(Some(report)),
            Err(EvaluationFailure::Deterministic { code, detail }) => {
                let report = rejection_report(&lease, code, detail, now);
                self.store.reject_proposal(
                    &lease.proposal_id,
                    &self.worker_id,
                    lease.row_version,
                    &report,
                    code,
                    now,
                )?;
                Ok(Some(report))
            }
            Err(EvaluationFailure::SuiteRequired) => {
                let report = blocked_report(&lease, "held_out_suite_required", now);
                self.store.complete_blocked_evaluation(
                    &lease.proposal_id,
                    &self.worker_id,
                    lease.row_version,
                    &report,
                    "held_out_suite_required",
                    now,
                )?;
                Ok(Some(report))
            }
            Err(EvaluationFailure::Infrastructure { error }) => {
                if lease.infrastructure_attempts.saturating_add(1) >= MAX_EVALUATION_ATTEMPTS {
                    self.store.defer_infrastructure_proposal(
                        &lease.proposal_id,
                        &self.worker_id,
                        lease.row_version,
                        "evaluation_infrastructure_deferred",
                        now,
                    )?;
                    tracing::warn!(
                        proposal_id = %lease.proposal_id,
                        skill_id = %lease.skill_id,
                        reason_code = "evaluation_infrastructure_deferred",
                        "skill proposal deferred after repeated verification infrastructure failures"
                    );
                    return Err(AdmissionError::Retryable(error));
                }
                let exponent = lease.infrastructure_attempts.min(8);
                let delay = (1i64 << exponent).min(MAX_RETRY_BACKOFF_SECONDS);
                self.store.retry_infrastructure_proposal(
                    &lease.proposal_id,
                    &self.worker_id,
                    lease.row_version,
                    now.saturating_add(delay),
                    now,
                )?;
                Err(AdmissionError::Retryable(error))
            }
        }
    }

    fn evaluate_lease(
        &mut self,
        lease: &ProposalLease,
        now: i64,
    ) -> Result<EvaluationReportRecord, EvaluationFailure> {
        let artifact = self
            .store
            .get(&lease.skill_id)
            .map_err(classify_store)?
            .ok_or_else(|| deterministic("identity_invalid", "artifact missing"))?;
        artifact
            .verify_identity()
            .map_err(|_| deterministic("identity_invalid", "artifact identity mismatch"))?;
        let predecessor = match lease.predecessor_id.as_deref() {
            Some(id) => Some(self.store.get(id).map_err(classify_store)?.ok_or_else(|| {
                deterministic("inherited_regression_failed", "predecessor missing")
            })?),
            None => None,
        };
        if let Some(predecessor) = predecessor.as_ref()
            && !capability_is_non_escalating(&artifact.capability, &predecessor.capability)
        {
            return Err(deterministic(
                "contract_invalid",
                "replacement capability escalation",
            ));
        }
        if self
            .is_policy_duplicate(&artifact, lease.predecessor_id.as_deref())
            .map_err(classify_store)?
        {
            return Err(deterministic(
                "duplicate_skill",
                "active skill has the same normalized contract",
            ));
        }

        #[cfg(test)]
        if let Some(error) = self.verification_failure.take() {
            return Err(classify_verification(
                &error,
                "embedded_test_failed",
                "embedded verification failed",
            ));
        }

        let held_out = match evaluate(&self.store, &artifact, predecessor.as_ref()) {
            Ok(report) => report,
            Err(HeldOutError::SuiteRequired) => return Err(EvaluationFailure::SuiteRequired),
            Err(HeldOutError::Identity(_)) => {
                return Err(deterministic(
                    "identity_invalid",
                    "artifact identity mismatch",
                ));
            }
            Err(HeldOutError::Inherited(error)) => {
                return Err(classify_verification(
                    &error,
                    "inherited_regression_failed",
                    "predecessor regression failed",
                ));
            }
            Err(HeldOutError::InheritedTestsRemoved) => {
                return Err(deterministic(
                    "inherited_regression_failed",
                    "candidate removed an inherited test",
                ));
            }
            Err(HeldOutError::Embedded(error)) => {
                return Err(classify_verification(
                    &error,
                    "embedded_test_failed",
                    "embedded verification failed",
                ));
            }
            Err(HeldOutError::Infrastructure(error)) => {
                return Err(classify_verification(
                    &error,
                    "evaluation_infrastructure_unavailable",
                    "verification infrastructure unavailable",
                ));
            }
            Err(HeldOutError::CaseFailed { .. } | HeldOutError::TranscriptMismatch { .. }) => {
                return Err(deterministic(
                    "held_out_failed",
                    "held-out verification failed",
                ));
            }
            Err(HeldOutError::Store(error)) => return Err(classify_store(error)),
            Err(HeldOutError::Json(_) | HeldOutError::TamperedSuite(_)) => {
                return Err(deterministic(
                    "held_out_failed",
                    "held-out suite is invalid",
                ));
            }
            Err(HeldOutError::InvalidSuite(_) | HeldOutError::UnsupportedVersion(_)) => {
                return Err(deterministic(
                    "held_out_failed",
                    "held-out suite is unsupported",
                ));
            }
        };

        let document = skill_document(&artifact);
        let embeddings = self
            .embedder
            .embed_documents(&[document])
            .map_err(|error| EvaluationFailure::Infrastructure {
                error: error.to_string(),
            })?;
        let embedding = embeddings
            .first()
            .ok_or_else(|| EvaluationFailure::Infrastructure {
                error: "embedding result missing".to_string(),
            })?;
        let metadata = self.embedder.model_metadata().clone();
        let embedding_bytes = embedding
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>();
        self.store
            .store_embedding(
                &artifact.id,
                &metadata.model_id,
                &metadata.model_revision,
                u32::try_from(metadata.dimensions).map_err(|_| {
                    EvaluationFailure::Infrastructure {
                        error: "embedding dimensions overflow".to_string(),
                    }
                })?,
                metadata.normalized,
                &embedding_bytes,
            )
            .map_err(classify_store)?;

        let report = success_report(
            lease,
            &held_out,
            &metadata.model_id,
            &metadata.model_revision,
            now,
        );
        self.store
            .complete_evaluation(
                &lease.proposal_id,
                &self.worker_id,
                lease.row_version,
                &report,
                now,
            )
            .map_err(classify_store)?;
        Ok(report)
    }

    fn is_policy_duplicate(
        &self,
        artifact: &SkillArtifact,
        predecessor_id: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.store.has_policy_duplicate(artifact, predecessor_id)
    }

    pub(crate) fn review_and_admit<R: HumanReviewer>(
        &mut self,
        proposal_id: &str,
        reviewer: &R,
        now: i64,
    ) -> Result<ReviewOutcome, AdmissionError> {
        let proposal = self
            .store
            .get_proposal(proposal_id)?
            .ok_or_else(|| AdmissionError::NotFound(proposal_id.to_string()))?;
        if proposal.status == ProposalStatus::Approved
            && let Some(result) = self.store.canary_approval_result(proposal_id)?
        {
            return Ok(ReviewOutcome::Canary(result));
        }
        if proposal.status != ProposalStatus::AwaitingApproval {
            return Err(AdmissionError::NotAwaitingApproval);
        }
        let report_id = proposal
            .report_id
            .clone()
            .ok_or(AdmissionError::MissingReport)?;
        let report = self
            .store
            .get_evaluation_report(&report_id)?
            .ok_or(AdmissionError::MissingReport)?;
        let artifact = self
            .store
            .get(&proposal.skill_id)?
            .ok_or_else(|| AdmissionError::NotFound(proposal.skill_id.clone()))?;
        // The contained held-out gate is expensive and is re-run exactly once,
        // on the approval path only, after the reviewed record is confirmed
        // unchanged. Running it here as well doubled the cost of every approval
        // and made a denial impossible while the worker was unavailable.
        let artifact_version = self
            .store
            .revision_row_version(&artifact.id)?
            .ok_or(AdmissionError::StaleReview)?;
        let packet = ReviewPacket {
            proposal_id: proposal.proposal_id.clone(),
            report_id: report.report_id.clone(),
            artifact_id: artifact.id.clone(),
            source: artifact.source.clone(),
            description: artifact.description.clone(),
            tags: artifact.tags.clone(),
            exports: artifact.exports.clone(),
            tests: artifact.tests.clone(),
            capability: artifact.capability.clone(),
            gate_summary_json: report.summary_json.clone(),
            verifier_version: report.verifier_version,
            fakes_version: report.fakes_version,
            held_out_suite_hashes: report.suite_hashes.clone(),
            embedding_model_id: report.embedding_model_id.clone(),
            embedding_model_revision: report.embedding_model_revision.clone(),
        };

        match reviewer.review(&packet) {
            ReviewDecision::Approve(decision) => {
                if decision.authenticated_at > now
                    || now.saturating_sub(decision.authenticated_at) > MAX_AUTH_AGE_SECONDS
                {
                    return Err(AdmissionError::UnauthenticatedApproval);
                }
                let current_proposal = self
                    .store
                    .get_proposal(proposal_id)?
                    .ok_or(AdmissionError::StaleReview)?;
                let current_report = self
                    .store
                    .get_evaluation_report(&report_id)?
                    .ok_or(AdmissionError::StaleReview)?;
                let current_artifact = self
                    .store
                    .get(&artifact.id)?
                    .ok_or(AdmissionError::StaleReview)?;
                let current_artifact_version = self
                    .store
                    .revision_row_version(&artifact.id)?
                    .ok_or(AdmissionError::StaleReview)?;
                if current_proposal != proposal
                    || current_report != report
                    || current_artifact != artifact
                    || current_artifact_version != artifact_version
                {
                    return Err(AdmissionError::StaleReview);
                }
                // Semantic equality above proves the reviewed record is intact;
                // this single contained re-run additionally proves the gates
                // still pass against the current store (for example after a
                // held-out suite was disabled mid-review).
                #[cfg(test)]
                if let Some(error) = self.review_gate_failure.take() {
                    return Err(review_gate_verification_error(&error));
                }
                self.revalidate_review_gates(
                    &current_proposal,
                    &current_report,
                    &current_artifact,
                )?;
                let expires_at = now
                    .checked_add(MAX_AUTH_AGE_SECONDS)
                    .ok_or(AdmissionError::UnauthenticatedApproval)?;
                let mut admission = AdmissionStore::new(&mut self.store);
                let authorization = admission.authorize_canary(
                    &decision,
                    &artifact,
                    &report.report_id,
                    now,
                    expires_at,
                )?;
                let result = admission.approve_canary(
                    &proposal.proposal_id,
                    &artifact.id,
                    &report.report_id,
                    artifact_version,
                    proposal.row_version,
                    &authorization,
                    now,
                )?;
                if self.store.is_retrievable(&artifact.id)? {
                    return Err(AdmissionError::CanaryBecameRetrievable);
                }
                Ok(ReviewOutcome::Canary(result))
            }
            ReviewDecision::Deny { reason_code } => {
                AdmissionStore::new(&mut self.store).deny(
                    &proposal.proposal_id,
                    proposal.row_version,
                    &reason_code,
                    now,
                )?;
                Ok(ReviewOutcome::Denied)
            }
            ReviewDecision::Cancelled => Ok(ReviewOutcome::Cancelled),
            ReviewDecision::TimedOut => Ok(ReviewOutcome::TimedOut),
        }
    }

    pub(crate) fn request_reevaluation(
        &mut self,
        proposal_id: &str,
        admin: &AdminIdentity,
        now: i64,
    ) -> Result<(), AdmissionError> {
        let proposal = self
            .store
            .get_proposal(proposal_id)?
            .ok_or_else(|| AdmissionError::NotFound(proposal_id.to_string()))?;
        self.store.request_blocked_reevaluation(
            Some(admin),
            proposal_id,
            proposal.row_version,
            now,
        )?;
        Ok(())
    }

    /// Re-run every admission gate against the current store.
    ///
    /// This runs exactly once per `review_and_admit` call, and only on the
    /// approval path. Failures are classified by cause: a transient
    /// verification outage is retryable and must not be reported to operators
    /// as a changed review, and neither is an active duplicate or a missing
    /// compatible embedding.
    fn revalidate_review_gates(
        &self,
        proposal: &super::store::ProposalRecord,
        report: &EvaluationReportRecord,
        artifact: &SkillArtifact,
    ) -> Result<(), AdmissionError> {
        #[cfg(test)]
        self.review_gate_runs.set(self.review_gate_runs.get() + 1);
        artifact
            .verify_identity()
            .map_err(|_| AdmissionError::StaleReview)?;
        if report.outcome != "passed"
            || report.proposal_id != proposal.proposal_id
            || report.skill_id != artifact.id
            || report.predecessor_id != proposal.predecessor_id
            || report.verifier_version != super::verify::VERIFIER_VERSION
            || report.fakes_version != super::fakes::FAKES_VERSION
        {
            return Err(AdmissionError::StaleReview);
        }
        let predecessor = proposal
            .predecessor_id
            .as_deref()
            .map(|id| {
                self.store
                    .get(id)?
                    .ok_or_else(|| AdmissionError::NotFound(id.to_string()))
            })
            .transpose()?;
        if let Some(predecessor) = predecessor.as_ref()
            && !capability_is_non_escalating(&artifact.capability, &predecessor.capability)
        {
            return Err(AdmissionError::StaleReview);
        }
        if self.is_policy_duplicate(artifact, proposal.predecessor_id.as_deref())? {
            return Err(AdmissionError::DuplicateOfActive);
        }
        let held_out = evaluate(&self.store, artifact, predecessor.as_ref())
            .map_err(review_gate_held_out_error)?;
        if held_out.skill_id != artifact.id
            || held_out.predecessor_id != proposal.predecessor_id
            || held_out.suite_hashes != report.suite_hashes
            || held_out.verifier_version != report.verifier_version
            || held_out.fakes_version != report.fakes_version
        {
            return Err(AdmissionError::StaleReview);
        }
        let (Some(model_id), Some(model_revision)) = (
            report.embedding_model_id.as_deref(),
            report.embedding_model_revision.as_deref(),
        ) else {
            return Err(AdmissionError::EmbeddingUnavailable);
        };
        if !self
            .store
            .has_compatible_embedding(&artifact.id, model_id, model_revision)?
        {
            return Err(AdmissionError::EmbeddingUnavailable);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &SkillStore {
        &self.store
    }

    #[cfg(test)]
    pub(crate) fn fail_next_verification_for_test(&mut self, error: VerificationError) {
        self.verification_failure = Some(error);
    }

    /// Fail the next approval-gate re-run with `error`, standing in for a
    /// contained worker that is unavailable at approval time.
    #[cfg(test)]
    pub(crate) fn fail_next_review_gate_for_test(&mut self, error: VerificationError) {
        self.review_gate_failure = Some(error);
    }

    /// Number of contained approval-gate re-runs performed so far.
    #[cfg(test)]
    pub(crate) fn review_gate_runs_for_test(&self) -> usize {
        self.review_gate_runs.get()
    }

    #[cfg(test)]
    pub(crate) fn store_mut(&mut self) -> &mut SkillStore {
        &mut self.store
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn authorize_canary_for_test(
        &mut self,
        authorization_id: &str,
        principal: &str,
        artifact: &SkillArtifact,
        report_id: &str,
        issued_at: i64,
        expires_at: i64,
    ) -> Result<ApprovalAuthorization, AdmissionError> {
        Ok(self
            .store
            .issue_approval_authorization_for_test(ApprovalAuthorizationRequest {
                authorization_id: authorization_id.to_string(),
                principal: principal.to_string(),
                artifact_id: artifact.id.clone(),
                report_id: report_id.to_string(),
                manifest_digest: approval_manifest_digest(artifact)?,
                transition: ApprovalTransition::VerifiedToCanary,
                issued_at,
                expires_at,
            })?)
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn consume_canary_for_test(
        &mut self,
        proposal_id: &str,
        artifact_id: &str,
        report_id: &str,
        artifact_version: u64,
        proposal_version: u64,
        authorization: &ApprovalAuthorization,
        now: i64,
    ) -> Result<CanaryApprovalResult, AdmissionError> {
        Ok(AdmissionStore::new(&mut self.store).approve_canary(
            proposal_id,
            artifact_id,
            report_id,
            artifact_version,
            proposal_version,
            authorization,
            now,
        )?)
    }
}

pub(crate) struct AdmissionWorker {
    shutdown: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    runtime: Option<tokio::runtime::Handle>,
}

impl AdmissionWorker {
    pub(crate) fn start_session_scoped(
        mut evaluator: AdmissionEvaluator,
    ) -> Result<Self, AdmissionError> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let join = std::thread::Builder::new()
            .name("skill-admission".to_string())
            .spawn(move || {
                while !worker_shutdown.load(Ordering::Acquire) {
                    let now = match super::store::current_timestamp() {
                        Ok(now) => now,
                        Err(_) => {
                            std::thread::sleep(WORKER_IDLE_POLL);
                            continue;
                        }
                    };
                    match evaluator.evaluate_next(now) {
                        Ok(Some(report)) => {
                            tracing::info!(
                                proposal_id = %report.proposal_id,
                                skill_id = %report.skill_id,
                                report_id = %report.report_id,
                                outcome = %report.outcome,
                                reason_code = report.reason_code.as_deref().unwrap_or(""),
                                "skill proposal evaluation completed"
                            );
                        }
                        Ok(None) | Err(AdmissionError::Retryable(_)) => {
                            std::thread::sleep(WORKER_IDLE_POLL);
                        }
                        Err(error) => {
                            tracing::warn!(
                                error_kind = %admission_error_kind(&error),
                                "skill admission worker encountered a recoverable error"
                            );
                            std::thread::sleep(WORKER_IDLE_POLL);
                        }
                    }
                }
            })
            .map_err(|_| AdmissionError::WorkerUnavailable)?;
        Ok(Self {
            shutdown,
            join: Some(join),
            runtime: tokio::runtime::Handle::try_current().ok(),
        })
    }
}

impl Drop for AdmissionWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            if join.is_finished() {
                let _ = join.join();
            } else if let Some(runtime) = &self.runtime {
                std::mem::drop(crate::agent::runner::spawn_blocking_scoped_on(
                    runtime,
                    move || {
                        let _ = join.join();
                    },
                ));
            } else {
                let _ = join.join();
            }
        }
    }
}

fn admission_error_kind(error: &AdmissionError) -> &'static str {
    match error {
        AdmissionError::InvalidWorker => "invalid_worker",
        AdmissionError::WorkerUnavailable => "worker_unavailable",
        AdmissionError::Retryable(_) => "retryable",
        AdmissionError::NotFound(_) => "not_found",
        AdmissionError::NotAwaitingApproval => "not_awaiting_approval",
        AdmissionError::MissingReport => "missing_report",
        AdmissionError::Infrastructure(_) => "infrastructure_unavailable",
        AdmissionError::DuplicateOfActive => "duplicate_of_active",
        AdmissionError::EmbeddingUnavailable => "embedding_unavailable",
        AdmissionError::StaleReview => "stale_review",
        AdmissionError::UnauthenticatedApproval => "unauthenticated_approval",
        AdmissionError::CanaryBecameRetrievable => "canary_became_retrievable",
        AdmissionError::Store(_) => "store",
    }
}

fn skill_document(artifact: &SkillArtifact) -> String {
    SkillDocument::new(artifact.description.clone())
        .with_exports(
            artifact
                .exports
                .iter()
                .map(|export| (export.name.clone(), export.signature.clone()))
                .collect(),
        )
        .with_tags(artifact.tags.clone())
        .with_identifiers(
            artifact
                .exports
                .iter()
                .map(|export| export.name.clone())
                .collect(),
        )
        .render()
}

pub(crate) fn capability_is_non_escalating(
    candidate: &CapabilityManifest,
    predecessor: &CapabilityManifest,
) -> bool {
    candidate.tier <= predecessor.tier
        && candidate.grants.iter().all(|candidate_scope| {
            predecessor
                .grants
                .iter()
                .any(|predecessor_scope| scope_is_subset(candidate_scope, predecessor_scope))
        })
}

fn scope_is_subset(candidate: &CapabilityScope, predecessor: &CapabilityScope) -> bool {
    match (candidate, predecessor) {
        (
            CapabilityScope::ReadFile {
                workspace_prefixes: candidate,
            },
            CapabilityScope::ReadFile {
                workspace_prefixes: predecessor,
            },
        )
        | (
            CapabilityScope::WriteFile {
                workspace_prefixes: candidate,
            },
            CapabilityScope::WriteFile {
                workspace_prefixes: predecessor,
            },
        ) => candidate.iter().all(|candidate| {
            predecessor.iter().any(|predecessor| {
                candidate == predecessor
                    || candidate
                        .strip_prefix(predecessor)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
        }),
        (
            CapabilityScope::Fetch {
                origins: candidate_origins,
                methods: candidate_methods,
            },
            CapabilityScope::Fetch {
                origins: predecessor_origins,
                methods: predecessor_methods,
            },
        ) => {
            candidate_origins
                .iter()
                .all(|origin| predecessor_origins.contains(origin))
                && candidate_methods
                    .iter()
                    .all(|method| predecessor_methods.contains(method))
        }
        (
            CapabilityScope::Spawn {
                programs: candidate,
            },
            CapabilityScope::Spawn {
                programs: predecessor,
            },
        ) => candidate
            .iter()
            .all(|program| predecessor.contains(program)),
        _ => false,
    }
}

fn success_report(
    lease: &ProposalLease,
    held_out: &HeldOutEvaluationReport,
    model_id: &str,
    model_revision: &str,
    now: i64,
) -> EvaluationReportRecord {
    build_report(
        lease,
        "passed",
        None,
        held_out.suite_hashes.clone(),
        Some(model_id.to_string()),
        Some(model_revision.to_string()),
        serde_json::json!({
            "embedded": "passed",
            "mutation": "passed",
            "inherited": "passed",
            "held_out": "passed",
            "held_out_case_count": held_out.cases.len(),
            "duplicate": "passed",
            "contract": "passed",
            "embedding": "passed"
        }),
        now,
    )
}

fn rejection_report(
    lease: &ProposalLease,
    reason_code: &str,
    detail: &'static str,
    now: i64,
) -> EvaluationReportRecord {
    build_report(
        lease,
        "rejected",
        Some(reason_code.to_string()),
        Vec::new(),
        None,
        None,
        serde_json::json!({"gate": reason_code, "result": "rejected", "detail": detail}),
        now,
    )
}

fn blocked_report(lease: &ProposalLease, reason_code: &str, now: i64) -> EvaluationReportRecord {
    build_report(
        lease,
        "retryable",
        Some(reason_code.to_string()),
        Vec::new(),
        None,
        None,
        serde_json::json!({"gate": reason_code, "result": "blocked"}),
        now,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_report(
    lease: &ProposalLease,
    outcome: &str,
    reason_code: Option<String>,
    suite_hashes: Vec<String>,
    embedding_model_id: Option<String>,
    embedding_model_revision: Option<String>,
    summary: serde_json::Value,
    now: i64,
) -> EvaluationReportRecord {
    let mut report = EvaluationReportRecord {
        report_id: String::new(),
        proposal_id: lease.proposal_id.clone(),
        skill_id: lease.skill_id.clone(),
        attempt: lease.attempt,
        verifier_version: super::verify::VERIFIER_VERSION,
        fakes_version: super::fakes::FAKES_VERSION,
        suite_hashes,
        predecessor_id: lease.predecessor_id.clone(),
        embedding_model_id,
        embedding_model_revision,
        outcome: outcome.to_string(),
        reason_code,
        summary_json: serde_json::to_string(&summary)
            .expect("report summary serialization uses infallible values"),
        created_at: now,
    };
    report.report_id = report
        .recompute_id()
        .expect("report identity serialization uses infallible values");
    report
}

fn deterministic(code: &'static str, detail: &'static str) -> EvaluationFailure {
    EvaluationFailure::Deterministic { code, detail }
}

fn classify_store(error: StoreError) -> EvaluationFailure {
    match error {
        StoreError::IdentityValidation(_) | StoreError::CorruptRow(_) => {
            deterministic("identity_invalid", "stored artifact is corrupt")
        }
        other => EvaluationFailure::Infrastructure {
            error: other.to_string(),
        },
    }
}

/// Classify an approval-gate verification failure.
///
/// Only a failure attributable to the reviewed record itself may be reported as
/// a stale review; infrastructure outages are retryable and must say so.
fn review_gate_verification_error(error: &VerificationError) -> AdmissionError {
    if error.is_infrastructure() {
        AdmissionError::Infrastructure(error.to_string())
    } else {
        AdmissionError::StaleReview
    }
}

fn review_gate_held_out_error(error: HeldOutError) -> AdmissionError {
    match error {
        HeldOutError::Infrastructure(error)
        | HeldOutError::Embedded(error)
        | HeldOutError::Inherited(error) => review_gate_verification_error(&error),
        HeldOutError::Store(error) => AdmissionError::Store(error),
        _ => AdmissionError::StaleReview,
    }
}

fn classify_verification(
    error: &VerificationError,
    fallback_code: &'static str,
    fallback_detail: &'static str,
) -> EvaluationFailure {
    // Runtime creation, context creation, and the worker verification contract
    // are build- and parent-owned. Attributing them to the candidate would
    // permanently reject an identity that can never be re-proposed.
    if error.is_infrastructure() {
        return EvaluationFailure::Infrastructure {
            error: error.to_string(),
        };
    }
    // Classification is decided on the typed worker diagnostic carried by the
    // error. The rendered diagnostic is `Class/Stage/Role`
    // (`ResourceLimit/Evaluation/SkillSource`), which shares no substring with
    // the outcome names the previous text match looked for, so every genuine
    // source-side resource limit was reported as an embedded-test failure.
    if error.is_resource_limit() {
        deterministic(
            "verification_resource_limit",
            "verification resource limit exceeded",
        )
    } else {
        deterministic(fallback_code, fallback_detail)
    }
}

enum EvaluationFailure {
    Deterministic {
        code: &'static str,
        detail: &'static str,
    },
    SuiteRequired,
    Infrastructure {
        error: String,
    },
}

pub(crate) trait HumanReviewer {
    fn review(&self, packet: &ReviewPacket) -> ReviewDecision;
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ReviewPacket {
    pub proposal_id: String,
    pub report_id: String,
    pub artifact_id: String,
    pub source: String,
    pub description: String,
    pub tags: Vec<String>,
    pub exports: Vec<SkillExport>,
    pub tests: Vec<String>,
    pub capability: CapabilityManifest,
    pub gate_summary_json: String,
    pub verifier_version: u32,
    pub fakes_version: u32,
    pub held_out_suite_hashes: Vec<String>,
    pub embedding_model_id: Option<String>,
    pub embedding_model_revision: Option<String>,
}

impl fmt::Debug for ReviewPacket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReviewPacket")
            .field("proposal_id", &self.proposal_id)
            .field("report_id", &self.report_id)
            .field("artifact_id", &self.artifact_id)
            .field("source", &"<authorized review only>")
            .field("source_len", &self.source.len())
            .field("description", &self.description)
            .field("tags", &self.tags)
            .field("exports", &self.exports.len())
            .field("tests", &"<authorized review only>")
            .field("test_count", &self.tests.len())
            .field("capability_tier", &self.capability.tier)
            .field("gate_summary_json", &self.gate_summary_json)
            .field("verifier_version", &self.verifier_version)
            .field("fakes_version", &self.fakes_version)
            .field("held_out_suite_hashes", &self.held_out_suite_hashes)
            .finish()
    }
}

pub(crate) enum ReviewDecision {
    Approve(AuthenticatedHumanDecision),
    Deny { reason_code: String },
    Cancelled,
    TimedOut,
}

pub(crate) struct AuthenticatedHumanDecision {
    decision_id: String,
    principal: String,
    authenticated_at: i64,
}

impl AuthenticatedHumanDecision {
    /// Mint the fixed local-owner capability for an explicit one-shot CLI
    /// operation. The OS account that can access the private application data
    /// directory is the authentication boundary; no caller-controlled
    /// principal is accepted here.
    pub(crate) fn local_owner(authenticated_at: i64) -> Self {
        Self {
            decision_id: format!("local-owner-review-{}", uuid::Uuid::new_v4()),
            principal: "local-owner".to_string(),
            authenticated_at,
        }
    }

    /// Test-only stand-in for the parent authentication boundary.
    ///
    /// Production code cannot construct this capability from caller-provided strings. The
    /// eventual UI/authentication adapter must live in this module and mint it only after its
    /// authenticated interaction completes.
    #[cfg(test)]
    pub(crate) fn verified(
        decision_id: impl Into<String>,
        principal: impl Into<String>,
        authenticated_at: i64,
    ) -> Self {
        Self {
            decision_id: decision_id.into(),
            principal: principal.into(),
            authenticated_at,
        }
    }

    pub(super) fn authorization_id(&self) -> &str {
        &self.decision_id
    }

    pub(super) fn principal(&self) -> &str {
        &self.principal
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewOutcome {
    Canary(CanaryApprovalResult),
    Denied,
    Cancelled,
    TimedOut,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("invalid worker identity")]
    InvalidWorker,
    #[error("admission worker could not start")]
    WorkerUnavailable,
    #[error("admission infrastructure retry required: {0}")]
    Retryable(String),
    #[error("proposal not found: {0}")]
    NotFound(String),
    #[error("proposal is not awaiting approval")]
    NotAwaitingApproval,
    #[error("evaluation report is missing")]
    MissingReport,
    #[error("verification infrastructure was unavailable while re-running the approval gate: {0}")]
    Infrastructure(String),
    #[error("an active skill already has the same normalized contract")]
    DuplicateOfActive,
    #[error("no compatible embedding is available for the reviewed skill")]
    EmbeddingUnavailable,
    #[error("reviewed proposal changed before approval")]
    StaleReview,
    #[error("approval is not backed by a fresh authenticated human session")]
    UnauthenticatedApproval,
    #[error("canary unexpectedly became retrievable")]
    CanaryBecameRetrievable,
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use crate::extras::js::protocol::{
        Diagnostic, DiagnosticClass, DiagnosticStage, ScriptRole, VerificationResult,
    };
    use crate::extras::js::skills::verify::{
        SourceFailure, TestResult, test_result, validate_worker_result,
    };

    fn assert_never_permanently_rejects(error: &VerificationError, context: &str) {
        let failure = classify_verification(
            error,
            "embedded_test_failed",
            "embedded verification failed",
        );
        assert!(
            matches!(failure, EvaluationFailure::Infrastructure { .. }),
            "{context} must classify as retryable infrastructure; otherwise the proposal identity is permanently rejected"
        );
    }

    #[test]
    fn worker_internal_diagnostic_is_retryable_admission_infrastructure() {
        // `DiagnosticClass::Internal` is emitted only for the worker's own
        // host-side failures (Runtime::new, binding preparation, Context::full,
        // missing trusted bootstrap bytecode).
        let internal = Diagnostic {
            class: DiagnosticClass::Internal,
            stage: DiagnosticStage::Initialization,
            script_role: ScriptRole::SkillSource,
            exception_class: None,
            line: None,
            column: None,
        };
        let error = test_result(Some(&internal))
            .expect_err("a worker internal failure is not a candidate test outcome");
        assert!(matches!(
            error,
            VerificationError::InfrastructureUnavailable(_)
        ));
        assert_never_permanently_rejects(&error, "a worker internal diagnostic");

        // A genuinely skill-attributable diagnostic still classifies as a
        // deterministic outcome, so the fix does not blanket-excuse failures.
        let thrown = Diagnostic {
            class: DiagnosticClass::Exception,
            stage: DiagnosticStage::Evaluation,
            script_role: ScriptRole::EmbeddedTest,
            exception_class: None,
            line: None,
            column: None,
        };
        assert!(matches!(
            test_result(Some(&thrown)).expect("a thrown exception is a test outcome"),
            TestResult::Threw(_)
        ));
        let deterministic_failure = classify_verification(
            &VerificationError::TestFailed {
                index: 0,
                outcome: TestResult::ReturnedFalse,
            },
            "embedded_test_failed",
            "embedded verification failed",
        );
        assert!(matches!(
            deterministic_failure,
            EvaluationFailure::Deterministic { .. }
        ));
    }

    #[test]
    fn worker_verification_contract_mismatch_is_retryable_admission_infrastructure() {
        let case_count_mismatch = VerificationResult {
            passed: false,
            cases: Vec::new(),
            loader_version: 1,
        };
        let error = validate_worker_result(&case_count_mismatch, 1)
            .expect_err("a case-count mismatch breaks the worker contract");
        assert!(matches!(error, VerificationError::RuntimeCreationFailed(_)));
        assert_never_permanently_rejects(&error, "a worker verification case-count mismatch");

        let loader_skew = VerificationResult {
            passed: false,
            cases: Vec::new(),
            loader_version: u16::MAX,
        };
        let error = validate_worker_result(&loader_skew, 0)
            .expect_err("loader-version skew breaks the worker contract");
        assert_never_permanently_rejects(&error, "worker loader-version skew");

        assert_never_permanently_rejects(
            &VerificationError::ContextCreationFailed("allocation failed".to_string()),
            "a worker context creation failure",
        );
    }

    #[test]
    fn source_resource_limits_classify_on_the_typed_diagnostic() {
        fn source_failure(class: DiagnosticClass, stage: DiagnosticStage) -> VerificationError {
            VerificationError::SourceEvaluationFailed(SourceFailure::from_diagnostic(&Diagnostic {
                class,
                stage,
                script_role: ScriptRole::SkillSource,
                exception_class: None,
                line: None,
                column: None,
            }))
        }

        // The worker renders these as `ResourceLimit/Evaluation/SkillSource`
        // and `Contract/JobDrain/SkillSource`; neither contains any of the
        // substrings the previous text match looked for.
        for error in [
            source_failure(DiagnosticClass::ResourceLimit, DiagnosticStage::Evaluation),
            source_failure(DiagnosticClass::Contract, DiagnosticStage::JobDrain),
            VerificationError::MutationPassFailed {
                export: "run".to_string(),
                reason: "ResourceLimit/Evaluation/SkillSource".to_string(),
                diagnostic: Some(Diagnostic {
                    class: DiagnosticClass::ResourceLimit,
                    stage: DiagnosticStage::Evaluation,
                    script_role: ScriptRole::SkillSource,
                    exception_class: None,
                    line: None,
                    column: None,
                }),
            },
            VerificationError::TestFailed {
                index: 0,
                outcome: TestResult::JobLimitExceeded,
            },
        ] {
            assert!(
                matches!(
                    classify_verification(
                        &error,
                        "embedded_test_failed",
                        "embedded verification failed",
                    ),
                    EvaluationFailure::Deterministic {
                        code: "verification_resource_limit",
                        ..
                    }
                ),
                "a candidate resource limit must be reported as such: {error}"
            );
        }

        // A thrown exception in the source is still an ordinary failure.
        assert!(matches!(
            classify_verification(
                &source_failure(DiagnosticClass::Exception, DiagnosticStage::Evaluation),
                "embedded_test_failed",
                "embedded verification failed",
            ),
            EvaluationFailure::Deterministic {
                code: "embedded_test_failed",
                ..
            }
        ));
    }

    #[test]
    fn verification_scheduler_queue_outage_is_retryable_admission_infrastructure() {
        let failure = classify_verification(
            &VerificationError::InfrastructureUnavailable("queue full".into()),
            "embedded_test_failed",
            "embedded verification failed",
        );
        assert!(matches!(failure, EvaluationFailure::Infrastructure { .. }));
    }

    #[test]
    fn evaluation_lease_covers_the_bounded_held_out_worst_case() {
        const MAX_HELD_OUT_CASES: i64 = 64;
        const WORKER_CASE_DEADLINE_SECONDS: i64 = 30;
        const {
            assert!(
                EVALUATION_LEASE_SECONDS
                    >= MAX_HELD_OUT_CASES * WORKER_CASE_DEADLINE_SECONDS + 5 * 60
            );
        }
    }
}
