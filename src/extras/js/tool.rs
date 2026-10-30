use std::fmt;
#[cfg(feature = "skills")]
use std::sync::atomic::AtomicU64;
#[cfg(feature = "skills")]
use std::sync::atomic::Ordering;
#[cfg(any(test, feature = "sandbox"))]
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use compact_str::CompactString;
use rig::tool::Tool;
use serde::Deserialize;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use crate::agent::tools::ToolError;
use crate::extras::js::audit::{AuditError, EffectAudit};
use crate::extras::js::broker::{
    GrantPrincipal, HostCapability, InvocationBroker, InvocationGrant, SharedEffectAudit,
};
#[cfg(feature = "skills")]
use crate::extras::js::broker::{
    PreparedSkillManifest, SkillCallAuthority, SkillExportAuthoritySpec,
};
use crate::extras::js::host::{
    AllowConfig, FileEffectService, ParentHostEffectService, SPAWN_STDERR_MAX_BYTES,
    SPAWN_STDOUT_MAX_BYTES, SpawnEffectService,
};
#[cfg(feature = "sandbox")]
use crate::extras::js::host::{
    FETCH_REQUEST_BODY_MAX_BYTES, FETCH_RESPONSE_BODY_MAX_BYTES, FetchEffectService,
};
#[cfg(test)]
use crate::extras::js::protocol::JsDiagnosticHint;
use crate::extras::js::protocol::{
    ConsoleLevel, ConsoleRecord, Diagnostic, DiagnosticClass, DiagnosticStage, InvocationId,
    JsErrorCode, JsExceptionClass, MAX_CONSOLE_BYTES, MAX_CONSOLE_RECORD_BYTES,
    MAX_CONSOLE_RECORDS, MAX_EFFECTS_PER_STEP, MAX_RESULT_BYTES, ModelEffectProfile, RunStep,
    ScriptRole, StepOutcome, StepResult, source_position_is_valid,
};
#[cfg(feature = "skills")]
use crate::extras::js::protocol::{
    MAX_SKILL_ARTIFACTS_PER_STEP, MAX_SKILL_CAPABILITY_GRANTS_PER_STEP,
    MAX_SKILL_EXPORTS_PER_ARTIFACT,
};
use crate::extras::js::session::{
    SCRATCH_KEY_MAX_BYTES, SCRATCH_MAX_ENTRIES, SCRATCH_TOTAL_MAX_BYTES, SCRATCH_VALUE_MAX_BYTES,
    STRUCTURED_RESULT_MAX_BYTES, ScratchStore,
};
#[cfg(feature = "skills")]
use crate::extras::js::skills::proposal::ProposalEffectService;
#[cfg(all(feature = "skills", test))]
use crate::extras::js::skills::proposal::{
    AttemptBudget, DEFAULT_SESSION_ATTEMPTS, ProposalHost, ProposalWorker,
};
use crate::extras::js::supervisor::{JsWorkerSupervisor, WorkerError};
use crate::extras::js::types::{
    MEMORY_LIMIT, PermCancellation, PermOutcome, PermRequest, PermRequestBuildError, PermResponse,
    PermResponseRejection, PermissionBackendFailure, PermissionDenial, READ_FILE_MAX_BYTES,
    READ_FILES_MAX_PATHS, READ_FILES_MAX_RESULT_BYTES, READ_LINES_MAX_COUNT,
    READ_LINES_MAX_LINE_BYTES, READ_LINES_MAX_SCAN_BYTES, STACK_LIMIT, STEP_TIMEOUT,
    WRITE_FILE_MAX_BYTES,
};
use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::{CheckResult, PermCheck};
use crate::sandbox::Sandbox;

#[cfg(any(test, feature = "sandbox"))]
const PERMISSION_WAIT_POLL: Duration = Duration::from_millis(10);

#[derive(Clone)]
enum PermissionCheckKind {
    Input,
    Path,
    StructuredInput { policy_input: String },
    BoundPath,
}

enum PermissionReply {
    #[cfg(any(test, feature = "sandbox"))]
    Sync(mpsc::SyncSender<PermResponse>),
    Async(oneshot::Sender<PermResponse>),
}

impl PermissionReply {
    fn send(self, response: PermResponse) {
        match self {
            #[cfg(any(test, feature = "sandbox"))]
            Self::Sync(reply) => {
                let _ = reply.send(response);
            }
            Self::Async(reply) => {
                let _ = reply.send(response);
            }
        }
    }
}

struct PermissionEnvelope {
    request: PermRequest,
    kind: PermissionCheckKind,
    invocation_cancellation: Option<PermCancellation>,
    #[cfg(feature = "hooks")]
    hook_work_scope: Option<Arc<crate::agent::runner::AgentWorkScope>>,
    reply: PermissionReply,
}

#[derive(Clone)]
pub(crate) struct PermissionBridge {
    tx: tokio_mpsc::UnboundedSender<PermissionEnvelope>,
    shutdown: PermCancellation,
    invocation_cancellation: Option<PermCancellation>,
    #[cfg(feature = "hooks")]
    hook_work_scope: Option<Arc<crate::agent::runner::AgentWorkScope>>,
    host_call_cancellation: Option<PermCancellation>,
    timeout: Duration,
    /// The same checker the receiver task consults, read only for the
    /// non-prompting deny snapshot that filters discovery walks.
    deny_source: Option<PermCheck>,
    #[cfg(test)]
    sync_wait_clock: Option<Arc<std::sync::Mutex<Instant>>>,
}

impl PermissionBridge {
    fn new(
        tx: tokio_mpsc::UnboundedSender<PermissionEnvelope>,
        shutdown: PermCancellation,
        timeout: Duration,
    ) -> Self {
        Self {
            tx,
            shutdown,
            invocation_cancellation: None,
            #[cfg(feature = "hooks")]
            hook_work_scope: None,
            host_call_cancellation: None,
            timeout,
            deny_source: None,
            #[cfg(test)]
            sync_wait_clock: None,
        }
    }

    /// Deny-only filter for a JS discovery walk (`js/list_dir`, `js/glob`,
    /// `js/grep`) rooted beneath the approved directory: entries matched by
    /// the tool's own or the `read`/`js/read_file` path deny rules are hidden
    /// and denied directories are pruned (mini-agent-c1o7m). It never
    /// prompts, never consults allow rules or the session allowlist, and
    /// never feeds doom-loop tracking; see
    /// [`crate::permission::checker::PermissionChecker::path_deny_probe`].
    pub(crate) fn walk_deny_filter(
        &self,
        tool: &str,
        workspace_root: Option<&std::path::Path>,
    ) -> Option<crate::agent::tools::find_files::PathFilter> {
        crate::agent::tools::walk_deny_filter(&self.deny_source, tool, workspace_root)
    }

    pub(crate) fn for_invocation(&self, cancellation: PermCancellation) -> Self {
        let mut bridge = self.clone();
        bridge.invocation_cancellation = Some(cancellation);
        #[cfg(feature = "hooks")]
        {
            bridge.hook_work_scope = crate::agent::runner::current_hook_work_scope();
        }
        bridge
    }

    pub(crate) fn for_host_call(&self, cancellation: PermCancellation) -> Self {
        let mut bridge = self.clone();
        bridge.host_call_cancellation = Some(cancellation);
        bridge
    }

    pub(crate) async fn cancelled(&self) {
        tokio::select! {
            _ = self.shutdown.cancelled() => {}
            _ = wait_for_cancellation(self.invocation_cancellation.clone()) => {}
            _ = wait_for_cancellation(self.host_call_cancellation.clone()) => {}
        }
    }

    #[cfg(any(test, feature = "sandbox"))]
    pub(crate) fn check(&self, tool: &str, key: &str) -> Result<(), PermissionBridgeError> {
        self.check_sync(PermissionCheckKind::Input, tool, key)
    }

    #[cfg(any(test, feature = "sandbox"))]
    fn check_sync(
        &self,
        kind: PermissionCheckKind,
        tool: &str,
        key: &str,
    ) -> Result<(), PermissionBridgeError> {
        self.ensure_active()?;
        let cancellation = PermCancellation::new();
        let request = PermRequest::new(tool, key, self.timeout, cancellation.clone())
            .map_err(PermissionBridgeError::InvalidRequest)?;
        let deadline = request.deadline();
        let guard = request.response_guard();
        let mut cancel_on_drop = CancelOnDrop::new(cancellation);
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);

        self.tx
            .send(PermissionEnvelope {
                request,
                kind,
                invocation_cancellation: self.invocation_cancellation.clone(),
                #[cfg(feature = "hooks")]
                hook_work_scope: self.hook_work_scope.clone(),
                reply: PermissionReply::Sync(reply_tx),
            })
            .map_err(|_| self.closed_request_error())?;

        loop {
            if self.is_cancelled() {
                return Err(PermissionBridgeError::Cancelled);
            }
            let now = Instant::now();
            #[cfg(test)]
            let now = self
                .sync_wait_clock
                .as_ref()
                .map_or(now, |clock| *clock.lock().unwrap());
            let Some(remaining) = deadline.checked_duration_since(now) else {
                self.mark_permission_prompt_timed_out();
                return Err(PermissionBridgeError::TimedOut);
            };
            match reply_rx.recv_timeout(remaining.min(PERMISSION_WAIT_POLL)) {
                Ok(response) => {
                    let outcome = guard
                        .accept_response(response)
                        .map_err(PermissionBridgeError::from_rejection)?;
                    cancel_on_drop.disarm();
                    return self.finish_outcome(outcome);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.closed_response_error());
                }
            }
        }
    }

    pub(crate) async fn check_async(
        &self,
        tool: &str,
        key: &str,
    ) -> Result<(), PermissionBridgeError> {
        self.check_async_kind(PermissionCheckKind::Input, tool, key)
            .await
    }

    pub(crate) async fn check_path_async(
        &self,
        tool: &str,
        key: &str,
    ) -> Result<(), PermissionBridgeError> {
        self.check_async_kind(PermissionCheckKind::Path, tool, key)
            .await
    }

    pub(crate) async fn check_bound_path_async(
        &self,
        tool: &str,
        key: &str,
    ) -> Result<(), PermissionBridgeError> {
        self.check_async_kind(PermissionCheckKind::BoundPath, tool, key)
            .await
    }

    pub(crate) async fn check_structured_async(
        &self,
        tool: &str,
        identity: &str,
        policy_input: String,
    ) -> Result<(), PermissionBridgeError> {
        self.check_async_kind(
            PermissionCheckKind::StructuredInput { policy_input },
            tool,
            identity,
        )
        .await
    }

    async fn check_async_kind(
        &self,
        kind: PermissionCheckKind,
        tool: &str,
        key: &str,
    ) -> Result<(), PermissionBridgeError> {
        self.ensure_active()?;
        let cancellation = PermCancellation::new();
        let request = PermRequest::new(tool, key, self.timeout, cancellation.clone())
            .map_err(PermissionBridgeError::InvalidRequest)?;
        let deadline = request.deadline();
        let guard = request.response_guard();
        let mut cancel_on_drop = CancelOnDrop::new(cancellation);
        let (reply_tx, reply_rx) = oneshot::channel();

        self.tx
            .send(PermissionEnvelope {
                request,
                kind,
                invocation_cancellation: self.invocation_cancellation.clone(),
                #[cfg(feature = "hooks")]
                hook_work_scope: self.hook_work_scope.clone(),
                reply: PermissionReply::Async(reply_tx),
            })
            .map_err(|_| self.closed_request_error())?;

        let response = tokio::select! {
            response = reply_rx => {
                response.map_err(|_| self.closed_response_error())?
            }
            _ = self.shutdown.cancelled() => {
                return Err(PermissionBridgeError::Cancelled);
            }
            _ = wait_for_cancellation(self.invocation_cancellation.clone()) => {
                return Err(PermissionBridgeError::Cancelled);
            }
            _ = wait_for_cancellation(self.host_call_cancellation.clone()) => {
                return Err(PermissionBridgeError::Cancelled);
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                self.mark_permission_prompt_timed_out();
                return Err(PermissionBridgeError::TimedOut);
            }
        };

        let outcome = guard
            .accept_response(response)
            .map_err(PermissionBridgeError::from_rejection)?;
        cancel_on_drop.disarm();
        self.finish_outcome(outcome)
    }

    fn finish_outcome(&self, outcome: PermOutcome) -> Result<(), PermissionBridgeError> {
        if matches!(outcome, PermOutcome::TimedOut) {
            self.mark_permission_prompt_timed_out();
        }
        PermissionBridgeError::from_outcome(outcome)
    }

    fn mark_permission_prompt_timed_out(&self) {
        if let Some(cancellation) = &self.invocation_cancellation
            && cancellation.permission_prompt_pending()
        {
            cancellation.mark_permission_prompt_timed_out();
        }
    }

    fn ensure_active(&self) -> Result<(), PermissionBridgeError> {
        if self.is_cancelled() {
            Err(PermissionBridgeError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn closed_request_error(&self) -> PermissionBridgeError {
        if self.is_cancelled() {
            PermissionBridgeError::Cancelled
        } else {
            PermissionBridgeError::RequestChannelClosed
        }
    }

    fn closed_response_error(&self) -> PermissionBridgeError {
        if self.is_cancelled() {
            PermissionBridgeError::Cancelled
        } else {
            PermissionBridgeError::ResponseChannelClosed
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.shutdown.is_cancelled()
            || self
                .invocation_cancellation
                .as_ref()
                .is_some_and(PermCancellation::is_cancelled)
            || self
                .host_call_cancellation
                .as_ref()
                .is_some_and(PermCancellation::is_cancelled)
    }
}

async fn wait_for_cancellation(cancellation: Option<PermCancellation>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

struct CancelOnDrop(Option<PermCancellation>);

impl CancelOnDrop {
    fn new(cancellation: PermCancellation) -> Self {
        Self(Some(cancellation))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = self.0.take() {
            cancellation.cancel();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PermissionBridgeError {
    InvalidRequest(PermRequestBuildError),
    RequestChannelClosed,
    ResponseChannelClosed,
    Cancelled,
    TimedOut,
    Denied(PermissionDenial),
    BackendFailure(PermissionBackendFailure),
    RejectedResponse(PermResponseRejection),
}

impl PermissionBridgeError {
    fn from_outcome(outcome: PermOutcome) -> Result<(), Self> {
        match outcome {
            PermOutcome::Allowed => Ok(()),
            PermOutcome::Denied(reason) => Err(Self::Denied(reason)),
            PermOutcome::BackendFailure(reason) => Err(Self::BackendFailure(reason)),
            PermOutcome::Cancelled => Err(Self::Cancelled),
            PermOutcome::TimedOut => Err(Self::TimedOut),
        }
    }

    fn from_rejection(rejection: PermResponseRejection) -> Self {
        match rejection {
            PermResponseRejection::Cancelled { .. } => Self::Cancelled,
            PermResponseRejection::DeadlineExpired { .. } => Self::TimedOut,
            mismatch @ PermResponseRejection::MismatchedRequestId { .. } => {
                Self::RejectedResponse(mismatch)
            }
        }
    }
}

impl fmt::Display for PermissionBridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(error) => write!(f, "invalid permission request: {error}"),
            Self::RequestChannelClosed => f.write_str("permission request channel closed"),
            Self::ResponseChannelClosed => f.write_str("permission response channel closed"),
            Self::Cancelled => f.write_str("permission request cancelled"),
            Self::TimedOut => f.write_str("permission request timed out"),
            Self::Denied(PermissionDenial::Policy(reason)) => {
                write!(f, "Permission denied: {reason}")
            }
            Self::Denied(PermissionDenial::User) => f.write_str("Permission denied by user"),
            Self::Denied(PermissionDenial::NonInteractive(diagnostic)) => {
                f.write_str(&diagnostic.noninteractive_error())
            }
            Self::BackendFailure(reason) => {
                write!(f, "permission backend failure: {reason:?}")
            }
            Self::RejectedResponse(PermResponseRejection::MismatchedRequestId {
                expected,
                actual,
            }) => write!(
                f,
                "stale permission response: expected request {}, got {}",
                expected.get(),
                actual.get()
            ),
            Self::RejectedResponse(PermResponseRejection::Cancelled { request_id }) => {
                write!(f, "permission request {} was cancelled", request_id.get())
            }
            Self::RejectedResponse(PermResponseRejection::DeadlineExpired { request_id }) => {
                write!(f, "permission request {} timed out", request_id.get())
            }
        }
    }
}

pub(crate) struct PermissionBridgeOwner {
    bridge: PermissionBridge,
    task: AbortHandle,
}

impl PermissionBridgeOwner {
    pub(crate) fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        timeout: Duration,
    ) -> Self {
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        let shutdown = PermCancellation::new();
        let mut bridge = PermissionBridge::new(tx, shutdown.clone(), timeout);
        bridge.deny_source = permission.clone();
        let task = tokio::spawn(run_permission_receiver(rx, permission, ask_tx, shutdown));
        Self {
            bridge,
            task: task.abort_handle(),
        }
    }

    pub(crate) fn bridge(&self) -> PermissionBridge {
        self.bridge.clone()
    }

    pub(crate) fn shutdown(&self) {
        self.bridge.shutdown.cancel();
        self.task.abort();
    }
}

impl Drop for PermissionBridgeOwner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn run_permission_receiver(
    mut rx: tokio_mpsc::UnboundedReceiver<PermissionEnvelope>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    shutdown: PermCancellation,
) {
    let mut requests = JoinSet::new();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            envelope = rx.recv() => {
                let Some(envelope) = envelope else {
                    break;
                };
                let permission = permission.clone();
                let ask_tx = ask_tx.clone();
                let shutdown = shutdown.clone();
                requests.spawn(async move {
                    process_permission_request(envelope, permission, ask_tx, shutdown).await;
                });
            }
            completed = requests.join_next(), if !requests.is_empty() => {
                let _ = completed;
            }
        }
    }

    requests.abort_all();
    while requests.join_next().await.is_some() {}
}

async fn process_permission_request(
    envelope: PermissionEnvelope,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    shutdown: PermCancellation,
) {
    let PermissionEnvelope {
        request,
        kind,
        invocation_cancellation,
        #[cfg(feature = "hooks")]
        hook_work_scope,
        reply,
    } = envelope;
    let request_id = request.id();
    let deadline = request.deadline();
    let cancellation = request.cancellation().clone();

    let outcome = tokio::select! {
        _ = shutdown.cancelled() => PermOutcome::Cancelled,
        _ = cancellation.cancelled() => PermOutcome::Cancelled,
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            if let Some(cancellation) = &invocation_cancellation
                && cancellation.permission_prompt_pending()
            {
                cancellation.mark_permission_prompt_timed_out();
            }
            PermOutcome::TimedOut
        }
        outcome = async {
            let resolve = crate::startup::heap_future(|| resolve_permission(
                &request,
                kind,
                permission,
                ask_tx,
                invocation_cancellation.as_ref(),
            ));
            #[cfg(feature = "hooks")]
            if let Some(scope) = hook_work_scope {
                return scope.run(resolve).await;
            }
            resolve.await
        } => outcome,
    };

    reply.send(PermResponse::new(request_id, outcome));
}

async fn resolve_permission(
    request: &PermRequest,
    kind: PermissionCheckKind,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
    invocation_cancellation: Option<&PermCancellation>,
) -> PermOutcome {
    let Some(permission) = permission else {
        return PermOutcome::Allowed;
    };

    let (check, diagnostic) = {
        let mut checker = permission.lock().unwrap_or_else(|error| error.into_inner());
        checker.check_diagnosed(|checker| match kind {
            PermissionCheckKind::Input => checker.check(request.tool(), request.key()),
            PermissionCheckKind::Path => checker.check_path(request.tool(), request.key()),
            PermissionCheckKind::StructuredInput { policy_input } => {
                checker.check_with_identity(request.tool(), &policy_input, request.key())
            }
            PermissionCheckKind::BoundPath => {
                checker.check_bound_path(request.tool(), request.key())
            }
        })
    };

    match check {
        CheckResult::Allowed | CheckResult::AllowedWithCoaching(_) => PermOutcome::Allowed,
        CheckResult::Denied(reason) => PermOutcome::Denied(PermissionDenial::Policy(reason)),
        CheckResult::Ask => {
            let Some(ask_tx) = ask_tx else {
                return PermOutcome::Denied(PermissionDenial::NonInteractive(diagnostic));
            };
            let _prompt_guard =
                invocation_cancellation.map(PermCancellation::begin_permission_prompt);
            let (reply_tx, reply_rx) = oneshot::channel();
            if crate::permission::ask::send_permission_request(
                &ask_tx,
                AskRequest {
                    diagnostic: Some(diagnostic),
                    #[cfg(feature = "subagents")]
                    background: None,
            agent_name: crate::permission::ask::subagent_name(),
                    tool: CompactString::new(request.tool()),
                    input: request.key().to_string(),
                    #[cfg(feature = "acp")]
                    // Broker effects use a separate receiver task; ACP announces
                    // a synthetic call for each independently requested approval.
                    tool_call_id: None,
                    suggested_pattern: None,
                    additional_allow_patterns: Vec::new(),
                    reply: reply_tx,
                },
            )
            .await
            .is_err()
            {
                return PermOutcome::BackendFailure(PermissionBackendFailure::AskChannelClosed);
            }

            match reply_rx.await {
                Ok(UserDecision::AllowOnce) => PermOutcome::Allowed,
                Ok(UserDecision::AllowAlways(pattern)) => {
                    permission
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .add_session_allowlist(request.tool().to_string(), &pattern);
                    PermOutcome::Allowed
                }
                Ok(UserDecision::Deny) => PermOutcome::Denied(PermissionDenial::User),
                Err(_) => PermOutcome::BackendFailure(PermissionBackendFailure::AskResponseDropped),
            }
        }
    }
}

pub struct JsTool {
    sandbox: Sandbox,
    allow_config: AllowConfig,
    supervisor: Arc<JsWorkerSupervisor>,
    // Production construction leaves the audit unopened. The first JS call
    // performs retained-history validation on Tokio's blocking pool.
    audit: Option<Result<SharedEffectAudit, AuditError>>,
    permission_bridge: PermissionBridgeOwner,
    scratch: ScratchStore,
    #[cfg(feature = "sandbox")]
    runtime: tokio::runtime::Handle,
    #[cfg(feature = "skills")]
    skill_turn_context: Arc<crate::extras::js::skills::turn::SkillTurnContext>,
    #[cfg(all(feature = "skills", test))]
    _proposal_worker: Option<ProposalWorker>,
    #[cfg(feature = "skills")]
    proposal_service: Option<ProposalEffectService>,
    #[cfg(feature = "skills")]
    telemetry: Option<Arc<crate::extras::js::skills::telemetry::TelemetryDispatcher>>,
    #[cfg(feature = "skills")]
    skill_production: bool,
    #[cfg(feature = "skills")]
    skill_tool_call_ordinal: AtomicU64,
    profile: ModelEffectProfile,
}

impl JsTool {
    pub fn new(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
    ) -> Self {
        Self::new_with_runtime(
            sandbox,
            permission,
            ask_tx,
            allow_config,
            JsWorkerSupervisor::shared(),
            None,
        )
    }

    /// Construct the code-as-tool surface used by read-only exploration
    /// subagents. The worker installs only `read_file`, `list_dir`, and `grep`,
    /// while the parent broker issues only `read_file` authority.
    #[cfg(feature = "subagents")]
    pub(crate) fn new_read_only(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
    ) -> Self {
        let mut tool = Self::new(sandbox, permission, ask_tx, allow_config);
        tool.profile = ModelEffectProfile::ReadOnly;
        tool
    }

    #[cfg(all(feature = "skills", test))]
    pub(crate) fn new_with_proposals(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
        proposal_worker: ProposalWorker,
    ) -> Self {
        let mut tool = Self::new(sandbox, permission, ask_tx, allow_config);
        tool.proposal_service = Some(ProposalEffectService::new(ProposalHost::new(
            proposal_worker.sender(),
            AttemptBudget::new(DEFAULT_SESSION_ATTEMPTS),
        )));
        tool._proposal_worker = Some(proposal_worker);
        tool
    }

    fn new_with_runtime(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
        supervisor: Arc<JsWorkerSupervisor>,
        audit: Option<Result<SharedEffectAudit, AuditError>>,
    ) -> Self {
        let permission_bridge = PermissionBridgeOwner::new(permission, ask_tx, STEP_TIMEOUT);
        #[cfg(feature = "sandbox")]
        let runtime = tokio::runtime::Handle::current();

        Self {
            sandbox,
            allow_config,
            supervisor,
            audit,
            permission_bridge,
            scratch: ScratchStore::default(),
            #[cfg(feature = "sandbox")]
            runtime,
            #[cfg(feature = "skills")]
            skill_turn_context: Arc::new(crate::extras::js::skills::turn::SkillTurnContext::new(
                crate::extras::js::skills::turn::TurnSkillBundle::empty("unconfigured"),
            )),
            #[cfg(all(feature = "skills", test))]
            _proposal_worker: None,
            #[cfg(feature = "skills")]
            proposal_service: None,
            #[cfg(feature = "skills")]
            telemetry: None,
            #[cfg(feature = "skills")]
            // Only the production agent builder may opt a tool into durable
            // production evidence. Directly constructed tools are used by
            // tests, evals, and the gym and must stay non-production.
            skill_production: false,
            #[cfg(feature = "skills")]
            skill_tool_call_ordinal: AtomicU64::new(0),
            profile: ModelEffectProfile::Full,
        }
    }

    pub(crate) fn with_scratch_store(mut self, scratch: ScratchStore) -> Self {
        self.scratch = scratch;
        self
    }

    fn spawn_available(&self) -> bool {
        self.profile == ModelEffectProfile::Full && self.sandbox.owns_complete_process_tree()
    }

    fn model_capabilities(&self) -> std::collections::BTreeSet<HostCapability> {
        let mut capabilities = if self.profile == ModelEffectProfile::ReadOnly {
            std::collections::BTreeSet::from([HostCapability::ReadFile])
        } else {
            std::collections::BTreeSet::from([
                HostCapability::ReadFile,
                HostCapability::WriteFile,
                HostCapability::Fetch,
                HostCapability::SessionState,
            ])
        };
        if self.spawn_available() {
            capabilities.insert(HostCapability::Spawn);
        }
        capabilities
    }

    #[cfg(feature = "skills")]
    pub fn with_skill_turn_context(
        mut self,
        context: Arc<crate::extras::js::skills::turn::SkillTurnContext>,
    ) -> Self {
        self.skill_turn_context = context;
        self
    }

    // Test-only: production wires a session-shared dispatcher through
    // `with_shared_telemetry`. The previous `cfg_attr(test, allow(dead_code))` was
    // inverted -- it silenced the lint in exactly the build where the fn is live.
    #[cfg(all(feature = "skills", test))]
    pub(crate) fn with_telemetry(
        mut self,
        telemetry: crate::extras::js::skills::telemetry::TelemetryDispatcher,
    ) -> Self {
        self.telemetry = Some(Arc::new(telemetry));
        self
    }

    #[cfg(feature = "skills")]
    pub(crate) fn with_shared_telemetry(
        mut self,
        telemetry: Arc<crate::extras::js::skills::telemetry::TelemetryDispatcher>,
    ) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    #[cfg(feature = "skills")]
    pub(crate) fn with_skill_production(mut self, production: bool) -> Self {
        self.skill_production = production;
        self
    }

    #[cfg(feature = "skills")]
    pub(crate) fn with_proposal_service(mut self, service: ProposalEffectService) -> Self {
        self.proposal_service = Some(service);
        self
    }

    #[cfg(test)]
    pub(crate) fn new_with_runtime_for_test(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
        supervisor: Arc<JsWorkerSupervisor>,
        audit: SharedEffectAudit,
    ) -> Self {
        Self::new_with_runtime(
            sandbox,
            permission,
            ask_tx,
            allow_config,
            supervisor,
            Some(Ok(audit)),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_failed_audit_for_test(
        sandbox: Sandbox,
        allow_config: AllowConfig,
        supervisor: Arc<JsWorkerSupervisor>,
        error: AuditError,
    ) -> Self {
        Self::new_with_runtime(
            sandbox,
            None,
            None,
            allow_config,
            supervisor,
            Some(Err(error)),
        )
    }
}

/// Populates the process-wide effect audit now, so the first `JsTool::new` finds it ready.
/// Intended for a blocking-pool thread; a concurrent `JsTool::new` waits on the same `OnceLock`.
pub(crate) fn warm_shared_effect_audit() {
    let _ = shared_effect_audit();
}

/// Terminal group-commit barrier for one invocation: one `fsync` (on the blocking pool, never
/// under the executor) covers every completion appended since the last intent/denial sync.
async fn flush_pending_audit(audit: SharedEffectAudit) -> Result<(), ToolError> {
    let unavailable = || audit_error(AuditError::Unavailable);
    crate::agent::runner::spawn_blocking_scoped(move || {
        audit
            .lock()
            .map_err(|_| AuditError::Unavailable)
            .and_then(|mut audit| audit.sync_pending())
    })
    .await
    .map_err(|_| unavailable())?
    .map_err(|error| {
        tracing::warn!(%error, "JS effect audit terminal sync failed");
        audit_error(error)
    })
}

/// Model-facing text for an audit failure: the failure class and a remediation, never a path.
/// The detailed stage/errno is logged where the failure happens.
fn audit_unavailable_message(error: AuditError) -> &'static str {
    match error {
        AuditError::WriterLocked => {
            "JS effect audit unavailable: every audit slot is held by other mini-agent sessions; close one and retry later, or use read, grep or shell"
        }
        AuditError::PathUnavailable => {
            "JS effect audit unavailable: audit storage is not accessible (check permissions of the mini-agent state directory)"
        }
        AuditError::SyncFailed => {
            "JS effect audit unavailable: durable write failed (check free disk space in the mini-agent state directory)"
        }
        AuditError::CorruptRecord
        | AuditError::HashMismatch
        | AuditError::MissingSegment
        | AuditError::InvalidMetadata => {
            "JS effect audit unavailable: audit data is corrupt or incomplete (move the js-effects state directory aside to start a fresh audit)"
        }
        AuditError::RetentionLimit => {
            "JS effect audit unavailable: audit retention limit reached (archive or remove old audit segments)"
        }
        AuditError::KeyUnavailable => {
            "JS effect audit unavailable: audit target-correlation key is unavailable"
        }
        _ => {
            "JS effect audit unavailable; retrying will not help in this session; use read, grep or shell"
        }
    }
}

fn audit_error(error: AuditError) -> ToolError {
    ToolError::Msg(audit_unavailable_message(error).into())
}

/// Maximum failed `WriterLocked` opens retried within one process before the failure is final.
const AUDIT_OPEN_MAX_RETRIES: u32 = 64;
/// Minimum spacing between retried opens, so a held-slot storm cannot make every call re-scan.
const AUDIT_OPEN_RETRY_SPACING: Duration = Duration::from_secs(2);

/// Process-wide audit handle. A successful open and any permanent failure are cached; a
/// `WriterLocked` failure (every slot held by other sessions) is transient and retried later.
#[derive(Default)]
struct SharedAuditCell {
    state: std::sync::Mutex<SharedAuditState>,
}

#[derive(Default)]
struct SharedAuditState {
    result: Option<Result<SharedEffectAudit, AuditError>>,
    transient_failures: u32,
    last_attempt: Option<Instant>,
}

impl SharedAuditCell {
    fn get_or_open(
        &self,
        now: Instant,
        open: impl FnOnce() -> Result<SharedEffectAudit, AuditError>,
    ) -> Result<SharedEffectAudit, AuditError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match &state.result {
            Some(Ok(audit)) => return Ok(audit.clone()),
            Some(Err(AuditError::WriterLocked)) => {
                let spaced = state.last_attempt.is_none_or(|last| {
                    now.saturating_duration_since(last) >= AUDIT_OPEN_RETRY_SPACING
                });
                if state.transient_failures >= AUDIT_OPEN_MAX_RETRIES || !spaced {
                    return Err(AuditError::WriterLocked);
                }
            }
            Some(Err(error)) => return Err(*error),
            None => {}
        }
        state.last_attempt = Some(now);
        let result = open();
        if matches!(result, Err(AuditError::WriterLocked)) {
            state.transient_failures = state.transient_failures.saturating_add(1);
        }
        state.result = Some(result.clone());
        result
    }
}

fn shared_effect_audit() -> Result<SharedEffectAudit, AuditError> {
    static SHARED: OnceLock<SharedAuditCell> = OnceLock::new();
    SHARED
        .get_or_init(SharedAuditCell::default)
        .get_or_open(Instant::now(), || {
            #[cfg(test)]
            let paths = {
                let root = std::env::temp_dir().join(format!(
                    "mini-agent-js-tool-audit-{}-{}",
                    std::process::id(),
                    uuid::Uuid::new_v4()
                ));
                crate::paths::AppPaths {
                    config_dir: root.join("config"),
                    data_dir: root.join("data"),
                    local_data_dir: root.join("local"),
                    state_dir: root.join("state"),
                    cache_dir: root.join("cache"),
                    credentials_dir: root.join("credentials"),
                    project_dir: None,
                }
            };
            #[cfg(not(test))]
            let paths =
                crate::paths::process_paths_ref().map_err(|_| AuditError::PathUnavailable)?;
            {
                #[cfg(test)]
                let audit = open_first_free_audit_slot(&paths);
                // Outside tests `paths` is already a reference to the process paths.
                #[cfg(not(test))]
                let audit = open_first_free_audit_slot(paths);
                audit
            }
        })
}

/// The audit allows one writer per directory, and every mini-agent on the machine shares the
/// state directory. A process therefore claims the first slot whose writer lock is free, so
/// parallel agents (other panes, headless goals) never lose the `js` tool to each other. Any
/// failure other than a held lock is final: it is logged and returned, not retried.
fn open_first_free_audit_slot(
    paths: &crate::paths::AppPaths,
) -> Result<SharedEffectAudit, AuditError> {
    for slot in 0..MAX_AUDIT_SLOTS {
        match EffectAudit::open(paths.effect_audit_slot(slot)) {
            Ok(audit) => return Ok(Arc::new(std::sync::Mutex::new(audit))),
            Err(AuditError::WriterLocked) => continue,
            Err(error) => {
                tracing::warn!(%error, slot, "JS effect audit could not be opened");
                return Err(error);
            }
        }
    }
    tracing::warn!(
        slots = MAX_AUDIT_SLOTS,
        "every JS effect audit slot is locked"
    );
    Err(AuditError::WriterLocked)
}

/// Upper bound on concurrent audit writers (one slot directory per live mini-agent process).
const MAX_AUDIT_SLOTS: u32 = 64;

/// Idioms taught in the full description; each line addresses a measured wrong answer.
const IDIOMS: &str = "Idioms: one script per question. If the format is unknown, log one \
    sample first. `console.log` is for debugging; the answer is `result(x)` or the last \
    expression (scripts are not functions: no top-level `return`; a plain string answer is \
    capped by the result limit). Count lines with `splitLines(t).length`, never `t.split('\\n')`. \
    Parse JSON that may hold integers above 2^53 with `json_exact`; `JSON.parse` silently \
    rounds them. Use `grep` or `read_lines` for huge files, not `read_file`. To explore a \
    repo, use one script: list the tree with `list_dir`/`glob`, then print the first ~15 lines \
    of each interesting file via `read_lines(path, 1, 15).lines`, not many separate calls. Other \
    pure helpers are listed by `help()`.";

const READ_ONLY_IDIOMS: &str = "Idioms: one script per question; log one sample first if the \
    format is unknown; answer with the last expression, not `return`; count with \
    `splitLines(t).length`, not `split('\\n')`; use `json_exact` for integers above 2^53; use `grep` \
    or `read_lines` for huge files. Other pure helpers are listed by `help()`.";

/// Contracts for `splitLines`, `json_exact`, and `read_lines`, shared by both description variants.
fn helper_globals_doc() -> [String; 4] {
    [
        "`splitLines(text: string): string[]` (pure; handles \\r\\n; no phantom last element)."
            .to_string(),
        "`json_exact(text: string): any` (pure; JSON.parse that returns integers beyond 2^53 as BigInt, not rounded; output with `String(x)`)."
            .to_string(),
        "`toml_parse(text: string): object` and `csv_parse(text: string, {header?: boolean}): string[][] | object[]` (pure; 4 MiB max; TOML inline tables/arrays of tables/quoted keys, CSV quotes/CRLF; integers beyond 2^53 are BigInt)."
            .to_string(),
        format!(
            "`read_lines(path: string, from: number, count?: number): {{lines: string[], eof: boolean}}` (synchronous; one read effect; 1-based `from`, negative = last N lines; files over {} MiB ok, scans max {} MiB, count max {}, about {} MiB returned else eof=false, lines cut at {} KiB). Example: `read_lines('app.log', -50).lines`.",
            READ_FILE_MAX_BYTES / (1024 * 1024),
            READ_LINES_MAX_SCAN_BYTES / (1024 * 1024),
            READ_LINES_MAX_COUNT,
            READ_FILE_MAX_BYTES / (1024 * 1024),
            READ_LINES_MAX_LINE_BYTES / 1024,
        ),
    ]
}

impl Tool for JsTool {
    const NAME: &'static str = "js";
    type Error = ToolError;
    type Args = JsArgs;
    type Output = String;

    fn description(&self) -> String {
        if self.profile == ModelEffectProfile::ReadOnly {
            return format!(
                "Execute JavaScript for bounded read-only codebase exploration and local \
                 computation. Code runs as a strict script in a fresh runtime for every call, \
                 with top-level `await` supported and no persistent JavaScript state. The only \
                 brokered effect globals are `read_file(path: string): string`, \
                 `list_dir(path?: string): {{entries: {{name: string, kind: \
                 'file'|'directory', size: number}}[], truncated: boolean}}`, and \
                 `grep(pattern: string, options?: {{path?: string, include?: string, \
                 case_sensitive?: boolean}}): {{matches: {{path: string, line: number, text: \
                 string}}[], truncated: boolean}}`, plus `splitLines`, `json_exact`, and \
                 `read_lines`. {} `console.log` is available for bounded diagnostics. {} Writer, network, process, session-state, proposal, batch-read, and \
                 glob globals are absent. Return a string, finite number, boolean, or \
                 accessor-free plain JSON value. Limits per call: {} s total, {} MiB JavaScript \
                 heap, {} KiB JavaScript stack, {} KiB result, and {} read effects.",
                helper_globals_doc().join(" "),
                READ_ONLY_IDIOMS,
                STEP_TIMEOUT.as_secs(),
                MEMORY_LIMIT / (1024 * 1024),
                STACK_LIMIT / 1024,
                MAX_RESULT_BYTES / 1024,
                MAX_EFFECTS_PER_STEP,
            );
        }
        let mut globals = vec![
            format!(
                "`read_file(path: string): string` (synchronous; max {} MiB). Example: `const text = read_file('src/main.rs');`",
                READ_FILE_MAX_BYTES / (1024 * 1024)
            ),
            format!(
                "`read_files(paths: string[]): string[]` (synchronous; one brokered batch, max {} paths and {} MiB aggregate JSON content). Example: `const texts = read_files(['README.md', 'SPEC.md']);`",
                READ_FILES_MAX_PATHS,
                READ_FILES_MAX_RESULT_BYTES / (1024 * 1024)
            ),
            helper_globals_doc().join(" "),
            "`list_dir(path?: string): {entries: {name: string, kind: 'file'|'directory', size: number}[], truncated: boolean}` (synchronous; defaults to '.').".to_string(),
            "`glob(pattern: string, options?: {path?: string}): {paths: string[], truncated: boolean}` (synchronous, recursive, respects ignore files). Example: `const files = glob('**/*.rs').paths;`".to_string(),
            "`grep(pattern: string, options?: {path?: string, include?: string, case_sensitive?: boolean}): {matches: {path: string, line: number, text: string}[], truncated: boolean}` (synchronous Rust regex search; case-sensitive by default; matched lines only).".to_string(),
            format!(
                "`write_file(path: string, content: string): void` (synchronous; max {} MiB). Example: `write_file('out.txt', 'done\\n');`",
                WRITE_FILE_MAX_BYTES / (1024 * 1024)
            ),
            format!(
                "`result(value): never` (synchronous terminal structured result; strict plain JSON, max {} KiB; a plain returned value is capped at {} KiB; still accepted after the effect limit, so a catch block can return partial data). Example: `result({{files: 3, ok: true}});`",
                STRUCTURED_RESULT_MAX_BYTES / 1024,
                MAX_RESULT_BYTES / 1024
            ),
            format!(
                "`scratch_put(key: string, value): void` / `scratch_get(key: string): JSON|null` (synchronous, parent-owned across fresh runtimes; keys max {} bytes, values max {} MiB, {} MiB/{} entries per session).",
                SCRATCH_KEY_MAX_BYTES,
                SCRATCH_VALUE_MAX_BYTES / (1024 * 1024),
                SCRATCH_TOTAL_MAX_BYTES / (1024 * 1024),
                SCRATCH_MAX_ENTRIES
            ),
            format!(
                "`console.log(...values): void` (max {} records, {} KiB each, {} KiB total). Example: `console.log('count', 3);`",
                MAX_CONSOLE_RECORDS,
                MAX_CONSOLE_RECORD_BYTES / 1024,
                MAX_CONSOLE_BYTES / 1024
            ),
        ];
        #[cfg(feature = "sandbox")]
        {
            globals.push(format!(
                "`fetch(url: string, options?: {{method?: 'GET'|'POST', headers?: Record<string,string>, body?: string}}): {{status: number, text: string}}` (synchronous; request bodies are POST-only and capped at {} KiB; response body max {} MiB; the result has no `ok`, `headers`, or `json()`). Example: `const r = fetch('https://example.com/data', {{method: 'GET'}}); console.log(r.status, r.text);`",
                FETCH_REQUEST_BODY_MAX_BYTES / 1024,
                FETCH_RESPONSE_BODY_MAX_BYTES / (1024 * 1024)
            ));
        }
        if self.sandbox.owns_complete_process_tree() {
            globals.push(format!(
                "`spawn(program: string, args: string[]): {{stdout: string, stderr: string, code: number, timed_out: boolean, stdout_truncated: boolean, stderr_truncated: boolean}}` (synchronous; stdout and stderr max {} MiB each; `args` must be an array and no shell parsing occurs). Example: `const p = spawn('git', ['status', '--short']); console.log(p.code, p.stdout);`",
                SPAWN_STDOUT_MAX_BYTES.min(SPAWN_STDERR_MAX_BYTES) / (1024 * 1024)
            ));
        }
        #[cfg(feature = "skills")]
        let mut proposal_guidance = "";
        #[cfg(feature = "skills")]
        if self.proposal_service.is_some() {
            globals.push(
                "`propose_skill(draft): string` (synchronous; returns the JSON string \
                 `{id, proposal_id, status, report_id}`)."
                    .to_string(),
            );
            proposal_guidance = " After a pattern proves repeated and generalizable, curate it with \
                propose_skill({source, description, exports: [{name, signature}], tests, \
                capability: {tier, grants}, tags?, predecessor_id?}). The stored source declares \
                a hidden capability parameter FIRST in every export: write `function name(_cap, \
                a, b) {...}`, while callers (and tests) omit it and write `name(a, b)`; \
                `signature` shows the caller's view without `_cap`. Every test must be a \
                JavaScript expression that returns exactly true. Tier is pure, read_only, or \
                side_effecting. Grants use {kind: 'read_file'|'write_file', workspace_prefixes}, \
                {kind: 'fetch', origins, methods}, or {kind: 'spawn', programs}; use [] when no \
                effects are needed. A proposal is an immutable candidate for verification and \
                human-gated admission; it is not executed or activated by proposing it. The \
                returned `id` is the canonical immutable revision id of that candidate and never \
                changes; `proposal_id` is the same id. The returned status `pending` is only an \
                acknowledgement that the candidate was durably queued: evaluation runs off this \
                call, and the proposal later settles as awaiting_approval, rejected, verified \
                (gates passed but a held-out suite is still required), or deferred. A settled \
                outcome is reported at the end of a later js call; proposing the same draft again \
                does not speed it up.";
        }
        #[cfg(not(feature = "skills"))]
        let proposal_guidance = "";
        format!(
            "Execute JavaScript code. Prefer this tool for computation, parsing, data \
             transformation, control flow, and cross-platform automation instead of invoking \
             Python through a shell. Use read/grep for direct lookup and shell for builds, tests, \
             version control, or commands that need shell semantics. Code runs as a strict script \
             in a fresh runtime for every call, with top-level `await` supported; no variables or \
             other JavaScript state persist between calls. Host globals are synchronous, \
             so awaiting them is optional. Available global contracts and examples: {} Worked \
             multi-file aggregate: `const total = read_files(glob('**/*.txt').paths).reduce((n, \
             text) => n + text.length, 0); total`. Strings, finite numbers, booleans, and \
             accessor-free plain JSON objects/arrays are valid results. For values containing \
             `undefined`, NaN/Infinity, holes, accessors, Date, Map, Set, or class instances, end \
             with `JSON.stringify(value)` or return a string explicitly. Use `result(value)` for \
             a validated structured result and `scratch_put`/`scratch_get` for JSON values that \
             must survive the next fresh-runtime call. {} \
             Limits per call: {} s total, {} MiB JavaScript heap, {} KiB JavaScript stack, {} KiB \
             result, and {} effect calls. The next effect returns a closed effect-limit error while \
             preserving bounded console output. Catchable effect failures are plain `Error` \
             objects with a stable `.code` such as `not_found`, `is_directory`, `denied`, \
             `invalid_target`, or `too_large`; messages never include the target. Runtime failures \
             use closed, source-free error classes.{}",
            globals.join(" "),
            IDIOMS,
            STEP_TIMEOUT.as_secs(),
            MEMORY_LIMIT / (1024 * 1024),
            STACK_LIMIT / 1024,
            MAX_RESULT_BYTES / 1024,
            MAX_EFFECTS_PER_STEP,
            proposal_guidance,
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "JavaScript code to execute" }
            },
            "required": ["code"]
        })
    }

    async fn call(&self, args: JsArgs) -> Result<String, ToolError> {
        self.permission_bridge
            .bridge()
            .check_async("js", &args.code)
            .await
            .map_err(|error| ToolError::Msg(error.to_string()))?;

        let audit = match &self.audit {
            Some(audit) => audit.clone(),
            None => tokio::task::spawn_blocking(shared_effect_audit)
                .await
                .map_err(|_| audit_error(AuditError::Unavailable))?,
        }
        .map_err(audit_error)?;

        // Create one absolute deadline for the entire invocation (30s from start)
        let call_deadline = Instant::now()
            .checked_add(STEP_TIMEOUT)
            .ok_or_else(|| ToolError::Msg("JS tool deadline unavailable".into()))?;

        let cancellation = PermCancellation::new();
        let mut cancel_on_drop = CancelOnDrop::new(cancellation.clone());
        #[cfg(feature = "skills")]
        let skill_bundle = self.skill_turn_context.snapshot();
        #[cfg(feature = "skills")]
        if self.profile == ModelEffectProfile::ReadOnly
            && skill_bundle.skills.iter().any(|skill| {
                skill.capability.tier != crate::extras::js::skills::CapabilityTier::Pure
            })
        {
            return Err(ToolError::Msg(
                "read-only JS skill bundle contains a non-pure capability".into(),
            ));
        }
        #[cfg(feature = "skills")]
        let skill_tool_call_id = format!(
            "{}:js:{}",
            skill_bundle.turn_id,
            self.skill_tool_call_ordinal.fetch_add(1, Ordering::Relaxed)
        );
        #[cfg(not(feature = "skills"))]
        let skill_tool_call_id = format!("js:{}", uuid::Uuid::new_v4());
        let invocation_id = InvocationId::new(format!("tool:{}", uuid::Uuid::new_v4()))
            .map_err(|_| ToolError::Msg("JS invocation identity unavailable".into()))?;
        #[cfg(feature = "skills")]
        let prepared_skill_manifests =
            prepare_skill_manifests(&skill_bundle, call_deadline, cancellation.clone()).await?;
        let grant_expires_at = call_deadline;
        let model_capabilities = self.model_capabilities();
        let grant = InvocationGrant::issue(
            invocation_id.clone(),
            GrantPrincipal::ModelAuthored {
                tool_call_id: skill_tool_call_id.clone(),
            },
            model_capabilities.clone(),
            grant_expires_at,
        );
        let model_grant_id = grant.grant_id().clone();
        let grants = vec![grant];
        let session_capabilities = model_capabilities;
        #[cfg(feature = "skills")]
        let mut grants = grants;
        #[cfg(feature = "skills")]
        let mut session_capabilities = session_capabilities;
        #[cfg(feature = "skills")]
        let proposal_grant_id = (self.profile == ModelEffectProfile::Full)
            .then_some(self.proposal_service.as_ref())
            .flatten()
            .map(|_| {
                let proposal_grant = InvocationGrant::issue(
                    invocation_id.clone(),
                    GrantPrincipal::ModelAuthored {
                        tool_call_id: skill_tool_call_id.clone(),
                    },
                    std::collections::BTreeSet::from([HostCapability::ProposeSkill]),
                    grant_expires_at,
                );
                let id = proposal_grant.grant_id().clone();
                grants.push(proposal_grant);
                session_capabilities.insert(HostCapability::ProposeSkill);
                id
            });
        #[cfg(feature = "skills")]
        let skill_call_authority = build_skill_call_authority(
            &skill_bundle,
            &prepared_skill_manifests,
            &skill_tool_call_id,
            grant_expires_at,
        )?;
        let bridge = self
            .permission_bridge
            .bridge()
            .for_invocation(cancellation.clone());
        // Compute remaining time from the absolute call_deadline for all effect services
        let remaining_deadline = call_deadline.saturating_duration_since(Instant::now());
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                bridge.clone(),
                self.allow_config.clone(),
                remaining_deadline,
            ),
            SpawnEffectService::new(self.sandbox.clone(), bridge.clone(), remaining_deadline),
        )
        .with_scratch_store(self.scratch.clone());
        #[cfg(feature = "sandbox")]
        let service = service.with_fetch(FetchEffectService::new(
            bridge,
            self.runtime.clone(),
            self.allow_config.fetch_policy(),
            remaining_deadline,
        ));
        #[cfg(feature = "skills")]
        let service = if let Some(proposal) = self.proposal_service.clone() {
            service.with_proposal(proposal)
        } else {
            service
        };
        let structured_result_receipt = super::session::StructuredResultReceipt::default();
        let terminal_audit = Arc::clone(&audit);
        let broker = InvocationBroker::new(
            invocation_id.clone(),
            grants,
            session_capabilities,
            service,
            audit,
        )
        .map_err(|_| ToolError::Msg("JS invocation authority unavailable".into()))?
        .with_structured_result_receipt(structured_result_receipt.clone());
        #[cfg(feature = "skills")]
        let broker = broker.with_skill_call_authority(skill_call_authority);
        #[cfg(feature = "skills")]
        let capability_denials = broker.capability_denial_tracker();
        let model_source = args.code;
        let run_step = RunStep::new(model_source.clone())
            .with_model_grant(model_grant_id)
            .with_model_effect_profile(self.profile)
            .with_spawn_available(self.spawn_available());
        #[cfg(feature = "skills")]
        let run_step = if let Some(grant_id) = proposal_grant_id {
            run_step.with_proposal_grant(grant_id)
        } else {
            run_step
        };
        #[cfg(feature = "skills")]
        let run_step = run_step.with_skills(
            Vec::new(),
            skill_bundle.turn_id.clone(),
            skill_tool_call_id.clone(),
        );
        let invocation_state = cancellation.clone();
        #[cfg(feature = "skills")]
        let execution = self.supervisor.execute_bound_with_deadline_and_skills(
            invocation_id,
            run_step,
            skill_bundle.clone(),
            broker,
            cancellation,
            Some(call_deadline),
        );
        #[cfg(not(feature = "skills"))]
        let execution = self.supervisor.execute_bound_with_deadline(
            invocation_id,
            run_step,
            broker,
            cancellation,
            Some(call_deadline),
        );
        let execution_result = execution.await;
        // Effect completions are group-committed (phase-6 spec, effect-audit ordering): make any
        // still-unsynced completion durable before the step result is released. Fail closed.
        flush_pending_audit(terminal_audit).await?;
        let response = match execution_result {
            Ok(response) => response,
            Err(error) => {
                return render_worker_execution_error(
                    error,
                    invocation_state.permission_prompt_blocked_deadline(),
                );
            }
        };
        cancel_on_drop.disarm();
        validate_step_result_bounds(&response, &model_source)?;
        validate_structured_result_receipt(&response, &structured_result_receipt)?;
        if invocation_state.permission_prompt_blocked_deadline()
            && matches!(
                response.outcome,
                StepOutcome::Error(_) | StepOutcome::Timeout
            )
        {
            return Ok(PERMISSION_PROMPT_TIMEOUT_MESSAGE.to_string());
        }

        // mini-agent-pff43: a read-only child runs without a telemetry dispatcher by design
        // (`fork_for_read_only_child`), so its skill events have nowhere to go. That is not an
        // observability loss: nothing is reported or marked lost. A parent tool without a
        // dispatcher still fails closed inside `dispatch_skill_telemetry`.
        #[cfg(feature = "skills")]
        if self.profile != ModelEffectProfile::ReadOnly || self.telemetry.is_some() {
            dispatch_skill_telemetry(
                self.telemetry.as_deref(),
                &self.skill_turn_context,
                &skill_bundle,
                &skill_tool_call_id,
                &response.outcome,
                &response.skill_events,
                response.evidence_complete,
                capability_denials.snapshot(),
                self.skill_production,
            );
        }

        let output = render_step_result(&response);
        #[cfg(feature = "skills")]
        let output = match self.proposal_service.as_ref() {
            Some(service) => with_settled_proposal_outcomes(output, service),
            None => output,
        };
        Ok(output)
    }
}

/// Append the settled admission outcome of every proposal this session
/// enqueued and has not reported yet.
///
/// `propose_skill` answers with a queue acknowledgement (`pending`), so this
/// follow-up note is how the model learns that its candidate reached
/// `awaiting_approval`, was rejected, or was deferred. Outcomes are closed
/// values: a canonical revision id, a status token, and an internal reason
/// code; a proposal that is still queued stays tracked for a later call.
#[cfg(feature = "skills")]
pub(crate) fn with_settled_proposal_outcomes(
    mut output: String,
    service: &ProposalEffectService,
) -> String {
    let outcomes = service.settled_outcomes();
    if outcomes.is_empty() {
        return output;
    }
    // `propose_skill` answers with a JSON object, and appending prose to it
    // would leave the tool result unparseable for the very caller that asked
    // for the proposal. Carry the notice as a field when the result is an
    // object, and fall back to prose for every other JavaScript value.
    if let Ok(serde_json::Value::Object(mut object)) =
        serde_json::from_str::<serde_json::Value>(&output)
    {
        let settled = outcomes
            .iter()
            .map(|outcome| {
                serde_json::json!({
                    "proposal_id": outcome.proposal_id,
                    "status": outcome.status.as_str(),
                    "reason_code": outcome.reason_code,
                    "detail": outcome.detail,
                })
            })
            .collect::<Vec<_>>();
        object.insert(
            "settled_proposals".to_string(),
            serde_json::Value::Array(settled),
        );
        return serde_json::Value::Object(object).to_string();
    }
    for outcome in outcomes {
        output.push_str("\n\nSkill proposal ");
        output.push_str(&outcome.proposal_id);
        output.push_str(" settled as ");
        output.push_str(outcome.status.as_str());
        if let Some(reason_code) = outcome.reason_code.as_deref() {
            output.push_str(" (reason: ");
            output.push_str(reason_code);
            output.push(')');
        }
        if let Some(detail) = outcome.detail.as_deref() {
            output.push_str(" Detail: ");
            output.push_str(detail);
        }
        output.push('.');
    }
    output
}

#[cfg(feature = "skills")]
async fn prepare_skill_manifests(
    bundle: &crate::extras::js::skills::turn::TurnSkillBundle,
    deadline: Instant,
    cancellation: PermCancellation,
) -> Result<Vec<PreparedSkillManifest>, ToolError> {
    validate_skill_bundle_bounds(bundle)?;
    let mut prepared = Vec::with_capacity(bundle.skills.len());
    for skill in &bundle.skills {
        prepared.push(
            InvocationGrant::prepare_skill_manifest(
                skill.capability.clone(),
                deadline,
                cancellation.clone(),
            )
            .await
            .map_err(|_| ToolError::Msg("skill invocation authority unavailable".into()))?,
        );
    }
    Ok(prepared)
}

#[cfg(feature = "skills")]
fn validate_skill_bundle_bounds(
    bundle: &crate::extras::js::skills::turn::TurnSkillBundle,
) -> Result<(), ToolError> {
    if bundle.skills.len() > MAX_SKILL_ARTIFACTS_PER_STEP {
        return Err(ToolError::Msg(
            "skill invocation authority unavailable".into(),
        ));
    }
    let mut total_grants = 0_usize;
    for skill in &bundle.skills {
        if skill.exports.len() > MAX_SKILL_EXPORTS_PER_ARTIFACT {
            return Err(ToolError::Msg(
                "skill invocation authority unavailable".into(),
            ));
        }
        let grants = skill
            .exports
            .len()
            .checked_mul(skill.capability.grants.len())
            .ok_or_else(|| ToolError::Msg("skill invocation authority unavailable".into()))?;
        total_grants = total_grants
            .checked_add(grants)
            .ok_or_else(|| ToolError::Msg("skill invocation authority unavailable".into()))?;
        if total_grants > MAX_SKILL_CAPABILITY_GRANTS_PER_STEP {
            return Err(ToolError::Msg(
                "skill invocation authority unavailable".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "skills")]
fn build_skill_call_authority(
    bundle: &crate::extras::js::skills::turn::TurnSkillBundle,
    prepared_manifests: &[PreparedSkillManifest],
    tool_call_id: &str,
    expires_at: Instant,
) -> Result<SkillCallAuthority, ToolError> {
    if prepared_manifests.len() != bundle.skills.len() {
        return Err(ToolError::Msg(
            "skill invocation authority unavailable".into(),
        ));
    }
    let mut specs = Vec::new();
    for (skill, prepared_manifest) in bundle.skills.iter().zip(prepared_manifests) {
        for export in &skill.exports {
            specs.push(SkillExportAuthoritySpec {
                artifact_id: skill.id.clone(),
                export_name: export.name.clone(),
                prepared_manifest: prepared_manifest.clone(),
            });
        }
    }
    SkillCallAuthority::new(
        bundle.turn_id.clone(),
        tool_call_id.to_string(),
        expires_at,
        specs,
    )
    .map_err(|_| ToolError::Msg("skill invocation authority unavailable".into()))
}

#[cfg(feature = "skills")]
#[allow(clippy::too_many_arguments)]
fn dispatch_skill_telemetry(
    dispatcher: Option<&crate::extras::js::skills::telemetry::TelemetryDispatcher>,
    turn_context: &crate::extras::js::skills::turn::SkillTurnContext,
    bundle: &crate::extras::js::skills::turn::TurnSkillBundle,
    tool_call_id: &str,
    step_outcome: &StepOutcome,
    worker_events: &[crate::extras::js::skills::telemetry::SkillEvent],
    worker_claimed_evidence_complete: bool,
    capability_denials: Option<crate::extras::js::broker::SkillDenialSnapshot>,
    production: bool,
) -> bool {
    use crate::extras::js::skills::telemetry::{
        ParentSkillBinding, ParentTelemetryContext, bind_worker_events, observability_lost_batch,
    };

    let Some(crate::extras::js::broker::SkillDenialSnapshot {
        capability_faults: capability_denials,
        scope_misses,
    }) = capability_denials
    else {
        turn_context.mark_evidence_lost();
        record_observability_lost(dispatcher, "capability_denial_binding_unavailable");
        return false;
    };
    let mut skills = Vec::with_capacity(bundle.skills.len());
    for skill in &bundle.skills {
        let Ok(retrieval_rank) = u32::try_from(skill.rank) else {
            turn_context.mark_evidence_lost();
            record_observability_lost(dispatcher, "parent_binding_unavailable");
            return false;
        };
        skills.push(ParentSkillBinding {
            skill_id: skill.id.clone(),
            exports: skill
                .exports
                .iter()
                .map(|export| export.name.clone())
                .collect(),
            retrieval_score: f64::from(skill.score()),
            retrieval_rank,
        });
    }
    let context = ParentTelemetryContext {
        turn_id: bundle.turn_id.clone(),
        tool_call_id: tool_call_id.to_string(),
        query_fingerprint: (!bundle.query_fingerprint.is_empty())
            .then(|| bundle.query_fingerprint.clone()),
        index_generation: bundle.index_generation,
        production,
        step_outcome: step_outcome.clone(),
        skills,
        capability_denials,
        scope_misses,
    };

    let mut batch = match bind_worker_events(&context, worker_events) {
        Ok(batch) => batch,
        Err(_) => {
            turn_context.mark_evidence_lost();
            record_observability_lost(dispatcher, "invalid_worker_batch");
            if let Some(dispatcher) = dispatcher
                && let Ok(lost) = observability_lost_batch(&context)
                && !lost.events().is_empty()
            {
                let _ = dispatcher.try_dispatch(lost);
            }
            return false;
        }
    };
    if !worker_claimed_evidence_complete {
        turn_context.mark_evidence_lost();
        batch.mark_evidence_incomplete();
    }
    if batch.events().is_empty() {
        return true;
    }
    let Some(dispatcher) = dispatcher else {
        turn_context.mark_evidence_lost();
        record_observability_lost(None, "dispatcher_unavailable");
        return false;
    };
    match dispatcher.try_dispatch(batch) {
        Ok(()) => true,
        Err(_) => {
            // The queue that would carry the loss report is the one that just
            // rejected this batch, so the parent records the loss itself.
            turn_context.mark_evidence_lost();
            dispatcher.record_observability_lost("dispatch_failed");
            false
        }
    }
}

#[cfg(feature = "skills")]
fn record_observability_lost(
    dispatcher: Option<&crate::extras::js::skills::telemetry::TelemetryDispatcher>,
    reason: &'static str,
) {
    if let Some(dispatcher) = dispatcher {
        dispatcher.record_observability_lost(reason);
    } else {
        tracing::warn!(
            event_kind =
                crate::extras::js::skills::telemetry::SkillEventKind::ObservabilityLost.as_token(),
            reason,
            "skill telemetry observability was lost; positive evidence was excluded"
        );
    }
}

fn worker_tool_error(error: WorkerError) -> ToolError {
    ToolError::Msg(error.to_string())
}

const PERMISSION_PROMPT_TIMEOUT_MESSAGE: &str = "JS error: permission prompt not answered within the 30s budget; do not retry without a new user decision";

fn render_worker_execution_error(
    error: WorkerError,
    permission_prompt_blocked_deadline: bool,
) -> Result<String, ToolError> {
    match error {
        WorkerError::PermissionPromptTimedOut => Ok(PERMISSION_PROMPT_TIMEOUT_MESSAGE.to_string()),
        WorkerError::TimedOut if permission_prompt_blocked_deadline => {
            Ok(PERMISSION_PROMPT_TIMEOUT_MESSAGE.to_string())
        }
        WorkerError::TimedOut => Ok("JS error: execution timed out (30s limit exceeded)".into()),
        error => Err(worker_tool_error(error)),
    }
}

/// Parent-side ceilings for worker-supplied result and console payloads.
///
/// They are the same constants the worker enforces (`MAX_RESULT_BYTES`,
/// `MAX_CONSOLE_*` in `protocol.rs`). The parent never trusts the worker to have applied them:
/// a step result that exceeds them is a protocol violation and is rejected
/// instead of being forwarded to the model.
fn validate_step_result_bounds(result: &StepResult, model_source: &str) -> Result<(), ToolError> {
    let violation = || worker_tool_error(WorkerError::Protocol);
    match &result.outcome {
        StepOutcome::Value(value) if value.len() > MAX_RESULT_BYTES => return Err(violation()),
        StepOutcome::Structured(value) => {
            // Count without materializing a copy of the (up to 256 KiB) document.
            let mut counter = JsonByteCounter::default();
            serde_json::to_writer(&mut counter, value).map_err(|_| violation())?;
            if counter.bytes > STRUCTURED_RESULT_MAX_BYTES {
                return Err(violation());
            }
        }
        _ => {}
    }
    if result.console.len() > MAX_CONSOLE_RECORDS {
        return Err(violation());
    }
    let mut console_bytes = 0_usize;
    for record in &result.console {
        if record.text.len() > MAX_CONSOLE_RECORD_BYTES {
            return Err(violation());
        }
        console_bytes = console_bytes.saturating_add(record.text.len());
        if console_bytes > MAX_CONSOLE_BYTES {
            return Err(violation());
        }
    }
    validate_step_diagnostic(result, model_source).map_err(|()| violation())?;
    Ok(())
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: usize,
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Fails the write as soon as the serialized bytes diverge from `expected`.
struct JsonPrefixComparer<'a> {
    expected: &'a [u8],
    offset: usize,
}

impl<'a> JsonPrefixComparer<'a> {
    fn new(expected: &'a [u8]) -> Self {
        Self {
            expected,
            offset: 0,
        }
    }

    fn is_exact(&self) -> bool {
        self.offset == self.expected.len()
    }
}

impl std::io::Write for JsonPrefixComparer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let end = self.offset.saturating_add(buf.len());
        if self.expected.get(self.offset..end) != Some(buf) {
            return Err(std::io::Error::other("structured result mismatch"));
        }
        self.offset = end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn validate_structured_result_receipt(
    result: &StepResult,
    receipt: &super::session::StructuredResultReceipt,
) -> Result<(), ToolError> {
    let violation = || worker_tool_error(WorkerError::Protocol);
    let accepted = receipt.accepted().map_err(|_| violation())?;
    match (&result.outcome, accepted) {
        (StepOutcome::Structured(value), Some(expected)) => {
            // Stream-compare against the parent-authoritative string; no allocation and it
            // stops at the first differing byte.
            let mut comparer = JsonPrefixComparer::new(expected.as_bytes());
            serde_json::to_writer(&mut comparer, value).map_err(|_| violation())?;
            comparer.is_exact().then_some(()).ok_or_else(violation)
        }
        (StepOutcome::Structured(_), None) | (_, Some(_)) => Err(violation()),
        (_, None) => Ok(()),
    }
}

fn validate_step_diagnostic(result: &StepResult, model_source: &str) -> Result<(), ()> {
    let Some(diagnostic) = &result.diagnostic else {
        return matches!(
            result.outcome,
            StepOutcome::Value(_) | StepOutcome::Structured(_) | StepOutcome::Void
        )
        .then_some(())
        .ok_or(());
    };
    if matches!(
        result.outcome,
        StepOutcome::Value(_) | StepOutcome::Structured(_) | StepOutcome::Void
    ) {
        return Err(());
    }
    if diagnostic.hint.is_some()
        && (diagnostic.script_role != ScriptRole::Model
            || !matches!(
                diagnostic.class,
                DiagnosticClass::Exception | DiagnosticClass::Syntax
            ))
    {
        return Err(());
    }
    let location = match (diagnostic.line, diagnostic.column) {
        (None, None) => None,
        (Some(line), Some(column))
            if diagnostic.script_role == ScriptRole::Model
                && diagnostic.exception_class.is_some()
                && source_position_is_valid(model_source, line, column) =>
        {
            Some((line, column))
        }
        _ => return Err(()),
    };
    if location.is_some() && diagnostic.script_role != ScriptRole::Model {
        return Err(());
    }

    let valid = match result.outcome {
        StepOutcome::Error(JsErrorCode::Syntax) => {
            diagnostic.class == DiagnosticClass::Syntax
                && diagnostic.script_role == ScriptRole::Model
                && diagnostic.exception_class == Some(JsExceptionClass::SyntaxError)
        }
        StepOutcome::Error(JsErrorCode::StackLimit) => {
            diagnostic.class == DiagnosticClass::ResourceLimit
                && diagnostic.script_role == ScriptRole::Model
                && diagnostic.exception_class == Some(JsExceptionClass::RangeError)
        }
        StepOutcome::Error(JsErrorCode::Exception) => {
            diagnostic.class == DiagnosticClass::Exception
                && (diagnostic.script_role != ScriptRole::Model
                    || diagnostic.exception_class.is_some())
        }
        StepOutcome::Error(JsErrorCode::JobLimit | JsErrorCode::EffectLimit) => {
            diagnostic.class == DiagnosticClass::ResourceLimit
                && diagnostic.exception_class.is_none()
        }
        StepOutcome::Error(JsErrorCode::InvalidResult) => {
            diagnostic.class == DiagnosticClass::Contract && diagnostic.exception_class.is_none()
        }
        StepOutcome::Error(JsErrorCode::Internal) => {
            diagnostic.class == DiagnosticClass::Internal && diagnostic.exception_class.is_none()
        }
        StepOutcome::Timeout | StepOutcome::OutOfMemory => {
            diagnostic.class == DiagnosticClass::ResourceLimit
                && diagnostic.exception_class.is_none()
        }
        StepOutcome::Value(_) | StepOutcome::Structured(_) | StepOutcome::Void => false,
    };
    valid.then_some(()).ok_or(())
}

/// Renders the model-visible text for a bounded step result: console records
/// in emission order (level-prefixed), the returned value, and on failure the
/// stable diagnostic stage / script role.
fn render_step_result(result: &StepResult) -> String {
    match &result.outcome {
        StepOutcome::Value(value) => {
            let mut text = String::with_capacity(value.len());
            render_console(&result.console, &mut text);
            text.push_str(value);
            text
        }
        StepOutcome::Structured(value) => {
            let value = serde_json::to_string(value)
                .expect("validated structured result remains serializable");
            let mut text = String::with_capacity(value.len());
            render_console(&result.console, &mut text);
            text.push_str(&value);
            text
        }
        StepOutcome::Void => {
            let mut text = String::new();
            render_console(&result.console, &mut text);
            if text.ends_with('\n') {
                text.pop();
            }
            text
        }
        StepOutcome::Error(code) => render_failure(
            &js_error_headline(*code, result.diagnostic.as_ref()),
            result,
        ),
        StepOutcome::Timeout => {
            render_failure("JS error: execution timed out (30s limit exceeded)", result)
        }
        StepOutcome::OutOfMemory => {
            render_failure("JS error: out of memory (64 MiB limit exceeded)", result)
        }
    }
}

fn render_failure(headline: &str, result: &StepResult) -> String {
    let mut text = String::from(headline);
    if let Some(diagnostic) = &result.diagnostic {
        if let (Some(line), Some(column)) = (diagnostic.line, diagnostic.column) {
            text.push_str(&format!(" at {line}:{column}"));
        }
        if let Some(hint) = diagnostic.hint {
            text.push_str(": ");
            text.push_str(hint.text());
        }
        text.push_str(" (");
        render_diagnostic(diagnostic, &mut text);
        text.push(')');
    }
    if !result.console.is_empty() {
        text.push('\n');
        render_console(&result.console, &mut text);
        text.pop();
    }
    text
}

fn js_error_headline(code: JsErrorCode, diagnostic: Option<&Diagnostic>) -> String {
    if code == JsErrorCode::InvalidResult {
        return format!(
            "JS error: invalid result; return a string/plain JSON value of at most {} KiB or end with JSON.stringify(value) (cap for returned values; result(value), where available, allows {} KiB)",
            MAX_RESULT_BYTES / 1024,
            STRUCTURED_RESULT_MAX_BYTES / 1024
        );
    }
    if code == JsErrorCode::EffectLimit {
        return format!(
            "JS error: effect limit exceeded ({MAX_EFFECTS_PER_STEP} per call); batch with read_files(paths) or glob()"
        );
    }
    let Some(exception_class) = diagnostic.and_then(|diagnostic| diagnostic.exception_class) else {
        return format!("JS error: {}", js_error_code(code));
    };
    if code == JsErrorCode::StackLimit {
        return format!(
            "JS {}: stack limit exceeded ({} KiB stack)",
            js_exception_class_name(exception_class),
            STACK_LIMIT / 1024
        );
    }
    if exception_class == JsExceptionClass::Other {
        return "JS exception".into();
    }
    format!("JS {}", js_exception_class_name(exception_class))
}

fn js_exception_class_name(class: JsExceptionClass) -> &'static str {
    match class {
        JsExceptionClass::SyntaxError => "SyntaxError",
        JsExceptionClass::TypeError => "TypeError",
        JsExceptionClass::ReferenceError => "ReferenceError",
        JsExceptionClass::RangeError => "RangeError",
        JsExceptionClass::InternalError => "InternalError",
        JsExceptionClass::Other => "exception",
    }
}

fn render_console(records: &[ConsoleRecord], text: &mut String) {
    for record in records {
        text.push_str("[console.");
        text.push_str(console_level_name(record.level));
        text.push_str("] ");
        text.push_str(&record.text);
        if record.truncated {
            text.push_str(" [truncated]");
        }
        text.push('\n');
    }
}

fn render_diagnostic(diagnostic: &Diagnostic, text: &mut String) {
    text.push_str("stage: ");
    text.push_str(diagnostic_stage_name(diagnostic.stage));
    text.push_str("; script: ");
    text.push_str(script_role_name(diagnostic.script_role));
}

fn console_level_name(level: ConsoleLevel) -> &'static str {
    match level {
        ConsoleLevel::Log => "log",
        ConsoleLevel::Warn => "warn",
        ConsoleLevel::Error => "error",
    }
}

fn diagnostic_stage_name(stage: DiagnosticStage) -> &'static str {
    match stage {
        DiagnosticStage::Initialization => "initialization",
        DiagnosticStage::Evaluation => "evaluation",
        DiagnosticStage::JobDrain => "job_drain",
        DiagnosticStage::ResultConversion => "result_conversion",
        DiagnosticStage::Verification => "verification",
    }
}

fn script_role_name(role: ScriptRole) -> &'static str {
    match role {
        ScriptRole::Model => "model",
        ScriptRole::SkillSource => "skill_source",
        ScriptRole::EmbeddedTest => "embedded_test",
        ScriptRole::MutationTest => "mutation_test",
        ScriptRole::InheritedTest => "inherited_test",
        ScriptRole::HeldOutTest => "held_out_test",
    }
}

fn js_error_code(code: JsErrorCode) -> &'static str {
    match code {
        JsErrorCode::Syntax => "syntax error",
        JsErrorCode::Exception => "exception",
        JsErrorCode::StackLimit => "stack limit exceeded",
        JsErrorCode::JobLimit => "job limit exceeded",
        JsErrorCode::EffectLimit => "effect limit exceeded",
        JsErrorCode::InvalidResult => "invalid result",
        JsErrorCode::Internal => "internal error",
    }
}

#[derive(Deserialize)]
pub struct JsArgs {
    pub code: String,
}

// Tests of the `pack_diff.js` / `pack_iac.js` segments, kept in their own file.
#[cfg(test)]
#[path = "pack_diff_iac_tests.rs"]
mod pack_diff_iac_tests;

#[cfg(test)]
mod js_permission_bridge {
    use std::sync::Mutex;

    use super::*;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    fn permission(action: Action) -> PermCheck {
        // Use "read" tool (not "bash") so ToolPerm::Simple's "**" glob matches
        // any key via normal pattern matching. Bash uses model-B exact-match
        // for Allow rules, which rejects the "**" wildcard and falls through to
        // the mode default, causing spurious NonInteractive denials.
        let config = PermissionConfig {
            read: Some(ToolPerm::Simple(action)),
            ..PermissionConfig::default()
        };
        Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(config),
                SecurityMode::Restrictive,
                std::env::current_dir().ok(),
                Some(vec!["restrictive".to_string()]),
            )
            .expect("valid permission test configuration"),
        ))
    }

    fn raw_bridge(
        timeout: Duration,
    ) -> (
        PermissionBridge,
        tokio_mpsc::UnboundedReceiver<PermissionEnvelope>,
        PermCancellation,
    ) {
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        let shutdown = PermCancellation::new();
        (
            PermissionBridge::new(tx, shutdown.clone(), timeout),
            rx,
            shutdown,
        )
    }

    fn reply(envelope: PermissionEnvelope, outcome: PermOutcome) {
        let request_id = envelope.request.id();
        envelope.reply.send(PermResponse::new(request_id, outcome));
    }

    #[tokio::test]
    async fn js_tool_constructor_defers_shared_audit_open() {
        let tool = JsTool::new(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        );

        assert!(tool.audit.is_none());
    }

    #[tokio::test]
    async fn js_permission_bridge_propagates_allow_deny_and_ask_approval() {
        let allow_owner =
            PermissionBridgeOwner::new(Some(permission(Action::Allow)), None, STEP_TIMEOUT);
        allow_owner
            .bridge()
            .check_async("read", "allowed-file.txt")
            .await
            .expect("allow should pass");

        let deny_owner =
            PermissionBridgeOwner::new(Some(permission(Action::Deny)), None, STEP_TIMEOUT);
        assert!(
            matches!(
                deny_owner
                    .bridge()
                    .check_async("read", "denied-file.txt")
                    .await,
                Err(PermissionBridgeError::Denied(PermissionDenial::Policy(_)))
            ),
            "configured denial should stay typed as a policy denial"
        );

        let (ask_tx, mut ask_rx) = tokio_mpsc::channel(1);
        let ask_owner =
            PermissionBridgeOwner::new(Some(permission(Action::Ask)), Some(ask_tx), STEP_TIMEOUT);
        let bridge = ask_owner.bridge();
        let approval =
            tokio::spawn(async move { bridge.check_async("read", "ask-file.txt").await });
        let request = ask_rx
            .recv()
            .await
            .expect("ask request should arrive")
            .into_permission();
        assert_eq!(
            request.diagnostic,
            Some(crate::permission::ask::AskDiagnostic {
                effective_mode: SecurityMode::Restrictive,
                selected_mode: SecurityMode::Restrictive,
                cause: crate::permission::ask::AskCause::ConfiguredAskRule,
            })
        );
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("approval receiver should still exist");
        approval
            .await
            .expect("approval task should not panic")
            .expect("approval should allow the request");
    }

    #[tokio::test]
    async fn permission126_js_broker_benign_heredoc_yolo_zero_ask_standard_one() {
        let input =
            "python3 - <<'PY'\nprint('read only')\nPY\necho ok | wc -l > /tmp/owned-report.txt";
        for mode in [SecurityMode::Yolo, SecurityMode::Standard] {
            let permission = Arc::new(Mutex::new(
                PermissionChecker::new(
                    &PermissionConfigs::default(),
                    mode,
                    std::env::current_dir().ok(),
                    None,
                )
                .unwrap(),
            ));
            let (sender, mut receiver) = tokio_mpsc::channel(1);
            let owner = PermissionBridgeOwner::new(Some(permission), Some(sender), STEP_TIMEOUT);
            let bridge = owner.bridge();
            let call = bridge.check_async("shell", input);
            tokio::pin!(call);
            if mode == SecurityMode::Yolo {
                tokio::time::timeout(Duration::from_secs(2), &mut call)
                    .await
                    .expect("unmatched Yolo broker check must not wait for approval")
                    .unwrap();
                assert!(receiver.try_recv().is_err());
            } else {
                let request = tokio::time::timeout(Duration::from_secs(2), async {
                    tokio::select! {
                        request = receiver.recv() => request.unwrap().into_permission(),
                        result = &mut call => panic!("broker completed before approval: {result:?}"),
                    }
                }).await.unwrap();
                assert_eq!(request.input, input);
                assert_eq!(
                    request.diagnostic,
                    Some(crate::permission::ask::AskDiagnostic {
                        effective_mode: mode,
                        selected_mode: mode,
                        cause: crate::permission::ask::AskCause::ModePolicy,
                    })
                );
                request.reply.send(UserDecision::Deny).unwrap();
                assert_eq!(
                    call.await,
                    Err(PermissionBridgeError::Denied(PermissionDenial::User))
                );
                assert!(receiver.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn js_permission_bridge_reports_backend_channel_failures() {
        let (closed_tx, closed_rx) = tokio_mpsc::channel(1);
        drop(closed_rx);
        let closed_owner = PermissionBridgeOwner::new(
            Some(permission(Action::Ask)),
            Some(closed_tx),
            STEP_TIMEOUT,
        );
        assert_eq!(
            closed_owner
                .bridge()
                .check_async("read", "closed-file.txt")
                .await,
            Err(PermissionBridgeError::BackendFailure(
                PermissionBackendFailure::AskChannelClosed
            ))
        );

        let (dropped_tx, mut dropped_rx) = tokio_mpsc::channel(1);
        let dropped_owner = PermissionBridgeOwner::new(
            Some(permission(Action::Ask)),
            Some(dropped_tx),
            STEP_TIMEOUT,
        );
        let bridge = dropped_owner.bridge();
        let check =
            tokio::spawn(async move { bridge.check_async("read", "dropped-file.txt").await });
        drop(dropped_rx.recv().await.expect("ask request should arrive"));
        assert_eq!(
            check.await.expect("check task should not panic"),
            Err(PermissionBridgeError::BackendFailure(
                PermissionBackendFailure::AskResponseDropped
            ))
        );
    }

    fn exercise_sync_settlement(expected: PermissionBridgeError) {
        use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

        let (bridge, mut rx, shutdown) = raw_bridge(Duration::from_secs(60));
        let invocation = PermCancellation::new();
        let mut bridge = bridge.for_invocation(invocation.clone());
        let clock = Arc::new(std::sync::Mutex::new(Instant::now()));
        bridge.sync_wait_clock = Some(clock.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        // This guards a broken wait path, not the duration of a correct check.
        let hang_guard = Duration::from_secs(15);

        std::thread::scope(|scope| {
            let (result_tx, mut result_rx) = oneshot::channel();
            let check = scope.spawn(move || {
                let _ = result_tx.send(bridge.check("bash", "sleep forever"));
            });
            let scenario = catch_unwind(AssertUnwindSafe(|| {
                runtime.block_on(async {
                    let envelope = tokio::time::timeout(hang_guard, rx.recv())
                        .await
                        .expect("permission request readiness stalled")
                        .expect("permission request channel closed");
                    assert!(!envelope.request.cancellation().is_cancelled());
                    assert!(matches!(
                        result_rx.try_recv(),
                        Err(oneshot::error::TryRecvError::Empty)
                    ));
                    if expected == PermissionBridgeError::TimedOut {
                        *clock.lock().unwrap() =
                            envelope.request.deadline() + Duration::from_millis(1);
                    } else if expected == PermissionBridgeError::Cancelled {
                        invocation.cancel();
                    }
                    let request_cancellation = envelope.request.cancellation().clone();
                    let retained = if expected == PermissionBridgeError::ResponseChannelClosed {
                        drop(envelope);
                        None
                    } else {
                        Some(envelope)
                    };
                    assert_eq!(
                        tokio::time::timeout(hang_guard, &mut result_rx)
                            .await
                            .expect("permission settlement stalled")
                            .expect("permission check thread panicked"),
                        Err(expected)
                    );
                    assert!(request_cancellation.is_cancelled());
                    if let Some(envelope) = retained {
                        let PermissionReply::Sync(reply) = envelope.reply else {
                            panic!("expected sync channel")
                        };
                        assert!(
                            reply
                                .send(PermResponse::new(
                                    envelope.request.id(),
                                    PermOutcome::Allowed
                                ))
                                .is_err()
                        );
                    }
                });
            }));
            // Release the blocking check on every failure before joining it.
            shutdown.cancel();
            drop(rx);
            let joined = check.join();
            if let Err(payload) = scenario {
                resume_unwind(payload);
            }
            joined.expect("permission check thread panicked during cleanup");
        });
    }

    #[test]
    fn js_permission_bridge_timeout_is_bounded() {
        exercise_sync_settlement(PermissionBridgeError::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn js_permission_bridge_tracks_only_an_unanswered_user_prompt() {
        let (ask_tx, mut ask_rx) = tokio_mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(
            Some(permission(Action::Ask)),
            Some(ask_tx),
            Duration::from_secs(60),
        );
        let invocation = PermCancellation::new();
        let bridge = owner.bridge().for_invocation(invocation.clone());
        let check = bridge.check_async("read", "pending.txt");
        tokio::pin!(check);
        let mut request = tokio::select! {
            request = ask_rx.recv() => request.expect("ask request should arrive"),
            result = &mut check => panic!("permission completed before prompt readiness: {result:?}"),
        }.into_permission();
        assert!(invocation.permission_prompt_pending());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(check.await, Err(PermissionBridgeError::TimedOut));
        request.reply.closed().await;
        assert!(!invocation.permission_prompt_pending());
        assert!(invocation.permission_prompt_blocked_deadline());
        drop(request);

        let noninteractive_owner = PermissionBridgeOwner::new(
            Some(permission(Action::Ask)),
            None,
            Duration::from_secs(60)
                + tokio::time::Instant::now()
                    .into_std()
                    .saturating_duration_since(Instant::now()),
        );
        let noninteractive_invocation = PermCancellation::new();
        assert_eq!(
            noninteractive_owner
                .bridge()
                .for_invocation(noninteractive_invocation.clone())
                .check_async("read", "noninteractive.txt")
                .await,
            Err(PermissionBridgeError::Denied(
                PermissionDenial::NonInteractive(crate::permission::ask::AskDiagnostic {
                    effective_mode: SecurityMode::Restrictive,
                    selected_mode: SecurityMode::Restrictive,
                    cause: crate::permission::ask::AskCause::ConfiguredAskRule,
                })
            ))
        );
        assert!(!noninteractive_invocation.permission_prompt_pending());
        assert!(!noninteractive_invocation.permission_prompt_blocked_deadline());
    }

    #[test]
    fn js_permission_bridge_dropped_request_receiver_is_deterministic() {
        let (bridge, rx, _shutdown) = raw_bridge(Duration::from_secs(1));
        drop(rx);

        assert_eq!(
            bridge.check("bash", "printf disconnected"),
            Err(PermissionBridgeError::RequestChannelClosed)
        );
    }

    #[test]
    fn js_permission_bridge_dropped_response_sender_is_deterministic() {
        exercise_sync_settlement(PermissionBridgeError::ResponseChannelClosed);
    }

    #[test]
    fn js_permission_bridge_cancellation_unblocks_sync_wait() {
        exercise_sync_settlement(PermissionBridgeError::Cancelled);
    }

    fn start_check(
        bridge: PermissionBridge,
        synchronous: bool,
    ) -> tokio::task::JoinHandle<Result<(), PermissionBridgeError>> {
        if synchronous {
            tokio::task::spawn_blocking(move || bridge.check("read", "same-file.txt"))
        } else {
            tokio::spawn(async move { bridge.check_async("read", "same-file.txt").await })
        }
    }

    async fn next_request(
        receiver: &mut tokio_mpsc::UnboundedReceiver<PermissionEnvelope>,
    ) -> PermissionEnvelope {
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("permission request should arrive within the deadline")
            .expect("permission request channel should remain open")
    }

    async fn check_result(
        check: tokio::task::JoinHandle<Result<(), PermissionBridgeError>>,
    ) -> Result<(), PermissionBridgeError> {
        tokio::time::timeout(Duration::from_secs(1), check)
            .await
            .expect("permission check should settle within the deadline")
            .expect("permission check should not panic")
    }

    #[tokio::test]
    async fn js_permission_bridge_late_reply_cannot_satisfy_repeated_call() {
        use futures::FutureExt;
        for synchronous in [true, false] {
            let (mut bridge, mut rx, shutdown) = raw_bridge(Duration::from_secs(60));
            let clock = Arc::new(Mutex::new(Instant::now()));
            bridge.sync_wait_clock = Some(clock.clone());
            let mut checks = Vec::new();
            let scenario = std::panic::AssertUnwindSafe(async {
                checks.push(start_check(bridge.clone(), synchronous));
                let first = next_request(&mut rx).await;
                let first_id = first.request.id();
                if synchronous {
                    *clock.lock().unwrap() = first.request.deadline() + Duration::from_secs(1);
                } else {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(61)).await;
                }
                let result = (&mut checks[0]).await;
                checks.clear();
                if !synchronous {
                    tokio::time::resume();
                }
                assert_eq!(result.unwrap(), Err(PermissionBridgeError::TimedOut));
                *clock.lock().unwrap() = Instant::now();
                bridge.timeout = Duration::from_secs(60)
                    + tokio::time::Instant::now()
                        .into_std()
                        .saturating_duration_since(Instant::now());
                checks.push(start_check(bridge.clone(), synchronous));
                let second = next_request(&mut rx).await;
                assert_ne!(first_id, second.request.id());
                // Observe the actual abandoned response receiver. No scheduling delay
                // can make an old approval enter the second request's channel.
                let late = PermResponse::new(first_id, PermOutcome::Allowed);
                match first.reply {
                    PermissionReply::Sync(tx) => assert!(tx.send(late).is_err()),
                    PermissionReply::Async(tx) => assert!(tx.send(late).is_err()),
                }
                reply(second, PermOutcome::Denied(PermissionDenial::User));
                let result = (&mut checks[0]).await;
                checks.clear();
                assert_eq!(
                    result.unwrap(),
                    Err(PermissionBridgeError::Denied(PermissionDenial::User))
                );
            })
            .catch_unwind()
            .await;
            shutdown.cancel();
            drop(rx);
            for check in checks {
                let _ = check.await;
            }
            if let Err(payload) = scenario {
                std::panic::resume_unwind(payload);
            }
        }
    }

    #[tokio::test]
    async fn js_permission_bridge_out_of_order_replies_remain_correlated() {
        for synchronous in [true, false] {
            let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_secs(2));
            let first = start_check(bridge.clone(), synchronous);
            let first_envelope = next_request(&mut rx).await;
            let second = start_check(bridge, synchronous);
            let second_envelope = next_request(&mut rx).await;
            assert_ne!(first_envelope.request.id(), second_envelope.request.id());

            reply(second_envelope, PermOutcome::Allowed);
            reply(first_envelope, PermOutcome::Denied(PermissionDenial::User));

            assert_eq!(
                check_result(first).await,
                Err(PermissionBridgeError::Denied(PermissionDenial::User))
            );
            assert_eq!(check_result(second).await, Ok(()));
        }
    }

    #[tokio::test]
    async fn js_permission_bridge_rejects_crossed_response_identities() {
        for synchronous in [true, false] {
            let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_secs(2));
            let first = start_check(bridge.clone(), synchronous);
            let first_envelope = next_request(&mut rx).await;
            let first_id = first_envelope.request.id();
            let second = start_check(bridge, synchronous);
            let second_envelope = next_request(&mut rx).await;
            let second_id = second_envelope.request.id();

            second_envelope
                .reply
                .send(PermResponse::new(first_id, PermOutcome::Allowed));
            first_envelope
                .reply
                .send(PermResponse::new(second_id, PermOutcome::Allowed));

            assert_eq!(
                check_result(first).await,
                Err(PermissionBridgeError::RejectedResponse(
                    PermResponseRejection::MismatchedRequestId {
                        expected: first_id,
                        actual: second_id
                    }
                ))
            );
            assert_eq!(
                check_result(second).await,
                Err(PermissionBridgeError::RejectedResponse(
                    PermResponseRejection::MismatchedRequestId {
                        expected: second_id,
                        actual: first_id
                    }
                ))
            );
        }
    }

    #[cfg(feature = "skills")]
    fn telemetry_fixture() -> (
        crate::extras::js::skills::turn::TurnSkillBundle,
        crate::extras::js::skills::telemetry::SkillEvent,
    ) {
        use crate::extras::js::skills::telemetry::{SkillEvent, SkillEventKind};
        use crate::extras::js::skills::turn::{ResolvedSkill, TurnSkillBundle};
        use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};

        let artifact = SkillArtifact::new(
            "function run() { return 1; }".into(),
            "Parent telemetry fixture".into(),
            vec![],
            vec![SkillExport {
                name: "run".into(),
                signature: "() => number".into(),
            }],
            vec!["run() === 1".into()],
            CapabilityManifest::pure(),
        )
        .unwrap();
        let turn_id = "parent-turn".to_string();
        let tool_call_id = "parent-turn:js:0".to_string();
        let event = SkillEvent {
            invocation_id: None,
            skill_id: artifact.id.clone(),
            turn_id: turn_id.clone(),
            tool_call_id: Some(tool_call_id),
            kind: SkillEventKind::Injected,
            export_name: None,
            outcome: None,
            latency_us: None,
            retrieval_score: Some(-1.0),
            retrieval_rank: Some(99),
            query_fingerprint: Some("untrusted".into()),
            index_generation: 999,
            evidence_complete: true,
            production: false,
            argument_shape: None,
            created_at: 1,
        };
        (
            TurnSkillBundle {
                turn_id,
                query_fingerprint: "parent-query".into(),
                embedding_model_revision: "fixture".into(),
                index_generation: 7,
                skills: vec![ResolvedSkill {
                    id: artifact.id,
                    identity_version: artifact.identity_version,
                    abi_version: artifact.abi_version,
                    description: artifact.description,
                    tags: artifact.tags,
                    exports: artifact.exports,
                    tests: artifact.tests,
                    capability: artifact.capability,
                    source: artifact.source,
                    score_bits: 0.75_f32.to_bits(),
                    rank: 2,
                    route: None,
                }],
            },
            event,
        )
    }

    #[cfg(feature = "skills")]
    #[test]
    fn invalid_worker_batch_with_true_completeness_records_only_parent_loss() {
        use crate::extras::js::skills::telemetry::{SkillEventKind, TelemetryDispatcher};

        let (bundle, mut forged) = telemetry_fixture();
        forged.kind = SkillEventKind::UserPositive;
        forged.evidence_complete = true;
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let dispatcher = TelemetryDispatcher::from_sender_for_test(tx);
        let turn_context = crate::extras::js::skills::turn::SkillTurnContext::new(bundle.clone());
        assert!(!dispatch_skill_telemetry(
            Some(&dispatcher),
            &turn_context,
            &bundle,
            "parent-turn:js:0",
            &StepOutcome::Value("ok".into()),
            &[forged],
            true,
            Some(Default::default()),
            false,
        ));
        let batch = rx.try_recv().expect("parent loss event should be queued");
        assert!(
            batch
                .events()
                .iter()
                .all(|event| event.kind == SkillEventKind::ObservabilityLost
                    && !event.evidence_complete),
            "worker feedback must not reach ingestion: {:?}",
            batch.events()
        );
        assert_eq!(dispatcher.observability_lost_for_test(), 1);
    }

    #[cfg(feature = "skills")]
    #[test]
    fn valid_worker_batch_with_incomplete_claim_cannot_be_promoted_to_complete() {
        use crate::extras::js::skills::telemetry::TelemetryDispatcher;

        let (bundle, injected) = telemetry_fixture();
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let dispatcher = TelemetryDispatcher::from_sender_for_test(tx);
        let turn_context = crate::extras::js::skills::turn::SkillTurnContext::new(bundle.clone());

        assert!(dispatch_skill_telemetry(
            Some(&dispatcher),
            &turn_context,
            &bundle,
            "parent-turn:js:0",
            &StepOutcome::Value("ok".into()),
            &[injected],
            false,
            Some(Default::default()),
            false,
        ));
        let batch = rx.try_recv().expect("canonical batch should be queued");
        assert!(
            batch.events().iter().all(|event| !event.evidence_complete),
            "the worker's incomplete ceiling must apply to parent-added events"
        );
        assert!(!turn_context.evidence_complete());
    }

    #[cfg(feature = "skills")]
    #[test]
    fn saturated_and_disconnected_dispatchers_fail_closed_for_positive_evidence() {
        use crate::extras::js::skills::telemetry::TelemetryDispatcher;

        let (bundle, injected) = telemetry_fixture();
        let turn_context = crate::extras::js::skills::turn::SkillTurnContext::new(bundle.clone());
        let (saturated_tx, saturated_rx) = std::sync::mpsc::sync_channel(0);
        let saturated = TelemetryDispatcher::from_sender_for_test(saturated_tx);
        assert!(!dispatch_skill_telemetry(
            Some(&saturated),
            &turn_context,
            &bundle,
            "parent-turn:js:0",
            &StepOutcome::Value("ok".into()),
            std::slice::from_ref(&injected),
            true,
            Some(Default::default()),
            false,
        ));
        assert_eq!(saturated.observability_lost_for_test(), 1);
        drop(saturated_rx);

        let (disconnected_tx, disconnected_rx) = std::sync::mpsc::sync_channel(1);
        drop(disconnected_rx);
        let disconnected = TelemetryDispatcher::from_sender_for_test(disconnected_tx);
        assert!(!dispatch_skill_telemetry(
            Some(&disconnected),
            &turn_context,
            &bundle,
            "parent-turn:js:0",
            &StepOutcome::Value("ok".into()),
            &[injected],
            true,
            Some(Default::default()),
            false,
        ));
        assert_eq!(disconnected.observability_lost_for_test(), 1);
        assert!(
            !turn_context.evidence_complete(),
            "a rejected batch must mark the turn's evidence incomplete"
        );
    }

    #[tokio::test]
    async fn js_audit_unavailable_returns_exact_error_without_launching_a_worker() {
        use rig::tool::Tool;

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::scripted_internal_worker(0),
        ));
        let tool = JsTool::new_with_failed_audit_for_test(
            Sandbox::new(false, "bwrap"),
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor.clone(),
            AuditError::Unavailable,
        );

        let error = tool
            .call(JsArgs {
                code: "40 + 2".into(),
            })
            .await
            .expect_err("unavailable audit must fail before worker launch");

        assert_eq!(
            error.to_string(),
            "JS effect audit unavailable; retrying will not help in this session; use read, grep or shell"
        );
        assert_eq!(supervisor.generation_for_test().await, None);
        assert_eq!(supervisor.active_generation_for_test().await, None);
    }

    #[tokio::test]
    async fn js_supervisor_agent_rebuild_reuses_worker_and_stops_old_permission_receiver() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JsTool>();

        let (ask_tx, mut ask_rx) = tokio_mpsc::channel(1);
        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let audit = shared_effect_audit().expect("test effect audit");
        let tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            Some(ask_tx),
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor.clone(),
            audit.clone(),
        );
        assert_eq!(
            tool.call(JsArgs {
                code: "20 + 1".into()
            })
            .await
            .expect("first agent JS call failed"),
            "21"
        );
        let generation = supervisor
            .generation_for_test()
            .await
            .expect("first call should launch the worker");
        let process_id = supervisor
            .process_id_for_test()
            .await
            .expect("first call should retain one worker process");
        drop(tool);

        assert!(
            tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("permission receiver did not stop")
                .is_none()
        );

        let rebuilt = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::from_settings(
                &std::env::current_dir().unwrap(),
                None,
                None,
                None,
                false,
                false,
            ),
            supervisor.clone(),
            audit,
        );
        assert_eq!(
            rebuilt
                .call(JsArgs {
                    code: "20 + 2".into()
                })
                .await
                .expect("rebuilt agent JS call failed"),
            "22"
        );
        let denied_path = std::env::temp_dir().join(format!(
            "mini-agent-js-rebuild-denied-{}",
            uuid::Uuid::new_v4()
        ));
        let denied = rebuilt
            .call(JsArgs {
                code: format!(
                    "try {{ write_file({:?}, 'leaked'); 'allowed' }} catch (_) {{ 'denied' }}",
                    denied_path.to_string_lossy()
                ),
            })
            .await
            .expect("rebuilt policy check failed");
        assert_eq!(denied, "denied");
        assert!(
            !denied_path.exists(),
            "prior allow policy leaked into rebuild"
        );
        assert_eq!(supervisor.generation_for_test().await, Some(generation));
        assert_eq!(supervisor.process_id_for_test().await, Some(process_id));
    }

    #[tokio::test]
    async fn js_call_uses_single_absolute_deadline_for_all_phases() {
        // This test verifies that call() creates one absolute deadline at entry and threads it
        // through all phases (permission check, skill preparation, effect services, supervisor)
        // instead of creating independent timeouts at each phase.
        //
        // The fix is verified by confirming that:
        // 1. call_deadline is created once at the start
        // 2. The same deadline is used for prepare_skill_manifests (via passed deadline)
        // 3. The same deadline is used for grant_expires_at
        // 4. Effect services get the remaining time from call_deadline
        // 5. execute_bound_with_deadline receives the call_deadline
        //
        // Without this fix, each phase would get its own independent STEP_TIMEOUT,
        // potentially exceeding 30 seconds total.

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let audit = shared_effect_audit().expect("test effect audit");
        let tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor.clone(),
            audit,
        );

        // A simple call should succeed within the single 30-second deadline
        let result = tool
            .call(JsArgs {
                code: "40 + 2".into(),
            })
            .await
            .expect("call should succeed within single deadline");
        assert_eq!(result, "42");
    }

    #[tokio::test]
    async fn read_only_tool_profile_reaches_the_worker_and_omits_all_other_globals() {
        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let mut tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor,
            shared_effect_audit().expect("test effect audit"),
        );
        tool.profile = ModelEffectProfile::ReadOnly;

        let observed = tool
            .call(JsArgs {
                code: "[typeof read_file, typeof list_dir, typeof grep, \
                       typeof read_files, typeof glob, typeof write_file, typeof fetch, \
                       typeof spawn, typeof result, typeof scratch_put, typeof scratch_get] \
                       .join(',')"
                    .into(),
            })
            .await
            .expect("read-only JS call");
        assert_eq!(
            observed,
            "function,function,function,undefined,undefined,undefined,undefined,undefined,undefined,undefined,undefined"
        );
        assert_eq!(
            tool.call(JsArgs {
                code: "read_file('Cargo.toml').includes('[package]')".into(),
            })
            .await
            .expect("brokered read in read-only JS"),
            "true"
        );
    }

    #[cfg(feature = "subagents")]
    #[tokio::test]
    async fn read_only_constructor_narrows_parent_authority_even_with_process_tree_ownership() {
        let tool = JsTool::new_read_only(
            Sandbox::new(false, "bwrap").with_complete_process_tree_for_test(),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        );

        assert_eq!(tool.profile, ModelEffectProfile::ReadOnly);
        assert_eq!(
            tool.model_capabilities(),
            std::collections::BTreeSet::from([HostCapability::ReadFile])
        );
        assert!(!tool.spawn_available());
    }

    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn read_only_tool_rejects_a_non_pure_bundle_before_worker_launch() {
        use crate::extras::js::skills::turn::{ResolvedSkill, SkillTurnContext, TurnSkillBundle};
        use crate::extras::js::skills::{CapabilityManifest, CapabilityScope, CapabilityTier};

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let context = Arc::new(SkillTurnContext::new(TurnSkillBundle {
            turn_id: "read-only-child".to_string(),
            query_fingerprint: "fixture".to_string(),
            embedding_model_revision: "fixture".to_string(),
            index_generation: 1,
            skills: vec![ResolvedSkill {
                id: "effectful-fixture".to_string(),
                identity_version: 2,
                abi_version: 1,
                description: "must not enter a read-only child".to_string(),
                tags: Vec::new(),
                exports: Vec::new(),
                tests: Vec::new(),
                capability: CapabilityManifest::new(
                    CapabilityTier::ReadOnly,
                    vec![CapabilityScope::ReadFile {
                        workspace_prefixes: vec!["src".to_string()],
                    }],
                )
                .unwrap(),
                source: "".to_string(),
                score_bits: 1.0_f32.to_bits(),
                rank: 1,
                route: None,
            }],
        }));
        let mut tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor.clone(),
            shared_effect_audit().expect("test effect audit"),
        )
        .with_skill_turn_context(context);
        tool.profile = ModelEffectProfile::ReadOnly;

        let error = tool
            .call(JsArgs { code: "1".into() })
            .await
            .expect_err("non-pure child bundle must fail closed");
        assert_eq!(
            error.to_string(),
            "read-only JS skill bundle contains a non-pure capability"
        );
        assert_eq!(supervisor.generation_for_test().await, None);
    }

    // mini-agent-pff43: a read-only child has no telemetry dispatcher by design, so calling a
    // pure skill there must not log `observability_lost` or mark its evidence lost; a parent
    // (non-read-only) tool without a dispatcher still fails closed.
    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn read_only_child_pure_skill_call_is_not_an_observability_loss() {
        use crate::extras::js::skills::turn::SkillTurnContext;
        use rig::tool::Tool;

        for (profile, expect_complete) in [
            (ModelEffectProfile::ReadOnly, true),
            (ModelEffectProfile::Full, false),
        ] {
            let (bundle, _) = telemetry_fixture();
            let context = Arc::new(SkillTurnContext::new(bundle));
            let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
                crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
            ));
            let mut tool = JsTool::new_with_runtime_for_test(
                Sandbox::new(false, "bwrap"),
                None,
                None,
                AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
                supervisor,
                shared_effect_audit().expect("test effect audit"),
            )
            .with_skill_turn_context(context.clone());
            tool.profile = profile;
            assert!(tool.telemetry.is_none());

            let out = tool
                .call(JsArgs {
                    code: "run()".into(),
                })
                .await
                .expect("the pure skill call runs");
            assert_eq!(out, "1", "{profile:?}");
            assert_eq!(context.evidence_complete(), expect_complete, "{profile:?}");
        }
    }

    #[tokio::test]
    async fn structured_result_and_scratch_survive_a_fresh_runtime_and_agent_rebuild() {
        use rig::tool::Tool;

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let audit = shared_effect_audit().expect("test effect audit");
        let scratch = ScratchStore::default();
        let first = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor.clone(),
            audit.clone(),
        )
        .with_scratch_store(scratch.clone());

        assert_eq!(
            first
                .call(JsArgs {
                    code: "scratch_get('missing') === null".into(),
                })
                .await
                .unwrap(),
            "true"
        );
        assert_eq!(
            first
                .call(JsArgs {
                    code: "scratch_put('turn:1', {items: [1, 2, 3], ok: true}); 'stored'".into(),
                })
                .await
                .unwrap(),
            "stored"
        );
        drop(first);

        let rebuilt = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor,
            audit,
        )
        .with_scratch_store(scratch);
        let started = Instant::now();
        assert_eq!(
            rebuilt
                .call(JsArgs {
                    code:
                        "result({kind: 'summary', value: scratch_get('turn:1')}); while (true) {}"
                            .into(),
                })
                .await
                .unwrap(),
            r#"{"kind":"summary","value":{"items":[1,2,3],"ok":true}}"#
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "terminal result must interrupt later JavaScript instead of waiting for the deadline"
        );
    }

    #[tokio::test]
    async fn structured_result_rejects_accessors_before_dispatch() {
        use rig::tool::Tool;

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor,
            shared_effect_audit().expect("test effect audit"),
        );
        let rendered = tool
            .call(JsArgs {
                code: "result(Object.defineProperty({}, 'secret', {get() { throw 'leak'; }}))"
                    .into(),
            })
            .await
            .unwrap();
        assert!(rendered.starts_with("JS exception"), "{rendered}");
        assert!(!rendered.contains("leak"), "{rendered}");
    }

    #[tokio::test]
    async fn structured_result_expands_the_legacy_limit_but_keeps_its_own_cap() {
        use rig::tool::Tool;

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor,
            shared_effect_audit().expect("test effect audit"),
        );

        let larger_than_legacy = "x".repeat(MAX_RESULT_BYTES + 1024);
        let literal = serde_json::to_string(&larger_than_legacy).unwrap();
        let accepted = tool
            .call(JsArgs {
                code: format!("result({literal})"),
            })
            .await
            .unwrap();
        assert_eq!(accepted, literal);

        let oversized = "x".repeat(STRUCTURED_RESULT_MAX_BYTES + 1);
        let literal = serde_json::to_string(&oversized).unwrap();
        let rejected = tool
            .call(JsArgs {
                code: format!("try {{ result({literal}); }} catch (error) {{ error.code }}"),
            })
            .await
            .unwrap();
        assert_eq!(rejected, "too_large");
    }

    fn private_effect_audit_for_test(tag: &str) -> SharedEffectAudit {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-js-tool-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let paths = crate::paths::AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: None,
        };
        Arc::new(std::sync::Mutex::new(
            EffectAudit::open(paths.effect_audit()).expect("private test effect audit"),
        ))
    }

    #[tokio::test]
    async fn effects_after_an_accepted_result_are_denied_locally_and_keep_the_result() {
        use rig::tool::Tool;

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let audit = private_effect_audit_for_test("post-result");
        let tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor,
            audit.clone(),
        );
        // More post-result effects than the per-step quota: without local denial the 257th
        // overwrote the accepted outcome with an effect-limit error and the parent reported a
        // protocol violation.
        let rendered = tool
            .call(JsArgs {
                code: "result({a:1}); \
                       for (let i = 0; i < 300; i++) { try { scratch_get('k') } catch (e) {} } \
                       'done'"
                    .into(),
            })
            .await
            .expect("post-result effects must not become a worker protocol violation");
        assert_eq!(rendered, r#"{"a":1}"#);

        let audit = audit.lock().unwrap();
        let records = audit.records();
        let result_completion = records
            .iter()
            .rposition(|record| {
                record.capability == "session_state"
                    && record.state == crate::extras::js::audit::AuditState::Completed
                    && record.decision == "authorized"
            })
            .expect("the accepted result must be audited");
        let after_result = records.len() - result_completion - 1;
        assert!(
            after_result <= 2,
            "post-result effects must not reach the parent audit: {after_result} records after the result"
        );
    }

    #[tokio::test]
    async fn terminal_audit_flush_failure_is_the_closed_audit_error() {
        use crate::extras::js::audit::{
            AuditCapability, AuditDecision, AuditFailurePoint, AuditResultCode, EffectCompletion,
            EffectIntent,
        };

        let audit = private_effect_audit_for_test("terminal-flush");
        {
            let mut guard = audit.lock().unwrap();
            let target = guard.file_target("safe/path");
            guard
                .append_intent(EffectIntent {
                    effect_id: "effect-terminal-flush".into(),
                    invocation_id: "inv-terminal-flush".into(),
                    grant_id: "grant-terminal-flush".into(),
                    sequence: 1,
                    timestamp_ms: 1_800_000_000_000,
                    artifact_id: None,
                    export: None,
                    capability: AuditCapability::ReadFile,
                    normalized_target: target,
                    decision: AuditDecision::Authorized,
                })
                .unwrap();
            guard
                .append_completion(EffectCompletion {
                    effect_id: "effect-terminal-flush".into(),
                    result_code: AuditResultCode::Succeeded,
                })
                .unwrap();
            guard.fail_next_durability_for_test(AuditFailurePoint::FileSync);
        }
        let error = flush_pending_audit(audit.clone())
            .await
            .expect_err("a failed terminal sync must replace the step result");
        assert_eq!(
            error.to_string(),
            audit_unavailable_message(AuditError::SyncFailed)
        );
    }

    #[test]
    fn audit_failures_map_to_distinct_source_free_classes() {
        let all = [
            AuditError::PathUnavailable,
            AuditError::WriterLocked,
            AuditError::SyncFailed,
            AuditError::CorruptRecord,
            AuditError::RetentionLimit,
            AuditError::KeyUnavailable,
            AuditError::Unavailable,
        ];
        let messages: Vec<_> = all.into_iter().map(audit_unavailable_message).collect();
        let distinct: std::collections::HashSet<_> = messages.iter().collect();
        assert_eq!(distinct.len(), messages.len());
        for message in &messages {
            assert!(message.starts_with("JS effect audit unavailable"));
            assert!(!message.contains('/'));
        }
        for same in [
            AuditError::HashMismatch,
            AuditError::MissingSegment,
            AuditError::InvalidMetadata,
        ] {
            assert_eq!(
                audit_unavailable_message(same),
                audit_unavailable_message(AuditError::CorruptRecord)
            );
        }
    }

    #[tokio::test]
    async fn structured_result_keeps_the_warm_worker() {
        use rig::tool::Tool;

        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            supervisor.clone(),
            shared_effect_audit().expect("test effect audit"),
        );
        assert_eq!(
            tool.call(JsArgs {
                code: "result(1)".into()
            })
            .await
            .unwrap(),
            "1"
        );
        let generation = supervisor
            .generation_for_test()
            .await
            .expect("an accepted structured result must keep the worker");
        assert_eq!(tool.call(JsArgs { code: "1".into() }).await.unwrap(), "1");
        assert_eq!(supervisor.generation_for_test().await, Some(generation));
    }

    fn helper_tool(profile: ModelEffectProfile, root: &std::path::Path) -> JsTool {
        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let mut tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(root),
            supervisor,
            shared_effect_audit().expect("test effect audit"),
        );
        tool.profile = profile;
        tool
    }

    async fn run_js(tool: &JsTool, code: &str) -> String {
        tool.call(JsArgs { code: code.into() })
            .await
            .unwrap_or_else(|error| format!("TOOL ERROR: {error}"))
    }

    // Data-oriented pack segments (path/tally/stats, semver, yaml); shares the helpers above.
    mod pack_data_tests;

    fn scratch_dir() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mini-agent-read-lines-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every helper of `pack_helpers.js`; each must be listed by `help()` and installed.
    const PACK_HELPER_NAMES: [&str; 4] = ["date_parse", "date_fmt", "glob_match", "glob_filter"];

    #[tokio::test]
    async fn description_mentions_every_helper_global_and_each_is_installed() {
        let root = std::env::current_dir().unwrap();
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = helper_tool(profile, &root);
            let description = tool.description();
            for name in [
                "splitLines",
                "json_exact",
                "read_lines",
                "toml_parse",
                "csv_parse",
            ] {
                assert!(
                    description.contains(&format!("`{name}")),
                    "{profile:?} description must document {name}"
                );
                assert_eq!(
                    run_js(&tool, &format!("typeof {name}")).await,
                    "function",
                    "{profile:?} must install {name}"
                );
            }
            assert!(description.contains("Idioms:"));
            // Pack helpers stay out of the always-sent description: they are asserted through
            // the `help()` catalogue and `typeof` (which triggers the lazy load).
            assert!(description.contains("`help()`"), "{profile:?}");
            assert_eq!(
                run_js(&tool, "typeof help").await,
                "function",
                "{profile:?}"
            );
            let catalogue = run_js(&tool, "help()").await;
            for name in PACK_HELPER_NAMES {
                assert!(
                    catalogue.contains(&format!("{name}(")),
                    "{profile:?} help() must list {name}: {catalogue}"
                );
                assert!(
                    !description.contains(&format!("`{name}")),
                    "{profile:?} description must not carry pack helper {name}"
                );
                assert_eq!(
                    run_js(&tool, &format!("typeof {name}")).await,
                    "function",
                    "{profile:?} must install {name}"
                );
                assert!(
                    run_js(&tool, &format!("help('{name}')"))
                        .await
                        .contains(name)
                );
            }
        }
        // The description is paid on every request: it grew by about 1.3 KB over the 3384-byte
        // baseline plus 227 bytes for toml_parse/csv_parse (4831 measured); it must stay modest.
        // The bound later absorbed other work (4956 bytes before the help() sentence) and rises
        // by exactly the 43 bytes of " Other pure helpers are listed by `help()`." (4999 measured).
        let full = helper_tool(ModelEffectProfile::Full, &root)
            .description()
            .len();
        assert!(
            full < 3384 + 1350 + 230 + 43,
            "full description is {full} bytes"
        );
    }

    #[tokio::test]
    async fn split_lines_has_no_phantom_trailing_element_and_handles_crlf() {
        let root = std::env::current_dir().unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &root);
        let code = r"JSON.stringify([
            splitLines('a\nb\n'), splitLines('a\nb'), splitLines('a\r\nb\r\n'), splitLines(''), splitLines('\n'),
            splitLines('a\n\nb\n'), splitLines('x\n'.repeat(4218)).length,
        ])";
        assert_eq!(
            run_js(&tool, code).await,
            r#"[["a","b"],["a","b"],["a","b"],[],[""],["a","","b"],4218]"#
        );
        let bad = run_js(&tool, "splitLines(1)").await;
        assert!(bad.contains("JS"), "{bad}");
    }

    #[tokio::test]
    async fn a_script_may_name_its_variable_lines_without_a_collision() {
        let root = std::env::current_dir().unwrap();
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = helper_tool(profile, &root);
            let ok = run_js(
                &tool,
                "const text = 'a\\nb\\n'; const lines = splitLines(text); lines.length + ':' + lines[1]",
            )
            .await;
            assert_eq!(ok, "2:b", "{profile:?}");
            // The old global is gone: nothing named `lines` is predefined, so the natural
            // variable name can never hit a temporal-dead-zone ReferenceError.
            assert_eq!(
                run_js(&tool, "typeof lines").await,
                "undefined",
                "{profile:?}"
            );
            assert!(!tool.description().contains("`lines("), "{profile:?}");
        }
    }

    #[tokio::test]
    async fn read_limit_errors_state_the_limit_and_the_remedy() {
        let root = std::env::current_dir().unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &root);
        let count = run_js(
            &tool,
            "try { read_lines('Cargo.toml', 1, 20000) } catch (e) { e.message + '|' + e.code }",
        )
        .await;
        assert_eq!(
            count,
            "read_lines: too_large (count max 10000, scan max 64 MiB per call; read a smaller from/count range or use grep())|too_large"
        );
        assert!(count.chars().count() <= 300, "{count}");
        let scratch = scratch_dir();
        let big = scratch.join("big.txt");
        std::fs::write(&big, vec![b'x'; 1024 * 1024 + 1]).unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &scratch);
        let file = run_js(
            &tool,
            &format!(
                "try {{ read_file('{}') }} catch (e) {{ e.message }}",
                big.display()
            ),
        )
        .await;
        assert_eq!(
            file,
            "read_file: too_large (limit 1 MiB; use read_lines(path, from, count) or grep() for large files)"
        );
    }

    #[tokio::test]
    async fn wrong_collection_type_errors_get_a_property_hint() {
        let root = std::env::current_dir().unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &root);
        for code in [
            "for (const f of list_dir('.')) {}",
            "read_files(glob('*.toml'))",
            "const [a] = grep('x', {path: '.'});",
        ] {
            let out = run_js(&tool, code).await;
            assert!(out.starts_with("JS TypeError"), "{code}: {out}");
            assert!(
                out.contains("glob() returns {paths}, list_dir() returns {entries}, grep() returns {matches}: use the array property"),
                "{code}: {out}"
            );
            assert!(
                out.chars().count() <= 330,
                "{code}: {}",
                out.chars().count()
            );
        }
    }

    #[test]
    fn collection_hint_only_applies_to_matching_type_errors() {
        use JsExceptionClass::{RangeError, TypeError};
        let hinted = |class, text: &str| JsDiagnosticHint::from_exception(class, text).is_some();
        assert!(hinted(TypeError, "value is not iterable"));
        assert!(hinted(
            TypeError,
            "Error converting from js object into type 'array'"
        ));
        assert!(!hinted(TypeError, "x is not a function"));
        assert!(!hinted(RangeError, "value is not iterable"));
        let long = format!("{} is not iterable", "y".repeat(500));
        let out = JsDiagnosticHint::from_exception(TypeError, &long)
            .unwrap()
            .text();
        assert!(!out.contains(&"y".repeat(500)), "{out}");
    }

    #[tokio::test]
    async fn json_exact_keeps_integers_beyond_2_pow_53() {
        let root = std::env::current_dir().unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &root);
        // Plain JSON.parse rounds 2^53+1; json_exact does not.
        let code = r#"
            const text = '{"a": 9007199254740993, "b": [12345678901234567890, -9007199254740993], "c": 1.5e300, "d": "9007199254740993", "e": 7, "f": 9007199254740991}';
            const v = json_exact(text);
            JSON.stringify([String(v.a), String(v.b[0]), String(v.b[1]), String(v.a + v.b[0]),
                typeof v.a, v.c, v.d, v.e, v.f, typeof v.e, String(JSON.parse(text).a)])"#;
        assert_eq!(
            run_js(&tool, code).await,
            r#"["9007199254740993","12345678901234567890","-9007199254740993","12354686100489308883","bigint",1.5e+300,"9007199254740993",7,9007199254740991,"number","9007199254740992"]"#
        );
        let malformed = run_js(&tool, "json_exact('{\"a\": 1,}')").await;
        assert!(malformed.contains("JS"), "{malformed}");
    }

    #[tokio::test]
    async fn date_helpers_are_utc_only_exact_and_bounded() {
        let root = std::env::current_dir().unwrap();
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = helper_tool(profile, &root);
            let code = r##"
                const p = (...a) => date_parse(...a);
                JSON.stringify([
                  p('2025-10-10'), p('2025-10-10T13:55:36Z'), p('2025-10-10T13:55:36.123456+05:30'),
                  p('2025-10-10 13:55'), p('2024-02-29'), p('0005-01-01'), p('2025-10-10T01:00:00-0130'),
                  p('2025-02-30'), p('2023-02-29'), p('2025-13-01'), p('2025-10-10T24:00:00Z'),
                  p('2025-10-10T13:55:36+25:00'), p('2025-04-31'), p('25-10-10'),
                  p('Fri, 10 Oct 2025 13:55:36 +0200'), p('Thu, 10 Oct 2025 13:55:36 +0200'),
                  p('10 Oct 2025 13:55 GMT'), p('Fri, 31 Feb 2025 13:55:36 +0000'),
                  p('10/Oct/2025:13:55:36 +0200'), p('[10/Oct/2025:13:55:36 -0700]'),
                  p('31/Feb/2025:10:00:00 +0000'), p('10/Foo/2025:10:00:00 +0000'),
                  p('10/31/2025'), p('02/30/2025'), p('12/25/2025 1:05 PM'), p('12/25/2025 12:05 AM'),
                  p('13/01/2025'), p('12/25/2025 13:05 PM'),
                  p('1760097336'), p(1760097336), p(1760097336000), p('1760097336.5'), p('5', 'epoch_ms'),
                  p('5', 'epoch_s'), p(5, 'epoch_ms'),
                  p('hello'), p(''), p('   '), p('2025-10-10', 'us'), p('10/31/2025', 'iso'),
                  p('2025-10-10T13:55:36Z', 'iso'), p('x'.repeat(1000000)), p('1'.repeat(1000000)),
                  p(NaN), p(Infinity),
                ])"##;
            let out = run_js(&tool, code).await;
            let expected = serde_json::json!([
                "2025-10-10T00:00:00.000Z",
                "2025-10-10T13:55:36.000Z",
                "2025-10-10T08:25:36.123Z",
                "2025-10-10T13:55:00.000Z",
                "2024-02-29T00:00:00.000Z",
                "0005-01-01T00:00:00.000Z",
                "2025-10-10T02:30:00.000Z",
                null,
                null,
                null,
                null,
                null,
                null,
                null,
                "2025-10-10T11:55:36.000Z",
                null,
                "2025-10-10T13:55:00.000Z",
                null,
                "2025-10-10T11:55:36.000Z",
                "2025-10-10T20:55:36.000Z",
                null,
                null,
                "2025-10-31T00:00:00.000Z",
                null,
                "2025-12-25T13:05:00.000Z",
                "2025-12-25T00:05:00.000Z",
                null,
                null,
                "2025-10-10T11:55:36.000Z",
                "2025-10-10T11:55:36.000Z",
                "2025-10-10T11:55:36.000Z",
                "2025-10-10T11:55:36.500Z",
                "1970-01-01T00:00:00.005Z",
                "1970-01-01T00:00:05.000Z",
                "1970-01-01T00:00:00.005Z",
                null,
                null,
                null,
                null,
                null,
                "2025-10-10T13:55:36.000Z",
                null,
                null,
                null,
                null,
            ]);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&out)
                    .unwrap_or_else(|_| out.clone().into()),
                expected,
                "{profile:?}: {out}"
            );

            let fmt = r##"
                const t = Date.UTC(2025, 9, 10, 13, 55, 36, 7);
                JSON.stringify([
                  date_fmt(t), date_fmt(t, 'iso'), date_fmt(t, 'date'), date_fmt(t, 'us'),
                  date_fmt(t, 'rfc2822'), date_fmt(t, 'nginx'),
                  date_fmt(t, 'YYYY-MM-DD HH:mm:ss.SSS'), date_fmt(t, 'epoch_s'), date_fmt(t, 'epoch_ms'),
                  date_fmt(Date.UTC(2025, 9, 10), 'YYYY-MM-DD HH:mm'), date_fmt(0, 'constructor'),
                  // Round trips through every text format.
                  ['iso', 'rfc2822', 'nginx'].map(f => date_parse(date_fmt(t, f)) === date_fmt(Math.floor(t / 1000) * 1000, 'iso')
                    || date_parse(date_fmt(t, f)) === date_fmt(t, 'iso')),
                  date_parse(date_fmt(t, 'us')),
                  (() => { try { date_fmt(NaN) } catch (e) { return e.name } })(),
                  (() => { try { date_parse('2025-10-10', 'weird') } catch (e) { return e.name } })(),
                  (() => { try { date_parse({}) } catch (e) { return e.name } })(),
                ])"##;
            let out = run_js(&tool, fmt).await;
            let expected = serde_json::json!([
                "2025-10-10T13:55:36.007Z",
                "2025-10-10T13:55:36.007Z",
                "2025-10-10",
                "10/10/2025",
                "Fri, 10 Oct 2025 13:55:36 +0000",
                "10/Oct/2025:13:55:36 +0000",
                "2025-10-10 13:55:36.007",
                1760104536u64,
                1760104536007u64,
                "2025-10-10 00:00",
                "constructor",
                [true, true, true],
                "2025-10-10T00:00:00.000Z",
                "RangeError",
                "RangeError",
                "TypeError",
            ]);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&out)
                    .unwrap_or_else(|_| out.clone().into()),
                expected,
                "{profile:?}: {out}"
            );
        }
    }

    #[tokio::test]
    async fn date_parse_matches_date_utc_arithmetic_for_every_day_of_a_leap_cycle() {
        let root = std::env::current_dir().unwrap();
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = helper_tool(profile, &root);
            // Every calendar day of 2023-2028 in four formats equals the Date.UTC instant, and
            // the day after each month end is rejected; nothing depends on the host timezone.
            let code = r##"
                const months = ['Jan','Feb','Mar','Apr','May','Jun','Jul','Aug','Sep','Oct','Nov','Dec'];
                let bad = 0, n = 0;
                for (let ms = Date.UTC(2023, 0, 1); ms < Date.UTC(2029, 0, 1); ms += 86400000) {
                  const d = new Date(ms), y = d.getUTCFullYear(), m = d.getUTCMonth() + 1, day = d.getUTCDate();
                  const z = (v, w = 2) => String(v).padStart(w, '0');
                  const iso = new Date(ms).toISOString();
                  for (const s of [`${y}-${z(m)}-${z(day)}`, `${z(day)}/${months[m - 1]}/${y}:00:00:00 +0000`,
                                   `${z(m)}/${z(day)}/${y}`, String(ms / 1000), String(ms)]) {
                    n++;
                    if (date_parse(s) !== iso) bad++;
                  }
                  const dim = new Date(Date.UTC(y, m, 0)).getUTCDate();
                  if (day === dim && date_parse(`${y}-${z(m)}-${z(dim + 1)}`) !== null) bad++;
                }
                bad + ':' + n"##;
            assert_eq!(run_js(&tool, code).await, "0:10960", "{profile:?}");
        }
    }

    #[tokio::test]
    async fn date_parse_is_bounded_on_adversarial_input() {
        let root = std::env::current_dir().unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &root);
        let started = std::time::Instant::now();
        let code = r##"
            const hostile = ['0'.repeat(99) + 'x', '1-'.repeat(50), '/'.repeat(99), ' '.repeat(5000) + '1',
                             '2025-10-10T' + '9'.repeat(5000), 'Fri, '.repeat(20), '1'.repeat(5000000)];
            hostile.map(s => date_parse(s)).join(',')"##;
        assert_eq!(run_js(&tool, code).await, ",,,,,,");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn glob_match_and_filter_follow_star_globstar_class_and_brace_rules() {
        let root = std::env::current_dir().unwrap();
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = helper_tool(profile, &root);
            let code = r##"
                const cases = [
                  ['*.rs', 'a.rs', true], ['*.rs', 'd/a.rs', false], ['*', '', true], ['*', 'a/b', false],
                  ['**/*.rs', 'a.rs', true], ['**/*.rs', 'd/e/a.rs', true], ['**/*.rs', 'a.rsx', false],
                  ['src/**/*.rs', 'src/a.rs', true], ['src/**/*.rs', 'src/x/y/a.rs', true],
                  ['src/**/*.rs', 'srcx/a.rs', false], ['src/**/*.rs', 'lib/src/a.rs', false],
                  ['src/**', 'src/a/b', true], ['src/**', 'src', false], ['**', '', true], ['**', 'a/b', true],
                  ['**/x', 'x', true], ['**/x', 'a/b/x', true], ['**/x', 'a/bx', false],
                  ['a**b', 'axb', true], ['a**b', 'a/b', false], ['a/**/**/b', 'a/b', true],
                  ['a?c', 'abc', true], ['a?c', 'a/c', false], ['a?c', 'ac', false],
                  ['[a-c]x', 'bx', true], ['[a-c]x', 'dx', false], ['[!a-c]x', 'bx', false],
                  ['[!a-c]x', 'dx', true], ['[^a]x', 'bx', true], ['[!a]x', '/x', false],
                  ['[]a]x', ']x', true], ['[a\\]]x', ']x', true],
                  ['*.{js,ts}', 'a.ts', true], ['*.{js,ts}', 'a.rs', false],
                  ['{a,b{c,d}}e', 'bde', true], ['{a,b{c,d}}e', 'bee', false], ['x{,y}', 'x', true],
                  ['{src,lib}/**/*.rs', 'lib/a/b.rs', true], ['[{]', '{', true],
                  ['\\*', '*', true], ['\\*', 'a', false], ['a.b', 'axb', false], ['A', 'a', false],
                  ['.*', '.gitignore', true], ['*', '.gitignore', true], ['./a', 'a', false],
                  ['(a)+', '(a)+', true], ['a+b', 'aab', false], ['^a$', 'a', false],
                ];
                const bad = cases.filter(([p, s, e]) => glob_match(p, s) !== e).map(([p, s, e]) => p + ' ' + s + ' ' + e);
                const throws = (f) => { try { f(); return 'no' } catch (e) { return e.name } };
                JSON.stringify([bad,
                  glob_filter('**/*.md', ['a.md', 'b/c.md', 'd.txt']),
                  glob_filter(['docs/**', '!docs/a/**'], ['docs/b.md', 'docs/a/b.md', 'x']),
                  glob_filter(['**/*.md', '!docs/**', 'docs/keep.md'], ['a.md', 'docs/x.md', 'docs/keep.md']),
                  glob_filter([], ['a']), glob_filter('*', []),
                  throws(() => glob_match('[abc', 'a')), throws(() => glob_match('{a,b', 'a')),
                  throws(() => glob_match('[z-a]', 'a')), throws(() => glob_match('a'.repeat(1025), 'a')),
                  throws(() => glob_match('a', 'a'.repeat(4097))), throws(() => glob_match(1, 'a')),
                  throws(() => glob_match('a', null)), throws(() => glob_filter(1, [])),
                  throws(() => glob_filter('a', 'a')), throws(() => glob_match('{a,b}'.repeat(8), 'a')),
                  glob_match('{a,b}'.repeat(7), 'abababa'),
                ])"##;
            let out = run_js(&tool, code).await;
            let expected = serde_json::json!([
                [],
                ["a.md", "b/c.md"],
                ["docs/b.md"],
                ["a.md", "docs/keep.md"],
                [],
                [],
                "SyntaxError",
                "SyntaxError",
                "SyntaxError",
                "RangeError",
                "RangeError",
                "TypeError",
                "TypeError",
                "TypeError",
                "TypeError",
                "RangeError",
                true,
            ]);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&out)
                    .unwrap_or_else(|_| out.clone().into()),
                expected,
                "{profile:?}: {out}"
            );
        }
    }

    #[tokio::test]
    async fn glob_match_agrees_with_a_reference_regex_on_small_exhaustive_inputs() {
        let root = std::env::current_dir().unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &root);
        // Reference: the naive glob-to-regex translation (fine here: tiny inputs).
        let code = r##"
            const atoms = ['a', 'b', '/', '*', '?', '[ab]', '[!a]'];
            const re = { 'a': 'a', 'b': 'b', '/': '\\/', '*': '[^/]*', '?': '[^/]', '[ab]': '[ab]', '[!a]': '[^a/]' };
            const patterns = [''];
            let layer = [''];
            for (let k = 0; k < 4; k++) {
              const next = [];
              for (const p of layer) for (const a of atoms) next.push(p + a);
              patterns.push(...next); layer = next;
            }
            const paths = [''];
            let pl = [''];
            for (let k = 0; k < 4; k++) {
              const next = [];
              for (const p of pl) for (const a of ['a', 'b', '/']) next.push(p + a);
              paths.push(...next); pl = next;
            }
            let diff = 0, checks = 0, first = '';
            for (const p of patterns) {
              if (p.includes('**')) continue;
              const r = new RegExp('^' + p.split(/(\[!a\]|\[ab\]|.)/).filter(Boolean).map(t => re[t]).join('') + '$');
              for (const s of paths) {
                checks++;
                if (glob_match(p, s) !== r.test(s)) { diff++; if (!first) first = p + ' vs ' + s; }
              }
            }
            diff + ':' + checks + ':' + first"##;
        let out = run_js(&tool, code).await;
        assert!(out.starts_with("0:"), "{out}");
    }

    #[tokio::test]
    async fn glob_match_is_bounded_on_pathological_patterns_and_paths() {
        let root = std::env::current_dir().unwrap();
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = helper_tool(profile, &root);
            let started = std::time::Instant::now();
            let code = r##"
                const out = [
                  glob_match('a*'.repeat(25), 'a'.repeat(10) + 'a'.repeat(3990) + 'b'),
                  glob_match('a*'.repeat(500) + 'b', 'a'.repeat(4000)),
                  glob_match('*a'.repeat(400) + 'c', 'ab'.repeat(2000)),
                  glob_match('**/'.repeat(100) + 'x', 'a/'.repeat(2000)),
                  glob_match('{a,b}*'.repeat(4) + 'c', 'ab'.repeat(2000)),
                  glob_match('[ab]*'.repeat(150) + 'c', 'ab'.repeat(2000)),
                  glob_match('?'.repeat(1000), 'a'.repeat(1000)),
                  glob_match('*', 'x'.repeat(4096)),
                ];
                out.join(',')"##;
            assert_eq!(
                run_js(&tool, code).await,
                "true,false,false,false,false,false,true,true",
                "{profile:?}"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "{profile:?} {:?}",
                started.elapsed()
            );
        }
    }

    #[tokio::test]
    async fn toml_parse_counts_dependencies_inline_tables_and_arrays_of_tables() {
        let root = std::env::current_dir().unwrap();
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = helper_tool(profile, &root);
            let code = r##"
                const t = toml_parse(`# manifest
name = "demo # not a comment" # real comment
big = 12345678901234567890
list = [1, 2, # c
  3,
]
ml = """
a \\
   b"""
[package]
version = "1"
[dependencies]
serde = { version = "1", features = ["derive"] }
tokio = { version = "1" }
rand = "0.8"
'quoted.key' = 1
[[bin]]
name = "a"
[[bin]]
name = "b"
[bin.extra]
k = true
`);
                JSON.stringify([t.name, String(t.big), typeof t.big, t.list, t.ml,
                    Object.keys(t.dependencies).length,
                    Object.values(t.dependencies).filter(v => typeof v === 'object').length,
                    t.dependencies['quoted.key'], t.bin.map(b => b.name), t.bin[1].extra.k])"##;
            assert_eq!(
                run_js(&tool, code).await,
                r##"["demo # not a comment","12345678901234567890","bigint",[1,2,3],"a b",4,2,1,["a","b"],true]"##,
                "{profile:?}"
            );
            let bad = run_js(
                &tool,
                "try { toml_parse('a = 1\\na = 2') } catch (error) { error.message }",
            )
            .await;
            assert!(bad.contains("duplicate key"), "{bad}");
            let huge = run_js(
                &tool,
                "try { toml_parse('#'.repeat(5 * 1024 * 1024)) } catch (error) { error.message }",
            )
            .await;
            assert!(huge.contains("exceeds"), "{huge}");
        }
    }

    #[tokio::test]
    async fn csv_parse_handles_quotes_crlf_and_headers() {
        let root = std::env::current_dir().unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &root);
        let code = r#"JSON.stringify([
            csv_parse('id,note\r\n1,"a, ""b"""\r\n2,"x\ny"\r\n'),
            csv_parse('id,note\n1,a\n2', {header: true}),
            csv_parse(''), csv_parse('a,\n'),
        ])"#;
        assert_eq!(
            run_js(&tool, code).await,
            r#"[[["id","note"],["1","a, \"b\""],["2","x\ny"]],[{"id":"1","note":"a"},{"id":"2","note":""}],[],[["a",""]]]"#
        );
        let bad = run_js(
            &tool,
            "try { csv_parse('\"abc') } catch (error) { error.message }",
        )
        .await;
        assert!(bad.contains("unterminated"), "{bad}");
    }

    #[tokio::test]
    async fn grep_accepts_a_single_file_path_and_truncates_deterministically() {
        let dir = scratch_dir();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let text: String = (1..=20_000)
            .map(|n| format!("ERROR line {n:06} {}\n", "x".repeat(40)))
            .collect();
        std::fs::write(dir.join("logs/app.log"), &text).unwrap();
        std::fs::write(dir.join("logs/other.log"), "ERROR elsewhere\n").unwrap();
        std::fs::write(dir.join("small.txt"), "alpha\nERROR beta\n").unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &dir);

        let relative = run_js(
            &tool,
            "const r = grep('ERROR beta', {path: 'small.txt'}); JSON.stringify([r.matches.map(m => m.path + ':' + m.line), r.truncated])",
        )
        .await;
        assert_eq!(relative, r#"[["small.txt:2"],false]"#);

        let absolute = dir.join("small.txt").to_string_lossy().replace('\\', "/");
        let by_absolute = run_js(
            &tool,
            &format!("grep('ERROR', {{path: '{absolute}'}}).matches.length"),
        )
        .await;
        assert_eq!(by_absolute, "1");

        // Only the named file is searched, never its siblings, and a miss is not an error.
        let only = run_js(
            &tool,
            "JSON.stringify(grep('elsewhere', {path: 'logs/app.log'}).matches)",
        )
        .await;
        assert_eq!(only, "[]");

        // Truncation is reported and repeatable: the first lines in file order.
        let code = "const r = grep('ERROR', {path: 'logs/app.log'}); \
            JSON.stringify([r.truncated, r.matches.length, r.matches[0].line, r.matches[r.matches.length - 1].line])";
        let first = run_js(&tool, code).await;
        assert!(first.starts_with("[true,"), "{first}");
        assert!(first.contains(",1,"), "{first}");
        assert_eq!(first, run_js(&tool, code).await);

        let missing = run_js(
            &tool,
            "try { grep('x', {path: 'logs/nope.log'}) } catch (error) { error.message }",
        )
        .await;
        assert!(
            missing.contains("path must be an existing directory or regular file"),
            "{missing}"
        );
    }

    #[tokio::test]
    async fn read_lines_reads_ranges_and_tails_files_larger_than_one_mib() {
        let dir = scratch_dir();
        // 40,000 lines of 62 bytes is about 2.4 MB, above the read_file cap.
        let text: String = (1..=40_000)
            .map(|n| format!("line {n:06} {}\n", "x".repeat(50)))
            .collect();
        assert!(text.len() > READ_FILE_MAX_BYTES * 2);
        std::fs::write(dir.join("big.log"), &text).unwrap();
        std::fs::write(dir.join("crlf.txt"), "a\r\nb\r\nc\r\n").unwrap();
        std::fs::write(
            dir.join("long.txt"),
            format!("{}\nshort\n", "y".repeat(200_000)),
        )
        .unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &dir);
        let path = |name: &str| dir.join(name).to_string_lossy().replace('\\', "/");
        let (big, crlf, long) = (path("big.log"), path("crlf.txt"), path("long.txt"));

        let capped = run_js(&tool, &format!("read_file('{big}').length")).await;
        assert!(
            capped.contains("JS"),
            "read_file must stay capped: {capped}"
        );

        let head = format!(
            "const r = read_lines('{big}', 3, 2); JSON.stringify([r.lines.map(l => l.slice(0, 11)), r.eof])"
        );
        assert_eq!(
            run_js(&tool, &head).await,
            r#"[["line 000003","line 000004"],false]"#
        );
        let tail = format!(
            "const r = read_lines('{big}', -2); JSON.stringify([r.lines.map(l => l.slice(0, 11)), r.eof])"
        );
        assert_eq!(
            run_js(&tool, &tail).await,
            r#"[["line 039999","line 040000"],true]"#
        );
        // A range ending exactly at the last line reports eof; past the end is empty.
        let exact =
            format!("const r = read_lines('{big}', 39999, 2); r.lines.length + ',' + r.eof");
        assert_eq!(run_js(&tool, &exact).await, "2,true");
        let past = format!("const r = read_lines('{big}', 50000); r.lines.length + ',' + r.eof");
        assert_eq!(run_js(&tool, &past).await, "0,true");
        // Paging the whole file yields the true line count with no phantom line.
        let paging = format!(
            "let n = 0, from = 1; for (;;) {{ const r = read_lines('{big}', from, 10000); n += r.lines.length; from += r.lines.length; if (r.eof) break; }} n"
        );
        assert_eq!(run_js(&tool, &paging).await, "40000");
        assert_eq!(
            run_js(
                &tool,
                &format!("JSON.stringify(read_lines('{crlf}', 1).lines)")
            )
            .await,
            r#"["a","b","c"]"#
        );
        let long_line =
            format!("const r = read_lines('{long}', 1); r.lines[0].length + ',' + r.lines[1]");
        assert_eq!(run_js(&tool, &long_line).await, "65536,short");
        for bad in ["0", "1.5", "NaN"] {
            let out = run_js(&tool, &format!("read_lines('{crlf}', {bad})")).await;
            assert!(out.contains("JS"), "from={bad}: {out}");
        }
        let many = run_js(&tool, &format!("read_lines('{crlf}', 1, 10001)")).await;
        assert!(many.contains("JS"), "{many}");
        let missing = run_js(&tool, &format!("read_lines('{crlf}.nope', 1)")).await;
        assert!(missing.contains("JS"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn read_lines_counts_against_the_effect_budget() {
        let dir = scratch_dir();
        std::fs::write(dir.join("f.txt"), "s\n").unwrap();
        let tool = helper_tool(ModelEffectProfile::Full, &dir);
        let path = dir.join("f.txt").to_string_lossy().replace('\\', "/");
        let code = format!(
            "let n = 0; try {{ for (let i = 0; i < 1000; i++) {{ read_lines('{path}', 1, 1); n++; }} }} catch (e) {{ console.log(e.code); }} n"
        );
        let out = run_js(&tool, &code).await;
        assert!(out.contains("effect limit exceeded"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod audit_slots {
    use super::*;

    fn paths(root: &std::path::Path) -> crate::paths::AppPaths {
        crate::paths::AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: None,
        }
    }

    #[test]
    fn concurrent_audits_claim_distinct_slots_and_reuse_freed_ones() {
        let root =
            std::env::temp_dir().join(format!("mini-agent-audit-slots-{}", uuid::Uuid::new_v4()));
        let paths = paths(&root);
        let audit_dir = |slot: u32| paths.effect_audit_slot(slot).directory().to_path_buf();

        let first = open_first_free_audit_slot(&paths).expect("first writer");
        let second = open_first_free_audit_slot(&paths).expect("second writer is not locked out");
        let third = open_first_free_audit_slot(&paths).expect("third writer is not locked out");
        for slot in 0..3 {
            assert!(
                audit_dir(slot).join("writer.lock").exists(),
                "slot {slot} directory exists"
            );
        }
        assert!(!audit_dir(3).exists(), "no slot beyond the live writers");
        assert_eq!(audit_dir(0).file_name().unwrap(), "js-effects");
        assert_eq!(audit_dir(1).file_name().unwrap(), "js-effects-1");

        // Releasing a writer frees its slot for the next process; nothing accumulates.
        drop(second);
        let reused = open_first_free_audit_slot(&paths).expect("freed slot is reused");
        assert!(!audit_dir(3).exists());
        drop((first, third, reused));
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod step_result_rendering {
    use super::*;
    use crate::extras::js::protocol::DiagnosticClass;

    fn step(
        outcome: StepOutcome,
        console: Vec<ConsoleRecord>,
        diagnostic: Option<Diagnostic>,
    ) -> StepResult {
        StepResult {
            outcome,
            console,
            diagnostic,
            #[cfg(feature = "skills")]
            skill_events: Vec::new(),
            #[cfg(feature = "skills")]
            evidence_complete: true,
        }
    }

    fn record(level: ConsoleLevel, text: &str) -> ConsoleRecord {
        ConsoleRecord {
            level,
            text: text.to_string(),
            truncated: false,
        }
    }

    fn diagnostic(stage: DiagnosticStage) -> Diagnostic {
        Diagnostic {
            class: DiagnosticClass::Exception,
            stage,
            script_role: ScriptRole::Model,
            exception_class: None,
            line: None,
            column: None,
            hint: None,
        }
    }

    #[test]
    fn value_without_console_is_verbatim() {
        let result = step(StepOutcome::Value("42".into()), Vec::new(), None);
        assert_eq!(render_step_result(&result), "42");
        assert_eq!(
            render_step_result(&step(StepOutcome::Void, Vec::new(), None)),
            ""
        );
    }

    #[test]
    fn console_records_precede_the_value_in_order() {
        let result = step(
            StepOutcome::Value("done".into()),
            vec![
                record(ConsoleLevel::Log, "one"),
                record(ConsoleLevel::Warn, "two"),
                ConsoleRecord {
                    level: ConsoleLevel::Error,
                    text: "thr".into(),
                    truncated: true,
                },
            ],
            None,
        );
        assert_eq!(
            render_step_result(&result),
            "[console.log] one\n[console.warn] two\n[console.error] thr [truncated]\ndone"
        );
        let void = step(
            StepOutcome::Void,
            vec![record(ConsoleLevel::Log, "x")],
            None,
        );
        assert_eq!(render_step_result(&void), "[console.log] x");
    }

    #[test]
    fn failures_render_stage_role_and_console() {
        let result = step(
            StepOutcome::Error(JsErrorCode::Exception),
            vec![record(ConsoleLevel::Log, "before")],
            Some(diagnostic(DiagnosticStage::Evaluation)),
        );
        assert_eq!(
            render_step_result(&result),
            "JS error: exception (stage: evaluation; script: model)\n[console.log] before"
        );
        let effect_limit = step(
            StepOutcome::Error(JsErrorCode::EffectLimit),
            vec![record(ConsoleLevel::Log, "before limit")],
            Some(diagnostic(DiagnosticStage::Evaluation)),
        );
        assert_eq!(
            render_step_result(&effect_limit),
            "JS error: effect limit exceeded (256 per call); batch with read_files(paths) or glob() (stage: evaluation; script: model)\n[console.log] before limit"
        );
        let invalid_result = step(
            StepOutcome::Error(JsErrorCode::InvalidResult),
            Vec::new(),
            Some(diagnostic(DiagnosticStage::ResultConversion)),
        );
        assert_eq!(
            render_step_result(&invalid_result),
            "JS error: invalid result; return a string/plain JSON value of at most 64 KiB or end with JSON.stringify(value) (cap for returned values; result(value), where available, allows 256 KiB) (stage: result_conversion; script: model)"
        );
        let oom = step(
            StepOutcome::OutOfMemory,
            Vec::new(),
            Some(diagnostic(DiagnosticStage::Initialization)),
        );
        assert_eq!(
            render_step_result(&oom),
            "JS error: out of memory (64 MiB limit exceeded) (stage: initialization; script: model)"
        );
        let bare = step(StepOutcome::Timeout, Vec::new(), None);
        assert_eq!(
            render_step_result(&bare),
            "JS error: execution timed out (30s limit exceeded)"
        );
    }

    #[test]
    fn closed_exception_classes_and_locations_render_exactly() {
        let type_error = step(
            StepOutcome::Error(JsErrorCode::Exception),
            Vec::new(),
            Some(Diagnostic {
                class: DiagnosticClass::Exception,
                stage: DiagnosticStage::Evaluation,
                script_role: ScriptRole::Model,
                exception_class: Some(JsExceptionClass::TypeError),
                line: Some(12),
                column: Some(7),
                hint: None,
            }),
        );
        assert_eq!(
            render_step_result(&type_error),
            "JS TypeError at 12:7 (stage: evaluation; script: model)"
        );

        let stack_limit = step(
            StepOutcome::Error(JsErrorCode::StackLimit),
            Vec::new(),
            Some(Diagnostic {
                class: DiagnosticClass::ResourceLimit,
                stage: DiagnosticStage::Evaluation,
                script_role: ScriptRole::Model,
                exception_class: Some(JsExceptionClass::RangeError),
                line: Some(3),
                column: Some(2),
                hint: None,
            }),
        );
        assert_eq!(
            render_step_result(&stack_limit),
            "JS RangeError: stack limit exceeded (512 KiB stack) at 3:2 (stage: evaluation; script: model)"
        );
    }

    #[test]
    fn worker_payloads_within_bounds_are_accepted() {
        let result = step(
            StepOutcome::Value("v".repeat(MAX_RESULT_BYTES)),
            (0..MAX_CONSOLE_RECORDS)
                .map(|_| {
                    record(
                        ConsoleLevel::Log,
                        &"c".repeat(MAX_CONSOLE_BYTES / MAX_CONSOLE_RECORDS),
                    )
                })
                .collect(),
            None,
        );
        assert!(validate_step_result_bounds(&result, "").is_ok());
    }

    #[test]
    fn permission_wait_timeout_is_distinct_and_tells_the_model_not_to_retry() {
        let rendered = render_worker_execution_error(WorkerError::PermissionPromptTimedOut, false)
            .expect("permission timeout is a closed model-visible result");

        assert_eq!(rendered, PERMISSION_PROMPT_TIMEOUT_MESSAGE);
        assert!(rendered.contains("permission prompt"));
        assert!(rendered.contains("do not retry"));
        assert!(!rendered.contains("execution timed out"));
        assert_eq!(
            render_worker_execution_error(WorkerError::TimedOut, true)
                .expect("sticky prompt attribution must survive the effect boundary"),
            PERMISSION_PROMPT_TIMEOUT_MESSAGE
        );
        assert_eq!(
            render_worker_execution_error(WorkerError::TimedOut, false)
                .expect("ordinary execution timeout stays model-visible"),
            "JS error: execution timed out (30s limit exceeded)"
        );
    }

    #[test]
    fn oversized_worker_payloads_are_protocol_errors() {
        let protocol = WorkerError::Protocol.to_string();
        let oversize_value = step(
            StepOutcome::Value("v".repeat(MAX_RESULT_BYTES + 1)),
            Vec::new(),
            None,
        );
        assert_eq!(
            validate_step_result_bounds(&oversize_value, "")
                .unwrap_err()
                .to_string(),
            protocol
        );
        let too_many_records = step(
            StepOutcome::Void,
            (0..=MAX_CONSOLE_RECORDS)
                .map(|_| record(ConsoleLevel::Log, "x"))
                .collect(),
            None,
        );
        assert_eq!(
            validate_step_result_bounds(&too_many_records, "")
                .unwrap_err()
                .to_string(),
            protocol
        );
        let oversize_record = step(
            StepOutcome::Void,
            vec![record(
                ConsoleLevel::Log,
                &"x".repeat(MAX_CONSOLE_RECORD_BYTES + 1),
            )],
            None,
        );
        assert_eq!(
            validate_step_result_bounds(&oversize_record, "")
                .unwrap_err()
                .to_string(),
            protocol
        );
        let oversize_total = step(
            StepOutcome::Void,
            (0..(MAX_CONSOLE_BYTES / MAX_CONSOLE_RECORD_BYTES) + 1)
                .map(|_| record(ConsoleLevel::Log, &"x".repeat(MAX_CONSOLE_RECORD_BYTES)))
                .collect(),
            None,
        );
        assert_eq!(
            validate_step_result_bounds(&oversize_total, "")
                .unwrap_err()
                .to_string(),
            protocol
        );
    }

    #[test]
    fn parent_requires_a_matching_durably_audited_structured_result_receipt() {
        let structured = step(
            StepOutcome::Structured(serde_json::json!({"answer": 42})),
            Vec::new(),
            None,
        );
        let absent = crate::extras::js::session::StructuredResultReceipt::default();
        assert!(validate_structured_result_receipt(&structured, &absent).is_err());

        let mismatched = crate::extras::js::session::StructuredResultReceipt::default();
        mismatched.record("{\"answer\":41}".into()).unwrap();
        assert!(validate_structured_result_receipt(&structured, &mismatched).is_err());

        for near_miss in ["{\"answer\":42}x", "{\"answer\":4", "{\"answer\":421}"] {
            let receipt = crate::extras::js::session::StructuredResultReceipt::default();
            receipt.record(near_miss.into()).unwrap();
            assert!(validate_structured_result_receipt(&structured, &receipt).is_err());
        }

        let matching = crate::extras::js::session::StructuredResultReceipt::default();
        matching.record("{\"answer\":42}".into()).unwrap();
        assert!(validate_structured_result_receipt(&structured, &matching).is_ok());
        assert!(
            validate_structured_result_receipt(
                &step(StepOutcome::Void, Vec::new(), None),
                &matching,
            )
            .is_err()
        );
    }

    #[test]
    fn parent_rejects_malformed_exception_metadata() {
        let source = "first();\nsecond();";
        let valid = step(
            StepOutcome::Error(JsErrorCode::Exception),
            Vec::new(),
            Some(Diagnostic {
                class: DiagnosticClass::Exception,
                stage: DiagnosticStage::Evaluation,
                script_role: ScriptRole::Model,
                exception_class: Some(JsExceptionClass::TypeError),
                line: Some(2),
                column: Some(3),
                hint: None,
            }),
        );
        assert!(validate_step_result_bounds(&valid, source).is_ok());

        for invalid in [
            step(
                StepOutcome::Error(JsErrorCode::Exception),
                Vec::new(),
                Some(Diagnostic {
                    column: None,
                    ..valid.diagnostic.clone().unwrap()
                }),
            ),
            step(
                StepOutcome::Error(JsErrorCode::Exception),
                Vec::new(),
                Some(Diagnostic {
                    line: Some(99),
                    column: Some(99),
                    ..valid.diagnostic.clone().unwrap()
                }),
            ),
            step(
                StepOutcome::Error(JsErrorCode::Exception),
                Vec::new(),
                Some(Diagnostic {
                    script_role: ScriptRole::SkillSource,
                    ..valid.diagnostic.clone().unwrap()
                }),
            ),
            step(
                StepOutcome::Error(JsErrorCode::Syntax),
                Vec::new(),
                Some(Diagnostic {
                    class: DiagnosticClass::Syntax,
                    exception_class: Some(JsExceptionClass::TypeError),
                    ..valid.diagnostic.clone().unwrap()
                }),
            ),
        ] {
            assert!(validate_step_result_bounds(&invalid, source).is_err());
        }
    }
}

#[cfg(test)]
mod error_ergonomics_tests {
    use super::*;
    use crate::extras::js::protocol::DiagnosticClass;

    fn failure(message: Option<&str>, role: ScriptRole, class: DiagnosticClass) -> StepResult {
        StepResult {
            outcome: StepOutcome::Error(JsErrorCode::Exception),
            console: Vec::new(),
            diagnostic: Some(Diagnostic {
                class,
                stage: DiagnosticStage::Evaluation,
                script_role: role,
                exception_class: Some(JsExceptionClass::Other),
                line: None,
                column: None,
                hint: message.and_then(|text| {
                    JsDiagnosticHint::from_exception(JsExceptionClass::Other, text)
                }),
            }),
            #[cfg(feature = "skills")]
            skill_events: Vec::new(),
            #[cfg(feature = "skills")]
            evidence_complete: true,
        }
    }

    #[test]
    fn arbitrary_exception_text_never_enters_the_wire_or_rendering() {
        let raw = "FILE_CONTENTS_SECRET";
        let result = failure(Some(raw), ScriptRole::Model, DiagnosticClass::Exception);
        assert!(!render_step_result(&result).contains(raw));
        assert!(!serde_json::to_string(&result).unwrap().contains(raw));
        let forged = serde_json::json!({
            "class": "exception", "stage": "evaluation", "script_role": "model",
            "exception_class": "Other", "message": raw
        });
        assert!(serde_json::from_value::<Diagnostic>(forged).is_err());
    }

    fn classed(class: JsExceptionClass, message: &str) -> String {
        let mut result = failure(Some(message), ScriptRole::Model, DiagnosticClass::Exception);
        result.diagnostic.as_mut().unwrap().exception_class = Some(class);
        result.diagnostic.as_mut().unwrap().hint = JsDiagnosticHint::from_exception(class, message);
        render_step_result(&result)
    }

    #[test]
    fn top_level_return_gets_a_result_hint() {
        // mini-agent-zk4co
        let text = classed(JsExceptionClass::SyntaxError, "return not in a function");
        assert!(
            text.contains("scripts are not functions: use result(value)"),
            "{text}"
        );
        assert!(text.chars().count() <= 300, "{text}");
        let other = classed(JsExceptionClass::SyntaxError, "unexpected token: ';'");
        assert!(!other.contains("scripts are not functions"), "{other}");
    }

    #[test]
    fn node_globals_get_an_availability_hint() {
        // mini-agent-6g00q
        for name in [
            "require",
            "process",
            "module",
            "exports",
            "fs",
            "Buffer",
            "__dirname",
        ] {
            let text = classed(
                JsExceptionClass::ReferenceError,
                &format!("{name} is not defined"),
            );
            assert!(
                text.contains("not available: only the listed globals exist"),
                "{text}"
            );
            assert!(text.chars().count() <= 300, "{text}");
        }
        let other = classed(JsExceptionClass::ReferenceError, "foo is not defined");
        assert!(!other.contains("not available"), "{other}");
        let wrong_class = classed(JsExceptionClass::TypeError, "require is not defined");
        assert!(!wrong_class.contains("not available"), "{wrong_class}");
    }

    #[test]
    fn invalid_result_headline_names_both_caps() {
        // mini-agent-vy9fw
        let headline = js_error_headline(JsErrorCode::InvalidResult, None);
        assert!(
            headline.contains("64 KiB") && headline.contains("256 KiB"),
            "{headline}"
        );
        assert!(headline.chars().count() <= 300, "{headline}");
    }

    #[test]
    fn validator_rejects_hints_outside_model_exception_diagnostics() {
        let mut result = failure(None, ScriptRole::Model, DiagnosticClass::Exception);
        result.diagnostic.as_mut().unwrap().hint = Some(JsDiagnosticHint::CollectionProperty);
        assert!(validate_step_diagnostic(&result, "x").is_ok());
        result.diagnostic.as_mut().unwrap().script_role = ScriptRole::SkillSource;
        assert!(validate_step_diagnostic(&result, "x").is_err());
    }

    #[test]
    fn limit_headlines_state_the_limit_and_remedy() {
        let diag = None;
        assert_eq!(
            js_error_headline(JsErrorCode::EffectLimit, diag),
            "JS error: effect limit exceeded (256 per call); batch with read_files(paths) or glob()"
        );
        assert!(js_error_headline(JsErrorCode::InvalidResult, diag).contains("at most 64 KiB"));
    }

    #[test]
    fn unavailable_audit_message_says_retrying_will_not_help() {
        let permanent = audit_unavailable_message(AuditError::Unavailable);
        assert!(
            permanent.contains("retrying will not help in this session; use read, grep or shell")
        );
        let locked = audit_unavailable_message(AuditError::WriterLocked);
        assert!(locked.contains("close one and retry later"), "{locked}");
    }

    #[test]
    fn shared_audit_cell_retries_writer_locked_but_caches_permanent_failures() {
        let good = shared_effect_audit().expect("test effect audit");
        let cell = SharedAuditCell::default();
        let start = Instant::now();
        let mut opens = 0;
        let first = cell.get_or_open(start, || {
            opens += 1;
            Err(AuditError::WriterLocked)
        });
        assert!(matches!(first, Err(AuditError::WriterLocked)));
        // Too soon: not retried.
        let early = cell.get_or_open(start + Duration::from_millis(10), || {
            opens += 1;
            Ok(good.clone())
        });
        assert!(matches!(early, Err(AuditError::WriterLocked)));
        assert_eq!(opens, 1);
        // Slot freed later: the next spaced call recovers and the success is cached.
        let later = start + AUDIT_OPEN_RETRY_SPACING;
        assert!(
            cell.get_or_open(later, || {
                opens += 1;
                Ok(good.clone())
            })
            .is_ok()
        );
        assert!(
            cell.get_or_open(later, || panic!("cached success must not reopen"))
                .is_ok()
        );
        assert_eq!(opens, 2);

        let permanent = SharedAuditCell::default();
        assert!(matches!(
            permanent.get_or_open(start, || Err(AuditError::PathUnavailable)),
            Err(AuditError::PathUnavailable)
        ));
        let again = permanent.get_or_open(later + Duration::from_secs(60), || panic!("reopened"));
        assert!(matches!(again, Err(AuditError::PathUnavailable)));
    }

    #[test]
    fn shared_audit_cell_bounds_transient_retries() {
        let cell = SharedAuditCell::default();
        let start = Instant::now();
        let mut opens = 0u32;
        for attempt in 0..(AUDIT_OPEN_MAX_RETRIES + 10) {
            let now = start + AUDIT_OPEN_RETRY_SPACING * (attempt + 1);
            let _ = cell.get_or_open(now, || {
                opens += 1;
                Err(AuditError::WriterLocked)
            });
        }
        assert_eq!(opens, AUDIT_OPEN_MAX_RETRIES);
    }
}

#[cfg(test)]
mod xml_bpmn_pack_tests {
    use super::*;
    use rig::tool::Tool;

    fn tool(profile: ModelEffectProfile, root: &std::path::Path) -> JsTool {
        let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
        ));
        let mut tool = JsTool::new_with_runtime_for_test(
            Sandbox::new(false, "bwrap"),
            None,
            None,
            AllowConfig::unrestricted(root),
            supervisor,
            shared_effect_audit().expect("test effect audit"),
        );
        tool.profile = profile;
        tool
    }

    /// The XML and BPMN helpers are pure, so both effect profiles install them and keep them out
    /// of the always-sent description (they are listed by `help()` only).
    #[tokio::test]
    async fn xml_and_bpmn_helpers_work_in_both_profiles_without_growing_the_description() {
        let root = std::env::current_dir().unwrap();
        let code = r##"
            const x = xml_parse('<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="d"><bpmn:process id="p"/></bpmn:definitions>');
            const outline = bpmn_from_outline([{id: 's', type: 'start', next: 'e'}, {id: 'e', type: 'end'}]);
            JSON.stringify([x.children[0].attrs.id, xml_build({name: 'a', text: '<'}),
              bpmn_validate(outline).filter((f) => f.level === 'error').length, bpmn_summary(outline).counts,
              (() => { try { xml_parse('<!DOCTYPE a><a/>'); } catch (e) { return e.name; } })()])"##;
        for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
            let tool = tool(profile, &root);
            let description = tool.description();
            let help = tool
                .call(JsArgs {
                    code: "help()".into(),
                })
                .await
                .unwrap();
            for name in [
                "xml_parse",
                "xml_build",
                "bpmn_validate",
                "bpmn_summary",
                "bpmn_from_outline",
            ] {
                assert!(!description.contains(name), "{profile:?} {name}");
                assert!(help.contains(&format!("{name}(")), "{profile:?}: {help}");
            }
            let out = tool.call(JsArgs { code: code.into() }).await.unwrap();
            assert_eq!(
                out, r#"["p","<a>&lt;</a>",0,{"endEvent":1,"startEvent":1},"SyntaxError"]"#,
                "{profile:?}"
            );
        }
    }
}
