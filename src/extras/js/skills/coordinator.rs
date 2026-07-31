//! Off-request-path embedding migration and immutable index publication.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use super::embed::{Embedder, EmbeddingError, SkillDocument};
use super::index::{ImmutableSkillIndex, SkillIndex, SkillIndexError};
use super::store::{SkillStore, StoreError};
use crate::paths::AppPaths;

const EMBEDDING_BATCH_SIZE: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Embedding(#[from] EmbeddingError),
    #[error(transparent)]
    Index(#[from] SkillIndexError),
    #[error("index coordinator lock was poisoned")]
    Poisoned,
}

/// Coordinates one learned-JS retrieval domain. Publication swaps one complete `Arc`.
pub struct IndexCoordinator {
    store: Mutex<SkillStore>,
    embedder: Arc<Embedder>,
    published: RwLock<Arc<ImmutableSkillIndex>>,
}

impl IndexCoordinator {
    pub fn open(paths: &AppPaths, embedder: Arc<Embedder>) -> Result<Self, CoordinatorError> {
        let mut store = SkillStore::open_at(paths)?;
        let model = embedder.model_metadata().clone();
        let mut state = store.generation_state()?;
        if state.model_id != model.model_id
            || state.model_revision != model.model_revision
            || state.dimensions != model.dimensions
            || state.normalized != model.normalized
        {
            store.request_generation(
                &model.model_id,
                &model.model_revision,
                model.dimensions,
                model.normalized,
            )?;
            state = store.generation_state()?;
        }
        let generation = state.applied_generation;
        let empty = Arc::new(ImmutableSkillIndex::empty(
            generation,
            model,
            store.database_path().to_path_buf(),
        ));
        Ok(Self {
            store: Mutex::new(store),
            embedder,
            published: RwLock::new(empty),
        })
    }

    /// Clone the exact generation lease used by one turn. Later publication cannot alter it.
    pub fn lease(&self) -> Result<Arc<ImmutableSkillIndex>, CoordinatorError> {
        self.published
            .read()
            .map(|snapshot| Arc::clone(&snapshot))
            .map_err(|_| CoordinatorError::Poisoned)
    }

    pub fn active_count(&self) -> Result<usize, CoordinatorError> {
        self.store
            .lock()
            .map_err(|_| CoordinatorError::Poisoned)?
            .active_count()
            .map_err(Into::into)
    }

    /// Recover a pending generation or request and publish a fresh generation.
    /// This performs embedding and SQLite work and must run on a blocking worker.
    pub fn rebuild_and_publish(&self) -> Result<u64, CoordinatorError> {
        let model = self.embedder.model_metadata().clone();
        let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let state = store.generation_state()?;
        let generation = if state.desired_generation > state.applied_generation {
            state.desired_generation
        } else {
            store.request_generation(
                &model.model_id,
                &model.model_revision,
                model.dimensions,
                model.normalized,
            )?
        };

        let initial = store.snapshot_rows(&model.model_id, &model.model_revision)?;
        let missing = initial
            .iter()
            .filter(|(_, embedding, _)| embedding.is_none())
            .map(|(artifact, _, _)| (artifact.id.clone(), skill_document(artifact)))
            .collect::<Vec<_>>();
        for batch in missing.chunks(EMBEDDING_BATCH_SIZE) {
            let documents = batch
                .iter()
                .map(|(_, document)| document.clone())
                .collect::<Vec<_>>();
            let vectors = self.embedder.embed_documents(&documents)?;
            if vectors.len() != batch.len() {
                return Err(EmbeddingError::InvalidConfiguration(
                    "embedding backend returned the wrong batch size".to_string(),
                )
                .into());
            }
            for ((skill_id, _), vector) in batch.iter().zip(vectors) {
                let bytes = vector
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect::<Vec<_>>();
                store.store_embedding(
                    skill_id,
                    &model.model_id,
                    &model.model_revision,
                    model.dimensions as u32,
                    model.normalized,
                    &bytes,
                )?;
            }
        }

        // Re-read one joined view after inference so the published snapshot is
        // bound to a single identity/lifecycle scan.
        let rows = store
            .snapshot_rows(&model.model_id, &model.model_revision)?
            .into_iter()
            .map(|(artifact, embedding, metadata)| {
                let embedding = embedding.ok_or_else(|| StoreError::MalformedEmbedding {
                    skill_id: artifact.id.clone(),
                    reason: "compatible vector is missing after rebuild".to_string(),
                })?;
                Ok((artifact, embedding, metadata))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let snapshot = Arc::new(ImmutableSkillIndex::build_without_ann(
            generation,
            model,
            store.database_path(),
            rows,
        )?);
        // Durable state must acknowledge this exact generation before readers can
        // observe it. If persistence fails, the prior Arc remains published.
        store.mark_generation_applied(generation)?;
        {
            let mut published = self
                .published
                .write()
                .map_err(|_| CoordinatorError::Poisoned)?;
            *published = Arc::clone(&snapshot);
        }
        // Publish the exact/FTS generation first, then build the expensive graph
        // without holding the store lock. A lifecycle update advances the durable
        // generation and prevents this stale graph from being published.
        drop(store);
        if snapshot.ann_recommended() {
            let ann_snapshot = Arc::new(snapshot.with_ann());
            let store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
            let state = store.generation_state()?;
            if state.applied_generation == generation && state.desired_generation == generation {
                let mut published = self
                    .published
                    .write()
                    .map_err(|_| CoordinatorError::Poisoned)?;
                if published.generation() == generation {
                    *published = ann_snapshot;
                }
            }
        }
        Ok(generation)
    }

    /// Retire durable state before publishing removal to readers.
    pub fn retire_and_publish(
        &self,
        id: &str,
        expected_version: u64,
    ) -> Result<u64, CoordinatorError> {
        let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        self.mutate_and_publish_hidden(&mut store, HashSet::from([id.to_string()]), |store| {
            store.retire(id, expected_version)
        })
    }

    /// Privacy-purge durable bytes before publishing removal to readers.
    pub fn purge_and_publish(&self, id: &str) -> Result<u64, CoordinatorError> {
        let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        self.mutate_and_publish_hidden(&mut store, HashSet::from([id.to_string()]), |store| {
            store.purge(id)
        })
    }

    fn mutate_and_publish_hidden(
        &self,
        store: &mut SkillStore,
        hidden: HashSet<String>,
        mutation: impl FnOnce(&mut SkillStore) -> Result<(), StoreError>,
    ) -> Result<u64, CoordinatorError> {
        let model = self.embedder.model_metadata();
        // Acquire every in-memory fallible resource and construct the fail-closed
        // snapshot before changing durable visibility. Once mutation succeeds,
        // the filtered Arc is published even if generation acknowledgement fails.
        let mut published = self
            .published
            .write()
            .map_err(|_| CoordinatorError::Poisoned)?;
        let generation = store.request_generation(
            &model.model_id,
            &model.model_revision,
            model.dimensions,
            model.normalized,
        )?;
        let filtered = Arc::new(published.without_ids(generation, &hidden));
        mutation(store)?;
        let acknowledgement = store.mark_generation_applied(generation);
        *published = filtered;
        acknowledgement.map(|()| generation).map_err(Into::into)
    }
}

fn skill_document(artifact: &super::SkillArtifact) -> String {
    SkillDocument::new(artifact.description.clone())
        .with_exports(
            artifact
                .exports
                .iter()
                .map(|export| (export.name.clone(), export.signature.clone()))
                .collect(),
        )
        .with_tags(artifact.tags.clone())
        .with_identifiers(
            artifact
                .exports
                .iter()
                .map(|export| export.name.clone())
                .collect(),
        )
        .render()
}
