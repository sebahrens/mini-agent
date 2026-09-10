//! Restart-safe, bounded leases for evidence-policy decisions.
//!
//! Settlement binds the owner, attempt generation and issued expiry so a stale
//! callback cannot complete or retry a later lease that reuses its owner name.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::store::SkillStore;

const MAX_POLICY_ATTEMPTS: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionLease {
    pub decision_id: String,
    pub skill_id: String,
    pub policy_version: String,
    pub attempts: u32,
    pub lease_expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    Scheduled(i64),
    DeadLettered,
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("lease parameters are invalid")]
    InvalidLease,
    #[error("decision lease is missing, stale, or owned by another worker")]
    StaleLease,
}

pub struct PolicyScheduler<'a> {
    store: &'a mut SkillStore,
}

impl<'a> PolicyScheduler<'a> {
    pub fn new(store: &'a mut SkillStore) -> Self {
        Self { store }
    }

    pub fn enqueue(
        &mut self,
        decision_id: &str,
        skill_id: &str,
        policy_version: &str,
        due_at: i64,
    ) -> Result<(), SchedulerError> {
        if decision_id.is_empty() || skill_id.is_empty() || policy_version.is_empty() || due_at < 0
        {
            return Err(SchedulerError::InvalidLease);
        }
        let changed = self.store.connection_mut().execute(
            "INSERT OR IGNORE INTO skill_decision_jobs (
                decision_id, skill_id, policy_version, due_at
             ) VALUES (?, ?, ?, ?)",
            params![decision_id, skill_id, policy_version, due_at],
        )?;
        if changed == 0 {
            let exact: bool = self.store.connection().query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM skill_decision_jobs
                    WHERE decision_id = ? AND skill_id = ?
                      AND policy_version = ? AND due_at = ?
                 )",
                params![decision_id, skill_id, policy_version, due_at],
                |row| row.get(0),
            )?;
            if !exact {
                return Err(SchedulerError::InvalidLease);
            }
        }
        Ok(())
    }

    pub fn lease_due(
        &mut self,
        owner: &str,
        now: i64,
        lease_seconds: i64,
    ) -> Result<Option<DecisionLease>, SchedulerError> {
        if owner.is_empty() || now < 0 || lease_seconds <= 0 {
            return Err(SchedulerError::InvalidLease);
        }
        let expires = now
            .checked_add(lease_seconds)
            .ok_or(SchedulerError::InvalidLease)?;
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let selected: Option<(String, String, String, i64)> = tx
            .query_row(
                "SELECT decision_id, skill_id, policy_version, attempts
                 FROM skill_decision_jobs
                 WHERE completed_at IS NULL AND due_at <= ?
                   AND (lease_expires_at IS NULL OR lease_expires_at <= ?)
                 ORDER BY due_at, decision_id LIMIT 1",
                params![now, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((decision_id, skill_id, policy_version, attempts)) = selected else {
            tx.commit()?;
            return Ok(None);
        };
        let attempts = u32::try_from(attempts)
            .ok()
            .and_then(|attempt| attempt.checked_add(1))
            .ok_or(SchedulerError::InvalidLease)?;
        let changed = tx.execute(
            "UPDATE skill_decision_jobs
             SET lease_owner = ?, lease_expires_at = ?, attempts = ?
             WHERE decision_id = ? AND completed_at IS NULL
               AND (lease_expires_at IS NULL OR lease_expires_at <= ?)",
            params![owner, expires, attempts, decision_id, now],
        )?;
        if changed != 1 {
            return Err(SchedulerError::StaleLease);
        }
        tx.commit()?;
        Ok(Some(DecisionLease {
            decision_id,
            skill_id,
            policy_version,
            attempts,
            lease_expires_at: expires,
        }))
    }

    pub fn complete(
        &mut self,
        lease: &DecisionLease,
        owner: &str,
        now: i64,
    ) -> Result<(), SchedulerError> {
        if lease.decision_id.is_empty() || lease.attempts == 0 || owner.is_empty() || now < 0 {
            return Err(SchedulerError::InvalidLease);
        }
        let changed = self.store.connection_mut().execute(
            "UPDATE skill_decision_jobs
             SET completed_at = ?, lease_owner = NULL, lease_expires_at = NULL
             WHERE decision_id = ? AND lease_owner = ?
               AND attempts = ? AND lease_expires_at = ?
               AND completed_at IS NULL AND lease_expires_at > ?",
            params![
                now,
                lease.decision_id,
                owner,
                lease.attempts,
                lease.lease_expires_at,
                now
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(SchedulerError::StaleLease)
        }
    }

    pub fn retry(
        &mut self,
        lease: &DecisionLease,
        owner: &str,
        now: i64,
        base_backoff_seconds: i64,
        max_backoff_seconds: i64,
        error_code: &str,
    ) -> Result<RetryOutcome, SchedulerError> {
        if lease.decision_id.is_empty()
            || lease.attempts == 0
            || owner.is_empty()
            || error_code.is_empty()
            || now < 0
            || base_backoff_seconds <= 0
            || max_backoff_seconds < base_backoff_seconds
        {
            return Err(SchedulerError::InvalidLease);
        }
        if lease.attempts >= MAX_POLICY_ATTEMPTS {
            let changed = self.store.connection_mut().execute(
                "UPDATE skill_decision_jobs
                 SET completed_at = ?, lease_owner = NULL, lease_expires_at = NULL,
                     last_error_code = ?
                 WHERE decision_id = ? AND lease_owner = ?
                   AND attempts = ? AND lease_expires_at = ?
                   AND completed_at IS NULL AND lease_expires_at > ?",
                params![
                    now,
                    error_code,
                    lease.decision_id,
                    owner,
                    lease.attempts,
                    lease.lease_expires_at,
                    now
                ],
            )?;
            return if changed == 1 {
                Ok(RetryOutcome::DeadLettered)
            } else {
                Err(SchedulerError::StaleLease)
            };
        }
        let exponent = lease.attempts.saturating_sub(1).min(30);
        let delay = base_backoff_seconds
            .saturating_mul(2i64.saturating_pow(exponent))
            .min(max_backoff_seconds);
        let due_at = now.checked_add(delay).ok_or(SchedulerError::InvalidLease)?;
        let changed = self.store.connection_mut().execute(
            "UPDATE skill_decision_jobs
             SET due_at = ?, lease_owner = NULL, lease_expires_at = NULL,
                 last_error_code = ?
             WHERE decision_id = ? AND lease_owner = ?
               AND attempts = ? AND lease_expires_at = ?
               AND completed_at IS NULL AND lease_expires_at > ?",
            params![
                due_at,
                error_code,
                lease.decision_id,
                owner,
                lease.attempts,
                lease.lease_expires_at,
                now
            ],
        )?;
        if changed == 1 {
            Ok(RetryOutcome::Scheduled(due_at))
        } else {
            Err(SchedulerError::StaleLease)
        }
    }

    /// Lease and dispatch one due decision with restart-safe completion and
    /// bounded retry. The callback must revalidate evidence and lifecycle state.
    pub fn run_one(
        &mut self,
        owner: &str,
        now: i64,
        lease_seconds: i64,
        base_backoff_seconds: i64,
        max_backoff_seconds: i64,
        mut dispatch: impl FnMut(&DecisionLease) -> Result<(), &'static str>,
    ) -> Result<bool, SchedulerError> {
        let Some(lease) = self.lease_due(owner, now, lease_seconds)? else {
            return Ok(false);
        };
        match dispatch(&lease) {
            Ok(()) => self.complete(&lease, owner, now)?,
            Err(error_code) => {
                self.retry(
                    &lease,
                    owner,
                    now,
                    base_backoff_seconds,
                    max_backoff_seconds,
                    error_code,
                )?;
            }
        }
        Ok(true)
    }
}
