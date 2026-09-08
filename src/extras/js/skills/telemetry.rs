//! Privacy-bounded, idempotent directly attributed skill telemetry.
//!
//! QuickJS wrappers build [`SkillEvent`] values in memory. The tokio side hands
//! bounded batches to [`TelemetryIngestor`], so the JS thread never blocks on
//! SQLite. No API in this module accepts prompts, source, raw arguments, file
//! contents, model output, or environment values.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use rusqlite::{ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{SkillStore, current_timestamp};
use crate::extras::js::protocol::StepOutcome;
use crate::hex;

pub const MAX_EVENT_BATCH: usize = 256;
pub const MAX_ARGUMENT_SHAPE_BYTES: usize = 512;
pub const TELEMETRY_QUEUE_CAPACITY: usize = 64;
pub const MAX_EVENT_ID_BYTES: usize = 256;
pub const MAX_EVENT_TOKEN_BYTES: usize = 128;
const TELEMETRY_BUSY_RETRY_INITIAL: Duration = Duration::from_millis(10);
const TELEMETRY_BUSY_RETRY_MAX: Duration = Duration::from_millis(250);
/// How long the worker may keep retrying a busy writer after shutdown was
/// requested. Without a bound, dropping the dispatcher waits for whatever
/// external process holds the SQLite write lock — potentially forever, and on
/// the Tokio blocking pool, which then blocks runtime teardown.
const TELEMETRY_SHUTDOWN_FLUSH_BUDGET: Duration = Duration::from_millis(1_500);
/// How many times a safety-triggered quarantine is re-applied against re-read
/// revision and generation state before it is reported as failed.
const QUARANTINE_SAFETY_ATTEMPTS: u32 = 3;

/// Cancellation state shared with the ingestion worker.
#[derive(Debug, Default)]
struct TelemetryShutdownState {
    requested: AtomicBool,
    /// Instant, as nanoseconds since the process-start baseline, after which a
    /// busy retry gives up. Only meaningful once `requested` is set.
    deadline: std::sync::Mutex<Option<std::time::Instant>>,
}

#[derive(Clone, Debug, Default)]
struct TelemetryShutdown(Arc<TelemetryShutdownState>);

impl TelemetryShutdown {
    /// Signal shutdown and start the bounded flush budget.
    fn request(&self, budget: Duration) {
        {
            let mut deadline = self
                .0
                .deadline
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if deadline.is_none() {
                *deadline = Some(std::time::Instant::now() + budget);
            }
        }
        self.0.requested.store(true, Ordering::Release);
    }

    /// `Some(remaining)` once shutdown was requested; `None` while running.
    fn remaining(&self) -> Option<Duration> {
        if !self.0.requested.load(Ordering::Acquire) {
            return None;
        }
        let deadline = self
            .0
            .deadline
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Some(
            deadline
                .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or_default(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillEventKind {
    Selected,
    Injected,
    Invoked,
    Returned,
    Threw,
    TimedOut,
    Oom,
    CapabilityDenied,
    UserPositive,
    UserNegative,
    ObservabilityLost,
}

impl SkillEventKind {
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Injected => "injected",
            Self::Invoked => "invoked",
            Self::Returned => "returned",
            Self::Threw => "threw",
            Self::TimedOut => "timed_out",
            Self::Oom => "oom",
            Self::CapabilityDenied => "capability_denied",
            Self::UserPositive => "user_positive",
            Self::UserNegative => "user_negative",
            Self::ObservabilityLost => "observability_lost",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Returned | Self::Threw | Self::TimedOut | Self::Oom | Self::CapabilityDenied
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillEvent {
    pub invocation_id: Option<String>,
    pub skill_id: String,
    pub turn_id: String,
    pub tool_call_id: Option<String>,
    pub kind: SkillEventKind,
    pub export_name: Option<String>,
    /// A closed, non-value-bearing outcome code such as `fulfilled`,
    /// `exception`, or `session_denied`.
    pub outcome: Option<String>,
    pub latency_us: Option<u64>,
    pub retrieval_score: Option<f64>,
    pub retrieval_rank: Option<u32>,
    pub query_fingerprint: Option<String>,
    pub index_generation: u64,
    pub evidence_complete: bool,
    pub production: bool,
    /// Coarse schema only, e.g. `{"argc":2,"types":["string","number"]}`.
    pub argument_shape: Option<String>,
    pub created_at: i64,
}

impl SkillEvent {
    pub fn validate(&self) -> Result<(), TelemetryError> {
        if self.skill_id.len() != 64
            || !self
                .skill_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || self.turn_id.is_empty()
            || self.turn_id.len() > MAX_EVENT_ID_BYTES
            || self
                .tool_call_id
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_EVENT_ID_BYTES)
            || self.invocation_id.as_ref().is_some_and(|value| {
                value.len() != 64
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
            || self
                .export_name
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_EVENT_TOKEN_BYTES)
            || self
                .outcome
                .as_ref()
                .is_some_and(|value| !valid_bounded_token(value))
            || self.query_fingerprint.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > MAX_EVENT_ID_BYTES
                    || value.chars().any(char::is_whitespace)
            })
            || self.created_at < 0
            || self.index_generation > i64::MAX as u64
            || self.latency_us.is_some_and(|value| value > i64::MAX as u64)
        {
            return Err(TelemetryError::InvalidEvent);
        }
        if matches!(
            self.kind,
            SkillEventKind::Invoked
                | SkillEventKind::Returned
                | SkillEventKind::Threw
                | SkillEventKind::TimedOut
                | SkillEventKind::Oom
                | SkillEventKind::CapabilityDenied
        ) && self.invocation_id.as_deref().is_none_or(str::is_empty)
        {
            return Err(TelemetryError::MissingInvocationId);
        }
        if self.kind.is_terminal() && self.export_name.as_deref().is_none_or(str::is_empty) {
            return Err(TelemetryError::InvalidEvent);
        }
        if self.argument_shape.as_ref().is_some_and(|shape| {
            shape.len() > MAX_ARGUMENT_SHAPE_BYTES || !valid_argument_shape(shape)
        }) {
            return Err(TelemetryError::ArgumentShapeTooLarge);
        }
        if self.retrieval_score.is_some_and(|score| !score.is_finite()) {
            return Err(TelemetryError::InvalidEvent);
        }
        Ok(())
    }
}

fn valid_bounded_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_EVENT_TOKEN_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
}

fn valid_argument_shape(shape: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(shape) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.len() == 1 {
        return object.get("truncated").and_then(serde_json::Value::as_bool) == Some(true);
    }
    if object.len() != 2 {
        return false;
    }
    let Some(argc) = object.get("argc").and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let Some(types) = object.get("types").and_then(serde_json::Value::as_array) else {
        return false;
    };
    argc <= 64
        && types.len() == argc as usize
        && types.iter().all(|value| {
            matches!(
                value.as_str(),
                Some(
                    "null"
                        | "array"
                        | "undefined"
                        | "boolean"
                        | "number"
                        | "string"
                        | "object"
                        | "function"
                        | "symbol"
                        | "bigint"
                )
            )
        })
}

#[derive(Debug, Clone)]
pub struct EventBatch {
    events: Vec<SkillEvent>,
}

impl EventBatch {
    pub fn new(events: Vec<SkillEvent>) -> Result<Self, TelemetryError> {
        if events.len() > MAX_EVENT_BATCH {
            return Err(TelemetryError::BatchTooLarge);
        }
        for event in &events {
            event.validate()?;
        }

        let mut terminal_by_invocation = BTreeMap::new();
        for event in &events {
            if event.kind.is_terminal() {
                let invocation = event
                    .invocation_id
                    .as_deref()
                    .ok_or(TelemetryError::MissingInvocationId)?;
                if terminal_by_invocation
                    .insert(invocation, event.kind)
                    .is_some()
                {
                    return Err(TelemetryError::MultipleTerminalOutcomes);
                }
            }
        }
        Ok(Self { events })
    }

    pub fn events(&self) -> &[SkillEvent] {
        &self.events
    }
}

/// Parent-owned metadata for one selected artifact. Worker event fields are
/// claims only; this is the source of identity and retrieval-policy truth.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParentSkillBinding {
    pub(crate) skill_id: String,
    pub(crate) exports: BTreeSet<String>,
    pub(crate) retrieval_score: f64,
    pub(crate) retrieval_rank: u32,
}

/// Immutable per-call telemetry authority retained only by the parent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParentTelemetryContext {
    pub(crate) turn_id: String,
    pub(crate) tool_call_id: String,
    pub(crate) query_fingerprint: Option<String>,
    pub(crate) index_generation: u64,
    pub(crate) production: bool,
    pub(crate) step_outcome: StepOutcome,
    pub(crate) skills: Vec<ParentSkillBinding>,
    pub(crate) capability_denials: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ParentBindingError {
    #[error("worker event shape is invalid")]
    InvalidShape,
    #[error("worker event kind is not execution-observable")]
    ForbiddenKind,
    #[error("worker event attribution does not match the parent snapshot")]
    AttributionMismatch,
    #[error("worker execution evidence is structurally incomplete")]
    IncompleteEvidence,
    #[error("parent event batch exceeds the configured bound")]
    BatchTooLarge,
}

/// Validate untrusted worker observations and rebuild a canonical batch from
/// parent-owned identity and policy state.
///
/// A selected artifact must have exactly one worker-observed injection. Every
/// invocation must use the deterministic parent turn/tool/artifact/export
/// mapping and have exactly one terminal observation. Selection,
/// user-feedback, observability, and capability-policy events are parent-owned
/// and can never be emitted by the worker.
pub(crate) fn bind_worker_events(
    context: &ParentTelemetryContext,
    worker_events: &[SkillEvent],
) -> Result<EventBatch, ParentBindingError> {
    if worker_events.len() > MAX_EVENT_BATCH {
        return Err(ParentBindingError::BatchTooLarge);
    }

    let selected = context
        .skills
        .iter()
        .map(|skill| (skill.skill_id.as_str(), skill))
        .collect::<BTreeMap<_, _>>();
    if selected.len() != context.skills.len() {
        return Err(ParentBindingError::AttributionMismatch);
    }
    if selected.is_empty() {
        return if worker_events.is_empty() {
            EventBatch::new(Vec::new()).map_err(|_| ParentBindingError::InvalidShape)
        } else {
            Err(ParentBindingError::AttributionMismatch)
        };
    }

    let mut injected = BTreeSet::new();
    let mut ordinals = BTreeMap::<(String, String), u32>::new();
    let mut open_invocations = BTreeMap::<String, (String, String)>::new();
    let mut canonical_worker_events = Vec::with_capacity(worker_events.len());
    let mut capability_denials = context.capability_denials.clone();
    let created_at = current_timestamp().map_err(|_| ParentBindingError::InvalidShape)?;

    for claim in worker_events {
        claim
            .validate()
            .map_err(|_| ParentBindingError::InvalidShape)?;
        if claim.turn_id != context.turn_id
            || claim.tool_call_id.as_deref() != Some(context.tool_call_id.as_str())
        {
            return Err(ParentBindingError::AttributionMismatch);
        }
        let Some(binding) = selected.get(claim.skill_id.as_str()) else {
            return Err(ParentBindingError::AttributionMismatch);
        };

        match claim.kind {
            SkillEventKind::Injected => {
                if claim.invocation_id.is_some()
                    || claim.export_name.is_some()
                    || claim.outcome.is_some()
                    || claim.latency_us.is_some()
                    || claim.argument_shape.is_some()
                    || !injected.insert(binding.skill_id.clone())
                {
                    return Err(ParentBindingError::InvalidShape);
                }
            }
            SkillEventKind::Invoked => {
                let export_name = claim
                    .export_name
                    .as_deref()
                    .ok_or(ParentBindingError::InvalidShape)?;
                if !binding.exports.contains(export_name)
                    || claim.outcome.is_some()
                    || claim.latency_us.is_some()
                {
                    return Err(ParentBindingError::AttributionMismatch);
                }
                let ordinal = ordinals
                    .entry((binding.skill_id.clone(), export_name.to_string()))
                    .or_default();
                let expected = stable_invocation_id(
                    &context.turn_id,
                    &context.tool_call_id,
                    &binding.skill_id,
                    export_name,
                    *ordinal,
                );
                *ordinal = ordinal
                    .checked_add(1)
                    .ok_or(ParentBindingError::IncompleteEvidence)?;
                if claim.invocation_id.as_deref() != Some(expected.as_str())
                    || open_invocations
                        .insert(
                            expected,
                            (binding.skill_id.clone(), export_name.to_string()),
                        )
                        .is_some()
                {
                    return Err(ParentBindingError::AttributionMismatch);
                }
            }
            SkillEventKind::Returned | SkillEventKind::Threw => {
                validate_terminal_claim(claim, binding, &mut open_invocations)?;
            }
            SkillEventKind::TimedOut => {
                if context.step_outcome != StepOutcome::Timeout {
                    return Err(ParentBindingError::AttributionMismatch);
                }
                validate_terminal_claim(claim, binding, &mut open_invocations)?;
            }
            SkillEventKind::Oom => {
                if context.step_outcome != StepOutcome::OutOfMemory {
                    return Err(ParentBindingError::AttributionMismatch);
                }
                validate_terminal_claim(claim, binding, &mut open_invocations)?;
            }
            SkillEventKind::Selected
            | SkillEventKind::CapabilityDenied
            | SkillEventKind::UserPositive
            | SkillEventKind::UserNegative
            | SkillEventKind::ObservabilityLost => {
                return Err(ParentBindingError::ForbiddenKind);
            }
        }

        let mut canonical = canonical_event(claim, binding, context, created_at, true);
        if claim.kind.is_terminal()
            && claim
                .invocation_id
                .as_ref()
                .is_some_and(|id| capability_denials.remove(id))
        {
            canonical.kind = SkillEventKind::CapabilityDenied;
            canonical.outcome = Some("capability_policy".to_string());
        }
        canonical_worker_events.push(canonical);
    }

    let expected_injections = selected.keys().copied().collect::<BTreeSet<_>>();
    let observed_injections = injected.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if observed_injections != expected_injections
        || !open_invocations.is_empty()
        || !capability_denials.is_empty()
    {
        return Err(ParentBindingError::IncompleteEvidence);
    }

    let mut canonical = Vec::with_capacity(context.skills.len() + canonical_worker_events.len());
    for binding in &context.skills {
        canonical.push(parent_lifecycle_event(
            binding,
            context,
            SkillEventKind::Selected,
            created_at,
            true,
        ));
    }
    canonical.extend(canonical_worker_events);
    EventBatch::new(canonical).map_err(|error| match error {
        TelemetryError::BatchTooLarge => ParentBindingError::BatchTooLarge,
        _ => ParentBindingError::InvalidShape,
    })
}

fn validate_terminal_claim(
    claim: &SkillEvent,
    binding: &ParentSkillBinding,
    open_invocations: &mut BTreeMap<String, (String, String)>,
) -> Result<(), ParentBindingError> {
    if claim.argument_shape.is_some() || claim.outcome.is_none() {
        return Err(ParentBindingError::InvalidShape);
    }
    let invocation_id = claim
        .invocation_id
        .as_deref()
        .ok_or(ParentBindingError::InvalidShape)?;
    let export_name = claim
        .export_name
        .as_deref()
        .ok_or(ParentBindingError::InvalidShape)?;
    match open_invocations.remove(invocation_id) {
        Some((skill_id, export))
            if skill_id == binding.skill_id
                && export == export_name
                && binding.exports.contains(export_name) =>
        {
            Ok(())
        }
        _ => Err(ParentBindingError::AttributionMismatch),
    }
}

fn canonical_event(
    claim: &SkillEvent,
    binding: &ParentSkillBinding,
    context: &ParentTelemetryContext,
    created_at: i64,
    evidence_complete: bool,
) -> SkillEvent {
    SkillEvent {
        invocation_id: claim.invocation_id.clone(),
        skill_id: binding.skill_id.clone(),
        turn_id: context.turn_id.clone(),
        tool_call_id: Some(context.tool_call_id.clone()),
        kind: claim.kind,
        export_name: claim.export_name.clone(),
        outcome: claim.outcome.clone(),
        latency_us: claim.latency_us,
        retrieval_score: Some(binding.retrieval_score),
        retrieval_rank: Some(binding.retrieval_rank),
        query_fingerprint: context.query_fingerprint.clone(),
        index_generation: context.index_generation,
        evidence_complete,
        production: context.production,
        argument_shape: claim.argument_shape.clone(),
        created_at,
    }
}

fn parent_lifecycle_event(
    binding: &ParentSkillBinding,
    context: &ParentTelemetryContext,
    kind: SkillEventKind,
    created_at: i64,
    evidence_complete: bool,
) -> SkillEvent {
    SkillEvent {
        invocation_id: None,
        skill_id: binding.skill_id.clone(),
        turn_id: context.turn_id.clone(),
        tool_call_id: Some(context.tool_call_id.clone()),
        kind,
        export_name: None,
        outcome: None,
        latency_us: None,
        retrieval_score: Some(binding.retrieval_score),
        retrieval_rank: Some(binding.retrieval_rank),
        query_fingerprint: context.query_fingerprint.clone(),
        index_generation: context.index_generation,
        evidence_complete,
        production: context.production,
        argument_shape: None,
        created_at,
    }
}

pub(crate) fn observability_lost_batch(
    context: &ParentTelemetryContext,
) -> Result<EventBatch, ParentBindingError> {
    let created_at = current_timestamp().map_err(|_| ParentBindingError::InvalidShape)?;
    EventBatch::new(
        context
            .skills
            .iter()
            .map(|binding| {
                parent_lifecycle_event(
                    binding,
                    context,
                    SkillEventKind::ObservabilityLost,
                    created_at,
                    false,
                )
            })
            .collect(),
    )
    .map_err(|error| match error {
        TelemetryError::BatchTooLarge => ParentBindingError::BatchTooLarge,
        _ => ParentBindingError::InvalidShape,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestionReport {
    pub inserted: usize,
    pub replayed: usize,
    pub evidence_complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("event batch exceeds the configured bound")]
    BatchTooLarge,
    #[error("event is missing its stable invocation ID")]
    MissingInvocationId,
    #[error("one invocation has multiple terminal direct outcomes")]
    MultipleTerminalOutcomes,
    #[error("coarse argument shape exceeds the configured bound")]
    ArgumentShapeTooLarge,
    #[error("invalid or sensitive event shape")]
    InvalidEvent,
    #[error("an idempotent event retry changed immutable fields")]
    IdempotencyConflict,
}

impl TelemetryError {
    fn is_sqlite_busy(&self) -> bool {
        matches!(
            self,
            Self::Sqlite(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                )
        )
    }
}

/// Stable invocation identity for acknowledged tool-call retries.
pub fn stable_invocation_id(
    turn_id: &str,
    tool_call_id: &str,
    skill_id: &str,
    export_name: &str,
    ordinal: u32,
) -> String {
    let mut bytes = Vec::new();
    for part in [turn_id, tool_call_id, skill_id, export_name] {
        bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
        bytes.extend_from_slice(part.as_bytes());
    }
    bytes.extend_from_slice(&ordinal.to_be_bytes());
    let digest = Sha256::digest([b"mini-agent/skill-invocation/v1".as_slice(), &bytes].concat());
    hex::encode_lower(digest.as_slice())
}

pub struct TelemetryIngestor<'a> {
    store: &'a mut SkillStore,
}

pub struct TelemetryDispatcher {
    tx: Option<SyncSender<TelemetryCommand>>,
    #[cfg(test)]
    test_batch_tx: Option<SyncSender<EventBatch>>,
    observability_lost: Arc<AtomicU64>,
    busy_retries: Arc<AtomicU64>,
    shutdown: TelemetryShutdown,
    shutdown_budget: Duration,
    join: Option<std::thread::JoinHandle<()>>,
    runtime: Option<tokio::runtime::Handle>,
}

enum TelemetryCommand {
    Events(EventBatch),
    TaskOutcome(super::policy::TaskOutcomeEvidence),
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(transparent)]
    Store(#[from] super::store::StoreError),
    #[error("telemetry queue is saturated")]
    Saturated,
    #[error("telemetry worker is unavailable")]
    Disconnected,
}

impl TelemetryDispatcher {
    /// Spawn the bounded off-JS-thread SQLite ingestion worker.
    // Test-only. This builds a dispatcher WITHOUT an index coordinator, so its worker
    // silently skips apply_automatic_quarantine. Production must use
    // spawn_session_scoped_with_coordinator; do not un-gate this for production use.
    #[cfg(test)]
    pub fn spawn(paths: &crate::paths::AppPaths) -> Result<Self, DispatchError> {
        Self::spawn_inner(paths, None, crate::agent::runner::current_work_guard())
    }

    pub(crate) fn spawn_session_scoped_with_coordinator(
        paths: &crate::paths::AppPaths,
        coordinator: std::sync::Arc<super::coordinator::IndexCoordinator>,
    ) -> Result<Self, DispatchError> {
        Self::spawn_inner(paths, Some(coordinator), None)
    }

    fn spawn_inner(
        paths: &crate::paths::AppPaths,
        coordinator: Option<std::sync::Arc<super::coordinator::IndexCoordinator>>,
        work_guard: Option<crate::agent::runner::AgentWorkGuard>,
    ) -> Result<Self, DispatchError> {
        let store = SkillStore::open_at(paths)?;
        Self::spawn_with_store(store, coordinator, work_guard)
    }

    fn spawn_with_store(
        mut store: SkillStore,
        coordinator: Option<std::sync::Arc<super::coordinator::IndexCoordinator>>,
        work_guard: Option<crate::agent::runner::AgentWorkGuard>,
    ) -> Result<Self, DispatchError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(TELEMETRY_QUEUE_CAPACITY);
        let observability_lost = Arc::new(AtomicU64::new(0));
        let worker_observability_lost = Arc::clone(&observability_lost);
        let busy_retries = Arc::new(AtomicU64::new(0));
        let worker_busy_retries = Arc::clone(&busy_retries);
        let shutdown = TelemetryShutdown::default();
        let worker_shutdown = shutdown.clone();
        let join = std::thread::Builder::new()
            .name("skill-telemetry".into())
            .spawn(move || {
                let _work_guard = work_guard;
                while let Ok(command) = rx.recv() {
                    let TelemetryCommand::Events(batch) = command else {
                        if let TelemetryCommand::TaskOutcome(outcome) = command
                            && ingest_task_outcome(&mut store, &outcome).is_err()
                        {
                            worker_observability_lost.fetch_add(1, Ordering::Relaxed);
                            tracing::error!(
                                "skill task-outcome ingestion failed; evidence was excluded"
                            );
                        }
                        continue;
                    };
                    match ingest_retrying_busy(
                        &mut store,
                        &batch,
                        &worker_busy_retries,
                        &worker_shutdown,
                    ) {
                        Ok(report) if report.evidence_complete => {
                            if let Some(coordinator) = &coordinator {
                                apply_automatic_quarantine(&mut store, coordinator, &batch);
                            }
                            compact_expired_events(&mut store);
                        }
                        Ok(_) => compact_expired_events(&mut store),
                        Err(error) => {
                            worker_observability_lost.fetch_add(1, Ordering::Relaxed);
                            // Never include event payloads in diagnostics.
                            tracing::error!(
                                error = %error,
                                event_count = batch.events().len(),
                                "skill telemetry ingestion failed; evidence was excluded"
                            );
                        }
                    }
                }
            })
            .map_err(|_| DispatchError::Disconnected)?;
        Ok(Self {
            tx: Some(tx),
            #[cfg(test)]
            test_batch_tx: None,
            observability_lost,
            busy_retries,
            shutdown,
            shutdown_budget: TELEMETRY_SHUTDOWN_FLUSH_BUDGET,
            join: Some(join),
            runtime: tokio::runtime::Handle::try_current().ok(),
        })
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_busy_timeout_for_test(
        paths: &crate::paths::AppPaths,
        busy_timeout: Duration,
    ) -> Result<Self, DispatchError> {
        let store = SkillStore::open_at(paths)?;
        store
            .connection()
            .busy_timeout(busy_timeout)
            .map_err(super::store::StoreError::from)?;
        Self::spawn_with_store(store, None, None)
    }

    pub fn try_dispatch(&self, batch: EventBatch) -> Result<(), DispatchError> {
        #[cfg(test)]
        if let Some(tx) = &self.test_batch_tx {
            return tx.try_send(batch).map_err(|error| match error {
                TrySendError::Full(_) => DispatchError::Saturated,
                TrySendError::Disconnected(_) => DispatchError::Disconnected,
            });
        }
        self.tx
            .as_ref()
            .ok_or(DispatchError::Disconnected)?
            .try_send(TelemetryCommand::Events(batch))
            .map_err(|error| match error {
                TrySendError::Full(_) => DispatchError::Saturated,
                TrySendError::Disconnected(_) => DispatchError::Disconnected,
            })
    }

    pub(crate) fn record_task_outcome(
        &self,
        outcome: super::policy::TaskOutcomeEvidence,
    ) -> Result<(), DispatchError> {
        outcome
            .validate()
            .map_err(|_| DispatchError::Disconnected)?;
        self.tx
            .as_ref()
            .ok_or(DispatchError::Disconnected)?
            .try_send(TelemetryCommand::TaskOutcome(outcome))
            .map_err(|error| match error {
                TrySendError::Full(_) => DispatchError::Saturated,
                TrySendError::Disconnected(_) => DispatchError::Disconnected,
            })
    }
}

/// Durable column encoding for a task-outcome source.
///
/// `gate_skipped` is its own token, distinct from `no_verify_command`: a turn
/// that ran under a *configured* verify command and simply never touched the
/// workspace is a different audit fact from one that could never have been
/// gated at all. Neither carries a pass/fail signal, and
/// `policy::evaluate_promotion_with_task_outcomes` excludes both.
/// `lifecycle.rs` holds the inverse mapping; the two must stay in step.
fn task_outcome_source_columns(
    source: &super::policy::TaskOutcomeSource,
) -> (&'static str, Option<&str>) {
    use super::policy::TaskOutcomeSource;
    match source {
        TaskOutcomeSource::VerifyCommand(id) => ("verify_command", Some(id.as_str())),
        TaskOutcomeSource::Oracle(id) => ("oracle", Some(id.as_str())),
        TaskOutcomeSource::NoVerifyCommand => ("no_verify_command", None),
        TaskOutcomeSource::GateSkipped => ("gate_skipped", None),
    }
}

#[cfg(test)]
pub(crate) fn task_outcome_source_columns_for_test(
    source: &super::policy::TaskOutcomeSource,
) -> (&'static str, Option<&str>) {
    task_outcome_source_columns(source)
}

fn ingest_task_outcome(
    store: &mut SkillStore,
    outcome: &super::policy::TaskOutcomeEvidence,
) -> Result<(), TelemetryError> {
    outcome
        .validate()
        .map_err(|_| TelemetryError::InvalidEvent)?;
    let tx = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (source_kind, source_id) = task_outcome_source_columns(&outcome.source);
    let mut attributed_skills = Vec::new();
    for skill_id in &outcome.skill_ids {
        let invoked: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM skill_events
                 WHERE skill_id = ?1 AND turn_id = ?2 AND event_kind = 'invoked'
                   AND production = ?3 AND evidence_complete = 1
             )",
            params![skill_id, outcome.turn_id, i64::from(outcome.production)],
            |row| row.get(0),
        )?;
        if !invoked {
            continue;
        }
        attributed_skills.push(skill_id);
    }
    let canonical = serde_json::to_vec(&serde_json::json!({
        "version": 2,
        "turn_id": outcome.turn_id,
        "verify_passed": outcome.verify_passed,
        "attempt": outcome.attempt,
        "source_kind": source_kind,
        "source_id": source_id,
        "production": outcome.production,
        "created_at": outcome.created_at,
    }))
    .map_err(|_| TelemetryError::InvalidEvent)?;
    let evidence_id = crate::hex::encode_lower(Sha256::digest(canonical));
    // A turn that recorded fewer invocation links than the parent selected is
    // not by itself incomplete — selected-but-unused skills are ordinary. The
    // parent's own completeness bit is what distinguishes a verified
    // no-invocation turn from one whose telemetry was lost.
    tx.execute(
        "INSERT OR IGNORE INTO skill_task_outcomes (
             evidence_id, turn_id, verify_passed, attempt,
             source_kind, source_id, production, evidence_complete, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            evidence_id,
            outcome.turn_id,
            i64::from(outcome.verify_passed),
            i64::from(outcome.attempt),
            source_kind,
            source_id,
            i64::from(outcome.production),
            i64::from(outcome.evidence_complete),
            outcome.created_at,
        ],
    )?;
    for skill_id in attributed_skills {
        tx.execute(
            "INSERT OR IGNORE INTO skill_task_outcome_links (evidence_id, skill_id)
             VALUES (?1, ?2)",
            params![evidence_id, skill_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn ingest_retrying_busy(
    store: &mut SkillStore,
    batch: &EventBatch,
    busy_retries: &AtomicU64,
    shutdown: &TelemetryShutdown,
) -> Result<IngestionReport, TelemetryError> {
    let mut delay = TELEMETRY_BUSY_RETRY_INITIAL;
    let mut attempts = 0u64;
    loop {
        match TelemetryIngestor::new(store).ingest(batch) {
            Err(error) if error.is_sqlite_busy() => {
                busy_retries.fetch_add(1, Ordering::Relaxed);
                attempts = attempts.saturating_add(1);
                // Retain the batch in this worker and retry with capped backoff.
                // Log only exponentially spaced attempts, and never payloads.
                if attempts.is_power_of_two() {
                    tracing::warn!(
                        event_count = batch.events().len(),
                        attempts,
                        retry_delay_ms = delay.as_millis(),
                        "skill telemetry store is busy; retaining batch for retry"
                    );
                }
                // A healthy session retries until the writer frees the store.
                // After shutdown the retry is bounded: an external writer that
                // keeps the lock must not hold the process open, so the batch
                // is given up and recorded as unrecoverable evidence loss.
                let sleep = match shutdown.remaining() {
                    Some(remaining) if remaining.is_zero() => {
                        tracing::warn!(
                            event_count = batch.events().len(),
                            attempts,
                            "skill telemetry shutdown budget elapsed while the store was busy"
                        );
                        return Err(error);
                    }
                    Some(remaining) => delay.min(remaining),
                    None => delay,
                };
                std::thread::sleep(sleep);
                delay = delay.saturating_mul(2).min(TELEMETRY_BUSY_RETRY_MAX);
            }
            result => return result,
        }
    }
}

fn compact_expired_events(store: &mut SkillStore) {
    let Ok(now) = current_timestamp() else {
        return;
    };
    let cutoff = now.saturating_sub(super::retention::DEFAULT_RAW_RETENTION_SECONDS);
    if let Err(error) =
        super::retention::RetentionService::new(store).compact_before(cutoff, 1, now)
    {
        tracing::warn!(error = %error, "automatic skill telemetry compaction failed");
    }
}

fn behavioral_window_counts(
    store: &SkillStore,
    skill_id: &str,
    window_end: i64,
) -> Result<(i64, i64), rusqlite::Error> {
    let window_start = window_end.saturating_sub(super::retention::DEFAULT_RAW_RETENTION_SECONDS);
    // Timeouts, OOMs and capability denials are faults on their own. A thrown
    // exception *or* an ordinary wrong result counts only when active
    // authenticated negative or severe feedback targets that exact invocation
    // (docs/specs/phase-5-evidence-learning.md section 8).
    store.connection().query_row(
        "SELECT COUNT(*), COALESCE(SUM(
             terminal.event_kind IN ('timed_out','oom','capability_denied')
             OR (
                 terminal.event_kind IN ('threw', 'returned')
                 AND EXISTS (
                     SELECT 1 FROM skill_feedback AS feedback
                      WHERE feedback.skill_id = terminal.skill_id
                        AND feedback.invocation_id = terminal.invocation_id
                        AND feedback.feedback_kind IN ('negative', 'severe')
                        AND feedback.state = 'active'
                 )
             )
         ), 0)
         FROM skill_events AS invoked
         JOIN skill_events AS terminal
           ON terminal.invocation_id = invoked.invocation_id
          AND terminal.skill_id = invoked.skill_id
          AND terminal.event_kind IN (
              'returned','threw','timed_out','oom','capability_denied'
          )
         WHERE invoked.skill_id = ?1
           AND invoked.event_kind = 'invoked'
           AND invoked.production = 1 AND invoked.evidence_complete = 1
           AND terminal.production = 1 AND terminal.evidence_complete = 1
           AND invoked.created_at >= ?2 AND invoked.created_at <= ?3
           AND terminal.created_at >= ?2 AND terminal.created_at <= ?3",
        params![skill_id, window_start, window_end],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

#[cfg(test)]
pub(crate) fn behavioral_window_counts_for_test(
    store: &SkillStore,
    skill_id: &str,
    window_end: i64,
) -> Result<(i64, i64), rusqlite::Error> {
    behavioral_window_counts(store, skill_id, window_end)
}

impl Drop for TelemetryDispatcher {
    fn drop(&mut self) {
        // Closing the queue cannot interrupt a busy-retry loop already in
        // flight, so shutdown is signalled explicitly with a bounded flush
        // budget before the join.
        self.shutdown.request(self.shutdown_budget);
        self.tx.take();
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

impl TelemetryDispatcher {
    /// Record a parent-owned signal even when the bounded telemetry queue is
    /// saturated or disconnected and therefore cannot persist an event.
    pub(crate) fn record_observability_lost(&self, reason: &'static str) {
        self.observability_lost.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            event_kind = SkillEventKind::ObservabilityLost.as_token(),
            reason,
            "skill telemetry observability was lost; positive evidence was excluded"
        );
    }

    /// Shorten the post-shutdown flush budget. Used by tests that hold the
    /// SQLite write lock past teardown on purpose.
    #[cfg(test)]
    pub(crate) fn set_shutdown_budget_for_test(&mut self, budget: Duration) {
        self.shutdown_budget = budget;
    }

    #[cfg(test)]
    pub(crate) fn from_sender_for_test(tx: SyncSender<EventBatch>) -> Self {
        Self {
            tx: None,
            test_batch_tx: Some(tx),
            observability_lost: Arc::new(AtomicU64::new(0)),
            busy_retries: Arc::new(AtomicU64::new(0)),
            shutdown: TelemetryShutdown::default(),
            shutdown_budget: TELEMETRY_SHUTDOWN_FLUSH_BUDGET,
            join: None,
            runtime: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn observability_lost_for_test(&self) -> u64 {
        self.observability_lost.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn busy_retries_for_test(&self) -> u64 {
        self.busy_retries.load(Ordering::Relaxed)
    }
}

fn apply_automatic_quarantine(
    store: &mut SkillStore,
    coordinator: &super::coordinator::IndexCoordinator,
    batch: &EventBatch,
) {
    use super::lifecycle::LifecycleStatus;
    use super::quarantine::{
        QuarantineEvidence, QuarantineExecutor, QuarantinePolicy, QuarantineReason,
    };

    let skill_ids = batch
        .events()
        .iter()
        .filter(|event| event.production && event.kind.is_terminal())
        .map(|event| event.skill_id.clone())
        .collect::<BTreeSet<_>>();
    for skill_id in skill_ids {
        let revision = store.connection().query_row(
            "SELECT status, row_version FROM skill_revisions WHERE id = ?",
            [&skill_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        );
        let Ok((status, row_version)) = revision else {
            continue;
        };
        let Some(status) = LifecycleStatus::from_token(&status) else {
            continue;
        };
        if !matches!(status, LifecycleStatus::Canary | LifecycleStatus::Active) {
            continue;
        }
        let severe = batch.events().iter().find(|event| {
            event.skill_id == skill_id
                && event.production
                && event.evidence_complete
                && matches!(
                    event.kind,
                    SkillEventKind::CapabilityDenied
                        | SkillEventKind::TimedOut
                        | SkillEventKind::Oom
                )
        });
        let Ok(now) = current_timestamp() else {
            continue;
        };
        let stats = behavioral_window_counts(store, &skill_id, now);
        let (invocations, failures) = stats.unwrap_or((0, 0));
        let reason = match severe.map(|event| event.kind) {
            Some(SkillEventKind::CapabilityDenied) => QuarantineReason::CapabilityPolicyFault,
            Some(SkillEventKind::TimedOut) if status == LifecycleStatus::Canary => {
                QuarantineReason::CanaryTimeout
            }
            Some(SkillEventKind::Oom) if status == LifecycleStatus::Canary => {
                QuarantineReason::CanaryOom
            }
            _ if status == LifecycleStatus::Active && invocations >= 20 && failures >= 5 => {
                QuarantineReason::BehavioralFailureRate
            }
            _ => continue,
        };
        let Ok(generation) = store.generation_state() else {
            continue;
        };
        let evidence = QuarantineEvidence {
            skill_id: skill_id.clone(),
            reason,
            qualified_invocations: usize::try_from(invocations).unwrap_or(0),
            direct_failures: usize::try_from(failures).unwrap_or(0),
            evidence_complete: true,
            authenticated_feedback: false,
            feedback_marked_severe: false,
            row_version_current: true,
            generation_current: generation.desired_generation == generation.applied_generation,
        };
        let policy = QuarantinePolicy::conservative("phase5-quarantine-v1");
        // A safety-triggered decision is retried against freshly re-read
        // revision and generation state, so an optimistic conflict with a
        // concurrent lifecycle write cannot silently drop it. Attempts are
        // bounded: a permanently ineligible revision must not spin.
        let immediate = reason.is_immediate();
        let attempts = if immediate {
            QUARANTINE_SAFETY_ATTEMPTS
        } else {
            1
        };
        let mut status = status;
        let mut row_version = row_version;
        let mut evidence = evidence;
        let mut desired_generation = generation.desired_generation;
        for attempt in 1..=attempts {
            match QuarantineExecutor::new(coordinator).apply(
                &policy,
                &evidence,
                status,
                row_version,
                desired_generation as i64,
                now,
            ) {
                Ok(_) => break,
                Err(error) => {
                    let retryable = immediate && attempt < attempts;
                    tracing::warn!(
                        skill_id = %skill_id,
                        error = %error,
                        attempt,
                        retryable,
                        "automatic skill quarantine held or failed"
                    );
                    if !retryable {
                        break;
                    }
                    // Re-read the revision and generation the decision applies
                    // to; a concurrent transition may have moved either.
                    let Ok((current_status, current_row_version)) = store.connection().query_row(
                        "SELECT status, row_version FROM skill_revisions WHERE id = ?",
                        [&skill_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    ) else {
                        break;
                    };
                    let Some(current_status) = LifecycleStatus::from_token(&current_status) else {
                        break;
                    };
                    if !matches!(
                        current_status,
                        LifecycleStatus::Canary | LifecycleStatus::Active
                    ) {
                        // Already quarantined or retired by another writer.
                        break;
                    }
                    let Ok(current_generation) = store.generation_state() else {
                        break;
                    };
                    status = current_status;
                    row_version = current_row_version;
                    desired_generation = current_generation.desired_generation;
                    evidence.row_version_current = true;
                    evidence.generation_current = current_generation.desired_generation
                        == current_generation.applied_generation;
                }
            }
        }
    }
}

impl<'a> TelemetryIngestor<'a> {
    pub fn new(store: &'a mut SkillStore) -> Self {
        Self { store }
    }

    pub fn ingest(&mut self, batch: &EventBatch) -> Result<IngestionReport, TelemetryError> {
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_against_durable_terminals(&tx, batch)?;

        let mut inserted = 0;
        let mut replayed = 0;
        let mut evidence_complete = true;
        for event in batch.events() {
            evidence_complete &=
                event.evidence_complete && event.kind != SkillEventKind::ObservabilityLost;
            if insert_event(&tx, event)? {
                inserted += 1;
                update_stats(&tx, event)?;
            } else {
                replayed += 1;
            }
        }
        tx.commit()?;
        Ok(IngestionReport {
            inserted,
            replayed,
            evidence_complete,
        })
    }
}

fn validate_against_durable_terminals(
    tx: &Transaction<'_>,
    batch: &EventBatch,
) -> Result<(), TelemetryError> {
    let mut checked = BTreeSet::new();
    for event in batch
        .events()
        .iter()
        .filter(|event| event.kind.is_terminal())
    {
        let invocation = event
            .invocation_id
            .as_deref()
            .ok_or(TelemetryError::MissingInvocationId)?;
        if !checked.insert(invocation) {
            return Err(TelemetryError::MultipleTerminalOutcomes);
        }
        let durable: Option<String> = tx
            .query_row(
                "SELECT event_kind FROM skill_events
                 WHERE invocation_id = ?
                   AND event_kind IN (
                     'returned','threw','timed_out','oom','capability_denied'
                   )",
                [invocation],
                |row| row.get(0),
            )
            .optional()?;
        if durable.is_some_and(|kind| kind != event.kind.as_token()) {
            return Err(TelemetryError::MultipleTerminalOutcomes);
        }
    }
    Ok(())
}

fn insert_event(tx: &Transaction<'_>, event: &SkillEvent) -> Result<bool, TelemetryError> {
    let changed = tx.execute(
        "INSERT OR IGNORE INTO skill_events (
            invocation_id, skill_id, turn_id, tool_call_id, event_kind,
            export_name, outcome, latency_us, retrieval_score, retrieval_rank,
            query_fingerprint, index_generation, evidence_complete, production,
            argument_shape, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            event.invocation_id,
            event.skill_id,
            event.turn_id,
            event.tool_call_id,
            event.kind.as_token(),
            event.export_name,
            event.outcome,
            event.latency_us.map(|value| value as i64),
            event.retrieval_score,
            event.retrieval_rank.map(i64::from),
            event.query_fingerprint,
            event.index_generation as i64,
            i64::from(event.evidence_complete),
            i64::from(event.production),
            event.argument_shape,
            event.created_at,
        ],
    )?;
    if changed == 1 {
        return Ok(true);
    }

    let exact_replay: bool = tx.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM skill_events
            WHERE invocation_id IS ?
              AND skill_id = ? AND turn_id = ? AND tool_call_id IS ?
              AND event_kind = ? AND export_name IS ? AND outcome IS ?
              AND latency_us IS ? AND retrieval_score IS ?
              AND retrieval_rank IS ? AND query_fingerprint IS ?
              AND index_generation = ? AND evidence_complete = ?
              AND production = ? AND argument_shape IS ? AND created_at = ?
         )",
        params![
            event.invocation_id,
            event.skill_id,
            event.turn_id,
            event.tool_call_id,
            event.kind.as_token(),
            event.export_name,
            event.outcome,
            event.latency_us.map(|value| value as i64),
            event.retrieval_score,
            event.retrieval_rank.map(i64::from),
            event.query_fingerprint,
            event.index_generation as i64,
            i64::from(event.evidence_complete),
            i64::from(event.production),
            event.argument_shape,
            event.created_at,
        ],
        |row| row.get(0),
    )?;
    if exact_replay {
        return Ok(false);
    }
    Err(TelemetryError::IdempotencyConflict)
}

fn update_stats(tx: &Transaction<'_>, event: &SkillEvent) -> Result<(), TelemetryError> {
    let selected = i64::from(event.kind == SkillEventKind::Selected);
    let invoked = i64::from(event.kind == SkillEventKind::Invoked);
    let success = i64::from(event.kind == SkillEventKind::Returned);
    let failure = i64::from(matches!(
        event.kind,
        SkillEventKind::Threw
            | SkillEventKind::TimedOut
            | SkillEventKind::Oom
            | SkillEventKind::CapabilityDenied
    ));
    let timeout = i64::from(event.kind == SkillEventKind::TimedOut);
    let oom = i64::from(event.kind == SkillEventKind::Oom);
    let policy_fault = i64::from(event.kind == SkillEventKind::CapabilityDenied);
    let positive = i64::from(event.kind == SkillEventKind::UserPositive);
    let negative = i64::from(event.kind == SkillEventKind::UserNegative);
    let latency = event.latency_us.unwrap_or(0) as i64;

    tx.execute(
        "INSERT INTO skill_stats (
            skill_id, selected_count, invoked_count, direct_success_count,
            direct_failure_count, timeout_count, oom_count, policy_fault_count,
            user_positive_count, user_negative_count, latency_total_us, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(skill_id) DO UPDATE SET
            selected_count = selected_count + excluded.selected_count,
            invoked_count = invoked_count + excluded.invoked_count,
            direct_success_count =
                direct_success_count + excluded.direct_success_count,
            direct_failure_count =
                direct_failure_count + excluded.direct_failure_count,
            timeout_count = timeout_count + excluded.timeout_count,
            oom_count = oom_count + excluded.oom_count,
            policy_fault_count =
                policy_fault_count + excluded.policy_fault_count,
            user_positive_count =
                user_positive_count + excluded.user_positive_count,
            user_negative_count =
                user_negative_count + excluded.user_negative_count,
            latency_total_us = latency_total_us + excluded.latency_total_us,
            updated_at = MAX(updated_at, excluded.updated_at)",
        params![
            event.skill_id,
            selected,
            invoked,
            success,
            failure,
            timeout,
            oom,
            policy_fault,
            positive,
            negative,
            latency,
            event.created_at,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod task_outcome_source_encoding_tests {
    use super::super::policy::TaskOutcomeSource;
    use super::task_outcome_source_columns;

    /// v98t: a skipped gate under a configured verify command is its own
    /// durable fact. Encoding it as `no_verify_command` would misattribute the
    /// reason in every audit built from task-outcome evidence.
    #[test]
    fn a_skipped_gate_encodes_to_its_own_token() {
        assert_eq!(
            task_outcome_source_columns(&TaskOutcomeSource::GateSkipped),
            ("gate_skipped", None)
        );
        assert_eq!(
            task_outcome_source_columns(&TaskOutcomeSource::NoVerifyCommand),
            ("no_verify_command", None)
        );
        assert_ne!(
            task_outcome_source_columns(&TaskOutcomeSource::GateSkipped).0,
            task_outcome_source_columns(&TaskOutcomeSource::NoVerifyCommand).0
        );
        // Neither skip reason may be laundered into a source that promotion
        // counts as evidence.
        assert_ne!(
            task_outcome_source_columns(&TaskOutcomeSource::GateSkipped).0,
            task_outcome_source_columns(&TaskOutcomeSource::Oracle("o".into())).0
        );
        assert_ne!(
            task_outcome_source_columns(&TaskOutcomeSource::GateSkipped).0,
            task_outcome_source_columns(&TaskOutcomeSource::VerifyCommand("v".into())).0
        );
    }
}
