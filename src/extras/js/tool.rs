use std::fmt;
#[cfg(feature = "skills")]
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use compact_str::CompactString;
use rig::tool::Tool;
use serde::Deserialize;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use crate::agent::tools::ToolError;
use crate::extras::js::engine::{NormalExecutionHosts, js_thread_main};
use crate::extras::js::host::AllowConfig;
#[cfg(feature = "skills")]
use crate::extras::js::skills::admission::AdmissionWorker;
#[cfg(feature = "skills")]
use crate::extras::js::skills::proposal::{
    AttemptBudget, DEFAULT_SESSION_ATTEMPTS, ProposalHost, ProposalWorker,
};
use crate::extras::js::types::{
    JsOutcome, JsRequest, JsResponse, PermCancellation, PermOutcome, PermRequest,
    PermRequestBuildError, PermResponse, PermResponseRejection, PermissionBackendFailure,
    PermissionDenial, STEP_TIMEOUT, THREAD_STACK,
};
use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::{CheckResult, PermCheck};
use crate::sandbox::Sandbox;

const PERMISSION_WAIT_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy)]
enum PermissionCheckKind {
    Input,
    Path,
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

    #[cfg(feature = "sandbox")]
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

    pub(crate) fn check_path(&self, tool: &str, key: &str) -> Result<(), PermissionBridgeError> {
        self.check_sync(PermissionCheckKind::Path, tool, key)
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
                kind: PermissionCheckKind::Input,
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
    tx: Option<mpsc::Sender<JsRequest>>,
    permission_bridge: PermissionBridgeOwner,
    js_thread: Option<std::thread::JoinHandle<()>>,
    runtime: tokio::runtime::Handle,
    _thread_stopped: Arc<AtomicBool>,
    #[cfg(feature = "skills")]
    skill_turn_context: Arc<crate::extras::js::skills::turn::SkillTurnContext>,
    #[cfg(feature = "skills")]
    proposal_worker: Option<ProposalWorker>,
    #[cfg(feature = "skills")]
    _admission_worker: Option<AdmissionWorker>,
    #[cfg(feature = "skills")]
    telemetry: Option<crate::extras::js::skills::telemetry::TelemetryDispatcher>,
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
        Self::new_with_hosts(
            sandbox,
            permission,
            ask_tx,
            allow_config,
            NormalExecutionHosts::default(),
        )
    }

    #[cfg(feature = "skills")]
    pub(crate) fn new_with_proposals(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
        proposal_worker: ProposalWorker,
    ) -> Self {
        let proposal_host = ProposalHost::new(
            proposal_worker.sender(),
            AttemptBudget::new(DEFAULT_SESSION_ATTEMPTS),
        );
        let mut tool = Self::new_with_hosts(
            sandbox,
            permission,
            ask_tx,
            allow_config,
            NormalExecutionHosts::with_proposal(proposal_host),
        );
        tool.proposal_worker = Some(proposal_worker);
        tool
    }

    #[cfg(feature = "skills")]
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn new_with_skill_workers(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
        proposal_worker: ProposalWorker,
        admission_worker: AdmissionWorker,
    ) -> Self {
        let mut tool =
            Self::new_with_proposals(sandbox, permission, ask_tx, allow_config, proposal_worker);
        tool._admission_worker = Some(admission_worker);
        tool
    }

    fn new_with_hosts(
        sandbox: Sandbox,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        allow_config: AllowConfig,
        execution_hosts: NormalExecutionHosts,
    ) -> Self {
        let permission_bridge = PermissionBridgeOwner::new(permission, ask_tx, STEP_TIMEOUT);
        let bridge = permission_bridge.bridge();
        let (tx, rx) = mpsc::channel();
        let runtime = tokio::runtime::Handle::current();
        let js_runtime = runtime.clone();
        let thread_stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped_on_exit = Arc::clone(&thread_stopped);
        let js_thread = std::thread::Builder::new()
            .name("js-engine".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let _stopped = ThreadStopped(thread_stopped_on_exit);
                js_thread_main(
                    rx,
                    sandbox,
                    bridge,
                    js_runtime,
                    allow_config,
                    execution_hosts,
                );
            })
            .expect("failed to spawn JS thread");

        Self {
            tx: Some(tx),
            permission_bridge,
            js_thread: Some(js_thread),
            runtime,
            _thread_stopped: thread_stopped,
            #[cfg(feature = "skills")]
            skill_turn_context: Arc::new(crate::extras::js::skills::turn::SkillTurnContext::new(
                crate::extras::js::skills::turn::TurnSkillBundle::empty("unconfigured"),
            )),
            #[cfg(feature = "skills")]
            proposal_worker: None,
            #[cfg(feature = "skills")]
            _admission_worker: None,
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
        self.telemetry = Some(telemetry);
        self
    }

    #[cfg(test)]
    fn thread_stopped_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self._thread_stopped)
    }
}

struct ThreadStopped(Arc<AtomicBool>);

impl Drop for ThreadStopped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Drop for JsTool {
    fn drop(&mut self) {
        self.permission_bridge.shutdown();
        self.tx.take();

        let Some(js_thread) = self.js_thread.take() else {
            return;
        };
        if js_thread.is_finished() {
            let _ = js_thread.join();
        } else {
            // Drop cannot await; retain the task long enough to make the intentional detach explicit.
            std::mem::drop(self.runtime.spawn_blocking(move || {
                let _ = js_thread.join();
            }));
        }
    }
}

async fn await_js_response(
    reply_rx: oneshot::Receiver<JsResponse>,
    timeout: Duration,
) -> Result<JsResponse, ToolError> {
    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => Err(ToolError::Msg("JS engine reply channel closed".into())),
        Err(_) => Err(ToolError::Msg("JS engine reply timeout".into())),
    }
}

impl Tool for JsTool {
    const NAME: &'static str = "js";
    type Error = ToolError;
    type Args = JsArgs;
    type Output = String;

    fn description(&self) -> String {
        let globals = if cfg!(feature = "sandbox") {
            "read_file(path), write_file(path, content), fetch(url, options), \
             spawn(cmd, args), console.log(...)"
                .to_string()
        } else {
            "read_file(path), write_file(path, content), spawn(cmd, args), console.log(...)"
                .to_string()
        };
        #[cfg(feature = "skills")]
        let globals = {
            let mut globals = globals;
            if self.proposal_worker.is_some() {
                globals.push_str(
                    ", propose_skill({source, description, exports, tests, capability, tags?, \
                     predecessor_id?})",
                );
            }
            globals
        };
        format!(
            "Execute JavaScript code. Available globals: {globals}. Returns the last expression \
             value as a string. Errors include the stack trace for self-correction."
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

        let cancellation = PermCancellation::new();
        let mut cancel_on_drop = CancelOnDrop::new(cancellation.clone());
        let (reply_tx, reply_rx) = oneshot::channel();
        #[cfg(feature = "skills")]
        let skill_bundle = self.skill_turn_context.snapshot();
        #[cfg(feature = "skills")]
        let skill_tool_call_id = format!(
            "{}:js:{}",
            skill_bundle.turn_id,
            self.skill_tool_call_ordinal.fetch_add(1, Ordering::Relaxed)
        );
        self.tx
            .as_ref()
            .ok_or_else(|| ToolError::Msg("JS engine thread disconnected".into()))?
            .send(JsRequest {
                code: args.code,
                #[cfg(feature = "skills")]
                skill_bundle,
                #[cfg(feature = "skills")]
                skill_tool_call_id,
                cancellation,
                reply: reply_tx,
            })
            .map_err(|_| ToolError::Msg("JS engine thread disconnected".into()))?;

        let response = await_js_response(reply_rx, STEP_TIMEOUT).await?;
        cancel_on_drop.disarm();

        #[cfg(feature = "skills")]
        if !response.skill_events.is_empty() {
            match crate::extras::js::skills::telemetry::EventBatch::new(response.skill_events) {
                Ok(batch) => {
                    if let Some(dispatcher) = &self.telemetry {
                        if let Err(error) = dispatcher.try_dispatch(batch) {
                            tracing::error!(
                                error = %error,
                                "skill telemetry queue unavailable; turn evidence was excluded"
                            );
                        }
                    } else {
                        tracing::warn!(
                            "skill telemetry dispatcher is not configured; turn evidence was excluded"
                        );
                    }
                }
                Err(error) => tracing::error!(
                    error = %error,
                    "invalid skill event batch was excluded"
                ),
            }
        }

        match response.outcome {
            JsOutcome::Value(value) => Ok(value),
            JsOutcome::Void => Ok(String::new()),
            JsOutcome::Error(error) => Ok(format!("JS error:\n{error}")),
            JsOutcome::Timeout => Ok("JS error: execution timed out (30s limit exceeded)".into()),
            JsOutcome::OomKilled => Ok("JS error: out of memory (64 MiB limit exceeded)".into()),
        }
    }
}

#[derive(Deserialize)]
pub struct JsArgs {
    pub code: String,
}

#[cfg(test)]
mod js_tool_reply {
    use super::*;

    #[tokio::test]
    async fn stalled_js_engine_reply_returns_timeout_error() {
        let (_reply_tx, reply_rx) = oneshot::channel();
        let started = Instant::now();

        let error = await_js_response(reply_rx, Duration::from_millis(30))
            .await
            .expect_err("stalled reply should time out");

        assert!(matches!(
            error,
            ToolError::Msg(message) if message == "JS engine reply timeout"
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
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
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Restrictive,
            std::env::current_dir().ok(),
            Some(vec!["restrictive".to_string()]),
        )))
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

    #[tokio::test]
    async fn js_permission_bridge_tool_drop_stops_receiver_and_js_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JsTool>();

        let (ask_tx, mut ask_rx) = tokio_mpsc::channel(1);
        let tool = JsTool::new(
            Sandbox::new(false, "bwrap"),
            Some(permission(Action::Ask)),
            Some(ask_tx),
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        );
        let thread_stopped = tool.thread_stopped_flag();
        drop(tool);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !thread_stopped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("JS thread did not stop after tool drop");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("permission receiver did not stop")
                .is_none()
        );
    }
}
