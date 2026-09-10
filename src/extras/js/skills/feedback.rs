//! Authenticated, targeted, append-only skill feedback.

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::privacy::Redactor;
use super::store::SkillStore;

pub const MAX_FEEDBACK_REASON_BYTES: usize = 512;
/// Maximum bytes accepted for `reason_code`, which is copied verbatim into the
/// append-only audit table.
pub const MAX_FEEDBACK_REASON_CODE_BYTES: usize = 64;
/// Maximum bytes accepted for `idempotency_key`, which is stored in a UNIQUE
/// TEXT column.
pub const MAX_FEEDBACK_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// The closed set of reason codes that may accompany [`FeedbackKind::Severe`].
/// Anything else is an ordinary quality report and must be submitted as
/// negative feedback.
pub const SEVERE_FEEDBACK_REASON_CODES: [&str; 3] =
    ["integrity", "permission_violation", "unsafe_effect"];
/// Whole days of raw telemetry retained before compaction removes the
/// `invoked` rows that attributed feedback is matched against.
pub const RAW_TELEMETRY_RETENTION_DAYS: i64 =
    super::retention::DEFAULT_RAW_RETENTION_SECONDS / (24 * 60 * 60);
/// Longest offending-value fragment echoed back in a format error.
const MALFORMED_ID_PREVIEW_CHARS: usize = 72;

type ExistingFeedback = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    Owner,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Retain scoped reviewer authority while operator integration is audited in mini-agent-kv9me"
        )
    )]
    Reviewer,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Retain the explicitly unauthorized model actor; production constructs only the local owner (mini-agent-kv9me)"
        )
    )]
    Model,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Retain the explicitly unauthorized anonymous actor; production constructs only the local owner (mini-agent-kv9me)"
        )
    )]
    Anonymous,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedActor {
    pub actor_id: String,
    pub kind: ActorKind,
    pub allowed_skill_ids: Option<std::collections::BTreeSet<String>>,
}

impl AuthenticatedActor {
    fn may_target(&self, skill_id: &str) -> bool {
        matches!(self.kind, ActorKind::Owner | ActorKind::Reviewer)
            && !self.actor_id.is_empty()
            && self
                .allowed_skill_ids
                .as_ref()
                .is_none_or(|allowed| allowed.contains(skill_id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Positive,
    Negative,
    Severe,
}

impl FeedbackKind {
    fn token(self) -> &'static str {
        match self {
            Self::Positive => "positive",
            Self::Negative => "negative",
            Self::Severe => "severe",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeedbackCommand {
    pub idempotency_key: String,
    pub skill_id: String,
    pub invocation_id: Option<String>,
    pub kind: FeedbackKind,
    pub reason_code: String,
    pub reason_text: Option<String>,
}

// Test-only: only [`FeedbackService::change_state`] consumes this, and that is cfg(test).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackState {
    Active,
    Resolved,
    Retracted,
}

#[cfg(test)]
impl FeedbackState {
    fn token(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Resolved => "resolved",
            Self::Retracted => "retracted",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FeedbackError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("actor is unauthenticated or outside the target scope")]
    Unauthorized,
    #[error("no learned-skill revision `{skill_id}` exists")]
    UnknownSkill { skill_id: String },
    #[error(
        "invocation `{invocation_id}` has no recorded `invoked` event in raw \
         skill telemetry; raw events are retained for {retention_days} days and \
         are compacted into daily aggregates afterwards, so an older invocation \
         id can no longer be attributed"
    )]
    UnknownInvocation {
        invocation_id: String,
        retention_days: i64,
    },
    #[error(
        "invocation `{invocation_id}` was recorded for a different learned skill \
         than `{skill_id}`"
    )]
    InvocationSkillMismatch {
        invocation_id: String,
        skill_id: String,
    },
    #[error("feedback record `{feedback_id}` does not exist")]
    #[cfg(test)]
    UnknownFeedback { feedback_id: String },
    #[error("feedback field `{field}` is invalid: {rule}")]
    InvalidFeedback {
        field: &'static str,
        rule: &'static str,
    },
    #[error(
        "feedback field `{field}` must be 64 lowercase hexadecimal characters as \
         copied from telemetry, not `{value}`"
    )]
    MalformedId { field: &'static str, value: String },
    #[error(
        "severe feedback requires reason_code to be one of `integrity`, \
         `permission_violation` or `unsafe_effect`; `{reason_code}` is not one of \
         them, so submit it as negative feedback instead"
    )]
    UnsupportedSevereReasonCode { reason_code: String },
    #[error("idempotency key was reused for different feedback")]
    IdempotencyConflict,
    #[error("feedback state transition is stale or illegal")]
    #[cfg(test)]
    InvalidStateTransition,
}

pub struct FeedbackService<'a> {
    store: &'a mut SkillStore,
    redactor: Redactor,
}

impl<'a> FeedbackService<'a> {
    pub fn new(store: &'a mut SkillStore, redactor: Redactor) -> Self {
        Self { store, redactor }
    }

    pub fn submit(
        &mut self,
        actor: &AuthenticatedActor,
        command: &FeedbackCommand,
        created_at: i64,
    ) -> Result<String, FeedbackError> {
        validate_command(actor, command)?;
        validate_timestamp(created_at)?;
        let reason_text = command
            .reason_text
            .as_deref()
            .map(|value| self.redactor.redact(value));
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision_exists = tx
            .query_row(
                "SELECT 1 FROM skill_revisions WHERE id = ?",
                [&command.skill_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !revision_exists {
            return Err(FeedbackError::UnknownSkill {
                skill_id: command.skill_id.clone(),
            });
        }
        if let Some(invocation_id) = &command.invocation_id {
            let target: Option<String> = tx
                .query_row(
                    "SELECT skill_id FROM skill_events
                     WHERE invocation_id = ? AND event_kind = 'invoked'",
                    [invocation_id],
                    |row| row.get(0),
                )
                .optional()?;
            match target {
                // The raw `invoked` row is gone. That is indistinguishable from
                // a typo here, but retention compacts every event older than
                // the raw window, so say so instead of claiming the invocation
                // never existed.
                None => {
                    return Err(FeedbackError::UnknownInvocation {
                        invocation_id: invocation_id.clone(),
                        retention_days: RAW_TELEMETRY_RETENTION_DAYS,
                    });
                }
                Some(observed) if observed != command.skill_id => {
                    return Err(FeedbackError::InvocationSkillMismatch {
                        invocation_id: invocation_id.clone(),
                        skill_id: command.skill_id.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        let feedback_id = feedback_id(command);
        let existing: Option<ExistingFeedback> = tx
            .query_row(
                "SELECT feedback_id, skill_id, invocation_id, actor_id,
                        feedback_kind, reason_code, reason_text
                 FROM skill_feedback WHERE idempotency_key = ?",
                [&command.idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        if let Some((
            existing_id,
            skill_id,
            invocation_id,
            actor_id,
            kind,
            reason_code,
            existing_reason_text,
        )) = existing
        {
            if existing_id == feedback_id
                && skill_id == command.skill_id
                && invocation_id == command.invocation_id
                && actor_id == actor.actor_id
                && kind == command.kind.token()
                && reason_code == command.reason_code
                && existing_reason_text == reason_text
            {
                return Ok(existing_id);
            }
            return Err(FeedbackError::IdempotencyConflict);
        }

        tx.execute(
            "INSERT INTO skill_feedback (
                feedback_id, idempotency_key, skill_id, invocation_id, actor_id,
                feedback_kind, reason_code, reason_text, state, version,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', 1, ?, ?)",
            params![
                feedback_id,
                command.idempotency_key,
                command.skill_id,
                command.invocation_id,
                actor.actor_id,
                command.kind.token(),
                command.reason_code,
                reason_text,
                created_at,
                created_at,
            ],
        )?;
        tx.execute(
            "INSERT INTO skill_feedback_audit (
                feedback_id, from_state, to_state, actor_id,
                reason_code, version, created_at
             ) VALUES (?, NULL, 'active', ?, ?, 1, ?)",
            params![feedback_id, actor.actor_id, command.reason_code, created_at],
        )?;
        // Authenticated user feedback is a first-class signal, so record it in
        // the same counters the telemetry path maintains. The counts are
        // cumulative reports received; resolving or retracting a report leaves
        // them alone so the columns can never underflow their CHECK.
        let positive = i64::from(command.kind == FeedbackKind::Positive);
        let negative = i64::from(matches!(
            command.kind,
            FeedbackKind::Negative | FeedbackKind::Severe
        ));
        tx.execute(
            "INSERT INTO skill_stats (
                skill_id, user_positive_count, user_negative_count, updated_at
             ) VALUES (?, ?, ?, ?)
             ON CONFLICT(skill_id) DO UPDATE SET
                user_positive_count =
                    user_positive_count + excluded.user_positive_count,
                user_negative_count =
                    user_negative_count + excluded.user_negative_count,
                updated_at = MAX(updated_at, excluded.updated_at)",
            params![command.skill_id, positive, negative, created_at],
        )?;
        tx.commit()?;
        Ok(feedback_id)
    }

    // Test-only: there is no operator surface that resolves or retracts feedback, so
    // nothing in production drives this transition. Kept for the state-machine test.
    #[cfg(test)]
    pub fn change_state(
        &mut self,
        actor: &AuthenticatedActor,
        feedback_id: &str,
        expected_version: i64,
        next: FeedbackState,
        reason_code: &str,
        created_at: i64,
    ) -> Result<(), FeedbackError> {
        if feedback_id.is_empty() {
            return Err(FeedbackError::InvalidFeedback {
                field: "feedback_id",
                rule: "must not be empty",
            });
        }
        validate_reason_code(reason_code)?;
        validate_timestamp(created_at)?;
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (skill_id, current, version): (String, String, i64) = tx
            .query_row(
                "SELECT skill_id, state, version FROM skill_feedback
                 WHERE feedback_id = ?",
                [feedback_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| FeedbackError::UnknownFeedback {
                feedback_id: feedback_id.to_string(),
            })?;
        if !actor.may_target(&skill_id) {
            return Err(FeedbackError::Unauthorized);
        }
        if current != "active" || version != expected_version || next == FeedbackState::Active {
            return Err(FeedbackError::InvalidStateTransition);
        }
        let next_version = version + 1;
        let changed = tx.execute(
            "UPDATE skill_feedback SET state = ?, version = ?, updated_at = ?
             WHERE feedback_id = ? AND state = 'active' AND version = ?",
            params![next.token(), next_version, created_at, feedback_id, version],
        )?;
        if changed != 1 {
            return Err(FeedbackError::InvalidStateTransition);
        }
        tx.execute(
            "INSERT INTO skill_feedback_audit (
                feedback_id, from_state, to_state, actor_id,
                reason_code, version, created_at
             ) VALUES (?, 'active', ?, ?, ?, ?, ?)",
            params![
                feedback_id,
                next.token(),
                actor.actor_id,
                reason_code,
                next_version,
                created_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn validate_command(
    actor: &AuthenticatedActor,
    command: &FeedbackCommand,
) -> Result<(), FeedbackError> {
    if !actor.may_target(&command.skill_id) {
        return Err(FeedbackError::Unauthorized);
    }
    validate_idempotency_key(&command.idempotency_key)?;
    validate_hex_id("skill_id", &command.skill_id)?;
    if let Some(invocation_id) = &command.invocation_id {
        validate_hex_id("invocation_id", invocation_id)?;
    }
    validate_reason_code(&command.reason_code)?;
    if command
        .reason_text
        .as_ref()
        .is_some_and(|text| text.len() > MAX_FEEDBACK_REASON_BYTES)
    {
        return Err(FeedbackError::InvalidFeedback {
            field: "reason_text",
            rule: "must not exceed 512 bytes",
        });
    }
    if command.kind == FeedbackKind::Severe
        && !SEVERE_FEEDBACK_REASON_CODES.contains(&command.reason_code.as_str())
    {
        return Err(FeedbackError::UnsupportedSevereReasonCode {
            reason_code: command.reason_code.clone(),
        });
    }
    Ok(())
}

fn validate_timestamp(created_at: i64) -> Result<(), FeedbackError> {
    if created_at < 0 {
        return Err(FeedbackError::InvalidFeedback {
            field: "created_at",
            rule: "must not be negative",
        });
    }
    Ok(())
}

/// The key is stored in a UNIQUE TEXT column and echoed in conflict reports, so
/// it is bounded and restricted to characters that survive logging verbatim.
fn validate_idempotency_key(key: &str) -> Result<(), FeedbackError> {
    if key.is_empty() {
        return Err(FeedbackError::InvalidFeedback {
            field: "idempotency_key",
            rule: "must not be empty",
        });
    }
    if key.len() > MAX_FEEDBACK_IDEMPOTENCY_KEY_BYTES {
        return Err(FeedbackError::InvalidFeedback {
            field: "idempotency_key",
            rule: "must not exceed 128 bytes",
        });
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(FeedbackError::InvalidFeedback {
            field: "idempotency_key",
            rule: "must contain only ASCII letters, digits, '.', '_', ':' or '-'",
        });
    }
    Ok(())
}

/// `reason_code` is copied unredacted into `skill_feedback_audit`, so it stays a
/// closed-shape token rather than free text.
fn validate_reason_code(reason_code: &str) -> Result<(), FeedbackError> {
    if reason_code.is_empty() {
        return Err(FeedbackError::InvalidFeedback {
            field: "reason_code",
            rule: "must not be empty",
        });
    }
    if reason_code.len() > MAX_FEEDBACK_REASON_CODE_BYTES {
        return Err(FeedbackError::InvalidFeedback {
            field: "reason_code",
            rule: "must not exceed 64 bytes",
        });
    }
    if !reason_code
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        return Err(FeedbackError::InvalidFeedback {
            field: "reason_code",
            rule: "must contain only lowercase ASCII letters and '_'",
        });
    }
    Ok(())
}

/// Skill and invocation ids are 64 lowercase hex characters everywhere else in
/// the telemetry and retention paths. Reject the wrong shape here so a typo or
/// an uppercase copy is not reported as a missing target.
fn validate_hex_id(field: &'static str, value: &str) -> Result<(), FeedbackError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(FeedbackError::MalformedId {
        field,
        value: preview(value),
    })
}

fn preview(value: &str) -> String {
    match value.char_indices().nth(MALFORMED_ID_PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_string(),
    }
}

fn feedback_id(command: &FeedbackCommand) -> String {
    let mut digest = Sha256::new();
    digest.update(b"mini-agent/targeted-feedback/v1");
    for value in [
        command.idempotency_key.as_str(),
        command.skill_id.as_str(),
        command.invocation_id.as_deref().unwrap_or(""),
        command.kind.token(),
        command.reason_code.as_str(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    crate::hex::encode_lower(digest.finalize())
}
