use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::extras::js::skills::coordinator::IndexCoordinator;
use crate::extras::js::skills::embed::{
    Embedder, EmbeddingBackend, EmbeddingError, ModelMetadata, SkillDocument,
};
use crate::extras::js::skills::index::{ImmutableSkillIndex, RetrievalPolicy, SkillIndex};
use crate::extras::js::skills::store::SkillStore;
use crate::extras::js::skills::{
    CapabilityManifest, CapabilityScope, CapabilityTier, SkillArtifact, SkillExport,
};
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

struct FailingEmbeddingBackend;

struct RevisionEmbeddingBackend;

struct BlockingEmbeddingBackend {
    entered: Mutex<Option<SyncSender<()>>>,
    release: Mutex<Receiver<()>>,
}

impl EmbeddingBackend for FailingEmbeddingBackend {
    fn embed_documents(&self, _documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(EmbeddingError::RequestFailed("fixture outage".to_string()))
    }

    fn embed_query(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::RequestFailed("fixture outage".to_string()))
    }

    fn model_id(&self) -> &str {
        "failing-fixture"
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

impl EmbeddingBackend for RevisionEmbeddingBackend {
    fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(vec![vec![1.0, 0.0]; documents.len()])
    }

    fn embed_query(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(vec![1.0, 0.0])
    }

    fn model_id(&self) -> &str {
        "routing-fixture"
    }

    fn model_revision(&self) -> &str {
        "v2"
    }

    fn dimensions(&self) -> usize {
        2
    }

    fn normalized(&self) -> bool {
        true
    }
}

impl EmbeddingBackend for BlockingEmbeddingBackend {
    fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        Ok(vec![vec![1.0, 0.0]; documents.len()])
    }

    fn embed_query(&self, _query: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(vec![1.0, 0.0])
    }

    fn model_id(&self) -> &str {
        "blocking-fixture"
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
fn skill_index_natural_language_query_uses_or_bm25_without_dense_candidates() {
    let temp = TempPaths::new();
    let (index, json, _) = built_index(&temp);
    let policy = RetrievalPolicy {
        dense_candidate_limit: 0,
        ..RetrievalPolicy::default()
    };
    let results = index
        .search(
            "please parse this JSON file and print the keys",
            &[0.0, 1.0],
            &policy,
        )
        .unwrap();
    assert_eq!(results.len(), 2, "OR semantics may return partial matches");
    assert_eq!(results[0].artifact.id, json.id);
    assert!(results.iter().all(|skill| skill.dense_score.is_none()));
    assert!(results[0].lexical_score.is_some());
}

#[test]
fn lexical_scores_preserve_bm25_relevance_and_the_floor_filters_candidates() {
    let temp = TempPaths::new();
    let (index, json, csv) = built_index(&temp);
    let lexical_only = RetrievalPolicy {
        dense_candidate_limit: 0,
        ..RetrievalPolicy::default()
    };
    let results = index
        .search("parse JSON", &[0.0, 1.0], &lexical_only)
        .unwrap();
    let json_score = results
        .iter()
        .find(|skill| skill.artifact.id == json.id)
        .and_then(|skill| skill.lexical_score)
        .expect("JSON lexical score");
    let csv_score = results
        .iter()
        .find(|skill| skill.artifact.id == csv.id)
        .and_then(|skill| skill.lexical_score)
        .expect("CSV lexical score");
    assert!(
        json_score > csv_score,
        "{json_score} must exceed {csv_score}"
    );

    let filtered = index
        .search(
            "parse JSON",
            &[0.0, 1.0],
            &RetrievalPolicy {
                dense_candidate_limit: 0,
                lexical_score_floor: (json_score + csv_score) / 2.0,
                ..RetrievalPolicy::default()
            },
        )
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].artifact.id, json.id);
}

#[test]
fn lexical_search_reads_the_durable_skill_search_table() {
    let temp = TempPaths::new();
    let (index, _, _) = built_index(&temp);
    let policy = RetrievalPolicy {
        dense_candidate_limit: 0,
        ..RetrievalPolicy::default()
    };
    assert!(
        !index
            .search("parse", &[0.0, 1.0], &policy)
            .unwrap()
            .is_empty()
    );

    let store = SkillStore::open_at(&temp.paths).unwrap();
    store
        .conn()
        .execute("DELETE FROM skill_search", [])
        .unwrap();
    drop(store);

    assert!(
        index
            .search("parse", &[0.0, 1.0], &policy)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn pure_search_filters_effectful_rows_before_lexical_candidate_limits() {
    let temp = TempPaths::new();
    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    let effectful = SkillArtifact::new(
        "function effectfulParser(_cap, value) { return value; }".to_string(),
        "Parse JSON JSON JSON documents exactly.".to_string(),
        vec!["parse".to_string(), "json".to_string()],
        vec![SkillExport {
            name: "effectfulParser".to_string(),
            signature: "effectfulParser(value: unknown): unknown".to_string(),
        }],
        vec!["effectfulParser(7) === 7".to_string()],
        CapabilityManifest::new(
            CapabilityTier::ReadOnly,
            vec![CapabilityScope::ReadFile {
                workspace_prefixes: vec!["src".to_string()],
            }],
        )
        .unwrap(),
    )
    .unwrap();
    let pure = artifact(
        "pureParser",
        "Parse structured documents without effects.",
        "parse",
    );
    for skill in [&effectful, &pure] {
        store.insert_verified(skill).unwrap();
        store
            .store_embedding(
                &skill.id,
                "fixture-model",
                "r1",
                2,
                true,
                &vector_bytes(&[1.0, 0.0]),
            )
            .unwrap();
    }
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
    let index = ImmutableSkillIndex::build(
        9,
        ModelMetadata {
            model_id: "fixture-model".to_string(),
            model_revision: "r1".to_string(),
            dimensions: 2,
            normalized: true,
        },
        store.database_path(),
        rows,
    )
    .unwrap();
    let output = index
        .search_pure_with_metrics(
            "parse JSON",
            &[1.0, 0.0],
            &RetrievalPolicy {
                max_skills: 1,
                dense_candidate_limit: 0,
                lexical_candidate_limit: 1,
                ..RetrievalPolicy::default()
            },
        )
        .unwrap();

    assert_eq!(output.skills.len(), 1);
    assert_eq!(output.skills[0].artifact.id, pure.id);
    assert_eq!(
        output.skills[0].artifact.capability.tier,
        CapabilityTier::Pure
    );
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
fn process_restart_hydrates_the_applied_generation_without_advancing_it() {
    let temp = TempPaths::new();
    let skill = artifact("stableGeneration", "Keep the generation stable.", "index");
    SkillStore::open_at(&temp.paths)
        .and_then(|mut store| store.insert_verified(&skill))
        .unwrap();
    let embedder = Arc::new(Embedder::new().unwrap());
    let first = IndexCoordinator::open(&temp.paths, Arc::clone(&embedder)).unwrap();
    let applied = first.rebuild_and_publish().unwrap();
    drop(first);

    let reopened = IndexCoordinator::open(&temp.paths, embedder).unwrap();
    let hydrated = reopened.rebuild_and_publish().unwrap();
    assert_eq!(hydrated, applied);
    assert!(reopened.lease().unwrap().contains_id(&skill.id));
    let state = SkillStore::open_at(&temp.paths)
        .unwrap()
        .generation_state()
        .unwrap();
    assert_eq!(state.desired_generation, applied);
    assert_eq!(state.applied_generation, applied);
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
fn skill_index_rebuild_backfills_canaries_for_the_new_model_revision() {
    let temp = TempPaths::new();
    let active = artifact("activeRun", "Active skill.", "routing");
    let canary = artifact("canaryRun", "Replacement canary.", "routing");
    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    store.insert_verified(&active).unwrap();
    store.insert_verified(&canary).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions SET status = 'active', lineage_root_id = id WHERE id = ?1",
            [&active.id],
        )
        .unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_revisions
             SET status = 'canary', supersedes_id = ?1, lineage_root_id = ?1
             WHERE id = ?2",
            rusqlite::params![active.id, canary.id],
        )
        .unwrap();
    drop(store);

    let embedder = Arc::new(Embedder::with_backend(Arc::new(RevisionEmbeddingBackend)).unwrap());
    let model = embedder.model_metadata().clone();
    let coordinator = IndexCoordinator::open(&temp.paths, embedder).unwrap();
    coordinator.rebuild_and_publish().unwrap();

    assert!(coordinator.lease().unwrap().contains_id(&active.id));
    assert!(!coordinator.lease().unwrap().contains_id(&canary.id));
    let store = SkillStore::open_at(&temp.paths).unwrap();
    let embedding = store
        .get_embedding(&canary.id, &model.model_id, &model.model_revision)
        .unwrap()
        .expect("canary embedding should migrate with the active generation");
    assert_eq!(embedding.values, vec![1.0, 0.0]);
}

#[test]
fn corrupt_embedding_row_is_reported_as_missing_and_repaired_without_hiding_siblings() {
    let temp = TempPaths::new();
    let first = artifact("parseJson", "Parse JSON documents.", "json");
    let second = artifact("parseCsv", "Parse CSV documents.", "csv");
    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    store.insert_verified(&first).unwrap();
    store.insert_verified(&second).unwrap();
    drop(store);

    let embedder = Arc::new(Embedder::new().unwrap());
    let model = embedder.model_metadata().clone();
    let coordinator = IndexCoordinator::open(&temp.paths, Arc::clone(&embedder)).unwrap();
    coordinator.rebuild_and_publish().unwrap();
    assert_eq!(coordinator.lease().unwrap().len(), 2);
    drop(coordinator);

    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_embeddings SET embedding = X'00' WHERE skill_id = ?1",
            [&first.id],
        )
        .unwrap();
    store
        .request_generation(
            &model.model_id,
            &model.model_revision,
            model.dimensions,
            model.normalized,
        )
        .unwrap();
    let rows = store
        .snapshot_rows(&model.model_id, &model.model_revision)
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .find(|(artifact, _, _)| artifact.id == first.id)
            .is_some_and(|(_, embedding, _)| embedding.is_none())
    );
    assert!(
        rows.iter()
            .find(|(artifact, _, _)| artifact.id == second.id)
            .is_some_and(|(_, embedding, _)| embedding.is_some())
    );
    drop(store);

    let reopened = IndexCoordinator::open(&temp.paths, Arc::clone(&embedder)).unwrap();
    reopened.rebuild_and_publish().unwrap();
    assert_eq!(reopened.lease().unwrap().len(), 2);
    let store = SkillStore::open_at(&temp.paths).unwrap();
    assert!(
        store
            .get_embedding(&first.id, &model.model_id, &model.model_revision)
            .unwrap()
            .is_some(),
        "the rebuild should replace the corrupt row"
    );
    drop(store);
    drop(reopened);

    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    store
        .conn_mut()
        .execute(
            "UPDATE skill_embeddings SET normalized = 2 WHERE skill_id = ?1",
            [&first.id],
        )
        .unwrap();
    store
        .request_generation(
            &model.model_id,
            &model.model_revision,
            model.dimensions,
            model.normalized,
        )
        .unwrap();
    drop(store);

    let reopened = IndexCoordinator::open(&temp.paths, embedder).unwrap();
    reopened.rebuild_and_publish().unwrap();
    let store = SkillStore::open_at(&temp.paths).unwrap();
    assert!(
        store
            .get_embedding(&first.id, &model.model_id, &model.model_revision)
            .unwrap()
            .is_some_and(|embedding| embedding.normalized),
        "incompatible metadata should also be re-embedded"
    );
}

#[test]
fn failed_rebuilds_back_off_before_another_background_attempt() {
    let temp = TempPaths::new();
    let skill = artifact("parseJson", "Parse JSON documents.", "json");
    SkillStore::open_at(&temp.paths)
        .and_then(|mut store| store.insert_verified(&skill))
        .unwrap();
    let embedder = Arc::new(Embedder::with_backend(Arc::new(FailingEmbeddingBackend)).unwrap());
    let coordinator = Arc::new(IndexCoordinator::open(&temp.paths, embedder).unwrap());

    let error = coordinator.rebuild_and_publish().unwrap_err();
    assert!(error.to_string().contains("fixture outage"));
    assert!(coordinator.needs_refresh().unwrap());
    assert!(
        coordinator
            .rebuild_backoff_diagnostic()
            .is_some_and(|diagnostic| diagnostic.contains("fixture outage"))
    );
    assert!(!coordinator.schedule_rebuild());
    assert_eq!(coordinator.rebuild_starts_for_test(), 0);
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
fn skill_index_rebuild_releases_store_lock_during_embedding_io() {
    let temp = TempPaths::new();
    let skill = artifact(
        "blockingSkill",
        "Wait while embedding this skill.",
        "blocking",
    );
    SkillStore::open_at(&temp.paths)
        .and_then(|mut store| store.insert_verified(&skill))
        .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let embedder = Embedder::with_backend(Arc::new(BlockingEmbeddingBackend {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    }))
    .unwrap();
    let coordinator = Arc::new(IndexCoordinator::open(&temp.paths, Arc::new(embedder)).unwrap());

    let rebuild_coordinator = Arc::clone(&coordinator);
    let rebuild = std::thread::spawn(move || rebuild_coordinator.rebuild_and_publish());
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("rebuild should enter embedding backend");

    let routing_coordinator = Arc::clone(&coordinator);
    let (routing_tx, routing_rx) = std::sync::mpsc::sync_channel(0);
    let routing = std::thread::spawn(move || {
        routing_tx.send(routing_coordinator.routing_key()).unwrap();
    });
    let routing_while_embedding = routing_rx.recv_timeout(Duration::from_secs(2));
    release_tx.send(()).unwrap();
    routing.join().unwrap();
    rebuild.join().unwrap().unwrap();

    assert!(
        routing_while_embedding
            .expect("SQLite access should not wait for embedding I/O")
            .is_ok()
    );
}

#[test]
fn coordinated_mutation_releases_store_lock_during_embedding_io() {
    let temp = TempPaths::new();
    let skill = artifact(
        "activatedBlockingSkill",
        "Activate while embedding this skill.",
        "blocking",
    );
    let skill_id = skill.id.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let embedder = Embedder::with_backend(Arc::new(BlockingEmbeddingBackend {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    }))
    .unwrap();
    let coordinator = Arc::new(IndexCoordinator::open(&temp.paths, Arc::new(embedder)).unwrap());

    let mutation_coordinator = Arc::clone(&coordinator);
    let mutation = std::thread::spawn(move || {
        mutation_coordinator.coordinate_mutation(
            std::collections::HashSet::new(),
            |store| -> Result<((), u64), crate::extras::js::skills::store::StoreError> {
                store.insert_verified(&skill)?;
                store
                    .conn_mut()
                    .execute(
                        "UPDATE skill_revisions SET status = 'active' WHERE id = ?1",
                        [&skill.id],
                    )
                    .map_err(crate::extras::js::skills::store::StoreError::from)?;
                let generation = store.request_generation("blocking-fixture", "v1", 2, true)?;
                Ok(((), generation))
            },
        )
    });
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("coordinated mutation should enter embedding backend");

    let routing_coordinator = Arc::clone(&coordinator);
    let (routing_tx, routing_rx) = std::sync::mpsc::sync_channel(0);
    let routing = std::thread::spawn(move || {
        routing_tx.send(routing_coordinator.routing_key()).unwrap();
    });
    let routing_while_embedding = routing_rx.recv_timeout(Duration::from_secs(2));
    release_tx.send(()).unwrap();
    routing.join().unwrap();
    let (_, report) = mutation.join().unwrap().unwrap();

    assert!(
        routing_while_embedding
            .expect("SQLite access should not wait for mutation embedding I/O")
            .is_ok()
    );
    assert!(!report.removal_only);
    assert!(coordinator.lease().unwrap().contains_id(&skill_id));
}

#[test]
fn routing_key_cache_is_scoped_to_the_published_generation() {
    let temp = TempPaths::new();
    let coordinator = IndexCoordinator::open(
        &temp.paths,
        Arc::new(Embedder::new().expect("deterministic embedder")),
    )
    .unwrap();
    let generation = coordinator.rebuild_and_publish().unwrap();
    let first = coordinator
        .routing_context(&[], generation)
        .unwrap()
        .expect("current routing generation")
        .key;

    let mut store = SkillStore::open_at(&temp.paths).unwrap();
    store
        .conn_mut()
        .execute(
            "DELETE FROM skill_runtime_secrets WHERE name = 'canary-routing-v1'",
            [],
        )
        .unwrap();
    let cached = coordinator
        .routing_context(&[], generation)
        .unwrap()
        .expect("same routing generation should remain current")
        .key;
    assert_eq!(cached, first);

    let model = coordinator.lease().unwrap().model().clone();
    store
        .request_generation(
            &model.model_id,
            &model.model_revision,
            model.dimensions,
            model.normalized,
        )
        .unwrap();
    drop(store);
    let next_generation = coordinator.rebuild_and_publish().unwrap();
    assert_ne!(next_generation, generation);
    let refreshed = coordinator
        .routing_context(&[], next_generation)
        .unwrap()
        .expect("new routing generation")
        .key;
    assert_ne!(refreshed, cached, "a new generation must reload its key");
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
