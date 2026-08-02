//! Private optimistic Phase 4 lifecycle transitions.
//!
//! Only the admission service can construct authenticated approval evidence and
//! call these operations. No public store API can move a revision to canary or
//! active.

use super::SkillArtifact;
use super::store::{
    ApprovalAuthorization, ApprovalAuthorizationRequest, ApprovalTransition, CanaryApprovalInput,
    CanaryApprovalResult, SkillStore, StoreError, approval_manifest_digest,
};

pub(super) struct AdmissionStore<'a> {
    store: &'a mut SkillStore,
}

impl<'a> AdmissionStore<'a> {
    pub(super) fn new(store: &'a mut SkillStore) -> Self {
        Self { store }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn authorize_canary(
        &mut self,
        authorization_id: String,
        principal: String,
        artifact: &SkillArtifact,
        report_id: &str,
        issued_at: i64,
        expires_at: i64,
    ) -> Result<ApprovalAuthorization, StoreError> {
        self.store
            .issue_approval_authorization(ApprovalAuthorizationRequest {
                authorization_id,
                principal,
                artifact_id: artifact.id.clone(),
                report_id: report_id.to_string(),
                manifest_digest: approval_manifest_digest(artifact)?,
                transition: ApprovalTransition::VerifiedToCanary,
                issued_at,
                expires_at,
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn approve_canary(
        &mut self,
        proposal_id: &str,
        skill_id: &str,
        report_id: &str,
        expected_artifact_version: u64,
        expected_proposal_version: u64,
        authorization: &ApprovalAuthorization,
        now: i64,
    ) -> Result<CanaryApprovalResult, StoreError> {
        self.store.approve_canary_transaction(
            &CanaryApprovalInput {
                proposal_id: proposal_id.to_string(),
                skill_id: skill_id.to_string(),
                report_id: report_id.to_string(),
                expected_artifact_version,
                expected_proposal_version,
            },
            authorization,
            now,
        )
    }

    pub(super) fn deny(
        &mut self,
        proposal_id: &str,
        expected_proposal_version: u64,
        reason_code: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        self.store.deny_approval_transaction(
            proposal_id,
            expected_proposal_version,
            reason_code,
            now,
        )
    }
}
