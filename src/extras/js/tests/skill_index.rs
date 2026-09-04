use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::extras::js::skills::coordinator::IndexCoordinator;
use crate::extras::js::skills::embed::{
    Embedder, EmbeddingBackend, EmbeddingError, ModelMetadata, SkillDocument,
};
use crate::extras::js::skills::index::{ImmutableSkillIndex, RetrievalPolicy, SkillIndex};
use crate::extras::js::skills::store::SkillStore;
use crate::extras::js::skills::{CapabilityManifest, SkillArtifact, SkillExport};
use crate::paths::AppPaths;

struct TempPaths {
    root: PathBuf,
    paths: AppPaths,
}

impl TempPaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mini-agent-index-{}", uuid::Uuid::new_v4()));
        Self {
            paths: AppPaths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                local_data_dir: root.join("local-data"),
                state_dir: root.join("state"),
                cache_dir: root.join("cache"),
                credentials_dir: root.join("credentials"),
                project_dir: None,
            },
            root,
        }
    }
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn artifact(name: &str, description: &str, tag: &str) -> SkillArtifact {
    SkillArtifact::new(
        format!("function {name}(_cap, value) {{ return value; }}"),
        description.to_string(),
        vec![tag.to_string()],
        vec![SkillExport {
            name: name.to_string(),
            signature: format!("{name}(value: unknown): unknown"),
        }],
        vec![format!("{name}(7) === 7")],
        CapabilityManifest::pure(),
    )
    .unwrap()
}

struct ConcurrentAdmissionBackend {
    paths: AppPaths,
    artifact: Mutex<Option<SkillArtifact>>,
}

impl EmbeddingBackend for ConcurrentAdmissionBackend {
    fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if let Some(artifact) = self.artifact.lock().unwrap().take() {
            let mut store = SkillStore::open_at(&self.paths).unwrap();
            store.insert_verified(&artifact).unwrap();
            store
                .request_generation(
                    self.model_id(),
                    self.model_revision(),
                    self.dimensions(),
                    true,
                )
                .unwrap();
        }
        Ok(vec![vec![1.0, 0.0]; documents.len()])
    }

    fn embed_query(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(vec![1.0, 0.0])
    }

    fn model_id(&self) -> &str {
        "concurrent-admission-fixture"
    }

    fn model_revision(&self) -> &str {
        "v1"
    }

    fn dimensions(&self) -> usize {
        2
    }

    fn normalized(&self) -> bool {
        true
    }
}

fn vector_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn built_index(temp: &TempPaths) -> (ImmutableSkillIndex, SkillArtifact, SkillArtifact) {
    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    let json = artifact("parseJson", "Parse JSON documents safely.", "json");
    let csv = artifact("parseCsv", "Parse comma separated tables.", "csv");
    for (skill, vector) in [(&json, [1.0, 0.0]), (&csv, [0.0, 1.0])] {
        store.insert_verified(skill).unwrap();
        store
            .store_embedding(
                &skill.id,
                "fixture-model",
                "r1",
                2,
                true,
                &vector_bytes(&vector),
            )
            .unwrap();
    }
    let model = ModelMetadata {
        model_id: "fixture-model".to_string(),
        model_revision: "r1".to_string(),
        dimensions: 2,
        normalized: true,
    };
    let rows = store
        .list_retrievable()
        .unwrap()
        .into_iter()
        .map(|skill| {
            let embedding = store
                .get_embedding(&skill.id, "fixture-model", "r1")
                .unwrap()
                .unwrap();
            let metadata = store.metadata(&skill.id).unwrap().unwrap();
            (skill, embedding, metadata)
        })
        .collect();
    (
        ImmutableSkillIndex::build(7, model, store.database_path(), rows).unwrap(),
        json,
        csv,
    )
}

#[test]
fn skill_index_dense_fts_and_fusion_are_deterministic() {
    let temp = TempPaths::new();
    let (index, json, _) = built_index(&temp);
    let results = index
        .search("parse JSON", &[1.0, 0.0], &RetrievalPolicy::default())
        .unwrap();
    assert_eq!(results[0].artifact.id, json.id);
    assert_eq!(results[0].generation, 7);
    assert!(results[0].dense_score.is_some());
    assert!(results[0].lexical_score.is_some());
    assert_eq!(results[0].rank, 1);
}

#[test]
fn skill_index_lifecycle_floor_and_budgets_can_return_zero() {
    let temp = TempPaths::new();
    let (index, _, _) = built_index(&temp);
    let policy = RetrievalPolicy {
        dense_score_floor: 0.9,
        lexical_score_floor: 2.0,
        ..RetrievalPolicy::default()
    };
    assert!(
        index
            .search("unrelated", &[0.70710677, 0.70710677], &policy)
            .unwrap()
            .is_empty()
    );

    let policy = RetrievalPolicy {
        manifest_byte_budget: 1,
        source_byte_budget: 1,
        ..RetrievalPolicy::default()
    };
    assert!(
        index
            .search("json", &[1.0, 0.0], &policy)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn skill_index_concurrent_generation_leases_remain_frozen() {
    let temp = TempPaths::new();
    let (index, _, _) = built_index(&temp);
    let index = Arc::new(index);
    let handles = (0..8)
        .map(|_| {
            let index = Arc::clone(&index);
            std::thread::spawn(move || {
                index
                    .search("csv", &[0.0, 1.0], &RetrievalPolicy::default())
                    .unwrap()[0]
                    .artifact
                    .id
                    .clone()
            })
        })
        .collect::<Vec<_>>();
    let ids = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn skill_index_generations_publish_complete_snapshots_and_recover() {
    let temp = TempPaths::new();
    let skill = artifact("slugify", "Create URL-safe slugs.", "text");
    SkillStore::open_at(&temp.paths)
        .and_then(|mut store| store.insert_verified(&skill))
        .unwrap();
    let coordinator =
        IndexCoordinator::open(&temp.paths, Arc::new(Embedder::new().unwrap())).unwrap();
    let old = coordinator.lease().unwrap();
    assert!(old.is_empty());
    let generation = coordinator.rebuild_and_publish().unwrap();
    let new = coordinator.lease().unwrap();
    assert_eq!(new.generation(), generation);
    assert_eq!(new.len(), 1);
    assert!(old.is_empty(), "an existing turn lease must stay immutable");

    let hidden_generation = coordinator.retire_and_publish(&skill.id, 1).unwrap();
    let hidden = coordinator.lease().unwrap();
    assert_eq!(hidden.generation(), hidden_generation);
    assert!(hidden.is_empty());
    assert_eq!(new.len(), 1, "older leases can finish after removal");
    let (purged_generation, _) =
        crate::extras::js::skills::retention::CoordinatedRetention::new(&coordinator)
            .privacy_purge(&skill.id, "test_request", 10)
            .unwrap();
    assert_eq!(
        coordinator.lease().unwrap().generation(),
        purged_generation as u64
    );

    drop(coordinator);
    let reopened = IndexCoordinator::open(&temp.paths, Arc::new(Embedder::new().unwrap())).unwrap();
    reopened.rebuild_and_publish().unwrap();
    assert!(
        reopened.lease().unwrap().is_empty(),
        "durably retired skills must not resurrect after restart"
    );
}

#[test]
fn skill_index_rebuild_batches_and_refreshes_embedding_only_rows() {
    let temp = TempPaths::new();
    let skill = artifact("slugify", "Create URL-safe slugs.", "text");
    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    store.insert_verified(&skill).unwrap();
    let before = store
        .snapshot_embeddings_only("mini-agent-deterministic", "v1")
        .unwrap();
    assert_eq!(before.len(), 1);
    assert!(before[0].1.is_none());
    drop(store);

    let embedder = Arc::new(Embedder::new().unwrap());
    let expected_document = SkillDocument::new(skill.description.clone())
        .with_exports(
            skill
                .exports
                .iter()
                .map(|export| (export.name.clone(), export.signature.clone()))
                .collect(),
        )
        .with_tags(skill.tags.clone())
        .with_identifiers(
            skill
                .exports
                .iter()
                .map(|export| export.name.clone())
                .collect(),
        )
        .render();
    let expected = embedder
        .embed_documents(&[expected_document])
        .unwrap()
        .remove(0);
    let model = embedder.model_metadata().clone();
    let coordinator = IndexCoordinator::open(&temp.paths, embedder).unwrap();
    coordinator.rebuild_and_publish().unwrap();
    drop(coordinator);

    let store = SkillStore::open_at(&temp.paths).unwrap();
    let after = store
        .snapshot_embeddings_only(&model.model_id, &model.model_revision)
        .unwrap();
    assert_eq!(after.len(), 1);
    let embedding = after[0].1.as_ref().expect("compatible embedding");
    assert_eq!(embedding.values, expected);
    let stored_bytes: Vec<u8> = store
        .conn()
        .query_row(
            "SELECT embedding FROM skill_embeddings
              WHERE skill_id = ?1 AND model_id = ?2 AND model_revision = ?3",
            (&skill.id, &model.model_id, &model.model_revision),
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_bytes, vector_bytes(&expected));
}

#[test]
fn skill_index_rebuild_rejects_generation_advanced_by_concurrent_admission() {
    let temp = TempPaths::new();
    let initial = artifact("initialSkill", "Initial searchable skill.", "initial");
    let concurrent = artifact(
        "concurrentSkill",
        "Skill admitted during index embedding.",
        "concurrent",
    );
    SkillStore::open_at(&temp.paths)
        .and_then(|mut store| store.insert_verified(&initial))
        .unwrap();

    let embedder = Embedder::with_backend(Arc::new(ConcurrentAdmissionBackend {
        paths: temp.paths.clone(),
        artifact: Mutex::new(Some(concurrent.clone())),
    }))
    .unwrap();
    let coordinator = IndexCoordinator::open(&temp.paths, Arc::new(embedder)).unwrap();

    let error = coordinator
        .rebuild_and_publish()
        .expect_err("a stale generation must not be acknowledged");
    assert!(error.to_string().contains("durable generation"));
    assert!(coordinator.lease().unwrap().is_empty());
    assert!(coordinator.needs_refresh().unwrap());

    let store = SkillStore::open_at(&temp.paths).unwrap();
    assert!(store.get(&concurrent.id).unwrap().is_some());
    let state = store.generation_state().unwrap();
    assert!(state.desired_generation > state.applied_generation);
}

#[test]
fn skill_retrieval_relevance() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/skill_relevance.json")).unwrap();
    let temp = TempPaths::new();
    let (index, json, csv) = built_index(&temp);
    for case in fixture["cases"].as_array().unwrap() {
        let vector = case["vector"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_f64().unwrap() as f32)
            .collect::<Vec<_>>();
        let policy = RetrievalPolicy {
            dense_score_floor: case["dense_floor"].as_f64().unwrap() as f32,
            lexical_score_floor: case["lexical_floor"].as_f64().unwrap() as f32,
            ..RetrievalPolicy::default()
        };
        let result = index
            .search(case["query"].as_str().unwrap(), &vector, &policy)
            .unwrap();
        let expected = case["expected"].as_str().map(|label| match label {
            "json" => json.id.as_str(),
            "csv" => csv.id.as_str(),
            other => panic!("unknown fixture label {other}"),
        });
        assert_eq!(
            result.first().map(|skill| skill.artifact.id.as_str()),
            expected,
            "relevance fixture {} failed",
            case["name"]
        );
    }
}
