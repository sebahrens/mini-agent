use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);
pub const MEMORY_LIMIT: usize = 64 * 1024 * 1024; // 64 MiB
pub const STACK_LIMIT: usize = 512 * 1024; // 512 KiB JS stack
pub const THREAD_STACK: usize = 8 * 1024 * 1024; // 8 MiB OS thread stack
pub const READ_FILE_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub const WRITE_FILE_MAX_BYTES: usize = 1024 * 1024; // 1 MiB

/// Closed failures returned by parent-side effect services.
///
/// The service layer deliberately carries no QuickJS values or errors. Worker
/// adapters translate these classes at the process boundary without exposing
/// backend details, paths, command output, or response bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum EffectServiceError {
    #[error("effect target is invalid")]
    InvalidTarget,
    #[error("write_file does not follow final symlinks")]
    FinalSymlink,
    #[error("Path changed after permission check")]
    TargetChanged,
    #[error("effect target is outside the configured policy")]
    TargetDenied,
    #[error("no roots are configured; unrestricted access requires an explicit opt-in")]
    FileNoConfiguredRoots,
    #[error("the configured roots are invalid or ambiguous")]
    FileInvalidConfiguration,
    #[error("the resolved target is outside the configured roots")]
    FileOutsideConfiguredRoots,
    #[error("Permission denied by user")]
    PermissionDenied,
    #[error("Doom loop: repeated identical tool call")]
    DoomLoopDenied,
    #[error("effect permission prompt timed out")]
    PermissionTimedOut,
    #[error("effect was cancelled")]
    Cancelled,
    #[error("effect timed out")]
    TimedOut,
    #[error("resource limit: effect output exceeded its limit")]
    OutputLimit,
    #[error("resource limit: effect body exceeded its limit")]
    BodyLimit,
    #[error("invalid encoding: effect body is invalid")]
    InvalidBody,
    #[error("effect backend is unavailable")]
    BackendFailure,
    #[error("effect outcome is unknown after dispatch")]
    OutcomeUnknown,
}

/// Reversibly canonical permission identity for one structured spawn request.
#[derive(serde::Serialize)]
#[serde(deny_unknown_fields)]
struct SpawnPermissionSubject<'a> {
    version: u8,
    program: &'a str,
    arguments: &'a [String],
}

pub(crate) fn canonical_spawn_permission_subject(
    program: &str,
    arguments: &[String],
) -> Result<String, EffectServiceError> {
    serde_json::to_string(&SpawnPermissionSubject {
        version: 1,
        program,
        arguments,
    })
    .map_err(|_| EffectServiceError::InvalidTarget)
}

/// Compatibility rendering used only to evaluate existing Bash policy rules.
/// Authority, prompting, session approval, and execution use the structured
/// canonical subject above.
pub(crate) fn spawn_policy_input(program: &str, arguments: &[String]) -> String {
    std::iter::once(program)
        .chain(arguments.iter().map(String::as_str))
        .map(quote_spawn_policy_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_spawn_policy_word(word: &str) -> String {
    if !word.is_empty()
        && word.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
                )
        })
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

static NEXT_PERMISSION_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub struct JsRequest {
    pub code: String,
    #[cfg(feature = "skills")]
    pub skill_bundle: Arc<crate::extras::js::skills::turn::TurnSkillBundle>,
    #[cfg(feature = "skills")]
    pub skill_tool_call_id: String,
    pub cancellation: PermCancellation,
    pub reply: tokio::sync::oneshot::Sender<JsResponse>,
}

impl fmt::Debug for JsRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsRequest")
            .field("code", &Redacted)
            .field("code_len", &self.code.len())
            .field("skill_count", &{
                #[cfg(feature = "skills")]
                {
                    self.skill_bundle.skills.len()
                }
                #[cfg(not(feature = "skills"))]
                {
                    0usize
                }
            })
            .field("skill_tool_call_id", &{
                #[cfg(feature = "skills")]
                {
                    self.skill_tool_call_id.as_str()
                }
                #[cfg(not(feature = "skills"))]
                {
                    ""
                }
            })
            .field("cancellation", &self.cancellation)
            .field("reply", &"<oneshot sender>")
            .finish()
    }
}

pub struct JsResponse {
    pub outcome: JsOutcome,
    #[cfg(feature = "skills")]
    pub skill_events: Vec<crate::extras::js::skills::telemetry::SkillEvent>,
    #[cfg(feature = "skills")]
    pub evidence_complete: bool,
}

impl fmt::Debug for JsResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("JsResponse");
        debug.field("outcome", &self.outcome);
        #[cfg(feature = "skills")]
        debug
            .field("skill_event_count", &self.skill_events.len())
            .field("evidence_complete", &self.evidence_complete);
        debug.finish()
    }
}

#[cfg(feature = "skills")]
#[derive(Debug, Clone)]
pub struct InstrumentedSkill {
    pub artifact: crate::extras::js::skills::SkillArtifact,
    pub retrieval_score: f64,
    pub retrieval_rank: u32,
    pub query_fingerprint: Option<String>,
}

#[cfg(feature = "skills")]
#[derive(Debug, Clone)]
pub struct SkillExecutionBundle {
    pub turn_id: String,
    pub tool_call_id: String,
    pub index_generation: u64,
    pub production: bool,
    pub skills: Vec<InstrumentedSkill>,
}

#[cfg(feature = "skills")]
impl SkillExecutionBundle {
    pub fn from_turn_bundle(
        bundle: &crate::extras::js::skills::turn::TurnSkillBundle,
        tool_call_id: String,
    ) -> Self {
        Self {
            turn_id: bundle.turn_id.clone(),
            tool_call_id,
            index_generation: bundle.index_generation,
            production: true,
            skills: bundle
                .skills
                .iter()
                .map(|resolved| InstrumentedSkill {
                    artifact: crate::extras::js::skills::SkillArtifact {
                        id: resolved.id.clone(),
                        identity_version: resolved.identity_version,
                        abi_version: resolved.abi_version,
                        source: resolved.source.clone(),
                        description: resolved.description.clone(),
                        tags: resolved.tags.clone(),
                        exports: resolved.exports.clone(),
                        tests: resolved.tests.clone(),
                        capability: resolved.capability.clone(),
                    },
                    retrieval_score: f64::from(resolved.score()),
                    retrieval_rank: u32::try_from(resolved.rank).unwrap_or(u32::MAX),
                    query_fingerprint: (!bundle.query_fingerprint.is_empty())
                        .then(|| bundle.query_fingerprint.clone()),
                })
                .collect(),
        }
    }
}

#[derive(PartialEq, Eq)]
pub enum JsOutcome {
    Value(String),
    Void,
    Error(String),
    Timeout,
    OomKilled,
}

impl fmt::Debug for JsOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(value) => f
                .debug_struct("Value")
                .field("body", &Redacted)
                .field("body_len", &value.len())
                .finish(),
            Self::Void => f.write_str("Void"),
            Self::Error(error) => f
                .debug_struct("Error")
                .field("detail", &Redacted)
                .field("detail_len", &error.len())
                .finish(),
            Self::Timeout => f.write_str("Timeout"),
            Self::OomKilled => f.write_str("OomKilled"),
        }
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Unique identity for one permission exchange.
///
/// IDs are allocated monotonically and allocation fails before the counter can
/// wrap, so an ID is never reused within a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PermRequestId(NonZeroU64);

impl PermRequestId {
    fn next() -> Result<Self, PermRequestBuildError> {
        NEXT_PERMISSION_REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map(|id| Self(NonZeroU64::new(id).expect("permission request IDs start at one")))
            .map_err(|_| PermRequestBuildError::RequestIdExhausted)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Cooperative cancellation shared by a permission requester and its bridge.
#[derive(Clone)]
pub struct PermCancellation {
    state: Arc<PermCancellationState>,
}

#[derive(Default)]
struct PermCancellationState {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
    next_blocking_wake: AtomicU64,
    blocking_wakes: Mutex<BTreeMap<u64, Arc<dyn Fn() + Send + Sync>>>,
}

pub(crate) struct PermCancellationWake {
    state: Arc<PermCancellationState>,
    id: Option<u64>,
}

impl Default for PermCancellation {
    fn default() -> Self {
        Self {
            state: Arc::new(PermCancellationState::default()),
        }
    }
}

impl PermCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_waiters();
            let wakes = self
                .state
                .blocking_wakes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for wake in wakes {
                wake();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn register_blocking_wake(
        &self,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> PermCancellationWake {
        let id = self
            .state
            .next_blocking_wake
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let mut wakes = self
            .state
            .blocking_wakes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_cancelled() || id == 0 || wakes.contains_key(&id) {
            drop(wakes);
            wake();
            return PermCancellationWake {
                state: Arc::clone(&self.state),
                id: None,
            };
        }
        wakes.insert(id, wake);
        PermCancellationWake {
            state: Arc::clone(&self.state),
            id: Some(id),
        }
    }
}

impl Drop for PermCancellationWake {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        self.state
            .blocking_wakes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id);
    }
}

impl fmt::Debug for PermCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Sent from the JS thread to tokio to request permission for a host call.
///
/// The payload intentionally contains no reply sender. A bridge must keep its
/// one-shot reply capability outside this loggable protocol value.
pub struct PermRequest {
    id: PermRequestId,
    tool: String,
    key: String,
    deadline: Instant,
    cancellation: PermCancellation,
}

impl PermRequest {
    pub fn new(
        tool: impl Into<String>,
        key: impl Into<String>,
        timeout: Duration,
        cancellation: PermCancellation,
    ) -> Result<Self, PermRequestBuildError> {
        let tool = tool.into();
        if tool.trim().is_empty() {
            return Err(PermRequestBuildError::EmptyTool);
        }

        let key = key.into();
        if key.is_empty() {
            return Err(PermRequestBuildError::EmptyKey);
        }
        if timeout.is_zero() {
            return Err(PermRequestBuildError::ZeroTimeout);
        }

        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(PermRequestBuildError::DeadlineOverflow)?;

        Ok(Self {
            id: PermRequestId::next()?,
            tool,
            key,
            deadline,
            cancellation,
        })
    }

    pub fn id(&self) -> PermRequestId {
        self.id
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn cancellation(&self) -> &PermCancellation {
        &self.cancellation
    }

    pub fn response_guard(&self) -> PermResponseGuard {
        PermResponseGuard {
            request_id: self.id,
            deadline: self.deadline,
            cancellation: self.cancellation.clone(),
        }
    }

    /// Accept a response only while this request is current.
    ///
    /// Consuming the request prevents a bridge from accepting multiple
    /// responses for the same exchange.
    #[cfg(test)]
    pub fn accept_response(
        self,
        response: PermResponse,
    ) -> Result<PermOutcome, PermResponseRejection> {
        self.response_guard().accept_response(response)
    }
}

pub struct PermResponseGuard {
    request_id: PermRequestId,
    deadline: Instant,
    cancellation: PermCancellation,
}

impl PermResponseGuard {
    pub fn accept_response(
        self,
        response: PermResponse,
    ) -> Result<PermOutcome, PermResponseRejection> {
        let actual = response.request_id();
        if actual != self.request_id {
            return Err(PermResponseRejection::MismatchedRequestId {
                expected: self.request_id,
                actual,
            });
        }
        if self.cancellation.is_cancelled() {
            return Err(PermResponseRejection::Cancelled {
                request_id: self.request_id,
            });
        }
        if Instant::now() >= self.deadline {
            return Err(PermResponseRejection::DeadlineExpired {
                request_id: self.request_id,
            });
        }

        Ok(response.into_outcome())
    }
}

impl fmt::Debug for PermRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermRequest")
            .field("id", &self.id)
            .field("tool", &self.tool)
            .field("key", &Redacted)
            .field("deadline", &self.deadline)
            .field("cancellation", &self.cancellation)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermRequestBuildError {
    EmptyTool,
    EmptyKey,
    ZeroTimeout,
    DeadlineOverflow,
    RequestIdExhausted,
}

impl fmt::Display for PermRequestBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyTool => "permission tool must not be empty",
            Self::EmptyKey => "permission key must not be empty",
            Self::ZeroTimeout => "permission timeout must be greater than zero",
            Self::DeadlineOverflow => "permission deadline is out of range",
            Self::RequestIdExhausted => "permission request identity space is exhausted",
        };
        f.write_str(message)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDenial {
    Policy(String),
    User,
    NonInteractive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionBackendFailure {
    AskChannelClosed,
    AskResponseDropped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermOutcome {
    Allowed,
    Denied(PermissionDenial),
    BackendFailure(PermissionBackendFailure),
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermResponse {
    request_id: PermRequestId,
    outcome: PermOutcome,
}

impl PermResponse {
    pub fn new(request_id: PermRequestId, outcome: PermOutcome) -> Self {
        Self {
            request_id,
            outcome,
        }
    }

    fn request_id(&self) -> PermRequestId {
        self.request_id
    }

    fn into_outcome(self) -> PermOutcome {
        self.outcome
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermResponseRejection {
    MismatchedRequestId {
        expected: PermRequestId,
        actual: PermRequestId,
    },
    Cancelled {
        request_id: PermRequestId,
    },
    DeadlineExpired {
        request_id: PermRequestId,
    },
}

/// Returned to JS by `spawn(cmd, args)`.
/// Visible in JS with explicit timeout and output-truncation status.
#[derive(serde::Serialize)]
pub struct SpawnResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl fmt::Debug for SpawnResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnResult")
            .field("stdout", &Redacted)
            .field("stdout_len", &self.stdout.len())
            .field("stderr", &Redacted)
            .field("stderr_len", &self.stderr.len())
            .field("code", &self.code)
            .field("timed_out", &self.timed_out)
            .field("stdout_truncated", &self.stdout_truncated)
            .field("stderr_truncated", &self.stderr_truncated)
            .finish()
    }
}

#[cfg(test)]
mod js_permission_types {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    fn request(tool: &str, key: &str) -> PermRequest {
        PermRequest::new(tool, key, Duration::from_secs(1), PermCancellation::new())
            .expect("valid permission request")
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn simultaneous_requests_receive_distinct_nonzero_ids() {
        let barrier = Arc::new(Barrier::new(3));
        let handles = (0..2)
            .map(|index| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    request("js/spawn", &format!("command-{index}")).id()
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        let ids = handles
            .into_iter()
            .map(|handle| handle.join().expect("request thread panicked"))
            .collect::<Vec<_>>();

        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0].get(), 0);
        assert_ne!(ids[1].get(), 0);
    }

    #[test]
    fn response_correlation_rejects_another_requests_id() {
        let first = request("js/read_file", "/secret/first");
        let second = request("js/read_file", "/secret/second");
        let expected = first.id();
        let actual = second.id();

        assert_eq!(
            first.accept_response(PermResponse::new(actual, PermOutcome::Allowed)),
            Err(PermResponseRejection::MismatchedRequestId { expected, actual })
        );
    }

    #[test]
    fn expired_and_cancelled_requests_reject_otherwise_matching_responses() {
        let expired = PermRequest::new(
            "js/spawn",
            "slow-command",
            Duration::from_millis(1),
            PermCancellation::new(),
        )
        .expect("valid expiring request");
        let expired_id = expired.id();
        thread::sleep(Duration::from_millis(5));
        assert_eq!(
            expired.accept_response(PermResponse::new(expired_id, PermOutcome::Allowed)),
            Err(PermResponseRejection::DeadlineExpired {
                request_id: expired_id
            })
        );

        let cancellation = PermCancellation::new();
        let cancelled = PermRequest::new(
            "js/write_file",
            "/secret/output",
            Duration::from_secs(1),
            cancellation.clone(),
        )
        .expect("valid cancellable request");
        let cancelled_id = cancelled.id();
        cancellation.cancel();
        assert_eq!(
            cancelled.accept_response(PermResponse::new(cancelled_id, PermOutcome::Allowed)),
            Err(PermResponseRejection::Cancelled {
                request_id: cancelled_id
            })
        );
    }

    #[test]
    fn all_permission_outcomes_are_typed_and_correlated() {
        let outcomes = [
            PermOutcome::Allowed,
            PermOutcome::Denied(PermissionDenial::Policy("rule denied".to_string())),
            PermOutcome::Denied(PermissionDenial::User),
            PermOutcome::Denied(PermissionDenial::NonInteractive),
            PermOutcome::BackendFailure(PermissionBackendFailure::AskChannelClosed),
            PermOutcome::BackendFailure(PermissionBackendFailure::AskResponseDropped),
            PermOutcome::Cancelled,
            PermOutcome::TimedOut,
        ];

        for outcome in outcomes {
            let request = request("js/spawn", "printf secret");
            let id = request.id();
            let response = PermResponse::new(id, outcome.clone());
            assert_eq!(response.request_id(), id);
            assert_eq!(request.accept_response(response), Ok(outcome));
        }
    }

    #[test]
    fn request_construction_enforces_protocol_invariants() {
        let cancellation = PermCancellation::new();
        let exact = PermRequest::new(
            "js/spawn",
            "env API_TOKEN=secret command",
            Duration::from_secs(1),
            cancellation.clone(),
        )
        .expect("exact request data should be accepted");
        assert_eq!(exact.tool(), "js/spawn");
        assert_eq!(exact.key(), "env API_TOKEN=secret command");
        assert!(exact.deadline() > Instant::now());
        assert!(!exact.cancellation().is_cancelled());

        assert_eq!(
            PermRequest::new("", "key", Duration::from_secs(1), cancellation.clone())
                .expect_err("empty tool must fail"),
            PermRequestBuildError::EmptyTool
        );
        assert_eq!(
            PermRequest::new("js/spawn", "", Duration::from_secs(1), cancellation.clone())
                .expect_err("empty key must fail"),
            PermRequestBuildError::EmptyKey
        );
        assert_eq!(
            PermRequest::new("js/spawn", "key", Duration::ZERO, cancellation)
                .expect_err("zero timeout must fail"),
            PermRequestBuildError::ZeroTimeout
        );
    }

    #[test]
    fn protocol_types_are_send_sync_and_debug_output_is_redacted() {
        assert_send_sync::<PermRequestId>();
        assert_send_sync::<PermCancellation>();
        assert_send_sync::<PermRequest>();
        assert_send_sync::<PermOutcome>();
        assert_send_sync::<PermResponse>();
        assert_send_sync::<PermResponseRejection>();
        assert_send_sync::<SpawnResult>();

        let request = request("js/write_file", "SECRET_PERMISSION_KEY");
        let request_debug = format!("{request:?}");
        assert!(!request_debug.contains("SECRET_PERMISSION_KEY"));

        let spawn = SpawnResult {
            stdout: "SECRET_RESPONSE_BODY".to_string(),
            stderr: "SECRET_PROCESS_ENV".to_string(),
            code: 0,
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let spawn_debug = format!("{spawn:?}");
        assert!(!spawn_debug.contains("SECRET_RESPONSE_BODY"));
        assert!(!spawn_debug.contains("SECRET_PROCESS_ENV"));
    }
}
