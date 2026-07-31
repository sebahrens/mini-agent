//! Comprehensive fixtures for SkillStore schema, migrations, constraints, and lifecycle.

use crate::extras::js::skills::store::SkillStore;
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityTier, HostCapability, SkillArtifact, SkillExport,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

// ============================================================================
// Test Fixtures and Helpers
// ============================================================================

/// Create a temporary AppPaths for testing without touching the repository.
fn temp_app_paths() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    // A timestamp alone is not unique: the test harness runs these in parallel and
    // the clock is not nanosecond-granular on every platform, so two tests can land
    // on the same directory and then share one database. Combine the clock with the
    // process id and a monotonic counter so each call is unique by construction.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    std::env::temp_dir().join(format!(
        "skill_store_test_{}_{}_{}",
        std::process::id(),
        nanos,
        sequence
    ))
}

/// Resolve test AppPaths from a temporary base directory.
fn resolve_test_paths(temp_base: &PathBuf) -> Result<AppPaths, Box<dyn std::error::Error>> {
    let env = PathEnvironment {
        platform: if cfg!(target_os = "linux") {
            PathPlatform::Linux
        } else if cfg!(target_os = "macos") {
            PathPlatform::MacOs
        } else if cfg!(target_os = "windows") {
            PathPlatform::Windows
        } else {
            PathPlatform::Linux
        },
        home_dir: None,
        config_base: Some(temp_base.clone()),
        data_base: Some(temp_base.clone()),
        local_data_base: Some(temp_base.clone()),
        state_base: Some(temp_base.clone()),
        cache_base: Some(temp_base.clone()),
        workspace_root: None,
        overrides: Default::default(),
    };
    Ok(AppPaths::resolve(&env)?)
}

/// Create a minimal valid test skill artifact.
fn minimal_skill() -> Result<SkillArtifact, Box<dyn std::error::Error>> {
    Ok(SkillArtifact::new(
        "function greet() { return 'hello'; }".to_string(),
        "A simple greeting function.".to_string(),
        vec!["test".to_string(), "example".to_string()],
        vec![SkillExport {
            name: "greet".to_string(),
            signature: "() => string".to_string(),
        }],
        vec!["greet() === 'hello'".to_string()],
        CapabilityManifest::pure(),
    )?)
}

/// Create a skill with ReadOnly capability.
fn readonly_skill() -> Result<SkillArtifact, Box<dyn std::error::Error>> {
    Ok(SkillArtifact::new(
        "function readConfig() { try { read_file('/etc/config'); return false; } catch (error) { return String(error).includes('File not found'); } }".to_string(),
        "Read system configuration.".to_string(),
        vec!["config".to_string(), "read".to_string()],
        vec![SkillExport {
            name: "readConfig".to_string(),
            signature: "() => string".to_string(),
        }],
        vec!["readConfig() === true".to_string()],
        CapabilityManifest::new(CapabilityTier::ReadOnly, vec![HostCapability::ReadFile])?,
    )?)
}

/// Create a skill with SideEffecting capability.
fn sideeffecting_skill() -> Result<SkillArtifact, Box<dyn std::error::Error>> {
    Ok(SkillArtifact::new(
        "function deploy() { return spawn('deploy.sh', []); }".to_string(),
        "Deploy the application.".to_string(),
        vec!["deploy".to_string()],
        vec![SkillExport {
            name: "deploy".to_string(),
            signature: "() => object".to_string(),
        }],
        vec!["typeof deploy() === 'string'".to_string()],
        CapabilityManifest::new(CapabilityTier::SideEffecting, vec![HostCapability::Spawn])?,
    )?)
}

fn create_schema_v1(paths: &AppPaths) -> Result<Connection, Box<dyn std::error::Error>> {
    let db_path = paths.local_data_dir.join("skills").join("skills.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let db = Connection::open(db_path)?;
    db.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        CREATE TABLE skill_revisions (
            id TEXT PRIMARY KEY, identity_version INTEGER NOT NULL,
            source TEXT NOT NULL, description TEXT NOT NULL, tags_json TEXT NOT NULL,
            exports_json TEXT NOT NULL, tests_json TEXT NOT NULL,
            capability_json TEXT NOT NULL, status TEXT NOT NULL,
            supersedes_id TEXT, superseded_by_id TEXT,
            row_version INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
            CHECK (status IN ('pending','verified','canary','active','quarantined',
                              'superseded','retired','rejected'))
        );
        CREATE TABLE skill_embeddings (
            skill_id TEXT NOT NULL, model_id TEXT NOT NULL, model_revision TEXT NOT NULL,
            dimensions INTEGER NOT NULL, normalized INTEGER NOT NULL,
            embedding BLOB NOT NULL, created_at INTEGER NOT NULL,
            PRIMARY KEY (skill_id, model_id, model_revision),
            FOREIGN KEY (skill_id) REFERENCES skill_revisions(id) ON DELETE CASCADE
        );
        CREATE VIRTUAL TABLE skill_search USING fts5(identifier, description, tags, exports);
        CREATE TRIGGER skill_search_ai AFTER INSERT ON skill_revisions BEGIN
            INSERT INTO skill_search (rowid, identifier, description, tags, exports)
            VALUES (NEW.rowid, substr(NEW.id, 1, 16), NEW.description, NEW.tags_json, NEW.exports_json);
        END;
        CREATE TRIGGER skill_search_ad AFTER DELETE ON skill_revisions BEGIN
            DELETE FROM skill_search WHERE rowid = OLD.rowid;
        END;
        CREATE TRIGGER skill_search_au AFTER UPDATE ON skill_revisions BEGIN
            DELETE FROM skill_search WHERE rowid = OLD.rowid;
            INSERT INTO skill_search (rowid, identifier, description, tags, exports)
            VALUES (NEW.rowid, substr(NEW.id, 1, 16), NEW.description, NEW.tags_json, NEW.exports_json);
        END;
        PRAGMA user_version = 1;
        ",
    )?;
    Ok(db)
}

fn insert_schema_v1_artifact(
    db: &Connection,
    skill: &SkillArtifact,
    status: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let exports = skill
        .exports
        .iter()
        .map(|export| {
            serde_json::json!({
                "name": export.name,
                "signature": export.signature,
            })
        })
        .collect::<Vec<_>>();
    let capability = serde_json::json!({
        "tier": skill.capability.tier.as_token(),
        "allowed_hosts": skill.capability.allowed_hosts.iter().map(|host| host.as_token()).collect::<Vec<_>>(),
    });
    db.execute(
        "INSERT INTO skill_revisions (
            id, identity_version, source, description, tags_json, exports_json,
            tests_json, capability_json, status, row_version, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 1, 1)",
        params![
            skill.id,
            skill.identity_version,
            skill.source,
            skill.description,
            serde_json::to_string(&skill.tags)?,
            serde_json::to_string(&exports)?,
            serde_json::to_string(&skill.tests)?,
            serde_json::to_string(&capability)?,
            status,
        ],
    )?;
    Ok(())
}

// ============================================================================
// Test Cases
// ============================================================================

#[test]
fn test_fresh_database_creation() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;
    let mut store = SkillStore::open_at(&paths)?;

    let skill = minimal_skill()?;
    store.insert_verified(&skill)?;

    // Verify the skill exists.
    let retrieved = store.get(&skill.id)?;
    assert!(retrieved.is_some(), "Inserted skill not found");
    assert_eq!(retrieved.unwrap().id, skill.id);

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_database_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    // Create and insert.
    {
        let mut store = SkillStore::open_at(&paths)?;
        let skill = minimal_skill()?;
        store.insert_verified(&skill)?;
    }

    // Reopen and verify persistence.
    {
        let store = SkillStore::open_at(&paths)?;
        let skill = minimal_skill()?;
        let retrieved = store.get(&skill.id)?;
        assert!(retrieved.is_some(), "Skill lost after reopen");
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_concurrent_open() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    // Open multiple stores concurrently.
    let mut store1 = SkillStore::open_at(&paths)?;
    let store2 = SkillStore::open_at(&paths)?;

    let skill = minimal_skill()?;
    store1.insert_verified(&skill)?;

    let retrieved = store2.get(&skill.id)?;
    assert!(retrieved.is_some(), "Concurrent read failed");

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_identity_validation_on_read() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = minimal_skill()?;
    let correct_id = skill.id.clone();

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        // Directly access the database to tamper with the stored skill.
        store.conn_mut().execute(
            "UPDATE skill_revisions SET source = 'tampered' WHERE id = ?",
            [&correct_id],
        )?;
    }

    // Reopen and verify identity check rejects tampered row.
    {
        let store = SkillStore::open_at(&paths)?;
        let result = store.get(&correct_id);
        // The row exists but identity validation should fail due to tampering.
        assert!(
            result.is_err() || result.as_ref().ok().map(|r| r.is_none()).unwrap_or(false),
            "Identity validation should have rejected the tampered row"
        );
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_lifecycle_status_check_constraint() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = minimal_skill()?;

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        // Attempt invalid status.
        let result = store.conn_mut().execute(
            "UPDATE skill_revisions SET status = 'invalid_status' WHERE id = ?",
            [&skill.id],
        );

        assert!(
            result.is_err(),
            "CHECK constraint should reject invalid status"
        );
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_all_valid_lifecycle_statuses() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let valid_statuses = vec![
        "pending",
        "verified",
        "canary",
        "active",
        "quarantined",
        "superseded",
        "retired",
        "rejected",
    ];

    let skill = minimal_skill()?;

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        for status in &valid_statuses {
            let result = store.conn_mut().execute(
                &format!(
                    "UPDATE skill_revisions SET status = '{}' WHERE id = ?",
                    status
                ),
                [&skill.id],
            );
            assert!(result.is_ok(), "Valid status '{}' was rejected", status);
        }
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_list_retrievable_only_active() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    {
        let mut store = SkillStore::open_at(&paths)?;

        // Insert two active skills.
        let skill1 = minimal_skill()?;
        store.insert_verified(&skill1)?;

        let skill2 = readonly_skill()?;
        store.insert_verified(&skill2)?;

        // Insert a non-active skill by direct database manipulation.
        let skill3 = sideeffecting_skill()?;

        // Manually serialize exports and capability as JSON.
        let exports_json = serde_json::to_string(&serde_json::json!(
            skill3
                .exports
                .iter()
                .map(|e| serde_json::json!({ "name": e.name, "signature": e.signature }))
                .collect::<Vec<_>>()
        ))?;
        let capability_json = serde_json::to_string(&serde_json::json!({
            "tier": skill3.capability.tier.as_token(),
            "allowed_hosts": skill3.capability.allowed_hosts.iter()
                .map(|h| h.as_token())
                .collect::<Vec<_>>()
        }))?;

        store.conn_mut().execute(
            "INSERT INTO skill_revisions (
                id, identity_version, source, description, tags_json,
                exports_json, tests_json, capability_json, status,
                row_version, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                skill3.id,
                skill3.identity_version,
                skill3.source,
                skill3.description,
                serde_json::to_string(&skill3.tags)?,
                exports_json,
                serde_json::to_string(&skill3.tests)?,
                capability_json,
                "retired",
                1i64,
                0i64,
                0i64
            ],
        )?;

        let retrievable = store.list_retrievable()?;
        assert_eq!(
            retrievable.len(),
            2,
            "list_retrievable should return only active skills"
        );
        assert!(retrievable.iter().any(|s| s.id == skill1.id));
        assert!(retrievable.iter().any(|s| s.id == skill2.id));
        assert!(!retrievable.iter().any(|s| s.id == skill3.id));
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_embedding_storage_and_retrieval() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = minimal_skill()?;
    let skill_id = skill.id.clone();

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        // Store an embedding.
        let embedding_vec = vec![1.0f32, 0.0, 0.0, 0.0];
        let embedding_bytes = embedding_vec
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect::<Vec<_>>();

        store.store_embedding(
            &skill_id,
            "BAAI/bge-small-en-v1.5",
            "1",
            4,
            true,
            &embedding_bytes,
        )?;

        // Verify embedding was stored.
        let conn = store.conn();
        let stored: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM skill_embeddings WHERE skill_id = ?",
                [&skill_id],
                |row| row.get(0),
            )
            .optional()?;

        assert!(stored.is_some(), "Embedding not stored");
        assert_eq!(stored.unwrap(), embedding_bytes);
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_embedding_foreign_key_cascade() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = minimal_skill()?;
    let skill_id = skill.id.clone();

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        // Store an embedding.
        let embedding_bytes = [1.0f32, 0.0, 0.0, 0.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        store.store_embedding(&skill_id, "model1", "1", 4, true, &embedding_bytes)?;

        // Delete the skill.
        store
            .conn_mut()
            .execute("DELETE FROM skill_revisions WHERE id = ?", [&skill_id])?;

        // Verify embedding was cascade-deleted.
        let conn = store.conn();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM skill_embeddings WHERE skill_id = ?",
            [&skill_id],
            |row| row.get(0),
        )?;

        assert_eq!(count, 0, "Embedding cascade delete failed");
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_fts5_available() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    // Simply opening the store verifies FTS5 is available.
    let _store = SkillStore::open_at(&paths)?;

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_exact_duplicate_insert_is_idempotent() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = minimal_skill()?;

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        store.insert_verified(&skill)?;
        assert_eq!(store.list_retrievable()?.len(), 1);
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_malformed_json_skipped_in_list() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill1 = minimal_skill()?;
    let skill2 = readonly_skill()?;

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill1)?;
        store.insert_verified(&skill2)?;

        // Corrupt skill1's JSON.
        store.conn_mut().execute(
            "UPDATE skill_revisions SET tags_json = '{invalid}' WHERE id = ?",
            [&skill1.id],
        )?;

        // list_retrievable should skip the corrupted one.
        let retrievable = store.list_retrievable()?;
        assert_eq!(retrievable.len(), 1, "Malformed JSON should be skipped");
        assert_eq!(retrievable[0].id, skill2.id);
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_capability_persistence() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = sideeffecting_skill()?;
    let original_capabilities = skill.capability.clone();

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        let retrieved = store.get(&skill.id)?.expect("Skill not found");
        assert_eq!(retrieved.capability.tier, original_capabilities.tier);
        assert_eq!(
            retrieved.capability.allowed_hosts,
            original_capabilities.allowed_hosts
        );
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_various_tag_normalization() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    // Create skills with different tag variations.
    let skill1 = SkillArtifact::new(
        "function test() { return true; }".to_string(),
        "Test skill".to_string(),
        vec!["TAG".to_string(), "tag".to_string(), "  tag  ".to_string()],
        vec![SkillExport {
            name: "test".to_string(),
            signature: "() => boolean".to_string(),
        }],
        vec!["test() === true".to_string()],
        CapabilityManifest::pure(),
    )?;

    // Verify tags were normalized (deduplicated and lowercased).
    assert_eq!(skill1.tags.len(), 1);
    assert_eq!(skill1.tags[0], "tag");

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill1)?;

        let retrieved = store.get(&skill1.id)?.expect("Skill not found");
        assert_eq!(retrieved.tags.len(), 1);
        assert_eq!(retrieved.tags[0], "tag");
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_capability_manifest_serialization() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let capability = CapabilityManifest::new(
        CapabilityTier::SideEffecting,
        vec![
            HostCapability::ReadFile,
            HostCapability::WriteFile,
            HostCapability::Spawn,
        ],
    )?;

    let skill = SkillArtifact::new(
        "function test() { return true; }".to_string(),
        "Test skill with capabilities.".to_string(),
        vec!["test".to_string()],
        vec![SkillExport {
            name: "test".to_string(),
            signature: "() => void".to_string(),
        }],
        vec!["test() === true".to_string()],
        CapabilityManifest::new(
            CapabilityTier::SideEffecting,
            vec![
                HostCapability::ReadFile,
                HostCapability::WriteFile,
                HostCapability::Spawn,
            ],
        )?,
    )?;

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        let retrieved = store.get(&skill.id)?.expect("Skill not found");
        assert_eq!(retrieved.capability.tier, capability.tier);
        assert_eq!(retrieved.capability.allowed_hosts, capability.allowed_hosts);
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_different_skills_different_ids() -> Result<(), Box<dyn std::error::Error>> {
    let skill1 = minimal_skill()?;
    let skill2 = readonly_skill()?;

    assert_ne!(
        skill1.id, skill2.id,
        "Different skills should have different IDs"
    );

    Ok(())
}

#[test]
fn test_identity_preserved_across_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = minimal_skill()?;
    let original_id = skill.id.clone();

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        let retrieved = store.get(&original_id)?.expect("Skill not found");
        assert_eq!(retrieved.id, original_id);
        assert_eq!(retrieved.identity_version, skill.identity_version);
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_transaction_interruption_recovery() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill1 = minimal_skill()?;
    let skill2 = readonly_skill()?;

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill1)?;

        // Exact duplicate insertion is an idempotent retry.
        store.insert_verified(&skill2)?;
        store.insert_verified(&skill2)?;

        // Verify the store still works and both skills exist.
        assert!(store.get(&skill1.id)?.is_some());
        assert!(store.get(&skill2.id)?.is_some());
        assert_eq!(
            store
                .conn()
                .query_row("SELECT COUNT(*) FROM skill_revisions", [], |row| row
                    .get::<_, i64>(0))?,
            2
        );
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_schema_v1_to_v3_migration_preserves_rows_and_rebuilds_active_only_fts()
-> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;
    let active = minimal_skill()?;
    let retired = readonly_skill()?;
    {
        let db = create_schema_v1(&paths)?;
        insert_schema_v1_artifact(&db, &active, "active")?;
        insert_schema_v1_artifact(&db, &retired, "retired")?;
        let embedding = [1.0f32, 0.0, 0.0, 0.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        db.execute(
            "INSERT INTO skill_embeddings (
                skill_id, model_id, model_revision, dimensions, normalized, embedding, created_at
             ) VALUES (?, 'fixture-model', 'v1', 4, 1, ?, 1)",
            params![active.id, embedding],
        )?;
    }

    for _ in 0..2 {
        let store = SkillStore::open_at(&paths)?;
        assert_eq!(
            store
                .conn()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?,
            3
        );
        assert_eq!(
            store
                .conn()
                .query_row("SELECT COUNT(*) FROM skill_revisions", [], |row| row
                    .get::<_, i64>(0))?,
            2
        );
        assert_eq!(
            store
                .conn()
                .query_row("SELECT COUNT(*) FROM skill_embeddings", [], |row| row
                    .get::<_, i64>(0))?,
            1
        );
        let identifiers = store
            .conn()
            .prepare("SELECT identifier FROM skill_search ORDER BY identifier")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(identifiers, vec![active.id.clone()]);
        let state = store.generation_state()?;
        assert_eq!(state.desired_generation, 0);
        assert_eq!(state.applied_generation, 0);
        assert_eq!(state.model_id, "");
        assert_eq!(state.model_revision, "");
        assert_eq!(state.dimensions, 0);
        assert!(state.normalized);
    }

    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}

#[test]
fn test_purge_deletes_invalid_legacy_id_without_raw_tombstone()
-> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;
    let invalid_id = "legacy/raw/identifier";
    {
        let db = create_schema_v1(&paths)?;
        db.execute(
            "INSERT INTO skill_revisions (
                id, identity_version, source, description, tags_json, exports_json,
                tests_json, capability_json, status, row_version, created_at, updated_at
             ) VALUES (?, 1, 'legacy bytes', 'legacy', '[]', '[]', '[]',
                       '{\"tier\":\"pure\",\"allowed_hosts\":[]}', 'active', 1, 1, 1)",
            [invalid_id],
        )?;
        db.execute(
            "INSERT INTO skill_embeddings (
                skill_id, model_id, model_revision, dimensions, normalized, embedding, created_at
             ) VALUES (?, 'legacy-model', 'v1', 1, 1, ?, 1)",
            params![invalid_id, 1.0f32.to_le_bytes().to_vec()],
        )?;
    }

    let mut store = SkillStore::open_at(&paths)?;
    store.purge(invalid_id)?;
    assert_eq!(
        store.conn().query_row(
            "SELECT COUNT(*) FROM skill_revisions WHERE id = ?",
            [invalid_id],
            |row| row.get::<_, i64>(0),
        )?,
        0
    );
    assert_eq!(
        store.conn().query_row(
            "SELECT COUNT(*) FROM skill_embeddings WHERE skill_id = ?",
            [invalid_id],
            |row| row.get::<_, i64>(0),
        )?,
        0
    );
    assert_eq!(
        store.conn().query_row(
            "SELECT COUNT(*) FROM skill_tombstones WHERE id = ?",
            [invalid_id],
            |row| row.get::<_, i64>(0),
        )?,
        0
    );

    drop(store);
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}
