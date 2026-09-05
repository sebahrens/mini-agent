//! Authenticated, targeted, append-only skill feedback.

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::privacy::Redactor;
use super::store::SkillStore;

pub const MAX_FEEDBACK_REASON_BYTES: usize = 512;

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
    Reviewer,
    Model,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackState {
    Active,
    Resolved,
    Retracted,
}

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
    #[error("feedback target does not exist or does not match the revision")]
    UnknownTarget,
    #[error("feedback fields exceed bounds or are invalid")]
    InvalidFeedback,
    #[error("idempotency key was reused for different feedback")]
    IdempotencyConflict,
    #[error("feedback state transition is stale or illegal")]
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
        if created_at < 0 {
            return Err(FeedbackError::InvalidFeedback);
        }
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
            return Err(FeedbackError::UnknownTarget);
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
            if target.as_deref() != Some(command.skill_id.as_str()) {
                return Err(FeedbackError::UnknownTarget);
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
        tx.commit()?;
        Ok(feedback_id)
    }

    pub fn change_state(
        &mut self,
        actor: &AuthenticatedActor,
        feedback_id: &str,
        expected_version: i64,
        next: FeedbackState,
        reason_code: &str,
        created_at: i64,
    ) -> Result<(), FeedbackError> {
        if feedback_id.is_empty() || reason_code.is_empty() || created_at < 0 {
            return Err(FeedbackError::InvalidFeedback);
        }
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
            .ok_or(FeedbackError::UnknownTarget)?;
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
    if command.idempotency_key.is_empty()
        || command.skill_id.is_empty()
        || command.reason_code.is_empty()
        || command.reason_code.len() > 64
        || command
            .reason_text
            .as_ref()
            .is_some_and(|text| text.len() > MAX_FEEDBACK_REASON_BYTES)
    {
        return Err(FeedbackError::InvalidFeedback);
    }
    if command.kind == FeedbackKind::Severe
        && !matches!(
            command.reason_code.as_str(),
            "integrity" | "permission_violation" | "unsafe_effect"
        )
    {
        return Err(FeedbackError::InvalidFeedback);
    }
    Ok(())
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
