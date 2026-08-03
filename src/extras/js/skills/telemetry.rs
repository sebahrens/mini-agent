//! Privacy-bounded, idempotent directly attributed skill telemetry.
//!
//! QuickJS wrappers build [`SkillEvent`] values in memory. The tokio side hands
//! bounded batches to [`TelemetryIngestor`], so the JS thread never blocks on
//! SQLite. No API in this module accepts prompts, source, raw arguments, file
//! contents, model output, or environment values.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{SyncSender, TrySendError};

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{SkillStore, current_timestamp};

pub const MAX_EVENT_BATCH: usize = 256;
pub const MAX_ARGUMENT_SHAPE_BYTES: usize = 512;
pub const TELEMETRY_QUEUE_CAPACITY: usize = 64;
pub const MAX_EVENT_ID_BYTES: usize = 256;
pub const MAX_EVENT_TOKEN_BYTES: usize = 128;

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
    format!("{digest:x}")
}

pub struct TelemetryIngestor<'a> {
    store: &'a mut SkillStore,
}

pub struct TelemetryDispatcher {
    tx: Option<SyncSender<EventBatch>>,
    join: Option<std::thread::JoinHandle<()>>,
    runtime: Option<tokio::runtime::Handle>,
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
    pub fn spawn(paths: &crate::paths::AppPaths) -> Result<Self, DispatchError> {
        Self::spawn_inner(paths, None)
    }

    pub fn spawn_with_coordinator(
        paths: &crate::paths::AppPaths,
        coordinator: std::sync::Arc<super::coordinator::IndexCoordinator>,
    ) -> Result<Self, DispatchError> {
        Self::spawn_inner(paths, Some(coordinator))
    }

    fn spawn_inner(
        paths: &crate::paths::AppPaths,
        coordinator: Option<std::sync::Arc<super::coordinator::IndexCoordinator>>,
    ) -> Result<Self, DispatchError> {
        let mut store = SkillStore::open_at(paths)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(TELEMETRY_QUEUE_CAPACITY);
        let work_guard = crate::agent::runner::current_work_guard();
        let join = std::thread::Builder::new()
            .name("skill-telemetry".into())
            .spawn(move || {
                let _work_guard = work_guard;
                while let Ok(batch) = rx.recv() {
                    match TelemetryIngestor::new(&mut store).ingest(&batch) {
                        Ok(report) if report.evidence_complete => {
                            if let Some(coordinator) = &coordinator {
                                apply_automatic_quarantine(&mut store, coordinator, &batch);
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
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
            join: Some(join),
            runtime: tokio::runtime::Handle::try_current().ok(),
        })
    }

    pub fn try_dispatch(&self, batch: EventBatch) -> Result<(), DispatchError> {
        self.tx
            .as_ref()
            .ok_or(DispatchError::Disconnected)?
            .try_send(batch)
            .map_err(|error| match error {
                TrySendError::Full(_) => DispatchError::Saturated,
                TrySendError::Disconnected(_) => DispatchError::Disconnected,
            })
    }
}

impl Drop for TelemetryDispatcher {
    fn drop(&mut self) {
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
        let stats: Result<(i64, i64), _> = store.connection().query_row(
            "SELECT invoked_count, direct_failure_count
             FROM skill_stats WHERE skill_id = ?",
            [&skill_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
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
        let Ok(created_at) = current_timestamp() else {
            continue;
        };
        if let Err(error) = QuarantineExecutor::new(coordinator).apply(
            &policy,
            &evidence,
            status,
            row_version,
            generation.desired_generation as i64,
            created_at,
        ) {
            tracing::warn!(
                skill_id = %skill_id,
                error = %error,
                "automatic skill quarantine held or failed"
            );
        }
    }
}

impl<'a> TelemetryIngestor<'a> {
    pub fn new(store: &'a mut SkillStore) -> Self {
        Self { store }
    }

    pub fn ingest(&mut self, batch: &EventBatch) -> Result<IngestionReport, TelemetryError> {
        let tx = self.store.connection_mut().transaction()?;
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
