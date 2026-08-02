//! Parent-only authority checks for effects requested by the JavaScript worker.
//!
//! Worker-provided grant and attribution fields are untrusted lookup hints. This module resolves
//! them against parent-created invocation grants, applies every fail-closed preflight, and only
//! then hands an authorized operation to the parent effect-service seam.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use uuid::Uuid;

use super::protocol::{
    AdvisoryAttribution, EffectError, EffectErrorCode, EffectRequest, GrantId, InvocationId,
};
pub(crate) use super::protocol::{EffectOperation, EffectResult};
use super::supervisor::{EffectFuture, InvocationEffectHandler};
use super::types::EffectServiceError;
use super::types::PermCancellation;

pub(crate) type ParentEffectFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Closed coarse capabilities understood by the invocation broker.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum HostCapability {
    ReadFile,
    WriteFile,
    Fetch,
    Spawn,
    ProposeSkill,
}

impl HostCapability {
    pub(crate) fn all() -> BTreeSet<Self> {
        BTreeSet::from([
            Self::ReadFile,
            Self::WriteFile,
            Self::Fetch,
            Self::Spawn,
            Self::ProposeSkill,
        ])
    }

    fn for_operation(operation: &EffectOperation) -> Self {
        match operation {
            EffectOperation::ReadFile { .. } => Self::ReadFile,
            EffectOperation::WriteFile { .. } => Self::WriteFile,
            EffectOperation::Fetch { .. } => Self::Fetch,
            EffectOperation::Spawn { .. } => Self::Spawn,
            EffectOperation::ProposeSkill { .. } => Self::ProposeSkill,
        }
    }
}

/// Parent-authoritative identity attached to one invocation grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GrantPrincipal {
    ModelAuthored {
        tool_call_id: String,
    },
    Skill {
        artifact_id: String,
        export: String,
        invocation_id: String,
    },
}

/// Immutable authority issued by the parent for exactly one invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InvocationGrant {
    grant_id: GrantId,
    principal: GrantPrincipal,
    allowed: BTreeSet<HostCapability>,
    bound_invocation: InvocationId,
    expires_at: Instant,
}

impl InvocationGrant {
    /// Issues an opaque, non-nil ID. Callers cannot supply worker-chosen grant identity.
    pub(crate) fn issue(
        bound_invocation: InvocationId,
        principal: GrantPrincipal,
        allowed: BTreeSet<HostCapability>,
        expires_at: Instant,
    ) -> Self {
        Self {
            grant_id: GrantId::new(Uuid::new_v4()).expect("UUID v4 is non-nil"),
            principal,
            allowed,
            bound_invocation,
            expires_at,
        }
    }

    pub(crate) fn grant_id(&self) -> &GrantId {
        &self.grant_id
    }
}

/// Source-free parent error classes. Only their closed wire mapping crosses the worker boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum HostEffectError {
    #[error("unknown invocation grant")]
    UnknownGrant,
    #[error("invocation grant has already been retired")]
    ReplayedGrant,
    #[error("invocation grant has expired")]
    ExpiredGrant,
    #[error("invocation grant belongs to another invocation")]
    WrongInvocation,
    #[error("worker attribution does not match parent authority")]
    AttributionMismatch,
    #[error("invocation has reached a terminal state")]
    InvocationTerminal,
    #[error("invocation is cancelled")]
    InvocationCancelled,
    #[error("worker process has been recycled")]
    InvocationRecycled,
    #[error("session policy denies the operation")]
    SessionDenied,
    #[error("invocation grant denies the operation")]
    CapabilityDenied,
    #[error("operation target is invalid")]
    InvalidTarget,
    #[error("operation target is outside the allowed scope")]
    TargetDenied,
    #[error("permission policy denies the operation")]
    PermissionDenied,
    #[error("permission prompt timed out")]
    AskTimedOut,
    #[error("effect execution timed out")]
    EffectTimedOut,
    #[error("effect output exceeded its limit")]
    OutputLimit,
    #[error("effect backend is unavailable")]
    BackendFailure,
    #[error("effect outcome is unknown after dispatch")]
    OutcomeUnknown,
}

impl HostEffectError {
    fn wire_code(self) -> EffectErrorCode {
        match self {
            Self::InvalidTarget | Self::TargetDenied => EffectErrorCode::InvalidTarget,
            Self::InvocationCancelled => EffectErrorCode::Cancelled,
            Self::AskTimedOut | Self::EffectTimedOut => EffectErrorCode::TimedOut,
            Self::OutputLimit => EffectErrorCode::OutputLimit,
            Self::BackendFailure => EffectErrorCode::BackendFailure,
            Self::OutcomeUnknown => EffectErrorCode::OutcomeUnknown,
            Self::UnknownGrant
            | Self::ReplayedGrant
            | Self::ExpiredGrant
            | Self::WrongInvocation
            | Self::AttributionMismatch
            | Self::InvocationTerminal
            | Self::InvocationRecycled
            | Self::SessionDenied
            | Self::CapabilityDenied
            | Self::PermissionDenied => EffectErrorCode::Denied,
        }
    }

    fn into_wire_result(self) -> EffectResult {
        EffectResult::Error(EffectError {
            code: self.wire_code(),
        })
    }
}

impl From<EffectServiceError> for HostEffectError {
    fn from(error: EffectServiceError) -> Self {
        match error {
            EffectServiceError::InvalidTarget
            | EffectServiceError::FinalSymlink
            | EffectServiceError::TargetChanged
            | EffectServiceError::InvalidBody => Self::InvalidTarget,
            EffectServiceError::TargetDenied
            | EffectServiceError::FileNoConfiguredRoots
            | EffectServiceError::FileInvalidConfiguration
            | EffectServiceError::FileOutsideConfiguredRoots => Self::TargetDenied,
            EffectServiceError::PermissionDenied | EffectServiceError::DoomLoopDenied => {
                Self::PermissionDenied
            }
            EffectServiceError::PermissionTimedOut => Self::AskTimedOut,
            EffectServiceError::Cancelled => Self::InvocationCancelled,
            EffectServiceError::TimedOut => Self::EffectTimedOut,
            EffectServiceError::OutputLimit | EffectServiceError::BodyLimit => Self::OutputLimit,
            EffectServiceError::BackendFailure => Self::BackendFailure,
            EffectServiceError::OutcomeUnknown => Self::OutcomeUnknown,
        }
    }
}

/// Parent-derived context delivered to the effect service after all broker preflights pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthorizedEffect {
    invocation_id: InvocationId,
    grant_id: GrantId,
    principal: GrantPrincipal,
    capability: HostCapability,
}

impl AuthorizedEffect {
    pub(crate) fn invocation_id(&self) -> &InvocationId {
        &self.invocation_id
    }

    pub(crate) fn grant_id(&self) -> &GrantId {
        &self.grant_id
    }

    pub(crate) fn principal(&self) -> &GrantPrincipal {
        &self.principal
    }

    pub(crate) fn capability(&self) -> HostCapability {
        self.capability
    }
}

/// Abstract parent effect seam. A12 supplies the real host implementations.
pub(crate) trait ParentEffectService: Send {
    fn validate_target(
        &mut self,
        authorized: &AuthorizedEffect,
        operation: &EffectOperation,
    ) -> Result<(), HostEffectError>;

    fn ensure_backend(
        &mut self,
        authorized: &AuthorizedEffect,
        operation: &EffectOperation,
    ) -> Result<(), HostEffectError>;

    fn authorize<'a>(
        &'a mut self,
        authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        cancellation: PermCancellation,
    ) -> ParentEffectFuture<'a, Result<(), HostEffectError>>;

    fn execute<'a>(
        &'a mut self,
        authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        cancellation: PermCancellation,
    ) -> ParentEffectFuture<'a, Result<EffectResult, HostEffectError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InvocationState {
    Active,
    Terminal,
    Cancelled,
    Recycled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum BrokerBuildError {
    #[error("duplicate invocation grant")]
    DuplicateGrant,
}

/// One invocation's parent-owned grant table and policy state.
pub(crate) struct InvocationBroker<S> {
    invocation_id: InvocationId,
    grants: HashMap<GrantId, InvocationGrant>,
    retired_grants: HashSet<GrantId>,
    session_allowed: BTreeSet<HostCapability>,
    state: InvocationState,
    service: S,
}

impl<S: ParentEffectService> InvocationBroker<S> {
    pub(crate) fn new(
        invocation_id: InvocationId,
        grants: Vec<InvocationGrant>,
        session_allowed: BTreeSet<HostCapability>,
        service: S,
    ) -> Result<Self, BrokerBuildError> {
        let mut grant_table = HashMap::with_capacity(grants.len());
        for grant in grants {
            if grant_table.insert(grant.grant_id.clone(), grant).is_some() {
                return Err(BrokerBuildError::DuplicateGrant);
            }
        }
        Ok(Self {
            invocation_id,
            grants: grant_table,
            retired_grants: HashSet::new(),
            session_allowed,
            state: InvocationState::Active,
            service,
        })
    }

    pub(crate) async fn dispatch(
        &mut self,
        request: EffectRequest,
        cancellation: PermCancellation,
    ) -> Result<EffectResult, HostEffectError> {
        self.ensure_active()?;
        if cancellation.is_cancelled() {
            self.cancel_invocation();
            return Err(HostEffectError::InvocationCancelled);
        }

        let grant = match self.grants.get(&request.grant_id) {
            Some(grant) => grant.clone(),
            None if self.retired_grants.contains(&request.grant_id) => {
                return Err(HostEffectError::ReplayedGrant);
            }
            None => return Err(HostEffectError::UnknownGrant),
        };

        if grant.bound_invocation != self.invocation_id
            || matches!(
                &grant.principal,
                GrantPrincipal::Skill { invocation_id, .. }
                    if invocation_id != self.invocation_id.as_str()
            )
        {
            return Err(HostEffectError::WrongInvocation);
        }

        self.ensure_grant_unexpired(&request.grant_id, grant.expires_at)?;

        if !attribution_matches(&grant.principal, &request.advisory) {
            return Err(HostEffectError::AttributionMismatch);
        }

        let capability = HostCapability::for_operation(&request.operation);
        if !grant.allowed.contains(&capability) {
            return Err(HostEffectError::CapabilityDenied);
        }
        if !self.session_allowed.contains(&capability) {
            return Err(HostEffectError::SessionDenied);
        }
        if capability == HostCapability::ProposeSkill
            && matches!(grant.principal, GrantPrincipal::Skill { .. })
        {
            return Err(HostEffectError::CapabilityDenied);
        }

        let authorized = AuthorizedEffect {
            invocation_id: self.invocation_id.clone(),
            grant_id: grant.grant_id,
            principal: grant.principal,
            capability,
        };
        self.service
            .validate_target(&authorized, &request.operation)?;
        self.service
            .ensure_backend(&authorized, &request.operation)?;

        let authorization_result = {
            let authorization =
                self.service
                    .authorize(&authorized, &request.operation, cancellation.clone());
            tokio::pin!(authorization);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(HostEffectError::InvocationCancelled),
                result = &mut authorization => result,
            }
        };
        if authorization_result == Err(HostEffectError::InvocationCancelled) {
            self.cancel_invocation();
        }
        authorization_result?;

        if cancellation.is_cancelled() {
            self.cancel_invocation();
            return Err(HostEffectError::InvocationCancelled);
        }
        self.ensure_grant_unexpired(&request.grant_id, grant.expires_at)?;

        let execution_result = self
            .service
            .execute(&authorized, &request.operation, cancellation)
            .await;
        if matches!(execution_result, Err(HostEffectError::InvocationCancelled)) {
            self.cancel_invocation();
        }
        execution_result
    }

    pub(crate) fn revoke_grant(&mut self, grant_id: &GrantId) -> bool {
        if self.grants.remove(grant_id).is_some() {
            self.retired_grants.insert(grant_id.clone());
            true
        } else {
            false
        }
    }

    pub(crate) fn finish(&mut self) {
        self.erase_authority(InvocationState::Terminal);
    }

    pub(crate) fn cancel_invocation(&mut self) {
        self.erase_authority(InvocationState::Cancelled);
    }

    pub(crate) fn recycle(&mut self) {
        self.erase_authority(InvocationState::Recycled);
    }

    pub(crate) fn tracked_grant_count(&self) -> usize {
        self.grants.len() + self.retired_grants.len()
    }

    fn ensure_active(&self) -> Result<(), HostEffectError> {
        match self.state {
            InvocationState::Active => Ok(()),
            InvocationState::Terminal => Err(HostEffectError::InvocationTerminal),
            InvocationState::Cancelled => Err(HostEffectError::InvocationCancelled),
            InvocationState::Recycled => Err(HostEffectError::InvocationRecycled),
        }
    }

    fn ensure_grant_unexpired(
        &mut self,
        grant_id: &GrantId,
        expires_at: Instant,
    ) -> Result<(), HostEffectError> {
        if Instant::now() < expires_at {
            return Ok(());
        }
        self.grants.remove(grant_id);
        self.retired_grants.insert(grant_id.clone());
        Err(HostEffectError::ExpiredGrant)
    }

    fn erase_authority(&mut self, state: InvocationState) {
        self.grants.clear();
        self.retired_grants.clear();
        self.state = state;
    }
}

impl<S: ParentEffectService> InvocationEffectHandler for InvocationBroker<S> {
    fn handle_effect(
        &mut self,
        request: EffectRequest,
        cancellation: PermCancellation,
    ) -> EffectFuture<'_> {
        Box::pin(async move {
            match self.dispatch(request, cancellation).await {
                Ok(result) => result,
                Err(error) => error.into_wire_result(),
            }
        })
    }
}

fn attribution_matches(principal: &GrantPrincipal, advisory: &AdvisoryAttribution) -> bool {
    match principal {
        GrantPrincipal::ModelAuthored { .. } => {
            advisory.artifact_id.is_none() && advisory.export.is_none()
        }
        GrantPrincipal::Skill {
            artifact_id,
            export,
            ..
        } => {
            advisory.artifact_id.as_deref() == Some(artifact_id.as_str())
                && advisory.export.as_deref() == Some(export.as_str())
        }
    }
}
