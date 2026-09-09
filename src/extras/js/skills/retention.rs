//! Transactional telemetry compaction and explicit privacy purge.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::coordinator::{
    CoordinatedMutationError, CoordinatorError, IndexCoordinator, PublicationReport,
};
use super::store::{SkillStore, StoreError};

pub const DEFAULT_RAW_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;
const DAY_SECONDS: i64 = 24 * 60 * 60;

/// Lifecycle statuses a purge may remove without an explicit operator
/// override: no transition leads out of them, so nothing is cut short.
pub const TERMINAL_PURGE_STATUSES: [&str; 3] = ["rejected", "retired", "superseded"];

/// Read-only facts the operator purge surface needs *before* immutable bytes
/// are deleted.
///
/// [`RetentionService::privacy_purge`] is deliberately unconditional — it is
/// the privacy escape hatch — so the lifecycle and reference guard lives here,
/// where the operator command can refuse or report before calling it. The read
/// is not part of the purge transaction; a concurrent writer could still move
/// the target, which is why the guard is an operator confirmation gate and not
/// a safety invariant.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PurgePlan {
    /// Current lifecycle status, or `None` when the target is already a
    /// tombstone and the purge is an idempotent replay.
    pub status: Option<String>,
    /// `(id, status)` of every revision naming the target as its predecessor.
    /// The purge clears their `supersedes_id` and makes each its own lineage
    /// root, so a canary replacement silently becomes an activatable root.
    pub rerooted_replacements: Vec<(String, String)>,
    /// Further revisions sharing the target's lineage root, which the purge
    /// also re-roots.
    pub rerooted_lineage: Vec<String>,
}

impl PurgePlan {
    /// True when nothing can transition out of the target's status.
    pub fn is_terminal(&self) -> bool {
        self.status
            .as_deref()
            .is_none_or(|status| TERMINAL_PURGE_STATUSES.contains(&status))
    }

    /// True when the purge would cut a live lifecycle short or silently
    /// re-root a dependant, and therefore needs an explicit operator override.
    pub fn requires_force(&self) -> bool {
        !self.is_terminal()
            || !self.rerooted_replacements.is_empty()
            || !self.rerooted_lineage.is_empty()
    }

    /// Every revision this purge would re-root, in stable order.
    pub fn rerooted_ids(&self) -> Vec<&str> {
        self.rerooted_replacements
            .iter()
            .map(|(id, _)| id.as_str())
            .chain(self.rerooted_lineage.iter().map(String::as_str))
            .collect()
    }
}

/// Collect the lifecycle status and dependent revisions of a purge target.
pub fn purge_preflight(store: &SkillStore, skill_id: &str) -> Result<PurgePlan, RetentionError> {
    let connection = store.connection();
    let status: Option<String> = connection
        .query_row(
            "SELECT status FROM skill_revisions WHERE id = ?",
            [skill_id],
            |row| row.get(0),
        )
        .optional()?;
    if status.is_none() {
        let tombstoned = connection
            .query_row(
                "SELECT 1 FROM skill_tombstones WHERE id = ?",
                [skill_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !tombstoned {
            return Err(RetentionError::NotFound);
        }
        return Ok(PurgePlan::default());
    }
    let mut statement = connection
        .prepare("SELECT id, status FROM skill_revisions WHERE supersedes_id = ? ORDER BY id")?;
    let rerooted_replacements = statement
        .query_map([skill_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut statement = connection.prepare(
        "SELECT id FROM skill_revisions
          WHERE lineage_root_id = ?1 AND id <> ?1
            AND (supersedes_id IS NULL OR supersedes_id <> ?1)
          ORDER BY id",
    )?;
    let rerooted_lineage = statement
        .query_map([skill_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PurgePlan {
        status,
        rerooted_replacements,
        rerooted_lineage,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    pub compacted_events: usize,
    pub through_event_id: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("retention cutoff or generation is invalid")]
    InvalidInput,
    #[error("privacy purge target does not exist")]
    NotFound,
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatedRetentionError {
    #[error(transparent)]
    Retention(#[from] RetentionError),
    #[error(transparent)]
    Publication(#[from] CoordinatorError),
}

impl From<CoordinatedMutationError<RetentionError>> for CoordinatedRetentionError {
    fn from(error: CoordinatedMutationError<RetentionError>) -> Self {
        match error {
            CoordinatedMutationError::Mutation(error) => Self::Retention(error),
            CoordinatedMutationError::Publication(error) => Self::Publication(error),
        }
    }
}

pub struct CoordinatedRetention<'a> {
    coordinator: &'a IndexCoordinator,
}

impl<'a> CoordinatedRetention<'a> {
    pub fn new(coordinator: &'a IndexCoordinator) -> Self {
        Self { coordinator }
    }

    /// Purge durable bytes and publish immediate exclusion through the same
    /// exclusive gate used by lifecycle transitions.
    pub fn privacy_purge(
        &self,
        skill_id: &str,
        reason_code: &str,
        now: i64,
    ) -> Result<(i64, PublicationReport), CoordinatedRetentionError> {
        self.coordinator
            .coordinate_removal(
                std::collections::HashSet::from([skill_id.to_string()]),
                |store| {
                    let generation =
                        RetentionService::new(store).privacy_purge(skill_id, reason_code, now)?;
                    Ok((generation, generation as u64))
                },
            )
            .map_err(Into::into)
    }
}

#[derive(Default)]
struct Aggregate {
    invoked: i64,
    success: i64,
    failure: i64,
    timeout: i64,
    oom: i64,
    policy: i64,
    latency: i64,
    through: i64,
}

pub struct RetentionService<'a> {
    store: &'a mut SkillStore,
}

impl<'a> RetentionService<'a> {
    pub fn new(store: &'a mut SkillStore) -> Self {
        Self { store }
    }

    pub fn compact_before(
        &mut self,
        cutoff: i64,
        aggregate_version: i64,
        now: i64,
    ) -> Result<CompactionReport, RetentionError> {
        if cutoff < 0 || aggregate_version < 1 || now < cutoff {
            return Err(RetentionError::InvalidInput);
        }
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let watermark: i64 = tx
            .query_row(
                "SELECT through_event_id FROM skill_compaction_watermarks
                 WHERE worker_name = 'raw-events'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut statement = tx.prepare(
            "SELECT event_id, skill_id, event_kind, latency_us, created_at
             FROM skill_events
             WHERE event_id > ?
             ORDER BY event_id",
        )?;
        let rows = statement.query_map([watermark], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut aggregates: BTreeMap<(String, i64), Aggregate> = BTreeMap::new();
        let mut count = 0usize;
        let mut through = watermark;
        for row in rows {
            let (event_id, skill_id, kind, latency, created_at) = row?;
            // Advance only through a contiguous event-ID prefix. Timestamps can
            // arrive out of order; skipping a newer event here and advancing
            // past it would later delete it without ever aggregating it.
            if created_at >= cutoff {
                break;
            }
            let day = created_at - created_at.rem_euclid(DAY_SECONDS);
            let aggregate = aggregates.entry((skill_id, day)).or_default();
            aggregate.invoked += i64::from(kind == "invoked");
            aggregate.success += i64::from(kind == "returned");
            aggregate.failure += i64::from(matches!(
                kind.as_str(),
                "threw" | "timed_out" | "oom" | "capability_denied"
            ));
            aggregate.timeout += i64::from(kind == "timed_out");
            aggregate.oom += i64::from(kind == "oom");
            aggregate.policy += i64::from(kind == "capability_denied");
            aggregate.latency += latency.unwrap_or(0);
            aggregate.through = event_id;
            through = event_id;
            count += 1;
        }
        drop(statement);

        for ((skill_id, day), aggregate) in aggregates {
            tx.execute(
                "INSERT INTO skill_daily_stats (
                    skill_id, day_start, aggregate_version, through_event_id,
                    invoked_count, direct_success_count, direct_failure_count,
                    timeout_count, oom_count, policy_fault_count, latency_total_us
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(skill_id, day_start, aggregate_version) DO UPDATE SET
                    through_event_id = MAX(through_event_id, excluded.through_event_id),
                    invoked_count = invoked_count + excluded.invoked_count,
                    direct_success_count =
                        direct_success_count + excluded.direct_success_count,
                    direct_failure_count =
                        direct_failure_count + excluded.direct_failure_count,
                    timeout_count = timeout_count + excluded.timeout_count,
                    oom_count = oom_count + excluded.oom_count,
                    policy_fault_count =
                        policy_fault_count + excluded.policy_fault_count,
                    latency_total_us = latency_total_us + excluded.latency_total_us",
                params![
                    skill_id,
                    day,
                    aggregate_version,
                    aggregate.through,
                    aggregate.invoked,
                    aggregate.success,
                    aggregate.failure,
                    aggregate.timeout,
                    aggregate.oom,
                    aggregate.policy,
                    aggregate.latency,
                ],
            )?;
        }
        if through > watermark {
            tx.execute(
                "INSERT INTO skill_compaction_watermarks (
                    worker_name, aggregate_version, through_event_id, updated_at
                 ) VALUES ('raw-events', ?, ?, ?)
                 ON CONFLICT(worker_name) DO UPDATE SET
                    aggregate_version = excluded.aggregate_version,
                    through_event_id = MAX(
                        through_event_id, excluded.through_event_id
                    ),
                    updated_at = excluded.updated_at",
                params![aggregate_version, through, now],
            )?;
            tx.execute("DELETE FROM skill_events WHERE event_id <= ?", [through])?;
        }
        // Loss summaries outlive raw rows while any task evidence still
        // depends on them. Only aged, unreferenced turns can be forgotten.
        tx.execute(
            "DELETE FROM skill_turn_losses WHERE created_at < ?1
             AND NOT EXISTS (SELECT 1 FROM skill_events AS event
                 WHERE event.turn_id = skill_turn_losses.turn_id
                   AND event.production = skill_turn_losses.production)
             AND NOT EXISTS (SELECT 1 FROM skill_task_outcomes AS outcome
                 WHERE outcome.turn_id = skill_turn_losses.turn_id
                   AND outcome.production = skill_turn_losses.production)",
            [cutoff],
        )?;
        tx.commit()?;
        Ok(CompactionReport {
            compacted_events: count,
            through_event_id: through,
        })
    }

    /// Delete the target's immutable bytes and dependent audit.
    ///
    /// This is the unconditional privacy escape hatch: it removes an `active`
    /// revision as readily as a `rejected` one and re-roots every dependant.
    /// Operator surfaces must call [`purge_preflight`] first and refuse or
    /// report what this will do.
    ///
    /// Telemetry goes with the revision: events, rolled-up stats, daily
    /// aggregates, feedback, task-outcome attributions, and the task outcomes
    /// this skill was the only claimant of. Nothing keyed to the purged id
    /// survives the transaction — telemetry outliving the artifact is exactly
    /// what a privacy purge exists to prevent.
    pub fn privacy_purge(
        &mut self,
        skill_id: &str,
        reason_code: &str,
        now: i64,
    ) -> Result<i64, RetentionError> {
        if skill_id.len() != 64
            || !skill_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || reason_code.is_empty()
            || now < 0
        {
            return Err(RetentionError::InvalidInput);
        }
        let tx = self
            .store
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists = tx
            .query_row(
                "SELECT 1 FROM skill_revisions WHERE id = ?",
                [skill_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            let tombstone = tx
                .query_row(
                    "SELECT last_generation FROM skill_tombstones
                     WHERE id = ?",
                    [skill_id],
                    |row| row.get(0),
                )
                .optional()?;
            return tombstone.ok_or(RetentionError::NotFound);
        }
        let desired: i64 = tx.query_row(
            "SELECT desired_generation FROM skill_generations WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let next_generation = desired + 1;
        // Break textual lineage pointers before deletion. Privacy purge is the
        // only operation permitted to remove immutable source and dependent audit.
        tx.execute(
            "UPDATE skill_revisions
             SET supersedes_id = NULL, lineage_root_id = id
             WHERE supersedes_id = ?",
            [skill_id],
        )?;
        tx.execute(
            "UPDATE skill_revisions
             SET superseded_by_id = NULL WHERE superseded_by_id = ?",
            [skill_id],
        )?;
        tx.execute(
            "UPDATE skill_revisions
             SET lineage_root_id = id WHERE lineage_root_id = ? AND id <> ?",
            params![skill_id, skill_id],
        )?;
        tx.execute(
            "DELETE FROM skill_transitions WHERE predecessor_id = ?",
            [skill_id],
        )?;
        // Phase 4 admission records bind revisions through restrictive foreign
        // keys. Privacy purge removes records owned by the target and clears
        // predecessor-only references before deleting its immutable bytes.
        tx.execute("DELETE FROM skill_approvals WHERE skill_id = ?", [skill_id])?;
        tx.execute(
            "DELETE FROM evaluation_reports WHERE skill_id = ?",
            [skill_id],
        )?;
        tx.execute(
            "UPDATE evaluation_reports SET predecessor_id = NULL
             WHERE predecessor_id = ?",
            [skill_id],
        )?;
        tx.execute("DELETE FROM skill_proposals WHERE skill_id = ?", [skill_id])?;
        tx.execute(
            "UPDATE skill_proposals SET predecessor_id = NULL
             WHERE predecessor_id = ?",
            [skill_id],
        )?;
        // Remove loss metadata owned only by the revision being purged.
        // Keep a turn's loss if any event or task outcome will survive below.
        tx.execute(
            "DELETE FROM skill_turn_losses
             WHERE (EXISTS (SELECT 1 FROM skill_events AS owned
                        WHERE owned.skill_id = ?1
                          AND owned.turn_id = skill_turn_losses.turn_id
                          AND owned.production = skill_turn_losses.production)
                 OR EXISTS (SELECT 1 FROM skill_task_outcomes AS outcome
                        JOIN skill_task_outcome_links AS owned USING (evidence_id)
                        WHERE owned.skill_id = ?1
                          AND outcome.turn_id = skill_turn_losses.turn_id
                          AND outcome.production = skill_turn_losses.production))
               AND NOT EXISTS (SELECT 1 FROM skill_events AS other
                        WHERE other.skill_id <> ?1
                          AND other.turn_id = skill_turn_losses.turn_id
                          AND other.production = skill_turn_losses.production)
               AND NOT EXISTS (SELECT 1 FROM skill_task_outcomes AS outcome
                        WHERE outcome.turn_id = skill_turn_losses.turn_id
                          AND outcome.production = skill_turn_losses.production
                          AND (NOT EXISTS (SELECT 1 FROM skill_task_outcome_links AS owned
                                  WHERE owned.evidence_id = outcome.evidence_id AND owned.skill_id = ?1)
                            OR EXISTS (SELECT 1 FROM skill_task_outcome_links AS other
                                  WHERE other.evidence_id = outcome.evidence_id AND other.skill_id <> ?1)))",
            [skill_id],
        )?;
        // A task outcome is stored once and attributed through
        // `skill_task_outcome_links`. Its own row is NOT skill-scoped, so
        // nothing removes it when the revision goes: it would survive the
        // purge as an unlinked row, and an unlinked outcome is exactly how the
        // store spells "this turn ran with no library" — the purged skill's
        // turns would be silently recycled as no-library baseline evidence for
        // every other skill's comparison, with their turn IDs intact. Delete
        // the outcomes this skill was the only claimant of, before the links
        // that identify them go. Outcomes shared with a surviving skill stay:
        // they remain that skill's evidence, and only the purged attribution
        // is removed.
        tx.execute(
            "DELETE FROM skill_task_outcomes
             WHERE evidence_id IN (
                 SELECT owned.evidence_id FROM skill_task_outcome_links AS owned
                  WHERE owned.skill_id = ?1
             )
             AND NOT EXISTS (
                 SELECT 1 FROM skill_task_outcome_links AS other
                  WHERE other.evidence_id = skill_task_outcomes.evidence_id
                    AND other.skill_id <> ?1
             )",
            [skill_id],
        )?;
        // Telemetry for the purged id. Every one of these tables declares
        // `ON DELETE CASCADE` on `skill_revisions(id)`, so the deletion below
        // already removes them whenever `PRAGMA foreign_keys` is on. They are
        // deleted explicitly anyway: this is the privacy escape hatch, and its
        // guarantee must not rest on a per-connection pragma being set. The
        // deletes are idempotent and cost nothing once the rows are gone.
        for table in [
            "skill_task_outcome_links",
            "skill_events",
            "skill_stats",
            "skill_feedback",
            "skill_daily_stats",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE skill_id = ?"),
                [skill_id],
            )?;
        }
        tx.execute("DELETE FROM skill_revisions WHERE id = ?", [skill_id])?;
        tx.execute(
            "INSERT INTO skill_tombstones (
                id, purged_at, reason_code, last_generation
             ) VALUES (?, ?, ?, ?)",
            params![skill_id, now, reason_code, next_generation],
        )?;
        tx.execute(
            "UPDATE skill_generations
             SET desired_generation = ?, publication_mode = 'removal_only',
                 last_error_code = NULL, updated_at = ?
             WHERE singleton = 1",
            params![next_generation, now],
        )?;
        tx.commit()?;
        Ok(next_generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
    use crate::paths::{AppPaths, PathEnvironment, PathPlatform};

    fn fixture() -> (std::path::PathBuf, SkillStore) {
        let root = std::env::temp_dir().join(format!(
            "skill-retention-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let paths = AppPaths::resolve(&PathEnvironment {
            platform: if cfg!(target_os = "macos") {
                PathPlatform::MacOs
            } else if cfg!(target_os = "windows") {
                PathPlatform::Windows
            } else {
                PathPlatform::Linux
            },
            home_dir: None,
            config_base: Some(root.join("config")),
            data_base: Some(root.join("data")),
            local_data_base: Some(root.join("local")),
            state_base: Some(root.join("state")),
            cache_base: Some(root.join("cache")),
            workspace_root: None,
            overrides: Default::default(),
        })
        .unwrap();
        let store = SkillStore::open_at(&paths).unwrap();
        (root, store)
    }

    fn artifact(source: &str, description: &str) -> SkillArtifact {
        SkillArtifact::new(
            source.into(),
            description.into(),
            vec![],
            vec![SkillExport {
                name: "run".into(),
                signature: "() => number".into(),
            }],
            vec!["run() === 1".into()],
            CapabilityManifest::pure(),
        )
        .unwrap()
    }

    #[test]
    fn purge_preflight_names_the_status_and_every_revision_the_purge_would_reroot() {
        let (root, mut store) = fixture();
        let predecessor = artifact(
            "function run() { return 1; }",
            "Purge preflight predecessor",
        );
        let replacement = artifact(
            "function run() { return 1; } // replacement",
            "Purge preflight replacement",
        );
        store.insert_verified(&predecessor).unwrap();
        store.insert_verified(&replacement).unwrap();
        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions
                 SET status = 'canary', supersedes_id = ?, lineage_root_id = ?
                 WHERE id = ?",
                rusqlite::params![predecessor.id, predecessor.id, replacement.id],
            )
            .unwrap();

        let plan = purge_preflight(&store, &predecessor.id).unwrap();
        assert_eq!(plan.status.as_deref(), Some("active"));
        assert!(!plan.is_terminal());
        assert!(plan.requires_force());
        assert_eq!(
            plan.rerooted_replacements,
            vec![(replacement.id.clone(), "canary".to_string())]
        );
        assert!(plan.rerooted_lineage.is_empty());
        assert_eq!(plan.rerooted_ids(), vec![replacement.id.as_str()]);

        // The unguarded purge is exactly what the plan warned about: the
        // canary replacement is re-rooted rather than removed with it.
        RetentionService::new(&mut store)
            .privacy_purge(&predecessor.id, "test_request", 10)
            .unwrap();
        let (supersedes, lineage_root): (Option<String>, String) = store
            .conn()
            .query_row(
                "SELECT supersedes_id, lineage_root_id FROM skill_revisions WHERE id = ?",
                [&replacement.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(supersedes, None);
        assert_eq!(lineage_root, replacement.id);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn purge_preflight_clears_force_for_terminal_revisions_and_replayed_tombstones() {
        let (root, mut store) = fixture();
        let skill = artifact("function run() { return 1; }", "Purge preflight terminal");
        store.insert_verified(&skill).unwrap();

        let live = purge_preflight(&store, &skill.id).unwrap();
        assert_eq!(live.status.as_deref(), Some("active"));
        assert!(live.requires_force());

        store
            .conn_mut()
            .execute(
                "UPDATE skill_revisions SET status = 'rejected' WHERE id = ?",
                [&skill.id],
            )
            .unwrap();
        let terminal = purge_preflight(&store, &skill.id).unwrap();
        assert!(terminal.is_terminal());
        assert!(!terminal.requires_force());

        RetentionService::new(&mut store)
            .privacy_purge(&skill.id, "test_request", 10)
            .unwrap();
        let replayed = purge_preflight(&store, &skill.id).unwrap();
        assert_eq!(replayed.status, None);
        assert!(!replayed.requires_force());

        assert!(matches!(
            purge_preflight(&store, &"f".repeat(64)),
            Err(RetentionError::NotFound)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Seed one row in every telemetry table keyed to `skill_id`, plus the
    /// task outcomes used to tell "owned by this skill" from "shared" and
    /// "no-library baseline" apart.
    fn seed_telemetry(store: &mut SkillStore, skill_id: &str, shared_with: &str) {
        let conn = store.conn_mut();
        conn.execute(
            "INSERT INTO skill_events (
                 invocation_id, skill_id, turn_id, event_kind, index_generation, created_at
             ) VALUES (?1, ?1, 'turn-1', 'invoked', 1, 10)",
            [skill_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skill_stats (skill_id, invoked_count, updated_at)
             VALUES (?, 1, 10)",
            [skill_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skill_daily_stats (
                 skill_id, day_start, aggregate_version, through_event_id,
                 invoked_count, direct_success_count, direct_failure_count,
                 timeout_count, oom_count, policy_fault_count, latency_total_us
             ) VALUES (?, 0, 1, 1, 1, 1, 0, 0, 0, 0, 5)",
            [skill_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skill_feedback (
                 feedback_id, idempotency_key, skill_id, actor_id, feedback_kind,
                 reason_code, state, created_at, updated_at
             ) VALUES ('fb-1', 'idem-1', ?, 'actor', 'negative', 'wrong', 'active', 10, 10)",
            [skill_id],
        )
        .unwrap();
        for (evidence, turn) in [("owned", "turn-owned"), ("shared", "turn-shared")] {
            conn.execute(
                "INSERT INTO skill_task_outcomes (
                     evidence_id, turn_id, verify_passed, attempt, source_kind,
                     source_id, production, created_at
                 ) VALUES (?1, ?2, 1, 1, 'verify_command', ?1, 1, 10)",
                rusqlite::params![evidence, turn],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO skill_task_outcome_links (evidence_id, skill_id) VALUES (?, ?)",
                rusqlite::params![evidence, skill_id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO skill_task_outcome_links (evidence_id, skill_id)
             VALUES ('shared', ?)",
            [shared_with],
        )
        .unwrap();
        // A genuine no-library baseline: an outcome with no attribution at all.
        conn.execute(
            "INSERT INTO skill_task_outcomes (
                 evidence_id, turn_id, verify_passed, attempt, source_kind,
                 source_id, production, created_at
             ) VALUES ('baseline', 'turn-baseline', 1, 1, 'verify_command', 'baseline', 1, 10)",
            [],
        )
        .unwrap();
    }

    fn count_for(store: &SkillStore, table: &str, skill_id: &str) -> i64 {
        store
            .conn()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE skill_id = ?"),
                [skill_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn surviving_outcomes(store: &SkillStore) -> Vec<String> {
        let mut statement = store
            .conn()
            .prepare("SELECT evidence_id FROM skill_task_outcomes ORDER BY evidence_id")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    /// d90k: a task outcome's own row is not skill-scoped, so nothing removed
    /// it with the revision. It survived as an *unlinked* row, which is how
    /// the store spells "this turn ran with no library" — the purged skill's
    /// turns silently became no-library baseline evidence, turn IDs intact.
    #[test]
    fn privacy_purge_deletes_the_task_outcomes_the_purged_skill_alone_owned() {
        let (root, mut store) = fixture();
        let purged = artifact("function run() { return 1; }", "Purged telemetry owner");
        let kept = artifact(
            "function run() { return 1; } // kept",
            "Surviving telemetry sharer",
        );
        store.insert_verified(&purged).unwrap();
        store.insert_verified(&kept).unwrap();
        seed_telemetry(&mut store, &purged.id, &kept.id);
        assert_eq!(surviving_outcomes(&store).len(), 3);
        store.conn().execute_batch(
            "INSERT INTO skill_turn_losses SELECT turn_id, production, 10 FROM skill_task_outcomes;",
        ).unwrap();

        RetentionService::new(&mut store)
            .privacy_purge(&purged.id, "test_request", 10)
            .unwrap();

        assert_eq!(
            surviving_outcomes(&store),
            vec!["baseline".to_string(), "shared".to_string()],
            "only the outcome the purged skill alone claimed may be deleted"
        );
        assert_eq!(
            count_for(&store, "skill_task_outcome_links", &kept.id),
            1,
            "the surviving skill keeps its own attribution"
        );
        let losses = store
            .conn()
            .prepare("SELECT turn_id FROM skill_turn_losses ORDER BY turn_id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(losses, ["turn-baseline", "turn-shared"]);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The purge deletes telemetry itself rather than trusting
    /// `ON DELETE CASCADE`: the privacy guarantee must not rest on a
    /// per-connection `PRAGMA foreign_keys` being set.
    #[test]
    fn privacy_purge_deletes_telemetry_without_relying_on_cascade() {
        let (root, mut store) = fixture();
        let purged = artifact("function run() { return 1; }", "Purged telemetry owner");
        let kept = artifact(
            "function run() { return 1; } // kept",
            "Surviving telemetry sharer",
        );
        store.insert_verified(&purged).unwrap();
        store.insert_verified(&kept).unwrap();
        seed_telemetry(&mut store, &purged.id, &kept.id);
        let tables = [
            "skill_events",
            "skill_stats",
            "skill_daily_stats",
            "skill_feedback",
            "skill_task_outcome_links",
        ];
        for table in tables {
            assert!(
                count_for(&store, table, &purged.id) > 0,
                "{table} must be seeded for this test to mean anything"
            );
        }
        store
            .conn_mut()
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();

        RetentionService::new(&mut store)
            .privacy_purge(&purged.id, "test_request", 10)
            .unwrap();

        for table in tables {
            assert_eq!(
                count_for(&store, table, &purged.id),
                0,
                "{table} kept telemetry for a purged skill"
            );
        }
        assert_eq!(
            count_for(&store, "skill_task_outcome_links", &kept.id),
            1,
            "the surviving skill's own attribution must not be swept up"
        );
        store
            .conn_mut()
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
