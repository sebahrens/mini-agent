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
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    CapabilityManifest, CapabilityTier, HostCapability, IdentityError, SkillArtifact, SkillExport,
};

/// Database schema version. Bump when schema changes; migrations bring older
/// databases forward idempotently.
pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 2;

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

    #[error("constraint violation: {0}")]
    Constraint(String),

    #[error("unsupported future schema version: {0}")]
    UnsupportedSchemaVersion(u32),

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
        let capability_json = serialize_capability(&artifact.capability)?;

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
            "SELECT COUNT(*) FROM skill_revisions WHERE status = 'active'",
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
             WHERE r.status = 'active'
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
                "SELECT status, supersedes_id, superseded_by_id, row_version
             FROM skill_revisions WHERE id = ?",
                params![id],
                |row| {
                    let row_version: i64 = row.get(3)?;
                    Ok(SkillRecordMetadata {
                        status: row.get(0)?,
                        supersedes_id: row.get(1)?,
                        superseded_by_id: row.get(2)?,
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
                    dimensions, normalized
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
        let changed = self.db.execute(
            "UPDATE skill_generations SET applied_generation = ?
             WHERE singleton = 1 AND desired_generation = ? AND applied_generation <= ?",
            params![generation as i64, generation as i64, generation as i64],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::Constraint(format!(
                "generation {generation} is no longer the desired generation"
            )))
        }
    }

    pub fn database_path(&self) -> &std::path::Path {
        &self.db_path
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
    let parse_result = (|| {
        let tags: Vec<String> = serde_json::from_str(&tags_json)?;
        let exports = deserialize_exports(&exports_json)?;
        let tests: Vec<String> = serde_json::from_str(&tests_json)?;
        let capability = deserialize_capability(&capability_json)?;

        Ok(SkillArtifact {
            id,
            identity_version,
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
        Err(e) => Ok(Err(StoreError::MalformedJson(e))),
    }
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
fn current_timestamp() -> Result<i64, StoreError> {
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
fn serialize_capability(capability: &CapabilityManifest) -> Result<String, StoreError> {
    let hosts: Vec<String> = capability
        .allowed_hosts
        .iter()
        .map(|h| h.as_token().to_string())
        .collect();

    let json = serde_json::json!({
        "tier": capability.tier.as_token(),
        "allowed_hosts": hosts,
    });
    Ok(serde_json::to_string(&json)?)
}

/// Deserialize CapabilityManifest from JSON string.
fn deserialize_capability(json: &str) -> Result<CapabilityManifest, serde_json::Error> {
    let obj = serde_json::from_str::<serde_json::Value>(json)?
        .as_object()
        .ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "capability is not an object",
            ))
        })?
        .clone();

    let tier_str = obj.get("tier").and_then(|v| v.as_str()).ok_or_else(|| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing tier field",
        ))
    })?;

    let tier = CapabilityTier::from_token(tier_str).ok_or_else(|| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unknown capability tier",
        ))
    })?;

    let hosts_array = obj
        .get("allowed_hosts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing or invalid allowed_hosts field",
            ))
        })?;

    let mut allowed_hosts = Vec::new();
    for host_value in hosts_array {
        let host_str = host_value.as_str().ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "host capability is not a string",
            ))
        })?;
        let capability = HostCapability::from_token(host_str).ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown host capability",
            ))
        })?;
        allowed_hosts.push(capability);
    }

    Ok(CapabilityManifest {
        tier,
        allowed_hosts,
    })
}
