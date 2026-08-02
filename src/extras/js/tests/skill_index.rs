use std::path::PathBuf;
use std::sync::Arc;

use crate::extras::js::skills::coordinator::IndexCoordinator;
use crate::extras::js::skills::embed::{Embedder, ModelMetadata};
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
    let purged_generation = coordinator.purge_and_publish(&skill.id).unwrap();
    assert_eq!(coordinator.lease().unwrap().generation(), purged_generation);

    drop(coordinator);
    let reopened = IndexCoordinator::open(&temp.paths, Arc::new(Embedder::new().unwrap())).unwrap();
    reopened.rebuild_and_publish().unwrap();
    assert!(
        reopened.lease().unwrap().is_empty(),
        "durably retired skills must not resurrect after restart"
    );
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
