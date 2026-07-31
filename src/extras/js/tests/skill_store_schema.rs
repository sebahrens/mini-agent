//! Comprehensive fixtures for SkillStore schema, migrations, constraints, and lifecycle.

use crate::extras::js::skills::store::{SkillStore, StoreError};
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityTier, HostCapability, SkillArtifact, SkillExport,
};
use crate::paths::{AppPaths, PathEnvironment, PathPlatform};
use rusqlite::OptionalExtension;
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
        vec!["test('greet', () => greet() === 'hello')".to_string()],
        CapabilityManifest::pure(),
    )?)
}

/// Create a skill with ReadOnly capability.
fn readonly_skill() -> Result<SkillArtifact, Box<dyn std::error::Error>> {
    Ok(SkillArtifact::new(
        "function readConfig() { return read_file('/etc/config'); }".to_string(),
        "Read system configuration.".to_string(),
        vec!["config".to_string(), "read".to_string()],
        vec![SkillExport {
            name: "readConfig".to_string(),
            signature: "() => string".to_string(),
        }],
        vec!["test('readConfig', () => typeof readConfig() === 'string')".to_string()],
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
        vec!["test('deploy', () => deploy().code !== undefined)".to_string()],
        CapabilityManifest::new(CapabilityTier::SideEffecting, vec![HostCapability::Spawn])?,
    )?)
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
        let embedding_vec = vec![0.1f32, 0.2, 0.3, 0.4];
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
        let embedding_bytes = vec![1, 2, 3, 4];
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
fn test_duplicate_insert_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let temp_dir = temp_app_paths();
    let paths = resolve_test_paths(&temp_dir)?;

    let skill = minimal_skill()?;

    {
        let mut store = SkillStore::open_at(&paths)?;
        store.insert_verified(&skill)?;

        // Attempt to insert the same skill again.
        let result = store.insert_verified(&skill);
        assert!(result.is_err(), "Duplicate insert should be rejected");

        if let Err(StoreError::AlreadyExists(id)) = result {
            assert_eq!(id, skill.id);
        } else {
            panic!("Expected StoreError::AlreadyExists");
        }
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
        "test".to_string(),
        "Test skill".to_string(),
        vec!["TAG".to_string(), "tag".to_string(), "  tag  ".to_string()],
        vec![],
        vec!["test()".to_string()],
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
        "function test() {}".to_string(),
        "Test skill with capabilities.".to_string(),
        vec!["test".to_string()],
        vec![SkillExport {
            name: "test".to_string(),
            signature: "() => void".to_string(),
        }],
        vec!["test('test', () => true)".to_string()],
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

        // Attempt to insert skill2 twice; second should fail.
        store.insert_verified(&skill2)?;
        let result = store.insert_verified(&skill2);
        assert!(result.is_err());

        // Verify the store still works and both skills exist.
        assert!(store.get(&skill1.id)?.is_some());
        assert!(store.get(&skill2.id)?.is_some());
    }

    // Clean up.
    std::fs::remove_dir_all(&temp_dir)?;
    Ok(())
}
