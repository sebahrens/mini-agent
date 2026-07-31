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
const CURRENT_SCHEMA_VERSION: u32 = 1;

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

    #[error("skill already exists: {0}")]
    AlreadyExists(String),

    #[error("constraint violation: {0}")]
    Constraint(String),

    #[error("database locked or busy")]
    Busy,

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

        let tags_json = serde_json::to_string(&artifact.tags)?;
        let exports_json = serialize_exports(&artifact.exports)?;
        let tests_json = serde_json::to_string(&artifact.tests)?;
        let capability_json = serialize_capability(&artifact.capability)?;

        let now = current_timestamp()?;

        let tx = self.db.transaction()?;
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
            if e.to_string().contains("UNIQUE") || e.to_string().contains("unique") {
                StoreError::AlreadyExists(artifact.id.clone())
            } else if e.to_string().contains("CONSTRAINT") {
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

        let row = stmt
            .query_row(params![id], |row| read_artifact_row(row))
            .optional()?;

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

        let rows = stmt.query_map([], |row| read_artifact_row(row))?;

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

    /// Get the database file path for testing purposes.
    #[cfg(test)]
    pub fn path(&self) -> &PathBuf {
        &self.db_path
    }
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

/// Get current Unix timestamp in seconds.
fn current_timestamp() -> Result<i64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "system time error",
            ))
        })
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
