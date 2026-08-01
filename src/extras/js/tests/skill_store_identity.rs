use std::path::PathBuf;

use crate::extras::js::skills::store::{SkillStore, StoreError};
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::paths::AppPaths;

struct TempPaths {
    root: PathBuf,
    paths: AppPaths,
}

impl TempPaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-skill-identity-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local-data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: None,
        };
        Self { root, paths }
    }
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn artifact() -> SkillArtifact {
    SkillArtifact::new(
        "function increment(value) { return value + 1; }".to_string(),
        "Increment a number.".to_string(),
        vec!["math".to_string(), "number".to_string()],
        vec![SkillExport {
            name: "increment".to_string(),
            signature: "increment(value: number): number".to_string(),
        }],
        vec!["increment(2) === 3".to_string()],
        CapabilityManifest::pure(),
    )
    .expect("valid fixture")
}

#[test]
fn skill_store_identity_crud_is_idempotent_and_optimistic() {
    let temp = TempPaths::new();
    let mut store = SkillStore::open_at(&temp.paths).expect("open store");
    let artifact = artifact();

    store.insert_verified(&artifact).expect("initial insert");
    store
        .insert_verified(&artifact)
        .expect("byte-identical insert is idempotent");
    assert_eq!(store.get(&artifact.id).unwrap(), Some(artifact.clone()));

    let metadata = store.metadata(&artifact.id).unwrap().unwrap();
    assert_eq!(metadata.status, "active");
    store
        .retire(&artifact.id, metadata.row_version)
        .expect("optimistic retire");
    assert!(store.list_retrievable().unwrap().is_empty());
    assert!(matches!(
        store.retire(&artifact.id, metadata.row_version),
        Err(StoreError::StaleVersion { .. })
    ));
    assert_eq!(store.get(&artifact.id).unwrap(), Some(artifact));
}

#[test]
fn skill_store_identity_tamper_and_collision_fail_closed() {
    let temp = TempPaths::new();
    let mut store = SkillStore::open_at(&temp.paths).expect("open store");
    let artifact = artifact();
    store.insert_verified(&artifact).expect("insert");
    let tamper = store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions SET source = 'function increment() { return 99; }' WHERE id = ?",
            [&artifact.id],
        )
        .unwrap_err();
    assert!(tamper.to_string().contains("immutable skill identity"));
    assert_eq!(store.get(&artifact.id).unwrap(), Some(artifact.clone()));
    assert_eq!(store.list_retrievable().unwrap(), vec![artifact.clone()]);
    store
        .insert_verified(&artifact)
        .expect("exact immutable retry remains idempotent");
}

#[test]
fn skill_store_identity_purge_tombstones_and_removes_dependencies() {
    let temp = TempPaths::new();
    let mut store = SkillStore::open_at(&temp.paths).expect("open store");
    let artifact = artifact();
    store.insert_verified(&artifact).expect("insert");
    let bytes = [1.0f32, 0.0, 0.0, 0.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    store
        .store_embedding(&artifact.id, "model", "r1", 4, true, &bytes)
        .expect("store embedding");

    store.purge(&artifact.id).expect("purge");
    assert!(store.get(&artifact.id).unwrap().is_none());
    assert!(
        store
            .get_embedding(&artifact.id, "model", "r1")
            .unwrap()
            .is_none()
    );
    store
        .purge(&artifact.id)
        .expect("purge retry is idempotent");
    assert!(matches!(
        store.insert_verified(&artifact),
        Err(StoreError::Purged(_))
    ));
}

#[test]
fn skill_store_identity_embedding_and_generation_metadata_are_validated() {
    let temp = TempPaths::new();
    let mut store = SkillStore::open_at(&temp.paths).expect("open store");
    let artifact = artifact();
    store.insert_verified(&artifact).expect("insert");

    assert!(matches!(
        store.store_embedding(&artifact.id, "model", "r1", 4, true, &[0; 4]),
        Err(StoreError::MalformedEmbedding { .. })
    ));
    let bytes = [0.0f32, 1.0, 0.0, 0.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    store
        .store_embedding(&artifact.id, "model", "r1", 4, true, &bytes)
        .unwrap();
    let embedding = store
        .get_embedding(&artifact.id, "model", "r1")
        .unwrap()
        .unwrap();
    assert_eq!(embedding.values, vec![0.0, 1.0, 0.0, 0.0]);

    let generation = store.request_generation("model", "r1", 4, true).unwrap();
    assert_eq!(generation, 1);
    store.mark_generation_applied(generation).unwrap();
    let state = store.generation_state().unwrap();
    assert_eq!(state.desired_generation, 1);
    assert_eq!(state.applied_generation, 1);
    assert_eq!(state.model_revision, "r1");
}
