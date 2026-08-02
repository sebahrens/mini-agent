//! Parent-only authority checks for effects requested by the JavaScript worker.
//!
//! Worker-provided grant and attribution fields are untrusted lookup hints. This module resolves
//! them against parent-created invocation grants, applies every fail-closed preflight, and only
//! then hands an authorized operation to the parent effect-service seam.

#[cfg(feature = "skills")]
use std::collections::BTreeMap;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::audit::{
    AuditCapability, AuditDecision, AuditError, AuditResultCode, EffectAudit, EffectCompletion,
    EffectIntent, SanitizedTarget,
};
use super::protocol::{
    AdvisoryAttribution, EffectError, EffectErrorCode, EffectRequest, GrantId, InvocationId,
};
pub(crate) use super::protocol::{EffectOperation, EffectResult};
#[cfg(feature = "skills")]
use super::skills::{
    CapabilityManifest, CapabilityScope, HostCapability as SkillHostCapability,
    HttpMethod as SkillHttpMethod,
};
use super::supervisor::{EffectFuture, InvocationEffectHandler};
use super::types::EffectServiceError;
use super::types::PermCancellation;

pub(crate) type ParentEffectFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub(crate) type SharedEffectAudit = Arc<Mutex<EffectAudit>>;

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
    #[cfg(feature = "skills")]
    manifest: Option<CapabilityManifest>,
    #[cfg(feature = "skills")]
    spawn_program_identities: BTreeMap<String, SpawnExecutableIdentity>,
}

#[cfg(feature = "skills")]
const MAX_SPAWN_PROGRAM_BINDINGS: usize = 256;
const MAX_RESOLVED_EXECUTABLE_BYTES: usize = 4096;
pub(crate) const MAX_SPAWN_EXECUTABLE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CONCURRENT_EXECUTABLE_PREPARATIONS: usize = 4;

fn executable_preparation_slots() -> Arc<tokio::sync::Semaphore> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    SLOTS
        .get_or_init(|| {
            Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_EXECUTABLE_PREPARATIONS,
            ))
        })
        .clone()
}

/// Cooperative stop state shared with one bounded executable-preparation worker.
///
/// Filesystem calls such as a FUSE `read` can remain blocked in the kernel, so the async caller
/// never waits for worker termination after cancellation. The worker keeps its bounded slot and
/// observes this state before the next filesystem/copy step; any late result is dropped by the
/// closed result channel, which closes owned files and sealed snapshots.
pub(crate) struct ExecutablePreparationControl {
    cancelled: AtomicBool,
    deadline: Instant,
}

impl ExecutablePreparationControl {
    fn new(deadline: Instant) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            deadline,
        }
    }

    pub(crate) fn checkpoint(&self) -> Result<(), ExecutablePreparationWaitError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(ExecutablePreparationWaitError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(ExecutablePreparationWaitError::TimedOut);
        }
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(deadline: Instant) -> Self {
        Self::new(deadline)
    }
}

struct CancelExecutablePreparationOnDrop(Arc<ExecutablePreparationControl>);

impl Drop for CancelExecutablePreparationOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutablePreparationWaitError {
    Cancelled,
    TimedOut,
    WorkerFailed,
}

/// Runs potentially blocking executable resolution/snapshot work without occupying an async
/// runtime thread. Queueing and work share one absolute deadline, and blocked calls cannot grow
/// the runtime's blocking pool without bound because the permit lives until the worker unwinds.
pub(crate) async fn run_executable_preparation<T, F>(
    deadline: Instant,
    cancellation: PermCancellation,
    work: F,
) -> Result<T, ExecutablePreparationWaitError>
where
    T: Send + 'static,
    F: FnOnce(Arc<ExecutablePreparationControl>) -> T + Send + 'static,
{
    if cancellation.is_cancelled() {
        return Err(ExecutablePreparationWaitError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(ExecutablePreparationWaitError::TimedOut);
    }

    let control = Arc::new(ExecutablePreparationControl::new(deadline));
    let _cancel_on_drop = CancelExecutablePreparationOnDrop(control.clone());
    let slots = executable_preparation_slots();
    let permit = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            return Err(ExecutablePreparationWaitError::Cancelled);
        }
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            return Err(ExecutablePreparationWaitError::TimedOut);
        }
        permit = slots.acquire_owned() => {
            permit.map_err(|_| ExecutablePreparationWaitError::WorkerFailed)?
        }
    };

    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = work(control);
        // If the caller has cancelled or timed out, dropping the failed send closes all resources
        // owned by a late prepared result on this worker thread.
        let _ = sender.send(result);
    });

    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ExecutablePreparationWaitError::Cancelled),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            Err(ExecutablePreparationWaitError::TimedOut)
        }
        result = receiver => result.map_err(|_| ExecutablePreparationWaitError::WorkerFailed),
    }
}

/// Stable parent-owned identity for one resolved executable. The canonical path is the
/// permission/audit label; platform identity rejects path replacement and the bounded content
/// digest rejects in-place mutation that preserves device/inode identity.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct SpawnExecutableIdentity {
    canonical_path: String,
    platform: PlatformExecutableIdentity,
    content_sha256: String,
    content_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
enum PlatformExecutableIdentity {
    Unix {
        device: u64,
        inode: u64,
    },
    Windows {
        volume_serial_number: u32,
        file_index: u64,
    },
}

impl SpawnExecutableIdentity {
    pub(crate) fn canonical_path(&self) -> &str {
        &self.canonical_path
    }

    pub(crate) fn matches_metadata(&self, metadata: &std::fs::Metadata) -> bool {
        platform_executable_identity(metadata).is_some_and(|identity| identity == self.platform)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn with_content(self, content: ExecutableContent) -> Self {
        Self {
            content_sha256: content.sha256,
            content_bytes: content.bytes,
            ..self
        }
    }

    #[cfg(test)]
    pub(crate) fn with_content_sha256_for_test(mut self, content_sha256: String) -> Self {
        self.content_sha256 = content_sha256;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExecutableContent {
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutableCopyError {
    Read,
    Write,
    TooLarge,
    Cancelled,
    TimedOut,
}

/// Copies and hashes one executable version without ever accepting an unbounded input.
pub(crate) fn copy_and_hash_executable(
    source: &mut impl Read,
    destination: &mut impl Write,
) -> Result<ExecutableContent, ExecutableCopyError> {
    copy_and_hash_executable_inner(source, destination, None)
}

pub(crate) fn copy_and_hash_executable_controlled(
    source: &mut impl Read,
    destination: &mut impl Write,
    control: &ExecutablePreparationControl,
) -> Result<ExecutableContent, ExecutableCopyError> {
    copy_and_hash_executable_inner(source, destination, Some(control))
}

fn copy_and_hash_executable_inner(
    source: &mut impl Read,
    destination: &mut impl Write,
    control: Option<&ExecutablePreparationControl>,
) -> Result<ExecutableContent, ExecutableCopyError> {
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        executable_copy_checkpoint(control)?;
        let read = source
            .read(&mut buffer)
            .map_err(|_| ExecutableCopyError::Read)?;
        executable_copy_checkpoint(control)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(ExecutableCopyError::TooLarge)?;
        if total > MAX_SPAWN_EXECUTABLE_BYTES {
            return Err(ExecutableCopyError::TooLarge);
        }
        destination
            .write_all(&buffer[..read])
            .map_err(|_| ExecutableCopyError::Write)?;
        digest.update(&buffer[..read]);
    }
    Ok(ExecutableContent {
        sha256: format!("{:x}", digest.finalize()),
        bytes: total,
    })
}

fn executable_copy_checkpoint(
    control: Option<&ExecutablePreparationControl>,
) -> Result<(), ExecutableCopyError> {
    match control.map(ExecutablePreparationControl::checkpoint) {
        Some(Err(ExecutablePreparationWaitError::Cancelled)) => Err(ExecutableCopyError::Cancelled),
        Some(Err(ExecutablePreparationWaitError::TimedOut)) => Err(ExecutableCopyError::TimedOut),
        Some(Err(ExecutablePreparationWaitError::WorkerFailed)) => {
            unreachable!("worker failure cannot originate inside preparation work")
        }
        Some(Ok(())) | None => Ok(()),
    }
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
            #[cfg(feature = "skills")]
            manifest: None,
            #[cfg(feature = "skills")]
            spawn_program_identities: BTreeMap::new(),
        }
    }

    #[cfg(feature = "skills")]
    pub(crate) async fn issue_scoped_skill(
        bound_invocation: InvocationId,
        principal: GrantPrincipal,
        manifest: CapabilityManifest,
        expires_at: Instant,
        cancellation: PermCancellation,
    ) -> Result<Self, BrokerBuildError> {
        let result = run_executable_preparation(expires_at, cancellation, move |control| {
            Self::issue_scoped_skill_with_resolver_inner(
                bound_invocation,
                principal,
                manifest,
                expires_at,
                |program| resolve_program_identity_controlled(program, &control),
            )
        })
        .await
        .map_err(|error| match error {
            ExecutablePreparationWaitError::Cancelled => BrokerBuildError::Cancelled,
            ExecutablePreparationWaitError::TimedOut => BrokerBuildError::TimedOut,
            ExecutablePreparationWaitError::WorkerFailed => {
                BrokerBuildError::ExecutablePreparationFailed
            }
        })?;
        result
    }

    #[cfg(feature = "skills")]
    fn issue_scoped_skill_with_resolver_inner(
        bound_invocation: InvocationId,
        principal: GrantPrincipal,
        manifest: CapabilityManifest,
        expires_at: Instant,
        resolver: impl Fn(&str) -> Result<SpawnExecutableIdentity, EffectServiceError>,
    ) -> Result<Self, BrokerBuildError> {
        if !matches!(principal, GrantPrincipal::Skill { .. }) {
            return Err(BrokerBuildError::InvalidScopedPrincipal);
        }
        manifest
            .validate()
            .map_err(|_| BrokerBuildError::InvalidManifest)?;
        let spawn_programs = manifest
            .grants
            .iter()
            .filter_map(|scope| match scope {
                CapabilityScope::Spawn { programs } => Some(programs),
                _ => None,
            })
            .flatten();
        let mut spawn_program_identities = BTreeMap::new();
        for program in spawn_programs {
            if spawn_program_identities.len() >= MAX_SPAWN_PROGRAM_BINDINGS {
                return Err(BrokerBuildError::InvalidManifest);
            }
            let identity = resolver(program).map_err(|error| match error {
                EffectServiceError::Cancelled => BrokerBuildError::Cancelled,
                EffectServiceError::TimedOut => BrokerBuildError::TimedOut,
                _ => BrokerBuildError::UnavailableManifestProgram,
            })?;
            if identity.canonical_path.len() > MAX_RESOLVED_EXECUTABLE_BYTES {
                return Err(BrokerBuildError::InvalidManifest);
            }
            spawn_program_identities.insert(program.clone(), identity);
        }
        let allowed = manifest
            .grants
            .iter()
            .map(|scope| match scope.capability() {
                SkillHostCapability::ReadFile => HostCapability::ReadFile,
                SkillHostCapability::WriteFile => HostCapability::WriteFile,
                SkillHostCapability::Fetch => HostCapability::Fetch,
                SkillHostCapability::Spawn => HostCapability::Spawn,
            })
            .collect();
        let mut grant = Self::issue(bound_invocation, principal, allowed, expires_at);
        grant.manifest = Some(manifest);
        grant.spawn_program_identities = spawn_program_identities;
        Ok(grant)
    }

    #[cfg(all(test, feature = "skills"))]
    pub(crate) fn issue_scoped_skill_with_resolver(
        bound_invocation: InvocationId,
        principal: GrantPrincipal,
        manifest: CapabilityManifest,
        expires_at: Instant,
        resolver: impl Fn(&str) -> Result<SpawnExecutableIdentity, EffectServiceError>,
    ) -> Result<Self, BrokerBuildError> {
        Self::issue_scoped_skill_with_resolver_inner(
            bound_invocation,
            principal,
            manifest,
            expires_at,
            resolver,
        )
    }

    #[cfg(all(test, feature = "skills"))]
    pub(crate) fn spawn_program_bindings_json_for_test(&self) -> String {
        serde_json::to_string(&self.spawn_program_identities).expect("bindings serialize")
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
    #[error("skill manifest denies the operation target")]
    ManifestDenied,
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
    #[error("effect audit is unavailable")]
    AuditFailure,
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
            Self::AuditFailure => EffectErrorCode::AuditFailure,
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
            | Self::ManifestDenied
            | Self::PermissionDenied => EffectErrorCode::Denied,
        }
    }

    fn into_wire_result(self) -> EffectResult {
        EffectResult::Error(EffectError {
            code: self.wire_code(),
        })
    }
}

impl From<AuditError> for HostEffectError {
    fn from(_error: AuditError) -> Self {
        Self::AuditFailure
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

/// Exact parent-normalized target returned by authorization and consumed only to derive
/// redacted audit metadata. Raw values never enter the persisted audit record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AuthorizedTarget {
    ReadFile {
        canonical_path: String,
    },
    WriteFile {
        canonical_path: String,
    },
    Fetch {
        normalized_url: String,
        method: String,
    },
    Spawn {
        resolved_executable: String,
    },
    ProposeSkill,
}

/// Parent-normalized, source-free target used only for manifest intersection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NormalizedTarget {
    ReadFile {
        workspace_relative: Option<String>,
    },
    WriteFile {
        workspace_relative: Option<String>,
    },
    Fetch {
        origin: String,
        method: String,
    },
    Spawn {
        program: String,
        resolved_executable: SpawnExecutableIdentity,
    },
    ProposeSkill,
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
    /// Releases any normalized-but-not-executed resource after a later policy layer denies it.
    fn discard_prepared(&mut self) {}

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

    fn normalize_target<'a>(
        &'a mut self,
        authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        cancellation: PermCancellation,
    ) -> ParentEffectFuture<'a, Result<NormalizedTarget, HostEffectError>>;

    fn authorize<'a>(
        &'a mut self,
        authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        cancellation: PermCancellation,
    ) -> ParentEffectFuture<'a, Result<AuthorizedTarget, HostEffectError>>;

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
    #[error("scoped grant principal is not a learned skill")]
    InvalidScopedPrincipal,
    #[error("scoped grant manifest is invalid")]
    InvalidManifest,
    #[error("scoped grant manifest names an unavailable executable")]
    UnavailableManifestProgram,
    #[error("scoped grant construction was cancelled")]
    Cancelled,
    #[error("scoped grant construction exceeded its deadline")]
    TimedOut,
    #[error("scoped grant executable preparation worker failed")]
    ExecutablePreparationFailed,
}

/// One invocation's parent-owned grant table and policy state.
pub(crate) struct InvocationBroker<S> {
    invocation_id: InvocationId,
    grants: HashMap<GrantId, InvocationGrant>,
    retired_grants: HashSet<GrantId>,
    session_allowed: BTreeSet<HostCapability>,
    state: InvocationState,
    service: S,
    audit: SharedEffectAudit,
    #[cfg(test)]
    fail_completion_durability: Option<super::audit::AuditFailurePoint>,
}

impl<S: ParentEffectService> InvocationBroker<S> {
    pub(crate) fn new(
        invocation_id: InvocationId,
        grants: Vec<InvocationGrant>,
        session_allowed: BTreeSet<HostCapability>,
        service: S,
        audit: SharedEffectAudit,
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
            audit,
            #[cfg(test)]
            fail_completion_durability: None,
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

        if grant.bound_invocation != self.invocation_id {
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
        if capability == HostCapability::ProposeSkill
            && matches!(grant.principal, GrantPrincipal::Skill { .. })
        {
            return Err(HostEffectError::CapabilityDenied);
        }

        let authorized = AuthorizedEffect {
            invocation_id: self.invocation_id.clone(),
            grant_id: grant.grant_id.clone(),
            principal: grant.principal.clone(),
            capability,
        };
        self.service
            .validate_target(&authorized, &request.operation)?;

        let normalization_result = {
            let normalization = self.service.normalize_target(
                &authorized,
                &request.operation,
                cancellation.clone(),
            );
            tokio::pin!(normalization);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(HostEffectError::InvocationCancelled),
                result = &mut normalization => result,
            }
        };
        if normalization_result == Err(HostEffectError::InvocationCancelled) {
            self.cancel_invocation();
        }
        let normalized_target = match normalization_result {
            Ok(target) => target,
            Err(error) => {
                self.service.discard_prepared();
                return Err(error);
            }
        };
        if let Err(error) = enforce_manifest_scope(&grant, capability, &normalized_target) {
            self.service.discard_prepared();
            return Err(error);
        }
        if !self.session_allowed.contains(&capability) {
            self.service.discard_prepared();
            return Err(HostEffectError::SessionDenied);
        }
        if let Err(error) = self.service.ensure_backend(&authorized, &request.operation) {
            self.service.discard_prepared();
            return Err(error);
        }

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
        let audit_target = match authorization_result {
            Ok(target) => target,
            Err(error) => {
                self.service.discard_prepared();
                return Err(error);
            }
        };

        if cancellation.is_cancelled() {
            self.cancel_invocation();
            self.service.discard_prepared();
            return Err(HostEffectError::InvocationCancelled);
        }
        if let Err(error) = self.ensure_grant_unexpired(&request.grant_id, grant.expires_at) {
            self.service.discard_prepared();
            return Err(error);
        }

        let effect_id = effect_id(&self.invocation_id, request.effect_ordinal);
        let (artifact_id, export) = match &authorized.principal {
            GrantPrincipal::ModelAuthored { .. } => (None, None),
            GrantPrincipal::Skill {
                artifact_id,
                export,
                ..
            } => (Some(artifact_id.clone()), Some(export.clone())),
        };
        let timestamp_ms = match timestamp_ms() {
            Ok(timestamp_ms) => timestamp_ms,
            Err(error) => {
                self.erase_authority(InvocationState::Terminal);
                self.service.discard_prepared();
                return Err(error);
            }
        };
        let intent_result = self
            .audit
            .lock()
            .map_err(|_| HostEffectError::AuditFailure)
            .and_then(|mut audit| {
                // The shared writer can be contended by another invocation. Authority is a
                // lease, so recheck it after acquiring that lock and before persisting intent.
                if Instant::now() >= grant.expires_at {
                    return Err(HostEffectError::ExpiredGrant);
                }
                let normalized_target = sanitize_target(&audit, capability, audit_target)?;
                audit
                    .append_intent(EffectIntent {
                        effect_id: effect_id.clone(),
                        invocation_id: self.invocation_id.to_string(),
                        grant_id: authorized.grant_id.get().to_string(),
                        sequence: u64::from(request.effect_ordinal) + 1,
                        timestamp_ms,
                        artifact_id,
                        export,
                        capability: audit_capability(capability),
                        normalized_target,
                        decision: AuditDecision::Authorized,
                    })
                    .map_err(HostEffectError::from)
            });
        if let Err(error) = intent_result {
            if error == HostEffectError::ExpiredGrant {
                let _ = self.ensure_grant_unexpired(&request.grant_id, grant.expires_at);
                self.service.discard_prepared();
                return Err(error);
            }
            self.erase_authority(InvocationState::Terminal);
            self.service.discard_prepared();
            return Err(HostEffectError::AuditFailure);
        }

        let execution_result = self
            .service
            .execute(&authorized, &request.operation, cancellation)
            .await;
        let completion_code = audit_result_code(&execution_result);
        let completion_result = self
            .audit
            .lock()
            .map_err(|_| HostEffectError::AuditFailure)
            .and_then(|mut audit| {
                #[cfg(test)]
                if let Some(failure) = self.fail_completion_durability.take() {
                    audit.fail_next_durability_for_test(failure);
                }
                audit
                    .append_completion(EffectCompletion {
                        effect_id,
                        result_code: completion_code,
                    })
                    .map_err(HostEffectError::from)
            });
        if completion_result.is_err() {
            self.erase_authority(InvocationState::Terminal);
            return Err(HostEffectError::AuditFailure);
        }
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

    #[cfg(test)]
    pub(crate) fn audit_records_for_test(&self) -> Vec<super::audit::EffectAuditRecord> {
        self.audit.lock().unwrap().records().to_vec()
    }

    #[cfg(test)]
    pub(crate) fn fail_next_audit_durability_for_test(
        &mut self,
        failure: super::audit::AuditFailurePoint,
    ) {
        self.audit
            .lock()
            .unwrap()
            .fail_next_durability_for_test(failure);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_completion_durability_for_test(
        &mut self,
        failure: super::audit::AuditFailurePoint,
    ) {
        self.fail_completion_durability = Some(failure);
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

#[cfg(feature = "skills")]
fn enforce_manifest_scope(
    grant: &InvocationGrant,
    capability: HostCapability,
    target: &NormalizedTarget,
) -> Result<(), HostEffectError> {
    if matches!(grant.principal, GrantPrincipal::ModelAuthored { .. }) {
        return Ok(());
    }
    let manifest = grant
        .manifest
        .as_ref()
        .ok_or(HostEffectError::ManifestDenied)?;
    let allowed = match (manifest.scope(skill_capability(capability)), target) {
        (
            Some(CapabilityScope::ReadFile { workspace_prefixes }),
            NormalizedTarget::ReadFile { workspace_relative },
        )
        | (
            Some(CapabilityScope::WriteFile { workspace_prefixes }),
            NormalizedTarget::WriteFile { workspace_relative },
        ) => workspace_prefixes.iter().any(|prefix| {
            workspace_relative
                .as_deref()
                .is_some_and(|target| path_scope_contains(prefix, target))
        }),
        (
            Some(CapabilityScope::Fetch { origins, methods }),
            NormalizedTarget::Fetch { origin, method },
        ) => {
            origins.contains(origin)
                && methods.iter().any(|allowed| {
                    matches!(
                        (allowed, method.as_str()),
                        (SkillHttpMethod::Get, "GET") | (SkillHttpMethod::Post, "POST")
                    )
                })
        }
        (
            Some(CapabilityScope::Spawn { programs }),
            NormalizedTarget::Spawn {
                program,
                resolved_executable,
            },
        ) => {
            programs.contains(program)
                && grant.spawn_program_identities.get(program) == Some(resolved_executable)
        }
        _ => false,
    };
    allowed.then_some(()).ok_or(HostEffectError::ManifestDenied)
}

#[cfg(not(feature = "skills"))]
fn enforce_manifest_scope(
    _grant: &InvocationGrant,
    _capability: HostCapability,
    _target: &NormalizedTarget,
) -> Result<(), HostEffectError> {
    Ok(())
}

#[cfg(feature = "skills")]
fn skill_capability(capability: HostCapability) -> SkillHostCapability {
    match capability {
        HostCapability::ReadFile => SkillHostCapability::ReadFile,
        HostCapability::WriteFile => SkillHostCapability::WriteFile,
        HostCapability::Fetch => SkillHostCapability::Fetch,
        HostCapability::Spawn => SkillHostCapability::Spawn,
        HostCapability::ProposeSkill => unreachable!("skills cannot receive proposal grants"),
    }
}

#[cfg(feature = "skills")]
fn path_scope_contains(prefix: &str, target: &str) -> bool {
    target == prefix
        || target
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Resolve a program once to the executable identity carried through scope,
/// permission, durable audit, and execution.
pub(crate) fn resolve_program_identity(
    program: &str,
) -> Result<SpawnExecutableIdentity, EffectServiceError> {
    resolve_program_identity_inner(program, None)
}

pub(crate) fn resolve_program_identity_controlled(
    program: &str,
    control: &ExecutablePreparationControl,
) -> Result<SpawnExecutableIdentity, EffectServiceError> {
    resolve_program_identity_inner(program, Some(control))
}

fn resolve_program_identity_inner(
    program: &str,
    control: Option<&ExecutablePreparationControl>,
) -> Result<SpawnExecutableIdentity, EffectServiceError> {
    executable_identity_checkpoint(control)?;
    let source = Path::new(program);
    let candidates = if source.is_absolute() || source.components().count() > 1 {
        spawn_executable_candidates(source.to_path_buf())
    } else {
        let path = std::env::var_os("PATH").ok_or(EffectServiceError::InvalidTarget)?;
        std::env::split_paths(&path)
            .flat_map(|directory| spawn_executable_candidates(directory.join(source)))
            .collect()
    };
    for candidate in candidates {
        executable_identity_checkpoint(control)?;
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        let canonical =
            std::fs::canonicalize(candidate).map_err(|_| EffectServiceError::InvalidTarget)?;
        executable_identity_checkpoint(control)?;
        let canonical_path = canonical
            .to_str()
            .map(str::to_string)
            .ok_or(EffectServiceError::InvalidTarget)?;
        if canonical_path.len() > MAX_RESOLVED_EXECUTABLE_BYTES {
            return Err(EffectServiceError::InvalidTarget);
        }
        let mut file =
            std::fs::File::open(&canonical).map_err(|_| EffectServiceError::InvalidTarget)?;
        executable_identity_checkpoint(control)?;
        let metadata = file
            .metadata()
            .map_err(|_| EffectServiceError::InvalidTarget)?;
        let current =
            std::fs::symlink_metadata(&canonical).map_err(|_| EffectServiceError::InvalidTarget)?;
        let platform =
            platform_executable_identity(&metadata).ok_or(EffectServiceError::InvalidTarget)?;
        if platform_executable_identity(&current) != Some(platform.clone()) {
            return Err(EffectServiceError::TargetChanged);
        }
        let content = match control {
            Some(control) => {
                copy_and_hash_executable_controlled(&mut file, &mut std::io::sink(), control)
            }
            None => copy_and_hash_executable(&mut file, &mut std::io::sink()),
        }
        .map_err(executable_copy_service_error)?;
        return Ok(SpawnExecutableIdentity {
            canonical_path,
            platform,
            content_sha256: content.sha256,
            content_bytes: content.bytes,
        });
    }
    Err(EffectServiceError::InvalidTarget)
}

fn executable_identity_checkpoint(
    control: Option<&ExecutablePreparationControl>,
) -> Result<(), EffectServiceError> {
    match control.map(ExecutablePreparationControl::checkpoint) {
        Some(Err(ExecutablePreparationWaitError::Cancelled)) => Err(EffectServiceError::Cancelled),
        Some(Err(ExecutablePreparationWaitError::TimedOut)) => Err(EffectServiceError::TimedOut),
        Some(Err(ExecutablePreparationWaitError::WorkerFailed)) => {
            Err(EffectServiceError::BackendFailure)
        }
        Some(Ok(())) | None => Ok(()),
    }
}

fn executable_copy_service_error(error: ExecutableCopyError) -> EffectServiceError {
    match error {
        ExecutableCopyError::Cancelled => EffectServiceError::Cancelled,
        ExecutableCopyError::TimedOut => EffectServiceError::TimedOut,
        ExecutableCopyError::Read | ExecutableCopyError::Write | ExecutableCopyError::TooLarge => {
            EffectServiceError::InvalidTarget
        }
    }
}

#[cfg(unix)]
fn platform_executable_identity(
    metadata: &std::fs::Metadata,
) -> Option<PlatformExecutableIdentity> {
    use std::os::unix::fs::MetadataExt;

    Some(PlatformExecutableIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn platform_executable_identity(
    metadata: &std::fs::Metadata,
) -> Option<PlatformExecutableIdentity> {
    use std::os::windows::fs::MetadataExt;

    Some(PlatformExecutableIdentity::Windows {
        volume_serial_number: metadata.volume_serial_number()?,
        file_index: metadata.file_index()?,
    })
}

#[cfg(not(any(unix, windows)))]
fn platform_executable_identity(
    _metadata: &std::fs::Metadata,
) -> Option<PlatformExecutableIdentity> {
    None
}

fn spawn_executable_candidates(path: PathBuf) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if path.extension().is_some() {
            return vec![path];
        }
        let extensions = std::env::var_os("PATHEXT")
            .and_then(|value| value.into_string().ok())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
        return extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| {
                let mut candidate = path.as_os_str().to_os_string();
                candidate.push(extension.to_ascii_lowercase());
                PathBuf::from(candidate)
            })
            .collect();
    }
    #[cfg(not(windows))]
    {
        vec![path]
    }
}

fn sanitize_target(
    audit: &EffectAudit,
    capability: HostCapability,
    target: AuthorizedTarget,
) -> Result<SanitizedTarget, HostEffectError> {
    match (capability, target) {
        (HostCapability::ReadFile, AuthorizedTarget::ReadFile { canonical_path }) => {
            Ok(audit.file_target(&canonical_path))
        }
        (HostCapability::WriteFile, AuthorizedTarget::WriteFile { canonical_path }) => {
            Ok(audit.write_file_target(&canonical_path))
        }
        (
            HostCapability::Fetch,
            AuthorizedTarget::Fetch {
                normalized_url,
                method,
            },
        ) => audit
            .fetch_target(&normalized_url, &method)
            .map_err(HostEffectError::from),
        (
            HostCapability::Spawn,
            AuthorizedTarget::Spawn {
                resolved_executable,
            },
        ) => Ok(audit.spawn_target(&resolved_executable)),
        (HostCapability::ProposeSkill, AuthorizedTarget::ProposeSkill) => {
            Ok(audit.proposal_target())
        }
        _ => Err(HostEffectError::AuditFailure),
    }
}

fn audit_capability(capability: HostCapability) -> AuditCapability {
    match capability {
        HostCapability::ReadFile => AuditCapability::ReadFile,
        HostCapability::WriteFile => AuditCapability::WriteFile,
        HostCapability::Fetch => AuditCapability::Fetch,
        HostCapability::Spawn => AuditCapability::Spawn,
        HostCapability::ProposeSkill => AuditCapability::ProposeSkill,
    }
}

fn audit_result_code(result: &Result<EffectResult, HostEffectError>) -> AuditResultCode {
    match result {
        Ok(EffectResult::Spawn {
            timed_out: true, ..
        }) => AuditResultCode::TimedOut,
        Ok(
            EffectResult::Spawn {
                stdout_truncated: true,
                ..
            }
            | EffectResult::Spawn {
                stderr_truncated: true,
                ..
            }
            | EffectResult::Fetch {
                truncated: true, ..
            },
        ) => AuditResultCode::OutputLimit,
        Ok(EffectResult::Error(error)) => match error.code {
            EffectErrorCode::Cancelled => AuditResultCode::Cancelled,
            EffectErrorCode::TimedOut => AuditResultCode::TimedOut,
            EffectErrorCode::OutputLimit => AuditResultCode::OutputLimit,
            EffectErrorCode::OutcomeUnknown => AuditResultCode::OutcomeUnknown,
            EffectErrorCode::BackendFailure | EffectErrorCode::AuditFailure => {
                AuditResultCode::BackendFailure
            }
            EffectErrorCode::Denied | EffectErrorCode::InvalidTarget => AuditResultCode::Denied,
        },
        Ok(_) => AuditResultCode::Succeeded,
        Err(HostEffectError::InvocationCancelled) => AuditResultCode::Cancelled,
        Err(HostEffectError::AskTimedOut | HostEffectError::EffectTimedOut) => {
            AuditResultCode::TimedOut
        }
        Err(HostEffectError::OutputLimit) => AuditResultCode::OutputLimit,
        Err(HostEffectError::OutcomeUnknown) => AuditResultCode::OutcomeUnknown,
        Err(HostEffectError::BackendFailure | HostEffectError::AuditFailure) => {
            AuditResultCode::BackendFailure
        }
        Err(_) => AuditResultCode::Denied,
    }
}

fn effect_id(invocation_id: &InvocationId, ordinal: u32) -> String {
    let bytes = invocation_id.as_str().as_bytes();
    let mut digest = Sha256::new();
    digest.update(b"mini-agent-js-effect-id-v1\0");
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    digest.update(ordinal.to_be_bytes());
    format!("effect-{:x}", digest.finalize())
}

fn timestamp_ms() -> Result<i64, HostEffectError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HostEffectError::AuditFailure)?
        .as_millis();
    i64::try_from(millis).map_err(|_| HostEffectError::AuditFailure)
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
