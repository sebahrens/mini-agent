use std::fmt;
#[cfg(feature = "skills")]
use std::sync::atomic::AtomicU64;
#[cfg(feature = "skills")]
use std::sync::atomic::Ordering;
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
#[cfg(feature = "sandbox")]
use crate::extras::js::host::FetchEffectService;
use crate::extras::js::host::{
    AllowConfig, FileEffectService, ParentHostEffectService, SpawnEffectService,
};
use crate::extras::js::protocol::{
    ConsoleLevel, ConsoleRecord, Diagnostic, DiagnosticStage, InvocationId, JsErrorCode, RunStep,
    ScriptRole, StepOutcome, StepResult,
};
#[cfg(feature = "skills")]
use crate::extras::js::protocol::{
    MAX_SKILL_ARTIFACTS_PER_STEP, MAX_SKILL_CAPABILITY_GRANTS_PER_STEP,
    MAX_SKILL_EXPORTS_PER_ARTIFACT,
};
#[cfg(feature = "skills")]
use crate::extras::js::skills::proposal::ProposalEffectService;
#[cfg(all(feature = "skills", test))]
use crate::extras::js::skills::proposal::{
    AttemptBudget, DEFAULT_SESSION_ATTEMPTS, ProposalHost, ProposalWorker,
};
use crate::extras::js::supervisor::{JsWorkerSupervisor, WorkerError};
use crate::extras::js::types::{
    PermCancellation, PermOutcome, PermRequest, PermRequestBuildError, PermResponse,
    PermResponseRejection, PermissionBackendFailure, PermissionDenial, STEP_TIMEOUT,
};
use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::{CheckResult, PermCheck};
use crate::sandbox::Sandbox;

const PERMISSION_WAIT_POLL: Duration = Duration::from_millis(10);

#[derive(Clone)]
enum PermissionCheckKind {
    Input,
    Path,
    StructuredInput { policy_input: String },
    BoundPath,
}

enum PermissionReply {
    Sync(mpsc::SyncSender<PermResponse>),
    Async(oneshot::Sender<PermResponse>),
}

impl PermissionReply {
    fn send(self, response: PermResponse) {
        match self {
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
    reply: PermissionReply,
}

#[derive(Clone)]
pub(crate) struct PermissionBridge {
    tx: tokio_mpsc::UnboundedSender<PermissionEnvelope>,
    shutdown: PermCancellation,
    invocation_cancellation: Option<PermCancellation>,
    host_call_cancellation: Option<PermCancellation>,
    timeout: Duration,
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
            host_call_cancellation: None,
            timeout,
        }
    }

    pub(crate) fn for_invocation(&self, cancellation: PermCancellation) -> Self {
        let mut bridge = self.clone();
        bridge.invocation_cancellation = Some(cancellation);
        bridge
    }

    pub(crate) fn for_host_call(&self, cancellation: PermCancellation) -> Self {
        let mut bridge = self.clone();
        bridge.host_call_cancellation = Some(cancellation);
        bridge
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    pub(crate) async fn cancelled(&self) {
        tokio::select! {
            _ = self.shutdown.cancelled() => {}
            _ = wait_for_cancellation(self.invocation_cancellation.clone()) => {}
            _ = wait_for_cancellation(self.host_call_cancellation.clone()) => {}
        }
    }

    pub(crate) fn check(&self, tool: &str, key: &str) -> Result<(), PermissionBridgeError> {
        self.check_sync(PermissionCheckKind::Input, tool, key)
    }

    #[allow(dead_code)] // Retained for compatibility with synchronous host tests.
    pub(crate) fn check_path(&self, tool: &str, key: &str) -> Result<(), PermissionBridgeError> {
        self.check_sync(PermissionCheckKind::Path, tool, key)
    }

    pub(crate) fn check_bound_path(
        &self,
        tool: &str,
        key: &str,
    ) -> Result<(), PermissionBridgeError> {
        self.check_sync(PermissionCheckKind::BoundPath, tool, key)
    }

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
                reply: PermissionReply::Sync(reply_tx),
            })
            .map_err(|_| self.closed_request_error())?;

        loop {
            if self.is_cancelled() {
                return Err(PermissionBridgeError::Cancelled);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(PermissionBridgeError::TimedOut);
            };
            match reply_rx.recv_timeout(remaining.min(PERMISSION_WAIT_POLL)) {
                Ok(response) => {
                    let outcome = guard
                        .accept_response(response)
                        .map_err(PermissionBridgeError::from_rejection)?;
                    cancel_on_drop.disarm();
                    return PermissionBridgeError::from_outcome(outcome);
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
                return Err(PermissionBridgeError::TimedOut);
            }
        };

        let outcome = guard
            .accept_response(response)
            .map_err(PermissionBridgeError::from_rejection)?;
        cancel_on_drop.disarm();
        PermissionBridgeError::from_outcome(outcome)
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
            Self::Denied(PermissionDenial::NonInteractive) => {
                f.write_str("Permission denied (non-interactive mode)")
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
        let task = tokio::spawn(run_permission_receiver(
            rx,
            permission,
            ask_tx,
            shutdown.clone(),
        ));
        Self {
            bridge: PermissionBridge::new(tx, shutdown, timeout),
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
        reply,
    } = envelope;
    let request_id = request.id();
    let deadline = request.deadline();
    let cancellation = request.cancellation().clone();

    let outcome = tokio::select! {
        _ = shutdown.cancelled() => PermOutcome::Cancelled,
        _ = cancellation.cancelled() => PermOutcome::Cancelled,
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            PermOutcome::TimedOut
        }
        outcome = resolve_permission(&request, kind, permission, ask_tx) => outcome,
    };

    reply.send(PermResponse::new(request_id, outcome));
}

async fn resolve_permission(
    request: &PermRequest,
    kind: PermissionCheckKind,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> PermOutcome {
    let Some(permission) = permission else {
        return PermOutcome::Allowed;
    };

    let check = {
        let mut checker = permission.lock().unwrap_or_else(|error| error.into_inner());
        match kind {
            PermissionCheckKind::Input => checker.check(request.tool(), request.key()),
            PermissionCheckKind::Path => checker.check_path(request.tool(), request.key()),
            PermissionCheckKind::StructuredInput { policy_input } => {
                checker.check_with_identity(request.tool(), &policy_input, request.key())
            }
            PermissionCheckKind::BoundPath => {
                checker.check_bound_path(request.tool(), request.key())
            }
        }
    };

    match check {
        CheckResult::Allowed | CheckResult::AllowedWithCoaching(_) => PermOutcome::Allowed,
        CheckResult::Denied(reason) => PermOutcome::Denied(PermissionDenial::Policy(reason)),
        CheckResult::Ask => {
            let Some(ask_tx) = ask_tx else {
                return PermOutcome::Denied(PermissionDenial::NonInteractive);
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if ask_tx
                .send(AskRequest {
                    tool: CompactString::new(request.tool()),
                    input: request.key().to_string(),
                    tool_call_id: crate::permission::ask::take_tool_call_id("js"),
                    suggested_pattern: None,
                    additional_allow_patterns: Vec::new(),
                    reply: reply_tx,
                })
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
    audit: Result<SharedEffectAudit, AuditError>,
    permission_bridge: PermissionBridgeOwner,
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
    skill_tool_call_ordinal: AtomicU64,
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
            shared_effect_audit(),
        )
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
        audit: Result<SharedEffectAudit, AuditError>,
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
            skill_tool_call_ordinal: AtomicU64::new(0),
        }
    }

    #[cfg(feature = "skills")]
    pub fn with_skill_turn_context(
        mut self,
        context: Arc<crate::extras::js::skills::turn::SkillTurnContext>,
    ) -> Self {
        self.skill_turn_context = context;
        self
    }

    #[cfg(feature = "skills")]
    #[cfg_attr(test, allow(dead_code))]
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
            Ok(audit),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_failed_audit_for_test(
        sandbox: Sandbox,
        allow_config: AllowConfig,
        supervisor: Arc<JsWorkerSupervisor>,
        error: AuditError,
    ) -> Self {
        Self::new_with_runtime(sandbox, None, None, allow_config, supervisor, Err(error))
    }
}

fn shared_effect_audit() -> Result<SharedEffectAudit, AuditError> {
    static SHARED: OnceLock<Result<SharedEffectAudit, AuditError>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
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
            let paths = crate::paths::process_paths().map_err(|_| AuditError::PathUnavailable)?;
            EffectAudit::open(paths.effect_audit())
                .map(|audit| Arc::new(std::sync::Mutex::new(audit)))
        })
        .clone()
}

impl Tool for JsTool {
    const NAME: &'static str = "js";
    type Error = ToolError;
    type Args = JsArgs;
    type Output = String;

    fn description(&self) -> String {
        let mut globals = vec![
            "read_file(path)",
            "write_file(path, content)",
            "console.log(...)",
        ];
        if cfg!(feature = "sandbox") {
            globals.push("fetch(url, options)");
        }
        if self.sandbox.owns_complete_process_tree() {
            globals.push("spawn(cmd, args)");
        }
        #[cfg(feature = "skills")]
        let mut proposal_guidance = "";
        #[cfg(feature = "skills")]
        if self.proposal_service.is_some() {
            globals.push("propose_skill(draft)");
            proposal_guidance = " After a pattern proves repeated and generalizable, curate it with \
                propose_skill({source, description, exports: [{name, signature}], tests, \
                capability: {tier, grants}, tags?, predecessor_id?}). Every test must be a \
                JavaScript expression that returns exactly true. Tier is pure, read_only, or \
                side_effecting. Grants use {kind: 'read_file'|'write_file', workspace_prefixes}, \
                {kind: 'fetch', origins, methods}, or {kind: 'spawn', programs}; use [] when no \
                effects are needed. A proposal is an immutable candidate for verification and \
                human-gated admission; it is not executed or activated by proposing it.";
        }
        #[cfg(not(feature = "skills"))]
        let proposal_guidance = "";
        format!(
            "Execute JavaScript code. Prefer this tool for computation, parsing, data \
             transformation, control flow, and cross-platform automation instead of invoking \
             Python through a shell. Available globals: {}. Returns the last expression \
             value as a string. Runtime failures use closed, source-free error classes.{}",
            globals.join(", "),
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

        // Create one absolute deadline for the entire invocation (30s from start)
        let call_deadline = Instant::now()
            .checked_add(STEP_TIMEOUT)
            .ok_or_else(|| ToolError::Msg("JS tool deadline unavailable".into()))?;

        let cancellation = PermCancellation::new();
        let mut cancel_on_drop = CancelOnDrop::new(cancellation.clone());
        #[cfg(feature = "skills")]
        let skill_bundle = self.skill_turn_context.snapshot();
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
        let mut model_capabilities = std::collections::BTreeSet::from([
            HostCapability::ReadFile,
            HostCapability::WriteFile,
            HostCapability::Fetch,
        ]);
        if self.sandbox.owns_complete_process_tree() {
            model_capabilities.insert(HostCapability::Spawn);
        }
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
        let proposal_grant_id = self.proposal_service.as_ref().map(|_| {
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
        );
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
        let audit = self
            .audit
            .clone()
            .map_err(|_| ToolError::Msg("JS effect audit unavailable".into()))?;
        let broker = InvocationBroker::new(
            invocation_id.clone(),
            grants,
            session_capabilities,
            service,
            audit,
        )
        .map_err(|_| ToolError::Msg("JS invocation authority unavailable".into()))?;
        #[cfg(feature = "skills")]
        let broker = broker.with_skill_call_authority(skill_call_authority);
        let run_step = RunStep::new(args.code).with_model_grant(model_grant_id);
        #[cfg(feature = "skills")]
        let run_step = if let Some(grant_id) = proposal_grant_id {
            run_step.with_proposal_grant(grant_id)
        } else {
            run_step
        };
        #[cfg(feature = "skills")]
        let run_step = run_step.with_skills(
            skill_bundle
                .skills
                .iter()
                .map(|skill| crate::extras::js::skills::SkillArtifact {
                    id: skill.id.clone(),
                    identity_version: skill.identity_version,
                    abi_version: skill.abi_version,
                    source: skill.source.clone(),
                    description: skill.description.clone(),
                    tags: skill.tags.clone(),
                    exports: skill.exports.clone(),
                    tests: skill.tests.clone(),
                    capability: skill.capability.clone(),
                })
                .collect(),
            skill_bundle.turn_id.clone(),
            skill_tool_call_id.clone(),
        );
        let response = match self
            .supervisor
            .execute_bound_with_deadline(
                invocation_id,
                run_step,
                broker,
                cancellation,
                Some(call_deadline),
            )
            .await
        {
            Ok(response) => response,
            Err(WorkerError::TimedOut) => {
                return Ok("JS error: execution timed out (30s limit exceeded)".into());
            }
            Err(error) => return Err(worker_tool_error(error)),
        };
        cancel_on_drop.disarm();
        validate_step_result_bounds(&response)?;

        #[cfg(feature = "skills")]
        dispatch_skill_telemetry(
            self.telemetry.as_deref(),
            &skill_bundle,
            &skill_tool_call_id,
            &response.outcome,
            &response.skill_events,
            response.evidence_complete,
        );

        Ok(render_step_result(&response))
    }
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
fn dispatch_skill_telemetry(
    dispatcher: Option<&crate::extras::js::skills::telemetry::TelemetryDispatcher>,
    bundle: &crate::extras::js::skills::turn::TurnSkillBundle,
    tool_call_id: &str,
    step_outcome: &StepOutcome,
    worker_events: &[crate::extras::js::skills::telemetry::SkillEvent],
    _worker_claimed_evidence_complete: bool,
) -> bool {
    use crate::extras::js::skills::telemetry::{
        ParentSkillBinding, ParentTelemetryContext, bind_worker_events, observability_lost_batch,
    };

    let mut skills = Vec::with_capacity(bundle.skills.len());
    for skill in &bundle.skills {
        let Ok(retrieval_rank) = u32::try_from(skill.rank) else {
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
        production: true,
        step_outcome: step_outcome.clone(),
        skills,
    };

    let batch = match bind_worker_events(&context, worker_events) {
        Ok(batch) => batch,
        Err(_) => {
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
    if batch.events().is_empty() {
        return true;
    }
    let Some(dispatcher) = dispatcher else {
        record_observability_lost(None, "dispatcher_unavailable");
        return false;
    };
    match dispatcher.try_dispatch(batch) {
        Ok(()) => true,
        Err(_) => {
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

/// Parent-side ceilings for worker-supplied result and console payloads.
///
/// They mirror the worker's own bounds (`MAX_RESULT_BYTES`, `MAX_CONSOLE_*`
/// in `worker.rs`). The parent never trusts the worker to have applied them:
/// a step result that exceeds them is a protocol violation and is rejected
/// instead of being forwarded to the model.
const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_CONSOLE_RECORDS: usize = 256;
const MAX_CONSOLE_BYTES: usize = 256 * 1024;
const MAX_CONSOLE_RECORD_BYTES: usize = 8 * 1024;

fn validate_step_result_bounds(result: &StepResult) -> Result<(), ToolError> {
    let violation = || worker_tool_error(WorkerError::Protocol);
    if let StepOutcome::Value(value) = &result.outcome
        && value.len() > MAX_RESULT_BYTES
    {
        return Err(violation());
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
    Ok(())
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
        StepOutcome::Void => {
            let mut text = String::new();
            render_console(&result.console, &mut text);
            if text.ends_with('\n') {
                text.pop();
            }
            text
        }
        StepOutcome::Error(code) => {
            render_failure(&format!("JS error: {}", js_error_code(*code)), result)
        }
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
    if let Some(line) = diagnostic.line {
        text.push_str(&format!("; line {line}"));
        if let Some(column) = diagnostic.column {
            text.push_str(&format!(", column {column}"));
        }
    }
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
        JsErrorCode::InvalidResult => "invalid result",
        JsErrorCode::Internal => "internal error",
    }
}

#[derive(Deserialize)]
pub struct JsArgs {
    pub code: String,
}

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
        let request = ask_rx.recv().await.expect("ask request should arrive");
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

    #[tokio::test]
    async fn js_permission_bridge_timeout_is_bounded() {
        let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_millis(30));
        let started = Instant::now();
        let check = tokio::task::spawn_blocking(move || bridge.check("bash", "sleep forever"));
        let _request = rx.recv().await.expect("permission request should arrive");

        assert_eq!(
            check.await.expect("permission task should not panic"),
            Err(PermissionBridgeError::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
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

    #[tokio::test]
    async fn js_permission_bridge_dropped_request_sender_stops_receiver() {
        let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_secs(1));
        drop(bridge);

        assert!(
            rx.recv().await.is_none(),
            "request receiver should close when its final sender drops"
        );
    }

    #[tokio::test]
    async fn js_permission_bridge_dropped_response_sender_is_deterministic() {
        let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_secs(1));
        let check =
            tokio::task::spawn_blocking(move || bridge.check("bash", "printf disconnected"));
        let envelope = rx.recv().await.expect("permission request should arrive");
        drop(envelope);

        assert_eq!(
            check.await.expect("permission task should not panic"),
            Err(PermissionBridgeError::ResponseChannelClosed)
        );
    }

    #[tokio::test]
    async fn js_permission_bridge_dropped_response_receiver_discards_late_reply() {
        let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_millis(30));
        let check = tokio::task::spawn_blocking(move || bridge.check("bash", "printf late"));
        let envelope = rx.recv().await.expect("permission request should arrive");
        assert_eq!(
            check.await.expect("permission task should not panic"),
            Err(PermissionBridgeError::TimedOut)
        );

        reply(envelope, PermOutcome::Allowed);
    }

    #[tokio::test]
    async fn js_permission_bridge_cancellation_unblocks_sync_wait() {
        let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_secs(1));
        let invocation = PermCancellation::new();
        let invocation_for_thread = invocation.clone();
        let bridge = bridge.for_invocation(invocation_for_thread);
        let check = tokio::task::spawn_blocking(move || bridge.check("bash", "printf cancelled"));
        let _request = rx.recv().await.expect("permission request should arrive");
        invocation.cancel();

        assert_eq!(
            check.await.expect("permission task should not panic"),
            Err(PermissionBridgeError::Cancelled)
        );
    }

    #[tokio::test]
    async fn js_permission_bridge_late_reply_cannot_satisfy_repeated_call() {
        let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_millis(40));
        let first_bridge = bridge.clone();
        let first = tokio::task::spawn_blocking(move || first_bridge.check("bash", "first"));
        let first_envelope = rx.recv().await.expect("first request should arrive");
        let first_id = first_envelope.request.id();
        assert_eq!(
            first.await.expect("first request should not panic"),
            Err(PermissionBridgeError::TimedOut)
        );

        let second = tokio::task::spawn_blocking(move || bridge.check("bash", "second"));
        let second_envelope = rx.recv().await.expect("second request should arrive");
        let second_id = second_envelope.request.id();
        assert_ne!(first_id, second_id);
        reply(first_envelope, PermOutcome::Allowed);
        reply(second_envelope, PermOutcome::Allowed);

        assert_eq!(
            second.await.expect("second request should not panic"),
            Ok(())
        );
    }

    #[tokio::test]
    async fn js_permission_bridge_out_of_order_replies_remain_correlated() {
        let (bridge, mut rx, _shutdown) = raw_bridge(Duration::from_secs(1));
        let first_bridge = bridge.clone();
        let first = tokio::task::spawn_blocking(move || first_bridge.check("bash", "first"));
        let second = tokio::task::spawn_blocking(move || bridge.check("bash", "second"));
        let first_envelope = rx.recv().await.expect("first request should arrive");
        let second_envelope = rx.recv().await.expect("second request should arrive");
        assert_ne!(first_envelope.request.id(), second_envelope.request.id());

        reply(second_envelope, PermOutcome::Allowed);
        reply(first_envelope, PermOutcome::Allowed);

        assert_eq!(first.await.expect("first request should not panic"), Ok(()));
        assert_eq!(
            second.await.expect("second request should not panic"),
            Ok(())
        );
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
        assert!(!dispatch_skill_telemetry(
            Some(&dispatcher),
            &bundle,
            "parent-turn:js:0",
            &StepOutcome::Value("ok".into()),
            &[forged],
            true,
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
    fn saturated_and_disconnected_dispatchers_fail_closed_for_positive_evidence() {
        use crate::extras::js::skills::telemetry::TelemetryDispatcher;

        let (bundle, injected) = telemetry_fixture();
        let (saturated_tx, saturated_rx) = std::sync::mpsc::sync_channel(0);
        let saturated = TelemetryDispatcher::from_sender_for_test(saturated_tx);
        assert!(!dispatch_skill_telemetry(
            Some(&saturated),
            &bundle,
            "parent-turn:js:0",
            &StepOutcome::Value("ok".into()),
            std::slice::from_ref(&injected),
            true,
        ));
        assert_eq!(saturated.observability_lost_for_test(), 1);
        drop(saturated_rx);

        let (disconnected_tx, disconnected_rx) = std::sync::mpsc::sync_channel(1);
        drop(disconnected_rx);
        let disconnected = TelemetryDispatcher::from_sender_for_test(disconnected_tx);
        assert!(!dispatch_skill_telemetry(
            Some(&disconnected),
            &bundle,
            "parent-turn:js:0",
            &StepOutcome::Value("ok".into()),
            &[injected],
            true,
        ));
        assert_eq!(disconnected.observability_lost_for_test(), 1);
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

        assert_eq!(error.to_string(), "JS effect audit unavailable");
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

    fn diagnostic(stage: DiagnosticStage, line: Option<u32>, column: Option<u32>) -> Diagnostic {
        Diagnostic {
            class: DiagnosticClass::Exception,
            stage,
            script_role: ScriptRole::Model,
            line,
            column,
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
    fn failures_render_stage_role_position_and_console() {
        let result = step(
            StepOutcome::Error(JsErrorCode::Exception),
            vec![record(ConsoleLevel::Log, "before")],
            Some(diagnostic(DiagnosticStage::Evaluation, Some(3), Some(7))),
        );
        assert_eq!(
            render_step_result(&result),
            "JS error: exception (stage: evaluation; script: model; line 3, column 7)\n[console.log] before"
        );
        let oom = step(
            StepOutcome::OutOfMemory,
            Vec::new(),
            Some(diagnostic(DiagnosticStage::Initialization, None, None)),
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
        assert!(validate_step_result_bounds(&result).is_ok());
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
            validate_step_result_bounds(&oversize_value)
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
            validate_step_result_bounds(&too_many_records)
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
            validate_step_result_bounds(&oversize_record)
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
            validate_step_result_bounds(&oversize_total)
                .unwrap_err()
                .to_string(),
            protocol
        );
    }
}
