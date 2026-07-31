//! Private optimistic Phase 4 lifecycle transitions.
//!
//! Only the admission service can construct authenticated approval evidence and
//! call these operations. No public store API can move a revision to canary or
//! active.

use super::store::{CanaryApprovalInput, CanaryApprovalResult, SkillStore, StoreError};

#[derive(Debug, Clone)]
pub(super) struct AuthenticatedApproval {
    approval_id: String,
    approver_id: String,
    authenticated_at: i64,
}

impl AuthenticatedApproval {
    pub(super) fn new(
        approval_id: String,
        approver_id: String,
        authenticated_at: i64,
    ) -> Result<Self, StoreError> {
        if approval_id.trim().is_empty()
            || approver_id.trim().is_empty()
            || approval_id.len() > 256
            || approver_id.len() > 256
        {
            return Err(StoreError::Unauthorized);
        }
        Ok(Self {
            approval_id,
            approver_id,
            authenticated_at,
        })
    }
}

pub(super) struct AdmissionStore<'a> {
    store: &'a mut SkillStore,
}

impl<'a> AdmissionStore<'a> {
    pub(super) fn new(store: &'a mut SkillStore) -> Self {
        Self { store }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn approve_canary(
        &mut self,
        proposal_id: &str,
        skill_id: &str,
        report_id: &str,
        expected_artifact_version: u64,
        expected_proposal_version: u64,
        approval: &AuthenticatedApproval,
        now: i64,
    ) -> Result<CanaryApprovalResult, StoreError> {
        self.store.approve_canary_transaction(
            &CanaryApprovalInput {
                approval_id: approval.approval_id.clone(),
                proposal_id: proposal_id.to_string(),
                skill_id: skill_id.to_string(),
                report_id: report_id.to_string(),
                approver_id: approval.approver_id.clone(),
                authenticated_at: approval.authenticated_at,
                expected_artifact_version,
                expected_proposal_version,
            },
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
