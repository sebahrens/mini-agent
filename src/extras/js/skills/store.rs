//! SQLite persistence foundation for immutable learned-JS skill revisions.
//!
//! This module manages versioned database schema, identity-validating reads, and
//! transactional migrations. The database is never held by JsTool/JsRequest or the
//! QuickJS thread; it is purely for durable storage and offline indexing.
//!
//! Database path: `<AppPaths.local_data_dir>/skills/skills.db`
//! All operations return typed errors and never panic on corruption.

use crate::paths::AppPaths;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{CapabilityManifest, IdentityError, SKILL_ABI_VERSION, SkillArtifact, SkillExport};

/// Database schema version. Bump when schema changes; migrations bring older
/// databases forward idempotently.
pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 5;

/// Model-versioned vector loaded only while constructing an immutable index generation.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEmbedding {
    pub skill_id: String,
    pub model_id: String,
    pub model_revision: String,
    pub dimensions: usize,
    pub normalized: bool,
    pub values: Vec<f32>,
}

/// Operational metadata that is deliberately excluded from artifact identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRecordMetadata {
    pub status: String,
    pub quarantine_reason: Option<String>,
    pub supersedes_id: Option<String>,
    pub superseded_by_id: Option<String>,
    pub row_version: u64,
}

/// Durable generation metadata used by the off-request-path publication coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationState {
    pub desired_generation: u64,
    pub applied_generation: u64,
    pub model_id: String,
    pub model_revision: String,
    pub dimensions: usize,
    pub normalized: bool,
    pub publication_mode: String,
}

pub(crate) const MAX_EVALUATION_ATTEMPTS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProposalStatus {
    Pending,
    Evaluating,
    Verified,
    Rejected,
    AwaitingApproval,
    Approved,
}

impl ProposalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Evaluating => "evaluating",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Approved => "approved",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "evaluating" => Ok(Self::Evaluating),
            "verified" => Ok(Self::Verified),
            "rejected" => Ok(Self::Rejected),
            "awaiting_approval" => Ok(Self::AwaitingApproval),
            "approved" => Ok(Self::Approved),
            other => Err(StoreError::CorruptRow(format!(
                "unknown proposal status {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueStatus {
    Pending,
    Verified,
    Rejected,
    AwaitingApproval,
    Approved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnqueueResult {
    pub proposal_id: String,
    pub skill_id: String,
    pub status: EnqueueStatus,
    pub report_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProposalRecord {
    pub proposal_id: String,
    pub skill_id: String,
    pub predecessor_id: Option<String>,
    pub status: ProposalStatus,
    pub attempt_count: u32,
    pub next_attempt_at: Option<i64>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub report_id: Option<String>,
    pub reason_code: Option<String>,
    pub row_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProposalLease {
    pub proposal_id: String,
    pub skill_id: String,
    pub predecessor_id: Option<String>,
    pub attempt: u32,
    pub row_version: u64,
    pub lease_expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EvaluationReportRecord {
    pub report_id: String,
    pub proposal_id: String,
    pub skill_id: String,
    pub attempt: u32,
    pub verifier_version: u32,
    pub fakes_version: u32,
    pub suite_hashes: Vec<String>,
    pub predecessor_id: Option<String>,
    pub embedding_model_id: Option<String>,
    pub embedding_model_revision: Option<String>,
    pub outcome: String,
    pub reason_code: Option<String>,
    pub summary_json: String,
    pub created_at: i64,
}

impl EvaluationReportRecord {
    pub(crate) fn recompute_id(&self) -> Result<String, StoreError> {
        let summary = serde_json::from_str::<serde_json::Value>(&self.summary_json)?;
        let identity = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "proposal_id": self.proposal_id,
            "skill_id": self.skill_id,
            "attempt": self.attempt,
            "predecessor_id": self.predecessor_id,
            "verifier_version": self.verifier_version,
            "fakes_version": self.fakes_version,
            "suite_hashes": self.suite_hashes,
            "embedding_model_id": self.embedding_model_id,
            "embedding_model_revision": self.embedding_model_revision,
            "outcome": self.outcome,
            "reason_code": self.reason_code,
            "summary": summary,
            "created_at": self.created_at
        }))?;
        Ok(format!("{:x}", Sha256::digest(identity)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeldOutSuiteRecord {
    pub suite_id: String,
    pub selector_json: String,
    pub cases_json: String,
    pub content_hash: String,
    pub canonical_payload: String,
    pub approved_by: String,
    pub approved_at: i64,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CanaryApprovalInput {
    pub approval_id: String,
    pub proposal_id: String,
    pub skill_id: String,
    pub report_id: String,
    pub approver_id: String,
    pub authenticated_at: i64,
    pub expected_artifact_version: u64,
    pub expected_proposal_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanaryApprovalResult {
    pub skill_id: String,
    pub generation: u64,
    pub idempotent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdminIdentity(String);

impl AdminIdentity {
    pub(crate) fn authenticated(principal: &str) -> Result<Self, StoreError> {
        let principal = principal.trim();
        if principal.is_empty() || principal.len() > 256 {
            return Err(StoreError::Unauthorized);
        }
        Ok(Self(principal.to_string()))
    }

    fn principal(&self) -> &str {
        &self.0
    }
}

/// Skill store errors, typed for caller handling.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid skill identity: {0}")]
    IdentityValidation(#[from] IdentityError),

    #[error("malformed JSON in stored field: {0}")]
    MalformedJson(#[from] serde_json::Error),

    #[error("skill not found: {0}")]
    NotFound(String),

    #[error("skill identity collision: {0}")]
    Collision(String),

    #[error("skill was privacy-purged and cannot be resurrected: {0}")]
    Purged(String),

    #[error("stale skill row version for {id}: expected {expected}")]
    StaleVersion { id: String, expected: u64 },

    #[error("malformed embedding for {skill_id}: {reason}")]
    MalformedEmbedding { skill_id: String, reason: String },

    #[error("skill verification failed: {0}")]
    Verification(#[from] super::verify::VerificationError),

    #[error("skill already exists: {0}")]
    AlreadyExists(String),

    #[error("constraint violation: {0}")]
    Constraint(String),

    #[error("database locked or busy")]
    Busy,

    #[error("unauthorized operation")]
    Unauthorized,

    #[error("proposal lease lost: {0}")]
    LeaseLost(String),

    #[error("stale proposal state: {0}")]
    Stale(String),

    #[error("corrupt stored row: {0}")]
    CorruptRow(String),

    #[error("invalid held-out suite: {0}")]
    InvalidSuite(String),

    #[error("unsupported future schema version: {0}")]
    UnsupportedSchemaVersion(u32),

    #[error("identity-v1 artifact is quarantined: manifest_scope_required")]
    LegacyIdentityQuarantined,

    #[error("FTS5 is not available in this SQLite build")]
    MissingFts5,
}

/// Skill store — persistent durable state for immutable learned-JS revisions.
pub struct SkillStore {
    db: Connection,
    db_path: PathBuf,
}

impl SkillStore {
    /// Open or create the skill database at the canonical location.
    ///
    /// Creates private parent directories, enables foreign keys, and runs
    /// idempotent transactional migrations. Returns a typed error if the database
    /// is corrupted, locked, or has an unsupported schema version.
    pub fn open_at(paths: &AppPaths) -> Result<Self, StoreError> {
        let db_dir = paths.local_data_dir.join("skills");
        std::fs::create_dir_all(&db_dir)?;

        let db_path = db_dir.join("skills.db");
        let db = Connection::open(&db_path)?;

        // Enable foreign keys for referential integrity.
        db.execute_batch("PRAGMA foreign_keys = ON;")?;

        // Check and verify FTS5 is available.
        verify_fts5(&db)?;

        // Run migrations idempotently.
        // SAFETY: We just opened the database, so we have exclusive access.
        // NOTE: Rust's type system doesn't let us reborrow mut here; we work around
        // it by using interior mutability through SQLite's own locking.
        migrate(&db)?;

        Ok(Self { db, db_path })
    }

    /// Insert a verified skill artifact as active status.
    ///
    /// Recomputes and validates the identity before insertion. Caller identity
    /// is never trusted.
    pub fn insert_verified(&mut self, artifact: &SkillArtifact) -> Result<(), StoreError> {
        artifact.verify_identity()?;
        let verification = super::verify::verify_skill(artifact)?;
        if verification.skill_id != artifact.id
            || verification.identity_version != artifact.identity_version
            || verification.capability != artifact.capability
        {
            return Err(StoreError::Constraint(
                "verification report is not bound to the inserted artifact".to_string(),
            ));
        }
        tracing::debug!(
            skill_id = %artifact.id,
            verifier_version = verification.verifier_version,
            fakes_version = verification.fakes_version,
            memory_limit = verification.memory_limit,
            stack_limit = verification.stack_limit,
            timeout_ms = verification.timeout.as_millis(),
            tests = verification.test_results.len(),
            mutations = verification.mutation_outcomes.len(),
            fake_reads = verification.transcript.reads.len(),
            fake_writes = verification.transcript.writes.len(),
            fake_spawns = verification.transcript.spawns.len(),
            fake_fetches = verification.transcript.fetches.len(),
            "verified immutable learned-JS artifact before insertion"
        );

        let tags_json = serde_json::to_string(&artifact.tags)?;
        let exports_json = serialize_exports(&artifact.exports)?;
        let tests_json = serde_json::to_string(&artifact.tests)?;
        let capability_json = serialize_capability(artifact)?;

        let now = current_timestamp()?;

        let tx = self.db.transaction()?;
        let tombstoned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM skill_tombstones WHERE id = ?)",
            params![artifact.id],
            |row| row.get(0),
        )?;
        if tombstoned {
            return Err(StoreError::Purged(artifact.id.clone()));
        }

        let existing = tx
            .query_row(
                "SELECT id, identity_version, source, description, tags_json,
                        exports_json, tests_json, capability_json, status
                 FROM skill_revisions WHERE id = ?",
                params![artifact.id],
                read_artifact_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            let existing = existing?;
            existing
                .verify_identity()
                .map_err(|_| StoreError::Collision(artifact.id.clone()))?;
            if &existing == artifact {
                return Ok(());
            }
            return Err(StoreError::Collision(artifact.id.clone()));
        }

        tx.execute(
            "INSERT INTO skill_revisions (
                id, identity_version, source, description, tags_json,
                exports_json, tests_json, capability_json, status,
                row_version, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                artifact.id,
                artifact.identity_version,
                artifact.source,
                artifact.description,
                tags_json,
                exports_json,
                tests_json,
                capability_json,
                "active",
                1i64,
                now,
                now
            ],
        )
        .map_err(|e| {
            if e.to_string().contains("CONSTRAINT") {
                StoreError::Constraint(e.to_string())
            } else {
                StoreError::Sqlite(e)
            }
        })?;

        // Insert empty embedding records initially; they will be filled by the embed module.
        // For now just mark the insertion complete.

        tx.commit()?;
        Ok(())
    }

    /// Retrieve a skill by ID with identity validation.
    ///
    /// Recomputes the canonical identity and refuses to return source for a
    /// tampered row. Returns None if the skill does not exist.
    pub fn get(&self, id: &str) -> Result<Option<SkillArtifact>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT id, identity_version, source, description, tags_json,
                    exports_json, tests_json, capability_json, status
             FROM skill_revisions WHERE id = ?",
        )?;

        let row = stmt.query_row(params![id], read_artifact_row).optional()?;

        match row {
            Some(Ok(artifact)) => {
                artifact.verify_identity()?;
                Ok(Some(artifact))
            }
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// List all active retrievable skills with identity validation.
    ///
    /// In Phase 3, only 'active' status rows are retrievable. Recomputes identity
    /// for each row; invalid rows are skipped and never returned to retrieval.
    pub fn list_retrievable(&self) -> Result<Vec<SkillArtifact>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT id, identity_version, source, description, tags_json,
                    exports_json, tests_json, capability_json, status
             FROM skill_revisions WHERE status = 'active'
               AND identity_version = 2
             ORDER BY id",
        )?;

        let rows = stmt.query_map([], read_artifact_row)?;

        let mut artifacts = Vec::new();
        for result in rows {
            match result? {
                Ok(artifact) => {
                    // Silently skip invalid rows; they cannot be indexed.
                    if artifact.verify_identity().is_ok() {
                        artifacts.push(artifact);
                    }
                }
                Err(_) => {
                    // Skip rows with JSON decode errors.
                }
            }
        }
        Ok(artifacts)
    }

    pub fn active_count(&self) -> Result<usize, StoreError> {
        let count = self.db.query_row(
            "SELECT COUNT(*) FROM skill_revisions
              WHERE status = 'active' AND identity_version = 2",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        usize::try_from(count)
            .map_err(|_| StoreError::Constraint("active skill count is invalid".to_string()))
    }

    /// Load one identity-checked active generation with compatible embeddings.
    ///
    /// Snapshot construction uses this joined scan instead of issuing metadata
    /// and embedding queries for every artifact in the corpus.
    pub fn snapshot_rows(
        &self,
        model_id: &str,
        model_revision: &str,
    ) -> Result<Vec<(SkillArtifact, Option<StoredEmbedding>, SkillRecordMetadata)>, StoreError>
    {
        let mut statement = self.db.prepare(
            "SELECT r.id, r.identity_version, r.source, r.description, r.tags_json,
                    r.exports_json, r.tests_json, r.capability_json, r.status,
                    r.supersedes_id, r.superseded_by_id, r.row_version,
                    e.dimensions, e.normalized, e.embedding
             FROM skill_revisions r
             LEFT JOIN skill_embeddings e
               ON e.skill_id = r.id AND e.model_id = ? AND e.model_revision = ?
             WHERE r.status = 'active' AND r.identity_version = 2
             ORDER BY r.id",
        )?;
        let rows = statement.query_map(params![model_id, model_revision], |row| {
            Ok((
                read_artifact_row(row)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<Vec<u8>>>(14)?,
            ))
        })?;

        let mut snapshot = Vec::new();
        for row in rows {
            let (
                artifact,
                supersedes_id,
                superseded_by_id,
                row_version,
                dimensions,
                normalized,
                bytes,
            ) = row?;
            let Ok(artifact) = artifact else {
                continue;
            };
            if artifact.verify_identity().is_err() {
                continue;
            }
            let metadata = SkillRecordMetadata {
                status: "active".to_string(),
                quarantine_reason: None,
                supersedes_id,
                superseded_by_id,
                row_version: u64::try_from(row_version).unwrap_or(0),
            };
            let embedding = match (dimensions, normalized, bytes) {
                (None, None, None) => None,
                (Some(dimensions), Some(normalized), Some(bytes)) => {
                    let dimensions = usize::try_from(dimensions).map_err(|_| {
                        StoreError::MalformedEmbedding {
                            skill_id: artifact.id.clone(),
                            reason: "dimensions are outside the supported range".to_string(),
                        }
                    })?;
                    let normalized = normalized == 1;
                    validate_embedding_bytes(&artifact.id, dimensions, normalized, &bytes)?;
                    Some(StoredEmbedding {
                        skill_id: artifact.id.clone(),
                        model_id: model_id.to_string(),
                        model_revision: model_revision.to_string(),
                        dimensions,
                        normalized,
                        values: decode_embedding(&bytes),
                    })
                }
                _ => {
                    return Err(StoreError::MalformedEmbedding {
                        skill_id: artifact.id.clone(),
                        reason: "embedding row is partially null".to_string(),
                    });
                }
            };
            snapshot.push((artifact, embedding, metadata));
        }
        Ok(snapshot)
    }

    /// Store an embedding vector for a skill.
    ///
    /// Keyed by (skill_id, model_id, model_revision). Incompatible vectors remain
    /// durable but ineligible for retrieval (enforced by caller).
    pub fn store_embedding(
        &mut self,
        skill_id: &str,
        model_id: &str,
        model_revision: &str,
        dimensions: u32,
        normalized: bool,
        embedding: &[u8],
    ) -> Result<(), StoreError> {
        validate_embedding_bytes(skill_id, dimensions as usize, normalized, embedding)?;
        self.get(skill_id)?
            .ok_or_else(|| StoreError::NotFound(skill_id.to_string()))?;
        let now = current_timestamp()?;
        let normalized_int = if normalized { 1 } else { 0 };

        self.db.execute(
            "INSERT OR REPLACE INTO skill_embeddings (
                skill_id, model_id, model_revision, dimensions,
                normalized, embedding, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                skill_id,
                model_id,
                model_revision,
                dimensions as i64,
                normalized_int,
                embedding,
                now
            ],
        )?;

        Ok(())
    }

    /// Load one compatible vector for immutable snapshot construction.
    pub fn get_embedding(
        &self,
        skill_id: &str,
        model_id: &str,
        model_revision: &str,
    ) -> Result<Option<StoredEmbedding>, StoreError> {
        let row = self
            .db
            .query_row(
                "SELECT dimensions, normalized, embedding
             FROM skill_embeddings
             WHERE skill_id = ? AND model_id = ? AND model_revision = ?",
                params![skill_id, model_id, model_revision],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;

        let Some((dimensions, normalized, bytes)) = row else {
            return Ok(None);
        };
        let dimensions =
            usize::try_from(dimensions).map_err(|_| StoreError::MalformedEmbedding {
                skill_id: skill_id.to_string(),
                reason: "dimensions are outside the supported range".to_string(),
            })?;
        let normalized = normalized == 1;
        validate_embedding_bytes(skill_id, dimensions, normalized, &bytes)?;
        Ok(Some(StoredEmbedding {
            skill_id: skill_id.to_string(),
            model_id: model_id.to_string(),
            model_revision: model_revision.to_string(),
            dimensions,
            normalized,
            values: decode_embedding(&bytes),
        }))
    }

    /// Operational lifecycle metadata for optimistic updates and lineage dedupe.
    pub fn metadata(&self, id: &str) -> Result<Option<SkillRecordMetadata>, StoreError> {
        self.db
            .query_row(
                "SELECT status, quarantine_reason, supersedes_id, superseded_by_id, row_version
             FROM skill_revisions WHERE id = ?",
                params![id],
                |row| {
                    let row_version: i64 = row.get(4)?;
                    Ok(SkillRecordMetadata {
                        status: row.get(0)?,
                        quarantine_reason: row.get(1)?,
                        supersedes_id: row.get(2)?,
                        superseded_by_id: row.get(3)?,
                        row_version: u64::try_from(row_version).unwrap_or(0),
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Optimistically retire an active skill without deleting identity or lineage bytes.
    pub fn retire(&mut self, id: &str, expected_version: u64) -> Result<(), StoreError> {
        let now = current_timestamp()?;
        let changed = self.db.execute(
            "UPDATE skill_revisions
             SET status = 'retired', row_version = row_version + 1, updated_at = ?
             WHERE id = ? AND status = 'active' AND row_version = ?",
            params![now, id, expected_version as i64],
        )?;
        if changed == 1 {
            return Ok(());
        }
        if self.metadata(id)?.is_none() {
            Err(StoreError::NotFound(id.to_string()))
        } else {
            Err(StoreError::StaleVersion {
                id: id.to_string(),
                expected: expected_version,
            })
        }
    }

    /// Privacy purge all durable bytes and leave a non-secret anti-resurrection tombstone.
    pub fn purge(&mut self, id: &str) -> Result<(), StoreError> {
        let now = current_timestamp()?;
        let tx = self.db.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM skill_revisions WHERE id = ?)",
            params![id],
            |row| row.get(0),
        )?;
        if !exists {
            let tombstoned: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM skill_tombstones WHERE id = ?)",
                params![id],
                |row| row.get(0),
            )?;
            return if tombstoned {
                Ok(())
            } else {
                Err(StoreError::NotFound(id.to_string()))
            };
        }
        tx.execute(
            "UPDATE skill_revisions SET supersedes_id = NULL, superseded_by_id = NULL
             WHERE supersedes_id = ? OR superseded_by_id = ?",
            params![id, id],
        )?;
        tx.execute("DELETE FROM skill_revisions WHERE id = ?", params![id])?;
        // Legacy databases may contain malformed identifiers. Privacy deletion must
        // still succeed for those rows, but retaining attacker-controlled/raw IDs in
        // the tombstone set would violate the v2 identity constraint.
        if is_full_skill_id(id) {
            tx.execute(
                "INSERT OR IGNORE INTO skill_tombstones (id, purged_at) VALUES (?, ?)",
                params![id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read persisted desired/applied generation and active model metadata.
    pub fn generation_state(&self) -> Result<GenerationState, StoreError> {
        self.db
            .query_row(
                "SELECT desired_generation, applied_generation, model_id, model_revision,
                    dimensions, normalized, publication_mode
             FROM skill_generations WHERE singleton = 1",
                [],
                |row| {
                    let desired: i64 = row.get(0)?;
                    let applied: i64 = row.get(1)?;
                    let dimensions: i64 = row.get(4)?;
                    Ok(GenerationState {
                        desired_generation: u64::try_from(desired).unwrap_or(0),
                        applied_generation: u64::try_from(applied).unwrap_or(0),
                        model_id: row.get(2)?,
                        model_revision: row.get(3)?,
                        dimensions: usize::try_from(dimensions).unwrap_or(0),
                        normalized: row.get::<_, i64>(5)? == 1,
                        publication_mode: row.get(6)?,
                    })
                },
            )
            .map_err(StoreError::from)
    }

    /// Monotonically request a new generation and bind its exact model metadata.
    pub fn request_generation(
        &mut self,
        model_id: &str,
        model_revision: &str,
        dimensions: usize,
        normalized: bool,
    ) -> Result<u64, StoreError> {
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE skill_generations
             SET desired_generation = desired_generation + 1,
                 model_id = ?, model_revision = ?, dimensions = ?, normalized = ?
             WHERE singleton = 1",
            params![
                model_id,
                model_revision,
                dimensions as i64,
                normalized as i64
            ],
        )?;
        let desired = tx.query_row(
            "SELECT desired_generation FROM skill_generations WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        tx.commit()?;
        u64::try_from(desired)
            .map_err(|_| StoreError::Constraint("desired generation became negative".to_string()))
    }

    /// Mark exactly the current desired generation as durable before atomic publication.
    pub fn mark_generation_applied(&mut self, generation: u64) -> Result<(), StoreError> {
        self.mark_generation_applied_with_mode(generation, "full", None)
    }

    pub(crate) fn mark_generation_applied_with_mode(
        &mut self,
        generation: u64,
        publication_mode: &str,
        last_error_code: Option<&str>,
    ) -> Result<(), StoreError> {
        if !matches!(publication_mode, "full" | "removal_only") {
            return Err(StoreError::Constraint(
                "invalid index publication mode".to_string(),
            ));
        }
        let now = current_timestamp()?;
        let changed = self.db.execute(
            "UPDATE skill_generations
             SET applied_generation = ?, publication_mode = ?,
                 last_error_code = ?, updated_at = ?
             WHERE singleton = 1 AND desired_generation = ? AND applied_generation <= ?",
            params![
                generation as i64,
                publication_mode,
                last_error_code,
                now,
                generation as i64,
                generation as i64
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::Constraint(format!(
                "generation {generation} is no longer the desired generation"
            )))
        }
    }

    pub(crate) fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self
            .db
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Persist one immutable pending revision and its queue record atomically.
    ///
    /// Repeated submission of byte-identical canonical content is idempotent.
    /// A rejected identity stays rejected and returns its original report.
    pub(crate) fn enqueue_proposal(
        &mut self,
        artifact: &SkillArtifact,
        predecessor_id: Option<&str>,
        now: i64,
    ) -> Result<EnqueueResult, StoreError> {
        artifact.verify_identity()?;
        validate_full_id(predecessor_id)?;

        let tx = self.db.transaction()?;
        let tombstoned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM skill_tombstones WHERE id = ?1)",
            [&artifact.id],
            |row| row.get(0),
        )?;
        if tombstoned {
            return Err(StoreError::Purged(artifact.id.clone()));
        }
        if let Some(predecessor_id) = predecessor_id {
            let predecessor_status: Option<String> = tx
                .query_row(
                    "SELECT status FROM skill_revisions WHERE id = ?1",
                    [predecessor_id],
                    |row| row.get(0),
                )
                .optional()?;
            if !matches!(
                predecessor_status.as_deref(),
                Some("active" | "canary" | "quarantined")
            ) {
                return Err(StoreError::Constraint(
                    "predecessor must be an active, canary, or quarantined immutable revision"
                        .to_string(),
                ));
            }
        }

        let existing = tx
            .query_row(
                "SELECT id, identity_version, source, description, tags_json,
                        exports_json, tests_json, capability_json, status
                 FROM skill_revisions WHERE id = ?1",
                [&artifact.id],
                read_artifact_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            let existing = existing?;
            existing.verify_identity()?;
            if existing != *artifact {
                return Err(StoreError::Constraint(format!(
                    "identity collision for {}",
                    artifact.id
                )));
            }
            let record = tx
                .query_row(
                    "SELECT proposal_id, skill_id, predecessor_id, status, attempt_count,
                            next_attempt_at, lease_owner, lease_expires_at, report_id,
                            reason_code, row_version
                     FROM skill_proposals WHERE skill_id = ?1",
                    [&artifact.id],
                    read_proposal_row,
                )
                .optional()?
                .ok_or_else(|| StoreError::AlreadyExists(artifact.id.clone()))?;
            if record.predecessor_id.as_deref() != predecessor_id {
                return Err(StoreError::Constraint(
                    "an existing proposal cannot be rebound to a different predecessor".to_string(),
                ));
            }
            let status = enqueue_status(record.status)?;
            tx.commit()?;
            return Ok(EnqueueResult {
                proposal_id: record.proposal_id,
                skill_id: record.skill_id,
                status,
                report_id: record.report_id,
            });
        }

        insert_revision(&tx, artifact, "pending", now)?;
        tx.execute(
            "INSERT INTO skill_proposals (
                proposal_id, skill_id, predecessor_id, proposed_at, status,
                attempt_count, row_version, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, 'pending', 0, 1, ?4, ?4)",
            params![artifact.id, artifact.id, predecessor_id, now],
        )?;
        tx.commit()?;

        Ok(EnqueueResult {
            proposal_id: artifact.id.clone(),
            skill_id: artifact.id.clone(),
            status: EnqueueStatus::Pending,
            report_id: None,
        })
    }

    pub(crate) fn get_proposal(
        &self,
        proposal_id: &str,
    ) -> Result<Option<ProposalRecord>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT proposal_id, skill_id, predecessor_id, status, attempt_count,
                        next_attempt_at, lease_owner, lease_expires_at, report_id,
                        reason_code, row_version
                 FROM skill_proposals WHERE proposal_id = ?1",
                [proposal_id],
                read_proposal_row,
            )
            .optional()?)
    }

    /// Claim the oldest due proposal. Expired evaluating leases are reclaimable.
    pub(crate) fn claim_due_proposal(
        &mut self,
        worker: &str,
        now: i64,
        lease_seconds: i64,
    ) -> Result<Option<ProposalLease>, StoreError> {
        if worker.trim().is_empty() || lease_seconds <= 0 {
            return Err(StoreError::Constraint(
                "worker and positive lease duration are required".to_string(),
            ));
        }
        let lease_expires_at = now
            .checked_add(lease_seconds)
            .ok_or_else(|| StoreError::Constraint("lease deadline overflow".to_string()))?;
        let tx = self.db.transaction()?;
        let candidate: Option<(String, String, Option<String>, u32, i64)> = tx
            .query_row(
                "SELECT proposal_id, skill_id, predecessor_id, attempt_count, row_version
                 FROM skill_proposals
                 WHERE attempt_count < ?1
                   AND (
                     (status = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2))
                     OR
                     (status = 'evaluating' AND lease_expires_at <= ?2)
                   )
                 ORDER BY proposed_at, proposal_id
                 LIMIT 1",
                params![MAX_EVALUATION_ATTEMPTS, now],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((proposal_id, skill_id, predecessor_id, attempt_count, row_version)) = candidate
        else {
            tx.commit()?;
            return Ok(None);
        };
        if row_version <= 0 {
            return Err(StoreError::CorruptRow(
                "proposal row version must be positive".to_string(),
            ));
        }
        let attempt = attempt_count + 1;
        let next_version = row_version + 1;
        let changed = tx.execute(
            "UPDATE skill_proposals
             SET status = 'evaluating', attempt_count = ?1, next_attempt_at = NULL,
                 lease_owner = ?2, lease_expires_at = ?3, row_version = ?4, updated_at = ?5
             WHERE proposal_id = ?6 AND row_version = ?7
               AND (
                 (status = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?5))
                 OR
                 (status = 'evaluating' AND lease_expires_at <= ?5)
               )",
            params![
                attempt,
                worker,
                lease_expires_at,
                next_version,
                now,
                proposal_id,
                row_version
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Stale(proposal_id));
        }
        tx.commit()?;
        Ok(Some(ProposalLease {
            proposal_id,
            skill_id,
            predecessor_id,
            attempt,
            row_version: next_version as u64,
            lease_expires_at,
        }))
    }

    pub(crate) fn renew_lease(
        &mut self,
        proposal_id: &str,
        worker: &str,
        now: i64,
        lease_seconds: i64,
    ) -> Result<u64, StoreError> {
        let expires_at = now
            .checked_add(lease_seconds)
            .ok_or_else(|| StoreError::Constraint("lease deadline overflow".to_string()))?;
        let changed = self.db.execute(
            "UPDATE skill_proposals
             SET lease_expires_at = ?1, row_version = row_version + 1, updated_at = ?2
             WHERE proposal_id = ?3 AND status = 'evaluating'
               AND lease_owner = ?4 AND lease_expires_at > ?2",
            params![expires_at, now, proposal_id, worker],
        )?;
        if changed != 1 {
            return Err(StoreError::LeaseLost(proposal_id.to_string()));
        }
        let version: i64 = self
            .db
            .query_row(
                "SELECT row_version FROM skill_proposals WHERE proposal_id = ?1",
                [proposal_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        u64::try_from(version)
            .map_err(|_| StoreError::CorruptRow("negative proposal row version".to_string()))
    }

    pub(crate) fn retry_proposal(
        &mut self,
        proposal_id: &str,
        worker: &str,
        row_version: u64,
        next_attempt_at: i64,
        now: i64,
    ) -> Result<(), StoreError> {
        let changed = self.db.execute(
            "UPDATE skill_proposals
             SET status = 'pending', next_attempt_at = ?1, lease_owner = NULL,
                 lease_expires_at = NULL, row_version = row_version + 1, updated_at = ?2
             WHERE proposal_id = ?3 AND status = 'evaluating'
               AND lease_owner = ?4 AND row_version = ?5",
            params![
                next_attempt_at,
                now,
                proposal_id,
                worker,
                sql_version(row_version)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::LeaseLost(proposal_id.to_string()));
        }
        Ok(())
    }

    pub(crate) fn complete_evaluation(
        &mut self,
        proposal_id: &str,
        worker: &str,
        row_version: u64,
        report: &EvaluationReportRecord,
        now: i64,
    ) -> Result<(), StoreError> {
        validate_report_binding(proposal_id, report)?;
        let tx = self.db.transaction()?;
        insert_report(&tx, report)?;
        let changed = tx.execute(
            "UPDATE skill_proposals
             SET status = 'awaiting_approval', report_id = ?1, reason_code = NULL,
                 lease_owner = NULL, lease_expires_at = NULL,
                 row_version = row_version + 1, updated_at = ?2
             WHERE proposal_id = ?3 AND skill_id = ?4 AND status = 'evaluating'
               AND lease_owner = ?5 AND row_version = ?6",
            params![
                report.report_id,
                now,
                proposal_id,
                report.skill_id,
                worker,
                sql_version(row_version)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::LeaseLost(proposal_id.to_string()));
        }
        let revision_changed = tx.execute(
            "UPDATE skill_revisions
             SET status = 'verified', row_version = row_version + 1, updated_at = ?1
             WHERE id = ?2 AND status = 'pending'",
            params![now, report.skill_id],
        )?;
        if revision_changed != 1 {
            return Err(StoreError::Stale(report.skill_id.clone()));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn reject_proposal(
        &mut self,
        proposal_id: &str,
        worker: &str,
        row_version: u64,
        report: &EvaluationReportRecord,
        reason_code: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        validate_report_binding(proposal_id, report)?;
        if reason_code.trim().is_empty() || report.reason_code.as_deref() != Some(reason_code) {
            return Err(StoreError::Constraint(
                "rejection report reason binding is invalid".to_string(),
            ));
        }

        let tx = self.db.transaction()?;
        let existing: Option<(String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT status, report_id, reason_code
                 FROM skill_proposals WHERE proposal_id = ?1",
                [proposal_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if matches!(
            existing,
            Some((ref status, ref report_id, ref reason))
                if status == "rejected"
                    && report_id.as_deref() == Some(report.report_id.as_str())
                    && reason.as_deref() == Some(reason_code)
        ) {
            tx.commit()?;
            return Ok(());
        }

        insert_report(&tx, report)?;
        let changed = tx.execute(
            "UPDATE skill_proposals
             SET status = 'rejected', report_id = ?1, reason_code = ?2,
                 lease_owner = NULL, lease_expires_at = NULL,
                 row_version = row_version + 1, updated_at = ?3
             WHERE proposal_id = ?4 AND skill_id = ?5 AND status = 'evaluating'
               AND lease_owner = ?6 AND row_version = ?7",
            params![
                report.report_id,
                reason_code,
                now,
                proposal_id,
                report.skill_id,
                worker,
                sql_version(row_version)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::LeaseLost(proposal_id.to_string()));
        }
        let revision_changed = tx.execute(
            "UPDATE skill_revisions
             SET status = 'rejected', row_version = row_version + 1, updated_at = ?1
             WHERE id = ?2 AND status = 'pending'",
            params![now, report.skill_id],
        )?;
        if revision_changed != 1 {
            return Err(StoreError::Stale(report.skill_id.clone()));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn import_held_out_suite(
        &mut self,
        admin: Option<&AdminIdentity>,
        suite: &HeldOutSuiteRecord,
        now: i64,
    ) -> Result<(), StoreError> {
        let admin = admin.ok_or(StoreError::Unauthorized)?;
        let content_hash = format!("{:x}", Sha256::digest(suite.canonical_payload.as_bytes()));
        if suite.suite_id != content_hash || suite.content_hash != content_hash {
            return Err(StoreError::InvalidSuite(
                "suite ID and content hash must match canonical payload".to_string(),
            ));
        }
        serde_json::from_str::<serde_json::Value>(&suite.selector_json)?;
        serde_json::from_str::<serde_json::Value>(&suite.cases_json)?;
        self.db
            .execute(
                "INSERT INTO held_out_suites (
                suite_id, selector_json, cases_json, canonical_payload, approved_by,
                approved_at, content_hash, enabled, row_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
                params![
                    suite.suite_id,
                    suite.selector_json,
                    suite.cases_json,
                    suite.canonical_payload,
                    admin.principal(),
                    now,
                    content_hash,
                    i64::from(suite.enabled)
                ],
            )
            .or_else(|error| {
                if error.to_string().contains("UNIQUE") {
                    let existing: Option<String> = self
                        .db
                        .query_row(
                            "SELECT content_hash FROM held_out_suites WHERE suite_id = ?1",
                            [&suite.suite_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if existing.as_deref() == Some(content_hash.as_str()) {
                        Ok(0)
                    } else {
                        Err(error)
                    }
                } else {
                    Err(error)
                }
            })?;
        Ok(())
    }

    /// Trusted evaluator-only suite access. Proposal/model APIs do not expose this.
    pub(crate) fn enabled_held_out_suites(&self) -> Result<Vec<HeldOutSuiteRecord>, StoreError> {
        let mut stmt = self.db.prepare(
            "SELECT suite_id, selector_json, cases_json, content_hash, canonical_payload,
                    approved_by, approved_at, enabled
             FROM held_out_suites
             WHERE enabled = 1
             ORDER BY suite_id",
        )?;
        let rows = stmt.query_map([], read_held_out_suite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub(crate) fn revision_status(&self, id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .db
            .query_row(
                "SELECT status FROM skill_revisions WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn revision_row_version(&self, id: &str) -> Result<Option<u64>, StoreError> {
        let version: Option<i64> = self
            .db
            .query_row(
                "SELECT row_version FROM skill_revisions WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        version
            .map(|version| {
                u64::try_from(version).map_err(|_| {
                    StoreError::CorruptRow("negative artifact row version".to_string())
                })
            })
            .transpose()
    }

    pub(crate) fn get_evaluation_report(
        &self,
        report_id: &str,
    ) -> Result<Option<EvaluationReportRecord>, StoreError> {
        let report = self
            .db
            .query_row(
                "SELECT report_id, proposal_id, skill_id, attempt, verifier_version,
                        fakes_version, suite_hashes_json, predecessor_id,
                        embedding_model_id, embedding_model_revision, outcome,
                        reason_code, summary_json, created_at
                 FROM evaluation_reports WHERE report_id = ?1",
                [report_id],
                read_evaluation_report,
            )
            .optional()?;
        if let Some(report) = report.as_ref()
            && report.recompute_id()? != report.report_id
        {
            return Err(StoreError::CorruptRow(
                "evaluation report identity mismatch".to_string(),
            ));
        }
        Ok(report)
    }

    pub(crate) fn has_compatible_embedding(
        &self,
        skill_id: &str,
        model_id: &str,
        model_revision: &str,
    ) -> Result<bool, StoreError> {
        let row: Option<(i64, i64, i64)> = self
            .db
            .query_row(
                "SELECT dimensions, normalized, length(embedding)
                 FROM skill_embeddings
                 WHERE skill_id = ?1 AND model_id = ?2 AND model_revision = ?3",
                params![skill_id, model_id, model_revision],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(matches!(
            row,
            Some((dimensions, normalized, byte_len))
                if dimensions > 0
                    && normalized == 1
                    && usize::try_from(dimensions)
                        .ok()
                        .and_then(|value| value.checked_mul(std::mem::size_of::<f32>()))
                        == usize::try_from(byte_len).ok()
        ))
    }

    pub(crate) fn canary_approval_result(
        &self,
        proposal_id: &str,
    ) -> Result<Option<CanaryApprovalResult>, StoreError> {
        let row: Option<(String, i64)> = self
            .db
            .query_row(
                "SELECT skill_id, generation FROM skill_approvals WHERE proposal_id = ?1",
                [proposal_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(skill_id, generation)| {
            Ok(CanaryApprovalResult {
                skill_id,
                generation: u64::try_from(generation)
                    .map_err(|_| StoreError::CorruptRow("negative generation".to_string()))?,
                idempotent: true,
            })
        })
        .transpose()
    }

    pub(super) fn approve_canary_transaction(
        &mut self,
        input: &CanaryApprovalInput,
        now: i64,
    ) -> Result<CanaryApprovalResult, StoreError> {
        if input.approval_id.trim().is_empty()
            || input.approver_id.trim().is_empty()
            || input.authenticated_at > now
        {
            return Err(StoreError::Unauthorized);
        }
        let tx = self.db.transaction()?;
        let existing: Option<(String, String, String, i64)> = tx
            .query_row(
                "SELECT proposal_id, skill_id, report_id, generation
                 FROM skill_approvals WHERE approval_id = ?1",
                [&input.approval_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((proposal_id, skill_id, report_id, generation)) = existing {
            if proposal_id == input.proposal_id
                && skill_id == input.skill_id
                && report_id == input.report_id
            {
                tx.commit()?;
                return Ok(CanaryApprovalResult {
                    skill_id,
                    generation: u64::try_from(generation)
                        .map_err(|_| StoreError::CorruptRow("negative generation".to_string()))?,
                    idempotent: true,
                });
            }
            return Err(StoreError::Constraint(
                "approval identity collision".to_string(),
            ));
        }

        let stored_artifact = tx
            .query_row(
                "SELECT id, identity_version, source, description, tags_json,
                        exports_json, tests_json, capability_json, status
                 FROM skill_revisions WHERE id = ?1",
                [&input.skill_id],
                read_artifact_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(input.skill_id.clone()))??;
        stored_artifact.verify_identity()?;

        let proposal = tx
            .query_row(
                "SELECT proposal_id, skill_id, predecessor_id, status, attempt_count,
                        next_attempt_at, lease_owner, lease_expires_at, report_id,
                        reason_code, row_version
                 FROM skill_proposals WHERE proposal_id = ?1",
                [&input.proposal_id],
                read_proposal_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(input.proposal_id.clone()))?;
        if proposal.status != ProposalStatus::AwaitingApproval
            || proposal.skill_id != input.skill_id
            || proposal.report_id.as_deref() != Some(input.report_id.as_str())
            || proposal.row_version != input.expected_proposal_version
        {
            return Err(StoreError::Stale(input.proposal_id.clone()));
        }
        let report_skill: Option<(String, String, String)> = tx
            .query_row(
                "SELECT proposal_id, skill_id, outcome
                 FROM evaluation_reports WHERE report_id = ?1",
                [&input.report_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if !matches!(
            report_skill,
            Some((ref proposal_id, ref skill_id, ref outcome))
                if proposal_id == &input.proposal_id
                    && skill_id == &input.skill_id
                    && outcome == "passed"
        ) {
            return Err(StoreError::Stale(input.report_id.clone()));
        }

        let artifact_version: i64 = tx.query_row(
            "SELECT row_version FROM skill_revisions WHERE id = ?1",
            [&input.skill_id],
            |row| row.get(0),
        )?;
        if u64::try_from(artifact_version).ok() != Some(input.expected_artifact_version) {
            return Err(StoreError::Stale(input.skill_id.clone()));
        }
        let generation: i64 = tx.query_row(
            "SELECT desired_generation FROM skill_generations WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let next_generation = generation
            .checked_add(1)
            .ok_or_else(|| StoreError::Constraint("generation overflow".to_string()))?;

        let revision_changed = tx.execute(
            "UPDATE skill_revisions
             SET status = 'canary', row_version = row_version + 1, updated_at = ?1
             WHERE id = ?2 AND status = 'verified' AND row_version = ?3",
            params![now, input.skill_id, artifact_version],
        )?;
        if revision_changed != 1 {
            return Err(StoreError::Stale(input.skill_id.clone()));
        }
        let proposal_changed = tx.execute(
            "UPDATE skill_proposals
             SET status = 'approved', row_version = row_version + 1, updated_at = ?1
             WHERE proposal_id = ?2 AND skill_id = ?3 AND status = 'awaiting_approval'
               AND report_id = ?4 AND row_version = ?5",
            params![
                now,
                input.proposal_id,
                input.skill_id,
                input.report_id,
                sql_version(input.expected_proposal_version)?
            ],
        )?;
        if proposal_changed != 1 {
            return Err(StoreError::Stale(input.proposal_id.clone()));
        }
        tx.execute(
            "INSERT INTO skill_approvals (
                approval_id, proposal_id, skill_id, report_id, approver_id,
                authenticated_at, approved_at, artifact_version,
                proposal_version, generation
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                input.approval_id,
                input.proposal_id,
                input.skill_id,
                input.report_id,
                input.approver_id,
                input.authenticated_at,
                now,
                artifact_version,
                sql_version(input.expected_proposal_version)?,
                next_generation
            ],
        )?;
        let generation_changed = tx.execute(
            "UPDATE skill_generations
             SET desired_generation = ?1, row_version = row_version + 1, updated_at = ?2
             WHERE singleton = 1 AND desired_generation = ?3",
            params![next_generation, now, generation],
        )?;
        if generation_changed != 1 {
            return Err(StoreError::Stale("desired generation".to_string()));
        }
        tx.commit()?;
        Ok(CanaryApprovalResult {
            skill_id: input.skill_id.clone(),
            generation: next_generation as u64,
            idempotent: false,
        })
    }

    pub(super) fn deny_approval_transaction(
        &mut self,
        proposal_id: &str,
        expected_proposal_version: u64,
        reason_code: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        if reason_code.trim().is_empty() {
            return Err(StoreError::Constraint(
                "denial reason is required".to_string(),
            ));
        }
        let tx = self.db.transaction()?;
        let skill_id: String = tx
            .query_row(
                "SELECT skill_id FROM skill_proposals
                 WHERE proposal_id = ?1 AND status = 'awaiting_approval'
                   AND row_version = ?2",
                params![proposal_id, sql_version(expected_proposal_version)?],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::Stale(proposal_id.to_string()))?;
        let proposal_changed = tx.execute(
            "UPDATE skill_proposals
             SET status = 'rejected', reason_code = ?1,
                 row_version = row_version + 1, updated_at = ?2
             WHERE proposal_id = ?3 AND status = 'awaiting_approval'
               AND row_version = ?4",
            params![
                reason_code,
                now,
                proposal_id,
                sql_version(expected_proposal_version)?
            ],
        )?;
        let revision_changed = tx.execute(
            "UPDATE skill_revisions
             SET status = 'rejected', row_version = row_version + 1, updated_at = ?1
             WHERE id = ?2 AND status = 'verified'",
            params![now, skill_id],
        )?;
        if proposal_changed != 1 || revision_changed != 1 {
            return Err(StoreError::Stale(proposal_id.to_string()));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn complete_blocked_evaluation(
        &mut self,
        proposal_id: &str,
        worker: &str,
        row_version: u64,
        report: &EvaluationReportRecord,
        reason_code: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        validate_report_binding(proposal_id, report)?;
        let tx = self.db.transaction()?;
        insert_report(&tx, report)?;
        let proposal_changed = tx.execute(
            "UPDATE skill_proposals
             SET status = 'verified', report_id = ?1, reason_code = ?2,
                 lease_owner = NULL, lease_expires_at = NULL,
                 row_version = row_version + 1, updated_at = ?3
             WHERE proposal_id = ?4 AND skill_id = ?5 AND status = 'evaluating'
               AND lease_owner = ?6 AND row_version = ?7",
            params![
                report.report_id,
                reason_code,
                now,
                proposal_id,
                report.skill_id,
                worker,
                sql_version(row_version)?
            ],
        )?;
        let revision_changed = tx.execute(
            "UPDATE skill_revisions
             SET status = 'verified', row_version = row_version + 1, updated_at = ?1
             WHERE id = ?2 AND status = 'pending'",
            params![now, report.skill_id],
        )?;
        if proposal_changed != 1 || revision_changed != 1 {
            return Err(StoreError::LeaseLost(proposal_id.to_string()));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn request_blocked_reevaluation(
        &mut self,
        admin: Option<&AdminIdentity>,
        proposal_id: &str,
        expected_row_version: u64,
        now: i64,
    ) -> Result<(), StoreError> {
        admin.ok_or(StoreError::Unauthorized)?;
        let tx = self.db.transaction()?;
        let skill_id: String = tx
            .query_row(
                "SELECT skill_id FROM skill_proposals
                 WHERE proposal_id = ?1 AND status = 'verified'
                   AND reason_code = 'held_out_suite_required' AND row_version = ?2",
                params![proposal_id, sql_version(expected_row_version)?],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::Stale(proposal_id.to_string()))?;
        let proposal_changed = tx.execute(
            "UPDATE skill_proposals
             SET status = 'pending', next_attempt_at = ?1, report_id = NULL,
                 reason_code = NULL, row_version = row_version + 1, updated_at = ?1
             WHERE proposal_id = ?2 AND status = 'verified'
               AND reason_code = 'held_out_suite_required' AND row_version = ?3",
            params![now, proposal_id, sql_version(expected_row_version)?],
        )?;
        let revision_changed = tx.execute(
            "UPDATE skill_revisions
             SET status = 'pending', row_version = row_version + 1, updated_at = ?1
             WHERE id = ?2 AND status = 'verified'",
            params![now, skill_id],
        )?;
        if proposal_changed != 1 || revision_changed != 1 {
            return Err(StoreError::Stale(proposal_id.to_string()));
        }
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn desired_generation(&self) -> Result<u64, StoreError> {
        let generation: i64 = self.db.query_row(
            "SELECT desired_generation FROM skill_generations WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(generation)
            .map_err(|_| StoreError::CorruptRow("negative generation".to_string()))
    }

    #[cfg(test)]
    pub(crate) fn count_revisions(&self) -> Result<u64, StoreError> {
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM skill_revisions", [], |row| row.get(0))?;
        u64::try_from(count).map_err(|_| StoreError::CorruptRow("negative row count".to_string()))
    }

    #[cfg(test)]
    pub(crate) fn count_proposals(&self) -> Result<u64, StoreError> {
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM skill_proposals", [], |row| row.get(0))?;
        u64::try_from(count).map_err(|_| StoreError::CorruptRow("negative row count".to_string()))
    }

    pub fn database_path(&self) -> &std::path::Path {
        &self.db_path
    }

    /// Internal connection used exclusively by typed skill services.
    ///
    /// The visibility keeps raw SQL inside the skills module so lifecycle,
    /// evidence, retention, and privacy mutations cannot be bypassed by callers.
    pub(super) fn connection(&self) -> &Connection {
        &self.db
    }

    /// Mutable internal connection used exclusively by typed skill services.
    pub(super) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.db
    }

    /// Get the database connection for direct queries (testing/internal use).
    ///
    /// Callers must not hold this across potential blocking operations.
    #[cfg(test)]
    pub fn conn(&self) -> &Connection {
        &self.db
    }

    /// Mutable database connection for testing only.
    #[cfg(test)]
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.db
    }
}

fn is_full_skill_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Verify that FTS5 is available in the bundled SQLite build.
fn verify_fts5(db: &Connection) -> Result<(), StoreError> {
    // Test FTS5 availability by attempting to create a virtual table.
    let test_table = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS __fts5_test_{} USING fts5(content, tokenize = 'unicode61')",
        std::process::id()
    );
    match db.execute(&test_table, []) {
        Ok(_) => {
            // Clean up the test table.
            let _ = db.execute(
                &format!("DROP TABLE IF EXISTS __fts5_test_{}", std::process::id()),
                [],
            );
            Ok(())
        }
        Err(_) => Err(StoreError::MissingFts5),
    }
}

fn ensure_column(
    db: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), StoreError> {
    let mut statement = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|existing| existing == column) {
        db.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

/// Run idempotent schema migrations.
fn migrate(db: &Connection) -> Result<(), StoreError> {
    // Get current schema version. PRAGMA user_version returns 0 for uninitialized DB.
    let current_version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if current_version == CURRENT_SCHEMA_VERSION {
        // Already up to date.
        return Ok(());
    }

    if current_version > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchemaVersion(current_version));
    }

    // Migration 0 -> 1: Initial schema.
    if current_version == 0 {
        db.execute_batch(
            "
            BEGIN;

            CREATE TABLE IF NOT EXISTS skill_revisions (
                id               TEXT PRIMARY KEY,
                identity_version INTEGER NOT NULL,
                source           TEXT NOT NULL,
                description      TEXT NOT NULL,
                tags_json        TEXT NOT NULL,
                exports_json     TEXT NOT NULL,
                tests_json       TEXT NOT NULL,
                capability_json  TEXT NOT NULL,
                status           TEXT NOT NULL,
                supersedes_id    TEXT,
                superseded_by_id TEXT,
                row_version      INTEGER NOT NULL DEFAULT 1,
                created_at       INTEGER NOT NULL,
                updated_at       INTEGER NOT NULL,
                CHECK (status IN (
                    'pending','verified','canary','active','quarantined',
                    'superseded','retired','rejected'
                ))
            );

            CREATE TABLE IF NOT EXISTS skill_embeddings (
                skill_id       TEXT NOT NULL,
                model_id       TEXT NOT NULL,
                model_revision TEXT NOT NULL,
                dimensions     INTEGER NOT NULL,
                normalized     INTEGER NOT NULL,
                embedding      BLOB NOT NULL,
                created_at     INTEGER NOT NULL,
                PRIMARY KEY (skill_id, model_id, model_revision),
                FOREIGN KEY (skill_id) REFERENCES skill_revisions(id) ON DELETE CASCADE
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS skill_search USING fts5(
                identifier,
                description,
                tags,
                exports
            );

            CREATE TRIGGER IF NOT EXISTS skill_search_ai AFTER INSERT ON skill_revisions BEGIN
                INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                VALUES (NEW.rowid, substr(NEW.id, 1, 16), NEW.description, NEW.tags_json, NEW.exports_json);
            END;

            CREATE TRIGGER IF NOT EXISTS skill_search_ad AFTER DELETE ON skill_revisions BEGIN
                DELETE FROM skill_search WHERE rowid = OLD.rowid;
            END;

            CREATE TRIGGER IF NOT EXISTS skill_search_au AFTER UPDATE ON skill_revisions BEGIN
                DELETE FROM skill_search WHERE rowid = OLD.rowid;
                INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                VALUES (NEW.rowid, substr(NEW.id, 1, 16), NEW.description, NEW.tags_json, NEW.exports_json);
            END;

            PRAGMA user_version = 1;

            COMMIT;
            ",
        )?;
    }

    // Migration 1 -> 2: privacy tombstones, monotonic index generations, and
    // active-only lexical visibility. Rebuilding FTS is safe because it is a
    // derived index; canonical artifact bytes remain in skill_revisions.
    if current_version < 2 {
        db.execute_batch(
            "
            BEGIN;

            CREATE TABLE IF NOT EXISTS skill_tombstones (
                id        TEXT PRIMARY KEY,
                purged_at INTEGER NOT NULL,
                CHECK (length(id) = 64)
            );

            CREATE TABLE IF NOT EXISTS skill_generations (
                singleton          INTEGER PRIMARY KEY CHECK (singleton = 1),
                desired_generation INTEGER NOT NULL CHECK (desired_generation >= 0),
                applied_generation INTEGER NOT NULL CHECK (
                    applied_generation >= 0 AND applied_generation <= desired_generation
                ),
                model_id           TEXT NOT NULL,
                model_revision     TEXT NOT NULL,
                dimensions         INTEGER NOT NULL CHECK (dimensions >= 0),
                normalized         INTEGER NOT NULL CHECK (normalized IN (0, 1))
            );

            INSERT OR IGNORE INTO skill_generations (
                singleton, desired_generation, applied_generation,
                model_id, model_revision, dimensions, normalized
            ) VALUES (1, 0, 0, '', '', 0, 1);

            DROP TRIGGER IF EXISTS skill_search_ai;
            DROP TRIGGER IF EXISTS skill_search_ad;
            DROP TRIGGER IF EXISTS skill_search_au;
            DELETE FROM skill_search;

            CREATE TRIGGER skill_search_ai AFTER INSERT ON skill_revisions
            WHEN NEW.status = 'active' BEGIN
                INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                VALUES (NEW.rowid, NEW.id, NEW.description, NEW.tags_json, NEW.exports_json);
            END;

            CREATE TRIGGER skill_search_ad AFTER DELETE ON skill_revisions
            WHEN OLD.status = 'active' BEGIN
                DELETE FROM skill_search WHERE rowid = OLD.rowid;
            END;

            CREATE TRIGGER skill_search_au AFTER UPDATE ON skill_revisions BEGIN
                DELETE FROM skill_search WHERE rowid = OLD.rowid;
                INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                SELECT NEW.rowid, NEW.id, NEW.description, NEW.tags_json, NEW.exports_json
                WHERE NEW.status = 'active';
            END;

            INSERT INTO skill_search (rowid, identifier, description, tags, exports)
            SELECT rowid, id, description, tags_json, exports_json
            FROM skill_revisions WHERE status = 'active';

            PRAGMA user_version = 2;
            COMMIT;
            ",
        )?;
    }

    // Migration 2 -> 3: reconcile the independently developed Phase 3 and
    // Phase 4 v2 layouts, then add durable proposal/admission state.
    if current_version < 3 {
        db.execute_batch("BEGIN IMMEDIATE;")?;
        let migration = (|| -> Result<(), StoreError> {
            ensure_column(
                db,
                "skill_generations",
                "applied_generation",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                db,
                "skill_generations",
                "model_id",
                "TEXT NOT NULL DEFAULT ''",
            )?;
            ensure_column(
                db,
                "skill_generations",
                "model_revision",
                "TEXT NOT NULL DEFAULT ''",
            )?;
            ensure_column(
                db,
                "skill_generations",
                "dimensions",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
            ensure_column(
                db,
                "skill_generations",
                "normalized",
                "INTEGER NOT NULL DEFAULT 1",
            )?;
            ensure_column(
                db,
                "skill_generations",
                "row_version",
                "INTEGER NOT NULL DEFAULT 1",
            )?;
            ensure_column(
                db,
                "skill_generations",
                "updated_at",
                "INTEGER NOT NULL DEFAULT 0",
            )?;

            db.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS skill_tombstones (
                    id        TEXT PRIMARY KEY,
                    purged_at INTEGER NOT NULL,
                    CHECK (length(id) = 64)
                );

                DROP TRIGGER IF EXISTS skill_search_ai;
                DROP TRIGGER IF EXISTS skill_search_ad;
                DROP TRIGGER IF EXISTS skill_search_au;
                DELETE FROM skill_search;

                CREATE TRIGGER skill_search_ai AFTER INSERT ON skill_revisions
                WHEN NEW.status = 'active' BEGIN
                    INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                    VALUES (NEW.rowid, NEW.id, NEW.description, NEW.tags_json, NEW.exports_json);
                END;

                CREATE TRIGGER skill_search_ad AFTER DELETE ON skill_revisions
                WHEN OLD.status = 'active' BEGIN
                    DELETE FROM skill_search WHERE rowid = OLD.rowid;
                END;

                CREATE TRIGGER skill_search_au AFTER UPDATE ON skill_revisions BEGIN
                    DELETE FROM skill_search WHERE rowid = OLD.rowid;
                    INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                    SELECT NEW.rowid, NEW.id, NEW.description, NEW.tags_json, NEW.exports_json
                    WHERE NEW.status = 'active';
                END;

                INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                SELECT rowid, id, description, tags_json, exports_json
                FROM skill_revisions WHERE status = 'active';

            CREATE TABLE IF NOT EXISTS skill_proposals (
                proposal_id      TEXT PRIMARY KEY,
                skill_id         TEXT NOT NULL UNIQUE,
                predecessor_id   TEXT,
                proposed_at      INTEGER NOT NULL,
                status           TEXT NOT NULL DEFAULT 'pending',
                attempt_count    INTEGER NOT NULL DEFAULT 0,
                next_attempt_at  INTEGER,
                lease_owner      TEXT,
                lease_expires_at INTEGER,
                report_id        TEXT,
                reason_code      TEXT,
                row_version      INTEGER NOT NULL DEFAULT 1,
                created_at       INTEGER NOT NULL,
                updated_at       INTEGER NOT NULL,
                FOREIGN KEY (skill_id) REFERENCES skill_revisions(id) ON DELETE RESTRICT,
                FOREIGN KEY (predecessor_id) REFERENCES skill_revisions(id) ON DELETE RESTRICT,
                CHECK (status IN (
                    'pending','evaluating','verified','rejected',
                    'awaiting_approval','approved'
                )),
                CHECK (attempt_count >= 0 AND attempt_count <= 8),
                CHECK (row_version > 0),
                CHECK (
                    (status = 'evaluating' AND lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL)
                    OR
                    (status <> 'evaluating' AND lease_owner IS NULL AND lease_expires_at IS NULL)
                ),
                CHECK (
                    (status IN ('rejected','awaiting_approval','approved') AND report_id IS NOT NULL)
                    OR
                    (status NOT IN ('rejected','awaiting_approval','approved'))
                ),
                CHECK (
                    (status = 'rejected' AND reason_code IS NOT NULL)
                    OR
                    (status <> 'rejected')
                )
            );

            CREATE INDEX IF NOT EXISTS skill_proposals_due_idx
                ON skill_proposals(status, next_attempt_at, lease_expires_at, proposed_at);
            CREATE INDEX IF NOT EXISTS skill_proposals_skill_idx
                ON skill_proposals(skill_id);

            CREATE TABLE IF NOT EXISTS held_out_suites (
                suite_id          TEXT PRIMARY KEY,
                selector_json     TEXT NOT NULL,
                cases_json        TEXT NOT NULL,
                canonical_payload TEXT NOT NULL,
                approved_by       TEXT NOT NULL,
                approved_at       INTEGER NOT NULL,
                content_hash      TEXT NOT NULL UNIQUE,
                enabled           INTEGER NOT NULL DEFAULT 1,
                row_version       INTEGER NOT NULL DEFAULT 1,
                CHECK (length(suite_id) = 64),
                CHECK (length(content_hash) = 64),
                CHECK (length(approved_by) BETWEEN 1 AND 256),
                CHECK (enabled IN (0, 1)),
                CHECK (row_version > 0)
            );
            CREATE INDEX IF NOT EXISTS held_out_suites_enabled_idx
                ON held_out_suites(enabled, suite_id);

            CREATE TABLE IF NOT EXISTS evaluation_reports (
                report_id                TEXT PRIMARY KEY,
                proposal_id              TEXT NOT NULL,
                skill_id                 TEXT NOT NULL,
                attempt                  INTEGER NOT NULL,
                verifier_version         INTEGER NOT NULL,
                fakes_version            INTEGER NOT NULL,
                suite_hashes_json        TEXT NOT NULL,
                predecessor_id           TEXT,
                embedding_model_id       TEXT,
                embedding_model_revision TEXT,
                outcome                  TEXT NOT NULL,
                reason_code              TEXT,
                summary_json             TEXT NOT NULL,
                created_at               INTEGER NOT NULL,
                FOREIGN KEY (proposal_id) REFERENCES skill_proposals(proposal_id) ON DELETE RESTRICT,
                FOREIGN KEY (skill_id) REFERENCES skill_revisions(id) ON DELETE RESTRICT,
                FOREIGN KEY (predecessor_id) REFERENCES skill_revisions(id) ON DELETE RESTRICT,
                UNIQUE (proposal_id, attempt),
                CHECK (attempt > 0),
                CHECK (verifier_version > 0),
                CHECK (fakes_version > 0),
                CHECK (outcome IN ('passed','rejected','retryable')),
                CHECK (
                    (outcome = 'rejected' AND reason_code IS NOT NULL)
                    OR
                    (outcome <> 'rejected')
                ),
                CHECK (
                    (embedding_model_id IS NULL AND embedding_model_revision IS NULL)
                    OR
                    (embedding_model_id IS NOT NULL AND embedding_model_revision IS NOT NULL)
                )
            );

            CREATE TABLE IF NOT EXISTS skill_approvals (
                approval_id       TEXT PRIMARY KEY,
                proposal_id       TEXT NOT NULL UNIQUE,
                skill_id          TEXT NOT NULL UNIQUE,
                report_id         TEXT NOT NULL UNIQUE,
                approver_id       TEXT NOT NULL,
                authenticated_at  INTEGER NOT NULL,
                approved_at       INTEGER NOT NULL,
                artifact_version  INTEGER NOT NULL,
                proposal_version  INTEGER NOT NULL,
                generation        INTEGER NOT NULL,
                FOREIGN KEY (proposal_id) REFERENCES skill_proposals(proposal_id) ON DELETE RESTRICT,
                FOREIGN KEY (skill_id) REFERENCES skill_revisions(id) ON DELETE RESTRICT,
                FOREIGN KEY (report_id) REFERENCES evaluation_reports(report_id) ON DELETE RESTRICT,
                CHECK (length(approver_id) BETWEEN 1 AND 256),
                CHECK (artifact_version > 0),
                CHECK (proposal_version > 0),
                CHECK (generation > 0)
            );



                PRAGMA user_version = 3;
                ",
            )?;
            Ok(())
        })();

        match migration {
            Ok(()) => db.execute_batch("COMMIT;")?,
            Err(error) => {
                let _ = db.execute_batch("ROLLBACK;");
                return Err(error);
            }
        }
    }

    // Migration 3 -> 4: directly attributed evidence, lifecycle automation,
    // repair/feedback records, and retention state. Phase 5 extends the
    // generation table introduced by Phase 3 instead of creating a competing
    // publication authority.
    if current_version < 4 {
        db.execute_batch("BEGIN IMMEDIATE;")?;
        let migration = (|| -> Result<(), StoreError> {
            ensure_column(db, "skill_revisions", "lineage_root_id", "TEXT")?;
            ensure_column(db, "skill_revisions", "evaluation_report_id", "TEXT")?;
            ensure_column(
                db,
                "skill_generations",
                "publication_mode",
                "TEXT NOT NULL DEFAULT 'full'",
            )?;
            ensure_column(db, "skill_generations", "last_error_code", "TEXT")?;
            ensure_column(db, "skill_tombstones", "reason_code", "TEXT")?;
            ensure_column(
                db,
                "skill_tombstones",
                "last_generation",
                "INTEGER NOT NULL DEFAULT 0",
            )?;

            db.execute_batch(
                "
                CREATE INDEX IF NOT EXISTS skill_revisions_status_idx
                    ON skill_revisions(status, id);
                CREATE INDEX IF NOT EXISTS skill_revisions_lineage_idx
                    ON skill_revisions(lineage_root_id, status, id);
                CREATE TRIGGER IF NOT EXISTS skill_revisions_identity_immutable
                BEFORE UPDATE OF
                    identity_version, source, description, tags_json,
                    exports_json, tests_json, capability_json
                ON skill_revisions
                WHEN OLD.identity_version IS NOT NEW.identity_version
                  OR OLD.source IS NOT NEW.source
                  OR OLD.description IS NOT NEW.description
                  OR OLD.tags_json IS NOT NEW.tags_json
                  OR OLD.exports_json IS NOT NEW.exports_json
                  OR OLD.tests_json IS NOT NEW.tests_json
                  OR OLD.capability_json IS NOT NEW.capability_json
                BEGIN
                    SELECT RAISE(ABORT, 'immutable skill identity');
                END;
                CREATE UNIQUE INDEX IF NOT EXISTS skill_revisions_one_live_successor
                    ON skill_revisions(supersedes_id)
                    WHERE supersedes_id IS NOT NULL
                      AND status IN ('verified', 'canary', 'active');

                UPDATE skill_revisions
                   SET lineage_root_id = id
                 WHERE lineage_root_id IS NULL AND supersedes_id IS NULL;

            CREATE TABLE IF NOT EXISTS skill_policy_versions (
                    policy_version TEXT PRIMARY KEY,
                    policy_json    TEXT NOT NULL,
                    created_at     INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS skill_runtime_secrets (
                name       TEXT PRIMARY KEY,
                secret     BLOB NOT NULL CHECK(length(secret) = 32),
                created_at INTEGER NOT NULL
            );

                CREATE TABLE IF NOT EXISTS skill_events (
                    event_id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    invocation_id      TEXT,
                    skill_id           TEXT NOT NULL,
                    turn_id            TEXT NOT NULL,
                    tool_call_id       TEXT,
                    event_kind         TEXT NOT NULL,
                    export_name        TEXT,
                    outcome            TEXT,
                    latency_us         INTEGER,
                    retrieval_score    REAL,
                    retrieval_rank     INTEGER,
                    query_fingerprint  TEXT,
                    index_generation   INTEGER NOT NULL,
                    evidence_complete  INTEGER NOT NULL DEFAULT 1
                        CHECK (evidence_complete IN (0, 1)),
                    production         INTEGER NOT NULL DEFAULT 1
                        CHECK (production IN (0, 1)),
                    argument_shape     TEXT,
                    created_at         INTEGER NOT NULL,
                    CHECK (event_kind IN (
                        'selected', 'injected', 'invoked', 'returned', 'threw',
                        'timed_out', 'oom', 'capability_denied',
                        'user_positive', 'user_negative', 'observability_lost'
                    )),
                    CHECK (latency_us IS NULL OR latency_us >= 0),
                    CHECK (retrieval_rank IS NULL OR retrieval_rank >= 0),
                    UNIQUE (invocation_id, event_kind),
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS skill_events_skill_time_idx
                    ON skill_events(skill_id, created_at, event_id);
                CREATE INDEX IF NOT EXISTS skill_events_turn_idx
                    ON skill_events(skill_id, turn_id, invocation_id);
                CREATE UNIQUE INDEX IF NOT EXISTS skill_events_one_terminal
                    ON skill_events(invocation_id)
                    WHERE invocation_id IS NOT NULL
                      AND event_kind IN (
                        'returned', 'threw', 'timed_out', 'oom',
                        'capability_denied'
                      );
                CREATE UNIQUE INDEX IF NOT EXISTS skill_events_non_invocation_retry
                    ON skill_events(skill_id, turn_id, tool_call_id, event_kind)
                    WHERE invocation_id IS NULL;

                CREATE TABLE IF NOT EXISTS skill_evidence (
                    evidence_id       TEXT PRIMARY KEY,
                    skill_id          TEXT NOT NULL,
                    evidence_kind     TEXT NOT NULL,
                    payload_json      TEXT NOT NULL,
                    policy_version    TEXT NOT NULL,
                    created_at        INTEGER NOT NULL,
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (policy_version)
                        REFERENCES skill_policy_versions(policy_version)
                );
                CREATE INDEX IF NOT EXISTS skill_evidence_skill_idx
                    ON skill_evidence(skill_id, evidence_kind, created_at);

                CREATE TABLE IF NOT EXISTS skill_transitions (
                    transition_id      INTEGER PRIMARY KEY AUTOINCREMENT,
                    idempotency_key    TEXT NOT NULL UNIQUE,
                    skill_id           TEXT NOT NULL,
                    predecessor_id     TEXT,
                    from_status        TEXT NOT NULL,
                    to_status          TEXT NOT NULL,
                    reason             TEXT NOT NULL,
                    evidence_snapshot  TEXT NOT NULL,
                    policy_version     TEXT NOT NULL,
                    row_version_from   INTEGER NOT NULL,
                    row_version_to     INTEGER NOT NULL,
                    desired_generation INTEGER NOT NULL,
                    created_at         INTEGER NOT NULL,
                    CHECK (from_status IN (
                        'pending','verified','canary','active','quarantined',
                        'superseded','retired','rejected'
                    )),
                    CHECK (to_status IN (
                        'pending','verified','canary','active','quarantined',
                        'superseded','retired','rejected'
                    )),
                    CHECK (row_version_to = row_version_from + 1),
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (predecessor_id) REFERENCES skill_revisions(id),
                    FOREIGN KEY (policy_version)
                        REFERENCES skill_policy_versions(policy_version)
                );
                CREATE INDEX IF NOT EXISTS skill_transitions_skill_idx
                    ON skill_transitions(skill_id, transition_id);

                CREATE TABLE IF NOT EXISTS skill_stats (
                    skill_id              TEXT PRIMARY KEY,
                    selected_count        INTEGER NOT NULL DEFAULT 0,
                    invoked_count         INTEGER NOT NULL DEFAULT 0,
                    direct_success_count  INTEGER NOT NULL DEFAULT 0,
                    direct_failure_count  INTEGER NOT NULL DEFAULT 0,
                    timeout_count         INTEGER NOT NULL DEFAULT 0,
                    oom_count             INTEGER NOT NULL DEFAULT 0,
                    policy_fault_count    INTEGER NOT NULL DEFAULT 0,
                    user_positive_count   INTEGER NOT NULL DEFAULT 0,
                    user_negative_count   INTEGER NOT NULL DEFAULT 0,
                    latency_total_us      INTEGER NOT NULL DEFAULT 0,
                    updated_at            INTEGER NOT NULL,
                    CHECK (
                        selected_count >= 0 AND invoked_count >= 0
                        AND direct_success_count >= 0 AND direct_failure_count >= 0
                        AND timeout_count >= 0 AND oom_count >= 0
                        AND policy_fault_count >= 0
                        AND user_positive_count >= 0 AND user_negative_count >= 0
                        AND latency_total_us >= 0
                    ),
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS skill_daily_stats (
                    skill_id             TEXT NOT NULL,
                    day_start            INTEGER NOT NULL,
                    aggregate_version    INTEGER NOT NULL,
                    through_event_id     INTEGER NOT NULL,
                    invoked_count        INTEGER NOT NULL,
                    direct_success_count INTEGER NOT NULL,
                    direct_failure_count INTEGER NOT NULL,
                    timeout_count        INTEGER NOT NULL,
                    oom_count            INTEGER NOT NULL,
                    policy_fault_count   INTEGER NOT NULL,
                    latency_total_us     INTEGER NOT NULL,
                    PRIMARY KEY (skill_id, day_start, aggregate_version),
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS skill_compaction_watermarks (
                    worker_name      TEXT PRIMARY KEY,
                    aggregate_version INTEGER NOT NULL,
                    through_event_id INTEGER NOT NULL,
                    updated_at       INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS skill_repair_records (
                    repair_id            TEXT PRIMARY KEY,
                    skill_id             TEXT NOT NULL,
                    export_name          TEXT,
                    outcome_kind         TEXT NOT NULL,
                    sanitized_payload    TEXT NOT NULL,
                    inherited_cases_json TEXT NOT NULL,
                    query_fingerprint    TEXT,
                    retrieval_score      REAL,
                    index_generation     INTEGER NOT NULL,
                    human_approved       INTEGER NOT NULL DEFAULT 0
                        CHECK (human_approved IN (0, 1)),
                    created_at           INTEGER NOT NULL,
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS skill_feedback (
                    feedback_id    TEXT PRIMARY KEY,
                    idempotency_key TEXT NOT NULL UNIQUE,
                    skill_id       TEXT NOT NULL,
                    invocation_id  TEXT,
                    actor_id       TEXT NOT NULL,
                    feedback_kind  TEXT NOT NULL
                        CHECK (feedback_kind IN ('positive', 'negative', 'severe')),
                    reason_code    TEXT NOT NULL,
                    reason_text    TEXT,
                    state          TEXT NOT NULL
                        CHECK (state IN ('active', 'resolved', 'retracted')),
                    version        INTEGER NOT NULL DEFAULT 1,
                    created_at     INTEGER NOT NULL,
                    updated_at     INTEGER NOT NULL,
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS skill_feedback_policy_idx
                    ON skill_feedback(skill_id, state, feedback_kind);

                CREATE TABLE IF NOT EXISTS skill_feedback_audit (
                    audit_id    INTEGER PRIMARY KEY AUTOINCREMENT,
                    feedback_id TEXT NOT NULL,
                    from_state  TEXT,
                    to_state    TEXT NOT NULL,
                    actor_id    TEXT NOT NULL,
                    reason_code TEXT NOT NULL,
                    version     INTEGER NOT NULL,
                    created_at  INTEGER NOT NULL,
                    FOREIGN KEY (feedback_id) REFERENCES skill_feedback(feedback_id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS skill_lifecycle_approvals (
                    approval_id          TEXT PRIMARY KEY,
                    skill_id             TEXT NOT NULL,
                    approval_kind        TEXT NOT NULL,
                    actor_id             TEXT NOT NULL,
                    artifact_row_version INTEGER NOT NULL,
                    evaluation_report_id TEXT NOT NULL,
                    created_at           INTEGER NOT NULL,
                    UNIQUE (skill_id, approval_kind),
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS skill_decision_jobs (
                    decision_id       TEXT PRIMARY KEY,
                    skill_id          TEXT NOT NULL,
                    policy_version    TEXT NOT NULL,
                    due_at            INTEGER NOT NULL,
                    lease_owner       TEXT,
                    lease_expires_at  INTEGER,
                    attempts          INTEGER NOT NULL DEFAULT 0,
                    last_error_code   TEXT,
                    completed_at      INTEGER,
                    FOREIGN KEY (skill_id) REFERENCES skill_revisions(id)
                        ON DELETE CASCADE,
                    FOREIGN KEY (policy_version)
                        REFERENCES skill_policy_versions(policy_version)
                );
                CREATE INDEX IF NOT EXISTS skill_decision_jobs_due_idx
                    ON skill_decision_jobs(completed_at, due_at, lease_expires_at);

                PRAGMA user_version = 4;
                ",
            )?;
            Ok(())
        })();

        match migration {
            Ok(()) => db.execute_batch("COMMIT;")?,
            Err(error) => {
                let _ = db.execute_batch("ROLLBACK;");
                return Err(error);
            }
        }
    }

    // Migration 4 -> 5: identity-v2 manifests bind ABI v2 and structured scopes.
    // Identity-v1 bytes and lineage remain immutable, but every legacy row is
    // operationally quarantined with a stable reason. FTS is rebuilt behind an
    // identity-v2 predicate. No legacy flat host list is interpreted.
    if current_version < 5 {
        db.execute_batch("BEGIN IMMEDIATE;")?;
        let migration = (|| -> Result<(), StoreError> {
            ensure_column(db, "skill_revisions", "quarantine_reason", "TEXT")?;
            db.execute(
                "UPDATE skill_revisions
                    SET status = 'quarantined',
                        quarantine_reason = 'manifest_scope_required',
                        row_version = row_version + 1,
                        updated_at = CASE WHEN updated_at < 1 THEN 1 ELSE updated_at END
                  WHERE identity_version = 1",
                [],
            )?;
            db.execute_batch(
                "DROP TRIGGER IF EXISTS skill_search_ai;
                 DROP TRIGGER IF EXISTS skill_search_ad;
                 DROP TRIGGER IF EXISTS skill_search_au;
                 DELETE FROM skill_search;

                 CREATE TRIGGER skill_search_ai AFTER INSERT ON skill_revisions
                 WHEN NEW.status = 'active' AND NEW.identity_version = 2 BEGIN
                     INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                     VALUES (NEW.rowid, NEW.id, NEW.description, NEW.tags_json, NEW.exports_json);
                 END;

                 CREATE TRIGGER skill_search_ad AFTER DELETE ON skill_revisions BEGIN
                     DELETE FROM skill_search WHERE rowid = OLD.rowid;
                 END;

                 CREATE TRIGGER skill_search_au AFTER UPDATE ON skill_revisions BEGIN
                     DELETE FROM skill_search WHERE rowid = OLD.rowid;
                     INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                     SELECT NEW.rowid, NEW.id, NEW.description, NEW.tags_json, NEW.exports_json
                      WHERE NEW.status = 'active' AND NEW.identity_version = 2;
                 END;

                 INSERT INTO skill_search (rowid, identifier, description, tags, exports)
                 SELECT rowid, id, description, tags_json, exports_json
                   FROM skill_revisions
                  WHERE status = 'active' AND identity_version = 2;

                 PRAGMA user_version = 5;",
            )?;
            Ok(())
        })();
        match migration {
            Ok(()) => db.execute_batch("COMMIT;")?,
            Err(error) => {
                let _ = db.execute_batch("ROLLBACK;");
                return Err(error);
            }
        }
    }

    Ok(())
}

/// Read a skill artifact row from the database.
fn read_artifact_row(row: &Row) -> rusqlite::Result<Result<SkillArtifact, StoreError>> {
    let id: String = row.get(0)?;
    let identity_version: u32 = row.get(1)?;
    let source: String = row.get(2)?;
    let description: String = row.get(3)?;
    let tags_json: String = row.get(4)?;
    let exports_json: String = row.get(5)?;
    let tests_json: String = row.get(6)?;
    let capability_json: String = row.get(7)?;

    // Parse JSON fields; return error if malformed.
    let parse_result = (|| -> Result<SkillArtifact, StoreError> {
        let tags: Vec<String> = serde_json::from_str(&tags_json)?;
        let exports = deserialize_exports(&exports_json)?;
        let tests: Vec<String> = serde_json::from_str(&tests_json)?;
        if identity_version == 1 {
            return Err(StoreError::LegacyIdentityQuarantined);
        }
        if identity_version != super::IDENTITY_VERSION {
            return Err(StoreError::IdentityValidation(
                IdentityError::UnsupportedIdentityVersion(identity_version),
            ));
        }
        let (abi_version, capability) = deserialize_capability(&capability_json)?;

        Ok(SkillArtifact {
            id,
            identity_version,
            abi_version,
            source,
            description,
            tags,
            exports,
            tests,
            capability,
        })
    })();

    match parse_result {
        Ok(artifact) => Ok(Ok(artifact)),
        Err(error) => Ok(Err(error)),
    }
}

fn read_proposal_row(row: &Row) -> rusqlite::Result<ProposalRecord> {
    let status: String = row.get(3)?;
    let status = ProposalStatus::parse(&status).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let attempt_count: i64 = row.get(4)?;
    let row_version: i64 = row.get(10)?;
    if !(0..=i64::from(MAX_EVALUATION_ATTEMPTS)).contains(&attempt_count) || row_version <= 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Integer,
            Box::new(StoreError::CorruptRow(
                "invalid proposal counter or row version".to_string(),
            )),
        ));
    }
    Ok(ProposalRecord {
        proposal_id: row.get(0)?,
        skill_id: row.get(1)?,
        predecessor_id: row.get(2)?,
        status,
        attempt_count: attempt_count as u32,
        next_attempt_at: row.get(5)?,
        lease_owner: row.get(6)?,
        lease_expires_at: row.get(7)?,
        report_id: row.get(8)?,
        reason_code: row.get(9)?,
        row_version: row_version as u64,
    })
}

fn read_held_out_suite(row: &Row) -> rusqlite::Result<HeldOutSuiteRecord> {
    Ok(HeldOutSuiteRecord {
        suite_id: row.get(0)?,
        selector_json: row.get(1)?,
        cases_json: row.get(2)?,
        content_hash: row.get(3)?,
        canonical_payload: row.get(4)?,
        approved_by: row.get(5)?,
        approved_at: row.get(6)?,
        enabled: row.get::<_, i64>(7)? == 1,
    })
}

fn read_evaluation_report(row: &Row) -> rusqlite::Result<EvaluationReportRecord> {
    let suite_hashes_json: String = row.get(6)?;
    let suite_hashes = serde_json::from_str(&suite_hashes_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let summary_json: String = row.get(12)?;
    serde_json::from_str::<serde_json::Value>(&summary_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(EvaluationReportRecord {
        report_id: row.get(0)?,
        proposal_id: row.get(1)?,
        skill_id: row.get(2)?,
        attempt: row.get(3)?,
        verifier_version: row.get(4)?,
        fakes_version: row.get(5)?,
        suite_hashes,
        predecessor_id: row.get(7)?,
        embedding_model_id: row.get(8)?,
        embedding_model_revision: row.get(9)?,
        outcome: row.get(10)?,
        reason_code: row.get(11)?,
        summary_json,
        created_at: row.get(13)?,
    })
}

fn validate_full_id(id: Option<&str>) -> Result<(), StoreError> {
    if let Some(id) = id
        && (id.len() != 64
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    {
        return Err(StoreError::Constraint(
            "predecessor ID must be 64 lowercase hexadecimal characters".to_string(),
        ));
    }
    Ok(())
}

fn sql_version(version: u64) -> Result<i64, StoreError> {
    i64::try_from(version)
        .map_err(|_| StoreError::Constraint("row version exceeds SQLite range".to_string()))
}

fn enqueue_status(status: ProposalStatus) -> Result<EnqueueStatus, StoreError> {
    match status {
        ProposalStatus::Pending | ProposalStatus::Evaluating => Ok(EnqueueStatus::Pending),
        ProposalStatus::Verified => Ok(EnqueueStatus::Verified),
        ProposalStatus::Rejected => Ok(EnqueueStatus::Rejected),
        ProposalStatus::AwaitingApproval => Ok(EnqueueStatus::AwaitingApproval),
        ProposalStatus::Approved => Ok(EnqueueStatus::Approved),
    }
}

fn insert_revision(
    tx: &rusqlite::Transaction<'_>,
    artifact: &SkillArtifact,
    status: &str,
    now: i64,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO skill_revisions (
            id, identity_version, source, description, tags_json,
            exports_json, tests_json, capability_json, status,
            row_version, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?10)",
        params![
            artifact.id,
            artifact.identity_version,
            artifact.source,
            artifact.description,
            serde_json::to_string(&artifact.tags)?,
            serialize_exports(&artifact.exports)?,
            serde_json::to_string(&artifact.tests)?,
            serialize_capability(artifact)?,
            status,
            now
        ],
    )?;
    Ok(())
}

fn validate_report_binding(
    proposal_id: &str,
    report: &EvaluationReportRecord,
) -> Result<(), StoreError> {
    if report.proposal_id != proposal_id
        || report.skill_id.len() != 64
        || report.report_id.trim().is_empty()
        || report.attempt == 0
        || report.verifier_version == 0
        || report.fakes_version == 0
    {
        return Err(StoreError::Constraint(
            "evaluation report binding or version is invalid".to_string(),
        ));
    }
    if report.recompute_id()? != report.report_id {
        return Err(StoreError::Constraint(
            "evaluation report identity is invalid".to_string(),
        ));
    }
    Ok(())
}

fn insert_report(
    tx: &rusqlite::Transaction<'_>,
    report: &EvaluationReportRecord,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO evaluation_reports (
            report_id, proposal_id, skill_id, attempt, verifier_version, fakes_version,
            suite_hashes_json, predecessor_id, embedding_model_id,
            embedding_model_revision, outcome, reason_code, summary_json, created_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
         )",
        params![
            report.report_id,
            report.proposal_id,
            report.skill_id,
            report.attempt,
            report.verifier_version,
            report.fakes_version,
            serde_json::to_string(&report.suite_hashes)?,
            report.predecessor_id,
            report.embedding_model_id,
            report.embedding_model_revision,
            report.outcome,
            report.reason_code,
            report.summary_json,
            report.created_at
        ],
    )?;
    Ok(())
}

fn validate_embedding_bytes(
    skill_id: &str,
    dimensions: usize,
    normalized: bool,
    bytes: &[u8],
) -> Result<(), StoreError> {
    if dimensions == 0 || bytes.len() != dimensions.saturating_mul(std::mem::size_of::<f32>()) {
        return Err(StoreError::MalformedEmbedding {
            skill_id: skill_id.to_string(),
            reason: format!(
                "expected {} bytes for {dimensions} dimensions, got {}",
                dimensions.saturating_mul(std::mem::size_of::<f32>()),
                bytes.len()
            ),
        });
    }
    let values = decode_embedding(bytes);
    if !values.iter().all(|value| value.is_finite()) {
        return Err(StoreError::MalformedEmbedding {
            skill_id: skill_id.to_string(),
            reason: "vector contains a non-finite value".to_string(),
        });
    }
    if normalized {
        let norm_squared: f32 = values.iter().map(|value| value * value).sum();
        if (norm_squared - 1.0).abs() > 1e-3 {
            return Err(StoreError::MalformedEmbedding {
                skill_id: skill_id.to_string(),
                reason: format!("vector claims normalization but squared norm is {norm_squared}"),
            });
        }
    }
    Ok(())
}

fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Get current Unix timestamp in seconds.
pub(crate) fn current_timestamp() -> Result<i64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| StoreError::Io(std::io::Error::other("system time error")))
}

/// Serialize SkillExport vec to JSON string.
fn serialize_exports(exports: &[SkillExport]) -> Result<String, StoreError> {
    let json_array: Vec<serde_json::Value> = exports
        .iter()
        .map(|export| {
            serde_json::json!({
                "name": export.name,
                "signature": export.signature,
            })
        })
        .collect();
    Ok(serde_json::to_string(&json_array)?)
}

/// Deserialize SkillExport vec from JSON string.
fn deserialize_exports(json: &str) -> Result<Vec<SkillExport>, serde_json::Error> {
    let json_array: Vec<serde_json::Value> = serde_json::from_str(json)?;
    let mut exports = Vec::new();
    for value in json_array {
        let obj = value.as_object().ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "export is not an object",
            ))
        })?;
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing name field",
                ))
            })?
            .to_string();
        let signature = obj
            .get("signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing signature field",
                ))
            })?
            .to_string();
        exports.push(SkillExport { name, signature });
    }
    Ok(exports)
}

/// Serialize CapabilityManifest to JSON string.
fn serialize_capability(artifact: &SkillArtifact) -> Result<String, StoreError> {
    let json = serde_json::json!({
        "abi_version": artifact.abi_version,
        "manifest": artifact.capability,
    });
    Ok(serde_json::to_string(&json)?)
}

/// Deserialize CapabilityManifest from JSON string.
fn deserialize_capability(json: &str) -> Result<(u16, CapabilityManifest), serde_json::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredCapability {
        abi_version: u16,
        manifest: CapabilityManifest,
    }

    let stored: StoredCapability = serde_json::from_str(json)?;
    if stored.abi_version != SKILL_ABI_VERSION {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported skill ABI version",
        )));
    }
    Ok((stored.abi_version, stored.manifest))
}
