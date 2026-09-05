//! Off-request-path embedding migration and immutable index publication.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use super::embed::{Embedder, EmbeddingError, ModelMetadata, SkillDocument};
use super::index::{ImmutableSkillIndex, SkillIndex, SkillIndexError};
use super::store::{SkillRecordMetadata, SkillStore, StoreError, StoredEmbedding};
use crate::paths::AppPaths;
use rusqlite::OptionalExtension;

use super::CapabilityTier;
use super::lifecycle::LifecycleStatus;
use super::router::CanaryCandidate;

const EMBEDDING_BATCH_SIZE: usize = 256;
const REBUILD_BACKOFF_BASE: Duration = Duration::from_secs(1);
const REBUILD_BACKOFF_MAX: Duration = Duration::from_secs(60);

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationReport {
    pub generation: u64,
    pub removal_only: bool,
    pub diagnostic: Option<String>,
}

#[derive(Debug)]
pub enum CoordinatedMutationError<E> {
    Mutation(E),
    Publication(CoordinatorError),
}

/// Coordinates one learned-JS retrieval domain. Publication swaps one complete `Arc`.
pub struct IndexCoordinator {
    store: Mutex<SkillStore>,
    embedder: Arc<Embedder>,
    published: RwLock<Arc<ImmutableSkillIndex>>,
    hydrated: AtomicBool,
    rebuild_in_flight: AtomicBool,
    rebuild_backoff: Mutex<RebuildBackoff>,
    #[cfg(test)]
    rebuild_starts: AtomicUsize,
}

#[derive(Debug, Default)]
struct RebuildBackoff {
    consecutive_failures: u32,
    retry_not_before: Option<Instant>,
    last_error: Option<String>,
}

struct RebuildFlightGuard<'a>(&'a AtomicBool);

impl Drop for RebuildFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
            hydrated: AtomicBool::new(false),
            rebuild_in_flight: AtomicBool::new(false),
            rebuild_backoff: Mutex::new(RebuildBackoff::default()),
            #[cfg(test)]
            rebuild_starts: AtomicUsize::new(0),
        })
    }

    /// Clone the exact generation lease used by one turn. Later publication cannot alter it.
    pub fn lease(&self) -> Result<Arc<ImmutableSkillIndex>, CoordinatorError> {
        self.published
            .read()
            .map(|snapshot| Arc::clone(&snapshot))
            .map_err(|_| CoordinatorError::Poisoned)
    }

    pub fn needs_refresh(&self) -> Result<bool, CoordinatorError> {
        if self.rebuild_in_flight.load(Ordering::Acquire) {
            return Ok(true);
        }
        let state = self
            .store
            .lock()
            .map_err(|_| CoordinatorError::Poisoned)?
            .generation_state()?;
        Ok(!self.hydrated.load(Ordering::Acquire)
            || state.desired_generation > state.applied_generation
            || state.publication_mode != "full")
    }

    /// Schedule at most one tracked rebuild while readers keep using the last
    /// immutable generation. The caller deliberately does not await the join
    /// handle: the current agent work scope owns it through cancellation.
    pub fn schedule_rebuild(self: &Arc<Self>) -> bool {
        if self.rebuild_backoff_active().unwrap_or(true) {
            return false;
        }
        if self
            .rebuild_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        #[cfg(test)]
        self.rebuild_starts.fetch_add(1, Ordering::Relaxed);
        let coordinator = Arc::clone(self);
        std::mem::drop(crate::agent::runner::spawn_blocking_scoped(move || {
            let _flight = RebuildFlightGuard(&coordinator.rebuild_in_flight);
            if let Err(error) = coordinator.rebuild_and_publish() {
                tracing::warn!(%error, "learned-skill background rebuild failed");
            }
        }));
        true
    }

    /// Resolve one eligible replacement canary against the exact applied
    /// generation used by a turn. Root canaries are excluded by construction.
    pub fn replacement_candidate(
        &self,
        active_id: &str,
        expected_generation: u64,
    ) -> Result<Option<(super::SkillArtifact, CanaryCandidate)>, CoordinatorError> {
        let store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let state = store.generation_state()?;
        if state.applied_generation != expected_generation
            || state.desired_generation != expected_generation
        {
            return Ok(None);
        }
        let active_lineage: Option<String> = store
            .connection()
            .query_row(
                "SELECT lineage_root_id FROM skill_revisions
                 WHERE id = ? AND status = 'active'",
                [active_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let Some(active_lineage) = active_lineage else {
            return Ok(None);
        };
        let candidate_id: Option<String> = store
            .connection()
            .query_row(
                "SELECT id FROM skill_revisions
                 WHERE supersedes_id = ? AND lineage_root_id = ? AND status = 'canary'
                 ORDER BY id LIMIT 1",
                [active_id, active_lineage.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let Some(candidate_id) = candidate_id else {
            return Ok(None);
        };
        let Some(artifact) = store.get(&candidate_id)? else {
            return Ok(None);
        };
        let model = self.embedder.model_metadata();
        let compatible = store
            .get_embedding(&candidate_id, &model.model_id, &model.model_revision)?
            .is_some_and(|embedding| {
                embedding.dimensions == model.dimensions && embedding.normalized == model.normalized
            });
        let candidate = CanaryCandidate {
            candidate_id,
            lineage_root_id: active_lineage,
            status: LifecycleStatus::Canary,
            model_compatible: compatible,
            identity_valid: artifact.verify_identity().is_ok(),
            capability_tier: artifact.capability.tier,
            explicitly_idempotent: artifact.capability.tier == CapabilityTier::Pure,
        };
        Ok(Some((artifact, candidate)))
    }

    pub fn routing_key(&self) -> Result<[u8; 32], CoordinatorError> {
        use sha2::{Digest, Sha256};
        let mut store = self.store.lock().map_err(|_| CoordinatorError::Poisoned)?;
        let existing: Option<Vec<u8>> = store
            .connection()
            .query_row(
                "SELECT secret FROM skill_runtime_secrets WHERE name = 'canary-routing-v1'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        let bytes = match existing {
            Some(bytes) => bytes,
            None => {
                let mut digest = Sha256::new();
                digest.update(b"mini-agent/local-canary-key/v1");
                digest.update(uuid::Uuid::new_v4().as_bytes());
                let bytes = digest.finalize().to_vec();
                store
                    .connection_mut()
                    .execute(
                        "INSERT OR IGNORE INTO skill_runtime_secrets (name, secret, created_at)
                         VALUES ('canary-routing-v1', ?, strftime('%s','now'))",
                        [&bytes],
                    )
                    .map_err(StoreError::from)?;
                store
                    .connection()
                    .query_row(
                        "SELECT secret FROM skill_runtime_secrets
                         WHERE name = 'canary-routing-v1'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(StoreError::from)?
            }
        };
        bytes.try_into().map_err(|_| {
            StoreError::Constraint("canary routing key has an invalid length".to_string()).into()
        })
    }

    /// Commit a lifecycle mutation, publish removals under the new-turn gate,
    /// then build additions off that gate. A failed rebuild leaves the verified
    /// removal-only generation published.
    pub fn coordinate_mutation<R, E>(
        &self,
        removed_ids: HashSet<String>,
        mutation: impl FnOnce(&mut SkillStore) -> Result<(R, u64), E>,
    ) -> Result<(R, PublicationReport), CoordinatedMutationError<E>> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        let mut published = self
            .published
            .write()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        let (result, generation) =
            mutation(&mut store).map_err(CoordinatedMutationError::Mutation)?;

        // Publish committed removals immediately. Additions remain invisible
        // while the complete generation is built off the new-turn gate.
        *published = Arc::new(published.without_ids(generation, &removed_ids));
        let removal_acknowledgement =
            store.mark_generation_applied_with_mode(generation, "removal_only", None);
        drop(published);
        if let Err(error) = removal_acknowledgement {
            return Err(CoordinatedMutationError::Publication(error.into()));
        }

        match build_generation(&mut store, &self.embedder, generation) {
            Ok(snapshot) => {
                store
                    .mark_generation_applied_with_mode(generation, "full", None)
                    .map_err(CoordinatorError::from)
                    .map_err(CoordinatedMutationError::Publication)?;
                let mut published = self.published.write().map_err(|_| {
                    CoordinatedMutationError::Publication(CoordinatorError::Poisoned)
                })?;
                *published = Arc::new(snapshot);
                self.hydrated.store(true, Ordering::Release);
                self.reset_rebuild_backoff();
                Ok((
                    result,
                    PublicationReport {
                        generation,
                        removal_only: false,
                        diagnostic: None,
                    },
                ))
            }
            Err(error) => {
                self.record_rebuild_failure(&error);
                let diagnostic = error.to_string();
                store
                    .mark_generation_applied_with_mode(
                        generation,
                        "removal_only",
                        Some(&diagnostic),
                    )
                    .map_err(CoordinatorError::from)
                    .map_err(CoordinatedMutationError::Publication)?;
                Ok((
                    result,
                    PublicationReport {
                        generation,
                        removal_only: true,
                        diagnostic: Some(diagnostic),
                    },
                ))
            }
        }
    }

    /// Commit and publish a removal-only generation without waiting for a
    /// physical rebuild. The next turn refresh compacts it off the JS and
    /// telemetry threads.
    pub fn coordinate_removal<R, E>(
        &self,
        removed_ids: HashSet<String>,
        mutation: impl FnOnce(&mut SkillStore) -> Result<(R, u64), E>,
    ) -> Result<(R, PublicationReport), CoordinatedMutationError<E>> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        let mut published = self
            .published
            .write()
            .map_err(|_| CoordinatedMutationError::Publication(CoordinatorError::Poisoned))?;
        let (result, generation) =
            mutation(&mut store).map_err(CoordinatedMutationError::Mutation)?;
        *published = Arc::new(published.without_ids(generation, &removed_ids));
        store
            .mark_generation_applied_with_mode(generation, "removal_only", None)
            .map_err(CoordinatorError::from)
            .map_err(CoordinatedMutationError::Publication)?;
        Ok((
            result,
            PublicationReport {
                generation,
                removal_only: true,
                diagnostic: None,
            },
        ))
    }

    /// Recover a pending generation or request and publish a fresh generation.
    /// This performs embedding and SQLite work and must run on a blocking worker.
    pub fn rebuild_and_publish(&self) -> Result<u64, CoordinatorError> {
        let result = self.rebuild_and_publish_inner();
        match &result {
            Ok(_) => self.reset_rebuild_backoff(),
            Err(error) => self.record_rebuild_failure(error),
        }
        result
    }

    fn rebuild_and_publish_inner(&self) -> Result<u64, CoordinatorError> {
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
            .filter(|(_, embedding, _)| {
                embedding
                    .as_ref()
                    .is_none_or(|embedding| !embedding_is_compatible(embedding, &model))
            })
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
            let embeddings = batch
                .iter()
                .zip(vectors)
                .map(|((skill_id, _), vector)| (skill_id.clone(), vector))
                .collect::<Vec<_>>();
            store.store_embedding_batch(
                &model.model_id,
                &model.model_revision,
                model.dimensions,
                model.normalized,
                &embeddings,
            )?;
        }

        let rows = refresh_snapshot_embeddings(&store, &model, initial)?;
        ensure_generation_current(&store, generation)?;
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
            self.hydrated.store(true, Ordering::Release);
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

    fn rebuild_backoff_active(&self) -> Result<bool, CoordinatorError> {
        let state = self
            .rebuild_backoff
            .lock()
            .map_err(|_| CoordinatorError::Poisoned)?;
        Ok(state
            .retry_not_before
            .is_some_and(|retry_not_before| Instant::now() < retry_not_before))
    }

    fn record_rebuild_failure(&self, error: &CoordinatorError) {
        let Ok(mut state) = self.rebuild_backoff.lock() else {
            return;
        };
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let shift = state.consecutive_failures.saturating_sub(1).min(6);
        let multiplier = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        let delay = REBUILD_BACKOFF_BASE
            .saturating_mul(multiplier)
            .min(REBUILD_BACKOFF_MAX);
        state.retry_not_before = Some(Instant::now() + delay);
        state.last_error = Some(error.to_string());
    }

    fn reset_rebuild_backoff(&self) {
        if let Ok(mut state) = self.rebuild_backoff.lock() {
            *state = RebuildBackoff::default();
        }
    }

    pub(crate) fn rebuild_backoff_diagnostic(&self) -> Option<String> {
        let state = self.rebuild_backoff.lock().ok()?;
        state
            .retry_not_before
            .filter(|retry_not_before| Instant::now() < *retry_not_before)
            .and_then(|_| state.last_error.clone())
    }

    #[cfg(test)]
    pub(crate) fn rebuild_starts_for_test(&self) -> usize {
        self.rebuild_starts.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn rebuild_in_flight_for_test(&self) -> bool {
        self.rebuild_in_flight.load(Ordering::Acquire)
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

fn build_generation(
    store: &mut SkillStore,
    embedder: &Embedder,
    generation: u64,
) -> Result<ImmutableSkillIndex, CoordinatorError> {
    let model = embedder.model_metadata().clone();
    ensure_generation_current(store, generation)?;
    let initial = store.snapshot_rows(&model.model_id, &model.model_revision)?;
    let missing = initial
        .iter()
        .filter(|(_, embedding, _)| {
            embedding
                .as_ref()
                .is_none_or(|embedding| !embedding_is_compatible(embedding, &model))
        })
        .map(|(artifact, _, _)| (artifact.id.clone(), skill_document(artifact)))
        .collect::<Vec<_>>();
    for batch in missing.chunks(EMBEDDING_BATCH_SIZE) {
        let documents = batch
            .iter()
            .map(|(_, document)| document.clone())
            .collect::<Vec<_>>();
        let vectors = embedder.embed_documents(&documents)?;
        if vectors.len() != batch.len() {
            return Err(EmbeddingError::InvalidConfiguration(
                "embedding backend returned the wrong batch size".to_string(),
            )
            .into());
        }
        let embeddings = batch
            .iter()
            .zip(vectors)
            .map(|((skill_id, _), vector)| (skill_id.clone(), vector))
            .collect::<Vec<_>>();
        store.store_embedding_batch(
            &model.model_id,
            &model.model_revision,
            model.dimensions,
            model.normalized,
            &embeddings,
        )?;
    }
    let rows = refresh_snapshot_embeddings(store, &model, initial)?;
    ensure_generation_current(store, generation)?;
    ImmutableSkillIndex::build(generation, model, store.database_path(), rows).map_err(Into::into)
}

fn ensure_generation_current(store: &SkillStore, generation: u64) -> Result<(), StoreError> {
    let state = store.generation_state()?;
    if state.desired_generation == generation {
        Ok(())
    } else {
        Err(StoreError::Constraint(format!(
            "lifecycle requested generation {generation}, durable generation is {}",
            state.desired_generation
        )))
    }
}

fn refresh_snapshot_embeddings(
    store: &SkillStore,
    model: &ModelMetadata,
    initial: Vec<(
        super::SkillArtifact,
        Option<StoredEmbedding>,
        SkillRecordMetadata,
    )>,
) -> Result<Vec<(super::SkillArtifact, StoredEmbedding, SkillRecordMetadata)>, StoreError> {
    let mut embeddings = store
        .snapshot_embeddings_only(&model.model_id, &model.model_revision)?
        .into_iter()
        .map(|(skill_id, embedding, metadata)| (skill_id, (embedding, metadata)))
        .collect::<HashMap<_, _>>();

    initial
        .into_iter()
        .map(|(artifact, _, _)| {
            let (embedding, metadata) =
                embeddings
                    .remove(&artifact.id)
                    .ok_or_else(|| StoreError::MalformedEmbedding {
                        skill_id: artifact.id.clone(),
                        reason: "active row disappeared during rebuild".to_string(),
                    })?;
            let embedding = embedding.ok_or_else(|| StoreError::MalformedEmbedding {
                skill_id: artifact.id.clone(),
                reason: "compatible vector is missing after rebuild".to_string(),
            })?;
            if !embedding_is_compatible(&embedding, model) {
                return Err(StoreError::MalformedEmbedding {
                    skill_id: artifact.id,
                    reason: "embedding metadata is incompatible after rebuild".to_string(),
                });
            }
            Ok((artifact, embedding, metadata))
        })
        .collect()
}

fn embedding_is_compatible(embedding: &StoredEmbedding, model: &ModelMetadata) -> bool {
    embedding.model_id == model.model_id
        && embedding.model_revision == model.model_revision
        && embedding.dimensions == model.dimensions
        && embedding.normalized == model.normalized
        && embedding.values.len() == model.dimensions
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
